use super::*;

pub(super) struct BufferedProviderResponse {
    thinking_provenance: Option<super::anthropic_compact::ThinkingProvenance>,
    pub(super) cache_observation: crate::observability::CacheObservation,
    request_diagnostic: Option<crate::observability::RequestDiagnostic>,
    status: StatusCode,
    version: axum::http::Version,
    headers: HeaderMap,
    pub(super) bytes: Bytes,
    pub(super) value: Option<Value>,
    usage: TokenUsage,
    retry_observation: Option<ProviderRetryObservation>,
    routing_attempt_lease: Option<RoutingAttemptLease>,
}

impl BufferedProviderResponse {
    pub(super) fn into_response(self) -> Result<reqwest::Response, AttemptFailure> {
        let mut builder = axum::http::Response::builder()
            .status(self.status)
            .version(self.version);
        if let Some(target) = builder.headers_mut() {
            *target = self.headers;
        }
        let mut response = builder
            .body(reqwest::Body::from(self.bytes))
            .map(reqwest::Response::from)
            .map_err(|_| request_failure("gateway_error", "failed to rebuild provider response"))?;
        if let Some(proof) = self.thinking_provenance {
            response.extensions_mut().insert(proof);
        }
        response.extensions_mut().insert(self.cache_observation);
        if let Some(diagnostic) = self.request_diagnostic {
            response.extensions_mut().insert(diagnostic);
        }
        if let Some(observation) = self.retry_observation {
            response.extensions_mut().insert(observation);
        }
        if let Some(lease) = self.routing_attempt_lease {
            response.extensions_mut().insert(lease);
        }
        Ok(response)
    }
}

pub(super) async fn buffer_provider_response(
    mut response: reqwest::Response,
    limit: u64,
) -> Result<BufferedProviderResponse, AttemptFailure> {
    let status = response.status();
    let version = response.version();
    let headers = response.headers().clone();
    let retry_observation = response
        .extensions()
        .get::<ProviderRetryObservation>()
        .cloned();
    let thinking_provenance = response
        .extensions()
        .get::<super::anthropic_compact::ThinkingProvenance>()
        .cloned();
    let existing_cache_observation = response
        .extensions()
        .get::<crate::observability::CacheObservation>()
        .cloned();
    let routing_attempt_lease = take_routing_attempt_lease(&mut response);
    let request_diagnostic = response
        .extensions()
        .get::<crate::observability::RequestDiagnostic>()
        .cloned();
    let bytes = super::stream::read_provider_bytes_with_usage(response, limit)
        .await
        .map_err(|(message, usage)| {
            let mut failure = failure_with_provider_metadata(
                request_failure("invalid_provider_response", &message),
                retry_observation.as_ref(),
                request_diagnostic.as_ref(),
            );
            failure
                .response
                .extensions_mut()
                .insert(crate::observability::ReportedUpstreamUsage(usage));
            failure
        })?;
    let value = serde_json::from_slice::<Value>(&bytes).ok();
    let usage = value
        .as_ref()
        .map(TokenUsage::from_json)
        .unwrap_or_else(|| {
            let mut collector = super::stream::SseUsageCollector::default();
            collector.push(&bytes);
            super::stream::provider_prefix_usage(&bytes, &collector)
        });
    let cache_observation = existing_cache_observation.unwrap_or_else(|| {
        value
            .as_ref()
            .map(crate::observability::CacheObservation::from_provider_json)
            .unwrap_or_default()
    });
    Ok(BufferedProviderResponse {
        thinking_provenance,
        cache_observation,
        request_diagnostic,
        status,
        version,
        headers,
        bytes: Bytes::from(bytes),
        value,
        usage,
        retry_observation,
        routing_attempt_lease,
    })
}

// A Chat stream sends about one event per token, so a long reasoning phase
// outgrew the old 1,024 events / 1 MiB: past the buffer the frames go to the
// client and an empty ending could no longer be retried (a 6,500-token
// reasoning turn on 2026-11-10 ended empty and stopped Codex). 4 MiB per stream
// is about 256 MiB at the 64 in-flight requests the gateway allows.
pub(super) const EMPTY_COMPLETION_MAX_BUFFERED_EVENTS: usize = 16_384;
const EMPTY_COMPLETION_MAX_BUFFERED_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const EMPTY_COMPLETION_RETRY_FAILED_CODE: &str = "empty_completion_retry_failed";
pub(crate) const EMPTY_COMPLETION_RECOVERY_MARKER: &[u8] =
    b": codetas-recovery=empty-completion\n\n";

