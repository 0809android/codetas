use super::*;

pub(crate) async fn responses(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    let admission = match authorize_request(&state.settings, &headers, "responses:write").await {
        Ok(admission) => admission,
        Err(response) => return response,
    };
    responses_inner(state, headers, body, admission.trusts_turn_metadata()).await
}

pub(crate) async fn responses_inner(
    state: GatewayState,
    headers: HeaderMap,
    body: Value,
    trust_turn_metadata: bool,
) -> Response<Body> {
    responses_inner_with_media(state, headers, body, trust_turn_metadata, true).await
}

pub(crate) async fn responses_inner_without_media(
    state: GatewayState,
    headers: HeaderMap,
    body: Value,
    trust_turn_metadata: bool,
) -> Response<Body> {
    responses_inner_with_media(state, headers, body, trust_turn_metadata, false).await
}

pub(crate) async fn ui_chat(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    let admission = match authorize_ui_chat(&state, &headers).await {
        Ok(admission) => admission,
        Err(response) => return response,
    };

    let streaming = ui_chat_streaming(&body);
    let (model, input, instructions) = match ui_chat_input(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let mut request = json!({
        "model": model,
        "input": input,
        "stream": streaming,
        "_codetas_client_surface": "ui-chat",
    });
    if let Some(instructions) = instructions {
        request["instructions"] = Value::String(instructions);
    }
    // The UI talks to the gateway directly. Do not forward browser admission
    // headers to upstream providers.
    responses_inner(
        state,
        HeaderMap::new(),
        request,
        admission.trusts_turn_metadata(),
    )
    .await
}

#[cfg(test)]
mod ui_chat_streaming_tests {
    use super::*;

    #[test]
    fn ui_chat_uses_requested_streaming_mode() {
        assert!(!ui_chat_streaming(&json!({"model": "m", "messages": []})));
        assert!(ui_chat_streaming(
            &json!({"model": "m", "messages": [], "stream": true})
        ));
        assert!(!ui_chat_streaming(
            &json!({"model": "m", "messages": [], "stream": false})
        ));
    }
}

fn ui_chat_input(body: &Value) -> Result<(String, Vec<Value>, Option<String>), Response<Body>> {
    let Some(model) = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "model is required",
        ));
    };
    let messages = body
        .get("messages")
        .or_else(|| body.get("input"))
        .and_then(Value::as_array)
        .cloned();
    let Some(messages) = messages else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "messages must be an array of role/content objects",
        ));
    };
    let input: Vec<Value> = messages
        .iter()
        .filter_map(|item| {
            let role = item.get("role").and_then(Value::as_str)?;
            let content = item.get("content").and_then(Value::as_str)?;
            Some(json!({
                "role": role,
                "content": content,
            }))
        })
        .collect();
    if input.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "messages must contain at least one message with role and content",
        ));
    }
    let instructions = body
        .get("instructions")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    Ok((model, input, instructions))
}

fn ui_chat_streaming(body: &Value) -> bool {
    body.get("stream").and_then(Value::as_bool).unwrap_or(false)
}

fn request_is_ui_chat(body: &Value) -> bool {
    body.get("_codetas_client_surface")
        .and_then(Value::as_str)
        == Some("ui-chat")
}

fn ui_chat_allows_credential(source: CredentialSource) -> bool {
    source != CredentialSource::Forward
}

fn exclude_forward_credentials_for_ui_chat(
    candidates: Vec<crate::routing::RouteCandidate>,
) -> Result<Vec<crate::routing::RouteCandidate>, String> {
    let filtered = candidates
        .into_iter()
        .filter(|candidate| {
            ui_chat_allows_credential(
                candidate
                    .credential
                    .as_ref()
                    .unwrap_or(&candidate.provider.credential)
                    .source,
            )
        })
        .collect::<Vec<_>>();
    if filtered.is_empty() {
        Err(
            "Bot chat cannot use Codex-forward providers. Choose a model whose provider stores its own credentials."
                .into(),
        )
    } else {
        Ok(filtered)
    }
}

async fn authorize_ui_chat(
    state: &GatewayState,
    headers: &HeaderMap,
) -> Result<Admission, Response<Body>> {
    let security = state.settings.read().await.security.clone();
    if security.require_local_token {
        if let Some(expected) = state.ui_chat_token.0.as_deref() {
            let provided = provided_access_tokens(headers)
                .into_iter()
                .any(|token| constant_time_equal(expected.as_bytes(), token.as_bytes()));
            if provided {
                return Ok(Admission::LocalMaster);
            }
        }
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_gateway_token",
            "a valid CODETAS UI token is required",
        ));
    }
    authorize_request(&state.settings, headers, "responses:write").await
}

#[cfg(test)]
mod ui_chat_tests {
    use super::*;

    #[test]
    fn ui_chat_accepts_messages_or_responses_input() {
        let messages = json!({
            "model": "provider/model",
            "messages": [
                {"role": "user", "content": "one"},
                {"role": "assistant", "content": "two"}
            ]
        });
        let responses_input = json!({
            "model": "provider/model",
            "input": [{"role": "user", "content": "three"}]
        });
        assert_eq!(
            ui_chat_input(&messages).expect("messages are valid"),
            (
                "provider/model".to_string(),
                vec![
                    json!({"role": "user", "content": "one"}),
                    json!({"role": "assistant", "content": "two"})
                ],
                None
            )
        );
        assert_eq!(
            ui_chat_input(&responses_input).expect("Responses input is valid"),
            (
                "provider/model".to_string(),
                vec![json!({"role": "user", "content": "three"})],
                None
            )
        );
    }

    #[test]
    fn ui_chat_rejects_invalid_input() {
        assert!(ui_chat_input(&json!({"messages": []})).is_err());
        assert!(ui_chat_input(&json!({"model": "provider/model"})).is_err());
        assert!(
            ui_chat_input(&json!({"model": "provider/model", "messages": [
                {"role": "user"}
            ]}))
            .is_err()
        );
        assert!(
            ui_chat_input(&json!({"model": "provider/model", "messages": [
                {"role": "user", "content": 42}
            ]}))
            .is_err()
        );
    }

    #[test]
    fn ui_chat_rejects_codex_forward_credentials() {
        assert!(request_is_ui_chat(&json!({"_codetas_client_surface": "ui-chat"})));
        assert!(!request_is_ui_chat(&json!({})));
        assert!(!ui_chat_allows_credential(CredentialSource::Forward));
        assert!(ui_chat_allows_credential(CredentialSource::OAuth));
        assert!(ui_chat_allows_credential(CredentialSource::Command));
        assert!(ui_chat_allows_credential(CredentialSource::Environment));
    }
}

#[allow(clippy::too_many_arguments)]
fn observe_candidate_preflight_failure(
    state: &GatewayState,
    observability_settings: &ObservabilitySettings,
    request_id: &str,
    streaming: bool,
    request_started: Instant,
    candidate_started: Instant,
    attempts: u16,
    candidate: &RouteCandidate,
    status: StatusCode,
    category: &str,
    has_next: bool,
    continuation_recovery: Option<&str>,
) {
    let mut observation = ObservationSeed::for_candidate(
        state.observability.clone(),
        observability_settings.clone(),
        request_id.to_string(),
        streaming,
        if has_next {
            candidate_started
        } else {
            request_started
        },
        attempts,
        candidate,
    )
    .with_recovery(continuation_recovery);
    // Preflight failures select a candidate but never send its main request.
    observation.send_count = 0;
    if has_next {
        observation = observation.as_attempt();
    }
    observation.finish(status, Some(category), TokenUsage::default());
}

fn repair_replayed_tool_outputs(
    body: &mut Value,
    response_state: &ResponseStateStore,
    response_id: &str,
) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    let mut repairs = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let Some(call_type) = (match item.get("type").and_then(Value::as_str) {
            Some("function_call_output") => Some("function_call"),
            Some("custom_tool_call_output") => Some("custom_tool_call"),
            Some("tool_search_output") => Some("tool_search_call"),
            Some("local_shell_call_output") => Some("local_shell_call"),
            _ => None,
        }) else {
            continue;
        };
        let Some(call_id) = item
            .get("call_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let already_paired = items.iter().any(|other| {
            other.get("type").and_then(Value::as_str) == Some(call_type)
                && other.get("call_id").and_then(Value::as_str) == Some(call_id)
        });
        if already_paired {
            continue;
        }
        if let Some(call) =
            response_state.find_tool_call_in_response(response_id, call_id, call_type)
        {
            repairs.push((index, call));
        }
    }
    for (index, call) in repairs.into_iter().rev() {
        items.insert(index, call);
    }
}

