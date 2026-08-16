use super::*;

#[derive(Clone, Copy)]
struct SentImageState {
    omitted_before_send: usize,
    sent_images: usize,
}

#[derive(Clone)]
pub(crate) struct EmptyCompletionRecoveryFailure {
    pub(crate) usage: TokenUsage,
}

#[derive(Clone, Copy)]
pub(crate) struct EmptyCompletionRecoverySuccess;

pub(crate) async fn send_candidate(
    state: &GatewayState,
    body: &mut Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
) -> Result<reqwest::Response, AttemptFailure> {
    let cloud_request_id = (candidate.provider.google_mode == GoogleMode::CloudCodeAssist)
        .then(|| format!("agent-{}", Uuid::new_v4()));
    let cloud_envelope_cache = cloud_request_id
        .as_ref()
        .map(|_| Arc::new(Mutex::new(None::<Value>)));
    let remote_compaction = request_is_remote_compaction(body);
    let protocol = candidate
        .provider
        .protocol_for_model(&candidate.upstream_model);
    let strip_unsupported_images = state.settings.read().await.agents.image_input_mode
        != crate::config::AuxiliaryInputMode::Native;
    apply_provider_request_compatibility(
        body,
        candidate,
        protocol,
        strip_unsupported_images,
    );
    if !remote_compaction {
        let image_report = normalize_translated_image_history(
            body,
            protocol,
            candidate.provider.transport,
            candidate.provider.limits.max_request_bytes,
        )
        .await
        .map_err(|message| request_failure("image_normalization_failed", &message))?;
        if image_report.omitted_images > 0 {
            crate::debug::log(&format!(
                "image history normalized: provider={} model={} omitted={} total={} kept_base64_chars={}",
                candidate.provider.id,
                candidate.upstream_model,
                image_report.omitted_images,
                image_report.inline_images,
                image_report.kept_base64_chars,
            ));
        }
    }
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let first = {
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
    let response = if first.status() == StatusCode::PAYLOAD_TOO_LARGE && !remote_compaction {
        let sent_image_state = first
            .extensions()
            .get::<SentImageState>()
            .copied()
            .unwrap_or(SentImageState {
                omitted_before_send: 0,
                sent_images: 0,
            });
        if sent_image_state.sent_images > 0 && tighten_image_history_after_413(body, sent_image_state)
        {
            let _ = read_bounded(first, 64 * 1024).await;
            if let Some(cache) = cloud_envelope_cache.as_ref() {
                *cache.lock().await = None;
            }
            crate::debug::log(&format!(
                "provider returned 413; retrying once with tighter image history: provider={} model={}",
                candidate.provider.id, candidate.upstream_model,
            ));
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
        } else {
            first
        }
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

fn tighten_image_history_after_413(body: &mut Value, sent: SentImageState) -> bool {
    if sent.sent_images == 0 {
        return false;
    }
    let mut omitted_for_retry = 0_usize;
    for _ in 0..=sent.omitted_before_send {
        if omit_oldest_translated_input_image(body) {
            omitted_for_retry += 1;
        } else {
            break;
        }
    }
    omitted_for_retry > sent.omitted_before_send
}

pub(crate) async fn retry_provider_request<F, Fut>(
    state: &GatewayState,
    candidate: &RouteCandidate,
    streaming: bool,
    mut operation: F,
) -> Result<reqwest::Response, AttemptFailure>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, AttemptFailure>>,
{
    let transport_retries = if streaming {
        candidate.provider.limits.stream_retries
    } else {
        candidate.provider.limits.request_retries
    };
    let mut transport_attempts = 0_u8;
    let mut rate_limit_attempts = 0_u8;
    loop {
        await_provider_pacing(state, candidate).await;
        match operation().await {
            Ok(response) if response.status() == StatusCode::TOO_MANY_REQUESTS => {
                let credential_source = candidate
                    .credential
                    .as_ref()
                    .unwrap_or(&candidate.provider.credential)
                    .source;
                if should_retry_rate_limit(
                    &candidate.provider.limits,
                    credential_source,
                    rate_limit_attempts,
                )
                {
                    let delay = rate_limit_retry_delay(response.headers(), rate_limit_attempts);
                    let _ = read_bounded(response, 64 * 1024).await;
                    tokio::time::sleep(delay).await;
                    rate_limit_attempts = rate_limit_attempts.saturating_add(1);
                    continue;
                }
                return Ok(response);
            }
            Ok(response)
                if response.status() == StatusCode::REQUEST_TIMEOUT
                    || response.status().is_server_error() =>
            {
                if transport_attempts < transport_retries {
                    let delay = validated_retry_after(response.headers())
                        .map(|(_, delay)| delay.unwrap_or(Duration::ZERO))
                        .unwrap_or_else(|| retry_backoff(transport_attempts));
                    let _ = read_bounded(response, 64 * 1024).await;
                    tokio::time::sleep(delay.min(Duration::from_secs(5))).await;
                    transport_attempts = transport_attempts.saturating_add(1);
                    continue;
                }
                return Ok(response);
            }
            Ok(response) => return Ok(response),
            Err(failure)
                if failure.kind == AttemptFailureKind::Retryable
                    && transport_attempts < transport_retries =>
            {
                tokio::time::sleep(retry_backoff(transport_attempts)).await;
                transport_attempts = transport_attempts.saturating_add(1);
            }
            Err(failure) => return Err(failure),
        }
    }
}

async fn await_provider_pacing(state: &GatewayState, candidate: &RouteCandidate) {
    let interval = Duration::from_millis(candidate.provider.limits.request_pacing_ms);
    if interval.is_zero() {
        return;
    }
    let now = Instant::now();
    let scheduled = {
        let mut pacing = state.pacing.lock().await;
        reserve_provider_start(&mut pacing, &candidate.provider.id, interval, now)
    };
    if scheduled > now {
        tokio::time::sleep_until(tokio::time::Instant::from_std(scheduled)).await;
    }
}

pub(crate) fn should_retry_rate_limit(
    limits: &crate::config::ProviderLimits,
    credential_source: CredentialSource,
    attempts: u8,
) -> bool {
    limits.retry_on_429
        && matches!(
            credential_source,
            CredentialSource::Environment
                | CredentialSource::Keychain
                | CredentialSource::Command
        )
        && attempts < limits.max_429_retries
}

pub(crate) fn rate_limit_retry_delay(headers: &HeaderMap, attempts: u8) -> Duration {
    validated_retry_after(headers)
        .map(|(_, delay)| delay.unwrap_or(Duration::ZERO))
        .unwrap_or_else(|| retry_backoff(attempts))
}

pub(crate) fn empty_completion_retry_enabled(
    candidate: &RouteCandidate,
    _streaming: bool,
    attempts: u8,
) -> bool {
    (model_matches_any(
            &candidate.upstream_model,
            &candidate.provider.empty_completion_retry_models,
        ) || model_matches_any(
            &candidate.upstream_model,
            &candidate.provider.terminal_continuation_guard_models,
        ))
        && attempts < candidate.provider.limits.empty_completion_retries
}

pub(crate) fn reserve_provider_start(
    pacing: &mut ProviderPacingState,
    key: &str,
    interval: Duration,
    now: Instant,
) -> Instant {
    if interval.is_zero() {
        return now;
    }
    pacing.reservations = pacing.reservations.saturating_add(1);
    if pacing.reservations % 128 == 0 || pacing.next_slots.len() >= MAX_PROVIDER_PACING_KEYS {
        pacing.next_slots.retain(|_, next| *next > now);
    }
    if !pacing.next_slots.contains_key(key)
        && pacing.next_slots.len() >= MAX_PROVIDER_PACING_KEYS
    {
        if let Some(oldest) = pacing.next_slots.iter()
            .min_by_key(|(_, next)| **next).map(|(key, _)| key.clone())
        {
            pacing.next_slots.remove(&oldest);
        }
    }
    let scheduled = pacing.next_slots.get(key).copied().unwrap_or(now).max(now);
    pacing.next_slots.insert(key.to_string(), scheduled + interval);
    scheduled
}

struct BufferedProviderResponse {
    status: StatusCode,
    version: axum::http::Version,
    headers: HeaderMap,
    bytes: Bytes,
    value: Option<Value>,
}

impl BufferedProviderResponse {
    fn into_response(self) -> Result<reqwest::Response, AttemptFailure> {
        let mut builder = axum::http::Response::builder()
            .status(self.status)
            .version(self.version);
        if let Some(target) = builder.headers_mut() {
            *target = self.headers;
        }
        builder
            .body(reqwest::Body::from(self.bytes))
            .map(reqwest::Response::from)
            .map_err(|_| request_failure("gateway_error", "failed to rebuild provider response"))
    }
}

async fn buffer_provider_response(
    response: reqwest::Response,
    limit: u64,
) -> Result<BufferedProviderResponse, AttemptFailure> {
    let status = response.status();
    let version = response.version();
    let headers = response.headers().clone();
    let bytes = read_bounded(response, limit)
        .await
        .map_err(|message| request_failure("invalid_provider_response", &message))?;
    let value = serde_json::from_slice::<Value>(&bytes).ok();
    Ok(BufferedProviderResponse {
        status,
        version,
        headers,
        bytes,
        value,
    })
}

const EMPTY_COMPLETION_MAX_BUFFERED_EVENTS: usize = 1_024;
const EMPTY_COMPLETION_MAX_BUFFERED_BYTES: usize = 1024 * 1024;
pub(crate) const EMPTY_COMPLETION_RETRY_FAILED_CODE: &str = "empty_completion_retry_failed";
pub(crate) const EMPTY_COMPLETION_RECOVERY_MARKER: &[u8] =
    b": codetas-recovery=empty-completion\n\n";

async fn guard_empty_completion_response(
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

    let limit = candidate.provider.limits.max_response_bytes;
    let first = buffer_provider_response(response, limit).await?;
    let Some(first_value) = first.value.as_ref() else {
        return first.into_response();
    };
    if !completion_is_empty(first_value) {
        return first.into_response();
    }
    let first_usage = TokenUsage::from_json(first_value);
    tokio::time::sleep(retry_backoff(0)).await;
    let retry = match retry_provider_request(state, candidate, false, || {
        send_candidate_once(
            state,
            body,
            candidate,
            caller_headers,
            cloud_request_id.as_deref(),
            cloud_envelope_cache.as_ref(),
        )
    })
    .await {
        Ok(retry) => retry,
        Err(_) => return empty_completion_retry_failed_response(first_usage),
    };
    if !retry.status().is_success() {
        let retry_usage = buffer_provider_response(retry, limit)
            .await
            .ok()
            .and_then(|buffered| buffered.value)
            .as_ref()
            .map(TokenUsage::from_json)
            .unwrap_or_default();
        return empty_completion_retry_failed_response(sum_token_usage(
            first_usage,
            retry_usage,
        ));
    }
    let mut retry = match buffer_provider_response(retry, limit).await {
        Ok(retry) => retry,
        Err(_) => return empty_completion_retry_failed_response(first_usage),
    };
    let Some(mut retry_value) = retry.value.take() else {
        return empty_completion_retry_failed_response(first_usage);
    };
    if completion_is_empty(&retry_value) {
        let usage = sum_token_usage(first_usage, TokenUsage::from_json(&retry_value));
        return empty_completion_retry_failed_response(usage);
    }
    let usage = sum_token_usage(first_usage, TokenUsage::from_json(&retry_value));
    write_token_usage(&mut retry_value, usage);
    retry.bytes = Bytes::from(
        serde_json::to_vec(&retry_value)
            .map_err(|_| request_failure("gateway_error", "failed to encode retried response"))?,
    );
    retry.headers.remove(header::CONTENT_LENGTH);
    retry.value = Some(retry_value);
    let mut response = retry.into_response()?;
    response
        .extensions_mut()
        .insert(EmptyCompletionRecoverySuccess);
    Ok(response)
}

fn guarded_provider_sse(
    state: GatewayState,
    body: Value,
    candidate: RouteCandidate,
    caller_headers: Option<HeaderMap>,
    response: reqwest::Response,
    cloud_request_id: Option<String>,
    cloud_envelope_cache: Option<Arc<Mutex<Option<Value>>>>,
) -> Result<reqwest::Response, AttemptFailure> {
    let status = response.status();
    let version = response.version();
    let headers = response.headers().clone();
    let limit = candidate.provider.limits.max_response_bytes;
    let protocol = candidate
        .provider
        .protocol_for_model(&candidate.upstream_model);
    let tolerate_anthropic_eof = protocol == ProviderProtocol::AnthropicMessages
        && model_matches_any(
            &candidate.upstream_model,
            &candidate.provider.anthropic_eof_tolerance_models,
        );
    let output = stream! {
        let mut source = provider_guard_stream(response, tolerate_anthropic_eof);
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
        let mut received = 0_u64;
        loop {
            let mut terminal_seen = false;
            while let Some(chunk) = source.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
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
                received = received.saturating_add(chunk.len() as u64);
                if received > limit {
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
                                sum_token_usage(prior_usage.clone(), attempt_usage.clone()),
                                response_id.as_deref(),
                            ),
                        )));
                        return;
                    }
                    yield Err(std::io::Error::other(
                        "provider response exceeded the configured limit",
                    ));
                    return;
                }
                let values = match drain_sse_values(&mut pending, &chunk) {
                    Ok(values) => values,
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
                for mut value in values {
                    if protocol == ProviderProtocol::Responses && retries == 0 {
                        if prior_reasoning_history.is_empty()
                            || value.get("type").and_then(Value::as_str)
                                != Some("response.completed")
                        {
                            prior_reasoning_history.extend(
                                responses_reasoning_semantic_history(&value),
                            );
                        }
                    } else if protocol == ProviderProtocol::Responses
                        && retries > 0
                        && !prior_reasoning_history.is_empty()
                    {
                        attach_prior_reasoning_history(
                            &mut value,
                            &prior_reasoning_history,
                        );
                    }
                    let event_usage = TokenUsage::from_json(&value);
                    attempt_usage.merge_max(event_usage);
                    if retries > 0
                        && protocol == ProviderProtocol::AnthropicMessages
                        && value.get("type").and_then(Value::as_str) == Some("message_start")
                    {
                        write_anthropic_stream_start_usage(
                            &mut value,
                            sum_token_usage(prior_usage.clone(), attempt_usage.clone()),
                        );
                    }
                    let event_type = value
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("message")
                        .to_string();
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
                    if saw_content || passthrough {
                        if provider_stream_event_is_terminal(protocol, &value) && retries > 0 {
                            let usage = sum_token_usage(
                                prior_usage.clone(),
                                attempt_usage.clone(),
                            );
                            write_stream_token_usage(protocol, &mut value, usage);
                        }
                        terminal_seen = provider_stream_event_is_terminal(protocol, &value);
                        yield Ok(Bytes::from(provider_sse(protocol, &event_type, &value)));
                        if terminal_seen {
                            break;
                        }
                        continue;
                    }
                    if provider_stream_event_has_visible_content(protocol, &value) {
                        let terminal = provider_stream_event_is_terminal(protocol, &value);
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
                    if provider_stream_event_is_empty_terminal(protocol, &value) {
                        prior_usage = sum_token_usage(
                            prior_usage,
                            attempt_usage.clone(),
                        );
                        attempt_usage = TokenUsage::default();
                        if retries < 1 {
                            retries += 1;
                            yield Ok(Bytes::from_static(EMPTY_COMPLETION_RECOVERY_MARKER));
                            // The retry is a distinct upstream response lifecycle. Do not
                            // splice the first attempt's provider IDs/signatures into it.
                            if protocol == ProviderProtocol::Responses {
                                held.clear();
                                held_bytes = 0;
                            }
                            tokio::time::sleep(retry_backoff(0)).await;
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
                                    source = provider_guard_stream(retry, tolerate_anthropic_eof);
                                    pending.clear();
                                    received = 0;
                                    terminal_seen = true;
                                    break;
                                }
                                _ => {
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
                        continue;
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
    builder
        .body(reqwest::Body::wrap_stream(output))
        .map(reqwest::Response::from)
        .map_err(|_| request_failure("gateway_error", "failed to build guarded provider stream"))
}

fn provider_guard_stream(
    response: reqwest::Response,
    tolerate_incomplete_eof: bool,
) -> Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Send>> {
    let source = response.bytes_stream();
    if tolerate_incomplete_eof {
        // Anthropic-compatible servers are allowed by the provider policy to omit
        // the final blank SSE delimiter. Feed the same synthetic delimiter used by
        // the translated adapter so the empty-completion guard does not turn a
        // complete final frame into a retry failure.
        Box::pin(source.chain(futures_util::stream::once(async {
            Ok(Bytes::from_static(b"\n\n"))
        })))
    } else {
        Box::pin(source)
    }
}

fn provider_stream_event_is_terminal(protocol: ProviderProtocol, value: &Value) -> bool {
    match protocol {
        ProviderProtocol::Responses => matches!(
            value.get("type").and_then(Value::as_str),
            Some("response.completed" | "response.failed" | "response.incomplete")
        ),
        ProviderProtocol::ChatCompletions => value
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(|choices| {
                choices.iter().any(|choice| {
                    choice.get("finish_reason").is_some_and(|reason| !reason.is_null())
                })
            }) || value.get("error").is_some(),
        ProviderProtocol::AnthropicMessages => matches!(
            value.get("type").and_then(Value::as_str),
            Some("message_stop" | "error")
        ) || value.get("type").and_then(Value::as_str) == Some("message_delta")
            && value.pointer("/delta/stop_reason").is_some_and(|reason| !reason.is_null()),
        ProviderProtocol::GeminiGenerateContent => value
            .get("candidates")
            .and_then(Value::as_array)
            .is_some_and(|candidates| {
                candidates.iter().any(|candidate| {
                    candidate.get("finishReason").is_some_and(|reason| !reason.is_null())
                })
            }) || value.get("error").is_some(),
    }
}

fn provider_stream_event_is_visible_failure(protocol: ProviderProtocol, value: &Value) -> bool {
    match protocol {
        ProviderProtocol::Responses => matches!(
            value.get("type").and_then(Value::as_str),
            Some("response.failed" | "response.incomplete")
        ) || value
            .pointer("/response/status")
            .and_then(Value::as_str)
            .is_some_and(|status| matches!(status, "failed" | "incomplete"))
            || value
                .pointer("/response/error")
                .is_some_and(|error| !error.is_null())
            || value
                .pointer("/response/incomplete_details")
                .is_some_and(|details| !details.is_null()),
        ProviderProtocol::ChatCompletions => value.get("error").is_some()
            || value
                .get("choices")
                .and_then(Value::as_array)
                .is_some_and(|choices| {
                    choices.iter().any(|choice| {
                        choice
                            .get("finish_reason")
                            .and_then(Value::as_str)
                            .is_some_and(|reason| {
                                matches!(reason, "length" | "max_tokens" | "content_filter")
                            })
                    })
                }),
        ProviderProtocol::AnthropicMessages => value.get("type").and_then(Value::as_str)
            == Some("error")
            || value
                .pointer("/delta/stop_reason")
                .and_then(Value::as_str)
                .is_some_and(|reason| {
                    matches!(reason, "max_tokens" | "content_filter" | "refusal")
                }),
        ProviderProtocol::GeminiGenerateContent => value.get("error").is_some()
            || value
                .get("candidates")
                .and_then(Value::as_array)
                .is_some_and(|candidates| {
                    candidates.iter().any(|candidate| {
                        candidate
                            .get("finishReason")
                            .and_then(Value::as_str)
                            .is_some_and(|reason| reason != "STOP")
                    })
                }),
    }
}

fn provider_stream_event_is_error(protocol: ProviderProtocol, value: &Value) -> bool {
    match protocol {
        ProviderProtocol::Responses => {
            value.get("type").and_then(Value::as_str) == Some("response.failed")
                || value
                    .pointer("/response/error")
                    .is_some_and(|error| !error.is_null())
        }
        ProviderProtocol::ChatCompletions | ProviderProtocol::GeminiGenerateContent => {
            value.get("error").is_some_and(|error| !error.is_null())
        }
        ProviderProtocol::AnthropicMessages => {
            value.get("type").and_then(Value::as_str) == Some("error")
        }
    }
}

pub(crate) fn provider_stream_event_is_empty_terminal(
    protocol: ProviderProtocol,
    value: &Value,
) -> bool {
    provider_stream_event_is_terminal(protocol, value)
        && !provider_stream_event_is_visible_failure(protocol, value)
        && match protocol {
            ProviderProtocol::Responses => value
                .get("response")
                .is_some_and(|response| !responses_value_has_content(response)),
            ProviderProtocol::ChatCompletions => !chat_value_has_content(value),
            ProviderProtocol::AnthropicMessages => true,
            ProviderProtocol::GeminiGenerateContent => !gemini_value_has_content(value),
        }
}

pub(crate) fn provider_stream_event_has_visible_content(
    protocol: ProviderProtocol,
    value: &Value,
) -> bool {
    match protocol {
        ProviderProtocol::Responses => responses_event_has_visible_content(value),
        ProviderProtocol::ChatCompletions => chat_value_has_content(value),
        ProviderProtocol::AnthropicMessages => match value.get("type").and_then(Value::as_str) {
            Some("content_block_start") => value
                .pointer("/content_block/type")
                .and_then(Value::as_str)
                == Some("tool_use"),
            Some("content_block_delta") => {
                value
                    .pointer("/delta/type")
                    .and_then(Value::as_str)
                    == Some("input_json_delta")
                    || value
                        .pointer("/delta/text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty())
            }
            _ => false,
        },
        ProviderProtocol::GeminiGenerateContent => gemini_value_has_content(value),
    }
}

fn provider_stream_event_is_reasoning(protocol: ProviderProtocol, value: &Value) -> bool {
    match protocol {
        ProviderProtocol::Responses => value
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| {
                kind.contains("reasoning")
                    || matches!(kind, "response.output_item.added" | "response.output_item.done")
                        && value
                            .pointer("/item/type")
                            .and_then(Value::as_str)
                            == Some("reasoning")
            }),
        ProviderProtocol::AnthropicMessages => value
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| {
                kind == "content_block_start"
                    && matches!(
                        value.pointer("/content_block/type").and_then(Value::as_str),
                        Some("thinking" | "redacted_thinking")
                    )
                    || kind == "content_block_delta"
                        && matches!(
                            value.pointer("/delta/type").and_then(Value::as_str),
                            Some("thinking_delta" | "signature_delta")
                        )
            }),
        ProviderProtocol::GeminiGenerateContent => value
            .get("candidates")
            .and_then(Value::as_array)
            .is_some_and(|candidates| {
                candidates.iter().any(|candidate| {
                    candidate
                        .pointer("/content/parts")
                        .and_then(Value::as_array)
                        .is_some_and(|parts| {
                            parts.iter().any(|part| {
                                part.get("thought").and_then(Value::as_bool) == Some(true)
                                    || part.get("thoughtSignature").is_some()
                            })
                        })
                })
            }),
        ProviderProtocol::ChatCompletions => value
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(|choices| {
                choices.iter().any(|choice| {
                    choice
                        .get("delta")
                        .or_else(|| choice.get("message"))
                        .is_some_and(|message| {
                            message.get("reasoning_content").is_some()
                                || message.get("reasoning").is_some()
                        })
                })
            }),
    }
}

fn responses_event_has_visible_content(value: &Value) -> bool {
    match value.get("type").and_then(Value::as_str) {
        Some("response.completed") => value
            .get("response")
            .is_some_and(responses_value_has_content),
        Some("response.output_text.delta" | "response.output_text.done") => value
            .get("delta")
            .or_else(|| value.get("text"))
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty()),
        Some(
            "response.function_call_arguments.delta"
            | "response.function_call_arguments.done"
            | "response.custom_tool_call_input.delta"
            | "response.custom_tool_call_input.done"
            | "response.tool_search_call_arguments.delta"
            | "response.tool_search_call_arguments.done"
            | "response.local_shell_call.in_progress"
            | "response.local_shell_call.completed"
            | "response.web_search_call.in_progress"
            | "response.web_search_call.searching"
            | "response.web_search_call.completed",
        ) => true,
        Some("response.output_item.added" | "response.output_item.done") => value
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|kind| {
                matches!(
                    kind,
                    "function_call"
                        | "custom_tool_call"
                        | "tool_search_call"
                        | "web_search_call"
                        | "local_shell_call"
                )
            }),
        _ => false,
    }
}

fn responses_reasoning_semantic_history(value: &Value) -> Vec<Value> {
    let mut history = Vec::new();
    match value.get("type").and_then(Value::as_str) {
        Some("response.output_item.done")
            if value.pointer("/item/type").and_then(Value::as_str) == Some("reasoning") =>
        {
            if let Some(mut item) = value.get("item").cloned() {
                if let Some(object) = item.as_object_mut() {
                    object.remove("id");
                    object.remove("status");
                }
                history.push(item);
            }
        }
        Some(kind) if kind.contains("reasoning") => {
            let mut semantic = json!({"type": kind});
            for field in ["delta", "text", "signature", "encrypted_content"] {
                if let Some(payload) = value.get(field) {
                    semantic[field] = payload.clone();
                }
            }
            history.push(semantic);
        }
        Some("response.completed") => {
            if let Some(output) = value.pointer("/response/output").and_then(Value::as_array) {
                for item in output.iter().filter(|item| {
                    item.get("type").and_then(Value::as_str) == Some("reasoning")
                }) {
                    let mut item = item.clone();
                    if let Some(object) = item.as_object_mut() {
                        object.remove("id");
                        object.remove("status");
                    }
                    history.push(item);
                }
            }
        }
        _ => {}
    }
    history
}

fn attach_prior_reasoning_history(value: &mut Value, history: &[Value]) {
    let item = match value.get("type").and_then(Value::as_str) {
        Some("response.output_item.added" | "response.output_item.done") => {
            value.get_mut("item")
        }
        Some("response.completed" | "response.incomplete" | "response.failed") => value
            .pointer_mut("/response/output")
            .and_then(Value::as_array_mut)
            .and_then(|output| output.first_mut()),
        _ => None,
    };
    let Some(item) = item else {
        return;
    };
    if item.get("type").and_then(Value::as_str) == Some("reasoning") {
        return;
    }
    item["provider_metadata"]["codetas"]["empty_completion_prior_reasoning"] =
        Value::Array(history.to_vec());
}

fn chat_value_has_content(value: &Value) -> bool {
    value
        .get("choices")
        .and_then(Value::as_array)
        .is_some_and(|choices| {
            choices.iter().any(|choice| {
                choice
                    .get("delta")
                    .or_else(|| choice.get("message"))
                    .is_some_and(|message| {
                        message
                            .get("content")
                            .and_then(Value::as_str)
                            .is_some_and(|text| !text.is_empty())
                            || message
                                .get("tool_calls")
                                .and_then(Value::as_array)
                                .is_some_and(|calls| !calls.is_empty())
                    })
            })
        })
}

fn gemini_value_has_content(value: &Value) -> bool {
    value
        .get("candidates")
        .and_then(Value::as_array)
        .is_some_and(|candidates| {
            candidates.iter().any(|candidate| {
                candidate
                    .pointer("/content/parts")
                    .and_then(Value::as_array)
                    .is_some_and(|parts| {
                        parts.iter().any(|part| {
                            part.get("thought").and_then(Value::as_bool) != Some(true)
                                && part.get("text")
                                .and_then(Value::as_str)
                                .is_some_and(|text| !text.is_empty())
                                || part.get("functionCall").is_some_and(Value::is_object)
                                || part.get("function_call").is_some_and(Value::is_object)
                        })
                    })
            })
        })
}

fn provider_sse(protocol: ProviderProtocol, event_type: &str, value: &Value) -> String {
    match protocol {
        ProviderProtocol::Responses | ProviderProtocol::AnthropicMessages => {
            sse(event_type, value)
        }
        ProviderProtocol::ChatCompletions | ProviderProtocol::GeminiGenerateContent => format!(
            "data: {}\n\n",
            serde_json::to_string(value).unwrap_or_else(|_| "{}".into())
        ),
    }
}

fn provider_error_event_type(protocol: ProviderProtocol) -> &'static str {
    match protocol {
        ProviderProtocol::Responses => "response.failed",
        ProviderProtocol::AnthropicMessages => "error",
        ProviderProtocol::ChatCompletions | ProviderProtocol::GeminiGenerateContent => "message",
    }
}

fn empty_completion_failed_event_for_protocol(
    protocol: ProviderProtocol,
    usage: TokenUsage,
    response_id: Option<&str>,
) -> Value {
    let message = "The model returned an empty completion and the retry failed or was empty again.";
    match protocol {
        ProviderProtocol::Responses => json!({
            "type": "response.failed",
            "response": {
                "id": response_id
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("resp_{}", Uuid::new_v4().simple())),
                "status": "failed",
                "output": [],
                "error": {
                    "type": "upstream_error",
                    "code": EMPTY_COMPLETION_RETRY_FAILED_CODE,
                    "message": message
                },
                "usage": token_usage_json(&usage)
            }
        }),
        ProviderProtocol::ChatCompletions | ProviderProtocol::GeminiGenerateContent => json!({
            "error": {
                "type": "upstream_error",
                "code": EMPTY_COMPLETION_RETRY_FAILED_CODE,
                "message": message
            },
            "usage": token_usage_json(&usage)
        }),
        ProviderProtocol::AnthropicMessages => json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "code": EMPTY_COMPLETION_RETRY_FAILED_CODE,
                "message": message
            },
            "usage": token_usage_json(&usage)
        }),
    }
}

