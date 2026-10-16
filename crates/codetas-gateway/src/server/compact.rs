use super::*;

#[derive(Clone, Copy, Debug)]
pub(crate) struct CompactionQuotaExhausted {
    pub(crate) retry_after: Option<Duration>,
}

pub(crate) async fn send_compact_candidate(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
) -> Result<reqwest::Response, AttemptFailure> {
    retry_provider_request(state, candidate, false, || {
        send_compact_candidate_once(state, body, candidate, caller_headers)
    })
    .await
}

pub(crate) async fn send_compact_candidate_once(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
) -> Result<reqwest::Response, AttemptFailure> {
    super::senders::validate_candidate_image_references(body, candidate)?;
    let mut body = body.clone();
    if let Some(object)=body.as_object_mut() {
        object.remove(crate::anthropic_compaction::REQUEST_FLAG);
        object.remove(crate::anthropic_compaction::HISTORY_REWRITTEN);
    }
    body["model"] = Value::String(candidate.provider.wire_model_id(&candidate.upstream_model));
    crate::compaction::validate_local_compactions(&body)
        .map_err(|message| request_failure("invalid_compaction_history", &message))?;
    // Local `codetas1:` envelopes are CODETAS transport. Expand them on every
    // compact hop, including native `/responses/compact`, so the upstream never
    // sees a prefix it cannot decrypt. The trigger itself is left intact.
    expand_local_compactions(&mut body);
    if let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) {
        crate::compaction::normalize_completed_empty_function_arguments(input);
    }
    // Keep the native compact envelope and history intact, but mirror OpenCodex's
    // narrow direct-forwarding sanitizer: raw reasoning_text input and the
    // top-level reasoning option are rejected by the native backend.
    crate::compat::sanitize_compact_request(&mut body);
    crate::debug::log(
        "send_compact_candidate: applied compact reasoning sanitize; broader compatibility skipped",
    );
    let (serialized, omitted_images) = crate::translate::serialize_with_image_budget_async(
        &mut body,
        candidate.provider.limits.max_request_bytes,
    )
    .await
    .map_err(|error| {
        request_failure(
            "invalid_request",
            &format!("request cannot be encoded after image budgeting: {error}"),
        )
    })?;
    if omitted_images > 0 {
        crate::debug::log(&format!(
            "native compact wire image budget: provider={} omitted={} request_bytes={}",
            candidate.provider.id, omitted_images, serialized.len(),
        ));
    }
    if serialized.len() as u64 > candidate.provider.limits.max_request_bytes {
        return Err(request_failure(
            "request_too_large",
            &format!(
                "compaction request exceeds the configured provider limit: provider={} model={} request_bytes={} max_request_bytes={}",
                candidate.provider.id,
                candidate.upstream_model,
                serialized.len(),
                candidate.provider.limits.max_request_bytes,
            ),
        ));
    }
    let endpoint = candidate.provider.compact_endpoint();
    let dns_pinning = state.settings.read().await.security.dns_pinning;
    let client = if dns_pinning {
        pinned_client(&endpoint, &candidate.provider, "CODETAS-Gateway/0.1")
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "provider_dns_rejected",
                    &message,
                ),
                kind: AttemptFailureKind::Retryable,
            })?
    } else {
        state.client.clone()
    };
    let request = client
        .post(endpoint)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json")
        .query(&candidate.provider.query_params)
        .timeout(Duration::from_millis(
            candidate.provider.limits.request_timeout_ms,
        ));
    let request = apply_provider_auth(request, &candidate.provider, candidate.credential.as_ref())
        .await
        .map_err(|message| AttemptFailure {
            response: error_response(
                StatusCode::UNAUTHORIZED,
                "missing_provider_credential",
                &message,
            ),
            kind: AttemptFailureKind::Credential,
        })?;
    let credential_source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let request = if credential_source == CredentialSource::Forward {
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
    match diagnostics::send_with_wire_diagnostic(request, serialized, &body, caller_headers).await {
        Ok(response)
            if response
                .content_length()
                .is_some_and(|length| length > candidate.provider.limits.max_response_bytes) =>
        {
            Err(super::senders::reject_oversized_provider_response(response, candidate.provider.limits.max_response_bytes, false).await)
        }
        Ok(response) => Ok(response),
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

pub(crate) fn offline_compact_value(
    body: &Value,
    exposed_model: &str,
    request_kind: CompactionRequestKind,
    settings: &crate::config::LocalCompactionSettings,
) -> Result<(Value, TokenUsage), String> {
    let history_items = body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let history = crate::compaction::normalize_compaction_history(&history_items)?;
    if !crate::compaction::offline_recovery_has_progress(&history) {
        return Err("offline compaction has no recoverable task progress".into());
    }
    let (context, _metrics) =
        crate::compaction::build_offline_compacted_context(&history, settings)?;
    let compacted = match request_kind {
        CompactionRequestKind::Standalone => json!({
            "output": crate::compaction::standalone_output_items(&context)?
        }),
        CompactionRequestKind::NativeTrigger => {
            let item = crate::compaction::native_compaction_item(&context, settings)?;
            json!({
                "id": format!("cmpct_{}", Uuid::new_v4().simple()),
                "object": "response.compaction",
                "created_at": ObservationEvent::now_ms() / 1_000,
                "model": exposed_model,
                "output": [item],
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "total_tokens": 0
                }
            })
        }
    };
    Ok((compacted, TokenUsage::default()))
}

pub(crate) async fn synthetic_compact_candidate(
    state: &GatewayState,
    caller_headers: &HeaderMap,
    body: &Value,
    candidate: &RouteCandidate,
    request_kind: CompactionRequestKind,
) -> Result<(Value, TokenUsage, Option<ProviderRetryObservation>), AttemptFailure> {
    let mut billed=TokenUsage::default();
    match synthetic_compact_candidate_inner(state, caller_headers, body, candidate, request_kind,&mut billed).await {
        Ok(result)=>Ok(result),
        Err(mut failure)=>{
            let previous=failure.response.extensions_mut().remove::<crate::observability::ReportedUpstreamUsage>().map(|usage|usage.0).unwrap_or_default();
            failure.response.extensions_mut().insert(crate::observability::ReportedUpstreamUsage(sum_token_usage(previous,billed)));
            Err(failure)
        }
    }
}

async fn synthetic_compact_candidate_inner(
    state: &GatewayState,
    caller_headers: &HeaderMap,
    body: &Value,
    candidate: &RouteCandidate,
    request_kind: CompactionRequestKind,
    billed_usage:&mut TokenUsage,
) -> Result<(Value, TokenUsage, Option<ProviderRetryObservation>), AttemptFailure> {
    super::senders::validate_candidate_image_references(body, candidate)?;
    crate::debug::log(&format!(
        "synthetic_compact_candidate: begin model={} input_items={}",
        candidate.upstream_model,
        body.get("input")
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0)
    ));
    let settings = state.settings.read().await.local_compaction.clone();
    let history_items = body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let history = crate::compaction::normalize_compaction_history(&history_items)
        .map_err(|message| request_failure("invalid_compaction_history", &message))?;
    let split =
        crate::compaction::split_prefix_and_tail(&history.items, settings.tail_token_limit())
            .map_err(|message| request_failure("invalid_compaction_request", &message))?;
    let summarizer_input = crate::compaction::build_summarizer_input(
        history.previous_checkpoint.as_deref(),
        crate::compaction::summarizer_source_items(&split),
    );
    let request = prepare_synthetic_compaction_request(body, candidate, Some(summarizer_input))?;
    // Prefix-only summarizer input is bounded by tail selection. The original
    // compact request still bypasses the ordinary admission gate.

    let mut request = request;
    let upstream = send_candidate(state, &mut request, candidate, Some(caller_headers)).await?;
    let mut provider_retry = upstream
        .extensions()
        .get::<ProviderRetryObservation>()
        .cloned();
    crate::debug::log(&format!(
        "synthetic_compact_candidate: upstream status={}",
        upstream.status()
    ));
    if !upstream.status().is_success() {
        let status = upstream.status();
        let retry_after = validated_retry_after(upstream.headers());
        let classified = upstream_responses_error_classified(
            upstream,
            retry_after.as_ref().map(|value| &value.0),
        )
        .await;
        let retry_duration = compaction_quota_retry_after(&classified.response);
        let mut failure = AttemptFailure {
            response: classified.response,
            kind: if classified.context_window_exceeded {
                AttemptFailureKind::ContextWindow
            } else if status == StatusCode::REQUEST_TIMEOUT
                || status == StatusCode::TOO_MANY_REQUESTS
                || status.is_server_error()
            {
                AttemptFailureKind::Retryable
            } else {
                AttemptFailureKind::Request
            },
        };
        if status == StatusCode::TOO_MANY_REQUESTS {
            failure
                .response
                .extensions_mut()
                .insert(CompactionQuotaExhausted {
                    retry_after: retry_duration,
                });
        }
        return Err(compaction_failure_with_retry(
            failure,
            provider_retry.as_ref(),
        ));
    }
    let upstream_value = read_compaction_candidate(upstream, candidate, billed_usage, provider_retry.as_ref()).await?;
    let adapted = match candidate
        .provider
        .protocol_for_model(&candidate.upstream_model)
    {
        ProviderProtocol::Responses => Ok(upstream_value),
        ProviderProtocol::ChatCompletions => chat_to_response(
            &upstream_value,
            &candidate.exposed_model,
            &ResponseToolMap::default(),
        ),
        ProviderProtocol::AnthropicMessages => anthropic_to_response(
            &upstream_value,
            &candidate.exposed_model,
            &ResponseToolMap::default(),
        ),
        ProviderProtocol::GeminiGenerateContent => gemini_to_response(
            &upstream_value,
            &candidate.exposed_model,
            &ResponseToolMap::default(),
        ),
    }
    .map_err(|message| {
        compaction_failure_with_retry(
            AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "invalid_provider_response",
                    &message,
                ),
                kind: AttemptFailureKind::Retryable,
            },
            provider_retry.as_ref(),
        )
    })?;
    require_completed_compaction_source(&adapted).map_err(|message| {
        compaction_failure_with_retry(
            AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "invalid_compaction_response",
                    &message,
                ),
                kind: AttemptFailureKind::Retryable,
            },
            provider_retry.as_ref(),
        )
    })?;
    let mut summary = response_output_text(&adapted);
    let mut usage = billed_usage.clone();
    let mut repaired = false;
    if crate::compaction::validate_checkpoint_summary(&summary).is_err() {
        let mut repair_request = prepare_synthetic_compaction_request(
            body,
            candidate,
            Some(vec![
                json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": summary}]
                }),
                json!({
                    "type": "message",
                    "role": "developer",
                    "content": [{"type": "input_text", "text": crate::compaction::REPAIR_PROMPT}]
                }),
            ]),
        ).map_err(|failure| compaction_failure_with_retry(failure, provider_retry.as_ref()))?;
        let retry = provider_retry.get_or_insert_with(ProviderRetryObservation::default);
        retry.additional_sends = retry.additional_sends.saturating_add(1);
        retry.recovery_kinds.push("compaction-repair".into());
        let repair_upstream = match send_candidate(state, &mut repair_request, candidate, Some(caller_headers)).await {
            Ok(response) => response,
            Err(failure) => return Err(merge_compaction_repair_failure(failure, &mut provider_retry)),
        };
        merge_provider_retry_observation(&mut provider_retry, repair_upstream.extensions().get::<ProviderRetryObservation>().cloned());
        if !repair_upstream.status().is_success() {
            let classified=upstream_responses_error_classified(repair_upstream,None).await;
            *billed_usage=sum_token_usage(billed_usage.clone(),classified.usage);
            return Err(compaction_failure_with_retry(
                AttemptFailure {
                    response: error_response(
                        StatusCode::BAD_GATEWAY,
                        "invalid_compaction_response",
                        "compaction repair request failed",
                    ),
                    kind: AttemptFailureKind::Retryable,
                },
                provider_retry.as_ref(),
            ));
        }
        let repair_value = read_compaction_candidate(repair_upstream, candidate, billed_usage, provider_retry.as_ref()).await?;
        let repair_usage = TokenUsage::from_json(&repair_value);
        let repair_adapted = match candidate
            .provider
            .protocol_for_model(&candidate.upstream_model)
        {
            ProviderProtocol::Responses => Ok(repair_value),
            ProviderProtocol::ChatCompletions => chat_to_response(
                &repair_value,
                &candidate.exposed_model,
                &ResponseToolMap::default(),
            ),
            ProviderProtocol::AnthropicMessages => anthropic_to_response(
                &repair_value,
                &candidate.exposed_model,
                &ResponseToolMap::default(),
            ),
            ProviderProtocol::GeminiGenerateContent => gemini_to_response(
                &repair_value,
                &candidate.exposed_model,
                &ResponseToolMap::default(),
            ),
        }
        .map_err(|message| {
            compaction_failure_with_retry(
                AttemptFailure {
                    response: error_response(
                        StatusCode::BAD_GATEWAY,
                        "invalid_provider_response",
                        &message,
                    ),
                    kind: AttemptFailureKind::Retryable,
                },
                provider_retry.as_ref(),
            )
        })?;
        require_completed_compaction_source(&repair_adapted).map_err(|message| {
            compaction_failure_with_retry(
                AttemptFailure {
                    response: error_response(
                        StatusCode::BAD_GATEWAY,
                        "invalid_compaction_response",
                        &message,
                    ),
                    kind: AttemptFailureKind::Retryable,
                },
                provider_retry.as_ref(),
            )
        })?;
        summary = response_output_text(&repair_adapted);
        usage = sum_token_usage(usage, repair_usage);
        repaired = true;
    }
    crate::compaction::validate_checkpoint_summary(&summary).map_err(|error| {
        compaction_failure_with_retry(
            AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "invalid_compaction_response",
                    &error.to_string(),
                ),
                kind: AttemptFailureKind::Retryable,
            },
            provider_retry.as_ref(),
        )
    })?;
    let (context, _metrics) = crate::compaction::build_compacted_context_with_repair(
        &history, summary, &settings, repaired,
    )
    .map_err(|message| {
        compaction_failure_with_retry(
            AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "invalid_compaction_response",
                    &message,
                ),
                kind: AttemptFailureKind::Retryable,
            },
            provider_retry.as_ref(),
        )
    })?;
    let compacted = match request_kind {
        CompactionRequestKind::Standalone => json!({
            "output": crate::compaction::standalone_output_items(&context).map_err(|message| {
                compaction_failure_with_retry(
                    AttemptFailure {
                        response: error_response(StatusCode::BAD_GATEWAY, "invalid_compaction_response", &message),
                        kind: AttemptFailureKind::Retryable,
                    },
                    provider_retry.as_ref(),
                )
            })?
        }),
        CompactionRequestKind::NativeTrigger => {
            let item = crate::compaction::native_compaction_item(&context, &settings).map_err(
                |message| {
                    compaction_failure_with_retry(
                        AttemptFailure {
                            response: error_response(
                                StatusCode::BAD_GATEWAY,
                                "invalid_compaction_response",
                                &message,
                            ),
                            kind: AttemptFailureKind::Retryable,
                        },
                        provider_retry.as_ref(),
                    )
                },
            )?;
            json!({
                "id": format!("cmpct_{}", Uuid::new_v4().simple()),
                "object": "response.compaction",
                "created_at": ObservationEvent::now_ms() / 1_000,
                "model": candidate.exposed_model,
                "output": [item],
                "usage": super::senders::token_usage_json(&usage)
            })
        }
    };
    let mut compacted = compacted;
    compacted["usage"] = super::senders::token_usage_json(&usage);
    Ok((compacted, usage, provider_retry))
}

