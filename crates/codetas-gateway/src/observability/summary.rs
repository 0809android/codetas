use super::cleanup::{ensure_real_directory, write_new_secure_file};
use super::*;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
};

fn load_summary(directory: &Path) -> ObservabilitySummary {
    let mut summary = ObservabilitySummary {
        storage_path: Some(directory.to_string_lossy().into_owned()),
        ..ObservabilitySummary::default()
    };
    let mut files = event_files(directory).unwrap_or_default();
    files.sort_by(|left, right| left.name.cmp(&right.name));
    for event_file in files {
        summary.storage_bytes = summary.storage_bytes.saturating_add(event_file.bytes);
        let Ok(file) = File::open(event_file.path) else {
            continue;
        };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if let Ok(event) = serde_json::from_str::<ObservationEvent>(&line) {
                summary.include(&event);
            }
        }
    }
    summary
}

pub(crate) fn empty_persistent_summary() -> PersistentSummary {
    PersistentSummary {
        version: SUMMARY_VERSION,
        summary: ObservabilitySummary::default(),
        last_sequence: 0,
        checkpoints: BTreeMap::new(),
    }
}

pub(crate) fn load_persistent_state(directory: &Path) -> PersistentSummary {
    let path = directory.join(SUMMARY_FILE);
    let persisted = fs::symlink_metadata(&path)
        .ok()
        .filter(|metadata| metadata.file_type().is_file() && !metadata.file_type().is_symlink())
        .filter(|metadata| metadata.len() <= 64 * 1024)
        .and_then(|_| fs::read(&path).ok())
        .and_then(|bytes| serde_json::from_slice::<PersistentSummary>(&bytes).ok());
    let mut state = match persisted {
        Some(mut state) if state.version == SUMMARY_VERSION => {
            reconcile_uncheckpointed_events(directory, &mut state);
            state
        }
        Some(mut legacy) if legacy.version == 1 => {
            // V1 already contains lifetime totals but no read offsets. Snapshot every current
            // segment at its present size so migration preserves pruned history without replaying
            // retained events a second time.
            legacy.version = SUMMARY_VERSION;
            legacy.last_sequence = max_ledger_sequence(directory);
            legacy.checkpoints = current_checkpoints(directory);
            legacy
        }
        _ => {
            let mut state = empty_persistent_summary();
            state.summary = load_summary(directory);
            state.last_sequence = max_ledger_sequence(directory);
            state.checkpoints = current_checkpoints(directory);
            state
        }
    };
    state.version = SUMMARY_VERSION;
    state.summary.storage_path = Some(directory.to_string_lossy().into_owned());
    state.summary.storage_bytes = active_storage_bytes(directory);
    compact_checkpoints(&mut state.checkpoints);
    state
}

fn reconcile_uncheckpointed_events(directory: &Path, state: &mut PersistentSummary) {
    let mut files = event_files(directory).unwrap_or_default();
    files.sort_by(|left, right| left.name.cmp(&right.name));
    let checkpoint_sequence = state.last_sequence;
    let mut highest_sequence = state.last_sequence;
    for event_file in files {
        let Ok(mut file) = File::open(&event_file.path) else {
            continue;
        };
        let mut bytes = Vec::new();
        if std::io::Read::read_to_end(&mut file, &mut bytes).is_err() {
            continue;
        }
        let complete = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        for line in bytes[..complete].split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_slice::<ObservationEvent>(line) else {
                continue;
            };
            // Scan the retained, capacity-bounded ledger against one fixed sequence. A file
            // name can be reused after retention removes its old segment, so a byte offset by
            // itself is never treated as file identity.
            if event.ledger_sequence > checkpoint_sequence {
                state.summary.include(&event);
            }
            highest_sequence = highest_sequence.max(event.ledger_sequence);
        }
        state.checkpoints.insert(event_file.name, complete as u64);
    }
    state.last_sequence = highest_sequence;
}

