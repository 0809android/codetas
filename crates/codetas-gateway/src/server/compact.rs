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
    let mut body = body.clone();
    body["model"] = Value::String(candidate.provider.wire_model_id(&candidate.upstream_model));
    if !request_is_remote_compaction(&body) {
        expand_local_compactions(&mut body);
    }
    // Compaction envelopes must reach the backend verbatim (see apply_provider_request_compatibility).
    crate::debug::log("send_compact_candidate: sanitize SKIPPED (verbatim envelope)");
    let _ = &candidate;
    let serialized = serde_json::to_vec(&body).map_err(|error| {
        request_failure(
            "invalid_request",
            &format!("request cannot be encoded: {error}"),
        )
    })?;
    if serialized.len() as u64 > candidate.provider.limits.max_request_bytes {
        return Err(request_failure(
            "request_too_large",
            "compaction request exceeds the configured provider limit",
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
    match request.body(serialized).send().await {
        Ok(response)
            if response
                .content_length()
                .is_some_and(|length| length > candidate.provider.limits.max_response_bytes) =>
        {
            Err(AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "provider_response_too_large",
                    "provider response exceeds the configured limit",
                ),
                kind: AttemptFailureKind::Retryable,
            })
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

pub(crate) async fn synthetic_compact_candidate(
    state: &GatewayState,
    caller_headers: &HeaderMap,
    body: &Value,
    candidate: &RouteCandidate,
    request_kind: CompactionRequestKind,
) -> Result<(Value, TokenUsage, Option<ProviderRetryObservation>), AttemptFailure> {
    crate::debug::log(&format!(
        "synthetic_compact_candidate: begin model={} input_items={}",
        candidate.upstream_model,
        body.get("input").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0)
    ));
    let request = prepare_synthetic_compaction_request(body, candidate)?;
    // Compaction envelopes carry the full conversation history by design and
    // routinely exceed the model input budget, so no input-size gate is applied.

    let mut request = request;
    let upstream = send_candidate(state, &mut request, candidate, Some(caller_headers)).await?;
    let provider_retry = upstream.extensions()
        .get::<ProviderRetryObservation>().cloned();
    crate::debug::log(&format!(
        "synthetic_compact_candidate: upstream status={}",
        upstream.status()
    ));
    if !upstream.status().is_success() {
        let status = upstream.status();
        let retry_after = validated_retry_after(upstream.headers());
        let retry_duration = retry_after.as_ref().and_then(|value| value.1);
        let classified = upstream_responses_error_classified(
            upstream,
            retry_after.as_ref().map(|value| &value.0),
        )
        .await;
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
            failure.response.extensions_mut().insert(CompactionQuotaExhausted {
                retry_after: retry_duration,
            });
        }
        return Err(compaction_failure_with_retry(failure, provider_retry.as_ref()));
    }
    let upstream_value = candidate_response_value(
        upstream,
        candidate,
        candidate.provider.limits.max_response_bytes,
    )
    .await
    .map_err(|message| compaction_failure_with_retry(AttemptFailure {
        response: error_response(
            StatusCode::BAD_GATEWAY,
            "invalid_provider_response",
            &message,
        ),
        kind: AttemptFailureKind::Retryable,
    }, provider_retry.as_ref()))?;
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
    .map_err(|message| compaction_failure_with_retry(AttemptFailure {
        response: error_response(
            StatusCode::BAD_GATEWAY,
            "invalid_provider_response",
            &message,
        ),
        kind: AttemptFailureKind::Retryable,
    }, provider_retry.as_ref()))?;
    require_completed_compaction_source(&adapted).map_err(|message| compaction_failure_with_retry(AttemptFailure {
        response: error_response(
            StatusCode::BAD_GATEWAY,
            "invalid_compaction_response",
            &message,
        ),
        kind: AttemptFailureKind::Retryable,
    }, provider_retry.as_ref()))?;
    let summary = response_output_text(&adapted);
    let usage = TokenUsage::from_json(&adapted);
    let compacted = match request_kind {
        CompactionRequestKind::Standalone => json!({
            "output": build_compact_v1_output(body.get("input"), &summary)
        }),
        CompactionRequestKind::NativeTrigger => {
            let encrypted = encode_summary(&summary).map_err(|message| compaction_failure_with_retry(AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "invalid_compaction_response",
                    &message,
                ),
                kind: AttemptFailureKind::Retryable,
            }, provider_retry.as_ref()))?;
            json!({
                "id": format!("cmpct_{}", Uuid::new_v4().simple()),
                "object": "response.compaction",
                "created_at": ObservationEvent::now_ms() / 1_000,
                "model": candidate.exposed_model,
                "output": [{
                    "id": format!("cmpctitem_{}", Uuid::new_v4().simple()),
                    "type": "compaction",
                    "encrypted_content": encrypted,
                    "created_by": "codetas"
                }],
                "usage": adapted.get("usage").cloned().unwrap_or_else(|| json!({
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens,
                    "total_tokens": usage.total_tokens
                }))
            })
        }
    };
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
const COMPACT_SUMMARY_PREFIX: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";