pub(super) async fn guard_empty_completion_response(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
    streaming: bool,
    response: reqwest::Response,
    cloud_request_id: Option<String>,
    cloud_envelope_cache: Option<Arc<Mutex<Option<Value>>>>,
) -> Result<reqwest::Response, AttemptFailure> {
    if request_is_remote_compaction(body)
        || candidate.provider.transport == ProviderTransport::Kiro
        || !empty_completion_retry_enabled(candidate, streaming, 0)
        || !response.status().is_success()
    {
        return Ok(response);
    }
    if streaming {
        return guarded_provider_sse(
            state.clone(),
            body.clone(),
            candidate.clone(),
            caller_headers.cloned(),
            response,
            cloud_request_id,
            cloud_envelope_cache,
        );
    }

    guard_buffered_empty_completion_response(
        response,
        candidate.provider.limits.max_response_bytes,
        candidate.provider.limits.empty_completion_retries,
        &response_tool_map(body),
        || {
            retry_provider_request(state, candidate, false, || {
                send_candidate_once(
                    state,
                    body,
                    candidate,
                    caller_headers,
                    cloud_request_id.as_deref(),
                    cloud_envelope_cache.as_ref(),
                )
            })
        },
    )
    .await
}

pub(super) async fn guard_buffered_empty_completion_response<F, Fut>(
    response: reqwest::Response,
    limit: u64,
    budget: u8,
    tools: &ResponseToolMap,
    mut operation: F,
) -> Result<reqwest::Response, AttemptFailure>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, AttemptFailure>>,
{
    let mut first = buffer_provider_response(response, limit).await?;
    let mut provider_retries = first.retry_observation.clone();
    let Some(first_value) = first.value.as_ref() else {
        return first.into_response();
    };
    if !completion_is_empty_with_tools(first_value, tools) {
        return first.into_response();
    }
    let mut usage = first.usage.clone();
    // The first provider body has reached EOF and will not be reconstructed;
    // release that attempt before starting an independent empty retry.
    drop(first.routing_attempt_lease.take());
    for retry_index in 0..budget {
        tokio::time::sleep(retry_backoff(retry_index)).await;
        let retry = match operation().await {
            Ok(retry) => retry,
            Err(failure) => {
                merge_provider_retry_observation(
                    &mut provider_retries,
                    failure
                        .response
                        .extensions()
                        .get::<ProviderRetryObservation>()
                        .cloned(),
                );
                let retry_usage = response_body_usage(failure.response).await;
                return empty_completion_retry_failed_response_with_observation(
                    sum_token_usage(usage, retry_usage),
                    u16::from(retry_index) + 1,
                    provider_retries,
                );
            }
        };
        if !retry.status().is_success() {
            merge_provider_retry_observation(
                &mut provider_retries,
                retry
                    .extensions()
                    .get::<ProviderRetryObservation>()
                    .cloned(),
            );
            let retry_usage = response_body_usage_from_reqwest(retry).await;
            return empty_completion_retry_failed_response_with_observation(
                sum_token_usage(usage, retry_usage),
                u16::from(retry_index) + 1,
                provider_retries,
            );
        }
        let retry_observation = retry
            .extensions()
            .get::<ProviderRetryObservation>()
            .cloned();
        let mut retry = match buffer_provider_response(retry, limit).await {
            Ok(retry) => retry,
            Err(failure) => {
                merge_provider_retry_observation(&mut provider_retries, retry_observation);
                let final_usage = failure
                    .response
                    .extensions()
                    .get::<crate::observability::ReportedUpstreamUsage>()
                    .map(|usage| usage.0.clone())
                    .unwrap_or_default();
                return empty_completion_retry_failed_response_with_observation(
                    sum_token_usage(usage, final_usage),
                    u16::from(retry_index) + 1,
                    provider_retries,
                );
            }
        };
        merge_provider_retry_observation(&mut provider_retries, retry.retry_observation.clone());
        usage = sum_token_usage(usage, retry.usage.clone());
        let Some(mut retry_value) = retry.value.take() else {
            return empty_completion_retry_failed_response_with_observation(
                usage,
                u16::from(retry_index) + 1,
                provider_retries,
            );
        };
        if completion_is_empty_with_tools(&retry_value, tools) {
            continue;
        }
        let known_billing = usage.clone();
        write_token_usage(&mut retry_value, usage);
        retry.bytes =
            Bytes::from(serde_json::to_vec(&retry_value).map_err(|_| {
                request_failure("gateway_error", "failed to encode retried response")
            })?);
        retry.headers.remove(header::CONTENT_LENGTH);
        retry.value = Some(retry_value);
        let empty_sends = u16::from(retry_index) + 1;
        let observation = provider_retries.get_or_insert_with(ProviderRetryObservation::default);
        observation.known_billing = Some(known_billing);
        observation.additional_sends = observation.additional_sends.saturating_add(empty_sends);
        observation.recovery_kinds.extend(std::iter::repeat_n(
            "empty-completion".to_string(),
            usize::from(empty_sends),
        ));
        retry.retry_observation = provider_retries;
        let mut response = retry.into_response()?;
        response
            .extensions_mut()
            .insert(EmptyCompletionRecoverySuccess {
                additional_sends: empty_sends,
            });
        return Ok(response);
    }
    empty_completion_retry_failed_response_with_observation(
        usage,
        u16::from(budget),
        provider_retries,
    )
}