fn compaction_failure_with_retry(
    mut failure: AttemptFailure,
    retry: Option<&ProviderRetryObservation>,
) -> AttemptFailure {
    if let Some(retry) = retry {
        failure.response.extensions_mut().insert(retry.clone());
    }
    failure
}

const COMPACT_V1_RETAINED_CHAR_BUDGET: usize = 20_000 * 4;
use crate::compaction::{COMPACT_PROMPT, SUMMARY_PREFIX as COMPACT_SUMMARY_PREFIX};

fn compact_user_message_text(item: &Value) -> Option<String> {
    if item
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != "message")
        || item.get("role").and_then(Value::as_str) != Some("user")
    {
        return None;
    }
    let text = match item.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| {
                matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("input_text" | "text")
                )
            })
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<String>(),
        _ => String::new(),
    };
    (!text.trim().is_empty()).then_some(text)
}

fn compact_v1_message(text: String) -> Value {
    json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": text}]
    })
}

pub(crate) fn build_compact_v1_output(input: Option<&Value>, summary: &str) -> Vec<Value> {
    let messages = input
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(compact_user_message_text)
        .collect::<Vec<_>>();
    let mut retained = Vec::new();
    let mut remaining = COMPACT_V1_RETAINED_CHAR_BUDGET;
    for message in messages.into_iter().rev() {
        if remaining == 0 {
            break;
        }
        let char_count = message.encode_utf16().count();
        if char_count <= remaining {
            remaining -= char_count;
            retained.push(message);
        } else {
            let mut retained_units = 0;
            let tail = message
                .chars()
                .rev()
                .take_while(|character| {
                    let width = character.len_utf16();
                    if retained_units + width > remaining {
                        false
                    } else {
                        retained_units += width;
                        true
                    }
                })
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<String>();
            retained.push(tail);
            break;
        }
    }
    retained.reverse();
    let summary = if summary.trim().is_empty() {
        "(no summary available)".to_string()
    } else {
        format!("{COMPACT_SUMMARY_PREFIX}\n{summary}")
    };
    retained.push(summary);
    retained.into_iter().map(compact_v1_message).collect()
}

