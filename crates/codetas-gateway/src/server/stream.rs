use super::*;

pub(crate) fn repairing_responses_stream(
    upstream: reqwest::Response,
    limit: u64,
    idle_timeout: Duration,
    observation: ObservationSeed,
    mut repair: ResponsesItemIdRepair,
    response_state: Arc<ResponseStateStore>,
    request_body: Value,
    force_record: bool,
    snapshot_repair: bool,
) -> Response<Body> {
    let status = upstream.status();
    let mut source = upstream.bytes_stream();
    let output = stream! {
        let mut completion = StreamObservation::new(observation);
        let mut usage = TokenUsage::default();
        let mut pending = Vec::new();
        let mut received = 0_u64;
        let mut failure = None;
        let mut terminal_response = None;
        let mut terminal_recorded = false;
        let mut collected_output = Vec::new();
        loop {
            let item = match tokio::time::timeout(idle_timeout, source.next()).await {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(_) => {
                    failure = Some("stream_idle_timeout");
                    yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                        "provider stream exceeded the idle timeout",
                    ));
                    break;
                }
            };
            match item {
                Ok(bytes) => {
                    received = received.saturating_add(bytes.len() as u64);
                    if received > limit {
                        failure = Some("response_too_large");
                        yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                            "provider response exceeded the configured limit",
                        ));
                        break;
                    }
                    let values = match drain_sse_values(&mut pending, &bytes) {
                        Ok(values) => values,
                        Err(error) => {
                            failure = Some("invalid_provider_stream");
                            yield Err::<Bytes, std::io::Error>(std::io::Error::other(error));
                            break;
                        }
                    };
                    for mut value in values {
                        repair.repair_event(&mut value);
                        usage.merge_max(TokenUsage::from_json(&value));
                        match value.get("type").and_then(Value::as_str) {
                            Some("response.output_item.done") => {
                                if let Some(item) = value.get("item").cloned() {
                                    collected_output.push(item);
                                }
                            }
                            Some(
                                "response.completed" | "response.failed" | "response.incomplete",
                            ) => {
                                if terminal_response.is_none() {
                                    let mut response = value.get("response").cloned();
                                    if let Some(Value::Object(object)) = response.as_mut() {
                                        if snapshot_repair && object
                                            .get("output")
                                            .and_then(Value::as_array)
                                            .is_none_or(Vec::is_empty)
                                        {
                                            object.insert(
                                                "output".into(),
                                                Value::Array(std::mem::take(&mut collected_output)),
                                            );
                                        }
                                    }
                                    terminal_response = response;
                                }
                            }
                            _ => {}
                        }
                        if !terminal_recorded {
                            if let Some(response) = terminal_response.as_ref() {
                                response_state.remember(&request_body, response, force_record);
                                completion.finish(status, None, usage.clone());
                                terminal_recorded = true;
                            }
                        }
                        let event = value
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("message");
                        yield Ok(Bytes::from(sse(event, &value)));
                    }
                }
                Err(error) => {
                    failure = Some("upstream_stream_error");
                    yield Err(std::io::Error::other(error.to_string()));
                    break;
                }
            }
        }
        if !terminal_recorded {
            if let Some(response) = terminal_response {
                crate::debug::log(&format!(
                    "remember stream: id={} status={} force={} out={}",
                    response.get("id").and_then(serde_json::Value::as_str).unwrap_or(""),
                    response.get("status").and_then(serde_json::Value::as_str).unwrap_or(""),
                    force_record,
                    response.get("output").and_then(serde_json::Value::as_array).map(|a| a.len()).unwrap_or(0)
                ));
                response_state.remember(&request_body, &response, force_record);
            }
        }
        if failure.is_none() && !pending.iter().all(u8::is_ascii_whitespace) {
            failure = Some("invalid_provider_stream");
            yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                "provider stream ended with an incomplete SSE frame",
            ));
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
                "failed to build response",
            )
        })
}

