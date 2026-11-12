use super::*;

pub(super) struct ProviderPacingReservation {
    pub(super) pacing: Arc<ProviderPacing>,
    pub(super) provider_id: String,
    pub(super) ticket: u64,
    pub(super) generation: u64,
    pub(super) armed: bool,
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

pub(super) fn remove_provider_pacing_ticket_at(
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
        if let Some(index) = queue
            .waiters
            .iter()
            .position(|waiter| waiter.ticket == ticket)
        {
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
            next += waiter.interval;
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

pub(super) async fn await_provider_pacing(
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
            pacing
                .queues
                .get(&candidate.provider.id)
                .and_then(|queue| {
                    queue
                        .waiters
                        .iter()
                        .find(|waiter| waiter.ticket == slot.ticket)
                })
                .map(|waiter| (waiter.queued_at, provider_pacing_waiter_wake_at(waiter)))
        };
        let Some((queued_at, wake_at)) = waiter else {
            return Err(provider_pacing_failure(
                ProviderPacingError::SettingsChanged,
            ));
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

pub(super) fn provider_pacing_waiter_wake_at(waiter: &ProviderPacingWaiter) -> Instant {
    waiter
        .scheduled
        .min(waiter.queued_at + MAX_PROVIDER_PACING_WAIT)
}

pub(super) fn admission_reacquire_failure(message: &'static str) -> AttemptFailure {
    AttemptFailure {
        response: error_response(StatusCode::SERVICE_UNAVAILABLE, "gateway_capacity", message),
        // Local admission reacquire failure is terminal for this client
        // request. No alternate candidate may send after the reservation has
        // been released, and provider cooldown state must remain untouched.
        kind: AttemptFailureKind::Request,
    }
}

pub(super) fn provider_pacing_failure(error: ProviderPacingError) -> AttemptFailure {
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

fn provider_pacing_queue_reclaim_at(queue: &ProviderPacingQueue, now: Instant) -> Option<Instant> {
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