fn prepare_synthetic_compaction_request(
    body: &Value,
    candidate: &RouteCandidate,
    input_override: Option<Vec<Value>>,
) -> Result<Value, AttemptFailure> {
    let mut request = body.clone();
    crate::compaction::expand_translated_compactions(&mut request);
    strip_translated_input_images_for_compaction(&mut request);
    let Some(object) = request.as_object_mut() else {
        return Err(request_failure(
            "invalid_compaction_request",
            "compact request must be an object",
        ));
    };
    // Synthetic compaction sends the full local history; provider-side continuation
    // state would otherwise point at the pre-compaction conversation as well.
    for field in [
        "text",
        "response_format",
        "parallel_tool_calls",
        "tool_choice",
        "previous_response_id",
        "conversation",
    ] {
        object.remove(field);
    }
    object.insert("stream".into(), Value::Bool(false));
    object.insert("store".into(), Value::Bool(false));
    object.insert("tools".into(), Value::Array(Vec::new()));
    object.insert(
        "max_output_tokens".into(),
        json!(candidate
            .max_output_tokens
            .unwrap_or(4_096)
            .clamp(256, 8_192)),
    );
    if let Some(input_override) = input_override {
        object.insert("input".into(), Value::Array(input_override));
    }
    let input = object
        .get_mut("input")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            request_failure(
                "invalid_compaction_request",
                "compact request input must be an array",
            )
        })?;
    crate::compaction::normalize_completed_empty_function_arguments(input);
    // Compaction triggers and dynamic tool catalogs are control-plane items,
    // not conversation prose. Remove them only on this local-summary path so
    // neither the Responses passthrough nor a translated adapter can recollect
    // a schema and constrain or invoke tools while producing the summary.
    input.retain(|item| {
        !matches!(
            item.get("type").and_then(Value::as_str),
            Some("compaction_trigger" | "additional_tools")
        )
    });
    for item in input.iter_mut() {
        if item.get("type").and_then(Value::as_str) == Some("tool_search_output") {
            if let Some(object) = item.as_object_mut() {
                object.remove("tools");
            }
        }
    }
    input.push(json!({
        "type": "message",
        "role": "developer",
        "content": [{
            "type": "input_text",
            "text": COMPACT_PROMPT
        }]
    }));
    Ok(request)
}

pub(crate) fn require_completed_compaction_source(value: &Value) -> Result<(), String> {
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return Err("compaction source reported an error".into());
    }
    if value
        .get("incomplete_details")
        .is_some_and(|details| !details.is_null())
    {
        return Err("compaction source reported incomplete details".into());
    }
    match value.get("status").and_then(Value::as_str) {
        Some("completed") => Ok(()),
        Some(status) => Err(format!(
            "compaction source did not complete (status: {status})"
        )),
        None => Err("compaction source is missing completed terminal status".into()),
    }
}

pub(crate) fn ensure_single_compaction_output(value: Value, model: &str) -> Result<Value, String> {
    ensure_single_compaction_output_from_history(
        value,
        model,
        &[],
        &crate::config::LocalCompactionSettings::default(),
    )
}

