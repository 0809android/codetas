use super::*;

mod snapshot;
mod sse;

pub(crate) use snapshot::*;
pub(crate) use sse::*;

pub(crate) fn repairing_responses_stream(
    mut upstream: reqwest::Response,
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
    let routing_attempt_lease = take_routing_attempt_lease(&mut upstream);
    let mut source = upstream.bytes_stream();
    let output = stream! {
        let _routing_attempt_lease = routing_attempt_lease;
        let mut completion = StreamObservation::new(observation);
        let mut usage = TokenUsage::default();
        let mut pending = Vec::new();
        let mut received = 0_u64;
        let mut prefix_usage = SseUsageCollector::default();
        let mut failure = None;
        let mut terminal_response = None;
        let mut terminal_recorded = false;
        let mut terminal_seen = false;
        let mut raw_snapshot = ResponsesSnapshotAccumulator::default();
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        'upstream: loop {
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
                    if bytes.as_ref() == EMPTY_COMPLETION_RECOVERY_MARKER {
                        completion.record_recovery("empty-completion");
                        continue;
                    }
                    let take = bytes.len().min(usize::try_from(limit.saturating_sub(received)).unwrap_or(usize::MAX));
                    prefix_usage.push(&bytes[..take]);
                    usage.merge_max(prefix_usage.usage.clone());
                    completion.record_usage(&usage);
                    received = received.saturating_add(bytes.len() as u64);
                    if received > limit {
                        failure = Some("response_too_large");
                        completion.finish(StatusCode::BAD_GATEWAY, failure, usage.clone());
                        yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                            "provider response exceeded the configured limit",
                        ));
                        break;
                    }
                    for value in drain_sse_results(&mut pending, &bytes, false) {
                        let mut value = match value {
                            Ok(value) => value,
                            Err(error) => {
                                failure = Some("invalid_provider_stream");
                                completion.finish(StatusCode::BAD_GATEWAY, failure, usage.clone());
                                yield Err::<Bytes, std::io::Error>(std::io::Error::other(error));
                                break 'upstream;
                            }
                        };
                        let mut injected = Vec::new();
                        let terminal_kind = matches!(
                            value.get("type").and_then(Value::as_str),
                            Some("response.completed" | "response.failed" | "response.incomplete")
                        );
                        terminal_seen |= terminal_kind;
                        let mut replay_response = None;
                        if snapshot_repair {
                            let mut replay_event = value.clone();
                            raw_snapshot.observe(&replay_event);
                            if terminal_kind {
                                raw_snapshot.closing_events_before_terminal(&replay_event);
                                replay_response = raw_snapshot
                                    .repair_terminal_event_with_request(
                                        &mut replay_event,
                                        &request_body,
                                    );
                            }
                            if raw_snapshot.is_tainted() {
                                snapshot.taint();
                            }
                            repair_responses_snapshot_event(&mut value, &request_body);
                            injected = snapshot.injected_events_before(&value);
                            snapshot.observe(&value);
                            injected.extend(snapshot.closing_events_before_terminal(&value));
                        } else {
                            snapshot.observe(&value);
                            if terminal_kind {
                                replay_response = value.get("response").cloned();
                            }
                        }
                        replay_response = replay_response.map(|response| {
                            if snapshot_repair { raw_snapshot.response_for_continuation(response) }
                            else { snapshot.response_for_continuation(response) }
                        });
                        for mut injected_event in injected {
                            repair.repair_event(&mut injected_event);
                            usage.merge_max(TokenUsage::from_json(&injected_event));
                        completion.record_usage(&usage);
                            let event = injected_event
                                .get("type")
                                .and_then(Value::as_str)
                                .unwrap_or("message");
                            yield Ok(Bytes::from(sse(event, &injected_event)));
                        }
                        repair.repair_event(&mut value);
                        let repaired_response = if snapshot_repair {
                            snapshot.repair_terminal_event_with_request(&mut value, &request_body)
                        } else {
                            snapshot.preserve_generic_terminal_event(&mut value)
                        };
                        if snapshot_repair && repaired_response.is_some() {
                            // Reconstruction uses the raw lifecycle collector by design. Repair
                            // the reconstructed terminal once more so raw placeholder/invalid IDs
                            // cannot be reintroduced after the first client-facing repair pass.
                            repair.repair_event(&mut value);
                        }
                        usage.merge_max(TokenUsage::from_json(&value));
                        completion.record_usage(&usage);
                        if let Some(
                                "response.completed" | "response.failed" | "response.incomplete",
                            ) = value.get("type").and_then(Value::as_str) {
                            if terminal_response.is_none() {
                                terminal_response = replay_response;
                            }
                        }
                        if !terminal_recorded {
                            if let Some(response) = terminal_response.as_ref() {
                                response_state.remember(&request_body, response, force_record);
                                completion.record_terminal_diagnostic(response);
                                let failure_category = response_terminal_failure_category(response);
                                completion.finish(
                                    if failure_category.is_some() {
                                        StatusCode::BAD_GATEWAY
                                    } else {
                                        status
                                    },
                                    failure_category,
                                    usage.clone(),
                                );
                                terminal_recorded = true;
                            }
                        }
                        let event = value
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("message");
                        yield Ok(Bytes::from(sse(event, &value)));
                        if terminal_kind { break 'upstream; }
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
        if failure.is_none() && !terminal_seen {
            let (category, message) = if pending.iter().all(u8::is_ascii_whitespace) {
                ("incomplete_provider_stream", "provider stream ended before a terminal event")
            } else {
                ("invalid_provider_stream", "provider stream ended with an incomplete SSE frame")
            };
            failure = Some(category);
            completion.finish(StatusCode::BAD_GATEWAY, failure, usage.clone());
            yield Err::<Bytes, std::io::Error>(std::io::Error::other(message));
        }
        completion.finish(
            if failure.is_some() { StatusCode::BAD_GATEWAY } else { status },
            failure,
            usage,
        );
    };
    let mut response = Response::builder()
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
        });
    if snapshot_repair {
        response
            .extensions_mut()
            .insert(SparseSnapshotRepairApplied);
    }
    response
}

