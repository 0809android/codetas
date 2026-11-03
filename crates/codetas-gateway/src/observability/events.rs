use super::*;

pub fn read_recent_observability_events(
    directory: impl AsRef<Path>,
    since_ms: u64,
    limit: usize,
) -> Vec<ObservationEvent> {
    let limit = limit.clamp(1, 500);
    let scan_limit = limit.saturating_mul(16).clamp(4_096, 50_000);
    let mut files = event_files(directory.as_ref()).unwrap_or_default();
    files.sort_by(|left, right| right.name.cmp(&left.name));
    let mut events = Vec::new();
    'scan: for event_file in files {
        let Ok(bytes) = fs::read(event_file.path) else {
            continue;
        };
        for line in bytes.split(|byte| *byte == b'\n').rev() {
            if line.is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_slice::<ObservationEvent>(line) else {
                continue;
            };
            events.push(event);
            if events.len() >= scan_limit {
                break 'scan;
            }
        }
    }
    events.retain(|event| event.timestamp_ms >= since_ms);
    events.sort_by(|left, right| {
        right
            .timestamp_ms
            .cmp(&left.timestamp_ms)
            .then_with(|| right.ledger_sequence.cmp(&left.ledger_sequence))
    });
    events.truncate(limit);
    events
}

pub fn read_observability_events_after_sequence(
    directory: impl AsRef<Path>,
    since_ms: u64,
    after_sequence: u64,
    limit: usize,
) -> Vec<ObservationEvent> {
    let limit = limit.clamp(1, 500);
    let mut files = event_files(directory.as_ref()).unwrap_or_default();
    files.sort_by(|left, right| left.name.cmp(&right.name));
    let mut events = BTreeMap::<u64, ObservationEvent>::new();
    for event_file in files {
        let Ok(bytes) = fs::read(event_file.path) else {
            continue;
        };
        for line in bytes.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_slice::<ObservationEvent>(line) else {
                continue;
            };
            if event.timestamp_ms >= since_ms && event.ledger_sequence > after_sequence {
                events.insert(event.ledger_sequence, event);
                if events.len() > limit {
                    let _ = events.pop_last();
                }
            }
        }
    }
    events.into_values().collect()
}

pub fn read_observability_breakdown(
    directory: impl AsRef<Path>,
    since_ms: u64,
    max_events: usize,
) -> ObservabilityBreakdown {
    let limit = max_events.clamp(1, 50_000);
    let mut files = event_files(directory.as_ref()).unwrap_or_default();
    files.sort_by(|left, right| right.name.cmp(&left.name));
    let mut daily = BTreeMap::<String, ObservabilityBreakdownRow>::new();
    let mut providers = BTreeMap::<String, ObservabilityBreakdownRow>::new();
    let mut models = BTreeMap::<String, ObservabilityBreakdownRow>::new();
    let mut surfaces = BTreeMap::<String, ObservabilityBreakdownRow>::new();
    let mut model_accounts =
        BTreeMap::<(String, String, Option<String>), ModelAccountUsageRow>::new();
    let scan_limit = limit.saturating_mul(4).clamp(limit, 200_000);
    let mut candidates = Vec::new();
    let mut scan_truncated = false;

    'files: for event_file in files {
        let Ok(bytes) = fs::read(event_file.path) else {
            continue;
        };
        for line in bytes.split(|byte| *byte == b'\n').rev() {
            if line.is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_slice::<ObservationEvent>(line) else {
                continue;
            };
            candidates.push(event);
            if candidates.len() >= scan_limit {
                scan_truncated = true;
                break 'files;
            }
        }
    }
    candidates.retain(|event| event.timestamp_ms >= since_ms);
    candidates.sort_by(|left, right| {
        right
            .timestamp_ms
            .cmp(&left.timestamp_ms)
            .then_with(|| right.ledger_sequence.cmp(&left.ledger_sequence))
    });
    let truncated = scan_truncated || candidates.len() > limit;
    candidates.truncate(limit);
    let scanned = candidates.len();
    for event in candidates {
        // Attribute only actual, foreground request records. Never assign an
        // unscoped historical request to whichever account happens to be main now.
        if !event.attempt_only && !event.shadow && !event.usage_suppressed {
            if let (Some(provider), Some(model)) = (&event.provider_id, &event.upstream_model) {
                let key = (provider.clone(), model.clone(), event.account_id.clone());
                let row = model_accounts
                    .entry(key)
                    .or_insert_with(|| ModelAccountUsageRow {
                        provider_id: provider.clone(),
                        model_id: model.clone(),
                        account_id: event.account_id.clone(),
                        ..ModelAccountUsageRow::default()
                    });
                row.usage.include(&event);
                row.last_event_at_ms = row.last_event_at_ms.max(event.timestamp_ms);
                if event.usage.total_tokens > 0
                    || event.usage.input_tokens > 0
                    || event.usage.output_tokens > 0
                {
                    row.measured_requests += 1;
                }
                if event.estimated_cost_usd.is_some() {
                    row.costed_requests += 1;
                }
            }
        }
        include_breakdown(
            &mut daily,
            (event.timestamp_ms / MILLIS_PER_DAY).to_string(),
            &event,
        );
        include_breakdown(
            &mut providers,
            event
                .provider_id
                .clone()
                .unwrap_or_else(|| "unrouted".into()),
            &event,
        );
        include_breakdown(&mut models, event.exposed_model.clone(), &event);
        include_breakdown(&mut surfaces, observation_surface(&event), &event);
    }

    let mut daily = daily.into_values().collect::<Vec<_>>();
    daily.sort_by(|left, right| left.key.cmp(&right.key));
    ObservabilityBreakdown {
        generated_at_ms: ObservationEvent::now_ms(),
        since_ms,
        scanned_events: scanned as u64,
        truncated,
        daily,
        providers: sorted_breakdown(providers),
        models: sorted_breakdown(models),
        surfaces: sorted_breakdown(surfaces),
        model_accounts: model_accounts.into_values().collect(),
        quotas: Vec::new(),
    }
}

