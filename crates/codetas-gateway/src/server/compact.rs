use super::*;

pub(crate) async fn send_compact_candidate(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
) -> Result<reqwest::Response, AttemptFailure> {
    retry_provider_request(candidate, false, || {
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
    expand_local_compactions(&mut body);
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
) -> Result<(Value, TokenUsage), AttemptFailure> {
    crate::debug::log(&format!(
        "synthetic_compact_candidate: begin model={} input_items={}",
        candidate.upstream_model,
        body.get("input").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0)
    ));
    let mut request = body.clone();
    strip_translated_input_images_for_compaction(&mut request);
    let Some(object) = request.as_object_mut() else {
        return Err(request_failure(
            "invalid_compaction_request",
            "compact request must be an object",
        ));
    };
    let input = object
        .get_mut("input")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            request_failure(
                "invalid_compaction_request",
                "compact request input must be an array",
            )
        })?;
    // `compaction_trigger` is a control marker accepted by the Responses
    // compaction endpoint. It marks where compaction should occur, but it is
    // not conversation content and cannot be represented in Chat Completions.
    // Remove it only on this dedicated compaction path; the normal Responses
    // adapter remains strict about unknown input item types.
    input.retain(|item| item.get("type").and_then(Value::as_str) != Some("compaction_trigger"));
    input.push(json!({
        "type": "message",
        "role": "developer",
        "content": [{
            "type": "input_text",
            "text": "Create a faithful compact summary of the conversation state. Preserve user requirements, decisions, constraints, file paths, completed work, unresolved work, and tool results needed to continue. Do not add commentary; output only the summary."
        }]
    }));
    object.insert("stream".into(), Value::Bool(false));
    object.insert("store".into(), Value::Bool(false));
    object.insert("tools".into(), Value::Array(Vec::new()));
    object.remove("tool_choice");
    object.remove("previous_response_id");
    object.insert(
        "max_output_tokens".into(),
        json!(candidate
            .max_output_tokens
            .unwrap_or(4_096)
            .clamp(256, 8_192)),
    );
    // Compaction envelopes carry the full conversation history by design and
    // routinely exceed the model input budget, so no input-size gate is applied.

    let upstream = send_candidate(state, &mut request, candidate, Some(caller_headers)).await?;
    crate::debug::log(&format!(
        "synthetic_compact_candidate: upstream status={}",
        upstream.status()
    ));
    if !upstream.status().is_success() {
        let status = upstream.status();
        let classified = upstream_responses_error_classified(upstream, None).await;
        return Err(AttemptFailure {
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
        });
    }
    let upstream_value = candidate_response_value(
        upstream,
        candidate,
        candidate.provider.limits.max_response_bytes,
    )
    .await
    .map_err(|message| AttemptFailure {
        response: error_response(
            StatusCode::BAD_GATEWAY,
            "invalid_provider_response",
            &message,
        ),
        kind: AttemptFailureKind::Retryable,
    })?;
    let adapted = match candidate
        .provider
        .protocol_for_model(&candidate.upstream_model)
    {
        ProviderProtocol::Responses => Ok(upstream_value),
        ProviderProtocol::ChatCompletions => {
            chat_to_response(
                &upstream_value,
                &candidate.exposed_model,
                &ResponseToolMap::default(),
            )
        }
        ProviderProtocol::AnthropicMessages => {
            anthropic_to_response(
                &upstream_value,
                &candidate.exposed_model,
                &ResponseToolMap::default(),
            )
        }
        ProviderProtocol::GeminiGenerateContent => {
            gemini_to_response(
                &upstream_value,
                &candidate.exposed_model,
                &ResponseToolMap::default(),
            )
        }
    }
    .map_err(|message| AttemptFailure {
        response: error_response(
            StatusCode::BAD_GATEWAY,
            "invalid_provider_response",
            &message,
        ),
        kind: AttemptFailureKind::Retryable,
    })?;
    let summary = response_output_text(&adapted);
    let encrypted = encode_summary(&summary).map_err(|message| AttemptFailure {
        response: error_response(
            StatusCode::BAD_GATEWAY,
            "invalid_compaction_response",
            &message,
        ),
        kind: AttemptFailureKind::Retryable,
    })?;
    let usage = TokenUsage::from_json(&adapted);
    let compacted = json!({
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
    });
    Ok((compacted, usage))
}

pub(crate) fn ensure_single_compaction_output(value: Value, model: &str) -> Result<Value, String> {
    if compaction_item_count(&value) == 1 {
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
