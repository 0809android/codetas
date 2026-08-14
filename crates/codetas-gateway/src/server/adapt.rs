use super::*;

pub(crate) async fn adapt_successful_response(
    upstream: reqwest::Response,
    candidate: &RouteCandidate,
    observation: ObservationSeed,
    custom_tools: BTreeSet<String>,
    response_state: &Arc<ResponseStateStore>,
    request_body: &Value,
    record_eligible: bool,
) -> Response<Body> {
    if candidate.provider.transport == ProviderTransport::Kiro {
        return kiro_response(upstream, candidate, observation).await;
    }
    // Some Responses-compatible upstreams omit or rewrite the SSE content type even
    // after accepting `stream: true`. Trust the already-validated request intent as
    // well as the response header so an SSE body is not parsed as ordinary JSON.
    let streaming = candidate.provider.capabilities.streaming
        && (observation.streaming
            || upstream
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.contains("text/event-stream")));
    let limit = candidate.provider.limits.max_response_bytes;
    let idle_timeout = Duration::from_millis(candidate.provider.limits.stream_idle_timeout_ms);
    // Only the ChatGPT forward / stateless Responses path strips `previous_response_id`
    // (see `sanitize_responses_upstream_request`), so only there the local continuation
    // cache must record with `force` (Codex sends `store: false` on every request).
    // Requests whose own continuation could not be expanded are excluded as well.
    let force_record = candidate.provider.credential.source == CredentialSource::Forward
        || candidate.provider.stateless_responses;
    let should_record = force_record && record_eligible;
    let state_for_stream = Arc::clone(response_state);
    let body_for_stream = request_body.clone();
    match candidate
        .provider
        .protocol_for_model(&candidate.upstream_model)
    {
        ProviderProtocol::Responses if streaming => {
            if let Some(repair) =
                ResponsesItemIdRepair::new(&candidate.provider.response_item_id_repair)
            {
                repairing_responses_stream(
                    upstream,
                    limit,
                    idle_timeout,
                    observation,
                    repair,
                    state_for_stream,
                    body_for_stream,
                    should_record,
                )
            } else {
                passthrough_stream(
                    upstream,
                    limit,
                    idle_timeout,
                    observation,
                    state_for_stream,
                    body_for_stream,
                    should_record,
                )
            }
        }
        ProviderProtocol::Responses => {
            responses_json_response(
                upstream,
                limit,
                observation,
                ResponsesItemIdRepair::new(&candidate.provider.response_item_id_repair),
                Arc::clone(response_state),
                request_body.clone(),
                should_record,
            )
            .await
        }
        ProviderProtocol::ChatCompletions if streaming => translated_stream_response(
            upstream,
            candidate.exposed_model.clone(),
            StreamAdapter::Chat,
            limit,
            idle_timeout,
            observation,
            false,
            custom_tools,
        ),
        ProviderProtocol::ChatCompletions => {
            chat_json_response(
                upstream,
                &candidate.exposed_model,
                limit,
                observation,
                custom_tools,
            )
            .await
        }
        ProviderProtocol::AnthropicMessages if streaming => translated_stream_response(
            upstream,
            candidate.exposed_model.clone(),
            StreamAdapter::Anthropic { input_tokens: 0 },
            limit,
            idle_timeout,
            observation,
            candidate.provider.escape_builtin_tool_names,
            custom_tools,
        ),
        ProviderProtocol::AnthropicMessages => {
            adapted_json_response(
                upstream,
                &candidate.exposed_model,
                anthropic_to_response,
                limit,
                observation,
                candidate.provider.escape_builtin_tool_names,
                custom_tools,
            )
            .await
        }
        ProviderProtocol::GeminiGenerateContent if streaming => translated_stream_response(
            upstream,
            candidate.exposed_model.clone(),
            StreamAdapter::Gemini,
            limit,
            idle_timeout,
            observation,
            false,
            custom_tools,
        ),
        ProviderProtocol::GeminiGenerateContent => {
            adapted_json_response(
                upstream,
                &candidate.exposed_model,
                gemini_to_response,
                limit,
                observation,
                false,
                custom_tools,
            )
            .await
        }
    }
}

pub(crate) async fn candidate_response_value(
    upstream: reqwest::Response,
    candidate: &RouteCandidate,
    limit: u64,
) -> Result<Value, String> {
    if candidate.provider.transport != ProviderTransport::Kiro {
        return bounded_json(upstream, limit).await;
    }
    let context = upstream
        .extensions()
        .get::<KiroRequestContext>()
        .cloned()
        .ok_or_else(|| "Kiro response lost its request context".to_string())?;
    let bytes = read_bounded(upstream, limit).await?;
    kiro_eventstream_to_response(&bytes, &context, &candidate.exposed_model)
}