fn empty_completion_retry_failed_response(
    usage: TokenUsage,
) -> Result<reqwest::Response, AttemptFailure> {
    let value = json!({
        "error": {
            "type": "upstream_error",
            "code": EMPTY_COMPLETION_RETRY_FAILED_CODE,
            "message": "The model returned an empty completion and the retry failed or was empty again."
        },
        "usage": token_usage_json(&usage)
    });
    let mut response = axum::http::Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .header(header::CONTENT_TYPE, "application/json")
        .body(reqwest::Body::from(value.to_string()))
        .map(reqwest::Response::from)
        .map_err(|_| request_failure("gateway_error", "failed to build empty completion error"))?;
    response
        .extensions_mut()
        .insert(EmptyCompletionRecoveryFailure { usage });
    Ok(response)
}

fn sum_token_usage(left: TokenUsage, right: TokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: left.input_tokens.saturating_add(right.input_tokens),
        output_tokens: left.output_tokens.saturating_add(right.output_tokens),
        cached_input_tokens: left
            .cached_input_tokens
            .saturating_add(right.cached_input_tokens),
        reasoning_tokens: left.reasoning_tokens.saturating_add(right.reasoning_tokens),
        total_tokens: left.total_tokens.saturating_add(right.total_tokens),
    }
}

