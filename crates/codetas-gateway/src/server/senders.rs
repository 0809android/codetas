use super::*;

#[derive(Clone, Copy)]
struct SentImageState {
    omitted_before_send: usize,
    sent_images: usize,
}

#[derive(Clone)]
pub(crate) struct EmptyCompletionRecoveryFailure {
    pub(crate) usage: TokenUsage,
    pub(crate) additional_sends: u16,
}

#[derive(Clone, Copy)]
pub(crate) struct EmptyCompletionRecoverySuccess {
    pub(crate) additional_sends: u16,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ProviderRetryObservation {
    pub(crate) additional_sends: u16,
    pub(crate) recovery_kinds: Vec<String>,
    pub(crate) usage: TokenUsage,
}

#[derive(Clone, Default)]
pub(crate) struct SharedProviderRetryObservation(
    pub(crate) Arc<std::sync::Mutex<ProviderRetryObservation>>,
);

#[derive(Clone)]
pub(crate) struct RoutingAttemptLease(Arc<RoutingAttemptLeaseInner>);

struct RoutingAttemptLeaseInner {
    routing: Arc<Mutex<RoutingRuntime>>,
    candidate: RouteCandidate,
}

impl Drop for RoutingAttemptLeaseInner {
    fn drop(&mut self) {
        // Prefer a synchronous release so Drop never depends solely on a live
        // Tokio runtime. Fall back to an async unlock only when contended.
        if let Ok(mut routing) = self.routing.try_lock() {
            routing.end_attempt(&self.candidate);
            return;
        }
        let routing = Arc::clone(&self.routing);
        let candidate = self.candidate.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                routing.lock().await.end_attempt(&candidate);
            });
            return;
        }
        // Last resort: block for the mutex when no runtime is available.
        // This path is rare (tests / teardown) but must not leak active leases.
        let mut routing = self.routing.blocking_lock();
        routing.end_attempt(&candidate);
    }
}

pub(crate) fn take_routing_attempt_lease(
    response: &mut reqwest::Response,
) -> Option<RoutingAttemptLease> {
    response.extensions_mut().remove::<RoutingAttemptLease>()
}

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
    crate::compaction::validate_local_compactions(body)
        .map_err(|message| request_failure("invalid_compaction_history", &message))?;
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
    let mut first = {
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
    first = if matches!(first.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
        && candidate_oauth_credential(candidate)
    {
        match crate::oauth::force_refresh_oauth_access_token(&candidate.provider.id).await {
            Ok(_) => {
                crate::debug::log(&format!(
                    "provider returned {}; refreshed OAuth and retrying once: provider={} model={}",
                    first.status(), candidate.provider.id, candidate.upstream_model,
                ));
                let prepared_body: &Value = body;
                match retry_provider_request(state, candidate, streaming, || {
                    send_candidate_once(
                        state,
                        prepared_body,
                        candidate,
                        caller_headers,
                        cloud_request_id.as_deref(),
                        cloud_envelope_cache.as_ref(),
                    )
                })
                .await
                {
                    Ok(mut response) => {
                        let first_diagnostic = first
                            .extensions()
                            .get::<UpstreamErrorDiagnostic>()
                            .cloned();
                        let mut recovery = first
                            .extensions_mut()
                            .remove::<ProviderRetryObservation>()
                            .unwrap_or_default();
                        recovery.additional_sends = recovery.additional_sends.saturating_add(1);
                        recovery.recovery_kinds.push("oauth-refresh".into());
                        let nested = response
                            .extensions_mut()
                            .remove::<ProviderRetryObservation>();
                        let mut combined = Some(recovery);
                        merge_provider_retry_observation(&mut combined, nested);
                        if let Some(combined) = combined {
                            response.extensions_mut().insert(combined);
                        }
                        if let Some(diagnostic) = first_diagnostic {
                            response.extensions_mut().insert(diagnostic);
                        }
                        response
                    }
                    Err(mut failure) => {
                        let mut recovery = failure
                            .response
                            .extensions_mut()
                            .remove::<ProviderRetryObservation>()
                            .unwrap_or_default();
                        recovery.additional_sends = recovery.additional_sends.saturating_add(1);
                        recovery.recovery_kinds.push("oauth-refresh".into());
                        failure.response.extensions_mut().insert(recovery);
                        return Err(failure);
                    }
                }
            }
            Err(error) => {
                crate::debug::log(&format!(
                    "OAuth refresh after provider auth rejection failed: provider={} error={}",
                    candidate.provider.id, error,
                ));
                let mut recovery = first
                    .extensions_mut()
                    .remove::<ProviderRetryObservation>()
                    .unwrap_or_default();
                recovery.recovery_kinds.push("oauth-refresh-failed".into());
                first.extensions_mut().insert(recovery);
                first
            }
        }
    } else {
        first
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
            let mut recovery = first
                .extensions()
                .get::<ProviderRetryObservation>()
                .cloned()
                .unwrap_or_default();
            if let Ok(bytes) = read_bounded(first, 64 * 1024).await {
                if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                    recovery.usage =
                        sum_token_usage(recovery.usage, TokenUsage::from_json(&value));
                }
            }
            recovery.additional_sends = recovery.additional_sends.saturating_add(1);
            recovery.recovery_kinds.push("payload-too-large".into());
            if let Some(cache) = cloud_envelope_cache.as_ref() {
                *cache.lock().await = None;
            }
            crate::debug::log(&format!(
                "provider returned 413; retrying once with tighter image history: provider={} model={}",
                candidate.provider.id, candidate.upstream_model,
            ));
            let prepared_body: &Value = body;
            let mut response = retry_provider_request(state, candidate, streaming, || {
                send_candidate_once(
                    state,
                    prepared_body,
                    candidate,
                    caller_headers,
                    cloud_request_id.as_deref(),
                    cloud_envelope_cache.as_ref(),
                )
            })
            .await?;
            let nested = response
                .extensions_mut()
                .remove::<ProviderRetryObservation>();
            let mut combined = Some(recovery);
            merge_provider_retry_observation(&mut combined, nested);
            if let Some(combined) = combined {
                response.extensions_mut().insert(combined);
            }
            response
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

fn candidate_oauth_credential(candidate: &RouteCandidate) -> bool {
    candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source
        == CredentialSource::OAuth
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
    state
        .routing
        .lock()
        .await
        .begin_attempt(candidate)
        .map_err(|message| AttemptFailure {
            response: error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "routing_capacity",
                &message,
            ),
            kind: AttemptFailureKind::Request,
        })?;
    let lease = RoutingAttemptLease(Arc::new(RoutingAttemptLeaseInner {
        routing: Arc::clone(&state.routing),
        candidate: candidate.clone(),
    }));
    let transport_retries = if streaming {
        candidate.provider.limits.stream_retries
    } else {
        candidate.provider.limits.request_retries
    };
    let mut transport_attempts = 0_u8;
    let mut rate_limit_attempts = 0_u8;
    let mut sends = 0_u16;
    let mut retry_observation = ProviderRetryObservation::default();
    let result = async {
    loop {
        await_provider_pacing(state, candidate).await?;
        sends = sends.saturating_add(1);
        match operation().await {
            Ok(mut response) if response.status() == StatusCode::TOO_MANY_REQUESTS => {
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
                    merge_retry_response_usage(&mut retry_observation, response).await;
                    retry_observation.recovery_kinds.push("rate-limit".into());
                    tokio::time::sleep(delay).await;
                    rate_limit_attempts = rate_limit_attempts.saturating_add(1);
                    continue;
                }
                attach_retry_observation(&mut response, sends, retry_observation);
                return Ok(response);
            }
            Ok(mut response)
                if response.status() == StatusCode::REQUEST_TIMEOUT
                    || response.status().is_server_error() =>
            {
                if transport_attempts < transport_retries {
                    let recovery = if response.status() == StatusCode::REQUEST_TIMEOUT {
                        "request-timeout"
                    } else {
                        "provider-5xx"
                    };
                    let delay = validated_retry_after(response.headers())
                        .map(|(_, delay)| delay.unwrap_or(Duration::ZERO))
                        .unwrap_or_else(|| retry_backoff(transport_attempts));
                    merge_retry_response_usage(&mut retry_observation, response).await;
                    retry_observation.recovery_kinds.push(recovery.into());
                    tokio::time::sleep(delay.min(Duration::from_secs(5))).await;
                    transport_attempts = transport_attempts.saturating_add(1);
                    continue;
                }
                attach_retry_observation(&mut response, sends, retry_observation);
                return Ok(response);
            }
            Ok(mut response) => {
                attach_retry_observation(&mut response, sends, retry_observation);
                return Ok(response);
            }
            Err(failure)
                if failure.kind == AttemptFailureKind::Retryable
                    && transport_attempts < transport_retries =>
            {
                retry_observation.recovery_kinds.push("transport".into());
                tokio::time::sleep(retry_backoff(transport_attempts)).await;
                transport_attempts = transport_attempts.saturating_add(1);
            }
            Err(mut failure) => {
                attach_retry_observation_to_failure(
                    &mut failure,
                    sends,
                    retry_observation,
                );
                return Err(failure);
            }
        }
    }
    }
    .await;
    match result {
        Ok(mut response) => {
            response.extensions_mut().insert(lease);
            Ok(response)
        }
        Err(mut failure) => {
            failure.response.extensions_mut().insert(lease);
            Err(failure)
        }
    }
}