pub(crate) fn passthrough_stream(
    mut upstream: reqwest::Response,
    limit: u64,
    idle_timeout: Duration,
    observation: ObservationSeed,
    response_state: Arc<ResponseStateStore>,
    request_body: Value,
    force_record: bool,
    snapshot_repair: bool,
) -> Response<Body> {
    let status = upstream.status();
    let routing_attempt_lease = take_routing_attempt_lease(&mut upstream);
    let mut source = upstream.bytes_stream();
    let output = stream! {
        let _routing_attempt_lease = routing_attempt_lease;
        let mut completion = StreamObservation::new(observation);
        let mut usage = TokenUsage::default();
        let mut pending = Vec::new();
        let mut received = 0_u64;
        let mut prefix_usage = SseUsageCollector::default();
        let mut failure = None;
        let mut terminal_response = None;
        let mut terminal_recorded = false;
        let mut terminal_seen = false;
        let mut raw_snapshot = ResponsesSnapshotAccumulator::default();
        let mut snapshot = ResponsesSnapshotAccumulator::default();
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
                    if bytes.as_ref() == EMPTY_COMPLETION_RECOVERY_MARKER {
                        completion.record_recovery("empty-completion");
                        continue;
                    }
                    let take = bytes.len().min(usize::try_from(limit.saturating_sub(received)).unwrap_or(usize::MAX));
                    prefix_usage.push(&bytes[..take]);
                    usage.merge_max(prefix_usage.usage.clone());
                    completion.record_usage(&usage);
                    received = received.saturating_add(bytes.len() as u64);
                    if received > limit {
                        failure = Some("response_too_large");
                        completion.finish(StatusCode::BAD_GATEWAY, failure, usage.clone());
                        yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                            "provider response exceeded the configured limit",
                        ));
                        break;
                    }
                    if snapshot_repair {
                        let frames = match drain_snapshot_repair_frames(
                            &mut pending,
                            &bytes,
                            &mut usage,
                            &mut raw_snapshot,
                            &mut snapshot,
                            &request_body,
                        ) {
                            Ok(frames) => frames,
                            Err(error) => {
                                completion.finish(StatusCode::BAD_GATEWAY, Some("invalid_provider_stream"), usage.clone());
                                failure = Some("invalid_provider_stream");
                                yield Err::<Bytes, std::io::Error>(std::io::Error::other(error));
                                break;
                            }
                        };
                        completion.record_usage(&usage);
                        let mut terminal = false;
                        for frame in frames {
                            if frame.terminal {
                                terminal = true;
                                terminal_seen = true;
                                terminal_response = frame.response;
                                if let Some(response) = terminal_response.as_ref() {
                                    response_state.remember(&request_body, response, force_record);
                                    terminal_recorded = true;
                                }
                                if let Some(response) = terminal_response.as_ref() {
                                    completion.record_terminal_diagnostic(response);
                                }
                                let failure_category = terminal_response
                                    .as_ref()
                                    .and_then(response_terminal_failure_category);
                                completion.finish(
                                    if failure_category.is_some() {
                                        StatusCode::BAD_GATEWAY
                                    } else {
                                        status
                                    },
                                    failure_category,
                                    usage.clone(),
                                );
                            }
                            yield Ok(frame.bytes);
                            if terminal {
                                break;
                            }
                        }
                        if terminal {
                            break;
                        }
                        continue;
                    }
                    let terminal = match inspect_sse_usage_checked(
                        &mut pending,
                        &bytes,
                        &mut usage,
                        &mut terminal_response,
                        &mut snapshot,
                    ) {
                        Ok(terminal) => terminal,
                        Err(error) => {
                            failure = Some("invalid_provider_stream");
                            completion.finish(StatusCode::BAD_GATEWAY, failure, usage.clone());
                            yield Err::<Bytes, std::io::Error>(std::io::Error::other(error));
                            break;
                        }
                    };
                    terminal_seen |= terminal;
                    completion.record_usage(&usage);
                    // Codex closes the downstream body as soon as it receives a terminal
                    // Responses event. Record success before yielding that chunk so Drop does
                    // not misclassify a completed turn as a cancelled 502.
                    if terminal {
                        if let Some(response) = terminal_response.as_ref() {
                            response_state.remember(&request_body, response, force_record);
                            terminal_recorded = true;
                        }
                        if let Some(response) = terminal_response.as_ref() {
                            completion.record_terminal_diagnostic(response);
                        }
                        let failure_category = terminal_response
                            .as_ref()
                            .and_then(response_terminal_failure_category);
                        completion.finish(
                            if failure_category.is_some() {
                                StatusCode::BAD_GATEWAY
                            } else {
                                status
                            },
                            failure_category,
                            usage.clone(),
                        );
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
        if failure.is_none() && !terminal_seen {
            failure = Some("incomplete_provider_stream");
            completion.finish(StatusCode::BAD_GATEWAY, failure, usage.clone());
            yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                "provider stream ended before a terminal event",
            ));
        }
        if failure.is_none()
            && snapshot_repair
            && !pending.iter().all(u8::is_ascii_whitespace)
        {
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
    let mut response = Response::builder()
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
        });
    if snapshot_repair {
        response
            .extensions_mut()
            .insert(SparseSnapshotRepairApplied);
    }
    response
}

