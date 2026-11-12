use super::*;
use crate::config::{ProviderCredential, ProviderDefinition};

mod copilot;
mod errors;
mod events;
mod guard;
mod kiro;
mod pacing;
mod retry;

pub(crate) use copilot::*;
use errors::*;
pub(crate) use events::*;
pub(crate) use guard::*;
pub(crate) use kiro::*;
pub(crate) use pacing::*;
pub(crate) use retry::*;

// OpenCode Go requires a session identifier for routing. Keep a stable,
// opaque identifier per Codex thread without exposing the original thread ID.
fn opencode_go_session(base_url: &str, headers: Option<&HeaderMap>) -> Option<String> {
    if base_url.trim().trim_end_matches('/') != "https://opencode.ai/zen/go/v1" {
        return None;
    }
    use sha2::{Digest, Sha256};
    let thread =
        diagnostics::client_thread_id(headers).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    Some(format!("codetas-{:x}", Sha256::digest(thread.as_bytes())))
}

#[cfg(test)]
#[path = "tests/opencode_go_session_tests.rs"]
mod opencode_go_session_tests;

pub(crate) fn image_history_failure(error: crate::translate::ImageHistoryError) -> AttemptFailure {
    match error {
        crate::translate::ImageHistoryError::BudgetExceeded { .. }
        | crate::translate::ImageHistoryError::OmissionRequired => {
            image_history_compaction_required(&error.to_string())
        }
        crate::translate::ImageHistoryError::Worker(_) => {
            request_failure("image_normalization_failed", &error.to_string())
        }
    }
}

fn image_history_compaction_required(message: &str) -> AttemptFailure {
    let mut failure = request_failure("context_length_exceeded", message);
    failure.response =
        candidate_policy_error_response(&CandidatePolicyError::InputBudget(message.into()));
    failure
}

#[cfg(test)]
#[path = "tests/image_budget_boundary_tests.rs"]
mod image_budget_boundary_tests;

#[derive(Clone, Copy)]
struct SentImageState {
    sent_images: usize,
}

#[derive(Clone)]
pub(crate) struct EmptyCompletionRecoveryFailure {
    pub(crate) usage: TokenUsage,
    pub(crate) additional_sends: u16,
}

#[derive(Clone, Copy)]
pub(crate) struct EmptyCompletionRecoverySuccess {
    pub(crate) additional_sends: u16,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ProviderRetryObservation {
    pub(crate) thinking_provenance: Option<super::anthropic_compact::ThinkingProvenance>,
    pub(crate) known_billing: Option<TokenUsage>,
    pub(crate) request_diagnostic: Option<crate::observability::RequestDiagnostic>,
    pub(crate) terminal_failure: bool,
    pub(crate) cache_observation: Option<crate::observability::CacheObservation>,
    pub(crate) additional_sends: u16,
    pub(crate) recovery_kinds: Vec<String>,
    pub(crate) usage: TokenUsage,
}

#[derive(Clone, Default)]
pub(crate) struct SharedProviderRetryObservation(
    pub(crate) Arc<std::sync::Mutex<ProviderRetryObservation>>,
);

/// Ends the routing attempt when the last clone is dropped. Nothing reads the inner
/// value; holding it is the point (see `Drop for RoutingAttemptLeaseInner`).
#[derive(Clone)]
pub(crate) struct RoutingAttemptLease {
    _inner: Arc<RoutingAttemptLeaseInner>,
}

impl RoutingAttemptLease {
    fn new(inner: Arc<RoutingAttemptLeaseInner>) -> Self {
        Self { _inner: inner }
    }
}

struct RoutingAttemptLeaseInner {
    routing: Arc<Mutex<RoutingRuntime>>,
    candidate: RouteCandidate,
}

impl Drop for RoutingAttemptLeaseInner {
    fn drop(&mut self) {
        // Prefer a synchronous release so Drop never depends solely on a live
        // Tokio runtime. Fall back to an async unlock only when contended.
        if let Ok(mut routing) = self.routing.try_lock() {
            routing.end_attempt(&self.candidate);
            return;
        }
        let routing = Arc::clone(&self.routing);
        let candidate = self.candidate.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                routing.lock().await.end_attempt(&candidate);
            });
            return;
        }
        // Last resort: block for the mutex when no runtime is available.
        // This path is rare (tests / teardown) but must not leak active leases.
        let mut routing = self.routing.blocking_lock();
        routing.end_attempt(&candidate);
    }
}

pub(crate) fn take_routing_attempt_lease(
    response: &mut reqwest::Response,
) -> Option<RoutingAttemptLease> {
    response.extensions_mut().remove::<RoutingAttemptLease>()
}

pub(crate) fn validate_candidate_image_references(
    body: &Value,
    candidate: &RouteCandidate,
) -> Result<(), AttemptFailure> {
    crate::compaction::rendered_compaction_view(body)
        .map_err(|message| request_failure("invalid_compaction_history", &message))?;
    if candidate.provider.transport != ProviderTransport::Standard
        || candidate
            .provider
            .protocol_for_model(&candidate.upstream_model)
            != ProviderProtocol::Responses
    {
        crate::translate::validate_translated_image_references(body)
            .map_err(|message| request_failure("unsupported_request", &message))?;
    }
    Ok(())
}