pub(crate) fn ensure_single_compaction_output_from_history(
    value: Value,
    model: &str,
    history_items: &[Value],
    settings: &crate::config::LocalCompactionSettings,
) -> Result<Value, String> {
    let existing_compaction = compaction_item_count(&value) == 1;
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return Err("compaction response reported an error".into());
    }
    if value
        .get("incomplete_details")
        .is_some_and(|details| !details.is_null())
    {
        return Err("compaction response reported incomplete details".into());
    }
    if let Some(status) = value.get("status").and_then(Value::as_str) {
        if status != "completed" {
            return Err(format!(
                "compaction response did not complete (status: {status})"
            ));
        }
    } else if !existing_compaction {
        return Err("compaction response is missing completed terminal status".into());
    }
    if existing_compaction {
        return Ok(value);
    }
    let summary = crate::compaction::sanitize_model_summary(&response_output_text(&value));
    crate::compaction::validate_checkpoint_summary(&summary).map_err(|error| error.to_string())?;
    let history = crate::compaction::normalize_compaction_history(history_items)?;
    let (context, _) = crate::compaction::build_compacted_context(&history, summary, &settings)?;
    let encrypted = crate::compaction::encode_context_for_settings(&context, &settings)?;
    let mut wrapped = value;
    let object = wrapped
        .as_object_mut()
        .ok_or_else(|| "compaction response must be an object".to_string())?;
    object.insert("object".into(), json!("response.compaction"));
    object.insert("model".into(), json!(model));
    object.insert(
        "output".into(),
        json!([{
            "id": format!("cmpctitem_{}", Uuid::new_v4().simple()),
            "type": "compaction",
            "encrypted_content": encrypted,
            "created_by": "codetas"
        }]),
    );
    Ok(wrapped)
}

#[cfg(test)]
mod synthetic_compaction_tests {
    use super::*;
    use crate::config::ProviderDefinition;

    #[tokio::test]
    async fn model_switch_provider_metadata_is_kept_locally_but_rejected_fields_never_reach_responses(
    ) {
        type Captured = Arc<Mutex<Vec<Value>>>;
        async fn strict_responses(
            axum::extract::State(captured): axum::extract::State<Captured>,
            axum::Json(body): axum::Json<Value>,
        ) -> (StatusCode, axum::Json<Value>) {
            let invalid = body["input"].as_array().unwrap().iter().any(|item| {
                item.get("provider_metadata").is_some()
                    || item.get("codetas_provider_metadata").is_some()
            });
            captured.lock().await.push(body);
            if invalid {
                (
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({"error":{
                    "code":"unknown_parameter", "message":"Unknown parameter: input.provider_metadata"}})),
                )
            } else {
                (
                    StatusCode::OK,
                    axum::Json(json!({"id":"wire-ok", "status":"completed", "output":[]})),
                )
            }
        }
        let captured: Captured = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = axum::Router::new()
            .route("/responses", axum::routing::post(strict_responses))
            .route("/responses/compact", axum::routing::post(strict_responses))
            .with_state(Arc::clone(&captured));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut settings = GatewaySettings::default();
        settings.security.dns_pinning = false;
        let settings = Arc::new(RwLock::new(settings));
        let state = GatewayState {
            settings: Arc::clone(&settings),
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            routing: Arc::new(Mutex::new(RoutingRuntime::default())),
            observability: ObservabilityLedger::new(None),
            video_jobs: Arc::new(Mutex::new(HashMap::new())),
            instance_id: "provider-metadata-model-switch-test".into(),
            response_state: Arc::new(ResponseStateStore::new(None)),
            memory: Arc::new(MemoryAdmission {
                settings,
                inflight: AtomicU32::new(0),
                reserved_bytes: AtomicU64::new(0),
                rejected: AtomicU64::new(0),
            }),
            pacing: Arc::new(ProviderPacing::default()),
        };
        let gemini = gemini_to_response(&json!({"candidates":[{
            "content":{"role":"model", "parts":[{"functionCall":{"name":"lookup", "args":{"provider_metadata":"user payload"}, "id":"signed"},
                "thoughtSignature":"opaque-gemini-signature"}]}, "finishReason":"STOP"
        }]}), "gemini-test", &ResponseToolMap::default()).unwrap();
        let signed_call = &gemini["output"][0];
        assert_eq!(
            signed_call["provider_metadata"]["gemini"]["thought_signature"],
            "opaque-gemini-signature"
        );
        state.response_state.remember(
            &json!({"model":"gemini-test", "store":false,
            "input":[{"type":"message", "role":"user", "content":"build the model hub"}]}),
            &gemini,
            true,
        );
        let mut switched = json!({"model":"gpt-test", "store":false, "previous_response_id":gemini["id"], "input":[
            {"type":"function_call_output", "call_id":signed_call["call_id"], "output":{"provider_metadata":"keep this tool payload"}},
            {"type":"message", "role":"user", "content":"continue on Cloudflare"}
        ]});
        assert!(state
            .response_state
            .expand_previous_response_input(&mut switched));
        assert!(switched["input"].as_array().unwrap().contains(signed_call));
        let original = switched.clone();
        let rejected = state
            .client
            .post(format!("http://{address}/responses"))
            .json(&switched)
            .send()
            .await
            .unwrap();
        assert_eq!(
            rejected.status(),
            StatusCode::BAD_REQUEST,
            "reproduce pre-fix 400"
        );