fn current_checkpoints(directory: &Path) -> BTreeMap<String, u64> {
    event_files(directory)
        .unwrap_or_default()
        .into_iter()
        .map(|file| (file.name, file.bytes))
        .collect()
}

pub(crate) fn repair_incomplete_event_tails(directory: &Path) -> Result<(), String> {
    for event_file in event_files(directory)? {
        if event_file.bytes == 0 {
            continue;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&event_file.path)
            .map_err(|error| format!("failed to inspect observability segment tail: {error}"))?;
        file.seek(SeekFrom::End(-1))
            .map_err(|error| format!("failed to inspect observability segment tail: {error}"))?;
        let mut last = [0_u8; 1];
        std::io::Read::read_exact(&mut file, &mut last)
            .map_err(|error| format!("failed to inspect observability segment tail: {error}"))?;
        if last[0] == b'\n' {
            continue;
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|error| format!("failed to repair observability segment tail: {error}"))?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes)
            .map_err(|error| format!("failed to repair observability segment tail: {error}"))?;
        let complete = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        file.set_len(complete as u64)
            .and_then(|_| file.sync_data())
            .map_err(|error| format!("failed to repair observability segment tail: {error}"))?;
    }
    Ok(())
}

fn max_ledger_sequence(directory: &Path) -> u64 {
    let mut maximum = 0;
    for event_file in event_files(directory).unwrap_or_default() {
        let Ok(file) = File::open(event_file.path) else {
            continue;
        };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if let Ok(event) = serde_json::from_str::<ObservationEvent>(&line) {
                maximum = maximum.max(event.ledger_sequence);
            }
        }
    }
    maximum
}

pub(crate) fn compact_checkpoints(checkpoints: &mut BTreeMap<String, u64>) {
    while checkpoints.len() > MAX_SUMMARY_CHECKPOINTS {
        let Some(oldest) = checkpoints.keys().next().cloned() else {
            break;
        };
        checkpoints.remove(&oldest);
    }
}

pub(crate) fn active_storage_bytes(directory: &Path) -> u64 {
    event_files(directory)
        .unwrap_or_default()
        .into_iter()
        .fold(0_u64, |total, file| total.saturating_add(file.bytes))
}

pub(crate) fn persist_summary(directory: &Path, state: &PersistentSummary) -> Result<(), String> {
    ensure_real_directory(directory)?;
    let path = directory.join(SUMMARY_FILE);
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err("observability summary path is not a regular owned file".into());
        }
    }
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|error| format!("failed to encode observability summary: {error}"))?;
    if bytes.len() > 64 * 1024 {
        return Err("observability summary exceeded 64 KiB".into());
    }
    let temporary = directory.join(format!(
        ".usage-totals-{}-{}.tmp",
        std::process::id(),
        ObservationEvent::now_ms()
    ));
    let _ = fs::remove_file(&temporary);
    write_new_secure_file(&temporary, &bytes)?;
    if let Err(error) = replace_summary_file(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("failed to replace observability summary: {error}"));
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_summary_file(temporary: &Path, path: &Path) -> std::io::Result<()> {
    fs::rename(temporary, path)
}

#[cfg(windows)]
fn replace_summary_file(temporary: &Path, path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return fs::rename(temporary, path);
    }
    let backup = path.with_extension(format!("codetas-summary-{}.bak", std::process::id()));
    let _ = fs::remove_file(&backup);
    fs::rename(path, &backup)?;
    match fs::rename(temporary, path) {
        Ok(()) => {
            let _ = fs::remove_file(backup);
            Ok(())
        }
        Err(error) => {
            let _ = fs::rename(&backup, path);
            Err(error)
        }
    }
}

#[cfg(unix)]
pub(crate) fn secure_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(unix)]
pub(crate) fn secure_directory(directory: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("failed to secure observability directory: {error}"))
}

#[cfg(not(unix))]
pub(crate) fn secure_mode(_options: &mut OpenOptions) {}