pub(crate) async fn send_candidate(
    state: &GatewayState,
    body: &mut Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
) -> Result<reqwest::Response, AttemptFailure> {
    validate_candidate_image_references(body, candidate)?;
    let cloud_request_id = (candidate.provider.google_mode == GoogleMode::CloudCodeAssist)
        .then(|| format!("agent-{}", Uuid::new_v4()));
    let cloud_envelope_cache = cloud_request_id
        .as_ref()
        .map(|_| Arc::new(Mutex::new(None::<Value>)));
    let remote_compaction = request_is_remote_compaction(body);
    let protocol = candidate
        .provider
        .protocol_for_model(&candidate.upstream_model);
    crate::compaction::validate_local_compactions(body)
        .map_err(|message| request_failure("invalid_compaction_history", &message))?;
    crate::anthropic_compaction::expand_retained(body)
        .map_err(|message| request_failure("invalid_compaction_history", &message))?;
    let strip_unsupported_images = state.settings.read().await.agents.image_input_mode
        != crate::config::AuxiliaryInputMode::Native;
    apply_provider_request_compatibility(body, candidate, protocol, strip_unsupported_images);
    if !remote_compaction {
        let image_report = normalize_translated_image_history(
            body,
            protocol,
            candidate.provider.transport,
            candidate.provider.limits.max_request_bytes,
        )
        .await
        .map_err(image_history_failure)?;
        if image_report.omitted_images > 0 || image_report.pruned_images > 0 {
            crate::debug::log(&format!(
                "image history normalized: provider={} model={} omitted={} pruned={} total={} kept_base64_chars={}",
                candidate.provider.id,
                candidate.upstream_model,
                image_report.omitted_images,
                image_report.pruned_images,
                image_report.inline_images,
                image_report.kept_base64_chars,
            ));
        }
        // Kiro rejects a message with more than 20 images, and Codex never
        // compacts on image bytes or counts, so the thread would stop for good.
        let max_images = if candidate.provider.transport == ProviderTransport::Kiro {
            Some(crate::kiro::MAX_KIRO_IMAGES)
        } else {
            crate::translate::provider_max_request_images(
                &candidate.provider.id,
                &candidate.upstream_model,
            )
        };
        if let Some(max_images) = max_images {
            let dropped = crate::translate::prune_oldest_images_to_count(body, max_images);
            if dropped > 0 {
                crate::debug::log(&format!(
                    "image count pruned: provider={} model={} dropped={} limit={}",
                    candidate.provider.id, candidate.upstream_model, dropped, max_images,
                ));
            }
        }
    }
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut first = {
        let prepared_body: &Value = body;
        retry_provider_request(state, candidate, streaming, || {
            send_candidate_once(
                state,
                prepared_body,
                candidate,
                caller_headers,
                cloud_request_id.as_deref(),
                cloud_envelope_cache.as_ref(),
            )
        })
        .await?
    };
    first =
        if matches!(
            first.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) && candidate_oauth_credential(candidate)
        {
            let login_key =
                crate::oauth::request_login_key(&candidate.provider, candidate.credential.as_ref());
            match crate::oauth::force_refresh_oauth_access_token(login_key).await {
                Ok(_) => {
                    crate::debug::log(&format!(
                    "provider returned {}; refreshed OAuth and retrying once: provider={} model={}",
                    first.status(), candidate.provider.id, candidate.upstream_model,
                ));
                    let (recovery, first_diagnostic, first_request_diagnostic) =
                        retire_oauth_response(first).await;
                    let prepared_body: &Value = body;
                    match retry_provider_request(state, candidate, streaming, || {
                        send_candidate_once(
                            state,
                            prepared_body,
                            candidate,
                            caller_headers,
                            cloud_request_id.as_deref(),
                            cloud_envelope_cache.as_ref(),
                        )
                    })
                    .await
                    {
                        Ok(mut response) => {
                            let nested = response
                                .extensions_mut()
                                .remove::<ProviderRetryObservation>();
                            let mut combined = Some(recovery);
                            merge_provider_retry_observation(&mut combined, nested);
                            if let Some(combined) = combined {
                                response.extensions_mut().insert(combined);
                            }
                            if let Some(diagnostic) = first_diagnostic {
                                response.extensions_mut().insert(diagnostic);
                            }
                            response
                        }
                        Err(mut failure) => {
                            let nested = failure
                                .response
                                .extensions_mut()
                                .remove::<ProviderRetryObservation>();
                            let mut combined = Some(recovery);
                            merge_provider_retry_observation(&mut combined, nested);
                            failure = failure_with_provider_metadata(
                                failure,
                                combined.as_ref(),
                                first_request_diagnostic.as_ref(),
                            );
                            if let Some(diagnostic) = first_diagnostic {
                                failure.response.extensions_mut().insert(diagnostic);
                            }
                            return Err(failure);
                        }
                    }
                }
                Err(error) => {
                    crate::debug::log(&format!(
                        "OAuth refresh after provider auth rejection failed: provider={} error={}",
                        candidate.provider.id, error,
                    ));
                    let mut recovery = first
                        .extensions_mut()
                        .remove::<ProviderRetryObservation>()
                        .unwrap_or_default();
                    recovery.recovery_kinds.push("oauth-refresh-failed".into());
                    first.extensions_mut().insert(recovery);
                    first
                }
            }
        } else {
            first
        };
    let first_status = first.status();
    let image_rejection_retry = matches!(
        first_status,
        StatusCode::PAYLOAD_TOO_LARGE | StatusCode::BAD_REQUEST
    ) && !remote_compaction;
    let response = if image_rejection_retry {
        require_compaction_on_image_rejection(first, first_status).await?
    } else {
        first
    };
    guard_empty_completion_response(
        state,
        body,
        candidate,
        caller_headers,
        streaming,
        response,
        cloud_request_id,
        cloud_envelope_cache,
    )
    .await
}

fn candidate_oauth_credential(candidate: &RouteCandidate) -> bool {
    candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source
        == CredentialSource::OAuth
}