pub(crate) async fn responses_json_response(
    upstream: reqwest::Response,
    limit: u64,
    observation: ObservationSeed,
    mut repair: Option<ResponsesItemIdRepair>,
    response_state: Arc<ResponseStateStore>,
    request_body: Value,
    force_record: bool,
    snapshot_repair: bool,
) -> Response<Body> {
    let status = upstream.status();
    let value = match bounded_json_with_usage(upstream, limit).await {
        Ok(value) => value,
        Err((message, usage)) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("invalid_provider_response"),
                usage,
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            );
        }
    };
    let (value, replay_value) = prepare_responses_json_for_client_and_replay(
        value,
        &request_body,
        repair.as_mut(),
        snapshot_repair,
    );
    crate::debug::log(&format!(
        "remember json: id={} status={} force={} out={}",
        value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(""),
        value
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(""),
        force_record,
        value
            .get("output")
            .and_then(serde_json::Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0)
    ));
    response_state.remember(&request_body, &replay_value, force_record);
    observation.finish(
        status,
        response_terminal_failure_category(&value),
        TokenUsage::from_json(&value),
    );
    json_response(status, value)
}

fn prepare_responses_json_for_client_and_replay(
    value: Value,
    request_body: &Value,
    repair: Option<&mut ResponsesItemIdRepair>,
    snapshot_repair: bool,
) -> (Value, Value) {
    let mut replay_value = value.clone();
    let mut client_value = value;
    if snapshot_repair {
        // Canonical fields are safe for provider replay, but client-only synthetic
        // item IDs are not: preserve a separate replay clone before ID repair.
        repair_responses_snapshot_json(&mut replay_value, request_body);
        repair_responses_snapshot_json(&mut client_value, request_body);
    }
    if let Some(repair) = repair {
        repair.repair_response(&mut client_value);
    }
    (client_value, replay_value)
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
    block_repeated_readonly_inspect: bool,
) -> Response<Body> {
    let value = match bounded_json_with_usage(upstream, limit).await {
        Ok(value) => value,
        Err((message, usage)) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("invalid_provider_response"),
                usage,
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            );
        }
    };
    let raw_usage = TokenUsage::from_json(&value);
    match chat_to_response(&value, exposed_model, &tool_map) {
        Ok(mut value) => {
            if block_repeated_readonly_inspect {
                block_repeated_readonly_tools(&mut value);
            }
            crate::translate::mark_empty_response_incomplete(&mut value);
            response_state.remember(&request_body, &value, force_record);
            let failure = response_terminal_failure_category(&value);
            observation.finish(StatusCode::OK, failure, TokenUsage::from_json(&value));
            json_response(StatusCode::OK, value)
        }
        Err(message) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("response_translation_error"),
                raw_usage,
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
    let provenance = upstream
        .extensions()
        .get::<super::anthropic_compact::ThinkingProvenance>()
        .cloned();
    let mut value = match bounded_json_with_usage(upstream, limit).await {
        Ok(value) => value,
        Err((message, usage)) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("invalid_provider_response"),
                usage,
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
    let raw_usage = TokenUsage::from_json(&value);
    match adapter(&value, exposed_model, &tool_map) {
        Ok(mut value) => {
            if let Some(proof) = &provenance {
                proof.annotate(&mut value);
            }
            crate::translate::mark_empty_response_incomplete(&mut value);
            response_state.remember(&request_body, &value, force_record);
            observation.finish(
                StatusCode::OK,
                response_terminal_failure_category(&value),
                TokenUsage::from_json(&value),
            );
            json_response(StatusCode::OK, value)
        }
        Err(message) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("response_translation_error"),
                raw_usage,
            );
            error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            )
        }
    }
}