fn token_usage_json(usage: &TokenUsage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "total_tokens": usage.total_tokens.max(
            usage.input_tokens.saturating_add(usage.output_tokens)
        ),
        "input_tokens_details": {"cached_tokens": usage.cached_input_tokens},
        "output_tokens_details": {"reasoning_tokens": usage.reasoning_tokens}
    })
}

fn write_token_usage(value: &mut Value, usage: TokenUsage) {
    let canonical = token_usage_json(&usage);
    if value.get("type").and_then(Value::as_str).is_some_and(|kind| {
        matches!(kind, "response.completed" | "response.failed" | "response.incomplete")
    }) {
        value["response"]["usage"] = canonical;
    } else if value.get("candidates").is_some() || value.get("usageMetadata").is_some() {
        value["usageMetadata"] = json!({
            "promptTokenCount": usage.input_tokens,
            "candidatesTokenCount": usage.output_tokens,
            "thoughtsTokenCount": usage.reasoning_tokens,
            "cachedContentTokenCount": usage.cached_input_tokens,
            "totalTokenCount": usage.total_tokens.max(
                usage.input_tokens.saturating_add(usage.output_tokens)
            )
        });
    } else if value.get("choices").is_some() {
        value["usage"] = json!({
            "prompt_tokens": usage.input_tokens,
            "completion_tokens": usage.output_tokens,
            "total_tokens": usage.total_tokens.max(
                usage.input_tokens.saturating_add(usage.output_tokens)
            ),
            "prompt_tokens_details": {"cached_tokens": usage.cached_input_tokens},
            "completion_tokens_details": {"reasoning_tokens": usage.reasoning_tokens}
        });
    } else {
        value["usage"] = canonical;
    }
}