pub(crate) fn passthrough_stream(
    upstream: reqwest::Response,
    limit: u64,
    idle_timeout: Duration,
    observation: ObservationSeed,
    response_state: Arc<ResponseStateStore>,
    request_body: Value,
    force_record: bool,
    snapshot_repair: bool,
) -> Response<Body> {
    let status = upstream.status();
    let mut source = upstream.bytes_stream();
    let output = stream! {
        let mut completion = StreamObservation::new(observation);
        let mut usage = TokenUsage::default();
        let mut pending = Vec::new();
        let mut received = 0_u64;
        let mut failure = None;
        let mut terminal_response = None;
        let mut terminal_recorded = false;
        let mut collected_output = Vec::new();
        loop {
            let item = match tokio::time::timeout(idle_timeout, source.next()).await {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(_) => {
                    failure = Some("stream_idle_timeout");
                    yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                        "provider stream exceeded the idle timeout",
                    ));
                    break;
                }
            };
            match item {
                Ok(bytes) => {
                    received = received.saturating_add(bytes.len() as u64);
                    if received > limit {
                        failure = Some("response_too_large");
                        yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                            "provider response exceeded the configured limit",
                        ));
                        break;
                    }
                    let terminal = inspect_sse_usage(
                        &mut pending,
                        &bytes,
                        &mut usage,
                        &mut terminal_response,
                        &mut collected_output,
                        snapshot_repair,
                    );
                    // Codex closes the downstream body as soon as it receives a terminal
                    // Responses event. Record success before yielding that chunk so Drop does
                    // not misclassify a completed turn as a cancelled 502.
                    if terminal {
                        if let Some(response) = terminal_response.as_ref() {
                            response_state.remember(&request_body, response, force_record);
                            terminal_recorded = true;
                        }
                        completion.finish(status, None, usage.clone());
                    }
                    yield Ok(bytes);
                    if terminal {
                        break;
                    }
                }
                Err(error) => {
                    failure = Some("upstream_stream_error");
                    yield Err(std::io::Error::other(error.to_string()));
                    break;
                }
            }
        }
        if !terminal_recorded {
            if let Some(response) = terminal_response {
                crate::debug::log(&format!(
                    "remember stream2: id={} status={} force={} out={}",
                    response.get("id").and_then(serde_json::Value::as_str).unwrap_or(""),
                    response.get("status").and_then(serde_json::Value::as_str).unwrap_or(""),
                    force_record,
                    response.get("output").and_then(serde_json::Value::as_array).map(|a| a.len()).unwrap_or(0)
                ));
                response_state.remember(&request_body, &response, force_record);
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
        // This helper is only used for a validated streaming Responses request.
        // Normalize the downstream type even when the upstream mislabeled its SSE body.
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(output))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "gateway_error",
                "failed to build response",
            )
        })
}

pub(crate) async fn responses_json_response(
    upstream: reqwest::Response,
    limit: u64,
    observation: ObservationSeed,
    mut repair: Option<ResponsesItemIdRepair>,
    response_state: Arc<ResponseStateStore>,
    request_body: Value,
    force_record: bool,
) -> Response<Body> {
    let status = upstream.status();
    let value = match bounded_json(upstream, limit).await {
        Ok(value) => value,
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
    let mut value = value;
    if let Some(repair) = repair.as_mut() {
        repair.repair_response(&mut value);
    }
    crate::debug::log(&format!(
        "remember json: id={} status={} force={} out={}",
        value.get("id").and_then(serde_json::Value::as_str).unwrap_or(""),
        value.get("status").and_then(serde_json::Value::as_str).unwrap_or(""),
        force_record,
        value.get("output").and_then(serde_json::Value::as_array).map(|a| a.len()).unwrap_or(0)
    ));
    response_state.remember(&request_body, &value, force_record);
    observation.finish(status, None, TokenUsage::from_json(&value));
    json_response(status, value)
}

pub(crate) async fn chat_json_response(
    upstream: reqwest::Response,
    exposed_model: &str,
    limit: u64,
    observation: ObservationSeed,
    tool_map: ResponseToolMap,
    response_state: Arc<ResponseStateStore>,
    request_body: Value,
    force_record: bool,
) -> Response<Body> {
    let value = match bounded_json(upstream, limit).await {
        Ok(value) => value,
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
    match chat_to_response(&value, exposed_model, &tool_map) {
        Ok(value) => {
            response_state.remember(&request_body, &value, force_record);
            observation.finish(StatusCode::OK, None, TokenUsage::from_json(&value));
            json_response(StatusCode::OK, value)
        }
        Err(message) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("response_translation_error"),
                TokenUsage::default(),
            );
            error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            )
        }
    }
}