async fn responses_inner_with_media(
    state: GatewayState,
    headers: HeaderMap,
    mut body: Value,
    trust_turn_metadata: bool,
    preprocess_media: bool,
) -> Response<Body> {
    let started = Instant::now();
    let request_id = Uuid::new_v4().to_string();
    let ui_chat = request_is_ui_chat(&body);
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let claims_subagent = is_subagent_request(&headers);
    let codex_client = is_codex_request(&headers);
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if request_is_remote_compaction(&body) {
        if let Some(items) = body.get("input").and_then(Value::as_array) {
            use std::collections::BTreeMap;
            let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
            for item in items {
                let t = item.get("type").and_then(Value::as_str).unwrap_or("?");
                *counts.entry(t).or_default() += 1;
            }
            let summary = counts
                .iter()
                .map(|(k, v)| format!("{k}:{v}"))
                .collect::<Vec<_>>()
                .join(" ");
            crate::debug::log_always(&format!(
                "COMPACTION request_id={} model={} stream={} input=[{}] total={} previous_response_id={}",
                request_id,
                requested_model,
                streaming,
                summary,
                items.len(),
                body.get("previous_response_id")
                    .and_then(Value::as_str)
                    .is_some(),
            ));
        } else {
            crate::debug::log_always("COMPACTION request: NO input array");
        }
        return compact_response_from_responses(State(state), headers, Json(body)).await;
    }
    let (candidates, observability_settings, effort_cap, is_subagent) = async {
        let settings = state.settings.read().await;
        // Turn metadata is only a routing authority after the caller has
        // passed admission authentication. Without it, the header is advisory
        // and cannot select a different cost/model policy.
        let is_subagent = claims_subagent && trust_turn_metadata;
        let observability_settings = settings.observability.clone();
        let effort_cap = if is_subagent {
            settings
                .agents
                .subagent_effort_cap
                .clone()
                .or_else(|| settings.agents.effort_cap.clone())
        } else {
            settings.agents.effort_cap.clone()
        };
        let desktop_target = claude_desktop_target(&headers, &settings, &requested_model);
        let effective_model = desktop_target.as_deref().unwrap_or(&requested_model);
        // Soft failures recorded for the resolved candidates are scoped to this
        // session (task/thread) so one session's provider trouble does not cool
        // the same target for every session.
        let session_scope = crate::response_state::session_key_from_headers(&headers);
        let mut routing = state.routing.lock().await;
        let mut candidates = routing.candidates_for_request(
            &settings,
            effective_model,
            is_subagent,
            session_scope.as_deref(),
        );
        if desktop_target.is_some() {
            if let Ok(candidates) = candidates.as_mut() {
                for candidate in candidates {
                    candidate.exposed_model = requested_model.clone();
                }
            }
        }
        (candidates, observability_settings, effort_cap, is_subagent)
    }
    .await;
    // Replay the locally cached continuation history for `previous_response_id`
    // before routing. ChatGPT Codex and translated/stateless providers reject or
    // cannot resolve that field, so they need a local expand. Stateful Responses
    // providers can resolve the id themselves; a local miss must not strip it
    // and continue as a delta. Compaction turns are excluded above.
    let previous_response_id = body
        .get("previous_response_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let had_previous_response = previous_response_id.is_some();
    // Drop any client-injected control fields before gateway-owned expand.
    crate::response_state::ResponseStateStore::strip_private_fields(&mut body);
    let session_hint = crate::response_state::session_key_from_headers(&headers);
    let needs_local_previous_response = match &candidates {
        Ok(list) => route_needs_local_previous_response(list),
        Err(_) => true,
    };
    // ChatGPT Codex and official OpenAI Responses resolve continuation
    // themselves. Local expand would replay the full history and drop the id,
    // which is the opposite of Codex CLI.
    let (expand_outcome, expand_attempts, replayed_response_id) =
        if needs_local_previous_response {
            expand_previous_response_for_request(
                &state.response_state,
                &mut body,
                session_hint.as_deref(),
                previous_response_id.as_deref(),
            )
            .await
        } else if previous_response_id.is_some() {
            (
                crate::response_state::ExpandOutcome::Miss("stateful-forward"),
                0,
                None,
            )
        } else {
            (
                crate::response_state::ExpandOutcome::NotRequested,
                0,
                None,
            )
        };
    let used_session_tip = replayed_response_id
        .as_deref()
        .is_some_and(|id| previous_response_id.as_deref() != Some(id));
    let continuation = plan_continuation(
        expand_outcome,
        needs_local_previous_response,
        expand_attempts,
        used_session_tip,
    );
    let expanded_previous = continuation.expanded;
    let continuation_recovery = continuation.recovery.clone();
    if had_previous_response {
        crate::debug::log_always(&format!(
            "continuation request_id={} previous_response_id={} outcome={} keep_id={} needs_local={} attempts={} session_tip={} model={} input=[{}]",
            request_id,
            previous_response_id.as_deref().unwrap_or(""),
            continuation.outcome,
            continuation.keep_previous_response_id,
            needs_local_previous_response,
            expand_attempts,
            used_session_tip,
            requested_model,
            input_item_summary(&body),
        ));
    }
    if !continuation.keep_previous_response_id {
        if let Some(object) = body.as_object_mut() {
            object.remove("previous_response_id");
        }
    }
    if !expanded_previous {
        if let Some(hint) = session_hint.as_deref() {
            // Root turns still benefit from session-scoped storage.
            state.response_state.attach_session_hint(&mut body, hint);
        }
    }
    if expanded_previous {
        if let Some(response_id) = replayed_response_id.as_deref() {
            repair_replayed_tool_outputs(&mut body, &state.response_state, response_id);
        }
    }
    // Keep `_codetas_*` control fields on `body` until `remember` consumes the
    // replay lease. Wire path strips them on the outbound provider copy only.

    // Orphan tool-output repair is provider-specific. A replay hit may restore a
    // missing pair from that exact previous-response entry; no global call_id
    // search is allowed because call IDs are not conversation keys. The native
    // sanitizer and translated adapters convert genuinely orphaned outputs into
    // user messages when their target requires that recovery.
    let input_summary = input_item_summary(&body);
    crate::debug::log_always(&format!(
        "request model={} had_prev={} expanded={} recovery={} tools={} input=[{}]",
        requested_model,
        had_previous_response,
        expanded_previous,
        continuation_recovery.as_deref().unwrap_or("-"),
        body.get("tools")
            .and_then(serde_json::Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0),
        input_summary
    ));
    if let Some(tools) = body.get("tools").and_then(serde_json::Value::as_array) {
        for tool in tools.iter().take(5) {
            let name = tool
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let desc = tool
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if name == "exec" {
                crate::debug::log(&format!("  EXEC_DESC_BEGIN"));
                crate::debug::log(desc);
                crate::debug::log(&format!("  EXEC_DESC_END len={}", desc.len()));
            } else {
                let short: String = desc.chars().take(120).collect();
                crate::debug::log(&format!("  tool: {} | {}", name, short));
            }
        }
    }
    // Local recording is only for providers that cannot keep `previous_response_id`.
    // A missed expand must not become a new Exact checkpoint of the truncated delta.
    let record_eligible = true;
    if let Some(cap) = effort_cap.as_deref() {
        cap_reasoning_effort(&mut body, cap);
    }
    let candidates = match candidates {
        Ok(candidates) if ui_chat => match exclude_forward_credentials_for_ui_chat(candidates) {
            Ok(candidates) => candidates,
            Err(message) => {
                ObservationSeed::without_candidate(
                    state.observability.clone(),
                    observability_settings,
                    request_id,
                    &requested_model,
                    streaming,
                    started,
                )
                .with_recovery(continuation_recovery.as_deref())
                .finish(
                    StatusCode::BAD_REQUEST,
                    Some("ui_chat_forward_unsupported"),
                    TokenUsage::default(),
                );
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "ui_chat_forward_unsupported",
                    &message,
                );
            }
        },
        Ok(candidates) => candidates,
        Err(message) => {
            // A provider target that is cooling down is a transient backoff
            // condition, not an invalid request. Signal it with 503 +
            // Retry-After so compliant clients retry instead of surfacing an
            // error to the user.
            let (status, category, code): (StatusCode, &str, &str) =
                if is_cooldown_rejection(&message) {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        "provider_cooling_down",
                        "provider_cooling_down",
                    )
                } else {
                    (
                        StatusCode::BAD_REQUEST,
                        "routing_rejected",
                        "invalid_request",
                    )
                };
            ObservationSeed::without_candidate(
                state.observability.clone(),
                observability_settings,
                request_id,
                &requested_model,
                streaming,
                started,
            )
            .with_recovery(continuation_recovery.as_deref())
            .finish(status, Some(category), TokenUsage::default());
            if status == StatusCode::SERVICE_UNAVAILABLE {
                return cooldown_response_for_message(&message);
            }
            return error_response(StatusCode::BAD_REQUEST, code, &message);
        }
    };
    let mut last_failure = None;
    for (index, candidate) in candidates.iter().enumerate() {
        let attempts = (index + 1).min(usize::from(u16::MAX)) as u16;
        let has_next = index + 1 < candidates.len();
        let candidate_started = Instant::now();
        let mut candidate_body = body.clone();
        if let Err(error) =
            apply_candidate_model_policy(&mut candidate_body, candidate, is_subagent)
        {
            observe_candidate_preflight_failure(
                &state,
                &observability_settings,
                &request_id,
                streaming,
                started,
                candidate_started,
                attempts,
                candidate,
                StatusCode::BAD_REQUEST,
                "model_limit_exceeded",
                has_next,
                continuation_recovery.as_deref(),
            );
            if has_next {
                last_failure = Some(candidate_policy_error_response(&error));
                continue;
            }
            return candidate_policy_error_response(&error);
        }
        if preprocess_media {
            if let Err(response) =
                prepare_candidate_media_input(&state, &headers, &mut candidate_body, candidate)
                    .await
            {
                let status = response.status();
                let category = response
                    .extensions()
                    .get::<MediaPreprocessingFailureCategory>()
                    .map(|category| category.0)
                    .unwrap_or("media_preprocessing_failed");
                observe_candidate_preflight_failure(
                    &state,
                    &observability_settings,
                    &request_id,
                    streaming,
                    started,
                    candidate_started,
                    attempts,
                    candidate,
                    status,
                    category,
                    has_next,
                    continuation_recovery.as_deref(),
                );
                if has_next {
                    last_failure = Some(response);
                    continue;
                }
                return response;
            }
            if let Err(error) =
                apply_candidate_model_policy(&mut candidate_body, candidate, is_subagent)
            {
                observe_candidate_preflight_failure(
                    &state,
                    &observability_settings,
                    &request_id,
                    streaming,
                    started,
                    candidate_started,
                    attempts,
                    candidate,
                    StatusCode::BAD_REQUEST,
                    "model_limit_exceeded",
                    has_next,
                    continuation_recovery.as_deref(),
                );
                if has_next {
                    last_failure = Some(candidate_policy_error_response(&error));
                    continue;
                }
                return candidate_policy_error_response(&error);
            }
        }
        let tool_map = response_tool_map(&candidate_body);
        let upstream = match send_candidate(&state, &mut candidate_body, candidate, Some(&headers))
            .await
        {
            Ok(upstream) => upstream,
            Err(failure) => {
                let provider_retry = failure
                    .response
                    .extensions()
                    .get::<ProviderRetryObservation>()
                    .cloned();
                // Credential acquisition can briefly race provider startup
                // (notably Antigravity's CLI-backed token refresh). Keep the
                // account failover below, but do not turn those temporary 401s
                // into provider-unreachable strikes and a 60-second cooldown.
                if failure.kind.feeds_cooldown() {
                    state.routing.lock().await.record_failure(candidate);
                }
                let can_retry = match failure.kind {
                    AttemptFailureKind::Credential => candidates[index + 1..].iter().any(|next| {
                        next.target_key == candidate.target_key && next.account_id.is_some()
                    }),
                    AttemptFailureKind::Retryable => has_next,
                    AttemptFailureKind::ContextWindow => has_next,
                    AttemptFailureKind::Request => false,
                };
                if can_retry {
                    let status = failure.response.status();
                    let mut observation = ObservationSeed::for_candidate(
                        state.observability.clone(),
                        observability_settings.clone(),
                        request_id.clone(),
                        streaming,
                        candidate_started,
                        attempts,
                        candidate,
                    )
                    .with_recovery(continuation_recovery.as_deref());
                    if let Some(retry) = provider_retry.as_ref() {
                        observation.record_provider_retries(retry);
                    }
                    observation.as_attempt().finish(
                        status,
                        Some(failure.kind.category()),
                        TokenUsage::default(),
                    );
                    last_failure = Some(failure.response);
                    continue;
                }
                let status = failure.response.status();
                let mut observation = ObservationSeed::for_candidate(
                    state.observability.clone(),
                    observability_settings.clone(),
                    request_id.clone(),
                    streaming,
                    started,
                    attempts,
                    candidate,
                )
                .with_recovery(continuation_recovery.as_deref());
                if let Some(retry) = provider_retry.as_ref() {
                    observation.record_provider_retries(retry);
                }
                observation.finish(status, Some(failure.kind.category()), TokenUsage::default());
                return failure.response;
            }
        };
        let provider_retry = upstream
            .extensions()
            .get::<ProviderRetryObservation>()
            .cloned();
        let shared_provider_retries = upstream
            .extensions()
            .get::<SharedProviderRetryObservation>()
            .cloned();
        if let Some(recovery_failure) = upstream
            .extensions()
            .get::<EmptyCompletionRecoveryFailure>()
            .cloned()
        {
            let status = upstream.status();
            state.routing.lock().await.record_failure(candidate);
            let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
            let bytes = read_bounded(upstream, 64 * 1024).await.unwrap_or_else(|_| {
                Bytes::from_static(b"{\"error\":{\"code\":\"empty_completion_retry_failed\",\"message\":\"The empty-completion retry failed.\"}}").to_vec()
            });
            let mut observation = ObservationSeed::for_candidate(
                state.observability.clone(),
                observability_settings.clone(),
                request_id.clone(),
                streaming,
                started,
                attempts,
                candidate,
            )
            .with_recovery(continuation_recovery.as_deref());
            if let Some(retry) = provider_retry.as_ref() {
                let mut metadata = retry.clone();
                // The dedicated failure marker already carries the complete
                // aggregate usage in its body/EmptyCompletionRecoveryFailure.
                metadata.usage = TokenUsage::default();
                observation.record_provider_retries(&metadata);
            } else {
                observation.record_recovery("empty-completion");
                observation.send_count = observation
                    .send_count
                    .saturating_add(recovery_failure.additional_sends);
            }
            observation.finish(
                status,
                Some(EMPTY_COMPLETION_RETRY_FAILED_CODE),
                recovery_failure.usage,
            );
            let mut response = Response::builder().status(status);
            if let (Some(headers), Some(content_type)) = (response.headers_mut(), content_type) {
                headers.insert(header::CONTENT_TYPE, content_type);
            }
            return response.body(Body::from(bytes)).unwrap_or_else(|_| {
                error_response(
                    StatusCode::BAD_GATEWAY,
                    EMPTY_COMPLETION_RETRY_FAILED_CODE,
                    "The empty-completion retry failed.",
                )
            });
        }
        if !upstream.status().is_success() {
            let status = upstream.status();
            let account_retry = matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
                && candidates[index + 1..].iter().any(|next| {
                    next.target_key == candidate.target_key && next.account_id.is_some()
                });
            let transient = status == StatusCode::REQUEST_TIMEOUT
                || status == StatusCode::TOO_MANY_REQUESTS
                || status.is_server_error();
            let retry_after = validated_retry_after(upstream.headers());
            if status == StatusCode::TOO_MANY_REQUESTS {
                state.routing.lock().await.record_quota_exhausted(
                    candidate,
                    retry_after.as_ref().and_then(|value| value.1),
                );
            } else if upstream_status_feeds_cooldown(status) {
                // Authentication failures may select another configured
                // account, but they do not prove that the provider target is
                // unreachable. Only transport/server failures feed the
                // three-strike cooldown.
                state.routing.lock().await.record_failure(candidate);
            }
            let classified = upstream_responses_error_classified(
                upstream,
                retry_after.as_ref().map(|value| &value.0),
            )
            .await;
            let response = classified.response;
            let provider_error_usage = classified.usage;
            if has_next && (account_retry || transient) {
                let mut observation = ObservationSeed::for_candidate(
                    state.observability.clone(),
                    observability_settings.clone(),
                    request_id.clone(),
                    streaming,
                    candidate_started,
                    attempts,
                    candidate,
                )
                .with_recovery(continuation_recovery.as_deref());
                if let Some(retry) = provider_retry.as_ref() {
                    observation.record_provider_retries(retry);
                }
                observation.record_upstream_error(&response);
                observation.as_attempt().finish(
                    status,
                    Some("provider_http_error"),
                    provider_error_usage,
                );
                last_failure = Some(response);
                continue;
            }
            let mut observation = ObservationSeed::for_candidate(
                state.observability.clone(),
                observability_settings.clone(),
                request_id.clone(),
                streaming,
                started,
                attempts,
                candidate,
            )
            .with_recovery(continuation_recovery.as_deref());
            if let Some(retry) = provider_retry.as_ref() {
                observation.record_provider_retries(retry);
            }
            observation.record_upstream_error(&response);
            observation.finish(status, Some("provider_http_error"), provider_error_usage);
            return response;
        }

        let quota = if candidate.quota_threshold_percent == 0 {
            None
        } else {
            quota_usage_percent(upstream.headers())
        };
        state.routing.lock().await.record_success(candidate, quota);
        schedule_shadow_calls(
            state.clone(),
            body.clone(),
            request_id.clone(),
            requested_model.clone(),
            candidate.clone(),
        );
        let recovered_empty_completion = upstream
            .extensions()
            .get::<EmptyCompletionRecoverySuccess>()
            .copied();
        let mut observation = ObservationSeed::for_candidate(
            state.observability.clone(),
            observability_settings.clone(),
            request_id.clone(),
            streaming,
            started,
            attempts,
            candidate,
        )
        .with_recovery(continuation_recovery.as_deref());
        if let Some(retry) = provider_retry.as_ref() {
            observation.record_provider_retries(retry);
        }
        observation.record_reqwest_upstream_error(&upstream);
        if let Some(retry) = shared_provider_retries {
            observation.record_shared_provider_retries(retry);
        }
        if let Some(recovery) = recovered_empty_completion {
            if provider_retry.is_none() {
                observation.record_recovery("empty-completion");
                observation.send_count = observation
                    .send_count
                    .saturating_add(recovery.additional_sends);
            }
        }
        return adapt_successful_response(
            upstream,
            candidate,
            observation,
            tool_map,
            &state.response_state,
            &body,
            record_eligible,
            codex_client,
        )
        .await;
    }
    let response = last_failure.unwrap_or_else(|| {
        error_response(
            StatusCode::BAD_GATEWAY,
            "provider_unavailable",
            "no provider route completed the request",
        )
    });
    ObservationSeed::without_candidate(
        state.observability.clone(),
        observability_settings,
        request_id,
        &requested_model,
        streaming,
        started,
    )
    .with_recovery(continuation_recovery.as_deref())
    .finish(
        response.status(),
        Some("provider_unavailable"),
        TokenUsage::default(),
    );
    response
}