fn write_anthropic_stream_start_usage(value: &mut Value, usage: TokenUsage) {
    value["message"]["usage"]["input_tokens"] = json!(usage.input_tokens);
    value["message"]["usage"]["cache_read_input_tokens"] =
        json!(usage.cached_input_tokens);
}

fn write_stream_token_usage(
    protocol: ProviderProtocol,
    value: &mut Value,
    usage: TokenUsage,
) {
    if protocol == ProviderProtocol::AnthropicMessages
        && value.get("type").and_then(Value::as_str) == Some("message_delta")
    {
        value["usage"]["output_tokens"] = json!(usage.output_tokens);
        return;
    }
    write_token_usage(value, usage);
}

pub(crate) fn completion_is_empty(value: &Value) -> bool {
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return false;
    }
    if value
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| matches!(status, "failed" | "incomplete"))
        || value
            .get("incomplete_details")
            .is_some_and(|details| !details.is_null())
    {
        return false;
    }
    if value.get("output").is_some_and(Value::is_array)
        || value.get("status").and_then(Value::as_str) == Some("completed")
    {
        return !responses_value_has_content(value);
    }
    if value.get("stop_reason").and_then(Value::as_str) == Some("end_turn") {
        return !value
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|content| {
                content.iter().any(|part| {
                    part.get("type").and_then(Value::as_str) == Some("text")
                        && part
                            .get("text")
                            .and_then(Value::as_str)
                            .is_some_and(|text| !text.is_empty())
                        || part.get("type").and_then(Value::as_str) == Some("tool_use")
                })
            });
    }
    if let Some(choices) = value.get("choices").and_then(Value::as_array) {
        return choices.is_empty()
            || choices.iter().all(|choice| {
                if choice
                    .get("finish_reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| {
                        matches!(reason, "length" | "max_tokens" | "content_filter")
                    })
                {
                    return false;
                }
                let message = choice.get("message").or_else(|| choice.get("delta"));
                message.is_none_or(|message| {
                    message
                        .get("content")
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
                        && message
                            .get("tool_calls")
                            .and_then(Value::as_array)
                            .is_none_or(Vec::is_empty)
                })
            });
    }
    if let Some(content) = value.get("content").and_then(Value::as_array) {
        if value
            .get("stop_reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| {
                matches!(reason, "max_tokens" | "content_filter" | "refusal")
            })
        {
            return false;
        }
        return !content.iter().any(|part| match part.get("type").and_then(Value::as_str) {
            Some("text") => part
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.is_empty()),
            Some("tool_use") => true,
            _ => false,
        });
    }
    if let Some(candidates) = value.get("candidates").and_then(Value::as_array) {
        if candidates.iter().any(|candidate| {
            candidate
                .get("finishReason")
                .and_then(Value::as_str)
                .is_some_and(|reason| reason != "STOP")
        }) || value.get("promptFeedback").is_some_and(|feedback| {
            feedback.get("blockReason").is_some_and(|reason| !reason.is_null())
        }) {
            return false;
        }
        return !candidates.iter().any(|candidate| {
            candidate
                .pointer("/content/parts")
                .and_then(Value::as_array)
                .is_some_and(|parts| {
                    parts.iter().any(|part| {
                        part.get("thought").and_then(Value::as_bool) != Some(true)
                            && part.get("text")
                                .and_then(Value::as_str)
                                .is_some_and(|text| !text.is_empty())
                            || part.get("functionCall").is_some_and(Value::is_object)
                            || part.get("function_call").is_some_and(Value::is_object)
                    })
                })
        });
    }
    false
}