fn include_breakdown(
    values: &mut BTreeMap<String, ObservabilityBreakdownRow>,
    key: String,
    event: &ObservationEvent,
) {
    let row = values
        .entry(key.clone())
        .or_insert_with(|| ObservabilityBreakdownRow {
            key,
            ..ObservabilityBreakdownRow::default()
        });
    row.include(event);
}

fn sorted_breakdown(
    values: BTreeMap<String, ObservabilityBreakdownRow>,
) -> Vec<ObservabilityBreakdownRow> {
    let mut values = values.into_values().collect::<Vec<_>>();
    values.sort_by(|left, right| {
        right
            .requests
            .cmp(&left.requests)
            .then_with(|| left.key.cmp(&right.key))
    });
    values
}

fn observation_surface(event: &ObservationEvent) -> String {
    if event.shadow {
        return "shadow".into();
    }
    if let Some(surface) = event.exposed_model.strip_prefix("codetas-sidecar/") {
        return surface.to_string();
    }
    if event.route_id.as_deref() == Some("codetas-helper-intercept") {
        return "helper".into();
    }
    "responses".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    fn event(timestamp_ms: u64, request_id: &str) -> ObservationEvent {
        ObservationEvent {
            usage_suppressed: false,
            ledger_sequence: 0,
            timestamp_ms,
            request_id: request_id.into(),
            provider_id: Some("provider".into()),
            upstream_model: Some("model".into()),
            upstream_endpoint: None,
            exposed_model: "provider/model".into(),
            route_id: None,
            account_id: None,
            status_code: 200,
            outcome: "success".into(),
            failure_category: None,
            upstream_error: None,
            request_diagnostic: None,
            client_thread_id: None,
            cache_observation: None,
            latency_ms: 1,
            attempts: 1,
            candidate_ordinal: 1,
            send_count: 1,
            recovery_kinds: Vec::new(),
            recovery_kind: None,
            attempt_only: false,
            streaming: false,
            usage: TokenUsage::default(),
            estimated_cost_usd: None,
            shadow: false,
            shadow_rule_id: None,
            parent_request_id: None,
        }
    }

    #[test]
    fn model_account_usage_isolated_by_actual_target_and_account() {
        let directory = tempfile::tempdir().unwrap();
        let settings = ObservabilitySettings::default();
        for (index, (provider, model, account)) in [
            ("one", "model", Some("a")),
            ("one", "model", Some("b")),
            ("one", "model", Some("a")),
            ("two", "model", Some("a")),
            ("one", "model", None),
            ("one", "different", Some("a")),
        ]
        .into_iter()
        .enumerate()
        {
            let mut item = event(1_000 + index as u64, &format!("request-{index}"));
            item.provider_id = Some(provider.into());
            item.upstream_model = Some(model.into());
            item.account_id = account.map(str::to_string);
            item.exposed_model = "same-route".into();
            item.usage.input_tokens = 100;
            item.usage.output_tokens = 20;
            item.usage.total_tokens = 120;
            if index == 0 {
                item.estimated_cost_usd = Some(0.01);
            }
            persist(directory.path(), &item, &settings).unwrap();
        }
        for kind in ["attempt", "shadow", "suppressed"] {
            let mut item = event(2_000, kind);
            item.provider_id = Some("one".into());
            item.account_id = Some("a".into());
            item.usage.total_tokens = 9_999;
            item.attempt_only = kind == "attempt";
            item.shadow = kind == "shadow";
            item.usage_suppressed = kind == "suppressed";
            persist(directory.path(), &item, &settings).unwrap();
        }
        let snapshot = read_observability_breakdown(directory.path(), 0, 50_000);
        assert_eq!(snapshot.model_accounts.len(), 5);
        let row = snapshot
            .model_accounts
            .iter()
            .find(|row| {
                row.provider_id == "one"
                    && row.model_id == "model"
                    && row.account_id.as_deref() == Some("a")
            })
            .unwrap();
        assert_eq!(row.usage.requests, 2);
        assert_eq!(row.usage.total_tokens, 240);
        assert_eq!(row.measured_requests, 2);
        assert_eq!(row.costed_requests, 1);
        assert_eq!(row.usage.estimated_cost_usd, 0.01);
        assert_eq!(row.last_event_at_ms, 1_002);
        assert!(snapshot
            .model_accounts
            .iter()
            .any(|row| row.account_id.is_none()));
    }

    #[test]
    fn readers_filter_after_bounded_scan_when_timestamps_are_out_of_order() {
        let directory = std::env::temp_dir().join(format!(
            "codetas-observability-order-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed),
        ));
        let settings = ObservabilitySettings::default();
        persist(&directory, &event(3_000, "newer-timestamp"), &settings).expect("persist newer");
        persist(&directory, &event(1_000, "later-append"), &settings).expect("persist older");

        let recent = read_recent_observability_events(&directory, 2_000, 10);
        assert_eq!(
            recent
                .iter()
                .map(|event| event.request_id.as_str())
                .collect::<Vec<_>>(),
            vec!["newer-timestamp"]
        );
        let breakdown = read_observability_breakdown(&directory, 2_000, 10);
        assert_eq!(breakdown.scanned_events, 1);
        assert_eq!(breakdown.providers[0].requests, 1);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn sequence_reader_preserves_same_millisecond_attempts_and_terminal_event() {
        let directory = std::env::temp_dir().join(format!(
            "codetas-observability-sequence-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed),
        ));
        let settings = ObservabilitySettings::default();
        for (sequence, request_id) in [(41, "attempt-a"), (42, "attempt-b"), (43, "final")] {
            let mut item = event(1_000, request_id);
            item.ledger_sequence = sequence;
            persist(&directory, &item, &settings).expect("persist sequenced event");
        }

        let events = read_observability_events_after_sequence(&directory, 1_000, 40, 500);
        assert_eq!(
            events
                .iter()
                .map(|event| (event.timestamp_ms, event.ledger_sequence))
                .collect::<Vec<_>>(),
            vec![(1_000, 41), (1_000, 42), (1_000, 43)]
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn sequence_reader_pages_from_the_oldest_unread_event_beyond_fifty_thousand() {
        let directory = std::env::temp_dir().join(format!(
            "codetas-observability-backlog-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&directory).expect("create backlog directory");
        let mut file = fs::File::create(directory.join("events-0-000000.jsonl"))
            .expect("create event segment");
        for sequence in 1..=50_002_u64 {
            let mut item = event(1_000, &format!("request-{sequence}"));
            item.ledger_sequence = sequence;
            serde_json::to_writer(&mut file, &item).expect("write event");
            file.write_all(b"\n").expect("write delimiter");
        }
        drop(file);

        let first_page = read_observability_events_after_sequence(&directory, 1_000, 1, 2);
        assert_eq!(
            first_page
                .iter()
                .map(|event| event.ledger_sequence)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        let second_page = read_observability_events_after_sequence(&directory, 1_000, 3, 2);
        assert_eq!(
            second_page
                .iter()
                .map(|event| event.ledger_sequence)
                .collect::<Vec<_>>(),
            vec![4, 5]
        );
        let _ = fs::remove_dir_all(directory);
    }
}