fn attach_retry_observation(
    response: &mut reqwest::Response,
    sends: u16,
    mut observation: ProviderRetryObservation,
) {
    observation.additional_sends = sends.saturating_sub(1);
    if observation.additional_sends > 0 {
        response.extensions_mut().insert(observation);
    }
}

fn attach_retry_observation_to_failure(
    failure: &mut AttemptFailure,
    sends: u16,
    mut observation: ProviderRetryObservation,
) {
    observation.additional_sends = sends.saturating_sub(1);
    if observation.additional_sends > 0 {
        failure.response.extensions_mut().insert(observation);
    }
}

fn merge_provider_retry_observation(
    target: &mut Option<ProviderRetryObservation>,
    next: Option<ProviderRetryObservation>,
) {
    let Some(next) = next else {
        return;
    };
    if let Some(current) = target.as_mut() {
        current.additional_sends = current
            .additional_sends
            .saturating_add(next.additional_sends);
        current.recovery_kinds.extend(next.recovery_kinds);
        current.usage = sum_token_usage(current.usage.clone(), next.usage);
    } else {
        *target = Some(next);
    }
}

async fn merge_retry_response_usage(
    observation: &mut ProviderRetryObservation,
    response: reqwest::Response,
) {
    if let Ok(bytes) = read_bounded(response, 64 * 1024).await {
        if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
            observation.usage = sum_token_usage(
                observation.usage.clone(),
                TokenUsage::from_json(&value),
            );
        }
    }
}

struct ProviderPacingReservation {
    pacing: Arc<ProviderPacing>,
    provider_id: String,
    ticket: u64,
    generation: u64,
    armed: bool,
}

impl ProviderPacingReservation {
    async fn complete(mut self) {
        {
            let mut state = self.pacing.state.lock().await;
            remove_provider_pacing_ticket(
                &mut state,
                &self.provider_id,
                self.ticket,
                self.generation,
                true,
            );
        }
        self.pacing.changed.notify_waiters();
        self.armed = false;
    }

    async fn cancel(&mut self) {
        {
            let mut state = self.pacing.state.lock().await;
            remove_provider_pacing_ticket(
                &mut state,
                &self.provider_id,
                self.ticket,
                self.generation,
                false,
            );
        }
        self.pacing.changed.notify_waiters();
        self.armed = false;
    }

    fn try_remove(&self, started: bool) -> bool {
        let Ok(mut state) = self.pacing.state.try_lock() else {
            return false;
        };
        remove_provider_pacing_ticket(
            &mut state,
            &self.provider_id,
            self.ticket,
            self.generation,
            started,
        );
        true
    }
}

impl Drop for ProviderPacingReservation {
    fn drop(&mut self) {
        if self.armed {
            if !self.try_remove(false) {
                let pacing = Arc::clone(&self.pacing);
                let provider_id = self.provider_id.clone();
                let ticket = self.ticket;
                let generation = self.generation;
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        let mut state = pacing.state.lock().await;
                        remove_provider_pacing_ticket(
                            &mut state,
                            &provider_id,
                            ticket,
                            generation,
                            false,
                        );
                        pacing.changed.notify_waiters();
                    });
                }
            }
            self.pacing.changed.notify_waiters();
        }
    }
}

fn remove_provider_pacing_ticket(
    state: &mut ProviderPacingState,
    provider_id: &str,
    ticket: u64,
    generation: u64,
    started: bool,
) {
    remove_provider_pacing_ticket_at(
        state,
        provider_id,
        ticket,
        generation,
        started,
        Instant::now(),
    );
}