pub(crate) async fn adapted_json_response<F>(
    upstream: reqwest::Response,
    exposed_model: &str,
    adapter: F,
    limit: u64,
    observation: ObservationSeed,
    restore_escaped_anthropic_names: bool,
    tool_map: ResponseToolMap,
    response_state: Arc<ResponseStateStore>,
    request_body: Value,
    force_record: bool,
) -> Response<Body>
where
    F: Fn(&Value, &str, &ResponseToolMap) -> Result<Value, String>,
{
    let mut value = match bounded_json(upstream, limit).await {
        Ok(value) => value,
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
    if restore_escaped_anthropic_names {
        restore_anthropic_tool_names(&mut value);
    }
    match adapter(&value, exposed_model, &tool_map) {
        Ok(value) => {
            response_state.remember(&request_body, &value, force_record);
            observation.finish(StatusCode::OK, None, TokenUsage::from_json(&value));
            json_response(StatusCode::OK, value)
        }
        Err(message) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("response_translation_error"),
                TokenUsage::default(),
            );
            error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            )
        }
    }
}

pub(crate) enum StreamAdapter {
    Chat,
    Anthropic {
        input_tokens: u64,
        subscription_oauth: bool,
    },
    Gemini,
}

pub(crate) fn translated_stream_response(
    upstream: reqwest::Response,
    exposed_model: String,
    mut adapter: StreamAdapter,
    limit: u64,
    idle_timeout: Duration,
    observation: ObservationSeed,
    restore_escaped_anthropic_names: bool,
    tool_map: ResponseToolMap,
    progress_policy: ToolProgressPolicy,
    response_state: Arc<ResponseStateStore>,
    request_body: Value,
    force_record: bool,
    tolerate_incomplete_eof: bool,
) -> Response<Body> {
    let mut upstream_stream = upstream.bytes_stream();
    let output = stream! {
        let mut completion = StreamObservation::new(observation);
        let mut usage = TokenUsage::default();
        let (mut state, initial) =
            ChatStreamState::new_with_progress(exposed_model, tool_map, progress_policy);
        for event in initial {
            yield Ok::<Bytes, Infallible>(Bytes::from(event));
        }
        let mut pending = Vec::new();
        let mut failure = None;
        let mut received = 0_u64;
        let mut response_snapshot_tool_count = 0_usize;
        let mut flushed_tolerated_eof = false;
        'upstream: loop {
            let mut synthetic_eof_chunk = false;
            let chunk = match tokio::time::timeout(idle_timeout, upstream_stream.next()).await {
                Ok(Some(chunk)) => chunk,
                Ok(None) if tolerated_eof_delimiter(
                    &pending,
                    tolerate_incomplete_eof,
                    flushed_tolerated_eof,
                ).is_some() => {
                    flushed_tolerated_eof = true;
                    synthetic_eof_chunk = true;
                    // Some Anthropic-compatible providers omit the final SSE
                    // delimiter. Feed one synthetic delimiter through the
                    // normal strict parser so a complete final frame is
                    // emitted, while truncated JSON still fails.
                    Ok(Bytes::from_static(b"\n\n"))
                }
                Ok(None) => break,
                Err(_) => {
                    yield Ok(Bytes::from(state.fail("provider stream exceeded the idle timeout")));
                    failure = Some("stream_idle_timeout");
                    break;
                }
            };
            match chunk {
                Ok(bytes) => {
                    if !synthetic_eof_chunk {
                        received = received.saturating_add(bytes.len() as u64);
                    }
                    if received > limit {
                        yield Ok(Bytes::from(state.fail("provider stream exceeded the configured limit")));
                        failure = Some("response_too_large");
                        break;
                    }
                    let values = match drain_sse_values(&mut pending, &bytes) {
                        Ok(values) => values,
                        Err(error) => {
                            yield Ok(Bytes::from(state.fail(&format!(
                                "provider returned an invalid SSE frame: {error}"
                            ))));
                            failure = Some("invalid_provider_stream");
                            break 'upstream;
                        }
                    };
                    for mut value in values {
                        if restore_escaped_anthropic_names {
                            restore_anthropic_stream_tool_names(&mut value);
                        }
                        usage.merge_max(TokenUsage::from_json(&value));
                        let converted = match &mut adapter {
                            StreamAdapter::Chat => {
                                if let Some(error) = value.get("error") {
                                    let message = error
                                        .get("message")
                                        .and_then(Value::as_str)
                                        .unwrap_or("provider reported a streaming error")
                                        .to_string();
                                    let context_window_exceeded = serde_json::to_vec(error)
                                        .ok()
                                        .is_some_and(|body| {
                                            upstream_context_window_exceeded(&body)
                                        });
                                    Err((message, context_window_exceeded))
                                } else {
                                    Ok(Some(value))
                                }
                            }
                            StreamAdapter::Anthropic {
                                input_tokens,
                                subscription_oauth,
                            } => {
                                anthropic_stream_to_chat(
                                    &value,
                                    input_tokens,
                                    *subscription_oauth,
                                )
                                .map_err(|message| {
                                    let context_window_exceeded =
                                        context_window_error_message(&message);
                                    (message, context_window_exceeded)
                                })
                            }
                            StreamAdapter::Gemini => gemini_stream_to_chat(&value).map_err(
                                |message| {
                                    let context_window_exceeded =
                                        context_window_error_message(&message);
                                    (message, context_window_exceeded)
                                },
                            ),
                        };
                        match converted {
                            Ok(Some(chunk)) => {
                                usage.merge_max(TokenUsage::from_json(&chunk));
                                let events = state.push_chat_chunk(&chunk);
                                let actionable_tool_call_count =
                                    state.actionable_tool_call_count();
                                if actionable_tool_call_count > response_snapshot_tool_count {
                                    let response = state.completed_response_snapshot();
                                    response_state.remember(
                                        &request_body,
                                        &response,
                                        force_record,
                                    );
                                    response_snapshot_tool_count = actionable_tool_call_count;
                                }
                                for event in events {
                                    if actionable_tool_call_count > 0
                                        && translated_event_is_function_call_delta(&event)
                                    {
                                        completion.succeed_if_cancelled(usage.clone());
                                    }
                                    yield Ok(Bytes::from(event));
                                }
                            }
                            Ok(None) => {}
                            Err((_message, context_window_exceeded)) => {
                                if context_window_exceeded {
                                    yield Ok(Bytes::from(state.fail_with_code(
                                        "context_length_exceeded",
                                        "Your input exceeds the context window of this model.",
                                    )));
                                    failure = Some("context_length_exceeded");
                                } else {
                                    yield Ok(Bytes::from(state.fail(
                                        "The upstream provider reported a streaming error.",
                                    )));
                                    failure = Some("provider_stream_error");
                                }
                                break 'upstream;
                            }
                        }
                    }
                }
                Err(error) => {
                    yield Ok(Bytes::from(state.fail(&format!("upstream stream failed: {error}"))));
                    failure = Some("upstream_stream_error");
                    break;
                }
            }
        }
        if failure.is_none()
            && !pending.iter().all(u8::is_ascii_whitespace)
            && !tolerate_incomplete_eof
        {
            yield Ok(Bytes::from(state.fail("provider stream ended with an incomplete SSE frame")));
            failure = Some("invalid_provider_stream");
        } else if failure.is_none() {
            let (events, response) = state.finish_with_response();
            response_state.remember(&request_body, &response, force_record);
            // Upstream completion and continuation persistence are authoritative. Mark
            // the observation successful before yielding terminal events because Codex
            // may intentionally close the downstream body as soon as it receives them.
            completion.finish(StatusCode::OK, None, usage.clone());
            for event in events {
                yield Ok(Bytes::from(event));
            }
        }
        completion.finish(
            if failure.is_some() { StatusCode::BAD_GATEWAY } else { StatusCode::OK },
            failure,
            usage,
        );
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(output))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "gateway_error",
                "failed to build stream",
            )
        })
}