pub(crate) async fn kiro_response(
    upstream: reqwest::Response,
    candidate: &RouteCandidate,
    observation: ObservationSeed,
) -> Response<Body> {
    let status = upstream.status();
    let context = upstream.extensions().get::<KiroRequestContext>().cloned();
    let Some(context) = context else {
        observation.finish(
            StatusCode::BAD_GATEWAY,
            Some("invalid_provider_response"),
            TokenUsage::default(),
        );
        return error_response(
            StatusCode::BAD_GATEWAY,
            "invalid_provider_response",
            "Kiro response lost its request context",
        );
    };
    if context.streaming && context.completion_tool.is_none() {
        return kiro_incremental_response(upstream, candidate, observation, context);
    }
    let bytes = match read_bounded(upstream, candidate.provider.limits.max_response_bytes).await {
        Ok(bytes) => bytes,
        Err(message) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("invalid_provider_response"),
                TokenUsage::default(),
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            );
        }
    };
    let value = match kiro_eventstream_to_response(&bytes, &context, &candidate.exposed_model) {
        Ok(value) => value,
        Err(message) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("response_translation_error"),
                TokenUsage::default(),
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            );
        }
    };
    observation.finish(status, None, TokenUsage::from_json(&value));
    if !context.streaming {
        return json_response(status, value);
    }
    let mut encoded = String::new();
    for event in websocket_json_response_events(&value) {
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message");
        encoded.push_str(&sse(event_type, &event));
    }
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from(encoded))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "gateway_error",
                "failed to build Kiro stream response",
            )
        })
}

pub(crate) fn kiro_incremental_response(
    upstream: reqwest::Response,
    candidate: &RouteCandidate,
    observation: ObservationSeed,
    context: KiroRequestContext,
) -> Response<Body> {
    let status = upstream.status();
    let limit = candidate.provider.limits.max_response_bytes;
    let idle_timeout = Duration::from_millis(candidate.provider.limits.stream_idle_timeout_ms);
    let mut source = upstream.bytes_stream();
    let mut decoder = KiroStreamDecoder::new(context, candidate.exposed_model.clone());
    let output = stream! {
        let mut completion = StreamObservation::new(observation);
        let mut usage = TokenUsage::default();
        let mut received = 0_u64;
        let mut failure = None;
        let created = decoder.created_event();
        let created_type = created.get("type").and_then(Value::as_str).unwrap_or("response.created");
        yield Ok::<Bytes, std::io::Error>(Bytes::from(sse(created_type, &created)));
        loop {
            let chunk = match tokio::time::timeout(idle_timeout, source.next()).await {
                Ok(Some(Ok(chunk))) => chunk,
                Ok(Some(Err(_))) => {
                    failure = Some("upstream_stream_error");
                    yield Err(std::io::Error::other("Kiro provider stream failed"));
                    break;
                }
                Ok(None) => break,
                Err(_) => {
                    failure = Some("stream_idle_timeout");
                    yield Err(std::io::Error::other("Kiro provider stream exceeded the idle timeout"));
                    break;
                }
            };
            received = received.saturating_add(chunk.len() as u64);
            if received > limit {
                failure = Some("provider_response_too_large");
                yield Err(std::io::Error::other("Kiro provider stream exceeded the response limit"));
                break;
            }
            match decoder.push(&chunk) {
                Ok(events) => {
                    for event in events {
                        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("message");
                        yield Ok(Bytes::from(sse(event_type, &event)));
                    }
                }
                Err(_) => {
                    failure = Some("response_translation_error");
                    yield Err(std::io::Error::other("Kiro provider stream was invalid"));
                    break;
                }
            }
        }
        if failure.is_none() {
            match decoder.finish() {
                Ok((events, response)) => {
                    usage = TokenUsage::from_json(&response);
                    for event in events {
                        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("message");
                        yield Ok(Bytes::from(sse(event_type, &event)));
                    }
                }
                Err(_) => {
                    failure = Some("response_translation_error");
                    yield Err(std::io::Error::other("Kiro provider stream ended before a valid response"));
                }
            }
        }
        completion.finish(
            if failure.is_some() { StatusCode::BAD_GATEWAY } else { status },
            failure,
            usage,
        );
    };
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(output))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "gateway_error",
                "failed to build incremental Kiro stream response",
            )
        })
}