fn responses_value_has_content(value: &Value) -> bool {
    value
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|output| {
            output.iter().any(|item| match item.get("type").and_then(Value::as_str) {
                Some("message" | "agent_message") => item
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|parts| {
                        parts.iter().any(|part| {
                            matches!(
                                part.get("type").and_then(Value::as_str),
                                Some("output_text" | "text")
                            ) && part
                                .get("text")
                                .and_then(Value::as_str)
                                .is_some_and(|text| !text.is_empty())
                        })
                    }),
                Some(
                    "function_call"
                    | "custom_tool_call"
                    | "tool_search_call"
                    | "web_search_call"
                    | "local_shell_call",
                ) => true,
                _ => false,
            })
        })
}

pub(crate) fn retry_backoff(attempt: u8) -> Duration {
    Duration::from_millis(250_u64.saturating_mul(1_u64 << attempt.min(4)))
}

pub(crate) async fn send_candidate_once(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
    cloud_request_id: Option<&str>,
    cloud_envelope_cache: Option<&Arc<Mutex<Option<Value>>>>,
) -> Result<reqwest::Response, AttemptFailure> {
    let remote_compaction = request_is_remote_compaction(body);
    let source_image_count = count_translated_input_images(body);
    let protocol = candidate
        .provider
        .protocol_for_model(&candidate.upstream_model);
    let wire_model = wire_model_for_request(candidate, body);
    if candidate.provider.transport == ProviderTransport::Kiro {
        return send_kiro_candidate(
            state,
            body,
            candidate,
            caller_headers,
            &wire_model,
        )
        .await;
    }
    if candidate.provider.transport == ProviderTransport::GithubCopilot {
        return send_github_copilot_candidate(
            state,
            body,
            candidate,
            caller_headers,
            &wire_model,
        )
        .await;
    }
    let mut upstream_body = match protocol {
        ProviderProtocol::Responses => body.clone(),
        ProviderProtocol::ChatCompletions => match responses_to_chat_with_options(
            &chat_compatible_request(body, candidate.capabilities.provider_metadata),
            &wire_model,
            model_requires_reasoning_placeholder(
                &candidate.provider,
                &candidate.upstream_model,
            ),
        ) {
            Ok(value) => value,
            Err(message) => return Err(request_failure("unsupported_request", &message)),
        },
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
        ProviderProtocol::GeminiGenerateContent => {
            match responses_to_gemini(body, &wire_model) {
                Ok(value) => value,
                Err(message) => return Err(request_failure("unsupported_request", &message)),
            }
        }
    };
    if !remote_compaction {
        if let Err(message) =
            apply_provider_wire_compatibility(&mut upstream_body, body, candidate, protocol)
        {
            return Err(request_failure("unsupported_request", &message));
        }
    }
    let antigravity_project = if protocol == ProviderProtocol::GeminiGenerateContent
        && candidate.provider.google_mode == GoogleMode::CloudCodeAssist
        && candidate.provider.project.is_none()
        && candidate
            .credential
            .as_ref()
            .unwrap_or(&candidate.provider.credential)
            .source
            == CredentialSource::OAuth
        && candidate
            .credential
            .as_ref()
            .unwrap_or(&candidate.provider.credential)
            .reference
            .as_deref()
            == Some("google-antigravity")
    {
        let token = resolve_oauth_access_token("google-antigravity")
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
    let translation_image_omissions =
        source_image_count.saturating_sub(count_translated_input_images(&upstream_body));

    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut serialized = serde_json::to_vec(&upstream_body).map_err(|error| {
        request_failure(
            "invalid_request",
            &format!("request cannot be encoded: {error}"),
        )
    })?;
    let mut wire_image_omissions = 0_usize;
    while !remote_compaction
        && serialized.len() as u64 > candidate.provider.limits.max_request_bytes
        && omit_oldest_translated_input_image(&mut upstream_body)
    {
        wire_image_omissions += 1;
        serialized = serde_json::to_vec(&upstream_body).map_err(|error| {
            request_failure(
                "invalid_request",
                &format!("request cannot be encoded after image normalization: {error}"),
            )
        })?;
    }
    if serialized.len() as u64 > candidate.provider.limits.max_request_bytes {
        return Err(request_failure(
            "request_too_large",
            "translated request exceeds the configured provider limit",
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
    let mut request = client
        .post(endpoint.clone())
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
    if protocol == ProviderProtocol::AnthropicMessages {
        request = request.header("anthropic-version", "2023-06-01");
        if uses_anthropic_subscription_oauth(&candidate.provider) {
            for (name, value) in anthropic_subscription_oauth_headers() {
                request = request.header(name, value);
            }
        }
    }
    let request = match apply_provider_auth(
        request,
        &candidate.provider,
        candidate.credential.as_ref(),
    )
    .await
    {
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
    let request = if candidate.provider.google_mode == GoogleMode::CloudCodeAssist {
        request.header(header::USER_AGENT, "antigravity")
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
    match request.body(serialized).send().await {
        Ok(mut response) => {
            response.extensions_mut().insert(SentImageState {
                omitted_before_send: translation_image_omissions
                    .saturating_add(wire_image_omissions),
                sent_images: count_translated_input_images(&upstream_body),
            });
            crate::debug::log(&format!(
                "send_candidate_once: -> {}",
                response.status()
            ));
            if response
                .content_length()
                .is_some_and(|length| length > candidate.provider.limits.max_response_bytes)
            {
                Err(AttemptFailure {
                    response: error_response(
                        StatusCode::BAD_GATEWAY,
                        "provider_response_too_large",
                        "provider response exceeds the configured limit",
                    ),
                    kind: AttemptFailureKind::Retryable,
                })
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

pub(crate) async fn send_kiro_candidate(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
    wire_model: &str,
) -> Result<reqwest::Response, AttemptFailure> {
    let source_image_count = count_translated_input_images(body);
    let credential_source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let mut credential_headers =
        resolve_provider_headers(&candidate.provider, candidate.credential.as_ref())
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_provider_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?;
    if credential_source == CredentialSource::Forward {
        let Some(caller_headers) = caller_headers else {
            return Err(AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    "the selected Kiro provider requires caller authentication headers",
                ),
                kind: AttemptFailureKind::Credential,
            });
        };
        let forwarded = collect_forward_headers(&state.settings, caller_headers)
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?;
        let authorization = forwarded
            .get(header::AUTHORIZATION)
            .cloned()
            .ok_or_else(|| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    "the selected Kiro provider requires a caller Authorization header",
                ),
                kind: AttemptFailureKind::Credential,
            })?;
        credential_headers.insert(header::AUTHORIZATION, authorization);
    }
    let kiro_api_key = credential_headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|value| value.starts_with("ksk_"));
    let profile_arn = (!kiro_api_key)
        .then(|| candidate.provider.kiro_profile_arn.as_deref())
        .flatten();
    let (mut payload, context) = responses_to_kiro(body, wire_model, profile_arn)
        .map_err(|message| request_failure("unsupported_request", &message))?;
    let translation_image_omissions =
        source_image_count.saturating_sub(count_translated_input_images(&payload));
    let mut serialized = serde_json::to_vec(&payload).map_err(|error| {
        request_failure(
            "invalid_request",
            &format!("Kiro request cannot be encoded: {error}"),
        )
    })?;
    let mut wire_image_omissions = 0_usize;
    while serialized.len() as u64 > candidate.provider.limits.max_request_bytes
        && omit_oldest_kiro_wire_image(&mut payload)
    {
        wire_image_omissions += 1;
        serialized = serde_json::to_vec(&payload).map_err(|error| {
            request_failure(
                "invalid_request",
                &format!("Kiro request cannot be encoded after image normalization: {error}"),
            )
        })?;
    }
    if serialized.len() as u64 > candidate.provider.limits.max_request_bytes {
        return Err(request_failure(
            "request_too_large",
            "translated Kiro request exceeds the configured provider limit",
        ));
    }

    let endpoint = format!(
        "{}/",
        candidate.provider.base_url.trim().trim_end_matches('/')
    );
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
        .header(header::CONTENT_TYPE, "application/x-amz-json-1.0")
        .header(header::ACCEPT, "application/vnd.amazon.eventstream")
        .header(
            "x-amz-target",
            "AmazonCodeWhispererStreamingService.GenerateAssistantResponse",
        )
        .header("x-amzn-codewhisperer-optout", "true")
        .header("amz-sdk-request", "attempt=1; max=1")
        .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
        .header(
            header::USER_AGENT,
            "CODETAS-Gateway/0.1 (Kiro-compatible transport)",
        )
        .query(&candidate.provider.query_params)
        .timeout(Duration::from_millis(
            candidate.provider.limits.request_timeout_ms,
        ));
    let mut request = request;
    for (name, value) in &credential_headers {
        request = request.header(name, value);
    }
    if kiro_api_key {
        request = request.header("tokentype", "API_KEY");
    }
    if let Some(profile_arn) = profile_arn {
        request = request
            .header("x-amzn-kiro-profile-arn", profile_arn)
            .header("x-amzn-kiro-agent-mode", "vibe");
    }
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
                    "Kiro response exceeds the configured limit",
                ),
                kind: AttemptFailureKind::Retryable,
            })
        }
        Ok(mut response) => {
            response.extensions_mut().insert(SentImageState {
                omitted_before_send: translation_image_omissions
                    .saturating_add(wire_image_omissions),
                sent_images: count_translated_input_images(&payload),
            });
            response.extensions_mut().insert(context);
            Ok(response)
        }
        Err(error) => Err(AttemptFailure {
            response: error_response(
                StatusCode::BAD_GATEWAY,
                "provider_unreachable",
                &format!("Kiro request failed: {error}"),
            ),
            kind: AttemptFailureKind::Retryable,
        }),
    }
}