#[derive(Default)]
pub(super) struct FinalCacheObservation {
    pub(super) value: crate::observability::CacheObservation,
    cache_creation_input_tokens: u64,
    anthropic: AnthropicStreamState,
}
impl FinalCacheObservation {
    pub(super) fn update(&mut self, protocol: ProviderProtocol, event: &Value) {
        if protocol == ProviderProtocol::AnthropicMessages {
            if let Some(creation) = event
                .pointer("/usage/cache_creation_input_tokens")
                .or_else(|| event.pointer("/message/usage/cache_creation_input_tokens"))
                .and_then(Value::as_u64)
            {
                self.cache_creation_input_tokens = creation;
            }
            let kind = event.get("type").and_then(Value::as_str);
            if kind == Some("message_start") {
                self.value = crate::observability::CacheObservation::from_provider_json(event);
            }
            if matches!(kind, Some("message_start" | "message_delta")) {
                if let Ok(Some(chunk)) = anthropic_stream_to_chat(event, &mut self.anthropic, false)
                {
                    self.value = crate::observability::CacheObservation::from_provider_json(&chunk);
                }
            }
            return;
        }
        let usage = event
            .get("usage")
            .filter(|value| value.is_object())
            .or_else(|| {
                event
                    .pointer("/response/usage")
                    .filter(|value| value.is_object())
            })
            .or_else(|| event.get("usageMetadata").filter(|value| value.is_object()))
            .or_else(|| {
                event
                    .pointer("/response/usageMetadata")
                    .filter(|value| value.is_object())
            });
        if let Some(usage) = usage {
            let next = crate::observability::CacheObservation::from_provider_json(event);
            if ["input_tokens", "prompt_tokens", "promptTokenCount"]
                .iter()
                .any(|key| usage.get(*key).and_then(Value::as_u64).is_some())
            {
                self.value.input_tokens = next.input_tokens;
            }
            if next.cache_read_reported {
                self.value.cached_input_tokens = next.cached_input_tokens;
                self.value.cache_read_reported = true;
            }
        }
    }
}