fn upstream_status_feeds_cooldown(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT || status.is_server_error()
}

const CONTINUATION_EXPAND_ATTEMPTS: u8 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContinuationPlan {
    expanded: bool,
    keep_previous_response_id: bool,
    outcome: &'static str,
    recovery: Option<String>,
}

fn input_item_summary(body: &Value) -> String {
    body.get("input")
        .and_then(Value::as_array)
        .map(|items| {
            use std::collections::BTreeMap;
            let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
            for item in items {
                let item_type = item.get("type").and_then(Value::as_str).unwrap_or("?");
                *counts.entry(item_type).or_default() += 1;
            }
            counts
                .iter()
                .map(|(key, value)| format!("{key}:{value}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

fn candidate_needs_local_previous_response(candidate: &RouteCandidate) -> bool {
    candidate.provider.stateless_responses
        || candidate
            .provider
            .protocol_for_model(&candidate.upstream_model)
            != ProviderProtocol::Responses
}

fn route_needs_local_previous_response(candidates: &[RouteCandidate]) -> bool {
    !candidates.is_empty()
        && candidates
            .iter()
            .all(candidate_needs_local_previous_response)
}

fn plan_continuation(
    outcome: crate::response_state::ExpandOutcome,
    needs_local: bool,
    attempts: u8,
    used_session_tip: bool,
) -> ContinuationPlan {
    match outcome {
        crate::response_state::ExpandOutcome::NotRequested => ContinuationPlan {
            expanded: false,
            keep_previous_response_id: false,
            outcome: "not_requested",
            recovery: None,
        },
        crate::response_state::ExpandOutcome::Exact => ContinuationPlan {
            expanded: true,
            keep_previous_response_id: false,
            outcome: "hit",
            recovery: if used_session_tip {
                Some("continuation-session-tip".into())
            } else if attempts > 1 {
                Some(format!("continuation-retry:{attempts}"))
            } else {
                None
            },
        },
        crate::response_state::ExpandOutcome::Lossy => ContinuationPlan {
            expanded: true,
            keep_previous_response_id: false,
            outcome: "lossy",
            recovery: Some("continuation-lossy".into()),
        },
        crate::response_state::ExpandOutcome::Miss(reason) if needs_local => ContinuationPlan {
            expanded: false,
            keep_previous_response_id: false,
            outcome: "local_miss",
            recovery: Some(format!("continuation-local-miss:{reason}:{attempts}")),
        },
        crate::response_state::ExpandOutcome::Miss(reason) => ContinuationPlan {
            expanded: false,
            keep_previous_response_id: true,
            outcome: "forward",
            recovery: Some(format!("continuation-forward:{reason}:{attempts}")),
        },
    }
}

async fn expand_previous_response_for_request(
    store: &crate::response_state::ResponseStateStore,
    body: &mut Value,
    session_hint: Option<&str>,
    original_previous_id: Option<&str>,
) -> (crate::response_state::ExpandOutcome, u8, Option<String>) {
    if original_previous_id.is_none() {
        return (crate::response_state::ExpandOutcome::NotRequested, 0, None);
    }
    let mut outcome = store.expand_previous_response_input_with_hint(body, session_hint);
    let mut attempts = 1;
    while matches!(outcome, crate::response_state::ExpandOutcome::Miss(_))
        && attempts < CONTINUATION_EXPAND_ATTEMPTS
    {
        attempts = attempts.saturating_add(1);
        tokio::time::sleep(Duration::from_millis(50 * u64::from(attempts))).await;
        outcome = store.expand_previous_response_input_with_hint(body, session_hint);
    }
    if outcome.expanded() {
        return (outcome, attempts, original_previous_id.map(str::to_owned));
    }
    let Some(session_hint) = session_hint else {
        return (outcome, attempts, None);
    };
    let Some(tip) = store.unique_live_tip(session_hint) else {
        return (outcome, attempts, None);
    };
    if original_previous_id == Some(tip.as_str()) {
        return (outcome, attempts, None);
    }
    if let Some(object) = body.as_object_mut() {
        object.insert("previous_response_id".into(), json!(tip.clone()));
    }
    let tip_outcome = store.expand_previous_response_input_with_hint(body, Some(session_hint));
    if tip_outcome.expanded() {
        crate::debug::log_always(&format!(
            "continuation recovered unique session tip previous_response_id={tip}"
        ));
        return (tip_outcome, attempts, Some(tip));
    }
    if let Some(original) = original_previous_id {
        if let Some(object) = body.as_object_mut() {
            object.insert("previous_response_id".into(), json!(original));
        }
    }
    (outcome, attempts, None)
}

#[cfg(test)]
mod cooldown_classification_tests {
    use super::*;

    #[test]
    fn credential_failures_do_not_feed_provider_cooldown() {
        assert!(!AttemptFailureKind::Credential.feeds_cooldown());
        assert!(AttemptFailureKind::Retryable.feeds_cooldown());
    }

    #[test]
    fn authentication_statuses_do_not_feed_provider_cooldown() {
        assert!(!upstream_status_feeds_cooldown(StatusCode::UNAUTHORIZED));
        assert!(!upstream_status_feeds_cooldown(StatusCode::FORBIDDEN));
        assert!(!upstream_status_feeds_cooldown(
            StatusCode::TOO_MANY_REQUESTS
        ));
        assert!(upstream_status_feeds_cooldown(StatusCode::REQUEST_TIMEOUT));
        assert!(upstream_status_feeds_cooldown(
            StatusCode::SERVICE_UNAVAILABLE
        ));
    }
}

#[cfg(test)]
mod continuation_plan_tests {
    use super::*;
    use crate::config::{ProviderCredential, ProviderDefinition};
    use crate::response_state::ExpandOutcome;

    fn candidate_with(
        protocol: ProviderProtocol,
        stateless: bool,
        forward: bool,
        base_url: &str,
    ) -> RouteCandidate {
        let mut provider = ProviderDefinition::default();
        provider.id = "test".into();
        provider.protocol = protocol;
        provider.stateless_responses = stateless;
        provider.base_url = base_url.into();
        if forward {
            provider.credential = ProviderCredential {
                source: CredentialSource::Forward,
                ..ProviderCredential::default()
            };
        }
        let capabilities = provider.capabilities.clone();
        RouteCandidate {
            provider,
            upstream_model: "gpt-5.6-sol".into(),
            exposed_model: "gpt-5.6-sol".into(),
            credential: None,
            account_id: None,
            target_key: "test".into(),
            route_id: None,
            failure_threshold: 0,
            quota_threshold_percent: 0,
            input_price_per_million: None,
            output_price_per_million: None,
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            capabilities,
            routing_epoch: 0,
            routing_generation: 0,
            session_scope: None,
        }
    }

    #[test]
    fn stateful_openai_does_not_need_local_replay() {
        let candidate = candidate_with(
            ProviderProtocol::Responses,
            false,
            false,
            "https://api.openai.com/v1",
        );
        assert!(!candidate_needs_local_previous_response(&candidate));
        assert!(!route_needs_local_previous_response(&[candidate]));
    }

    #[test]
    fn chatgpt_codex_forwards_previous_response_like_cli() {
        let chatgpt = candidate_with(
            ProviderProtocol::Responses,
            false,
            true,
            "https://chatgpt.com/backend-api/codex",
        );
        assert!(!candidate_needs_local_previous_response(&chatgpt));
        assert!(!route_needs_local_previous_response(&[chatgpt]));
    }

    #[test]
    fn xai_chat_needs_local_replay() {
        let xai = candidate_with(
            ProviderProtocol::ChatCompletions,
            false,
            false,
            "https://api.x.ai/v1",
        );
        assert!(candidate_needs_local_previous_response(&xai));
        assert!(route_needs_local_previous_response(&[xai]));
    }

    #[test]
    fn miss_on_stateful_openai_keeps_previous_response_id() {
        let plan = plan_continuation(ExpandOutcome::Miss("unknown_id"), false, 3, false);
        assert!(!plan.expanded);
        assert!(plan.keep_previous_response_id);
        assert_eq!(plan.outcome, "forward");
        assert_eq!(
            plan.recovery.as_deref(),
            Some("continuation-forward:unknown_id:3")
        );
    }

    #[test]
    fn miss_on_local_only_route_strips_id_without_rebasing() {
        let plan = plan_continuation(ExpandOutcome::Miss("unknown_id"), true, 3, false);
        assert!(!plan.expanded);
        assert!(!plan.keep_previous_response_id);
        assert_eq!(plan.outcome, "local_miss");
        assert_eq!(
            plan.recovery.as_deref(),
            Some("continuation-local-miss:unknown_id:3")
        );
    }

    #[test]
    fn hit_strips_previous_response_id() {
        let plan = plan_continuation(ExpandOutcome::Exact, true, 1, false);
        assert!(plan.expanded);
        assert!(!plan.keep_previous_response_id);
        assert_eq!(plan.outcome, "hit");
        assert!(plan.recovery.is_none());
    }
}

pub(crate) fn claude_desktop_target(
    headers: &HeaderMap,
    settings: &GatewaySettings,
    requested_model: &str,
) -> Option<String> {
    let client = headers
        .get("x-codetas-client")
        .and_then(|value| value.to_str().ok())?;
    if client != "claude-desktop" || !settings.integrations.claude_desktop {
        return None;
    }
    settings
        .integrations
        .claude_desktop_aliases
        .get(requested_model)
        .cloned()
}

pub(crate) fn is_subagent_request(headers: &HeaderMap) -> bool {
    let Some(raw) = headers
        .get("x-codex-turn-metadata")
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 8 * 1024)
    else {
        return false;
    };
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|metadata| {
            metadata
                .get("thread_source")
                .or_else(|| metadata.pointer("/turn/thread_source"))
                .and_then(Value::as_str)
                .map(|source| source == "subagent")
        })
        .unwrap_or(false)
}

pub(crate) fn is_codex_request(headers: &HeaderMap) -> bool {
    [
        "x-codex-turn-metadata",
        "x-codex-turn-state",
        "x-codex-installation-id",
    ]
    .into_iter()
    .any(|name| headers.contains_key(name))
}

pub(crate) fn cap_reasoning_effort(body: &mut Value, cap: &str) {
    let requested = body
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(requested) = requested else { return };
    let Some(cap_rank) = reasoning_rank(cap) else {
        return;
    };
    if reasoning_rank(&requested).is_some_and(|rank| rank > cap_rank) {
        if let Some(reasoning) = body.get_mut("reasoning").and_then(Value::as_object_mut) {
            reasoning.insert("effort".into(), Value::String(cap.to_string()));
        }
    }
}

pub(crate) fn reasoning_rank(value: &str) -> Option<u8> {
    match value {
        "none" => Some(0),
        "minimal" => Some(1),
        "low" => Some(2),
        "medium" => Some(3),
        "high" => Some(4),
        "xhigh" => Some(5),
        "max" => Some(6),
        "ultra" => Some(7),
        _ => None,
    }
}

#[derive(Debug)]
pub(crate) enum CandidatePolicyError {
    InputBudget(String),
    Invalid(String),
}

impl std::fmt::Display for CandidatePolicyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InputBudget(message) | Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

fn candidate_policy_error_response(error: &CandidatePolicyError) -> Response<Body> {
    match error {
        CandidatePolicyError::InputBudget(message) => context_window_exceeded_response(message),
        CandidatePolicyError::Invalid(message) => {
            error_response(StatusCode::BAD_REQUEST, "model_limit_exceeded", message)
        }
    }
}

pub(crate) fn apply_candidate_model_policy(
    body: &mut Value,
    candidate: &RouteCandidate,
    is_subagent: bool,
) -> Result<(), CandidatePolicyError> {
    let requested_output = body.get("max_output_tokens").and_then(Value::as_u64);
    let effective_output = match (requested_output, candidate.max_output_tokens) {
        (Some(requested), Some(limit)) => {
            let value = requested.min(limit);
            body["max_output_tokens"] = json!(value);
            value
        }
        (None, Some(limit)) => {
            body["max_output_tokens"] = json!(limit);
            limit
        }
        (Some(requested), None) => requested,
        (None, None) => 0,
    };
    // Subagent turns inherit the parent conversation history through the
    // collaboration transport. When that history exceeds the subagent model's
    // context budget, shrink the input instead of failing the turn — the
    // parent's own budget stays untouched.
    if is_subagent {
        if shrink_subagent_input(body, candidate, effective_output)
            .map_err(CandidatePolicyError::Invalid)?
        {
            eprintln!(
                "CODETAS Gateway: shrunk subagent input to fit {}/{} budget",
                candidate.provider.id, candidate.upstream_model
            );
        }
    } else if !request_is_remote_compaction(body)
        || candidate_compaction_mode(candidate, CompactionRequestKind::NativeTrigger)
            == CompactionMode::Local
    {
        trim_inline_images_to_input_budget(body, candidate, effective_output);
        enforce_pathological_input_budget(body, candidate, effective_output)?;
    }

    let requested_effort = body
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mapped = if candidate.reasoning_efforts.is_empty() {
        candidate.default_reasoning_effort.clone()
    } else if let Some(requested) = requested_effort.as_deref() {
        if candidate
            .reasoning_efforts
            .iter()
            .any(|effort| effort == requested)
        {
            Some(requested.to_string())
        } else {
            let requested_rank = reasoning_rank(requested).unwrap_or(u8::MAX);
            candidate
                .reasoning_efforts
                .iter()
                .filter_map(|effort| reasoning_rank(effort).map(|rank| (rank, effort)))
                .filter(|(rank, _)| *rank <= requested_rank)
                .max_by_key(|(rank, _)| *rank)
                .or_else(|| {
                    candidate
                        .reasoning_efforts
                        .iter()
                        .filter_map(|effort| reasoning_rank(effort).map(|rank| (rank, effort)))
                        .min_by_key(|(rank, _)| *rank)
                })
                .map(|(_, effort)| effort.clone())
        }
    } else {
        candidate
            .default_reasoning_effort
            .clone()
            .or_else(|| candidate.reasoning_efforts.first().cloned())
    };
    if let Some(mapped) = mapped {
        if !body.get("reasoning").is_some_and(Value::is_object) {
            body["reasoning"] = json!({});
        }
        body["reasoning"]["effort"] = Value::String(mapped);
    }
    Ok(())
}

fn usable_input_token_limit(
    candidate: &RouteCandidate,
    reserved_output_tokens: u64,
) -> Option<u64> {
    let context_input = candidate
        .context_window
        .map(|context| context.saturating_sub(reserved_output_tokens));
    match (candidate.max_input_tokens, context_input) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(limit), None) | (None, Some(limit)) => Some(limit),
        (None, None) => None,
    }
}

fn current_input_estimate(body: &Value) -> InputTokenEstimate {
    body.get("input")
        .and_then(Value::as_array)
        .map(|items| estimate_input_items(items))
        .unwrap_or_default()
}

/// Drop oldest inline images until the request fits the model input window.
/// Codex resends every screenshot in history; 16k tokens each will trip the
/// pathological guard long before the provider sees the body.
fn trim_inline_images_to_input_budget(
    body: &mut Value,
    candidate: &RouteCandidate,
    reserved_output_tokens: u64,
) {
    let Some(limit) = usable_input_token_limit(candidate, reserved_output_tokens) else {
        return;
    };
    let mut omitted = 0_usize;
    let mut estimate = current_input_estimate(body);
    loop {
        if estimate.total_tokens <= limit || estimate.image_count == 0 {
            break;
        }
        // One image can own every text token plus at most an original-detail
        // image charge. While even that pessimistic drop still leaves us over
        // the window, skip the full base64 walk. Final drops re-estimate so
        // the number of omitted images stays the same.
        let upper_per_image = ORIGINAL_DETAIL_IMAGE_TOKENS.saturating_add(estimate.text_tokens);
        let over = estimate.total_tokens.saturating_sub(limit);
        let batch = if upper_per_image == 0 {
            1
        } else {
            over.saturating_sub(1)
                .saturating_div(upper_per_image)
                .max(1)
                .min(estimate.image_count)
        };
        let mut dropped = 0_u64;
        while dropped < batch {
            if !omit_oldest_input_image_for_token_budget(body) {
                break;
            }
            dropped += 1;
        }
        if dropped == 0 {
            break;
        }
        omitted += dropped as usize;
        estimate = current_input_estimate(body);
    }
    if omitted > 0 {
        let remaining = current_input_estimate(body);
        crate::debug::log(&format!(
            "omitted {omitted} older inline images to fit {}/{} (remaining images={} tokens={})",
            candidate.provider.id,
            candidate.upstream_model,
            remaining.image_count,
            remaining.total_tokens
        ));
    }
}

/// Token-budget trim helper. Distinct from
/// `translate::omit_oldest_translated_input_image`, which replaces a wire image
/// with a marker for sender/provider fitting. This path only walks `input` and
/// removes the oldest `input_image` / `image_url` node so the estimator drops.
fn omit_oldest_input_image_for_token_budget(body: &mut Value) -> bool {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return false;
    };
    omit_oldest_image_in_array(items)
}

