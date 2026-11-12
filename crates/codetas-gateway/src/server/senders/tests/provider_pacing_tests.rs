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
        )
        .expect("bounded provider reservation");
        assert_eq!(slot.scheduled, now);
    }
    assert!(matches!(
        reserve_provider_start(&mut pacing, "overflow", Duration::from_secs(60), now,),
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
    )
    .expect("expired queues are reclaimed");
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
        let queue = pacing
            .queues
            .entry(format!("provider-{index}"))
            .or_default();
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

    let retry_after =
        match reserve_provider_start(&mut pacing, "overflow", Duration::from_millis(1), now) {
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

    let retry_after =
        match reserve_provider_start(&mut pacing, "overflow", Duration::from_millis(1), now) {
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
        remove_provider_pacing_ticket_at(&mut pacing, &key, 1, generation, false, expires_at);
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
        let result = reserve_provider_start(&mut pacing, "provider", Duration::from_secs(1), now);
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
    let first = reserve_provider_start(&mut pacing, "provider", interval, now).expect("first slot");
    let cancelled =
        reserve_provider_start(&mut pacing, "provider", interval, now).expect("cancelled slot");
    let trailing =
        reserve_provider_start(&mut pacing, "provider", interval, now).expect("trailing slot");
    assert_eq!(trailing.scheduled, now + Duration::from_secs(2));

    // Use the explicit clock: the wall-clock variant reschedules from
    // `Instant::now()`, which is a few microseconds after `now`.
    remove_provider_pacing_ticket_at(
        &mut pacing,
        "provider",
        cancelled.ticket,
        cancelled.generation,
        false,
        now,
    );
    let queue = pacing.queues.get("provider").expect("provider queue");
    assert_eq!(queue.waiters.len(), 2);
    assert_eq!(queue.waiters[0].ticket, first.ticket);
    assert_eq!(queue.waiters[1].ticket, trailing.ticket);
    assert_eq!(queue.waiters[1].scheduled, now + interval);
}

#[test]
fn pacing_overload_is_a_local_429_with_retry_after_and_no_route_failover() {
    let failure = provider_pacing_failure(ProviderPacingError::Overloaded(Duration::from_secs(3)));
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
    assert_eq!(retry_after_seconds_ceil(Duration::from_millis(1_100)), 2);
    assert_eq!(retry_after_seconds_ceil(Duration::ZERO), 1);
    let rounded = provider_pacing_failure(ProviderPacingError::Overloaded(Duration::from_millis(
        1_100,
    )));
    assert_eq!(
        rounded
            .response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
    let failure =
        admission_reacquire_failure("gateway inflight request limit reached after provider pacing");
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
        reserve_provider_start(&mut state, "removed-provider", Duration::from_secs(1), now)
            .expect("provider queued before settings replacement");
    }
    let before = pacing.state.lock().await.generation;
    reset_provider_pacing(&pacing).await;
    let state = pacing.state.lock().await;
    assert_eq!(state.generation, before.wrapping_add(1));
    assert!(state.queues.is_empty());
}