pub(crate) async fn send_candidate_once(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
    cloud_request_id: Option<&str>,
    cloud_envelope_cache: Option<&Arc<Mutex<Option<Value>>>>,
) -> Result<reqwest::Response, AttemptFailure> {
    validate_candidate_image_references(body, candidate)?;
    // Resolve once per physical Anthropic send, then reuse that very request's
    // auth for both signature admission and transport (including refresh retries).
    let authenticated_request = if candidate.provider.transport == ProviderTransport::Standard
        && candidate
            .provider
            .protocol_for_model(&candidate.upstream_model)
            == ProviderProtocol::AnthropicMessages
    {
        let wire_model = wire_model_for_request(candidate, body);
        let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        let endpoint = candidate
            .provider
            .endpoint_for_model_streaming(&wire_model, streaming);
        let request = apply_provider_auth(
            state.client.post(endpoint),
            &candidate.provider,
            candidate.credential.as_ref(),
        )
        .await
        .map_err(|message| AttemptFailure {
            response: error_response(
                StatusCode::UNAUTHORIZED,
                "missing_provider_credential",
                &message,
            ),
            kind: AttemptFailureKind::Credential,
        })?;
        let request = if candidate
            .credential
            .as_ref()
            .unwrap_or(&candidate.provider.credential)
            .source
            == CredentialSource::Forward
        {
            let headers = caller_headers.ok_or_else(|| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    "caller authentication headers are required",
                ),
                kind: AttemptFailureKind::Credential,
            })?;
            apply_forward_headers(request, &state.settings, headers)
                .await
                .map_err(|message| AttemptFailure {
                    response: error_response(
                        StatusCode::UNAUTHORIZED,
                        "missing_forward_credential",
                        &message,
                    ),
                    kind: AttemptFailureKind::Credential,
                })?
        } else {
            request
        };
        Some(request.build().map_err(|_| {
            request_failure(
                "invalid_request",
                "cannot build authenticated provider request",
            )
        })?)
    } else {
        None
    };
    let resolved_scope = authenticated_request
        .as_ref()
        .and_then(|request| resolved_anthropic_checkpoint_scope(candidate, request));
    let had_checkpoint = body
        .get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(crate::anthropic_compaction::is_item));
    // This function is also called directly by retry/compact paths.
    let scoped_body;
    let body = if body
        .get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(crate::anthropic_compaction::is_item))
    {
        scoped_body = scoped_native_history(body, candidate, resolved_scope.as_deref())
            .map_err(|message| request_failure("invalid_compaction_history", &message))?;
        &scoped_body
    } else {
        body
    };
    let remote_compaction = request_is_remote_compaction(body);
    let native_compact = remote_compaction
        && body
            .get(crate::anthropic_compaction::REQUEST_FLAG)
            .and_then(Value::as_bool)
            == Some(true)
        && candidate
            .provider
            .protocol_for_model(&candidate.upstream_model)
            == ProviderProtocol::AnthropicMessages;
    let source_image_count = count_translated_input_images(body);
    let protocol = candidate
        .provider
        .protocol_for_model(&candidate.upstream_model);
    let wire_model = wire_model_for_request(candidate, body);
    if candidate.provider.transport == ProviderTransport::Kiro {
        return send_kiro_candidate(state, body, candidate, caller_headers, &wire_model).await;
    }
    if candidate.provider.transport == ProviderTransport::GithubCopilot {
        return send_github_copilot_candidate(state, body, candidate, caller_headers, &wire_model)
            .await;
    }
    let mut upstream_body = match protocol {
        ProviderProtocol::Responses => body.clone(),
        ProviderProtocol::ChatCompletions => {
            match chat_wire_request(body, candidate, &wire_model) {
                Ok(value) => value,
                Err(message) => return Err(request_failure("unsupported_request", &message)),
            }
        }
        ProviderProtocol::AnthropicMessages => {
            match responses_to_anthropic_with_oauth(
                body,
                &wire_model,
                uses_anthropic_subscription_oauth(&candidate.provider),
            ) {
                Ok(value) => value,
                Err(message) => return Err(request_failure("unsupported_request", &message)),
            }
        }
        ProviderProtocol::GeminiGenerateContent => match responses_to_gemini(body, &wire_model) {
            Ok(value) => value,
            Err(message) => return Err(request_failure("unsupported_request", &message)),
        },
    };
    // Continuity control fields stay on `body` for local `remember`; never wire them.
    crate::response_state::ResponseStateStore::strip_private_fields(&mut upstream_body);
    if let Some(object) = upstream_body.as_object_mut() {
        object.remove(crate::anthropic_compaction::REQUEST_FLAG);
        object.remove(crate::anthropic_compaction::HISTORY_REWRITTEN);
    }
    if !remote_compaction || native_compact {
        if let Err(message) =
            apply_provider_wire_compatibility(&mut upstream_body, body, candidate, protocol)
        {
            return Err(request_failure("unsupported_request", &message));
        }
    }
    let has_native_block = protocol == ProviderProtocol::AnthropicMessages
        && upstream_body
            .get("messages")
            .and_then(Value::as_array)
            .is_some_and(|messages| {
                messages.iter().any(|m| {
                    m.get("content")
                        .and_then(Value::as_array)
                        .is_some_and(|blocks| {
                            blocks.iter().any(|b| {
                                b.get("type").and_then(Value::as_str) == Some("compaction")
                            })
                        })
                })
            });
    let thinking_provenance = (protocol == ProviderProtocol::AnthropicMessages).then(|| {
        super::anthropic_compact::ThinkingProvenance::for_wire(
            &upstream_body,
            resolved_scope.as_deref(),
            had_checkpoint,
        )
    });
    if let Some(proof) = &thinking_provenance {
        super::anthropic_compact::filter_wire_thinking(&mut upstream_body, body, proof);
    }
    let mut native_context = native_compact.then(|| NativeAnthropicContext {
        scope: resolved_scope.clone().unwrap_or_else(|| {
            crate::anthropic_compaction::digest(&json!({"unbound":Uuid::new_v4().to_string()}))
        }),
        context_hash: crate::anthropic_compaction::context_hash(&upstream_body),
        history_rewritten: body
            .get(crate::anthropic_compaction::HISTORY_REWRITTEN)
            .and_then(Value::as_bool)
            == Some(true),
    });
    if native_compact {
        upstream_body["compaction"] = json!({"type":"summarize"});
        if let Some(object) = upstream_body.as_object_mut() {
            object.remove("context_management");
            object.remove("stop_sequences");
            if object
                .get("tool_choice")
                .and_then(|v| v.get("type"))
                .and_then(Value::as_str)
                .is_some_and(|s| matches!(s, "any" | "tool"))
            {
                object.remove("tool_choice");
            }
            if let Some(config) = object
                .get_mut("output_config")
                .and_then(Value::as_object_mut)
            {
                config.remove("format");
                if let Some(budget) = config.get_mut("task_budget").and_then(Value::as_object_mut) {
                    budget.remove("remaining");
                }
                if config.is_empty() {
                    object.remove("output_config");
                }
            }
        }
    }
    let antigravity_reference =
        antigravity_project_oauth_reference(&candidate.provider, candidate.credential.as_ref());
    let antigravity_project = if let Some(reference) =
        antigravity_reference.filter(|_| protocol == ProviderProtocol::GeminiGenerateContent)
    {
        let token = resolve_oauth_access_token(reference)
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_provider_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?;
        Some(
            resolve_antigravity_cloud_project(&token)
                .await
                .map_err(|message| AttemptFailure {
                    response: error_response(
                        StatusCode::BAD_GATEWAY,
                        "provider_configuration_error",
                        &message,
                    ),
                    kind: AttemptFailureKind::Retryable,
                })?,
        )
    } else {
        None
    };
    if protocol == ProviderProtocol::GeminiGenerateContent
        && candidate.provider.google_mode == GoogleMode::CloudCodeAssist
    {
        if let Some(cache) = cloud_envelope_cache {
            let mut cached = cache.lock().await;
            if let Some(envelope) = cached.as_ref() {
                upstream_body = envelope.clone();
            } else {
                upstream_body = cloud_code_assist_envelope(
                    upstream_body,
                    body,
                    candidate,
                    &wire_model,
                    antigravity_project.as_deref(),
                    cloud_request_id,
                )?;
                *cached = Some(upstream_body.clone());
            }
        } else {
            upstream_body = cloud_code_assist_envelope(
                upstream_body,
                body,
                candidate,
                &wire_model,
                antigravity_project.as_deref(),
                cloud_request_id,
            )?;
        }
    }
    if protocol != ProviderProtocol::GeminiGenerateContent {
        upstream_body["model"] = Value::String(wire_model.clone());
    }
    if remote_compaction && protocol == ProviderProtocol::Responses {
        // Match OpenCodex's normal Responses adapter: preserve the v2
        // compaction trigger and top-level options, expand local `codetas1:`
        // envelopes, and remove replayed raw reasoning content that the native
        // backend rejects.
        crate::compat::sanitize_compact_trigger_request(&mut upstream_body);
        crate::debug::log(
            "send_candidate: applied compact-trigger reasoning sanitize; broader compatibility skipped",
        );
    }
    if protocol == ProviderProtocol::Responses {
        // Provider metadata must remain on `body` for local continuation and
        // switching back to Gemini/Anthropic, but strict Responses backends
        // reject these private per-item fields (including compact requests).
        crate::compat::strip_responses_provider_metadata(&mut upstream_body);
    }
    if !remote_compaction
        && matches!(
            protocol,
            ProviderProtocol::AnthropicMessages
                | ProviderProtocol::GeminiGenerateContent
                | ProviderProtocol::ChatCompletions
        )
    {
        let image_report = normalize_translated_image_history(
            &mut upstream_body,
            protocol,
            candidate.provider.transport,
            candidate.provider.limits.max_request_bytes,
        )
        .await
        .map_err(image_history_failure)?;
        if image_report.omitted_images > 0 || image_report.pruned_images > 0 {
            crate::debug::log(&format!(
                "wire image history normalized: provider={} model={} protocol={:?} omitted={} pruned={} total={} kept_base64_chars={}",
                candidate.provider.id,
                candidate.upstream_model,
                protocol,
                image_report.omitted_images,
                image_report.pruned_images,
                image_report.inline_images,
                image_report.kept_base64_chars,
            ));
        }
    }
    let translation_image_omissions =
        source_image_count.saturating_sub(count_translated_input_images(&upstream_body));

    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    // Compact triggers bypass broad compatibility rewriting, not the wire byte
    // budget. Work on the outbound clone so replay/checkpoint history stays intact.
    let (serialized, wire_image_omissions) = if remote_compaction {
        // This is an explicit history-reset boundary, never an inference retry.
        crate::translate::serialize_with_image_budget_async(
            &mut upstream_body,
            candidate.provider.limits.max_request_bytes,
        )
        .await
        .map_err(|error| request_failure("invalid_request", &error))?
    } else {
        (
            crate::translate::serialize_preserving_image_history_async(
                &mut upstream_body,
                candidate.provider.limits.max_request_bytes,
            )
            .await
            .map_err(image_history_failure)?,
            0,
        )
    };
    if let Some(context) = native_context.as_mut() {
        context.history_rewritten |= translation_image_omissions > 0 || wire_image_omissions > 0;
    }
    if remote_compaction && wire_image_omissions > 0 {
        crate::debug::log(&format!(
            "compact wire image budget: provider={} omitted={} request_bytes={}",
            candidate.provider.id,
            wire_image_omissions,
            serialized.len(),
        ));
    }
    if serialized.len() as u64 > candidate.provider.limits.max_request_bytes {
        return Err(request_failure(
            "request_too_large",
            &format!(
                "translated request exceeds the configured provider limit: provider={} model={} request_bytes={} max_request_bytes={}",
                candidate.provider.id,
                candidate.upstream_model,
                serialized.len(),
                candidate.provider.limits.max_request_bytes,
            ),
        ));
    }

    let mut query_params = candidate.provider.query_params.clone();
    if let Some(version) = candidate.provider.azure_api_version.as_ref() {
        query_params.insert("api-version".into(), version.clone());
    }
    if protocol == ProviderProtocol::GeminiGenerateContent && streaming {
        query_params.insert("alt".into(), "sse".into());
    }

    let endpoint = candidate
        .provider
        .endpoint_for_model_streaming(&wire_model, streaming);
    let dns_pinning = state.settings.read().await.security.dns_pinning;
    let client = if dns_pinning {
        let client = pinned_client(&endpoint, &candidate.provider, "CODETAS-Gateway/0.1")
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "provider_dns_rejected",
                    &message,
                ),
                kind: AttemptFailureKind::Retryable,
            })?;
        client
    } else {
        state.client.clone()
    };
    let auth_already_resolved = authenticated_request.is_some();
    let mut request = match authenticated_request {
        Some(request) => reqwest::RequestBuilder::from_parts(client.clone(), request),
        None => client.post(endpoint.clone()),
    }
    .header(header::CONTENT_TYPE, "application/json")
    .header(
        header::ACCEPT,
        if streaming {
            "text/event-stream"
        } else {
            "application/json"
        },
    )
    .query(&query_params)
    .timeout(Duration::from_millis(
        candidate.provider.limits.request_timeout_ms,
    ));
    if let Some(session) = opencode_go_session(&candidate.provider.base_url, caller_headers) {
        request = request.header("x-opencode-session", session);
    }
    if protocol == ProviderProtocol::AnthropicMessages {
        request = request.header("anthropic-version", "2023-06-01");
        if uses_anthropic_subscription_oauth(&candidate.provider) {
            for (name, value) in anthropic_subscription_oauth_headers() {
                request = request.header(name, value);
            }
        }
    }
    let request = match if auth_already_resolved {
        Ok(request)
    } else {
        apply_provider_auth(request, &candidate.provider, candidate.credential.as_ref()).await
    } {
        Ok(request) => request,
        Err(message) => {
            return Err(AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_provider_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })
        }
    };
    let credential_source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let request = if credential_source == CredentialSource::Forward && !auth_already_resolved {
        let Some(caller_headers) = caller_headers else {
            return Err(AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    "the selected provider requires caller authentication headers",
                ),
                kind: AttemptFailureKind::Credential,
            });
        };
        apply_forward_headers(request, &state.settings, caller_headers)
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?
    } else {
        request
    };
    let request = if candidate.provider.google_mode == GoogleMode::CloudCodeAssist {
        request.header(header::USER_AGENT, crate::oauth::antigravity_user_agent())
    } else {
        request
    };

    let messages = upstream_body
        .get("messages")
        .and_then(serde_json::Value::as_array);
    let reasoning_count = messages.map(|m| {
        m.iter()
            .filter(|msg| msg.get("reasoning_content").is_some())
            .count()
    });
    let assistant_missing_reasoning = messages.map(|m| {
        m.iter()
            .filter(|msg| {
                msg.get("role").and_then(serde_json::Value::as_str) == Some("assistant")
                    && msg.get("reasoning_content").is_none()
            })
            .count()
    });
    let assistant_with_reasoning = messages.map(|m| {
        m.iter()
            .filter(|msg| {
                msg.get("role").and_then(serde_json::Value::as_str) == Some("assistant")
                    && msg.get("reasoning_content").is_some()
            })
            .count()
    });
    crate::debug::log(&format!(
        "send_candidate_once: POST {} (stream={} store={} input_items={} messages={} reasoning={} asst_no_reasoning={} asst_reasoning={} model={})",
        endpoint,
        upstream_body
            .get("stream")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        upstream_body
            .get("store")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
        upstream_body
            .get("input")
            .and_then(serde_json::Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0),
        messages.map(|m| m.len()).unwrap_or(0),
        reasoning_count.unwrap_or(0),
        assistant_missing_reasoning.unwrap_or(0),
        assistant_with_reasoning.unwrap_or(0),
        wire_model
    ));
    let mut request_diagnostic =
        diagnostics::wire_request_diagnostic(&upstream_body, serialized.len(), caller_headers);
    request_diagnostic.wire_model = Some(wire_model.clone());
    request_diagnostic.dynamic_tools = diagnostics::dynamic_tool_stats(body);
    let sent = async {
        let mut request = request.body(serialized).build()?;
        if protocol == ProviderProtocol::AnthropicMessages && (native_compact || has_native_block) {
            let mut betas = request
                .headers()
                .get_all("anthropic-beta")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(','))
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            if !betas.iter().any(|v| v == crate::anthropic_compaction::BETA) {
                betas.push(crate::anthropic_compaction::BETA.into());
            }
            request.headers_mut().insert(
                "anthropic-beta",
                HeaderValue::from_str(&betas.join(","))
                    .expect("existing validated headers and beta constant"),
            );
        }
        request_diagnostic.auth_binding = Some(diagnostics::resolved_auth_binding(&request));
        client.execute(request).await
    }
    .await;
    match sent {
        Ok(mut response) => {
            if let Some(proof) = thinking_provenance {
                response.extensions_mut().insert(proof);
            }
            if let Some(context) = native_context {
                response.extensions_mut().insert(context);
            }
            diagnostics::record_response_headers(&mut request_diagnostic, &response);
            response.extensions_mut().insert(request_diagnostic);
            response.extensions_mut().insert(SentImageState {
                sent_images: crate::translate::count_rendered_sent_images(&upstream_body),
            });
            let status = response.status();
            crate::debug::log(&format!("send_candidate_once: -> {status}"));
            if !status.is_success() {
                let preview = response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("-");
                crate::debug::log_always(&format!(
                    "send_candidate_once error status={status} content_type={preview} content_length={:?}",
                    response.content_length()
                ));
            }
            if response
                .content_length()
                .is_some_and(|length| length > candidate.provider.limits.max_response_bytes)
            {
                Err(reject_oversized_provider_response(
                    response,
                    candidate.provider.limits.max_response_bytes,
                    false,
                )
                .await)
            } else {
                Ok(response)
            }
        }
        Err(error) => Err(AttemptFailure {
            response: error_response(
                StatusCode::BAD_GATEWAY,
                "provider_unreachable",
                &format!("provider request failed: {error}"),
            ),
            kind: AttemptFailureKind::Retryable,
        }),
    }
}