fn omit_oldest_image_in_value(value: &mut Value) -> bool {
    match value {
        Value::Array(values) => omit_oldest_image_in_array(values),
        Value::Object(object) => {
            for key in ["content", "output", "input"] {
                if let Some(child) = object.get_mut(key) {
                    if omit_oldest_image_in_value(child) {
                        return true;
                    }
                }
            }
            for (key, child) in object.iter_mut() {
                if matches!(key.as_str(), "content" | "output" | "input") {
                    continue;
                }
                if omit_oldest_image_in_value(child) {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

fn omit_oldest_image_in_array(values: &mut Vec<Value>) -> bool {
    if let Some(index) = values.iter().position(value_is_inline_image) {
        values.remove(index);
        return true;
    }
    values.iter_mut().any(omit_oldest_image_in_value)
}

fn value_is_inline_image(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("input_image" | "image_url")
    )
}

pub(crate) fn enforce_pathological_input_budget(
    body: &Value,
    candidate: &RouteCandidate,
    reserved_output_tokens: u64,
) -> Result<(), CandidatePolicyError> {
    let Some(limit) = usable_input_token_limit(candidate, reserved_output_tokens) else {
        return Ok(());
    };
    // Ordinary context management belongs to Codex and the provider. This
    // fail-open guard only rejects inputs that are implausibly larger than the
    // usable window; local compaction requests bypass this path entirely. The
    // 2.5x tolerance also stays above the estimator's observed serialization
    // overhead, so an approximation mismatch cannot preempt normal compaction.
    const ADMISSION_TOLERANCE_NUMERATOR: u64 = 5;
    const ADMISSION_TOLERANCE_DENOMINATOR: u64 = 2;
    let admission_limit =
        limit.saturating_mul(ADMISSION_TOLERANCE_NUMERATOR) / ADMISSION_TOLERANCE_DENOMINATOR;
    let estimate = body
        .get("input")
        .and_then(Value::as_array)
        .map(|items| estimate_input_items(items))
        .unwrap_or_default();
    if estimate.total_tokens > admission_limit {
        let details = format!(
            "text_estimated_tokens={} image_count={} image_estimated_tokens={} total_estimated_tokens={} context_limit={} admission_limit={} provider={} model={}",
            estimate.text_tokens,
            estimate.image_count,
            estimate.image_tokens,
            estimate.total_tokens,
            limit,
            admission_limit,
            candidate.provider.id,
            candidate.upstream_model
        );
        eprintln!("CODETAS Gateway: input budget rejected {details}");
        crate::debug::log(&format!("input budget rejected {details}"));
        return Err(CandidatePolicyError::InputBudget(format!(
            "request input is pathologically larger than the {}-token context budget for {}/{} ({})",
            limit, candidate.provider.id, candidate.upstream_model, details
        )));
    }
    Ok(())
}

const BASE64_IMAGE_PLACEHOLDER: &str = "<base64-image-payload>";
const LOW_DETAIL_IMAGE_TOKENS: u64 = 1_024;
const DEFAULT_IMAGE_TOKENS: u64 = 8_192;
const ORIGINAL_DETAIL_IMAGE_TOKENS: u64 = 16_384;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct InputTokenEstimate {
    pub(crate) text_tokens: u64,
    pub(crate) image_count: u64,
    pub(crate) image_tokens: u64,
    pub(crate) total_tokens: u64,
}

#[derive(Default)]
struct InputEstimateAccumulator {
    text_bytes: u64,
    image_count: u64,
    image_tokens: u64,
}

impl InputEstimateAccumulator {
    fn add_text_bytes(&mut self, bytes: u64) {
        self.text_bytes = self.text_bytes.saturating_add(bytes);
    }

    fn add_image(&mut self, detail: Option<&str>) {
        self.image_count = self.image_count.saturating_add(1);
        self.image_tokens = self.image_tokens.saturating_add(image_token_cost(detail));
    }

    fn finish(self) -> InputTokenEstimate {
        let text_tokens = self
            .text_bytes
            .saturating_add(APPROX_INPUT_BYTES_PER_TOKEN - 1)
            / APPROX_INPUT_BYTES_PER_TOKEN;
        InputTokenEstimate {
            text_tokens,
            image_count: self.image_count,
            image_tokens: self.image_tokens,
            total_tokens: text_tokens.saturating_add(self.image_tokens),
        }
    }
}

/// Compute a modality-aware input estimate shared by admission enforcement and
/// subagent shrinking. Normal JSON, URL text, and image metadata retain the
/// conservative bytes-per-token estimate. Valid base64 image payloads are
/// replaced by a small marker and charged a separate, bounded image cost.
pub(crate) fn estimate_input_items(items: &[Value]) -> InputTokenEstimate {
    let mut estimate = InputEstimateAccumulator::default();
    estimate.add_text_bytes(2); // JSON array brackets.
    let mut first = true;
    for item in items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) != Some("additional_tools"))
    {
        if !first {
            estimate.add_text_bytes(1); // Comma between array items.
        }
        first = false;
        accumulate_json_value(item, &mut estimate);
    }
    estimate.finish()
}

/// Compute the total input token estimate for the given input items.
pub(crate) fn estimate_input_items_tokens(items: &[Value]) -> u64 {
    estimate_input_items(items).total_tokens
}

fn accumulate_json_value(value: &Value, estimate: &mut InputEstimateAccumulator) {
    match value {
        Value::Null => estimate.add_text_bytes(4),
        Value::Bool(true) => estimate.add_text_bytes(4),
        Value::Bool(false) => estimate.add_text_bytes(5),
        Value::Number(number) => estimate.add_text_bytes(number.to_string().len() as u64),
        Value::String(text) => estimate.add_text_bytes(serialized_json_string_bytes(text)),
        Value::Array(values) => {
            estimate.add_text_bytes(2);
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    estimate.add_text_bytes(1);
                }
                accumulate_json_value(value, estimate);
            }
        }
        Value::Object(object) => accumulate_json_object(object, estimate, false, None),
    }
}