fn compact_user_message_text(item: &Value) -> Option<String> {
    if item.get("type").and_then(Value::as_str).is_some_and(|kind| kind != "message")
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
) -> Result<Value, AttemptFailure> {
    let mut request = body.clone();
    strip_translated_input_images_for_compaction(&mut request);
    let Some(object) = request.as_object_mut() else {
        return Err(request_failure(
            "invalid_compaction_request",
            "compact request must be an object",
        ));
    };
    for field in [
        "text",
        "response_format",
        "parallel_tool_calls",
        "tool_choice",
        "previous_response_id",
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
    let input = object
        .get_mut("input")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            request_failure(
                "invalid_compaction_request",
                "compact request input must be an array",
            )
        })?;
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
            "text": "Create a faithful compact summary of the conversation state. Preserve user requirements, decisions, constraints, file paths, completed work, unresolved work, and tool results needed to continue. Do not add commentary; output only the summary."
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
    let summary = response_output_text(&value);
    let encrypted = encode_summary(&summary)?;
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
        }
    }

    #[test]
    fn synthetic_compaction_failures_preserve_internal_retry_observation() {
        let retry = ProviderRetryObservation {
            additional_sends: 2,
            recovery_kinds: vec!["empty-completion".into(), "http-5xx".into()],
            usage: TokenUsage { input_tokens: 7, total_tokens: 7, ..TokenUsage::default() },
        };
        let failure = compaction_failure_with_retry(
            request_failure("invalid_provider_response", "fixture"),
            Some(&retry),
        );
        let preserved = failure.response.extensions()
            .get::<ProviderRetryObservation>().expect("retry observation");
        assert_eq!(preserved.additional_sends, 2);
        assert_eq!(preserved.recovery_kinds, retry.recovery_kinds);
        assert_eq!(preserved.usage.input_tokens, 7);
    }

    #[test]
    fn synthetic_compaction_quota_classification_survives_caller_handoff() {
        let mut failure = request_failure("rate_limit_exceeded", "retry later");
        failure.response.extensions_mut().insert(CompactionQuotaExhausted {
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
            "input": [
                {"type": "message", "role": "user", "content": "retain this conversation"},
                {"type": "additional_tools", "tools": [{"type": "function", "name": "late_tool", "parameters": {"type": "object"}}]},
                {"type": "tool_search_output", "status": "completed", "tools": [{"type": "function", "name": "searched_tool", "parameters": {"type": "object"}}]},
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
        assert!(input.iter().filter(|item| {
            item.get("type").and_then(Value::as_str) == Some("tool_search_output")
        }).all(|item| item.get("tools").is_none()));
        assert!(response_tool_map(request).iter().next().is_none());
    }

    #[test]
    fn responses_synthetic_compaction_isolated_from_schema_and_tool_controls() {
        let request = prepare_synthetic_compaction_request(
            &controlled_compaction_body(),
            &candidate(ProviderProtocol::Responses),
        )
        .unwrap_or_else(|_| panic!("valid synthetic compaction request"));

        assert_prose_only_request(&request);
    }

    #[test]
    fn translated_synthetic_compaction_cannot_recollect_tools_or_response_schema() {
        let route = candidate(ProviderProtocol::ChatCompletions);
        let request = prepare_synthetic_compaction_request(&controlled_compaction_body(), &route)
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
        })).is_ok());
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
    fn standalone_synthetic_compaction_builds_v1_replacement_messages() {
        let input = json!([
            {"type": "message", "role": "developer", "content": "drop"},
            {"type": "message", "role": "user", "content": "first user requirement"},
            {"type": "function_call", "name": "lookup", "arguments": "{}"},
            {"type": "message", "role": "assistant", "content": "drop"},
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "latest "},
                {"type": "text", "text": "request"}
            ]}
        ]);

        let output = build_compact_v1_output(Some(&input), "summary text");

        assert_eq!(output.len(), 3);
        assert_eq!(output[0]["role"], "user");
        assert_eq!(output[0]["content"][0]["text"], "first user requirement");
        assert_eq!(output[1]["content"][0]["text"], "latest request");
        assert!(output[2]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.ends_with("\nsummary text")));
        assert!(output.iter().all(|item| item.get("type").and_then(Value::as_str)
            == Some("message")));
        assert!(!serde_json::to_string(&output).unwrap().contains("codetas1:"));
        assert!(!serde_json::to_string(&output).unwrap().contains("\"type\":\"compaction\""));
    }
}