/// The wire copy of a history holding a native checkpoint. `remember` keeps the
/// caller's `body`, so nothing here reaches stored history.
fn scoped_native_history(
    body: &Value,
    candidate: &RouteCandidate,
    resolved_scope: Option<&str>,
) -> Result<Value, String> {
    let mut scoped = body.clone();
    crate::anthropic_compaction::expand_retained(&mut scoped)?;
    prepare_native_anthropic_history(&mut scoped, candidate, resolved_scope)?;
    crate::anthropic_compaction::place_context_after_newest_checkpoint(&mut scoped);
    Ok(scoped)
}

fn chat_wire_request(
    body: &Value,
    candidate: &RouteCandidate,
    wire_model: &str,
) -> Result<Value, String> {
    let mut source = chat_compatible_request(body, candidate.capabilities.provider_metadata);
    let mistral = is_mistral_chat_endpoint(&candidate.provider.base_url);
    if mistral {
        promote_trailing_instruction_to_user(&mut source);
        describe_apply_patch_format(&mut source);
    }
    let mut chat = responses_to_chat_with_options(
        &source,
        wire_model,
        model_requires_reasoning_placeholder(&candidate.provider, &candidate.upstream_model),
    )?;
    if mistral {
        continue_after_trailing_assistant(&mut chat);
    }
    Ok(chat)
}