fn accumulate_json_object(
    object: &serde_json::Map<String, Value>,
    estimate: &mut InputEstimateAccumulator,
    image_url_object: bool,
    inherited_detail: Option<&str>,
) {
    estimate.add_text_bytes(2);
    let image_content = matches!(
        object.get("type").and_then(Value::as_str),
        Some("input_image" | "image_url")
    );
    let detail = object
        .get("detail")
        .and_then(Value::as_str)
        .or(inherited_detail);
    for (index, (key, value)) in object.iter().enumerate() {
        if index > 0 {
            estimate.add_text_bytes(1);
        }
        estimate.add_text_bytes(serialized_json_string_bytes(key).saturating_add(1));
        match (key.as_str(), value) {
            ("image_url", Value::String(url)) if image_content => {
                accumulate_image_reference(url, detail, estimate);
            }
            ("image_url", Value::Object(nested)) if image_content => {
                accumulate_json_object(nested, estimate, true, detail);
            }
            ("url", Value::String(url)) if image_url_object => {
                accumulate_image_reference(url, detail, estimate);
            }
            _ => accumulate_json_value(value, estimate),
        }
    }
}

fn accumulate_image_reference(
    url: &str,
    detail: Option<&str>,
    estimate: &mut InputEstimateAccumulator,
) {
    estimate.add_image(detail);
    if let Some(prefix) = valid_base64_image_data_url_prefix(url) {
        let normalized_bytes = 2_u64
            .saturating_add(serialized_json_string_content_bytes(prefix))
            .saturating_add(serialized_json_string_content_bytes(
                BASE64_IMAGE_PLACEHOLDER,
            ));
        estimate.add_text_bytes(normalized_bytes);
    } else {
        // HTTP(S) image URLs and malformed data URLs remain ordinary text. The
        // latter is intentionally conservative and cannot hide arbitrary input
        // behind a data URL prefix.
        estimate.add_text_bytes(serialized_json_string_bytes(url));
    }
}