        let mut route = candidate(ProviderProtocol::Responses);
        route.provider.base_url = format!("http://{address}");
        route.provider.credential.source = CredentialSource::None;
        for kind in 0..3 {
            let mut request = switched.clone();
            if kind == 1 {
                request["input"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"type":"compaction_trigger"}));
            }
            let response = if kind == 2 {
                send_compact_candidate_once(&state, &request, &route, None).await
            } else {
                send_candidate_once(&state, &request, &route, None, None, None).await
            }
            .unwrap_or_else(|failure| panic!("Responses wire failed: {:?}", failure.response));
            assert!(response.status().is_success());
            let wire = captured.lock().await.pop().unwrap();
            for item in wire["input"].as_array().unwrap() {
                assert!(item.get("provider_metadata").is_none());
                assert!(item.get("codetas_provider_metadata").is_none());
            }
            assert_eq!(wire["input"][1]["arguments"], signed_call["arguments"]);
            assert_eq!(wire["input"][2]["output"], original["input"][2]["output"]);
            assert_eq!(wire["input"][3], original["input"][3]);
            assert_eq!(
                switched, original,
                "saved replay history must not be stripped"
            );
        }
        let back_to_gemini = responses_to_gemini(&switched, "gemini-test").unwrap();
        assert!(back_to_gemini["contents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|content| content["parts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|part| part["thoughtSignature"] == "opaque-gemini-signature")));
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn compact_wire_budget_sender_paths_preserve_history_and_reject_oversized_text() {
        // Exercise the real HTTP senders, not just the shared fitting helper.
        // All traffic stays on an ephemeral loopback server; no saved settings
        // or active gateway instance are read or changed.
        type Captured = Arc<Mutex<Vec<(String, Value)>>>;
        async fn capture(
            axum::extract::State(captured): axum::extract::State<Captured>,
            uri: axum::http::Uri,
            bytes: axum::body::Bytes,
        ) -> axum::Json<Value> {
            assert!(bytes.len() <= 2048);
            captured.lock().await.push((
                uri.path().to_string(),
                serde_json::from_slice(&bytes).unwrap(),
            ));
            axum::Json(json!({"id": "fixture-response", "output": []}))
        }
        let captured: Captured = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = axum::Router::new()
            .route("/responses", axum::routing::post(capture))
            .route("/responses/compact", axum::routing::post(capture))
            .with_state(Arc::clone(&captured));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let mut settings = GatewaySettings::default();
        settings.security.dns_pinning = false;
        let settings = Arc::new(RwLock::new(settings));
        let state = GatewayState {
            settings: Arc::clone(&settings),
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            routing: Arc::new(Mutex::new(RoutingRuntime::default())),
            observability: ObservabilityLedger::new(None),
            video_jobs: Arc::new(Mutex::new(HashMap::new())),
            instance_id: "compact-wire-budget-test".into(),
            response_state: Arc::new(ResponseStateStore::new(None)),
            memory: Arc::new(MemoryAdmission {
                settings,
                inflight: AtomicU32::new(0),
                reserved_bytes: AtomicU64::new(0),
                rejected: AtomicU64::new(0),
            }),
            pacing: Arc::new(ProviderPacing::default()),
        };
        let mut candidate = candidate(ProviderProtocol::Responses);
        candidate.provider.base_url = format!("http://{address}");
        candidate.provider.credential.source = CredentialSource::None;
        candidate.provider.limits.max_request_bytes = 2048;
        candidate.provider.limits.request_timeout_ms = 10_000;

        for native in [false, true] {
            for encoded in [false, true] {
                let content = json!([
                    {"type": "input_text", "text": "important tool result"},
                    {"type": "input_image", "image_url": format!("data:image/png;base64,{}", "A".repeat(4096))}
                ]);
                let mut body = json!({"model": "fixture-model", "input": [
                    {"type": "custom_tool_call", "call_id": "c", "name": "view_image", "input": "fixture"},
                    {"type": "custom_tool_call_output", "call_id": "c", "output":
                        if encoded { json!(serde_json::to_string(&content).unwrap()) } else { content }},
                    {"type": "message", "role": "user", "content": "latest request"},
                    {"type": "message", "role": "assistant", "content": "latest question?"},
                    {"type": "function_call", "call_id": "failed-fetch", "name": "_fetch", "arguments": ""},
                    {"type": "function_call_output", "call_id": "failed-fetch", "output": "INVALID_ARGUMENT: missing url"}
                ]});
                if !native {
                    body["input"].as_array_mut().unwrap().push(json!({"type": "compaction_trigger"}));
                }
                let original = body.clone();
                let result = if native {
                    send_compact_candidate_once(&state, &body, &candidate, None).await
                } else {
                    send_candidate_once(&state, &body, &candidate, None, None, None).await
                };
                if encoded {
                    let failure = result.expect_err("encoded tool text must not be pruned as an image");
                    let error = to_bytes(failure.response.into_body(), 8192).await.unwrap();
                    let error: Value = serde_json::from_slice(&error).unwrap();
                    assert_eq!(error["error"]["code"], "request_too_large");
                    assert_eq!(body, original);
                    assert!(captured.lock().await.is_empty());
                    continue;
                }
                let response = match result {
                    Ok(response) => response,
                    Err(failure) => {
                        let status = failure.response.status();
                        let bytes = to_bytes(failure.response.into_body(), 8192).await.unwrap();
                        panic!("sender failed: {status}: {}", String::from_utf8_lossy(&bytes));
                    }
                };
                assert!(response.status().is_success());
                assert_eq!(body, original, "source replay history changed");
                let (path, wire) = captured.lock().await.pop().unwrap();
                assert_eq!(path, if native { "/responses/compact" } else { "/responses" });
                assert_eq!(wire["input"][0], original["input"][0]);
                assert_eq!(wire["input"][2], original["input"][2]);
                assert_eq!(wire["input"][3], original["input"][3]);
                if !native {
                    assert_eq!(wire["input"][6], original["input"][6]);
                } else {
                    assert_eq!(wire["input"][4]["arguments"], "{}");
                    assert_eq!(wire["input"][5], original["input"][5]);
                }
                let output = &wire["input"][1]["output"];
                let output: Value = if encoded {
                    serde_json::from_str(output.as_str().unwrap()).unwrap()
                } else {
                    output.clone()
                };
                assert_eq!(output[0]["text"], "important tool result");
                assert_eq!(output[1]["type"], "input_text");
                assert!(output[1]["text"].as_str().unwrap().contains("image omitted"));
            }
            let mut body = json!({"model": "fixture-model", "input": [
                {"type": "message", "role": "user", "content": "x".repeat(4096)}
            ]});
            if !native {
                body["input"].as_array_mut().unwrap().push(json!({"type": "compaction_trigger"}));
            }
            let original = body.clone();
            let result = if native {
                send_compact_candidate_once(&state, &body, &candidate, None).await
            } else {
                send_candidate_once(&state, &body, &candidate, None, None, None).await
            };
            let failure = match result {
                Err(failure) => failure,
                Ok(_) => panic!("oversized text was sent upstream"),
            };
            let error = axum::body::to_bytes(failure.response.into_body(), 8192).await.unwrap();
            let error: Value = serde_json::from_slice(&error).unwrap();
            assert_eq!(error["error"]["code"], "request_too_large");
            assert_eq!(body, original);
            assert!(captured.lock().await.is_empty());
        }
        server.abort();
    }

    fn candidate(protocol: ProviderProtocol) -> RouteCandidate {
        let mut provider = ProviderDefinition::default();
        provider.id = "fixture".into();
        provider.protocol = protocol;
        let capabilities = provider.capabilities.clone();
        RouteCandidate {
            provider,
            upstream_model: "fixture-model".into(),
            exposed_model: "fixture-model".into(),
            credential: None,
            account_id: None,
            target_key: "fixture/fixture-model".into(),
            route_id: None,
            failure_threshold: 0,
            quota_threshold_percent: 0,
            input_price_per_million: None,
            output_price_per_million: None,
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: Some(2_048),
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            capabilities,
            routing_epoch: 0,
            routing_generation: 0,
            session_scope: None,
        }
    }

    #[test]
    fn synthetic_compaction_failures_preserve_internal_retry_observation() {
        let retry = ProviderRetryObservation {
            thinking_provenance: None,
            request_diagnostic: None,
            known_billing:None,
            terminal_failure:false,
            cache_observation: None,
            additional_sends: 2,
            recovery_kinds: vec!["empty-completion".into(), "http-5xx".into()],
            usage: TokenUsage {
                input_tokens: 7,
                total_tokens: 7,
                ..TokenUsage::default()
            },
        };
        let failure = compaction_failure_with_retry(
            request_failure("invalid_provider_response", "fixture"),
            Some(&retry),
        );
        let preserved = failure
            .response
            .extensions()
            .get::<ProviderRetryObservation>()
            .expect("retry observation");
        assert_eq!(preserved.additional_sends, 2);
        assert_eq!(preserved.recovery_kinds, retry.recovery_kinds);
        assert_eq!(preserved.usage.input_tokens, 7);
    }

    #[test]
    fn synthetic_compaction_quota_classification_survives_caller_handoff() {
        let mut failure = request_failure("rate_limit_exceeded", "retry later");
        failure
            .response
            .extensions_mut()
            .insert(CompactionQuotaExhausted {
                retry_after: Some(Duration::from_secs(7_200)),
            });
        let failure = compaction_failure_with_retry(failure, None);

        let quota = failure
            .response
            .extensions()
            .get::<CompactionQuotaExhausted>()
            .expect("quota classification");
        assert_eq!(quota.retry_after, Some(Duration::from_secs(7_200)));
    }

    fn controlled_compaction_body() -> Value {
        json!({
            "model": "fixture-model",
            "stream": true,
            "text": {"format": {"type": "json_schema", "name": "summary", "schema": {"type": "object"}}},
            "response_format": {"type": "json_object"},
            "parallel_tool_calls": true,
            "tool_choice": "required",
            "tools": [{"type": "function", "name": "top_level_tool", "parameters": {"type": "object"}}],
            "previous_response_id": "resp_previous",
            "conversation": "conversation_previous",
            "input": [
                {"type": "message", "role": "user", "content": "retain this conversation"},
                {"type": "additional_tools", "tools": [{"type": "function", "name": "late_tool", "parameters": {"type": "object"}}]},
                {"type": "tool_search_output", "call_id": "ts_1", "status": "completed", "tools": [{"type": "function", "name": "searched_tool", "parameters": {"type": "object"}}]},
                {"type": "compaction_trigger"}
            ]
        })
    }

    fn assert_prose_only_request(request: &Value) {
        for field in [
            "text",
            "response_format",
            "parallel_tool_calls",
            "tool_choice",
            "previous_response_id",
            "conversation",
        ] {
            assert!(request.get(field).is_none(), "{field} must be removed");
        }
        assert_eq!(request["stream"], false);
        assert_eq!(request["store"], false);
        assert_eq!(request["tools"], json!([]));
        let input = request["input"].as_array().expect("compaction input");
        assert!(!input.iter().any(|item| matches!(
            item.get("type").and_then(Value::as_str),
            Some("compaction_trigger" | "additional_tools")
        )));
        assert!(input
            .iter()
            .filter(|item| {
                item.get("type").and_then(Value::as_str) == Some("tool_search_output")
            })
            .all(|item| item.get("tools").is_none()));
        assert!(response_tool_map(request).iter().next().is_none());
    }

    #[test]
    fn responses_synthetic_compaction_isolated_from_schema_and_tool_controls() {
        let request = prepare_synthetic_compaction_request(
            &controlled_compaction_body(),
            &candidate(ProviderProtocol::Responses),
            None,
        )
        .unwrap_or_else(|_| panic!("valid synthetic compaction request"));

        assert_prose_only_request(&request);
    }

    #[test]
    fn translated_synthetic_compaction_cannot_recollect_tools_or_response_schema() {
        let route = candidate(ProviderProtocol::ChatCompletions);
        let request =
            prepare_synthetic_compaction_request(&controlled_compaction_body(), &route, None)
                .unwrap_or_else(|_| panic!("valid synthetic compaction request"));
        assert_prose_only_request(&request);

        let translated = responses_to_chat_with_options(&request, "fixture-model", false)
            .expect("synthetic request translates");
        assert!(translated.get("tools").is_none());
        assert!(translated.get("tool_choice").is_none());
        assert!(translated.get("parallel_tool_calls").is_none());
        assert!(translated.get("response_format").is_none());
    }

    #[test]
    fn responses_and_translated_compaction_sources_require_completed_terminals() {
        for value in [
            json!({"status": "failed", "error": {"message": "upstream exploded"}, "output": []}),
            json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, "output": [{"type": "message", "content": [{"type": "output_text", "text": "partial"}]}]}),
            json!({"status": "completed", "error": {"message": "contradictory"}, "output": []}),
        ] {
            assert!(require_completed_compaction_source(&value).is_err());
        }

        assert!(require_completed_compaction_source(&json!({
            "status": "completed",
            "error": null,
            "incomplete_details": null,
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "summary"}]}]
        }))
        .is_ok());
    }

    #[test]
    fn leaked_or_tiny_model_summaries_are_not_installed_as_history() {
        let leaked = json!({
            "status": "completed",
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "特定します。\n<file_end><|eos|>"}]
            }]
        });
        assert!(ensure_single_compaction_output(leaked, "fixture-model").is_err());
    }

    #[test]
    fn incomplete_or_failed_responses_are_never_wrapped_as_compaction_history() {
        for value in [
            json!({"status": "failed", "error": {"message": "failed"}, "output": []}),
            json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, "output": [{"type": "message", "content": [{"type": "output_text", "text": "partial"}]}]}),
        ] {
            assert!(ensure_single_compaction_output(value, "fixture-model").is_err());
        }
    }

    #[test]
    fn native_compact_request_expands_local_envelopes_before_upstream() {
        let encrypted = encode_summary("keep this prior summary").expect("valid envelope");
        let mut body = json!({
            "model": "gpt-wire",
            "reasoning": {"effort": "high"},
            "input": [
                {"type": "compaction", "encrypted_content": encrypted},
                {"type": "message", "role": "user", "content": "continue"},
                {"type": "compaction_trigger", "id": "trigger_1"}
            ]
        });

        expand_local_compactions(&mut body);
        crate::compat::sanitize_compact_request(&mut body);

        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "assistant");
        assert!(body["input"][0]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("keep this prior summary")));
        assert_eq!(body["input"][1]["role"], "user");
        assert_eq!(body["input"][2]["type"], "compaction_trigger");
        assert!(body.get("reasoning").is_none());
        assert!(!serde_json::to_string(&body).unwrap().contains("codetas1:"));
    }

    #[test]
    fn offline_compaction_keeps_native_trigger_history_without_an_upstream_summary() {
        let body = json!({
            "model": "xai/grok-4.6",
            "stream": true,
            "input": [
                {"type": "message", "role": "user", "content": "continue the main line"},
                {"type": "compaction_trigger", "id": "trigger_1"}
            ]
        });
        let (value, usage) = offline_compact_value(
            &body,
            "xai/grok-4.6",
            CompactionRequestKind::NativeTrigger,
            &crate::config::LocalCompactionSettings::default(),
        )
        .expect("offline compact");
        assert_eq!(usage.total_tokens, 0);
        assert_eq!(value["object"], "response.compaction");
        assert_eq!(value["output"][0]["type"], "compaction");
        let encrypted = value["output"][0]["encrypted_content"]
            .as_str()
            .expect("local envelope");
        assert!(encrypted.starts_with("codetas2:") || encrypted.starts_with("codetas1:"));
        let decoded = crate::compaction::decode_summary(&value["output"][0])
            .expect("decode local envelope")
            .expect("checkpoint text");
        assert!(decoded.contains("cooling down"));
        assert!(decoded.contains("continue the main line"));
    }

    #[test]
    fn offline_compaction_without_task_progress_does_not_install_an_empty_success() {
        let body = json!({
            "model": "xai/grok-4.6",
            "input": [
                {"type": "compaction_trigger", "id": "trigger_1"}
            ]
        });
        let error = offline_compact_value(
            &body,
            "xai/grok-4.6",
            CompactionRequestKind::NativeTrigger,
            &crate::config::LocalCompactionSettings::default(),
        )
        .expect_err("empty offline compact");
        assert!(error.contains("no recoverable task progress"));
    }

    #[test]
    fn offline_compaction_recovers_tool_result_only_history_in_both_outputs() {
        for kind in [
            CompactionRequestKind::NativeTrigger,
            CompactionRequestKind::Standalone,
        ] {
            for output in ["deployment_id=dep-902", "", " \t\r\n"] {
                let body = json!({"input":[
                    {"type":"function_call", "name":"lookup", "call_id":"lookup", "arguments":"{}"},
                    {"type":"function_call_output", "call_id":"lookup", "output":output}
                ]});
                let (value, _) = offline_compact_value(
                    &body,
                    "xai/grok-4.6",
                    kind,
                    &crate::config::LocalCompactionSettings::default(),
                )
                .expect("recover completed result");
                let history = crate::compaction::normalize_compaction_history(
                    value["output"].as_array().unwrap(),
                )
                .unwrap();
                let checkpoint = history.previous_checkpoint.unwrap();
                let quoted = checkpoint
                    .lines()
                    .find_map(|line| line.strip_prefix("- lookup -> "))
                    .unwrap();
                assert_eq!(serde_json::from_str::<String>(quoted).unwrap(), output);
            }
        }
    }

    #[test]
    fn completed_empty_connector_arguments_survive_synthetic_and_repeated_offline_compaction() {
        let failure = json!({"type":"function_call_output", "call_id":"failed-fetch",
            "output":"INVALID_ARGUMENT: url is required"});
        let body = json!({"model":"fixture-model", "input":[
            {"type":"function_call", "call_id":"failed-fetch", "name":"_fetch", "arguments":""},
            failure,
            {"type":"message", "role":"assistant", "content":"Which course should I review?"},
            {"type":"message", "role":"user", "content":"continue the course review"}
        ]});
        for protocol in [
            ProviderProtocol::Responses,
            ProviderProtocol::AnthropicMessages,
        ] {
            let request =
                prepare_synthetic_compaction_request(&body, &candidate(protocol), None).unwrap();
            assert_eq!(request["input"][0]["arguments"], "{}");
            assert_eq!(request["input"][1], failure);
        }
        for kind in [
            CompactionRequestKind::NativeTrigger,
            CompactionRequestKind::Standalone,
        ] {
            let mut current = body.clone();
            for _ in 0..3 {
                let (value, _) = offline_compact_value(
                    &current,
                    "fixture-model",
                    kind,
                    &crate::config::LocalCompactionSettings::default(),
                )
                .unwrap();
                let history = crate::compaction::normalize_compaction_history(
                    value["output"].as_array().unwrap(),
                )
                .unwrap();
                assert!(history.items.contains(&failure));
                assert!(history.items.contains(&body["input"][2]));
                assert!(history.items.contains(&body["input"][3]));
                current["input"] = value["output"].clone();
            }
        }
        assert_eq!(body["input"][0]["arguments"], "", "source input untouched");
    }

    #[test]
    fn offline_compaction_rejects_invalid_history_even_outside_the_tail() {
        let call =
            json!({"type":"function_call", "call_id":"c", "name":"lookup", "arguments":"{}"});
        let output = json!({"type":"function_call_output", "call_id":"c", "output":"value"});
        let mut bad_arguments = call.clone();
        bad_arguments["arguments"] = json!("invalid-json");
        for input in [
            vec![bad_arguments, output.clone()],
            vec![output.clone()],
            vec![call.clone(), call.clone(), output.clone()],
        ] {
            for kind in [
                CompactionRequestKind::NativeTrigger,
                CompactionRequestKind::Standalone,
            ] {
                let mut input = input.clone();
                input.push(json!({"type":"message", "role":"user", "content":"continue"}));
                let settings = crate::config::LocalCompactionSettings {
                    tail_token_limit: 1,
                    ..Default::default()
                };
                assert!(offline_compact_value(
                    &json!({"input":input}),
                    "xai/grok-4.6",
                    kind,
                    &settings
                )
                .is_err());
            }
        }
    }

    #[test]
    fn standalone_synthetic_compaction_uses_shared_checkpoint_and_retained_tail() {
        let input = json!([
            {"type": "message", "role": "developer", "content": "drop"},
            {"type": "message", "role": "user", "content": "first user requirement"},
            {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"},
            {"type": "message", "role": "assistant", "content": "drop"},
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "latest "},
                {"type": "text", "text": "request"}
            ]}
        ]);
        let history = crate::compaction::normalize_compaction_history(input.as_array().unwrap())
            .expect("normalize");
        let checkpoint = "## User requirements and confirmed facts\n- first user requirement\n\n## User corrections and open disagreements\n- none\n\n## Durable observations\n- lookup returned ok\n\n## Agent conclusions (unverified)\n- none\n\n## Remaining work\n- answer the latest request";
        let (context, _) = crate::compaction::build_compacted_context(
            &history,
            checkpoint.to_string(),
            &crate::config::LocalCompactionSettings::default(),
        )
        .expect("context");
        let output =
            crate::compaction::standalone_output_items(&context).expect("standalone output");

        assert_eq!(output[0]["role"], "assistant");
        assert!(output[0]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("first user requirement")));
        assert!(output.iter().any(|item| {
            item.get("role").and_then(Value::as_str) == Some("user")
                && item.to_string().contains("latest")
        }));
        assert!(output
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call")));
        assert!(output
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call_output")));
        assert!(!serde_json::to_string(&output)
            .unwrap()
            .contains("codetas1:"));
        assert!(!serde_json::to_string(&output)
            .unwrap()
            .contains("\"type\":\"compaction\""));
    }

    #[test]
    fn native_trigger_empty_tiny_and_control_token_summaries_are_rejected() {
        let history = crate::compaction::normalize_compaction_history(&[
            json!({"type": "message", "role": "user", "content": "keep going"}),
        ])
        .expect("normalize");
        let settings = crate::config::LocalCompactionSettings::default();
        for summary in [
            "",
            "too short",
            "This looks long enough to pass the tiny check but it leaks <|eos|> and should fail.",
        ] {
            assert!(crate::compaction::build_compacted_context(
                &history,
                summary.to_string(),
                &settings
            )
            .is_err());
        }
    }

    #[test]
    fn wrap_path_honors_v1_generation_rollback() {
        let summary = "## User requirements and confirmed facts\n- keep going\n\n## User corrections and open disagreements\n- none\n\n## Durable observations\n- none\n\n## Agent conclusions (unverified)\n- none\n\n## Remaining work\n- continue";
        let value = json!({
            "status": "completed",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": summary}]
            }]
        });
        let v1 = crate::config::LocalCompactionSettings {
            envelope: crate::config::LocalCompactionEnvelope::V1,
            tail_token_limit: 20_000,
        };
        let wrapped = ensure_single_compaction_output_from_history(
            value,
            "fixture-model",
            &[json!({"type": "message", "role": "user", "content": "keep going"})],
            &v1,
        )
        .expect("wrap");
        let encrypted = wrapped["output"][0]["encrypted_content"].as_str().unwrap();
        assert!(encrypted.starts_with("codetas1:"));
        assert!(!encrypted.starts_with("codetas2:"));
    }
}