pub(super) fn guarded_provider_sse(
    state: GatewayState,
    body: Value,
    candidate: RouteCandidate,
    caller_headers: Option<HeaderMap>,
    response: reqwest::Response,
    cloud_request_id: Option<String>,
    cloud_envelope_cache: Option<Arc<Mutex<Option<Value>>>>,
) -> Result<reqwest::Response, AttemptFailure> {
    let thinking_provenance = response
        .extensions()
        .get::<super::anthropic_compact::ThinkingProvenance>()
        .cloned();
    let status = response.status();
    let version = response.version();
    let headers = response.headers().clone();
    let retry_observation = response
        .extensions()
        .get::<ProviderRetryObservation>()
        .cloned();
    let request_diagnostic = response
        .extensions()
        .get::<crate::observability::RequestDiagnostic>()
        .cloned();
    let shared_retry_observation =
        SharedProviderRetryObservation(Arc::new(std::sync::Mutex::new(ProviderRetryObservation {
            thinking_provenance: thinking_provenance.clone(),
            ..Default::default()
        })));
    let stream_retry_observation = shared_retry_observation.clone();
    let limit = candidate.provider.limits.max_response_bytes;
    let protocol = candidate
        .provider
        .protocol_for_model(&candidate.upstream_model);
    let tolerate_anthropic_eof = protocol == ProviderProtocol::AnthropicMessages
        && model_matches_any(
            &candidate.upstream_model,
            &candidate.provider.anthropic_eof_tolerance_models,
        );
    let tools = response_tool_map(&body);
    let output = stream! {
        let mut source = provider_guard_stream_for_protocol(response, tolerate_anthropic_eof, protocol,limit);
        let mut pending = Vec::new();
        let mut held = Vec::<Bytes>::new();
        let mut held_bytes = 0_usize;
        let mut saw_content = false;
        let mut passthrough = false;
        let mut retries = 0_u8;
        let mut prior_usage = TokenUsage::default();
        let mut attempt_usage = TokenUsage::default();
        let mut failure_prefix = None::<Bytes>;
        let mut response_id = None::<String>;
        let mut prior_reasoning_history = Vec::<Value>::new();
        let mut attempt_reasoning_history = Vec::<Value>::new();
        let mut prior_reasoning_injected = false;
        let mut next_response_sequence = 0_u64;
        loop {
            let mut final_cache_observation = FinalCacheObservation::default();
            stream_retry_observation.0.lock().unwrap_or_else(|p| p.into_inner()).cache_observation
                = Some(crate::observability::CacheObservation::default());
            {
                let mut observation=stream_retry_observation.0.lock().unwrap_or_else(|p|p.into_inner());
                observation.terminal_failure=false;observation.known_billing=Some(prior_usage.clone());
            }
            let mut terminal_seen = false;
            let mut chat_finished = false;
            let mut chat_terminal = None::<Value>;
            // A streamed call's name arrives before its arguments, so only the end of
            // the stream shows that a named call really had none.
            let mut chat_named_tool_call = false;
            while let Some(chunk) = source.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        if let Some(failure) = error.get_ref().and_then(|error| error.downcast_ref::<ProviderStreamReadFailure>()) {
                            attempt_usage.merge_max(failure.usage.clone());
                        }
                        {
                            let mut observation = stream_retry_observation.0.lock().unwrap_or_else(|p| p.into_inner());
                            observation.terminal_failure = true;
                            observation.known_billing = Some(sum_token_usage(prior_usage.clone(), attempt_usage.clone()));
                        }
                        if retries > 0 && !saw_content && !passthrough {
                            state.routing.lock().await.record_failure(&candidate);
                            if let Some(prefix) = failure_prefix.take() {
                                yield Ok(prefix);
                            }
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(provider_sse(
                                protocol,
                                provider_error_event_type(protocol),
                                &empty_completion_failed_event_for_protocol(
                                    protocol,
                                    sum_token_usage(
                                        prior_usage.clone(),
                                        attempt_usage.clone(),
                                    ),
                                    response_id.as_deref(),
                                ),
                            )));
                        } else {
                            yield Err(std::io::Error::other(error.to_string()));
                        }
                        return;
                    }
                };
                if chunk.as_ref()==KNOWN_PROVIDER_FAILURE_MARKER {
                    stream_retry_observation.0.lock().unwrap_or_else(|p|p.into_inner()).terminal_failure=true;
                    continue;
                }
                let values = super::stream::drain_sse_results(&mut pending, &chunk, protocol == ProviderProtocol::ChatCompletions);
                if values.is_empty() {yield Ok(Bytes::from_static(b": codetas-upstream-activity\n\n"));}
                for value in values {
                    let mut value = match value {
                        Ok(value) => value,
                    Err(error) => {
                        if retries > 0 && !saw_content && !passthrough {
                            state.routing.lock().await.record_failure(&candidate);
                            if let Some(prefix) = failure_prefix.take() {
                                yield Ok(prefix);
                            }
                            yield Ok(Bytes::from(provider_sse(
                                protocol,
                                provider_error_event_type(protocol),
                                &empty_completion_failed_event_for_protocol(
                                    protocol,
                                    sum_token_usage(
                                        prior_usage.clone(),
                                        attempt_usage.clone(),
                                    ),
                                    response_id.as_deref(),
                                ),
                            )));
                            return;
                        }
                        yield Err(std::io::Error::other(error));
                        return;
                    }
                    };
                    let chat_stream_end = protocol == ProviderProtocol::ChatCompletions
                        && value.get("type").and_then(Value::as_str) == Some("codetas.chat_stream_end");
                    if chat_stream_end && (saw_content || passthrough || !chat_finished) {
                        for buffered in held.drain(..) { yield Ok(buffered); }
                        return;
                    }
                    if chat_stream_end && chat_finished {
                        if let Some(mut terminal) = chat_terminal.take() {
                            if chat_named_tool_call || !provider_stream_event_is_empty_terminal_with_tools(protocol, &terminal, &tools) {
                                for buffered in held.drain(..) { yield Ok(buffered); }
                                write_stream_token_usage(protocol,&mut terminal,sum_token_usage(prior_usage.clone(),attempt_usage.clone()));
                                yield Ok(Bytes::from(provider_sse(protocol,"message",&terminal)));
                                return;
                            }
                        }
                    }
                    let event_type = value
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("message")
                        .to_string();
                    if protocol == ProviderProtocol::Responses {
                        let semantic = responses_reasoning_semantic_history(&value);
                        let terminal_without_items = matches!(
                            event_type.as_str(),
                            "response.completed" | "response.incomplete" | "response.failed"
                        ) && attempt_reasoning_history.is_empty();
                        if event_type == "response.output_item.done" || terminal_without_items {
                            attempt_reasoning_history.extend(semantic);
                        }
                    }
                    final_cache_observation.update(protocol, &value);
                    stream_retry_observation.0.lock().unwrap_or_else(|p| p.into_inner()).cache_observation
                        = Some(final_cache_observation.value.clone());
                    let event_usage = TokenUsage::from_json(&value);
                    attempt_usage.merge_max(event_usage);
                    if protocol==ProviderProtocol::AnthropicMessages {
                        attempt_usage.input_tokens=final_cache_observation.value.input_tokens;
                        attempt_usage.cached_input_tokens=final_cache_observation.value.cached_input_tokens;
                        attempt_usage.cache_read_reported=final_cache_observation.value.cache_read_reported;
                        attempt_usage.cache_creation_input_tokens=final_cache_observation.cache_creation_input_tokens;
                        attempt_usage.total_tokens=attempt_usage.total_tokens.max(attempt_usage.input_tokens.saturating_add(attempt_usage.output_tokens));
                    }
                    stream_retry_observation.0.lock().unwrap_or_else(|p|p.into_inner()).known_billing=Some(sum_token_usage(prior_usage.clone(),attempt_usage.clone()));
                    if retries>0 && ((protocol==ProviderProtocol::AnthropicMessages && value.get("type").and_then(Value::as_str)==Some("message_delta") && value.get("usage").and_then(Value::as_object).is_some_and(|raw|["input_tokens","cache_read_input_tokens","cache_creation_input_tokens"].iter().any(|key|raw.contains_key(*key))))
                        || (protocol==ProviderProtocol::GeminiGenerateContent && (value.get("usageMetadata").is_some()||value.pointer("/response/usageMetadata").is_some()))) {
                        write_stream_token_usage(protocol,&mut value,sum_token_usage(prior_usage.clone(),attempt_usage.clone()));
                    }
                    if retries > 0
                        && protocol == ProviderProtocol::AnthropicMessages
                        && value.get("type").and_then(Value::as_str) == Some("message_start")
                    {
                        write_anthropic_stream_start_usage(
                            &mut value,
                            sum_token_usage(prior_usage.clone(), attempt_usage.clone()),
                        );
                    }
                    if protocol == ProviderProtocol::Responses && retries > 0 {
                        let offset = prior_reasoning_history.len();
                        shift_responses_retry_event(&mut value, offset, &prior_reasoning_history);
                        if event_type != "response.created"
                            && !prior_reasoning_injected
                            && !prior_reasoning_history.is_empty()
                        {
                            for mut event in canonical_reasoning_lifecycle_events(
                                &prior_reasoning_history,
                            ) {
                                assign_responses_sequence(
                                    &mut event,
                                    &mut next_response_sequence,
                                );
                                let encoded = Bytes::from(provider_sse(
                                    protocol,
                                    event.get("type").and_then(Value::as_str).unwrap_or("message"),
                                    &event,
                                ));
                                held_bytes = held_bytes.saturating_add(encoded.len());
                                held.push(encoded);
                            }
                            prior_reasoning_injected = true;
                        }
                    }
                    if protocol == ProviderProtocol::Responses {
                        assign_responses_sequence(&mut value, &mut next_response_sequence);
                    }
                    if protocol == ProviderProtocol::Responses
                        && event_type == "response.created"
                    {
                        response_id = value
                            .pointer("/response/id")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        failure_prefix = Some(Bytes::from(provider_sse(
                            protocol,
                            &event_type,
                            &value,
                        )));
                    }
                    if protocol == ProviderProtocol::ChatCompletions && !chat_stream_end {
                        chat_named_tool_call |= chat_value_names_a_tool_call(&value, &tools);
                        if provider_stream_event_is_terminal(protocol, &value) {
                            chat_finished = true;
                            chat_terminal = Some(value.clone());
                        }
                        // Finish reasons end generation, not the wire stream. Usage is
                        // normally a later frame (including for empty generations). This
                        // only holds frames back for a possible retry: past the buffer
                        // nothing can be taken back, so they go to the client below.
                        if !saw_content && !passthrough && chat_finished
                            && !provider_stream_event_has_visible_content_with_tools(protocol, &value, &tools)
                            && value.get("error").is_none() { continue; }
                    }
                    if saw_content || passthrough {
                        if (provider_stream_event_is_terminal(protocol, &value)
                            || protocol == ProviderProtocol::ChatCompletions && value.get("usage").is_some()
                            || protocol==ProviderProtocol::AnthropicMessages && value.get("type").and_then(Value::as_str)==Some("message_delta") && value.get("usage").and_then(Value::as_object).is_some_and(|raw|["input_tokens","cache_read_input_tokens","cache_creation_input_tokens"].iter().any(|key|raw.contains_key(*key)))) && retries > 0 {
                            let usage = sum_token_usage(
                                prior_usage.clone(),
                                attempt_usage.clone(),
                            );
                            write_stream_token_usage(protocol, &mut value, usage);
                        }
                        terminal_seen = protocol != ProviderProtocol::ChatCompletions && provider_stream_event_is_terminal(protocol, &value);
                        yield Ok(Bytes::from(provider_sse(protocol, &event_type, &value)));
                        if terminal_seen {
                            break;
                        }
                        continue;
                    }
                    if provider_stream_event_has_visible_content_with_tools(protocol, &value, &tools) {
                        let terminal = protocol != ProviderProtocol::ChatCompletions && provider_stream_event_is_terminal(protocol, &value);
                        if terminal && retries > 0 {
                            let usage = sum_token_usage(
                                prior_usage.clone(),
                                attempt_usage.clone(),
                            );
                            write_stream_token_usage(protocol, &mut value, usage);
                        }
                        saw_content = true;
                        failure_prefix = None;
                        for buffered in held.drain(..) {
                            yield Ok(buffered);
                        }
                        held_bytes = 0;
                        yield Ok(Bytes::from(provider_sse(protocol, &event_type, &value)));
                        if terminal {
                            terminal_seen = true;
                            break;
                        }
                        continue;
                    }
                    if provider_stream_event_is_visible_failure(protocol, &value) {
                        if retries > 0 && provider_stream_event_is_error(protocol, &value) {
                            held.clear();
                            state.routing.lock().await.record_failure(&candidate);
                            if let Some(prefix) = failure_prefix.take() {
                                yield Ok(prefix);
                            }
                            yield Ok(Bytes::from(provider_sse(
                                protocol,
                                provider_error_event_type(protocol),
                                &empty_completion_failed_event_for_protocol(
                                    protocol,
                                    sum_token_usage(
                                        prior_usage.clone(),
                                        attempt_usage.clone(),
                                    ),
                                    response_id.as_deref(),
                                ),
                            )));
                            return;
                        }
                        for buffered in held.drain(..) {
                            yield Ok(buffered);
                        }
                        if retries > 0 {
                            let usage = sum_token_usage(
                                prior_usage.clone(),
                                attempt_usage.clone(),
                            );
                            write_stream_token_usage(protocol, &mut value, usage);
                        }
                        yield Ok(Bytes::from(provider_sse(protocol, &event_type, &value)));
                        return;
                    }
                    if (chat_stream_end && chat_finished) || (protocol != ProviderProtocol::ChatCompletions && provider_stream_event_is_empty_terminal_with_tools(protocol, &value, &tools)) {
                        prior_usage = sum_token_usage(
                            prior_usage,
                            attempt_usage.clone(),
                        );
                        attempt_usage = TokenUsage::default();
                        if retries < candidate.provider.limits.empty_completion_retries {
                            prior_reasoning_history.append(&mut attempt_reasoning_history);
                            for item in &mut prior_reasoning_history {
                                if item.get("id").is_none() {
                                    item["id"] =
                                        json!(format!("rs_{}", Uuid::new_v4().simple()));
                                    item["status"] = json!("completed");
                                }
                            }
                            retries += 1;
                            if let Ok(mut observation) = stream_retry_observation.0.lock() {
                                observation.recovery_kinds.push("empty-completion".into());
                            }
                            yield Ok(Bytes::from_static(EMPTY_COMPLETION_RECOVERY_MARKER));
                            // The retry is a distinct upstream response lifecycle. Do not
                            // splice the first attempt's provider IDs/signatures into it.
                            if matches!(protocol, ProviderProtocol::Responses | ProviderProtocol::AnthropicMessages) {
                                held.clear();
                                held_bytes = 0;
                                prior_reasoning_injected = false;
                                next_response_sequence = 0;
                            } else {
                                // The retry is a separate answer here too. A call the
                                // first attempt named without input would stay beside
                                // the retry's calls and fail the whole turn.
                                held.retain(|frame| !held_frame_has_tool_call(frame));
                                held_bytes = held.iter().map(Bytes::len).sum();
                            }
                            drop(std::mem::replace(&mut source,Box::pin(futures_util::stream::empty())));
                            tokio::time::sleep(retry_backoff(retries.saturating_sub(1))).await;
                            stream_retry_observation.0.lock().unwrap_or_else(|p|p.into_inner()).additional_sends+=1;
                            let retry = retry_provider_request(&state, &candidate, true, || {
                                send_candidate_once(
                                    &state,
                                    &body,
                                    &candidate,
                                    caller_headers.as_ref(),
                                    cloud_request_id.as_deref(),
                                    cloud_envelope_cache.as_ref(),
                                )
                            })
                            .await;
                            match retry {
                                Ok(retry) if retry.status().is_success() => {
                                    stream_retry_observation.0.lock().unwrap_or_else(|p| p.into_inner()).thinking_provenance =
                                        retry.extensions().get::<super::anthropic_compact::ThinkingProvenance>().cloned();
                                    stream_retry_observation.0.lock().unwrap_or_else(|p| p.into_inner()).request_diagnostic =
                                        retry.extensions().get::<crate::observability::RequestDiagnostic>().cloned();
                                    if let Some(next) = retry
                                        .extensions()
                                        .get::<ProviderRetryObservation>()
                                        .cloned()
                                    {
                                        if let Ok(mut observation) =
                                            stream_retry_observation.0.lock()
                                        {
                                            let mut merged = Some(observation.clone());
                                            merge_provider_retry_observation(
                                                &mut merged,
                                                Some(next),
                                            );
                                            if let Some(merged) = merged {
                                                *observation = merged;
                                            }
                                        }
                                    }
                                    source = provider_guard_stream_for_protocol(retry, tolerate_anthropic_eof, protocol,limit);
                                    pending.clear();
                                    attempt_reasoning_history.clear();
                                    terminal_seen = true;
                                    break;
                                }
                                Ok(retry) => {
                                    if let Some(next) = retry
                                        .extensions()
                                        .get::<ProviderRetryObservation>()
                                        .cloned()
                                    {
                                        if let Ok(mut observation) =
                                            stream_retry_observation.0.lock()
                                        {
                                            let mut merged = Some(observation.clone());
                                            merge_provider_retry_observation(
                                                &mut merged,
                                                Some(next),
                                            );
                                            if let Some(merged) = merged {
                                                *observation = merged;
                                            }
                                        }
                                    }
                                    let retry_usage = response_body_usage_from_reqwest(retry).await;
                                    prior_usage = sum_token_usage(prior_usage, retry_usage);
                                    held.clear();
                                    state.routing.lock().await.record_failure(&candidate);
                                    if let Some(prefix) = failure_prefix.take() {
                                        yield Ok(prefix);
                                    }
                                    yield Ok(Bytes::from(provider_sse(
                                        protocol,
                                        provider_error_event_type(protocol),
                                        &empty_completion_failed_event_for_protocol(
                                            protocol,
                                            prior_usage.clone(),
                                            response_id.as_deref(),
                                        ),
                                    )));
                                    return;
                                }
                                Err(failure) => {
                                    if let Some(next) = failure
                                        .response
                                        .extensions()
                                        .get::<ProviderRetryObservation>()
                                        .cloned()
                                    {
                                        if let Ok(mut observation) =
                                            stream_retry_observation.0.lock()
                                        {
                                            let mut merged = Some(observation.clone());
                                            merge_provider_retry_observation(
                                                &mut merged,
                                                Some(next),
                                            );
                                            if let Some(merged) = merged {
                                                *observation = merged;
                                            }
                                        }
                                    }
                                    let retry_usage = response_body_usage(failure.response).await;
                                    prior_usage = sum_token_usage(prior_usage, retry_usage);
                                    held.clear();
                                    state.routing.lock().await.record_failure(&candidate);
                                    if let Some(prefix) = failure_prefix.take() {
                                        yield Ok(prefix);
                                    }
                                    yield Ok(Bytes::from(provider_sse(
                                        protocol,
                                        provider_error_event_type(protocol),
                                        &empty_completion_failed_event_for_protocol(
                                            protocol,
                                            sum_token_usage(
                                                prior_usage.clone(),
                                                attempt_usage.clone(),
                                            ),
                                            response_id.as_deref(),
                                        ),
                                    )));
                                    return;
                                }
                            }
                        } else {
                            held.clear();
                            state.routing.lock().await.record_failure(&candidate);
                            if let Some(prefix) = failure_prefix.take() {
                                yield Ok(prefix);
                            }
                            yield Ok(Bytes::from(provider_sse(
                                protocol,
                                provider_error_event_type(protocol),
                                &empty_completion_failed_event_for_protocol(
                                    protocol,
                                    sum_token_usage(
                                        prior_usage.clone(),
                                        attempt_usage.clone(),
                                    ),
                                    response_id.as_deref(),
                                ),
                            )));
                            return;
                        }
                    }
                    let encoded = Bytes::from(provider_sse(protocol, &event_type, &value));
                    if held.len() + 1 > EMPTY_COMPLETION_MAX_BUFFERED_EVENTS
                        || held_bytes.saturating_add(encoded.len())
                            > EMPTY_COMPLETION_MAX_BUFFERED_BYTES
                    {
                        for buffered in held.drain(..) {
                            yield Ok(buffered);
                        }
                        held_bytes = 0;
                        passthrough = true;
                        yield Ok(encoded);
                    } else {
                        held_bytes = held_bytes.saturating_add(encoded.len());
                        held.push(encoded);
                        if provider_stream_event_is_reasoning(protocol, &value) {
                            yield Ok(Bytes::from_static(b": codetas-empty-completion-guard\n\n"));
                        }
                    }
                }
                if terminal_seen {
                    break;
                }
            }
            if terminal_seen && retries > 0 && !saw_content {
                continue;
            }
            if retries > 0 && !saw_content && !passthrough {
                held.clear();
                state.routing.lock().await.record_failure(&candidate);
                if let Some(prefix) = failure_prefix.take() {
                    yield Ok(prefix);
                }
                yield Ok(Bytes::from(provider_sse(
                    protocol,
                    provider_error_event_type(protocol),
                    &empty_completion_failed_event_for_protocol(
                        protocol,
                        sum_token_usage(prior_usage.clone(), attempt_usage.clone()),
                        response_id.as_deref(),
                    ),
                )));
                return;
            }
            for buffered in held.drain(..) {
                yield Ok(buffered);
            }
            return;
        }
    };
    let mut builder = axum::http::Response::builder()
        .status(status)
        .version(version);
    if let Some(target) = builder.headers_mut() {
        *target = headers;
        target.remove(header::CONTENT_LENGTH);
    }
    let mut response = builder
        .body(reqwest::Body::wrap_stream(output))
        .map(reqwest::Response::from)
        .map_err(|_| request_failure("gateway_error", "failed to build guarded provider stream"))?;
    if let Some(observation) = retry_observation {
        response.extensions_mut().insert(observation);
    }
    if let Some(diagnostic) = request_diagnostic {
        response.extensions_mut().insert(diagnostic);
    }
    if let Some(proof) = thinking_provenance {
        response.extensions_mut().insert(proof);
    }
    response.extensions_mut().insert(shared_retry_observation);
    Ok(response)
}