fn image_token_cost(detail: Option<&str>) -> u64 {
    match detail {
        Some(value) if value.eq_ignore_ascii_case("low") => LOW_DETAIL_IMAGE_TOKENS,
        Some(value) if value.eq_ignore_ascii_case("original") => ORIGINAL_DETAIL_IMAGE_TOKENS,
        _ => DEFAULT_IMAGE_TOKENS,
    }
}

fn valid_base64_image_data_url_prefix(url: &str) -> Option<&str> {
    let comma = url.find(',')?;
    let (header, payload_with_comma) = url.split_at(comma);
    let payload = payload_with_comma.strip_prefix(',')?;
    let media_and_parameters = header.get(5..)?;
    if !header.get(..5)?.eq_ignore_ascii_case("data:") {
        return None;
    }
    let mut segments = media_and_parameters.split(';');
    let media_type = segments.next()?;
    if !media_type
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
        || !segments.any(|segment| segment.eq_ignore_ascii_case("base64"))
        || !is_valid_base64_payload(payload)
    {
        return None;
    }
    url.get(..=comma)
}

fn is_valid_base64_payload(payload: &str) -> bool {
    if payload.is_empty() || !payload.is_ascii() {
        return false;
    }
    let bytes = payload.as_bytes();
    let padding_start = bytes
        .iter()
        .position(|byte| *byte == b'=')
        .unwrap_or(bytes.len());
    let padding = bytes.len().saturating_sub(padding_start);
    if padding > 2
        || bytes[padding_start..].iter().any(|byte| *byte != b'=')
        || bytes[..padding_start].iter().any(|byte| {
            !byte.is_ascii_alphanumeric() && !matches!(*byte, b'+' | b'/' | b'-' | b'_')
        })
    {
        return false;
    }
    if padding > 0 {
        bytes.len() % 4 == 0
    } else {
        bytes.len() % 4 != 1
    }
}