fn merge_compaction_repair_failure(mut failure: AttemptFailure, retry: &mut Option<ProviderRetryObservation>) -> AttemptFailure {
    let nested = failure.response.extensions_mut().remove::<ProviderRetryObservation>();
    merge_provider_retry_observation(retry, nested);
    compaction_failure_with_retry(failure, retry.as_ref())
}

#[cfg(test)]
mod review11_repair_failure_tests {
    use super::*;
    #[test]
    fn repair_send_failure_merges_both_retry_chains_and_first_summary_usage() {
        let mut previous = Some(ProviderRetryObservation {
            usage: TokenUsage { input_tokens: 100, total_tokens: 100, ..Default::default() },
            additional_sends: 2, recovery_kinds: vec!["provider-5xx".into(), "compaction-repair".into()], ..Default::default()
        });
        let mut failure = request_failure("fixture", "repair failed");
        failure.response.extensions_mut().insert(ProviderRetryObservation {
            usage: TokenUsage { input_tokens: 200, total_tokens: 200, ..Default::default() },
            additional_sends: 1, recovery_kinds: vec!["transport".into()], ..Default::default()
        });
        failure.response.extensions_mut().insert(crate::observability::ReportedUpstreamUsage(
            TokenUsage { input_tokens: 300, total_tokens: 300, ..Default::default() }));
        let failure = merge_compaction_repair_failure(failure, &mut previous);
        let retry = failure.response.extensions().get::<ProviderRetryObservation>().unwrap();
        assert_eq!(retry.additional_sends, 3);
        assert_eq!(retry.usage.input_tokens, 300);
        assert_eq!(retry.recovery_kinds, ["provider-5xx", "compaction-repair", "transport"]);
        let ledger = ObservabilityLedger::new(None);
        let mut observation = ObservationSeed::without_candidate(ledger.clone(), ObservabilitySettings::default(),
            "repair-failure".into(), "fixture/model", false, Instant::now());
        observation.record_provider_retries(retry);
        observation.record_upstream_error(&failure.response);
        observation.finish(StatusCode::BAD_GATEWAY, Some("repair"), TokenUsage::default());
        assert_eq!(ledger.summary().input_tokens, 600);
        assert_eq!(ledger.summary().failed_requests, 1);
    }
}