const KNOWN_PROVIDER_FAILURE_MARKER: &[u8] = b": codetas-known-provider-failure\n\n";

pub(super) fn provider_guard_stream_for_protocol(
    mut response: reqwest::Response,
    tolerate_eof: bool,
    protocol: ProviderProtocol,
    limit: u64,
) -> Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send>> {
    let lease = take_routing_attempt_lease(&mut response);
    Box::pin(stream! {
        let mut lease=lease;let mut source=response.bytes_stream();let mut pending=Vec::new();let mut delayed=None::<Value>;let mut received=0_u64;
        let mut collector = super::stream::SseUsageCollector::default();
        while let Some(chunk)=source.next().await {
            let chunk=match chunk {Ok(chunk)=>chunk,Err(_)=>{yield Err(provider_stream_read_failure("provider stream read failed", &collector));return;}};
            let take = chunk.len().min(usize::try_from(limit.saturating_sub(received)).unwrap_or(usize::MAX));
            collector.push(&chunk[..take]);
            received=received.saturating_add(chunk.len() as u64);
            if take < chunk.len() {yield Err(provider_stream_read_failure("provider response exceeded its byte limit", &collector));return;}
            if protocol != ProviderProtocol::GeminiGenerateContent { yield Ok(chunk); continue; }
            let values = super::stream::drain_sse_results(&mut pending, &chunk, false);
            if values.is_empty() {yield Ok(Bytes::from_static(b": codetas-upstream-activity\n\n"));}
            for value in values {
                let mut value = match value { Ok(value) => value, Err(_) => { yield Err(provider_stream_read_failure("invalid provider SSE frame", &collector)); return; } };
                let payload=unwrap_gemini_stream_payload(&value);
                let stopped=provider_stream_event_is_terminal(protocol,&value);
                let failed=provider_stream_event_is_visible_failure(protocol,&value);
                if stopped && failed {
                    yield Ok(Bytes::from_static(KNOWN_PROVIDER_FAILURE_MARKER));
                    let mut terminal=value.clone();let wrapped=value.get("response").is_some_and(Value::is_object);
                    let terminal_payload=if wrapped {&mut terminal["response"]} else {&mut terminal};
                    if let Some(object)=terminal_payload.as_object_mut() {object.remove("usageMetadata");}
                    delayed=Some(terminal);
                    if let Some(metadata)=payload.get("usageMetadata").filter(|v|v.is_object()) {
                        let metadata=if wrapped {json!({"response":{"usageMetadata":metadata}})} else {json!({"usageMetadata":metadata})};
                        yield Ok(Bytes::from(provider_sse(protocol,"message",&metadata)));
                    }
                    continue;
                }
                if stopped {
                    let mut terminal=value.clone();
                    let wrapped=value.get("response").is_some_and(Value::is_object);
                    let current=if wrapped {&mut value["response"]} else {&mut value};
                    if let Some(candidates)=current.get_mut("candidates").and_then(Value::as_array_mut) {for candidate in candidates {if let Some(candidate)=candidate.as_object_mut() {candidate.remove("finishReason");}}}
                    let end=if wrapped {&mut terminal["response"]} else {&mut terminal};
                    if let Some(object)=end.as_object_mut() {object.remove("usageMetadata");}
                    if let Some(candidates)=end.get_mut("candidates").and_then(Value::as_array_mut) {for candidate in candidates {if let Some(candidate)=candidate.as_object_mut() {candidate.remove("content");}}}
                    delayed=Some(terminal);
                }
                yield Ok(Bytes::from(provider_sse(protocol,"message",&value)));
            }
        }
        drop(lease.take());
        if protocol != ProviderProtocol::GeminiGenerateContent {
            if protocol == ProviderProtocol::ChatCompletions { yield Ok(Bytes::from_static(b"data: [DONE]\n\n")); }
            if tolerate_eof { yield Ok(Bytes::from_static(b"\n\n")); }
            return;
        }
        if !pending.iter().all(u8::is_ascii_whitespace) {yield Err(std::io::Error::other("provider stream ended with an incomplete SSE frame"));return;}
        if let Some(terminal)=delayed {yield Ok(Bytes::from(provider_sse(protocol,"message",&terminal)));}
    })
}

#[cfg(test)]
pub(super) fn provider_guard_stream(
    mut response: reqwest::Response,
    tolerate_incomplete_eof: bool,
    chat: bool,
) -> Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Send>> {
    let routing_attempt_lease = take_routing_attempt_lease(&mut response);
    Box::pin(stream! {
        let _routing_attempt_lease = routing_attempt_lease;
        let mut source = response.bytes_stream();
        while let Some(chunk) = source.next().await {
            yield chunk;
        }
        if chat {
            // Also support providers that close after finish/usage without [DONE].
            yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
        }
        if tolerate_incomplete_eof {
            // Anthropic-compatible servers are allowed by the provider policy to omit
            // the final blank SSE delimiter. Feed the same synthetic delimiter used by
            // the translated adapter so the empty-completion guard does not turn a
            // complete final frame into a retry failure.
            yield Ok(Bytes::from_static(b"\n\n"));
        }
    })
}

pub(super) fn unwrap_gemini_stream_payload(value: &Value) -> &Value {
    value
        .get("response")
        .filter(|response| response.is_object())
        .unwrap_or(value)
}