pub(crate) async fn send_github_copilot_candidate(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
    wire_model: &str,
) -> Result<reqwest::Response, AttemptFailure> {
    let source_image_count = count_translated_input_images(body);
    let mut upstream_body = responses_to_chat_with_options(
        body,
        wire_model,
        model_requires_reasoning_placeholder(&candidate.provider, &candidate.upstream_model),
    )
        .map_err(|message| request_failure("unsupported_request", &message))?;
    apply_provider_wire_compatibility(
        &mut upstream_body,
        body,
        candidate,
        ProviderProtocol::ChatCompletions,
    )
    .map_err(|message| request_failure("unsupported_request", &message))?;
    upstream_body["model"] = Value::String(wire_model.to_string());
    let translation_image_omissions =
        source_image_count.saturating_sub(count_translated_input_images(&upstream_body));
    let mut serialized = serde_json::to_vec(&upstream_body).map_err(|error| {
        request_failure(
            "invalid_request",
            &format!("GitHub Copilot request cannot be encoded: {error}"),
        )
    })?;
    let mut wire_image_omissions = 0_usize;
    while serialized.len() as u64 > candidate.provider.limits.max_request_bytes
        && omit_oldest_translated_input_image(&mut upstream_body)
    {
        wire_image_omissions += 1;
        serialized = serde_json::to_vec(&upstream_body).map_err(|error| {
            request_failure(
                "invalid_request",
                &format!("GitHub Copilot request cannot be encoded after image normalization: {error}"),
            )
        })?;
    }
    if serialized.len() as u64 > candidate.provider.limits.max_request_bytes {
        return Err(request_failure(
            "request_too_large",
            "translated GitHub Copilot request exceeds the configured provider limit",
        ));
    }

    let credential_source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let mut credential_headers =
        resolve_provider_headers(&candidate.provider, candidate.credential.as_ref())
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_provider_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?;
    if credential_source == CredentialSource::Forward {
        let Some(caller_headers) = caller_headers else {
            return Err(AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    "the selected GitHub Copilot provider requires caller authentication headers",
                ),
                kind: AttemptFailureKind::Credential,
            });
        };
        let forwarded = collect_forward_headers(&state.settings, caller_headers)
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?;
        let authorization = forwarded
            .get(header::AUTHORIZATION)
            .cloned()
            .ok_or_else(|| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    "the selected GitHub Copilot provider requires a caller Authorization header",
                ),
                kind: AttemptFailureKind::Credential,
            })?;
        credential_headers.insert(header::AUTHORIZATION, authorization);
    }
    let github_authorization = credential_headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AttemptFailure {
            response: error_response(
                StatusCode::UNAUTHORIZED,
                "missing_provider_credential",
                "GitHub Copilot requires a GitHub token via a bearer credential",
            ),
            kind: AttemptFailureKind::Credential,
        })?;
    let token_endpoint = "https://api.github.com/copilot_internal/v2/token";
    let dns_pinning = state.settings.read().await.security.dns_pinning;
    let exchange_client = if dns_pinning {
        pinned_client(token_endpoint, &candidate.provider, "CODETAS/0.1")
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
    let session = exchange_copilot_token(
        &exchange_client,
        github_authorization,
        Duration::from_millis(candidate.provider.limits.request_timeout_ms),
    )
    .await
    .map_err(|message| AttemptFailure {
        response: error_response(
            StatusCode::UNAUTHORIZED,
            "copilot_token_exchange_failed",
            &message,
        ),
        kind: AttemptFailureKind::Credential,
    })?;
    let endpoint = format!(
        "{}/chat/completions",
        session.api_base.trim_end_matches('/')
    );
    let client = if dns_pinning {
        pinned_client(&endpoint, &candidate.provider, "CODETAS/0.1")
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
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut request = client
        .post(endpoint)
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::ACCEPT,
            if streaming {
                "text/event-stream"
            } else {
                "application/json"
            },
        )
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", session.token.as_str()),
        )
        .header("editor-version", "CODETAS/0.1")
        .header("editor-plugin-version", "CODETAS/0.1")
        .header("copilot-integration-id", "vscode-chat")
        .header("openai-intent", "conversation-panel")
        .header(header::USER_AGENT, "CODETAS/0.1")
        .query(&candidate.provider.query_params)
        .timeout(Duration::from_millis(
            candidate.provider.limits.request_timeout_ms,
        ));
    for (name, value) in &credential_headers {
        if name != header::AUTHORIZATION {
            request = request.header(name, value);
        }
    }
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
                    "GitHub Copilot response exceeds the configured limit",
                ),
                kind: AttemptFailureKind::Retryable,
            })
        }
        Ok(mut response) => {
            response.extensions_mut().insert(SentImageState {
                omitted_before_send: translation_image_omissions
                    .saturating_add(wire_image_omissions),
                sent_images: count_translated_input_images(&upstream_body),
            });
            Ok(response)
        }
        Err(error) => Err(AttemptFailure {
            response: error_response(
                StatusCode::BAD_GATEWAY,
                "provider_unreachable",
                &format!("GitHub Copilot request failed: {error}"),
            ),
            kind: AttemptFailureKind::Retryable,
        }),
    }
}