fn chat_compatible_request(body: &Value, preserve_provider_metadata: bool) -> Value {
    let mut body = body.clone();
    if preserve_provider_metadata {
        return body;
    }
    if let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) {
        for item in items.iter_mut().filter_map(Value::as_object_mut) {
            item.remove("provider_metadata");
        }
    }
    body
}

pub(crate) fn cloud_code_assist_envelope(
    mut request: Value,
    source_request: &Value,
    candidate: &RouteCandidate,
    wire_model: &str,
    project_override: Option<&str>,
    request_id: Option<&str>,
) -> Result<Value, AttemptFailure> {
    let project = candidate
        .provider
        .project
        .as_deref()
        .or(project_override)
        .ok_or_else(|| {
            request_failure(
                "provider_configuration_error",
                "Cloud Code Assist requires provider.project or an Antigravity OAuth session",
            )
        })?;
    let session_source = source_request
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .unwrap_or(candidate.exposed_model.as_str());
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in session_source.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    if let Some(object) = request.as_object_mut() {
        object.insert("sessionId".into(), Value::String(format!("-{hash}")));
    }
    Ok(json!({
        "model": wire_model,
        "userAgent": "antigravity",
        "requestType": "agent",
        "project": project,
        "requestId": request_id
            .map(str::to_string)
            .unwrap_or_else(|| format!("agent-{}", Uuid::new_v4())),
        "request": request,
    }))
}

#[cfg(test)]
#[path = "tests/image_retry_tests.rs"]
mod image_retry_tests;

#[cfg(test)]
#[path = "tests/provider_pacing_tests.rs"]
mod provider_pacing_tests;

#[cfg(test)]
#[path = "tests/routing_attempt_lease_tests.rs"]
mod routing_attempt_lease_tests;

#[cfg(test)]
#[path = "tests/rate_limit_retry_tests.rs"]
mod rate_limit_retry_tests;

#[cfg(test)]
#[path = "tests/final_cache_measurement_tests.rs"]
mod final_cache_measurement_tests;

#[cfg(test)]
#[path = "tests/chat_trailing_usage_tests.rs"]
mod chat_trailing_usage_tests;

#[cfg(test)]
#[path = "tests/chat_nonretryable_terminal_tests.rs"]
mod chat_nonretryable_terminal_tests;

#[cfg(test)]
#[path = "tests/raw_anthropic_billing_tests.rs"]
mod raw_anthropic_billing_tests;

#[cfg(test)]
#[path = "tests/physical_stream_observation_tests.rs"]
mod physical_stream_observation_tests;