fn serialized_json_string_bytes(value: &str) -> u64 {
    serialized_json_string_content_bytes(value).saturating_add(2)
}

fn serialized_json_string_content_bytes(value: &str) -> u64 {
    value.chars().fold(0_u64, |bytes, character| {
        let encoded = match character {
            '"' | '\\' | '\u{0008}' | '\u{000c}' | '\n' | '\r' | '\t' => 2,
            '\u{0000}'..='\u{001f}' => 6,
            _ => character.len_utf8() as u64,
        };
        bytes.saturating_add(encoded)
    })
}

/// Subagent turns inherit the parent conversation history through the
/// collaboration transport. When that history exceeds the subagent model's
/// context budget, drop redundant intermediate items (reasoning traces and
/// older custom tool outputs) to fit, instead of failing the turn. Returns
/// `true` when the input was modified, `false` when it already fits or cannot
/// be shrunk further.
pub(crate) fn shrink_subagent_input(
    body: &mut Value,
    candidate: &RouteCandidate,
    reserved_output_tokens: u64,
) -> Result<bool, String> {
    let context_input = candidate
        .context_window
        .map(|context| context.saturating_sub(reserved_output_tokens));
    let limit = match (candidate.max_input_tokens, context_input) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(limit), None) | (None, Some(limit)) => Some(limit),
        (None, None) => None,
    };
    let Some(limit) = limit else { return Ok(false) };
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return Ok(false);
    };
    if estimate_input_items_tokens(items) <= limit {
        return Ok(false);
    }
    // Drop in order of least value for a subagent continuation: reasoning
    // traces first, then older custom tool outputs. Keep messages, function
    // calls, and their outputs so the turn stays coherent.
    let mut changed = false;
    let mut removable = items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            matches!(
                item.get("type").and_then(Value::as_str),
                Some("reasoning" | "custom_tool_call_output")
            )
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    // Prefer dropping reasoning first: they are the largest and least
    // semantically essential for a continuation. Collect removable indices,
    // build a to-drop set in that order, then rebuild the array once with
    // `retain` so indices never go stale.
    removable.sort_by_key(|index| {
        items[*index].get("type").and_then(Value::as_str) != Some("reasoning")
    });
    let mut drop = std::collections::HashSet::new();
    while estimate_input_items_tokens(items) > limit {
        let Some(index) = removable.pop() else {
            break;
        };
        drop.insert(index);
        changed = true;
    }
    if changed {
        let retained = items
            .iter()
            .enumerate()
            .filter(|(index, _)| !drop.contains(index))
            .map(|(_, item)| item.clone())
            .collect::<Vec<_>>();
        *items = retained;
    }
    Ok(changed)
}

pub(crate) fn quota_usage_percent(headers: &HeaderMap) -> Option<u8> {
    [
        (
            "x-ratelimit-limit-requests",
            "x-ratelimit-remaining-requests",
        ),
        ("x-ratelimit-limit-tokens", "x-ratelimit-remaining-tokens"),
        ("ratelimit-limit", "ratelimit-remaining"),
    ]
    .into_iter()
    .filter_map(|(limit_name, remaining_name)| {
        let limit = headers
            .get(limit_name)?
            .to_str()
            .ok()?
            .parse::<f64>()
            .ok()?;
        let remaining = headers
            .get(remaining_name)?
            .to_str()
            .ok()?
            .parse::<f64>()
            .ok()?;
        if !limit.is_finite() || !remaining.is_finite() || limit <= 0.0 {
            return None;
        }
        let used = ((limit - remaining).max(0.0) / limit * 100.0).ceil();
        Some(used.clamp(0.0, 100.0) as u8)
    })
    .max()
}

#[cfg(test)]
mod input_budget_tests {
    use super::*;
    use crate::config::ProviderDefinition;

    fn candidate(context_window: u64) -> RouteCandidate {
        let mut provider = ProviderDefinition::default();
        provider.id = "openai".into();
        let capabilities = provider.capabilities.clone();
        RouteCandidate {
            provider,
            upstream_model: "gpt-5.6-sol".into(),
            exposed_model: "gpt-5.6-sol".into(),
            credential: None,
            account_id: None,
            target_key: "test".into(),
            route_id: None,
            failure_threshold: 0,
            quota_threshold_percent: 0,
            input_price_per_million: None,
            output_price_per_million: None,
            context_window: Some(context_window),
            max_input_tokens: None,
            max_output_tokens: None,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            capabilities,
            routing_epoch: 0,
            routing_generation: 0,
            session_scope: None,
        }
    }

