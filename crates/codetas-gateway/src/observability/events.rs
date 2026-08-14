use super::*;

pub fn read_recent_observability_events(
    directory: impl AsRef<Path>,
    since_ms: u64,
    limit: usize,
) -> Vec<ObservationEvent> {
    let mut files = event_files(directory.as_ref()).unwrap_or_default();
    files.sort_by(|left, right| right.name.cmp(&left.name));
    let mut events = Vec::new();
    for event_file in files {
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
            if event.timestamp_ms < since_ms {
                return events;
            }
            events.push(event);
            if events.len() >= limit.min(500) {
                return events;
            }
        }
    }
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
    let mut scanned = 0_usize;
    let mut truncated = false;

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
            if event.timestamp_ms < since_ms {
                break 'files;
            }
            if scanned >= limit {
                truncated = true;
                break 'files;
            }
            scanned += 1;
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
