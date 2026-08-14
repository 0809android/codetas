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
                                        if object
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
                    );
                    // Codex closes the downstream body as soon as it receives a terminal
                    // Responses event. Record success before yielding that chunk so Drop does
                    // not misclassify a completed turn as a cancelled 502.
                    if terminal {
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
    custom_tools: BTreeSet<String>,
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
    match chat_to_response(&value, exposed_model, &custom_tools) {
        Ok(value) => {
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

pub(crate) async fn adapted_json_response(
    upstream: reqwest::Response,
    exposed_model: &str,
    adapter: fn(&Value, &str, &BTreeSet<String>) -> Result<Value, String>,
    limit: u64,
    observation: ObservationSeed,
    restore_escaped_anthropic_names: bool,
    custom_tools: BTreeSet<String>,
) -> Response<Body> {
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
    match adapter(&value, exposed_model, &custom_tools) {
        Ok(value) => {
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
    Anthropic { input_tokens: u64 },
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
    custom_tools: BTreeSet<String>,
) -> Response<Body> {
    let mut upstream_stream = upstream.bytes_stream();
    let output = stream! {
        let mut completion = StreamObservation::new(observation);
        let mut usage = TokenUsage::default();
        let (mut state, initial) = ChatStreamState::new(exposed_model, custom_tools);
        for event in initial {
            yield Ok::<Bytes, Infallible>(Bytes::from(event));
        }
        let mut pending = Vec::new();
        let mut failure = None;
        let mut received = 0_u64;
        'upstream: loop {
            let chunk = match tokio::time::timeout(idle_timeout, upstream_stream.next()).await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(_) => {
                    yield Ok(Bytes::from(state.fail("provider stream exceeded the idle timeout")));
                    failure = Some("stream_idle_timeout");
                    break;
                }
            };
            match chunk {
                Ok(bytes) => {
                    received = received.saturating_add(bytes.len() as u64);
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
                                    Err(error.get("message").and_then(Value::as_str).unwrap_or("provider reported a streaming error").to_string())
                                } else {
                                    Ok(Some(value))
                                }
                            }
                            StreamAdapter::Anthropic { input_tokens } => {
                                anthropic_stream_to_chat(&value, input_tokens)
                            }
                            StreamAdapter::Gemini => gemini_stream_to_chat(&value),
                        };
                        match converted {
                            Ok(Some(chunk)) => {
                                usage.merge_max(TokenUsage::from_json(&chunk));
                                let events = state.push_chat_chunk(&chunk);
                                let actionable_function_call =
                                    state.has_actionable_function_call();
                                for event in events {
                                    if actionable_function_call
                                        && translated_event_is_function_call_delta(&event)
                                    {
                                        completion.succeed_if_cancelled(usage.clone());
                                    }
                                    yield Ok(Bytes::from(event));
                                }
                            }
                            Ok(None) => {}
                            Err(message) => {
                                yield Ok(Bytes::from(state.fail(&message)));
                                failure = Some("provider_stream_error");
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
        if failure.is_none() && !pending.iter().all(u8::is_ascii_whitespace) {
            yield Ok(Bytes::from(state.fail("provider stream ended with an incomplete SSE frame")));
            failure = Some("invalid_provider_stream");
        } else if failure.is_none() {
            for event in state.finish() {
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
                                if object
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