pub(crate) fn failure_with_provider_metadata(
    mut failure: AttemptFailure,
    retry: Option<&ProviderRetryObservation>,
    diagnostic: Option<&crate::observability::RequestDiagnostic>,
) -> AttemptFailure {
    if let Some(retry) = retry {
        failure.response.extensions_mut().insert(retry.clone());
    }
    if let Some(diagnostic) = diagnostic {
        failure.response.extensions_mut().insert(diagnostic.clone());
    }
    failure
}

// Consume the rejected body and release its concurrency slot before acquiring
// another lease. Neither refresh retries nor their failures lose known usage.
async fn retire_oauth_response(
    mut response: reqwest::Response,
) -> (
    ProviderRetryObservation,
    Option<UpstreamErrorDiagnostic>,
    Option<crate::observability::RequestDiagnostic>,
) {
    let diagnostic = response
        .extensions()
        .get::<UpstreamErrorDiagnostic>()
        .cloned();
    let request_diagnostic = response
        .extensions()
        .get::<crate::observability::RequestDiagnostic>()
        .cloned();
    let mut recovery = response
        .extensions_mut()
        .remove::<ProviderRetryObservation>()
        .unwrap_or_default();
    let lease = take_routing_attempt_lease(&mut response);
    let usage = response_body_usage_from_reqwest(response).await;
    recovery.usage = sum_token_usage(recovery.usage, usage);
    drop(lease);
    recovery.additional_sends = recovery.additional_sends.saturating_add(1);
    recovery.recovery_kinds.push("oauth-refresh".into());
    (recovery, diagnostic, request_diagnostic)
}

#[cfg(test)]
#[path = "tests/review11_retry_tests.rs"]
mod review11_retry_tests;

#[cfg(test)]
#[path = "tests/regression12_sender_tests.rs"]
mod regression12_sender_tests;

#[cfg(test)]
#[path = "tests/review15_provenance_transport_regressions.rs"]
mod review15_provenance_transport_regressions;

#[derive(Debug)]
struct ProviderStreamReadFailure {
    message: &'static str,
    usage: TokenUsage,
}
impl std::fmt::Display for ProviderStreamReadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}
impl std::error::Error for ProviderStreamReadFailure {}
fn provider_stream_read_failure(
    message: &'static str,
    collector: &super::stream::SseUsageCollector,
) -> std::io::Error {
    std::io::Error::other(ProviderStreamReadFailure {
        message,
        usage: collector.usage.clone(),
    })
}

#[cfg(test)]
#[path = "tests/review16_physical_guard_regressions.rs"]
mod review16_physical_guard_regressions;

pub(crate) async fn reject_oversized_provider_response(
    response: reqwest::Response,
    limit: u64,
    kiro: bool,
) -> AttemptFailure {
    let retry = response
        .extensions()
        .get::<ProviderRetryObservation>()
        .cloned();
    let diagnostic = response
        .extensions()
        .get::<crate::observability::RequestDiagnostic>()
        .cloned();
    let usage = if kiro {
        match super::adapt::read_kiro_bounded(response, limit).await {
            Ok(bytes) => crate::kiro::buffered_usage(&bytes),
            Err((_, usage)) => usage,
        }
    } else {
        match super::stream::bounded_json_with_usage(response, limit).await {
            Ok(value) => TokenUsage::from_json(&value),
            Err((_, usage)) => usage,
        }
    };
    let mut failure = failure_with_provider_metadata(
        AttemptFailure {
            response: error_response(
                StatusCode::BAD_GATEWAY,
                "provider_response_too_large",
                "provider response exceeds the configured limit",
            ),
            kind: AttemptFailureKind::Retryable,
        },
        retry.as_ref(),
        diagnostic.as_ref(),
    );
    failure
        .response
        .extensions_mut()
        .insert(crate::observability::ReportedUpstreamUsage(usage));
    failure
}

#[cfg(test)]
#[path = "tests/review16_bounded_attempt_regressions.rs"]
mod review16_bounded_attempt_regressions;

#[cfg(test)]
#[path = "tests/review17_buffered_retry_regressions.rs"]
mod review17_buffered_retry_regressions;

#[cfg(test)]
pub(super) mod review18_regressions {
    use super::*;
    pub(crate) fn state() -> GatewayState {
        let mut settings = GatewaySettings::default();
        settings.security.dns_pinning = false;
        let settings = Arc::new(tokio::sync::RwLock::new(settings));
        GatewayState {
            settings: settings.clone(),
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            routing: Arc::new(Mutex::new(RoutingRuntime::default())),
            observability: ObservabilityLedger::new(None),
            video_jobs: Arc::new(Mutex::new(HashMap::new())),
            instance_id: Uuid::new_v4().to_string(),
            response_state: Arc::new(ResponseStateStore::new(None)),
            memory: Arc::new(MemoryAdmission {
                settings,
                inflight: std::sync::atomic::AtomicU32::new(0),
                reserved_bytes: std::sync::atomic::AtomicU64::new(0),
                rejected: std::sync::atomic::AtomicU64::new(0),
            }),
            pacing: Arc::new(ProviderPacing::default()),
        }
    }
    pub(crate) async fn registered_candidate(
        state: &GatewayState,
        c: RouteCandidate,
    ) -> RouteCandidate {
        let target = format!("{}/{}", c.provider.id, c.upstream_model);
        let mut provider = c.provider;
        provider.models = vec![c.upstream_model.clone()];
        provider.capabilities = c.capabilities;
        let mut settings = state.settings.write().await;
        settings.providers = vec![provider];
        let mut selected = state
            .routing
            .lock()
            .await
            .candidates_scoped(&settings, &target, None)
            .expect("fixture route must be selectable")
            .remove(0);
        selected.max_input_tokens = c.max_input_tokens;
        selected.max_output_tokens = c.max_output_tokens;
        selected.context_window = c.context_window;
        selected
    }
    #[tokio::test]
    async fn review19_registered_fixture_admits_real_routing_lease() {
        let state = state();
        let mut c = super::super::adapt::review16_regressions::candidate();
        c.provider.credential.source = CredentialSource::None;
        c.provider.credential.reference = None;
        assert!(
            state.routing.lock().await.begin_attempt(&c).is_err(),
            "the old fabricated fixture must be rejected before transport"
        );
        let c = registered_candidate(&state, c).await;
        let mut routing = state.routing.lock().await;
        routing
            .begin_attempt(&c)
            .expect("selected fixture must admit a physical send");
        routing.end_attempt(&c);
    }