fn tolerated_eof_delimiter(
    pending: &[u8],
    tolerate_incomplete_eof: bool,
    already_flushed: bool,
) -> Option<&'static [u8]> {
    (tolerate_incomplete_eof
        && !already_flushed
        && !pending.iter().all(u8::is_ascii_whitespace))
    .then_some(b"\n\n")
}

pub(crate) fn translated_event_is_function_call_delta(event: &str) -> bool {
    event
        .lines()
        .any(|line| line.trim() == "event: response.function_call_arguments.delta")
}

pub(crate) struct StreamObservation {
    seed: Option<ObservationSeed>,
    cancelled_outcome: Option<(StatusCode, TokenUsage)>,
}

impl StreamObservation {
    pub(crate) fn new(seed: ObservationSeed) -> Self {
        Self {
            seed: Some(seed),
            cancelled_outcome: None,
        }
    }

    fn succeed_if_cancelled(&mut self, usage: TokenUsage) {
        self.cancelled_outcome = Some((StatusCode::OK, usage));
    }

    pub(crate) fn finish(
        &mut self,
        status: StatusCode,
        failure_category: Option<&str>,
        usage: TokenUsage,
    ) {
        if let Some(seed) = self.seed.take() {
            seed.finish(status, failure_category, usage);
        }
    }
}