fn remove_provider_pacing_ticket_at(
    state: &mut ProviderPacingState,
    provider_id: &str,
    ticket: u64,
    generation: u64,
    started: bool,
    now: Instant,
) {
    if state.generation != generation {
        return;
    }
    if let Some(queue) = state.queues.get_mut(provider_id) {
        if let Some(index) = queue.waiters.iter().position(|waiter| waiter.ticket == ticket) {
            queue.waiters.remove(index);
        }
        if started {
            queue.last_started = Some(now);
        }
        let mut next = queue
            .last_started
            .and_then(|last| queue.waiters.front().map(|waiter| last + waiter.interval))
            .unwrap_or(now)
            .max(now);
        for waiter in &mut queue.waiters {
            waiter.scheduled = next;
            next = next + waiter.interval;
        }
        if queue.waiters.is_empty()
            && queue
                .last_started
                .is_none_or(|last| last + MAX_PROVIDER_PACING_WAIT <= now)
        {
            state.queues.remove(provider_id);
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ProviderPacingSlot {
    pub(crate) scheduled: Instant,
    pub(crate) ticket: u64,
    pub(crate) generation: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ProviderPacingError {
    Overloaded(Duration),
    SettingsChanged,
}

enum SuspendedProviderAdmission {
    Http(Arc<MemoryAdmission>, u64),
    WebSocket(Arc<MemoryAdmission>, u64),
    None,
}

async fn suspend_provider_admission_for_pacing() -> SuspendedProviderAdmission {
    if let Some((memory, bytes)) = suspend_http_admission_for_pacing().await {
        return SuspendedProviderAdmission::Http(memory, bytes);
    }
    if let Some((memory, bytes)) = suspend_websocket_admission_for_pacing().await {
        return SuspendedProviderAdmission::WebSocket(memory, bytes);
    }
    SuspendedProviderAdmission::None
}

async fn restore_provider_admission_after_pacing(
    suspended: SuspendedProviderAdmission,
) -> Result<(), &'static str> {
    match suspended {
        SuspendedProviderAdmission::Http(memory, bytes) => {
            restore_http_admission_after_pacing(Some((memory, bytes))).await
        }
        SuspendedProviderAdmission::WebSocket(memory, bytes) => {
            restore_websocket_admission_after_pacing(Some((memory, bytes))).await
        }
        SuspendedProviderAdmission::None => Ok(()),
    }
}

async fn await_provider_pacing(
    state: &GatewayState,
    candidate: &RouteCandidate,
) -> Result<(), AttemptFailure> {
    let interval = Duration::from_millis(candidate.provider.limits.request_pacing_ms);
    if interval.is_zero() {
        return Ok(());
    }
    // The provider queue is a pre-send admission boundary. Release the HTTP
    // request's global inflight/memory reservation while it waits, then restore
    // the exact reservation immediately before the upstream operation begins.
    let suspended_admission = suspend_provider_admission_for_pacing().await;
    let now = Instant::now();
    let slot = {
        let mut pacing = state.pacing.state.lock().await;
        reserve_provider_start(&mut pacing, &candidate.provider.id, interval, now)
            .map_err(provider_pacing_failure)?
    };
    let mut reservation = ProviderPacingReservation {
        pacing: Arc::clone(&state.pacing),
        provider_id: candidate.provider.id.clone(),
        ticket: slot.ticket,
        generation: slot.generation,
        armed: true,
    };
    loop {
        let changed = state.pacing.changed.notified();
        let waiter = {
            let pacing = state.pacing.state.lock().await;
            pacing.queues.get(&candidate.provider.id)
                .and_then(|queue| queue.waiters.iter().find(|waiter| waiter.ticket == slot.ticket))
                .map(|waiter| (waiter.queued_at, provider_pacing_waiter_wake_at(waiter)))
        };
        let Some((queued_at, wake_at)) = waiter else {
            return Err(provider_pacing_failure(ProviderPacingError::SettingsChanged));
        };
        let expires_at = queued_at + MAX_PROVIDER_PACING_WAIT;
        tokio::select! {
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wake_at)) => {
                if Instant::now() >= expires_at {
                    reservation.cancel().await;
                    return Err(provider_pacing_failure(ProviderPacingError::Overloaded(
                        Duration::ZERO,
                    )));
                }
                break;
            },
            _ = changed => {
                let pacing = state.pacing.state.lock().await;
                let still_queued = pacing.generation == slot.generation
                    && pacing.queues.get(&candidate.provider.id)
                        .is_some_and(|queue| queue.waiters.iter().any(|waiter| waiter.ticket == slot.ticket));
                if !still_queued {
                    return Err(provider_pacing_failure(ProviderPacingError::SettingsChanged));
                }
            }
        }
    }
    restore_provider_admission_after_pacing(suspended_admission)
        .await
        .map_err(admission_reacquire_failure)?;
    reservation.complete().await;
    Ok(())
}

fn provider_pacing_waiter_wake_at(waiter: &ProviderPacingWaiter) -> Instant {
    waiter
        .scheduled
        .min(waiter.queued_at + MAX_PROVIDER_PACING_WAIT)
}

fn admission_reacquire_failure(message: &'static str) -> AttemptFailure {
    AttemptFailure {
        response: error_response(StatusCode::SERVICE_UNAVAILABLE, "gateway_capacity", message),
        // Local admission reacquire failure is terminal for this client
        // request. No alternate candidate may send after the reservation has
        // been released, and provider cooldown state must remain untouched.
        kind: AttemptFailureKind::Request,
    }
}

fn provider_pacing_failure(error: ProviderPacingError) -> AttemptFailure {
    let (status, code, message, retry_after) = match error {
        ProviderPacingError::Overloaded(retry_after) => (
            StatusCode::TOO_MANY_REQUESTS,
            "provider_pacing_overloaded",
            "provider pacing queue is full or would exceed its maximum wait",
            Some(retry_after),
        ),
        ProviderPacingError::SettingsChanged => (
            StatusCode::SERVICE_UNAVAILABLE,
            "provider_pacing_reconfigured",
            "provider settings changed while the request was queued",
            None,
        ),
    };
    let mut response = error_response(status, code, message);
    if let Some(delay) = retry_after {
        let seconds = retry_after_seconds_ceil(delay);
        if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
    }
    AttemptFailure {
        response,
        // Local pacing admission is not an upstream transport failure and must
        // not silently switch a policy route to another provider.
        kind: AttemptFailureKind::Request,
    }
}

fn retry_after_seconds_ceil(delay: Duration) -> u64 {
    delay
        .as_secs()
        .saturating_add(u64::from(delay.subsec_nanos() > 0))
        .max(1)
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
        .min(Duration::from_secs(10 * 60))
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
) -> Result<ProviderPacingSlot, ProviderPacingError> {
    if interval.is_zero() {
        return Ok(ProviderPacingSlot {
            scheduled: now,
            ticket: 0,
            generation: pacing.generation,
        });
    }
    pacing.queues.retain(|_, queue| {
        !queue.waiters.is_empty()
            || queue
                .last_started
                .is_some_and(|last| last + MAX_PROVIDER_PACING_WAIT > now)
    });
    if !pacing.queues.contains_key(key) && pacing.queues.len() >= MAX_PROVIDER_PACING_KEYS {
        let retry_after = pacing
            .queues
            .values()
            .filter_map(|queue| provider_pacing_queue_reclaim_at(queue, now))
            .min()
            .map(|deadline| deadline.saturating_duration_since(now))
            .unwrap_or(interval);
        return Err(ProviderPacingError::Overloaded(retry_after));
    }
    if pacing
        .queues
        .get(key)
        .is_some_and(|queue| queue.waiters.len() >= MAX_PROVIDER_PACING_QUEUE_DEPTH)
    {
        let retry_after = pacing
            .queues
            .get(key)
            .and_then(|queue| queue.waiters.front())
            .map(|waiter| waiter.scheduled.saturating_duration_since(now))
            .unwrap_or(interval);
        return Err(ProviderPacingError::Overloaded(retry_after));
    }
    let scheduled = pacing.queues.get(key).map_or(now, |queue| {
        queue
            .waiters
            .back()
            .map(|waiter| waiter.scheduled + waiter.interval)
            .or_else(|| queue.last_started.map(|last| last + interval))
            .unwrap_or(now)
            .max(now)
    });
    let wait = scheduled.saturating_duration_since(now);
    if wait >= MAX_PROVIDER_PACING_WAIT {
        return Err(ProviderPacingError::Overloaded(wait));
    }
    pacing.next_ticket = pacing.next_ticket.wrapping_add(1).max(1);
    let ticket = pacing.next_ticket;
    let queue = pacing.queues.entry(key.to_string()).or_default();
    queue.waiters.push_back(ProviderPacingWaiter {
        ticket,
        queued_at: now,
        scheduled,
        interval,
    });
    Ok(ProviderPacingSlot {
        scheduled,
        ticket,
        generation: pacing.generation,
    })
}

fn provider_pacing_queue_reclaim_at(
    queue: &ProviderPacingQueue,
    now: Instant,
) -> Option<Instant> {
    if queue.waiters.is_empty() {
        return queue
            .last_started
            .map(|last| last + MAX_PROVIDER_PACING_WAIT);
    }

    let mut last_started = queue.last_started;
    let mut final_waiter_removed_at = now;
    for waiter in &queue.waiters {
        let scheduled = last_started
            .map(|last| last + waiter.interval)
            .unwrap_or(waiter.scheduled)
            .max(final_waiter_removed_at);
        let expires = waiter.queued_at + MAX_PROVIDER_PACING_WAIT;
        if scheduled < expires {
            last_started = Some(scheduled);
            final_waiter_removed_at = scheduled;
        } else {
            final_waiter_removed_at = final_waiter_removed_at.max(expires);
        }
    }
    Some(
        last_started
            .map(|last| last + MAX_PROVIDER_PACING_WAIT)
            .unwrap_or(final_waiter_removed_at)
            .max(final_waiter_removed_at),
    )
}

pub(crate) async fn reset_provider_pacing(pacing: &ProviderPacing) {
    {
        let mut state = pacing.state.lock().await;
        state.generation = state.generation.wrapping_add(1);
        state.queues.clear();
    }
    pacing.changed.notify_waiters();
}

struct BufferedProviderResponse {
    status: StatusCode,
    version: axum::http::Version,
    headers: HeaderMap,
    bytes: Bytes,
    value: Option<Value>,
    retry_observation: Option<ProviderRetryObservation>,
    routing_attempt_lease: Option<RoutingAttemptLease>,
}

impl BufferedProviderResponse {
    fn into_response(self) -> Result<reqwest::Response, AttemptFailure> {
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
        if let Some(observation) = self.retry_observation {
            response.extensions_mut().insert(observation);
        }
        if let Some(lease) = self.routing_attempt_lease {
            response.extensions_mut().insert(lease);
        }
        Ok(response)
    }
}

async fn buffer_provider_response(
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
    let routing_attempt_lease = take_routing_attempt_lease(&mut response);
    let bytes = read_bounded(response, limit)
        .await
        .map_err(|message| request_failure("invalid_provider_response", &message))?;
    let value = serde_json::from_slice::<Value>(&bytes).ok();
    Ok(BufferedProviderResponse {
        status,
        version,
        headers,
        bytes: Bytes::from(bytes),
        value,
        retry_observation,
        routing_attempt_lease,
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
    let mut first = buffer_provider_response(response, limit).await?;
    let mut provider_retries = first.retry_observation.clone();
    let Some(first_value) = first.value.as_ref() else {
        return first.into_response();
    };
    if !completion_is_empty(first_value) {
        return first.into_response();
    }
    let mut usage = TokenUsage::from_json(first_value);
    // The first provider body has reached EOF and will not be reconstructed;
    // release that attempt before starting an independent empty retry.
    drop(first.routing_attempt_lease.take());
    let budget = candidate.provider.limits.empty_completion_retries;
    for retry_index in 0..budget {
        tokio::time::sleep(retry_backoff(retry_index)).await;
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
            Err(_) => {
                merge_provider_retry_observation(&mut provider_retries, retry_observation);
                return empty_completion_retry_failed_response_with_observation(
                    usage,
                    u16::from(retry_index) + 1,
                    provider_retries,
                )
            }
        };
        merge_provider_retry_observation(
            &mut provider_retries,
            retry.retry_observation.clone(),
        );
        let Some(mut retry_value) = retry.value.take() else {
            return empty_completion_retry_failed_response_with_observation(
                usage,
                u16::from(retry_index) + 1,
                provider_retries,
            );
        };
        usage = sum_token_usage(usage, TokenUsage::from_json(&retry_value));
        if completion_is_empty(&retry_value) {
            continue;
        }
        write_token_usage(&mut retry_value, usage);
        retry.bytes = Bytes::from(serde_json::to_vec(&retry_value).map_err(|_| {
            request_failure("gateway_error", "failed to encode retried response")
        })?);
        retry.headers.remove(header::CONTENT_LENGTH);
        retry.value = Some(retry_value);
        let empty_sends = u16::from(retry_index) + 1;
        let observation = provider_retries
            .get_or_insert_with(ProviderRetryObservation::default);
        observation.additional_sends = observation.additional_sends.saturating_add(empty_sends);
        observation.recovery_kinds.extend(
            std::iter::repeat("empty-completion".to_string())
                .take(usize::from(empty_sends)),
        );
        retry.retry_observation = provider_retries;
        let mut response = retry.into_response()?;
        response.extensions_mut().insert(EmptyCompletionRecoverySuccess {
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
    let retry_observation = response
        .extensions()
        .get::<ProviderRetryObservation>()
        .cloned();
    let shared_retry_observation = SharedProviderRetryObservation(Arc::new(
        std::sync::Mutex::new(ProviderRetryObservation::default()),
    ));
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
        let mut attempt_reasoning_history = Vec::<Value>::new();
        let mut prior_reasoning_injected = false;
        let mut next_response_sequence = 0_u64;
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
                    let event_type = value
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("message")
                        .to_string();
                    if protocol == ProviderProtocol::Responses {
                        let semantic = responses_reasoning_semantic_history(&value);
                        if event_type == "response.output_item.done" {
                            attempt_reasoning_history.extend(semantic);
                        } else if matches!(
                            event_type.as_str(),
                            "response.completed" | "response.incomplete" | "response.failed"
                        ) && attempt_reasoning_history.is_empty()
                        {
                            attempt_reasoning_history.extend(semantic);
                        }
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
                                observation.additional_sends =
                                    observation.additional_sends.saturating_add(1);
                                observation.recovery_kinds.push("empty-completion".into());
                            }
                            yield Ok(Bytes::from_static(EMPTY_COMPLETION_RECOVERY_MARKER));
                            // The retry is a distinct upstream response lifecycle. Do not
                            // splice the first attempt's provider IDs/signatures into it.
                            if protocol == ProviderProtocol::Responses {
                                held.clear();
                                held_bytes = 0;
                                prior_reasoning_injected = false;
                                next_response_sequence = 0;
                            }
                            tokio::time::sleep(retry_backoff(retries.saturating_sub(1))).await;
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
                                    source = provider_guard_stream(retry, tolerate_anthropic_eof);
                                    pending.clear();
                                    received = 0;
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
    let mut response = builder
        .body(reqwest::Body::wrap_stream(output))
        .map(reqwest::Response::from)
        .map_err(|_| request_failure("gateway_error", "failed to build guarded provider stream"))?;
    if let Some(observation) = retry_observation {
        response.extensions_mut().insert(observation);
    }
    response
        .extensions_mut()
        .insert(shared_retry_observation);
    Ok(response)
}

fn provider_guard_stream(
    mut response: reqwest::Response,
    tolerate_incomplete_eof: bool,
) -> Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Send>> {
    let routing_attempt_lease = take_routing_attempt_lease(&mut response);
    Box::pin(stream! {
        let _routing_attempt_lease = routing_attempt_lease;
        let mut source = response.bytes_stream();
        while let Some(chunk) = source.next().await {
            yield chunk;
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

pub(crate) fn provider_stream_event_is_terminal(protocol: ProviderProtocol, value: &Value) -> bool {
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
                        choice.get("error").is_some_and(|error| !error.is_null())
                            || choice
                            .get("finish_reason")
                            .and_then(Value::as_str)
                            .is_some_and(|reason| !matches!(reason, "stop" | "tool_calls"))
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
                            .is_some_and(|reason| {
                                gemini_finish_disposition(Some(reason))
                                    != GeminiFinishDisposition::Stop
                            })
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
        ProviderProtocol::ChatCompletions => value.get("error").is_some_and(|error| !error.is_null())
            || value.get("choices").and_then(Value::as_array).is_some_and(|choices| {
                choices.iter().any(|choice| {
                    choice.get("error").is_some_and(|error| !error.is_null())
                        || choice.get("finish_reason").and_then(Value::as_str).is_some_and(
                            |reason| !matches!(reason, "stop" | "tool_calls" | "length" | "max_tokens" | "content_filter")
                        )
                })
            }),
        ProviderProtocol::GeminiGenerateContent => {
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

fn canonical_reasoning_lifecycle_events(items: &[Value]) -> Vec<Value> {
    let mut events = Vec::new();
    for (output_index, item) in items.iter().enumerate() {
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        events.push(json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": added,
        }));
        events.push(json!({
            "type": "response.output_item.done",
            "output_index": output_index,
            "item": item,
        }));
    }
    events
}

fn assign_responses_sequence(value: &mut Value, next: &mut u64) {
    value["sequence_number"] = json!(*next);
    *next = next.saturating_add(1);
}

fn shift_responses_retry_event(value: &mut Value, offset: usize, history: &[Value]) {
    if offset == 0 {
        return;
    }
    if let Some(index) = value.get("output_index").and_then(Value::as_u64) {
        value["output_index"] = json!(index.saturating_add(offset as u64));
    }
    if matches!(
        value.get("type").and_then(Value::as_str),
        Some("response.completed" | "response.incomplete" | "response.failed")
    ) {
        if let Some(output) = value
            .pointer_mut("/response/output")
            .and_then(Value::as_array_mut)
        {
            let mut combined = history.to_vec();
            combined.append(output);
            *output = combined;
        }
    }
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
    additional_sends: u16,
) -> Result<reqwest::Response, AttemptFailure> {
    empty_completion_retry_failed_response_with_observation(usage, additional_sends, None)
}

fn empty_completion_retry_failed_response_with_observation(
    usage: TokenUsage,
    additional_sends: u16,
    mut retry_observation: Option<ProviderRetryObservation>,
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
        .insert(EmptyCompletionRecoveryFailure {
            usage: usage.clone(),
            additional_sends,
        });
    let observation = retry_observation.get_or_insert_with(ProviderRetryObservation::default);
    observation.additional_sends = observation
        .additional_sends
        .saturating_add(additional_sends);
    observation.recovery_kinds.extend(
        std::iter::repeat("empty-completion".to_string())
            .take(usize::from(additional_sends)),
    );
    observation.usage = usage;
    response.extensions_mut().insert(observation.clone());
    Ok(response)
}

async fn response_body_usage(response: Response<Body>) -> TokenUsage {
    to_bytes(response.into_body(), 64 * 1024)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .as_ref()
        .map(TokenUsage::from_json)
        .unwrap_or_default()
}

async fn response_body_usage_from_reqwest(response: reqwest::Response) -> TokenUsage {
    // Failure-body accounting must stay independently bounded. A provider may
    // allow very large successful responses, but retry diagnostics never need
    // to buffer that configured maximum merely to recover usage metadata.
    buffer_provider_response(response, 64 * 1024)
        .await
        .ok()
        .and_then(|buffered| buffered.value)
        .as_ref()
        .map(TokenUsage::from_json)
        .unwrap_or_default()
}

pub(crate) fn sum_token_usage(left: TokenUsage, right: TokenUsage) -> TokenUsage {
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
            "candidatesTokenCount": usage.output_tokens.saturating_sub(usage.reasoning_tokens),
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
                .is_some_and(|reason| {
                    gemini_finish_disposition(Some(reason))
                        != GeminiFinishDisposition::Stop
                })
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
    // Continuity control fields stay on `body` for local `remember`; never wire them.
    crate::response_state::ResponseStateStore::strip_private_fields(&mut upstream_body);
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
    if remote_compaction && protocol == ProviderProtocol::Responses {
        // Match OpenCodex's normal Responses adapter: preserve the v2
        // compaction trigger and top-level options, expand local `codetas1:`
        // envelopes, and remove replayed raw reasoning content that the native
        // backend rejects.
        crate::compat::sanitize_compact_trigger_request(&mut upstream_body);
        crate::debug::log(
            "send_candidate: applied compact-trigger reasoning sanitize; broader compatibility skipped",
        );
    }
    let mut wire_budget_omissions = 0_usize;
    if !remote_compaction
        && matches!(
            protocol,
            ProviderProtocol::AnthropicMessages
                | ProviderProtocol::GeminiGenerateContent
                | ProviderProtocol::ChatCompletions
        )
    {
        let image_report = normalize_translated_image_history(
            &mut upstream_body,
            protocol,
            candidate.provider.transport,
            candidate.provider.limits.max_request_bytes,
        )
        .await
        .map_err(|message| request_failure("image_normalization_failed", &message))?;
        wire_budget_omissions = image_report.omitted_images;
        if image_report.omitted_images > 0 {
            crate::debug::log(&format!(
                "wire image history normalized: provider={} model={} protocol={:?} omitted={} total={} kept_base64_chars={}",
                candidate.provider.id,
                candidate.upstream_model,
                protocol,
                image_report.omitted_images,
                image_report.inline_images,
                image_report.kept_base64_chars,
            ));
        }
    }
    let translation_image_omissions =
        source_image_count.saturating_sub(count_translated_input_images(&upstream_body));
    let _ = wire_budget_omissions;

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
    let mut wire_budget_omissions = 0_usize;
    {
        let protocol = candidate
            .provider
            .protocol_for_model(&candidate.upstream_model);
        let image_report = normalize_translated_image_history(
            &mut payload,
            protocol,
            candidate.provider.transport,
            candidate.provider.limits.max_request_bytes,
        )
        .await
        .map_err(|message| request_failure("image_normalization_failed", &message))?;
        wire_budget_omissions = image_report.omitted_images;
        scrub_kiro_omitted_images(&mut payload);
        if image_report.omitted_images > 0 {
            crate::debug::log(&format!(
                "kiro wire image history normalized: provider={} model={} omitted={} total={} kept_base64_chars={}",
                candidate.provider.id,
                candidate.upstream_model,
                image_report.omitted_images,
                image_report.inline_images,
                image_report.kept_base64_chars,
            ));
        }
    }
    let translation_image_omissions =
        source_image_count.saturating_sub(count_translated_input_images(&payload));
    let _ = wire_budget_omissions;
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
    let image_report = normalize_translated_image_history(
        &mut upstream_body,
        ProviderProtocol::ChatCompletions,
        candidate.provider.transport,
        candidate.provider.limits.max_request_bytes,
    )
    .await
    .map_err(|message| request_failure("image_normalization_failed", &message))?;
    if image_report.omitted_images > 0 {
        crate::debug::log(&format!(
            "copilot wire image history normalized: provider={} model={} omitted={} total={}",
            candidate.provider.id,
            candidate.upstream_model,
            image_report.omitted_images,
            image_report.inline_images,
        ));
    }
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
            routing_epoch: 0,
            routing_generation: 0,
            session_scope: None,
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
        let response = empty_completion_retry_failed_response(usage.clone(), 2)
            .expect("dedicated failure response");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let marker = response
            .extensions()
            .get::<EmptyCompletionRecoveryFailure>()
            .expect("recovery failure marker");
        assert_eq!(marker.usage.total_tokens, usage.total_tokens);
        assert_eq!(marker.additional_sends, 2);
        let retry = response
            .extensions()
            .get::<ProviderRetryObservation>()
            .expect("provider retry observation");
        assert_eq!(retry.additional_sends, 2);
        assert_eq!(retry.recovery_kinds, ["empty-completion", "empty-completion"]);
        assert_eq!(retry.usage.total_tokens, usage.total_tokens);
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
        let mut history = responses_reasoning_semantic_history(&first_reasoning);
        assert_eq!(history.len(), 1);
        assert!(history[0].get("id").is_none());
        history[0]["id"] = json!("rs_gateway");
        history[0]["status"] = json!("completed");
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
        shift_responses_retry_event(&mut retry_message, history.len(), &history);
        let lifecycle = canonical_reasoning_lifecycle_events(&history);
        assert_eq!(lifecycle[0]["output_index"], 0);
        assert_eq!(lifecycle[1]["item"]["id"], "rs_gateway");
        assert_eq!(retry_message["output_index"], 1);
        assert_eq!(retry_message.pointer("/item/id").and_then(Value::as_str), Some("msg_retry"));
    }

    #[test]
    fn responses_multi_retry_reasoning_keeps_every_attempt_in_order() {
        let mut history = Vec::new();
        for (index, text) in ["first", "second", "third"].into_iter().enumerate() {
            let event = json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": format!("provider_reasoning_{index}"),
                    "type": "reasoning",
                    "status": "completed",
                    "summary": [{"type": "summary_text", "text": text}],
                    "encrypted_content": format!("signature-{index}")
                }
            });
            let mut semantic = responses_reasoning_semantic_history(&event);
            semantic[0]["id"] = json!(format!("rs_gateway_{index}"));
            semantic[0]["status"] = json!("completed");
            history.append(&mut semantic);
        }
        let mut retry_message = json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "sequence_number": 0,
            "item": {"id": "msg_final", "type": "message", "status": "completed"}
        });
        shift_responses_retry_event(&mut retry_message, history.len(), &history);
        let mut sequence = 0;
        let mut lifecycle = canonical_reasoning_lifecycle_events(&history);
        for event in &mut lifecycle {
            assign_responses_sequence(event, &mut sequence);
        }
        assign_responses_sequence(&mut retry_message, &mut sequence);

        assert_eq!(lifecycle.len(), 6);
        assert_eq!(lifecycle[0]["item"]["summary"][0]["text"], "first");
        assert_eq!(lifecycle[2]["item"]["summary"][0]["text"], "second");
        assert_eq!(lifecycle[4]["item"]["summary"][0]["text"], "third");
        assert!(lifecycle.iter().all(|event| !event["item"]["id"]
            .as_str()
            .unwrap_or_default()
            .starts_with("provider_")));
        assert_eq!(retry_message["output_index"], 3);
        assert_eq!(retry_message["sequence_number"], 6);
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

        let mut gemini_usage = json!({"candidates": []});
        write_token_usage(
            &mut gemini_usage,
            TokenUsage {
                input_tokens: 10,
                output_tokens: 9,
                reasoning_tokens: 4,
                total_tokens: 19,
                ..TokenUsage::default()
            },
        );
        assert_eq!(gemini_usage["usageMetadata"]["candidatesTokenCount"], 5);
        assert_eq!(gemini_usage["usageMetadata"]["thoughtsTokenCount"], 4);

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
        for failed_chat in [
            json!({"choices": [{"delta": {}, "finish_reason": "error"}]}),
            json!({"choices": [{"delta": {}, "error": {"message": "failed"}}]}),
        ] {
            assert!(provider_stream_event_is_visible_failure(
                ProviderProtocol::ChatCompletions,
                &failed_chat,
            ));
            assert!(provider_stream_event_is_error(
                ProviderProtocol::ChatCompletions,
                &failed_chat,
            ));
            assert!(!provider_stream_event_is_empty_terminal(
                ProviderProtocol::ChatCompletions,
                &failed_chat,
            ));
        }

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
        let first = reserve_provider_start(&mut pacing, "provider", interval, now)
            .expect("first pacing reservation")
            .scheduled;
        let second = reserve_provider_start(&mut pacing, "provider", interval, now)
            .expect("second pacing reservation")
            .scheduled;
        assert_eq!(first, now);
        assert!(second.duration_since(first) >= interval);
    }

    #[test]
    fn disabled_pacing_and_different_providers_do_not_share_a_queue() {
        let mut pacing = ProviderPacingState::default();
        let now = Instant::now();
        assert_eq!(
            reserve_provider_start(&mut pacing, "off", Duration::ZERO, now)
                .expect("disabled pacing")
                .scheduled,
            now
        );
        assert!(pacing.queues.is_empty());
        assert_eq!(
            reserve_provider_start(&mut pacing, "one", Duration::from_secs(1), now)
                .expect("provider one")
                .scheduled,
            now
        );
        assert_eq!(
            reserve_provider_start(&mut pacing, "two", Duration::from_secs(1), now)
                .expect("provider two")
                .scheduled,
            now
        );
    }

    #[test]
    fn pacing_state_fails_closed_at_key_capacity_and_reclaims_expired_keys() {
        let mut pacing = ProviderPacingState::default();
        let now = Instant::now();
        for index in 0..MAX_PROVIDER_PACING_KEYS {
            let slot = reserve_provider_start(
                &mut pacing,
                &format!("provider-{index}"),
                Duration::from_secs(60),
                now,
            ).expect("bounded provider reservation");
            assert_eq!(slot.scheduled, now);
        }
        assert!(matches!(
            reserve_provider_start(
                &mut pacing,
                "overflow",
                Duration::from_secs(60),
                now,
            ),
            Err(ProviderPacingError::Overloaded(_))
        ));
        for queue in pacing.queues.values_mut() {
            queue.waiters.clear();
        }
        let fresh = reserve_provider_start(
            &mut pacing,
            "fresh",
            Duration::from_millis(1),
            now + Duration::from_secs(61),
        ).expect("expired queues are reclaimed");
        assert_eq!(fresh.scheduled, now + Duration::from_secs(61));
        assert_eq!(pacing.queues.len(), 1);
        assert!(pacing.queues.contains_key("fresh"));
    }

    #[test]
    fn pacing_key_capacity_retry_after_covers_all_waiters_and_final_retention() {
        let mut pacing = ProviderPacingState::default();
        let now = Instant::now();
        let interval = Duration::from_secs(2);
        for index in 0..MAX_PROVIDER_PACING_KEYS {
            let queue = pacing.queues.entry(format!("provider-{index}")).or_default();
            queue.last_started = Some(now - Duration::from_secs(1));
            queue.waiters.push_back(ProviderPacingWaiter {
                ticket: 1,
                queued_at: now,
                scheduled: now + Duration::from_secs(1),
                interval,
            });
            queue.waiters.push_back(ProviderPacingWaiter {
                ticket: 2,
                queued_at: now,
                scheduled: now + Duration::from_secs(3),
                interval,
            });
        }

        let retry_after = match reserve_provider_start(
            &mut pacing,
            "overflow",
            Duration::from_millis(1),
            now,
        ) {
            Err(ProviderPacingError::Overloaded(delay)) => delay,
            _ => panic!("expected key-capacity overload"),
        };
        assert_eq!(retry_after, Duration::from_secs(63));
        let failure = provider_pacing_failure(ProviderPacingError::Overloaded(retry_after));
        assert_eq!(
            failure
                .response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("63")
        );

        let reclaim_at = now + retry_after;
        for queue in pacing.queues.values_mut() {
            queue.waiters.clear();
            queue.last_started = Some(reclaim_at - MAX_PROVIDER_PACING_WAIT);
        }
        let admitted = reserve_provider_start(
            &mut pacing,
            "overflow",
            Duration::from_millis(1),
            reclaim_at,
        )
        .expect("advertised Retry-After permits a new pacing key");
        assert_eq!(admitted.scheduled, reclaim_at);
    }

    #[test]
    fn pacing_capacity_expiry_deadline_wakes_and_removes_waiters_as_advertised() {
        let mut pacing = ProviderPacingState::default();
        let now = Instant::now();
        let queued_at = now;
        let expires_at = queued_at + MAX_PROVIDER_PACING_WAIT;
        for index in 0..MAX_PROVIDER_PACING_KEYS {
            pacing
                .queues
                .entry(format!("provider-{index}"))
                .or_default()
                .waiters
                .push_back(ProviderPacingWaiter {
                    ticket: 1,
                    queued_at,
                    scheduled: now + Duration::from_secs(90),
                    interval: Duration::from_secs(30),
                });
        }

        let retry_after = match reserve_provider_start(
            &mut pacing,
            "overflow",
            Duration::from_millis(1),
            now,
        ) {
            Err(ProviderPacingError::Overloaded(delay)) => delay,
            _ => panic!("expected key-capacity overload"),
        };
        assert_eq!(retry_after, MAX_PROVIDER_PACING_WAIT);
        let first = pacing.queues.get("provider-0").expect("first queue");
        assert_eq!(
            provider_pacing_waiter_wake_at(first.waiters.front().expect("waiter")),
            expires_at
        );

        let keys = pacing.queues.keys().cloned().collect::<Vec<_>>();
        let generation = pacing.generation;
        for key in keys {
            remove_provider_pacing_ticket_at(
                &mut pacing,
                &key,
                1,
                generation,
                false,
                expires_at,
            );
        }
        assert!(pacing.queues.is_empty());
        let admitted = reserve_provider_start(
            &mut pacing,
            "overflow",
            Duration::from_millis(1),
            expires_at,
        )
        .expect("capacity is available at the advertised expiry deadline");
        assert_eq!(admitted.scheduled, expires_at);
    }

    #[test]
    fn pacing_queue_rejects_excessive_depth_or_age() {
        let mut pacing = ProviderPacingState::default();
        let now = Instant::now();
        for _ in 0..=MAX_PROVIDER_PACING_QUEUE_DEPTH {
            let result = reserve_provider_start(
                &mut pacing,
                "provider",
                Duration::from_secs(1),
                now,
            );
            if matches!(result, Err(ProviderPacingError::Overloaded(_))) {
                return;
            }
        }
        panic!("pacing queue did not reject an excessive burst");
    }

    #[test]
    fn cancelled_waiter_is_removed_without_consuming_its_start_slot() {
        let mut pacing = ProviderPacingState::default();
        let now = Instant::now();
        let interval = Duration::from_secs(1);
        let first = reserve_provider_start(&mut pacing, "provider", interval, now)
            .expect("first slot");
        let cancelled = reserve_provider_start(&mut pacing, "provider", interval, now)
            .expect("cancelled slot");
        let trailing = reserve_provider_start(&mut pacing, "provider", interval, now)
            .expect("trailing slot");
        assert_eq!(trailing.scheduled, now + Duration::from_secs(2));

        remove_provider_pacing_ticket(
            &mut pacing,
            "provider",
            cancelled.ticket,
            cancelled.generation,
            false,
        );
        let queue = pacing.queues.get("provider").expect("provider queue");
        assert_eq!(queue.waiters.len(), 2);
        assert_eq!(queue.waiters[0].ticket, first.ticket);
        assert_eq!(queue.waiters[1].ticket, trailing.ticket);
        assert_eq!(queue.waiters[1].scheduled, now + interval);
    }

    #[test]
    fn pacing_overload_is_a_local_429_with_retry_after_and_no_route_failover() {
        let failure = provider_pacing_failure(ProviderPacingError::Overloaded(
            Duration::from_secs(3),
        ));
        assert_eq!(failure.response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            failure
                .response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("3")
        );
        assert!(matches!(failure.kind, AttemptFailureKind::Request));
    }

    #[test]
    fn retry_after_uses_integer_ceiling_and_admission_reacquire_is_terminal_local() {
        assert_eq!(
            retry_after_seconds_ceil(Duration::from_millis(1_100)),
            2
        );
        assert_eq!(retry_after_seconds_ceil(Duration::ZERO), 1);
        let rounded = provider_pacing_failure(ProviderPacingError::Overloaded(
            Duration::from_millis(1_100),
        ));
        assert_eq!(
            rounded
                .response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("2")
        );
        let failure = admission_reacquire_failure(
            "gateway inflight request limit reached after provider pacing",
        );
        assert_eq!(failure.response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(matches!(failure.kind, AttemptFailureKind::Request));
    }

    #[tokio::test]
    async fn cancelled_waiter_is_removed_and_settings_generation_reclaims_queues() {
        let pacing = Arc::new(ProviderPacing::default());
        let now = Instant::now();
        let slot = {
            let mut state = pacing.state.lock().await;
            reserve_provider_start(&mut state, "provider", Duration::from_secs(1), now)
                .expect("queued pacing slot")
        };
        let reservation = ProviderPacingReservation {
            pacing: Arc::clone(&pacing),
            provider_id: "provider".into(),
            ticket: slot.ticket,
            generation: slot.generation,
            armed: true,
        };
        drop(reservation);
        assert!(pacing
            .state
            .lock()
            .await
            .queues
            .get("provider")
            .is_none_or(|queue| queue.waiters.is_empty()));

        {
            let mut state = pacing.state.lock().await;
            reserve_provider_start(
                &mut state,
                "removed-provider",
                Duration::from_secs(1),
                now,
            )
            .expect("provider queued before settings replacement");
        }
        let before = pacing.state.lock().await.generation;
        reset_provider_pacing(&pacing).await;
        let state = pacing.state.lock().await;
        assert_eq!(state.generation, before.wrapping_add(1));
        assert!(state.queues.is_empty());
    }
}

#[cfg(test)]
mod routing_attempt_lease_tests {
    use super::*;
    use crate::config::{ProviderCapabilities, ProviderDefinition};

    fn candidate(epoch: u64) -> RouteCandidate {
        RouteCandidate {
            provider: ProviderDefinition::default(),
            upstream_model: "model".into(),
            exposed_model: "model".into(),
            credential: None,
            account_id: None,
            target_key: "provider/model".into(),
            route_id: None,
            failure_threshold: 3,
            quota_threshold_percent: 90,
            input_price_per_million: None,
            output_price_per_million: None,
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            capabilities: ProviderCapabilities::default(),
            routing_epoch: epoch,
            routing_generation: 0,
            session_scope: None,
        }
    }

    fn response_with_body(body: reqwest::Body) -> reqwest::Response {
        axum::http::Response::builder()
            .status(StatusCode::OK)
            .body(body)
            .map(reqwest::Response::from)
            .expect("provider response")
    }

    async fn leased_response(
        body: reqwest::Body,
    ) -> (
        reqwest::Response,
        Arc<Mutex<RoutingRuntime>>,
        RouteCandidate,
    ) {
        let routing = Arc::new(Mutex::new(RoutingRuntime::default()));
        let epoch = routing.lock().await.epoch();
        let candidate = candidate(epoch);
        routing
            .lock()
            .await
            .begin_attempt(&candidate)
            .expect("attempt lease");
        let lease = RoutingAttemptLease(Arc::new(RoutingAttemptLeaseInner {
            routing: Arc::clone(&routing),
            candidate: candidate.clone(),
        }));
        let mut response = response_with_body(body);
        response.extensions_mut().insert(lease);
        (response, routing, candidate)
    }

    async fn assert_attempt_released(
        routing: &Arc<Mutex<RoutingRuntime>>,
        candidate: &RouteCandidate,
    ) {
        for _ in 0..16 {
            if routing.lock().await.active_attempts(candidate) == 0 {
                return;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(routing.lock().await.active_attempts(candidate), 0);
    }

    #[tokio::test]
    async fn buffered_response_and_bounded_reader_hold_attempt_until_body_eof() {
        let (response, routing, candidate) =
            leased_response(reqwest::Body::from("complete")).await;
        let buffered = buffer_provider_response(response, 1024)
            .await
            .expect("buffered provider response");
        assert_eq!(routing.lock().await.active_attempts(&candidate), 1);

        let rebuilt = buffered.into_response().expect("rebuilt response");
        assert_eq!(routing.lock().await.active_attempts(&candidate), 1);
        assert_eq!(
            read_bounded(rebuilt, 1024).await.expect("body"),
            b"complete".to_vec()
        );
        assert_attempt_released(&routing, &candidate).await;
    }

    #[tokio::test]
    async fn cancelling_guarded_body_releases_attempt_lease() {
        let pending = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();
        let (response, routing, candidate) =
            leased_response(reqwest::Body::wrap_stream(pending)).await;
        let guarded = provider_guard_stream(response, false);
        assert_eq!(routing.lock().await.active_attempts(&candidate), 1);
        drop(guarded);
        assert_attempt_released(&routing, &candidate).await;
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

    #[test]
    fn same_key_wait_is_capped_without_shortening_routing_cooldown() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("7200"));

        assert_eq!(
            rate_limit_retry_delay(&headers, 0),
            Duration::from_secs(10 * 60)
        );
        assert_eq!(
            validated_retry_after(&headers).and_then(|value| value.1),
            Some(Duration::from_secs(7_200))
        );
    }
}