    #[tokio::test]
    async fn review19_file_reference_budget_preserves_subagent_tool_result() {
        for protocol in [
            ProviderProtocol::Responses,
            ProviderProtocol::ChatCompletions,
            ProviderProtocol::AnthropicMessages,
            ProviderProtocol::GeminiGenerateContent,
        ] {
            let mut c = super::super::adapt::review16_regressions::candidate();
            c.provider.protocol = protocol;
            c.max_input_tokens = Some(1000);
            let mut body = json!({"input":[
                {"role":"user","content":"Inspect this image and retain the result"},
                {"type":"custom_tool_call","call_id":"image-call","name":"view_image","input":"fixture.png"},
                {"type":"custom_tool_call_output","call_id":"image-call","output":[
                    {"type":"input_image","file_id":"file-opaque-reference"},
                    {"type":"input_text","text":"important result text ".repeat(1000)}]},
                {"role":"user","content":"Continue"}]});
            let original = body.clone();
            let estimate =
                super::super::requests::estimate_input_items(body["input"].as_array().unwrap());
            assert_eq!(estimate.image_count, 1);
            assert_eq!(estimate.image_tokens, super::requests::DEFAULT_IMAGE_TOKENS);
            assert!(estimate.total_tokens > 1000);
            let error = apply_candidate_model_policy(&mut body, &c, true).unwrap_err();
            assert!(matches!(error, CandidatePolicyError::InputBudget(_)));
            assert_eq!(body, original);
            let response = candidate_policy_error_response(&error);
            let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["error"]["code"], "context_length_exceeded");
            c.max_input_tokens = Some(100000);
            apply_candidate_model_policy(&mut body, &c, true).unwrap();
            assert_eq!(body["input"], original["input"]);
        }
    }

    #[test]
    fn review19_reference_estimates_share_rendered_scope_not_data_fields() {
        for part in [
            json!({"type":"input_image","file_id":"file-opaque"}),
            json!({"type":"image","source":{"type":"file","file_id":"file-opaque"}}),
            json!({"type":"image","source":{"type":"url","url":"https://example.invalid/image"}}),
            json!({"fileData":{"mimeType":"image/png","fileUri":"gs://fixture/image"}}),
            json!({"file_data":{"file_uri":"gs://fixture/image"}}),
            json!({"type":"image_url","image_url":{"url":"https://example.invalid/image"}}),
        ] {
            let body = json!({"input":[{"role":"user","content":[part]}]});
            let estimate =
                super::super::requests::estimate_input_items(body["input"].as_array().unwrap());
            assert_eq!(estimate.image_count, 1, "{part}");
            assert_eq!(estimate.image_tokens, super::requests::DEFAULT_IMAGE_TOKENS);
            assert_eq!(crate::translate::count_rendered_sent_images(&body), 1);
            assert_eq!(count_translated_input_images(&body), 0);
        }
        let image = json!({"type":"input_image","file_id":"file-opaque"});
        let body = json!({"tools":[{"parameters":{"const":image}}],"metadata":image,"input":[
            {"type":"custom_tool_call","input":image},
            {"type":"function_call","arguments":image},
            {"role":"user","content":"plain text","metadata":image,"provider_metadata":image},
            {"type":"custom_tool_call_output","output":image},
            {"type":"custom_tool_call_output","output":image.to_string()},
            {"type":"custom_tool_call_output","output":[{"file_id":"ordinary-data"}]},
            {"role":"user","content":[{"fileData":{"mimeType":"application/pdf","fileUri":"gs://fixture/doc"}}]}]});
        assert_eq!(
            super::super::requests::estimate_input_items(body["input"].as_array().unwrap())
                .image_count,
            0
        );
        assert_eq!(crate::translate::count_rendered_sent_images(&body), 0);
        // Malformed declared images are protected until explicit source validation rejects them.
        // They are not ordinary tool data and must not enter the text-only shrink path.
        let malformed =
            json!({"input":[{"role":"user","content":[{"type":"input_image","file_id":""}]}]});
        let protected =
            super::super::requests::estimate_input_items(malformed["input"].as_array().unwrap());
        assert_eq!(protected.image_count, 1);
        assert_eq!(
            protected.image_tokens,
            super::requests::DEFAULT_IMAGE_TOKENS
        );
        assert_eq!(crate::translate::count_rendered_sent_images(&malformed), 1);
        assert_eq!(count_translated_input_images(&malformed), 0);
        let inline = json!({"input":[{"role":"user","content":[
            {"type":"input_image","image_url":format!("data:image/jpeg;base64,{}", "YWJj".repeat(10000))}]}]});
        let estimate =
            super::super::requests::estimate_input_items(inline["input"].as_array().unwrap());
        assert_eq!(estimate.image_count, 1);
        assert!(estimate.text_tokens < 100);
        assert_eq!(count_translated_input_images(&inline), 1);
    }

    pub(crate) struct MockTask(tokio::task::JoinHandle<()>);
    impl Drop for MockTask {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    pub(crate) async fn mock(
        value: Value,
        status: StatusCode,
    ) -> (
        String,
        tokio::sync::mpsc::UnboundedReceiver<Value>,
        MockTask,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let router = axum::Router::new().fallback(move |Json(body): Json<Value>| {
            let tx = tx.clone();
            let value = value.clone();
            async move {
                tx.send(body).unwrap();
                (status, [("x-request-id", "review18-fixture")], Json(value))
            }
        });
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (format!("http://{address}/v1"), rx, MockTask(task))
    }
    /// A custom tool has no meaning without its input, so this is an empty attempt.
    /// The retry is a separate answer: the call the first attempt named must not be
    /// spliced into it, or the good retry would be reported as an invalid turn.
    #[tokio::test]
    async fn custom_call_without_input_is_retried_and_only_the_retry_reaches_the_client() {
        let chat_frame = |delta: Value, finish: Value| {
            format!(
                "data: {}\n\n",
                json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
            )
        };
        let chat_call = |id: &str, arguments: &str| {
            json!({"tool_calls": [{"index": 0, "id": id, "type": "function",
                "function": {"name": "exec", "arguments": arguments}}]})
        };
        let chat_wire = |id: &str, arguments: &str| {
            format!(
                "{}{}data: [DONE]\n\n",
                chat_frame(chat_call(id, arguments), Value::Null),
                chat_frame(json!({}), json!("tool_calls"))
            )
        };
        let gemini_wire = |call: Value| {
            format!(
                "data: {}\n\n",
                json!({"candidates": [{"content": {"parts": [{"functionCall": call}]},
                    "finishReason": "STOP"}]})
            )
        };
        for (protocol, first_wire, retry_wire) in [
            (
                ProviderProtocol::ChatCompletions,
                chat_wire("call_empty", ""),
                chat_wire("call_retry", "{\"input\":\"pwd\"}"),
            ),
            (
                ProviderProtocol::GeminiGenerateContent,
                gemini_wire(json!({"name": "exec"})),
                gemini_wire(json!({"name": "exec", "args": {"input": "pwd"}})),
            ),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let sends = Arc::new(AtomicU32::new(0));
            let router = axum::Router::new().fallback({
                let sends = Arc::clone(&sends);
                move || {
                    let sends = Arc::clone(&sends);
                    let retry_wire = retry_wire.clone();
                    async move {
                        sends.fetch_add(1, Ordering::SeqCst);
                        Response::builder()
                            .status(200)
                            .header(header::CONTENT_TYPE, "text/event-stream")
                            .body(Body::from(retry_wire))
                            .unwrap()
                    }
                }
            });
            let _task = MockTask(tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            }));
            let state = state();
            let mut c = super::super::adapt::review16_regressions::candidate();
            c.provider.base_url = format!("http://{address}/v1");
            c.provider.protocol = protocol;
            c.provider.credential.source = CredentialSource::None;
            c.provider.credential.reference = None;
            c.provider.limits.empty_completion_retries = 1;
            c.capabilities.streaming = true;
            let c = registered_candidate(&state, c).await;
            let body = json!({"model": "test", "stream": true,
                "input": [{"type": "message", "role": "user", "content": "go"}],
                "tools": [{"type": "custom", "name": "exec"}]});
            let raw = Response::builder()
                .status(200)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(reqwest::Body::from(first_wire))
                .unwrap();
            let guarded = guarded_provider_sse(
                state.clone(),
                body.clone(),
                c.clone(),
                None,
                reqwest::Response::from(raw),
                None,
                None,
            )
            .unwrap();
            let seed = ObservationSeed::without_candidate(
                ObservabilityLedger::new(None),
                ObservabilitySettings::default(),
                "custom-retry".into(),
                "test",
                true,
                Instant::now(),
            );
            let adapted = adapt_successful_response(
                guarded,
                &c,
                seed,
                response_tool_map(&body),
                &state.response_state,
                &body,
                false,
                false,
            )
            .await;
            let bytes = to_bytes(adapted.into_body(), 1_000_000).await.unwrap();
            let events = drain_sse_values(&mut Vec::new(), &bytes).unwrap();
            assert_eq!(sends.load(Ordering::SeqCst), 1, "{protocol:?}");
            let terminal = events
                .iter()
                .find(|event| event["type"] == "response.completed")
                .unwrap_or_else(|| panic!("{protocol:?}: {}", String::from_utf8_lossy(&bytes)));
            let output = terminal["response"]["output"].as_array().unwrap();
            assert_eq!(output.len(), 1, "{protocol:?}: {output:?}");
            assert_eq!(output[0]["type"], "custom_tool_call");
            assert_eq!(output[0]["input"], "pwd");
        }
    }

    #[tokio::test]
    async fn review18_url_only_send_wrapper_requires_compaction_without_retry() {
        let error = json!({"error":{"type":"invalid_request_error","message":"too many images"},
            "usage":{"prompt_tokens":100000,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":40000}}});
        let (url, mut received, _task) = mock(error, StatusCode::BAD_REQUEST).await;
        let state = state();
        let mut c = super::super::adapt::review16_regressions::candidate();
        c.provider.base_url = url;
        c.provider.protocol = ProviderProtocol::ChatCompletions;
        c.provider.credential.source = CredentialSource::None;
        c.provider.credential.reference = None;
        c.capabilities.vision = true;
        let c = registered_candidate(&state, c).await;
        let mut body = json!({"model":"test","input":[{"role":"user","content":[
            {"type":"input_image","image_url":"https://example.invalid/image.jpg"}]}]});
        let original = body["input"].clone();
        let failure = send_candidate(&state, &mut body, &c, None)
            .await
            .unwrap_err();
        let wire = match received.try_recv() {
            Ok(wire) => wire,
            Err(error) => {
                let status = failure.response.status();
                let bytes = to_bytes(failure.response.into_body(), 65536).await.unwrap();
                panic!(
                    "no physical request: {error}; status={status}; body={}",
                    String::from_utf8_lossy(&bytes)
                );
            }
        };
        assert_eq!(crate::translate::count_rendered_sent_images(&wire), 1);
        assert_eq!(count_translated_input_images(&wire), 0);
        assert!(received.try_recv().is_err());
        assert_eq!(body["input"], original);
        let retry = failure
            .response
            .extensions()
            .get::<ProviderRetryObservation>()
            .unwrap();
        assert_eq!(retry.additional_sends, 0);
        assert_eq!(retry.usage.input_tokens, 100000);
        assert_eq!(retry.usage.output_tokens, 3);
        assert_eq!(retry.usage.cached_input_tokens, 40000);
        let diagnostic = failure
            .response
            .extensions()
            .get::<crate::observability::RequestDiagnostic>()
            .unwrap();
        assert_eq!(
            diagnostic.upstream_request_id.as_deref(),
            Some("review18-fixture")
        );
        assert!(failure
            .response
            .extensions()
            .get::<UpstreamErrorDiagnostic>()
            .is_some());
        let bytes = to_bytes(failure.response.into_body(), 65536).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "context_length_exceeded");
    }
    #[tokio::test]
    async fn review18_reference_rejection_preserves_retry_usage_and_diagnostics() {
        let mut response = super::super::adapt::review16_regressions::response(vec![Ok(Bytes::from(
            json!({"error":{"message":"too many images"},"usage":{"input_tokens":12,"output_tokens":3}}).to_string()))], StatusCode::BAD_REQUEST);
        response.extensions_mut().insert(SentImageState {
            sent_images: crate::translate::count_rendered_sent_images(&json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.invalid/a.jpg"}}]}]})) });
        response.extensions_mut().insert(ProviderRetryObservation {
            additional_sends: 2,
            recovery_kinds: vec!["provider-5xx".into()],
            usage: TokenUsage {
                input_tokens: 8,
                output_tokens: 2,
                total_tokens: 10,
                ..Default::default()
            },
            ..Default::default()
        });
        response
            .extensions_mut()
            .insert(crate::observability::RequestDiagnostic {
                wire_bytes: 1234,
                ..Default::default()
            });
        let failure = require_compaction_on_image_rejection(response, StatusCode::BAD_REQUEST)
            .await
            .unwrap_err();
        let retry = failure
            .response
            .extensions()
            .get::<ProviderRetryObservation>()
            .unwrap();
        assert_eq!(retry.additional_sends, 2);
        assert_eq!(retry.usage.input_tokens, 20);
        assert_eq!(retry.usage.output_tokens, 5);
        assert_eq!(retry.recovery_kinds, ["provider-5xx"]);
        assert_eq!(
            failure
                .response
                .extensions()
                .get::<crate::observability::RequestDiagnostic>()
                .unwrap()
                .wire_bytes,
            1234
        );
        assert!(failure
            .response
            .extensions()
            .get::<UpstreamErrorDiagnostic>()
            .is_some());
    }
}