impl Drop for StreamObservation {
    fn drop(&mut self) {
        if let Some(seed) = self.seed.take() {
            if let Some((status, usage)) = self.cancelled_outcome.take() {
                seed.finish(status, None, usage);
            } else {
                seed.finish(
                    StatusCode::BAD_GATEWAY,
                    Some("stream_cancelled"),
                    TokenUsage::default(),
                );
            }
        }
    }
}

pub(crate) fn inspect_sse_usage(
    pending: &mut Vec<u8>,
    bytes: &[u8],
    usage: &mut TokenUsage,
    terminal_response: &mut Option<Value>,
    collected_output: &mut Vec<Value>,
    snapshot_repair: bool,
) -> bool {
    match drain_sse_values(pending, bytes) {
        Ok(values) => {
            let mut terminal = false;
            for value in values {
                usage.merge_max(TokenUsage::from_json(&value));
                match value.get("type").and_then(Value::as_str) {
                    Some("response.output_item.done") => {
                        if let Some(item) = value.get("item").cloned() {
                            collected_output.push(item);
                        }
                    }
                    Some("response.completed" | "response.failed" | "response.incomplete") => {
                        terminal = true;
                        if terminal_response.is_none() {
                            let mut response = value.get("response").cloned();
                            if let Some(Value::Object(object)) = response.as_mut() {
                                if snapshot_repair && object
                                    .get("output")
                                    .and_then(Value::as_array)
                                    .is_none_or(Vec::is_empty)
                                {
                                    object.insert(
                                        "output".into(),
                                        Value::Array(std::mem::take(collected_output)),
                                    );
                                }
                            }
                            *terminal_response = response;
                        }
                    }
                    _ => {}
                }
            }
            terminal
        }
        Err(_) => {
            pending.clear();
            false
        }
    }
}

pub(crate) fn drain_sse_values(pending: &mut Vec<u8>, bytes: &[u8]) -> Result<Vec<Value>, String> {
    pending.extend_from_slice(bytes);
    let mut values = Vec::new();
    while let Some((boundary, separator_len)) = sse_frame_boundary(pending) {
        let frame = pending.drain(..boundary).collect::<Vec<_>>();
        pending.drain(..separator_len);
        let frame =
            std::str::from_utf8(&frame).map_err(|_| "SSE frame is not valid UTF-8".to_string())?;
        let normalized = frame.replace("\r\n", "\n");
        let data = normalized
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        values.push(
            serde_json::from_str::<Value>(&data)
                .map_err(|error| format!("invalid JSON data: {error}"))?,
        );
    }
    Ok(values)
}

pub(crate) fn sse_frame_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let lf = bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2));
    let crlf = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4));
    match (lf, crlf) {
        (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
        (Some(boundary), None) | (None, Some(boundary)) => Some(boundary),
        (None, None) => None,
    }
}

#[cfg(test)]
mod eof_tolerance_tests {
    use super::*;

    #[test]
    fn anthropic_eof_tolerance_emits_a_complete_undelimited_final_frame() {
        let mut pending = br#"data: {"type":"message_stop"}"#.to_vec();
        let delimiter = tolerated_eof_delimiter(&pending, true, false)
            .expect("synthetic EOF delimiter");

        let values = drain_sse_values(&mut pending, delimiter).expect("complete final frame");

        assert_eq!(values, vec![json!({"type": "message_stop"})]);
        assert!(pending.is_empty());
    }

    #[test]
    fn anthropic_eof_tolerance_rejects_truncated_final_json() {
        let mut pending = br#"data: {"type":"content_block_delta""#.to_vec();
        let delimiter = tolerated_eof_delimiter(&pending, true, false)
            .expect("synthetic EOF delimiter");

        assert!(drain_sse_values(&mut pending, delimiter).is_err());
    }

    #[test]
    fn ordinary_streams_do_not_synthesize_an_eof_delimiter() {
        let pending = br#"data: {"type":"message_stop"}"#.to_vec();
        assert!(tolerated_eof_delimiter(&pending, false, false).is_none());
        assert!(tolerated_eof_delimiter(&pending, true, true).is_none());
    }
}
