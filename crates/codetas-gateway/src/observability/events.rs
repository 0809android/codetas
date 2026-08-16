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
        right.timestamp_ms.cmp(&left.timestamp_ms)
            .then_with(|| right.ledger_sequence.cmp(&left.ledger_sequence))
    });
    events.truncate(limit);
    events
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
        right.timestamp_ms.cmp(&left.timestamp_ms)
            .then_with(|| right.ledger_sequence.cmp(&left.ledger_sequence))
    });
    let truncated = scan_truncated || candidates.len() > limit;
    candidates.truncate(limit);
    let scanned = candidates.len();
    for event in candidates {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    fn event(timestamp_ms: u64, request_id: &str) -> ObservationEvent {
        ObservationEvent {
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
        assert_eq!(recent.iter().map(|event| event.request_id.as_str()).collect::<Vec<_>>(), vec!["newer-timestamp"]);
        let breakdown = read_observability_breakdown(&directory, 2_000, 10);
        assert_eq!(breakdown.scanned_events, 1);
        assert_eq!(breakdown.providers[0].requests, 1);
        let _ = fs::remove_dir_all(directory);
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