/// Fail closed when Gemini ignores the request-side repeated-read warning and
/// emits another read-only `exec` call. The shared tool remains available for
/// writes, so this check only removes calls classified as read-only.
pub(crate) fn block_repeated_readonly_tools(response: &mut Value) -> bool {
    let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut blocked = false;
    output.retain(|item| {
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            return true;
        };
        let arguments = item
            .get("arguments")
            .or_else(|| item.get("input"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if is_readonly_inspect_tool(name, arguments) {
            blocked = true;
            false
        } else {
            true
        }
    });
    if blocked {
        response["status"] = Value::String("incomplete".into());
        response["incomplete_details"] = json!({"reason": "repeated_tool_loop"});
        crate::debug::log_always("blocked readonly tool response after repeated-inspect guard");
    }
    blocked
}

pub(crate) enum StreamAdapter {
    Chat,
    Anthropic {
        state: AnthropicStreamState,
        subscription_oauth: bool,
    },
    Gemini,
}

/// Everything a failed stream must settle before it yields `response.failed`, because
/// a client may stop reading at that event and the WebSocket turn always does. The
/// mid-stream snapshot, if there is one, can hold calls the client never received, so
/// it must not stay replayable. The id belongs to this stream alone, so without a
/// snapshot there is nothing to find.
fn settle_translated_failure_before_yield(
    response_state: &ResponseStateStore,
    response_id: &str,
    completion: &mut StreamObservation,
    usage: &TokenUsage,
    category: &str,
    event: String,
) -> String {
    response_state.forget(response_id);
    completion.finish(StatusCode::BAD_GATEWAY, Some(category), usage.clone());
    event
}

pub(crate) fn translated_stream_response(
    mut upstream: reqwest::Response,
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
    block_repeated_readonly_inspect: bool,
) -> Response<Body> {
    let provenance = upstream
        .extensions()
        .get::<super::anthropic_compact::ThinkingProvenance>()
        .cloned();
    let provenance_updates = upstream
        .extensions()
        .get::<SharedProviderRetryObservation>()
        .cloned();
    let routing_attempt_lease = take_routing_attempt_lease(&mut upstream);
    let mut upstream_stream = upstream.bytes_stream();
    let output = stream! {
        let _routing_attempt_lease = routing_attempt_lease;
        let mut completion = StreamObservation::new(observation);
        let mut usage = TokenUsage::default();
        let (mut state, initial) = ChatStreamState::new_with_progress_and_guard(
            exposed_model,
            tool_map,
            progress_policy,
            block_repeated_readonly_inspect,
        );
        for event in initial {
            yield Ok::<Bytes, Infallible>(Bytes::from(event));
        }
        let mut pending = Vec::new();
        let mut failure = None;
        let mut upstream_terminal_seen = false;
        let mut received = 0_u64;
        let mut prefix_usage = SseUsageCollector::default();
        let mut response_snapshot_tool_count = 0_usize;
        let state_response_id = state.response_id().to_string();
        let mut gemini_state = GeminiStreamState::default();
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
                    yield Ok(Bytes::from(settle_translated_failure_before_yield(&response_state,&state_response_id,&mut completion,&usage,"stream_idle_timeout",state.fail("provider stream exceeded the idle timeout"))));
                    failure = Some("stream_idle_timeout");
                    break;
                }
            };
            match chunk {
                Ok(bytes) => {
                    if bytes.as_ref() == EMPTY_COMPLETION_RECOVERY_MARKER {
                        completion.record_recovery("empty-completion");
                        continue;
                    }
                    if !synthetic_eof_chunk {
                        let take = bytes.len().min(usize::try_from(limit.saturating_sub(received)).unwrap_or(usize::MAX));
                    prefix_usage.push(&bytes[..take]);
                    usage.merge_max(prefix_usage.usage.clone());
                    completion.record_usage(&usage);
                    received = received.saturating_add(bytes.len() as u64);
                    }
                    if received > limit {
                        yield Ok(Bytes::from(settle_translated_failure_before_yield(&response_state,&state_response_id,&mut completion,&usage,"response_too_large",state.fail("provider stream exceeded the configured limit"))));
                        failure = Some("response_too_large");
                        break;
                    }
                    for value in drain_sse_results(&mut pending, &bytes, false) {
                        let mut value = match value {
                            Ok(value) => value,
                            Err(error) => {
                                yield Ok(Bytes::from(settle_translated_failure_before_yield(&response_state,&state_response_id,&mut completion,&usage,"invalid_provider_stream",state.fail(&format!(
                                    "provider returned an invalid SSE frame: {error}"
                                )))));
                                failure = Some("invalid_provider_stream");
                                break 'upstream;
                            }
                        };
                        upstream_terminal_seen |= match &adapter {
                            StreamAdapter::Chat => provider_stream_event_is_terminal(
                                ProviderProtocol::ChatCompletions,
                                &value,
                            ),
                            StreamAdapter::Anthropic { .. } => provider_stream_event_is_terminal(
                                ProviderProtocol::AnthropicMessages,
                                &value,
                            ),
                            StreamAdapter::Gemini => provider_stream_event_is_terminal(
                                ProviderProtocol::GeminiGenerateContent,
                                &value,
                            ),
                        };
                        if restore_escaped_anthropic_names {
                            restore_anthropic_stream_tool_names(&mut value);
                        }
                        usage.merge_max(TokenUsage::from_json(&value));
                        completion.record_usage(&usage);
                        if value.pointer("/error/code").and_then(Value::as_str)
                            == Some(EMPTY_COMPLETION_RETRY_FAILED_CODE)
                        {
                            let message = value
                                .pointer("/error/message")
                                .and_then(Value::as_str)
                                .unwrap_or("The empty-completion retry failed.");
                            yield Ok(Bytes::from(settle_translated_failure_before_yield(&response_state,&state_response_id,&mut completion,&usage,EMPTY_COMPLETION_RETRY_FAILED_CODE,state.fail_with_code(
                                EMPTY_COMPLETION_RETRY_FAILED_CODE,
                                message,
                            ))));
                            failure = Some(EMPTY_COMPLETION_RETRY_FAILED_CODE);
                            break 'upstream;
                        }
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
                                state: anthropic_state,
                                subscription_oauth,
                            } => {
                                anthropic_stream_to_chat(
                                    &value,
                                    anthropic_state,
                                    *subscription_oauth,
                                )
                                .map_err(|message| {
                                    let context_window_exceeded =
                                        context_window_error_message(&message);
                                    (message, context_window_exceeded)
                                })
                            }
                            StreamAdapter::Gemini => gemini_stream_to_chat(&value, &mut gemini_state)
                                .map_err(|message| {
                                    let context_window_exceeded =
                                        context_window_error_message(&message);
                                    (message, context_window_exceeded)
                                }),
                        };
                        match converted {
                            Ok(Some(chunk)) => {
                                usage.merge_max(TokenUsage::from_json(&chunk));
                        completion.record_usage(&usage);
                                if chunk.get("choices").and_then(Value::as_array).is_some_and(|choices|choices.iter().any(|choice|choice.get("finish_reason").and_then(Value::as_str).is_some_and(|reason|!matches!(reason,"stop"|"tool_calls"|"function_call")))) {
                                    completion.cancelled_failure=Some("incomplete_provider_response");
                                }
                                let events = state.push_chat_chunk(&chunk);
                                let actionable_tool_call_count =
                                    state.actionable_tool_call_count();
                                if actionable_tool_call_count > response_snapshot_tool_count {
                                    let mut response = state.completed_response_snapshot();
                                    if let Some(proof) = super::anthropic_compact::current_thinking_provenance(&provenance, &provenance_updates) { proof.annotate(&mut response); }
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
                                    let event = match super::anthropic_compact::current_thinking_provenance(&provenance, &provenance_updates) {
                    Some(proof) => proof.annotate_sse(event), None => event,
                };
                yield Ok(Bytes::from(event));
                                }
                            }
                            Ok(None) => {}
                            Err((_message, context_window_exceeded)) => {
                                if context_window_exceeded {
                                    yield Ok(Bytes::from(settle_translated_failure_before_yield(&response_state,&state_response_id,&mut completion,&usage,"context_length_exceeded",state.fail_with_code(
                                        "context_length_exceeded",
                                        "Your input exceeds the context window of this model.",
                                    ))));
                                    failure = Some("context_length_exceeded");
                                } else {
                                    yield Ok(Bytes::from(settle_translated_failure_before_yield(&response_state,&state_response_id,&mut completion,&usage,"provider_stream_error",state.fail(
                                        "The upstream provider reported a streaming error.",
                                    ))));
                                    failure = Some("provider_stream_error");
                                }
                                break 'upstream;
                            }
                        }
                    }
                }
                Err(error) => {
                    yield Ok(Bytes::from(settle_translated_failure_before_yield(&response_state,&state_response_id,&mut completion,&usage,"upstream_stream_error",state.fail(&format!("upstream stream failed: {error}")))));
                    failure = Some("upstream_stream_error");
                    break;
                }
            }
        }
        if failure.is_none() && !upstream_terminal_seen {
            yield Ok(Bytes::from(settle_translated_failure_before_yield(&response_state,&state_response_id,&mut completion,&usage,"incomplete_provider_stream",state.fail("provider stream ended before a terminal event"))));
            failure = Some("incomplete_provider_stream");
        } else if failure.is_none()
            && !pending.iter().all(u8::is_ascii_whitespace)
            && !tolerate_incomplete_eof
        {
            yield Ok(Bytes::from(settle_translated_failure_before_yield(&response_state,&state_response_id,&mut completion,&usage,"invalid_provider_stream",state.fail("provider stream ended with an incomplete SSE frame"))));
            failure = Some("invalid_provider_stream");
        } else if failure.is_none() {
            let (events, mut response) = state.finish_with_response();
            if let Some(proof) = super::anthropic_compact::current_thinking_provenance(&provenance, &provenance_updates) { proof.annotate(&mut response); }
            let remembered = response_state.remember(&request_body, &response, force_record);
            if !remembered && response_snapshot_tool_count > 0 {
                // The mid-stream snapshot can contain calls the client never
                // received. Drop it so the next request cannot replay them.
                response_state.forget(&state_response_id);
            }
            let terminal_failure = response_terminal_failure_category(&response);
            // Upstream completion and continuation persistence are authoritative. Mark
            // the observation successful before yielding terminal events because Codex
            // may intentionally close the downstream body as soon as it receives them.
            completion.finish(StatusCode::OK, terminal_failure, usage.clone());
            for event in events {
                let event = match super::anthropic_compact::current_thinking_provenance(&provenance, &provenance_updates) {
                    Some(proof) => proof.annotate_sse(event), None => event,
                };
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

#[cfg(test)]
#[path = "tests/repeated_readonly_guard_tests.rs"]
mod repeated_readonly_guard_tests;

pub(crate) fn tolerated_eof_delimiter(
    pending: &[u8],
    tolerate_incomplete_eof: bool,
    already_flushed: bool,
) -> Option<&'static [u8]> {
    (tolerate_incomplete_eof && !already_flushed && !pending.iter().all(u8::is_ascii_whitespace))
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
    cancelled_usage: TokenUsage,
    cancelled_failure: Option<&'static str>,
}

impl StreamObservation {
    pub(crate) fn new(seed: ObservationSeed) -> Self {
        Self {
            seed: Some(seed),
            cancelled_outcome: None,
            cancelled_usage: TokenUsage::default(),
            cancelled_failure: None,
        }
    }

    pub(crate) fn record_usage(&mut self, usage: &TokenUsage) {
        self.cancelled_usage = usage.clone();
        if let Some((_, recorded)) = self.cancelled_outcome.as_mut() {
            *recorded = usage.clone();
        }
    }
    fn record_recovery(&mut self, kind: &str) {
        if let Some(seed) = self.seed.as_mut() {
            if seed.shared_provider_retries.is_none() {
                seed.send_count = seed.send_count.saturating_add(1);
                seed.recovery_kinds.push(kind.to_string());
                seed.recovery_kind = Some(kind.to_string());
            }
        }
    }

    fn succeed_if_cancelled(&mut self, usage: TokenUsage) {
        self.cancelled_outcome = Some((StatusCode::OK, usage));
    }

    fn record_terminal_diagnostic(&mut self, response: &Value) {
        if let Some(seed) = self.seed.as_mut() {
            if let Some(error) =
                diagnostics::stream_failure_diagnostic(response, seed.request_diagnostic.as_ref())
            {
                seed.upstream_error = Some(error);
            }
        }
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
            if let Some(failure) = self.cancelled_failure {
                seed.finish(
                    StatusCode::BAD_GATEWAY,
                    Some(failure),
                    self.cancelled_usage.clone(),
                );
                return;
            }
            if let Some((status, usage)) = self.cancelled_outcome.take() {
                seed.finish(status, None, usage);
            } else {
                seed.finish(
                    StatusCode::BAD_GATEWAY,
                    Some("stream_cancelled"),
                    self.cancelled_usage.clone(),
                );
            }
        }
    }
}