async fn read_compaction_candidate(
    upstream: reqwest::Response, candidate: &RouteCandidate,
    billed: &mut TokenUsage, retry: Option<&ProviderRetryObservation>,
) -> Result<Value, AttemptFailure> {
    match candidate_response_value(upstream, candidate, candidate.provider.limits.max_response_bytes).await {
        Ok(value) => {
            *billed = sum_token_usage(billed.clone(), TokenUsage::from_json(&value));
            Ok(value)
        }
        Err((message, usage)) => {
            *billed = sum_token_usage(billed.clone(), usage);
            Err(compaction_failure_with_retry(AttemptFailure {
                response: error_response(StatusCode::BAD_GATEWAY, "invalid_provider_response", &message),
                kind: AttemptFailureKind::Retryable,
            }, retry))
        }
    }
}

#[cfg(test)]
mod review14_kiro_compact_regressions {
    use super::*;
    fn frame(event:&str,value:Value)->Vec<u8> {
        let mut headers=Vec::new();
        for (key,value) in [(":message-type","event"),(":event-type",event)] {
            headers.push(key.len() as u8);headers.extend(key.as_bytes());headers.push(7);
            headers.extend((value.len() as u16).to_be_bytes());headers.extend(value.as_bytes());
        }
        let payload=serde_json::to_vec(&value).unwrap();let total=16+headers.len()+payload.len();
        let mut out=Vec::new();out.extend((total as u32).to_be_bytes());out.extend((headers.len() as u32).to_be_bytes());
        out.extend(crc32fast::hash(&out).to_be_bytes());out.extend(headers);out.extend(payload);out.extend(crc32fast::hash(&out).to_be_bytes());out
    }
    fn response(bytes:Vec<u8>,context:bool)->reqwest::Response {
        let mut response=reqwest::Response::from(axum::http::Response::builder().status(200).body(reqwest::Body::from(bytes)).unwrap());
        if context {response.extensions_mut().insert(KiroRequestContext {conversation_id:"fixture".into(),response_id:"resp_fixture".into(),streaming:false,tool_names:Default::default(),completion_tool:None});}
        response
    }
    #[tokio::test]
    async fn review14_summary_and_repair_conversion_recover_known_usage_once() {
        let mut candidate=super::super::anthropic_compact::review13_resolved_auth_regressions::candidate();
        candidate.provider.transport=ProviderTransport::Kiro;
        for has_context in [false,true] {
            let mut billed=TokenUsage {input_tokens:10,total_tokens:10,..Default::default()};
            for _ in 0..2 {
                let mut bytes=frame("metadataEvent",json!({"tokenUsage":{"uncachedInputTokens":20,"cacheReadInputTokens":80,"outputTokens":3}}));
                bytes.extend([0,1,2]);
                let failure=read_compaction_candidate(response(bytes,has_context),&candidate,&mut billed,None).await.unwrap_err();
                assert_eq!(failure.kind,AttemptFailureKind::Retryable);
            }
            assert_eq!(billed.input_tokens,210);assert_eq!(billed.cached_input_tokens,160);assert_eq!(billed.output_tokens,6);
            // The existing failure observation channel owns the recovered billing,
            // including an offline success or a failed candidate before failover.
            for attempt in [false,true] {
                let ledger=ObservabilityLedger::new(None);
                let mut seed=ObservationSeed::for_candidate(ledger.clone(),ObservabilitySettings::default(),"r14".into(),false,Instant::now(),1,&candidate)
                    .with_compaction_session(&HeaderMap::new());
                let mut failure=error_response(StatusCode::BAD_GATEWAY,"invalid_provider_response","fixture");
                failure.extensions_mut().insert(crate::observability::ReportedUpstreamUsage(billed.clone()));
                seed.record_upstream_error(&failure);
                if attempt {seed=seed.as_attempt();}
                seed.finish(if attempt {StatusCode::BAD_GATEWAY}else{StatusCode::OK},None,TokenUsage::default());
                assert_eq!(ledger.summary().input_tokens,210);
            }
        }
    }
}