    fn image_output(urls: impl IntoIterator<Item = (String, &'static str)>) -> Value {
        let output = urls
            .into_iter()
            .map(|(image_url, detail)| {
                json!({
                    "type": "input_image",
                    "image_url": image_url,
                    "detail": detail
                })
            })
            .collect::<Vec<_>>();
        json!({
            "type": "custom_tool_call_output",
            "call_id": "call_images",
            "output": output
        })
    }

    fn base64_image(payload_bytes: usize) -> String {
        format!("data:image/png;base64,{}", "A".repeat(payload_bytes))
    }

    #[test]
    fn rejects_only_pathologically_large_text_input() {
        let body = json!({
            "input": [{
                "type": "message",
                "role": "user",
                "content": "x".repeat(20_000)
            }]
        });
        let error = enforce_pathological_input_budget(&body, &candidate(1_000), 0)
            .expect_err("large text input should be rejected")
            .to_string();

        assert!(error.contains("text_estimated_tokens="));
        assert!(error.contains("image_count=0"));
        assert!(error.contains("image_estimated_tokens=0"));
        assert!(error.contains("total_estimated_tokens="));
        assert!(error.contains("context_limit=1000"));
        assert!(error.contains("admission_limit=2500"));
        assert!(error.contains("provider=openai"));
        assert!(error.contains("model=gpt-5.6-sol"));
    }

    #[tokio::test]
    async fn input_budget_rejection_uses_context_window_error_shape() {
        let body = json!({
            "input": [{
                "type": "message",
                "role": "user",
                "content": "x".repeat(20_000)
            }]
        });
        let error = enforce_pathological_input_budget(&body, &candidate(1_000), 0)
            .expect_err("large text input should be rejected");
        let response = candidate_policy_error_response(&error);

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(response.into_body(), 16 * 1024)
            .await
            .expect("error response should be readable");
        let payload: Value =
            serde_json::from_slice(&bytes).expect("error response should be valid JSON");
        assert_eq!(payload["error"]["type"], "invalid_request_error");
        assert_eq!(payload["error"]["code"], "context_length_exceeded");
        assert_eq!(payload["error"]["param"], "input");
        assert!(payload["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("exceeds the context window")));
    }

    #[test]
    fn openai_remote_compaction_routes_skip_token_admission() {
        let mut route = candidate(1_000);
        route.provider.credential.source = CredentialSource::Forward;
        route.provider.base_url = "https://chatgpt.com/backend-api/codex".into();
        let mut body = json!({
            "input": [
                {"type": "compaction_trigger"},
                {
                    "type": "message",
                    "role": "user",
                    "content": "x".repeat(20_000)
                }
            ]
        });

        assert!(apply_candidate_model_policy(&mut body, &route, false).is_ok());
    }

    #[test]
    fn openai_api_native_compaction_routes_skip_token_admission() {
        let mut route = candidate(1_000);
        route.provider.id = "openai-api".into();
        route.provider.credential.source = CredentialSource::Environment;
        route.provider.base_url = "https://api.openai.com/v1".into();
        let mut body = json!({
            "input": [
                {"type": "compaction_trigger"},
                {
                    "type": "message",
                    "role": "user",
                    "content": "x".repeat(20_000)
                }
            ]
        });

        assert_eq!(
            candidate_compaction_mode(&route, CompactionRequestKind::NativeTrigger),
            CompactionMode::Responses
        );
        assert!(apply_candidate_model_policy(&mut body, &route, false).is_ok());
    }

    #[test]
    fn local_compaction_routes_keep_only_the_pathological_guard() {
        let mut route = candidate(1_000);
        route.provider.id = "kiro".into();
        route.provider.transport = ProviderTransport::Kiro;
        let mut body = json!({
            "input": [{
                "type": "message",
                "role": "user",
                "content": "x".repeat(20_000)
            }]
        });

        assert!(apply_candidate_model_policy(&mut body, &route, false).is_err());
    }

    #[test]
    fn eight_megabyte_base64_image_is_not_counted_as_text() {
        let body = json!({
            "input": [
                {"type": "message", "role": "user", "content": "inspect this image"},
                image_output([(base64_image(8 * 1024 * 1024), "original")])
            ]
        });
        let estimate = estimate_input_items(body["input"].as_array().unwrap());

        assert_eq!(estimate.image_count, 1);
        assert_eq!(estimate.image_tokens, ORIGINAL_DETAIL_IMAGE_TOKENS);
        assert!(estimate.text_tokens < 1_000);
        assert!(enforce_pathological_input_budget(&body, &candidate(272_000), 0).is_ok());
    }

    #[test]
    fn four_base64_images_do_not_produce_a_multi_million_token_estimate() {
        let images = [
            6 * 1024 * 1024,
            4 * 1024 * 1024,
            2 * 1024 * 1024,
            1 * 1024 * 1024,
        ]
        .into_iter()
        .map(|size| (base64_image(size), "original"));
        let body = json!({
            "input": [
                {"type": "message", "role": "user", "content": "compare all four"},
                image_output(images)
            ]
        });
        let estimate = estimate_input_items(body["input"].as_array().unwrap());

        assert_eq!(estimate.image_count, 4);
        assert_eq!(estimate.image_tokens, 4 * ORIGINAL_DETAIL_IMAGE_TOKENS);
        assert!(estimate.total_tokens < 100_000);
        assert!(enforce_pathological_input_budget(&body, &candidate(272_000), 0).is_ok());
    }

    #[test]
    fn excess_inline_images_are_omitted_to_fit_the_model_window() {
        let images = (0..68).map(|_| (base64_image(1024), "original"));
        let mut body = json!({
            "input": [
                {"type": "message", "role": "user", "content": "review these screenshots"},
                image_output(images)
            ]
        });
        let before = estimate_input_items(body["input"].as_array().unwrap());
        assert_eq!(before.image_count, 68);
        assert!(before.total_tokens > 500_000);

        trim_inline_images_to_input_budget(&mut body, &candidate(500_000), 0);
        let after = estimate_input_items(body["input"].as_array().unwrap());
        assert!(after.total_tokens <= 500_000);
        assert!(after.image_count < 68);
        assert!(after.image_count > 0);
        assert!(enforce_pathological_input_budget(&body, &candidate(500_000), 0).is_ok());
    }

    #[test]
    fn additional_tools_remain_excluded_from_the_estimate() {
        let message = json!({"type": "message", "role": "user", "content": "hello"});
        let without_tools = vec![message.clone()];
        let with_tools = vec![
            message,
            json!({
                "type": "additional_tools",
                "tools": [{
                    "type": "function",
                    "name": "huge_tool",
                    "description": "z".repeat(1_000_000)
                }]
            }),
        ];

        assert_eq!(
            estimate_input_items(&without_tools),
            estimate_input_items(&with_tools)
        );
    }

    #[test]
    fn malformed_data_url_is_counted_as_text_without_panicking() {
        let malformed = format!("data:image/png;base64,{}", "%".repeat(20_000));
        let body = json!({"input": [image_output([(malformed, "high")])]});
        let estimate = estimate_input_items(body["input"].as_array().unwrap());

        assert_eq!(estimate.image_count, 1);
        assert!(estimate.text_tokens > 1_000);
        assert!(enforce_pathological_input_budget(&body, &candidate(1_000), 0).is_err());
    }

    #[test]
    fn url_and_base64_image_forms_are_both_estimated() {
        let body = json!({
            "input": [{
                "type": "message",
                "role": "user",
                "content": [
                    {
                        "type": "image_url",
                        "image_url": {
                            "url": format!("https://example.com/image.png?{}", "q".repeat(400)),
                            "detail": "low"
                        }
                    },
                    {
                        "type": "input_image",
                        "image_url": base64_image(1024 * 1024),
                        "detail": "high"
                    }
                ]
            }]
        });
        let estimate = estimate_input_items(body["input"].as_array().unwrap());

        assert_eq!(estimate.image_count, 2);
        assert_eq!(
            estimate.image_tokens,
            LOW_DETAIL_IMAGE_TOKENS + DEFAULT_IMAGE_TOKENS
        );
        assert!(estimate.text_tokens > 100);
        assert!(estimate.text_tokens < 1_000);
    }

    #[test]
    fn base64_image_does_not_trigger_subagent_shrinking_by_payload_length() {
        let mut body = json!({
            "input": [
                {"type": "reasoning", "summary": "keep this small history"},
                image_output([(base64_image(1024 * 1024), "original")])
            ]
        });

        let changed = shrink_subagent_input(&mut body, &candidate(20_000), 0).unwrap();

        assert!(!changed);
        assert_eq!(body["input"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn text_byte_estimate_matches_compact_json_serialization() {
        let items = vec![json!({
            "type": "message",
            "content": "quotes: \" slash: \\ newline:\n 日本語"
        })];
        let expected_bytes = serde_json::to_vec(&items).unwrap().len() as u64;
        let expected_tokens = expected_bytes.saturating_add(APPROX_INPUT_BYTES_PER_TOKEN - 1)
            / APPROX_INPUT_BYTES_PER_TOKEN;

        assert_eq!(estimate_input_items(&items).text_tokens, expected_tokens);
    }
}