fn inspect_sse_usage_checked(
    pending: &mut Vec<u8>,
    bytes: &[u8],
    usage: &mut TokenUsage,
    terminal_response: &mut Option<Value>,
    snapshot: &mut ResponsesSnapshotAccumulator,
) -> Result<bool, String> {
    for value in drain_sse_results(pending, bytes, false) {
        let mut value = value?;
        usage.merge_max(TokenUsage::from_json(&value));
        snapshot.observe(&value);
        if matches!(
            value.get("type").and_then(Value::as_str),
            Some("response.completed" | "response.failed" | "response.incomplete")
        ) {
            if terminal_response.is_none() {
                *terminal_response = snapshot
                    .preserve_generic_terminal_event(&mut value)
                    .or_else(|| value.get("response").cloned())
                    .map(|response| snapshot.response_for_continuation(response));
            }
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn response_terminal_failure_category(response: &Value) -> Option<&'static str> {
    if response.pointer("/error/code").and_then(Value::as_str)
        == Some(EMPTY_COMPLETION_RETRY_FAILED_CODE)
    {
        return Some(EMPTY_COMPLETION_RETRY_FAILED_CODE);
    }
    match response.get("status").and_then(Value::as_str) {
        Some("failed") => Some("provider_completion_failed"),
        Some("incomplete") => Some("provider_completion_incomplete"),
        _ => None,
    }
}

#[cfg(test)]
#[path = "tests/eof_tolerance_tests.rs"]
mod eof_tolerance_tests;

#[cfg(test)]
#[path = "tests/snapshot_repair_tests.rs"]
mod snapshot_repair_tests;

#[cfg(test)]
#[path = "tests/terminal_failure_observation_tests.rs"]
mod terminal_failure_observation_tests;

#[cfg(test)]
#[path = "tests/failed_adapter_billing_tests.rs"]
mod failed_adapter_billing_tests;

#[cfg(test)]
#[path = "tests/review11_stream_tests.rs"]
mod review11_stream_tests;

#[cfg(test)]
#[path = "tests/regression12_stream_tests.rs"]
mod regression12_stream_tests;

#[cfg(test)]
#[path = "tests/review16_stream_regressions.rs"]
mod review16_stream_regressions;