pub(crate) fn google_thinking_level(effort: &str) -> Option<&'static str> {
    match effort {
        "minimal" => Some("minimal"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" | "xhigh" | "max" | "ultra" => Some("high"),
        _ => None,
    }
}

pub(crate) fn wire_model_for_request(candidate: &RouteCandidate, request: &Value) -> String {
    let model = candidate.upstream_model.as_str();
    if candidate.provider.google_mode != GoogleMode::CloudCodeAssist {
        return candidate.provider.wire_model_id(model);
    }
    let effort = request.pointer("/reasoning/effort").and_then(Value::as_str);
    match (model, effort) {
        (
            "gemini-3.7-flash" | "gemini-3.6-flash" | "gemini-3.5-flash",
            Some("low"),
        ) => format!("{model}-low"),
        (
            "gemini-3.7-flash" | "gemini-3.6-flash" | "gemini-3.5-flash",
            Some("high" | "xhigh" | "max" | "ultra"),
        ) => format!("{model}-high"),
        ("gemini-3.7-flash" | "gemini-3.6-flash" | "gemini-3.5-flash", _) => {
            format!("{model}-medium")
        }
        ("gemini-3.1-pro", Some("low")) => "gemini-3.1-pro-low".into(),
        ("gemini-3.1-pro", _) => "gemini-pro-agent".into(),
        _ => candidate.provider.wire_model_id(model),
    }
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
        .unwrap_or_else(|| candidate.exposed_model.as_str());
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
mod image_retry_tests {
    use super::*;

    #[test]
    fn retry_tightening_removes_prior_wire_omissions_plus_one_image() {
        let image = |suffix: &str| {
            json!({
                "type": "input_image",
                "image_url": format!("data:image/png;base64,AA{suffix}=")
            })
        };
        let mut body = json!({"input": [image("A"), image("B"), image("C")]});
        assert!(tighten_image_history_after_413(
            &mut body,
            SentImageState {
                omitted_before_send: 1,
                sent_images: 2,
            },
        ));
        assert_eq!(count_translated_input_images(&body), 1);
    }

    #[test]
    fn empty_completion_detection_has_positive_and_negative_cases() {
        assert!(completion_is_empty(&json!({"choices": []})));
        assert!(completion_is_empty(&json!({"output": []})));
        assert!(completion_is_empty(&json!({
            "status": "completed",
            "output": [{"type": "reasoning", "encrypted_content": "signature-only"}]
        })));
        assert!(completion_is_empty(&json!({
            "content": [{"type": "thinking", "thinking": "private"}],
            "stop_reason": "end_turn"
        })));
        assert!(completion_is_empty(&json!({
            "candidates": [{"content": {"parts": [{"thought": true, "text": "private"}]},
                "finishReason": "STOP"}]
        })));
        assert!(!completion_is_empty(&json!({"choices": [{"message": {"content": "ok"}}]})));
        assert!(!completion_is_empty(&json!({
            "status": "completed", "output": [{"type": "function_call", "name": "lookup"}]
        })));
        assert!(!completion_is_empty(&json!({
            "content": [], "stop_reason": "max_tokens"
        })));
        assert!(!completion_is_empty(&json!({
            "candidates": [{"content": {"parts": []}, "finishReason": "MAX_TOKENS"}]
        })));
        for reason in ["OTHER", "MALFORMED_FUNCTION_CALL", "UNEXPECTED_TOOL_CALL"] {
            assert!(!completion_is_empty(&json!({
                "candidates": [{"content": {"parts": []}, "finishReason": reason}]
            })));
        }
        assert!(!completion_is_empty(&json!({"error": {"message": "failed"}})));
    }

    #[test]
    fn cloud_code_assist_retry_reuses_the_wire_request_id() {
        let provider = crate::config::ProviderDefinition {
            id: "cloud-code".into(),
            project: Some("project-one".into()),
            google_mode: GoogleMode::CloudCodeAssist,
            ..crate::config::ProviderDefinition::default()
        };
        let candidate = RouteCandidate {
            capabilities: provider.capabilities.clone(),
            provider,
            upstream_model: "gemini-3.1-pro".into(),
            exposed_model: "gemini-3.1-pro".into(),
            credential: None,
            account_id: None,
            target_key: "cloud-code/gemini-3.1-pro".into(),
            route_id: None,
            failure_threshold: 1,
            quota_threshold_percent: 0,
            input_price_per_million: None,
            output_price_per_million: None,
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
        };
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let source = json!({"prompt_cache_key": "session-one"});
        let first = cloud_code_assist_envelope(
            request.clone(),
            &source,
            &candidate,
            "gemini-pro-agent",
            None,
            Some("agent-stable"),
        )
        .expect("first envelope");
        let retry = cloud_code_assist_envelope(
            request,
            &source,
            &candidate,
            "gemini-pro-agent",
            None,
            Some("agent-stable"),
        )
        .expect("retry envelope");
        assert_eq!(first, retry);
        assert_eq!(first["requestId"], "agent-stable");
    }

    #[test]
    fn nonstream_empty_retry_failure_carries_usage_and_dedicated_marker() {
        let usage = TokenUsage {
            input_tokens: 13,
            output_tokens: 2,
            total_tokens: 15,
            ..TokenUsage::default()
        };
        let response = empty_completion_retry_failed_response(usage.clone())
            .expect("dedicated failure response");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let marker = response
            .extensions()
            .get::<EmptyCompletionRecoveryFailure>()
            .expect("recovery failure marker");
        assert_eq!(marker.usage.total_tokens, usage.total_tokens);
    }

    #[test]
    fn streaming_guard_treats_reasoning_signatures_and_redaction_as_precontent() {
        for event in [
            json!({"type": "response.reasoning_text.delta", "delta": "private"}),
            json!({"type": "response.reasoning_signature.delta", "delta": "signature"}),
            json!({"type": "response.output_item.added", "item": {
                "type": "reasoning", "encrypted_content": "redacted"}}),
        ] {
            assert!(!provider_stream_event_has_visible_content(
                ProviderProtocol::Responses,
                &event
            ));
            assert!(provider_stream_event_is_reasoning(
                ProviderProtocol::Responses,
                &event
            ));
        }
        assert!(provider_stream_event_has_visible_content(
            ProviderProtocol::Responses,
            &json!({"type": "response.output_text.delta", "delta": "answer"})
        ));
        assert!(provider_stream_event_has_visible_content(
            ProviderProtocol::Responses,
            &json!({"type": "response.output_item.added", "item": {
                "type": "function_call", "name": "exec"}})
        ));
        assert!(provider_stream_event_has_visible_content(
            ProviderProtocol::Responses,
            &json!({"type": "response.completed", "response": {
                "status": "completed",
                "output": [{"type": "message", "content": [{
                    "type": "output_text", "text": "terminal-only answer"
                }]}]
            }})
        ));

        let anthropic_redacted = json!({
            "type": "content_block_start",
            "content_block": {"type": "redacted_thinking", "data": "opaque"}
        });
        assert!(provider_stream_event_is_reasoning(
            ProviderProtocol::AnthropicMessages,
            &anthropic_redacted
        ));
        assert!(!provider_stream_event_has_visible_content(
            ProviderProtocol::AnthropicMessages,
            &anthropic_redacted
        ));
        assert!(provider_stream_event_is_empty_terminal(
            ProviderProtocol::AnthropicMessages,
            &json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}})
        ));

        let gemini_signature = json!({
            "candidates": [{"content": {"parts": [{
                "thought": true, "thoughtSignature": "opaque", "text": "private"
            }]}}]
        });
        assert!(provider_stream_event_is_reasoning(
            ProviderProtocol::GeminiGenerateContent,
            &gemini_signature
        ));
        assert!(!provider_stream_event_has_visible_content(
            ProviderProtocol::GeminiGenerateContent,
            &gemini_signature
        ));

        let first_reasoning = json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "id": "rs_first_attempt",
                "type": "reasoning",
                "status": "completed",
                "encrypted_content": "opaque-first"
            }
        });
        let history = responses_reasoning_semantic_history(&first_reasoning);
        assert_eq!(history.len(), 1);
        assert!(history[0].get("id").is_none());
        let mut retry_message = json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "id": "msg_retry",
                "type": "message",
                "status": "completed",
                "content": [{"type": "output_text", "text": "answer"}]
            }
        });
        attach_prior_reasoning_history(&mut retry_message, &history);
        assert_eq!(
            retry_message
                .pointer("/item/provider_metadata/codetas/empty_completion_prior_reasoning/0/encrypted_content")
                .and_then(Value::as_str),
            Some("opaque-first")
        );
        assert_eq!(retry_message.pointer("/item/id").and_then(Value::as_str), Some("msg_retry"));
    }

    #[test]
    fn streaming_guard_retries_only_silent_completed_terminals() {
        let empty = json!({
            "type": "response.completed",
            "response": {"status": "completed", "output": [{"type": "reasoning"}]}
        });
        assert!(provider_stream_event_is_empty_terminal(
            ProviderProtocol::Responses,
            &empty
        ));
        for terminal in [
            json!({"type": "response.incomplete", "response": {
                "status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}}}),
            json!({"type": "response.incomplete", "response": {
                "status": "incomplete", "incomplete_details": {"reason": "content_filter"}}}),
            json!({"type": "response.failed", "response": {"status": "failed"}}),
        ] {
            assert!(provider_stream_event_is_visible_failure(
                ProviderProtocol::Responses,
                &terminal
            ));
            assert!(!provider_stream_event_is_empty_terminal(
                ProviderProtocol::Responses,
                &terminal
            ));
        }
        let failed = empty_completion_failed_event_for_protocol(
            ProviderProtocol::Responses,
            TokenUsage::default(),
            Some("resp_retry"),
        );
        assert_eq!(
            failed.pointer("/response/error/code").and_then(Value::as_str),
            Some(EMPTY_COMPLETION_RETRY_FAILED_CODE)
        );
        assert_eq!(
            failed.pointer("/response/id").and_then(Value::as_str),
            Some("resp_retry")
        );

        let usage = sum_token_usage(
            TokenUsage {
                input_tokens: 3,
                output_tokens: 0,
                total_tokens: 3,
                ..TokenUsage::default()
            },
            TokenUsage {
                input_tokens: 5,
                output_tokens: 2,
                total_tokens: 7,
                ..TokenUsage::default()
            },
        );
        assert_eq!(usage.input_tokens, 8);
        assert_eq!(usage.output_tokens, 2);
        assert_eq!(usage.total_tokens, 10);

        assert!(provider_stream_event_is_error(
            ProviderProtocol::Responses,
            &json!({"type": "response.failed", "response": {
                "status": "failed", "error": {"message": "upstream failed"}
            }})
        ));
        assert!(!provider_stream_event_is_error(
            ProviderProtocol::Responses,
            &json!({"type": "response.incomplete", "response": {
                "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"}
            }})
        ));

        let mut anthropic_start = json!({
            "type": "message_start",
            "message": {"usage": {"input_tokens": 5}}
        });
        write_anthropic_stream_start_usage(&mut anthropic_start, usage.clone());
        assert_eq!(
            anthropic_start
                .pointer("/message/usage/input_tokens")
                .and_then(Value::as_u64),
            Some(8)
        );
    }

    #[test]
    fn chat_metadata_is_forwarded_only_when_provider_opts_in() {
        let body = json!({"input": [{"type": "reasoning", "provider_metadata": {"vendor": {"signature": "opaque"}}}]});
        assert!(chat_compatible_request(&body, false).pointer("/input/0/provider_metadata").is_none());
        assert!(chat_compatible_request(&body, true).pointer("/input/0/provider_metadata").is_some());
    }
}

#[cfg(test)]
mod provider_pacing_tests {
    use super::*;

    #[test]
    fn concurrent_reservations_for_one_provider_are_spaced() {
        let mut pacing = ProviderPacingState::default();
        let now = Instant::now();
        let interval = Duration::from_millis(250);
        let first = reserve_provider_start(&mut pacing, "provider", interval, now);
        let second = reserve_provider_start(&mut pacing, "provider", interval, now);
        assert_eq!(first, now);
        assert!(second.duration_since(first) >= interval);
    }

    #[test]
    fn disabled_pacing_and_different_providers_do_not_share_a_queue() {
        let mut pacing = ProviderPacingState::default();
        let now = Instant::now();
        assert_eq!(reserve_provider_start(&mut pacing, "off", Duration::ZERO, now), now);
        assert!(pacing.next_slots.is_empty());
        assert_eq!(reserve_provider_start(&mut pacing, "one", Duration::from_secs(1), now), now);
        assert_eq!(reserve_provider_start(&mut pacing, "two", Duration::from_secs(1), now), now);
    }

    #[test]
    fn pacing_state_is_bounded_and_periodically_drops_expired_keys() {
        let mut pacing = ProviderPacingState::default();
        let now = Instant::now();
        for index in 0..(MAX_PROVIDER_PACING_KEYS + 32) {
            reserve_provider_start(
                &mut pacing,
                &format!("provider-{index}"),
                Duration::from_secs(60),
                now,
            );
        }
        assert!(pacing.next_slots.len() <= MAX_PROVIDER_PACING_KEYS);

        pacing.reservations = 127;
        reserve_provider_start(
            &mut pacing,
            "fresh",
            Duration::from_millis(1),
            now + Duration::from_secs(61),
        );
        assert_eq!(pacing.next_slots.len(), 1);
        assert!(pacing.next_slots.contains_key("fresh"));
    }
}

#[cfg(test)]
mod rate_limit_retry_tests {
    use super::*;
    use crate::config::ProviderLimits;

    #[test]
    fn retry_on_429_is_disabled_when_missing_or_defaulted() {
        let defaulted = ProviderLimits::default();
        let deserialized: ProviderLimits =
            serde_json::from_value(json!({})).expect("empty limits use safe defaults");

        assert!(!defaulted.retry_on_429);
        assert!(!deserialized.retry_on_429);
        assert!(!should_retry_rate_limit(
            &deserialized,
            CredentialSource::Environment,
            0,
        ));
    }

    #[test]
    fn retry_on_429_requires_opt_in_and_api_key_equivalent_credentials() {
        let limits = ProviderLimits {
            retry_on_429: true,
            max_429_retries: 2,
            ..ProviderLimits::default()
        };

        assert!(should_retry_rate_limit(
            &limits,
            CredentialSource::Environment,
            0,
        ));
        assert!(should_retry_rate_limit(
            &limits,
            CredentialSource::Keychain,
            1,
        ));
        assert!(!should_retry_rate_limit(
            &limits,
            CredentialSource::OAuth,
            0,
        ));
        assert!(!should_retry_rate_limit(
            &limits,
            CredentialSource::Forward,
            0,
        ));
        assert!(!should_retry_rate_limit(
            &limits,
            CredentialSource::Environment,
            2,
        ));
    }

    #[test]
    fn retry_after_is_not_truncated_to_the_old_five_second_cap() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("30"));

        assert_eq!(rate_limit_retry_delay(&headers, 0), Duration::from_secs(30));
    }
}