#[cfg(not(unix))]
pub(crate) fn secure_directory(_directory: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);
    use serde_json::json;

    #[test]
    fn reads_responses_and_provider_usage_shapes() {
        let responses = TokenUsage::from_json(&json!({
            "usage": {
                "input_tokens": 12,
                "output_tokens": 7,
                "total_tokens": 19,
                "input_tokens_details": {"cached_tokens": 4},
                "output_tokens_details": {"reasoning_tokens": 3}
            }
        }));
        assert_eq!(responses.input_tokens, 12);
        assert_eq!(responses.output_tokens, 7);
        assert_eq!(responses.cached_input_tokens, 4);
        assert_eq!(responses.reasoning_tokens, 3);

        let gemini = TokenUsage::from_json(&json!({
            "usageMetadata": {
                "promptTokenCount": 8,
                "candidatesTokenCount": 5,
                "thoughtsTokenCount": 2,
                "cachedContentTokenCount": 3,
                "totalTokenCount": 15
            }
        }));
        assert_eq!(gemini.input_tokens, 8);
        assert_eq!(gemini.output_tokens, 7);
        assert_eq!(gemini.cached_input_tokens, 3);
        assert_eq!(gemini.total_tokens, 15);
    }

    #[test]
    fn persisted_summary_contains_metadata_only() {
        let directory = std::env::temp_dir().join(format!(
            "codetas-observability-test-{}-{}-{}",
            std::process::id(),
            ObservationEvent::now_ms(),
            NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        let event = ObservationEvent {
            ledger_sequence: 0,
            timestamp_ms: ObservationEvent::now_ms(),
            request_id: "request-test".into(),
            provider_id: Some("provider".into()),
            upstream_model: Some("model".into()),
            upstream_endpoint: Some("/v1/images/generations".into()),
            exposed_model: "provider/model".into(),
            route_id: None,
            account_id: None,
            status_code: 200,
            outcome: "success".into(),
            failure_category: None,
            latency_ms: 42,
            attempts: 1,
            candidate_ordinal: 1,
            send_count: 2,
            recovery_kinds: vec!["empty-completion".into()],
            recovery_kind: Some("empty-completion".into()),
            attempt_only: false,
            streaming: false,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: 2,
                reasoning_tokens: 1,
                total_tokens: 15,
            },
            estimated_cost_usd: Some(0.01),
            shadow: false,
            shadow_rule_id: None,
            parent_request_id: None,
        };
        persist(&directory, &event, &ObservabilitySettings::default()).unwrap();
        let summary = read_observability_summary(&directory);
        assert_eq!(summary.total_requests, 1);
        assert_eq!(summary.total_tokens, 15);
        assert_eq!(summary.estimated_cost_usd, 0.01);

        let files = event_files(&directory).unwrap();
        let contents = fs::read_to_string(&files[0].path).unwrap();
        assert!(!contents.contains("prompt"));
        assert!(!contents.contains("authorization"));
        assert!(contents.contains("\"recoveryKind\":\"empty-completion\""));
        assert!(contents.contains("\"candidateOrdinal\":1"));
        assert!(contents.contains("\"sendCount\":2"));
        assert!(contents.contains("\"recoveryKinds\":[\"empty-completion\"]"));

        let mut failover = event.clone();
        failover.provider_id = Some("provider-fallback".into());
        failover.candidate_ordinal = 2;
        failover.attempt_only = true;
        failover.send_count = 1;
        failover.recovery_kinds.clear();
        failover.recovery_kind = None;
        persist(&directory, &failover, &ObservabilitySettings::default()).unwrap();
        let attempts = event_files(&directory).unwrap();
        assert_eq!(attempts.len(), 2);
        assert!(attempts.iter().all(|attempt| {
            fs::read_to_string(&attempt.path)
                .is_ok_and(|contents| contents.contains("\"requestId\":\"request-test\""))
        }));
        let summary = read_observability_summary(&directory);
        assert_eq!(summary.total_requests, 1, "candidate attempts are not requests");
        let _ = fs::remove_dir_all(directory);
    }
}
