use crate::config::ObservabilitySettings;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

const MILLIS_PER_DAY: u64 = 86_400_000;
const MAX_SEGMENT_BYTES: u64 = 1024 * 1024;
const SUMMARY_FILE: &str = "usage-totals.json";
const SUMMARY_VERSION: u8 = 2;
const MAX_SUMMARY_CHECKPOINTS: usize = 512;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
}

impl TokenUsage {
    pub(crate) fn from_json(value: &Value) -> Self {
        if let Some(usage) = value
            .get("usageMetadata")
            .or_else(|| value.pointer("/response/usageMetadata"))
        {
            let input = number(usage, &["/promptTokenCount"]);
            let reasoning = number(usage, &["/thoughtsTokenCount"]);
            let visible_output = number(usage, &["/candidatesTokenCount"]);
            let output = visible_output.saturating_add(reasoning);
            return Self {
                input_tokens: input,
                output_tokens: output,
                cached_input_tokens: number(usage, &["/cachedContentTokenCount"]),
                reasoning_tokens: reasoning,
                total_tokens: number(usage, &["/totalTokenCount"])
                    .max(input.saturating_add(output)),
            };
        }

        let Some(usage) = value
            .get("usage")
            .or_else(|| value.pointer("/response/usage"))
            .or_else(|| value.pointer("/message/usage"))
        else {
            return Self::default();
        };
        let input = number(usage, &["/input_tokens", "/prompt_tokens"]);
        let output = number(usage, &["/output_tokens", "/completion_tokens"]);
        let cached = number(
            usage,
            &[
                "/input_tokens_details/cached_tokens",
                "/prompt_tokens_details/cached_tokens",
                "/cache_read_input_tokens",
            ],
        );
        let reasoning = number(
            usage,
            &[
                "/output_tokens_details/reasoning_tokens",
                "/completion_tokens_details/reasoning_tokens",
            ],
        );
        Self {
            input_tokens: input,
            output_tokens: output,
            cached_input_tokens: cached,
            reasoning_tokens: reasoning,
            total_tokens: number(usage, &["/total_tokens"]).max(input.saturating_add(output)),
        }
    }

    pub(crate) fn merge_max(&mut self, next: Self) {
        self.input_tokens = self.input_tokens.max(next.input_tokens);
        self.output_tokens = self.output_tokens.max(next.output_tokens);
        self.cached_input_tokens = self.cached_input_tokens.max(next.cached_input_tokens);
        self.reasoning_tokens = self.reasoning_tokens.max(next.reasoning_tokens);
        self.total_tokens = self.total_tokens.max(next.total_tokens);
    }
}

fn number(value: &Value, pointers: &[&str]) -> u64 {
    pointers
        .iter()
        .find_map(|pointer| value.pointer(pointer).and_then(Value::as_u64))
        .unwrap_or(0)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservationEvent {
    #[serde(default)]
    pub ledger_sequence: u64,
    pub timestamp_ms: u64,
    pub request_id: String,
    pub provider_id: Option<String>,
    pub upstream_model: Option<String>,
    pub exposed_model: String,
    pub route_id: Option<String>,
    pub account_id: Option<String>,
    pub status_code: u16,
    pub outcome: String,
    pub failure_category: Option<String>,
    pub latency_ms: u64,
    pub attempts: u16,
    pub streaming: bool,
    #[serde(flatten)]
    pub usage: TokenUsage,
    pub estimated_cost_usd: Option<f64>,
    #[serde(default)]
    pub shadow: bool,
    pub shadow_rule_id: Option<String>,
    pub parent_request_id: Option<String>,
}

impl ObservationEvent {
    pub(crate) fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservabilitySummary {
    pub total_requests: u64,
    pub successful_requests: u64,
    pub failed_requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub estimated_cost_usd: f64,
    pub last_event_at_ms: Option<u64>,
    pub storage_bytes: u64,
    pub storage_path: Option<String>,
    #[serde(default)]
    pub persistence_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistentSummary {
    version: u8,
    summary: ObservabilitySummary,
    #[serde(default)]
    last_sequence: u64,
    #[serde(default)]
    checkpoints: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservabilityBreakdownRow {
    pub key: String,
    pub requests: u64,
    pub successful_requests: u64,
    pub failed_requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub estimated_cost_usd: f64,
    pub total_latency_ms: u64,
    pub max_latency_ms: u64,
}

impl ObservabilityBreakdownRow {
    fn include(&mut self, event: &ObservationEvent) {
        self.requests = self.requests.saturating_add(1);
        if event.outcome == "success" {
            self.successful_requests = self.successful_requests.saturating_add(1);
        } else {
            self.failed_requests = self.failed_requests.saturating_add(1);
        }
        self.input_tokens = self.input_tokens.saturating_add(event.usage.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(event.usage.output_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(event.usage.cached_input_tokens);
        self.reasoning_tokens = self
            .reasoning_tokens
            .saturating_add(event.usage.reasoning_tokens);
        self.total_tokens = self.total_tokens.saturating_add(event.usage.total_tokens);
        self.estimated_cost_usd += event.estimated_cost_usd.unwrap_or(0.0);
        self.total_latency_ms = self.total_latency_ms.saturating_add(event.latency_ms);
        self.max_latency_ms = self.max_latency_ms.max(event.latency_ms);
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservabilityBreakdown {
    pub generated_at_ms: u64,
    pub since_ms: u64,
    pub scanned_events: u64,
    pub truncated: bool,
    pub daily: Vec<ObservabilityBreakdownRow>,
    pub providers: Vec<ObservabilityBreakdownRow>,
    pub models: Vec<ObservabilityBreakdownRow>,
    pub surfaces: Vec<ObservabilityBreakdownRow>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservabilityStorageFile {
    pub name: String,
    pub bytes: u64,
    pub day: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservabilityCleanupPreview {
    pub generated_at_ms: u64,
    pub total_bytes_before: u64,
    pub bytes_after: u64,
    pub files: Vec<ObservabilityStorageFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservabilityTrashReport {
    pub transaction_id: String,
    pub files: usize,
    pub bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservabilityTrashEntry {
    pub transaction_id: String,
    pub created_at_ms: u64,
    pub files: usize,
    pub bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ObservabilityTrashManifest {
    version: u8,
    transaction_id: String,
    created_at_ms: u64,
    files: Vec<ObservabilityStorageFile>,
}

impl ObservabilitySummary {
    fn include(&mut self, event: &ObservationEvent) {
        self.total_requests = self.total_requests.saturating_add(1);
        if event.outcome == "success" {
            self.successful_requests = self.successful_requests.saturating_add(1);
        } else {
            self.failed_requests = self.failed_requests.saturating_add(1);
        }
        self.input_tokens = self.input_tokens.saturating_add(event.usage.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(event.usage.output_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(event.usage.cached_input_tokens);
        self.reasoning_tokens = self
            .reasoning_tokens
            .saturating_add(event.usage.reasoning_tokens);
        self.total_tokens = self.total_tokens.saturating_add(event.usage.total_tokens);
        self.estimated_cost_usd += event.estimated_cost_usd.unwrap_or(0.0);
        self.last_event_at_ms = Some(self.last_event_at_ms.unwrap_or(0).max(event.timestamp_ms));
    }
}

#[derive(Clone)]
pub(crate) struct ObservabilityLedger {
    inner: Arc<LedgerInner>,
}

struct LedgerInner {
    directory: Option<PathBuf>,
    persistent: Mutex<PersistentSummary>,
    file_lock: Mutex<()>,
    next_sequence: AtomicU64,
    pending_writes: AtomicUsize,
    write_finished: tokio::sync::Notify,
}

struct PendingWriteGuard {
    inner: Arc<LedgerInner>,
}

impl Drop for PendingWriteGuard {
    fn drop(&mut self) {
        self.inner.pending_writes.fetch_sub(1, Ordering::AcqRel);
        self.inner.write_finished.notify_waiters();
    }
}

impl ObservabilityLedger {
    pub(crate) fn new(directory: Option<PathBuf>) -> Self {
        let repair_error = directory.as_deref().and_then(|directory| {
            // A killed process can leave a partial final JSONL record. Remove only that
            // incomplete tail before loading checkpoints so the next append starts on a
            // clean record boundary and remains replayable after another interruption.
            repair_incomplete_event_tails(directory).err().map(|_| {
                "observability startup could not repair an incomplete event tail".to_string()
            })
        });
        let mut persistent = directory
            .as_deref()
            .map(load_persistent_state)
            .unwrap_or_else(empty_persistent_summary);
        persistent.summary.persistence_error = repair_error;
        if let Some(directory) = directory.as_deref() {
            // Persist migration/replay progress immediately. A later request is not required
            // to make the recovered lifetime totals durable.
            if persist_summary(directory, &persistent).is_err() {
                persistent.summary.persistence_error =
                    Some("observability startup checkpoint could not be persisted".into());
            }
        }
        let next_sequence = persistent.last_sequence.saturating_add(1).max(1);
        Self {
            inner: Arc::new(LedgerInner {
                directory,
                persistent: Mutex::new(persistent),
                file_lock: Mutex::new(()),
                next_sequence: AtomicU64::new(next_sequence),
                pending_writes: AtomicUsize::new(0),
                write_finished: tokio::sync::Notify::new(),
            }),
        }
    }

    pub(crate) fn summary(&self) -> ObservabilitySummary {
        self.inner
            .persistent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .summary
            .clone()
    }

    pub(crate) fn record(&self, mut event: ObservationEvent, settings: ObservabilitySettings) {
        if !settings.request_log && !settings.usage_log {
            return;
        }
        if !settings.usage_log {
            event.usage = TokenUsage::default();
            event.estimated_cost_usd = None;
        }
        let Some(directory) = self.inner.directory.clone() else {
            event.ledger_sequence = self.inner.next_sequence.fetch_add(1, Ordering::Relaxed);
            self.inner
                .persistent
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .summary
                .include(&event);
            return;
        };
        let inner = Arc::clone(&self.inner);
        inner.pending_writes.fetch_add(1, Ordering::AcqRel);
        tokio::task::spawn_blocking(move || {
            let _pending = PendingWriteGuard {
                inner: Arc::clone(&inner),
            };
            let _guard = inner
                .file_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Allocate the sequence under the same lock that serializes file appends. This
            // keeps sequence/checkpoint order deterministic even when many completed requests
            // reach spawn_blocking concurrently.
            event.ledger_sequence = inner.next_sequence.fetch_add(1, Ordering::Relaxed);
            let persisted = persist(&directory, &event, &settings);
            let Ok((segment, offset)) = persisted else {
                inner
                    .persistent
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .summary
                    .persistence_error = Some("observability event could not be persisted".into());
                return;
            };
            let mut persistent = inner
                .persistent
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            persistent.summary.include(&event);
            persistent.summary.storage_path = Some(directory.to_string_lossy().into_owned());
            persistent.summary.storage_bytes = active_storage_bytes(&directory);
            persistent.summary.persistence_error = None;
            persistent.last_sequence = persistent.last_sequence.max(event.ledger_sequence);
            persistent.checkpoints.insert(segment, offset);
            compact_checkpoints(&mut persistent.checkpoints);
            let snapshot = persistent.clone();
            drop(persistent);
            if persist_summary(&directory, &snapshot).is_err() {
                inner
                    .persistent
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .summary
                    .persistence_error = Some(
                    "observability checkpoint could not be persisted; retained events will be replayed on startup".into(),
                );
            }
        });
    }

    pub(crate) async fn flush(&self) {
        loop {
            let notified = self.inner.write_finished.notified();
            if self.inner.pending_writes.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

pub fn read_observability_summary(directory: impl AsRef<Path>) -> ObservabilitySummary {
    load_persistent_state(directory.as_ref()).summary
}

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

pub fn preview_observability_cleanup(
    directory: impl AsRef<Path>,
    now_ms: u64,
    retention_days: u16,
    max_storage_bytes: u64,
) -> ObservabilityCleanupPreview {
    let mut files = event_files(directory.as_ref()).unwrap_or_default();
    files.sort_by(|left, right| left.name.cmp(&right.name));
    let total_before = files.iter().map(|file| file.bytes).sum::<u64>();
    let current_day = now_ms / MILLIS_PER_DAY;
    let oldest_day = current_day.saturating_sub(u64::from(retention_days.saturating_sub(1)));
    let mut selected = BTreeSet::new();
    let mut remaining = total_before;
    for file in &files {
        if file.day < oldest_day && selected.insert(file.name.clone()) {
            remaining = remaining.saturating_sub(file.bytes);
        }
    }
    for file in &files {
        if remaining <= max_storage_bytes {
            break;
        }
        if selected.insert(file.name.clone()) {
            remaining = remaining.saturating_sub(file.bytes);
        }
    }
    let files = files
        .into_iter()
        .filter(|file| selected.contains(&file.name))
        .map(|file| ObservabilityStorageFile {
            name: file.name,
            bytes: file.bytes,
            day: file.day,
        })
        .collect::<Vec<_>>();
    ObservabilityCleanupPreview {
        generated_at_ms: now_ms,
        total_bytes_before: total_before,
        bytes_after: remaining,
        files,
    }
}

pub fn trash_observability_cleanup(
    directory: impl AsRef<Path>,
    preview: &ObservabilityCleanupPreview,
) -> Result<Option<ObservabilityTrashReport>, String> {
    if preview.files.is_empty() {
        return Ok(None);
    }
    let directory = directory.as_ref();
    let trash_root = directory.join(".trash");
    ensure_real_directory(&trash_root)?;
    let transaction_id = format!(
        "txn-{}-{}-{}",
        ObservationEvent::now_ms(),
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    );
    let transaction = trash_root.join(&transaction_id);
    fs::create_dir(&transaction)
        .map_err(|error| format!("failed to create observability trash transaction: {error}"))?;
    secure_directory(&transaction)?;
    let manifest = ObservabilityTrashManifest {
        version: 1,
        transaction_id: transaction_id.clone(),
        created_at_ms: ObservationEvent::now_ms(),
        files: preview.files.clone(),
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| format!("failed to encode observability trash manifest: {error}"))?;
    write_new_secure_file(&transaction.join("manifest.json"), &manifest_bytes)?;

    let mut moved = Vec::<String>::new();
    for file in &preview.files {
        validate_event_file_name(&file.name)?;
        let source = directory.join(&file.name);
        let target = transaction.join(&file.name);
        let metadata = fs::symlink_metadata(&source)
            .map_err(|error| format!("observability cleanup source changed: {error}"))?;
        if !metadata.file_type().is_file() || metadata.len() != file.bytes {
            rollback_trash_moves(directory, &transaction, &moved);
            let _ = fs::remove_dir_all(&transaction);
            return Err("observability cleanup source changed after preview".into());
        }
        if let Err(error) = fs::rename(&source, &target) {
            rollback_trash_moves(directory, &transaction, &moved);
            let _ = fs::remove_dir_all(&transaction);
            return Err(format!(
                "failed to move observability segment to trash: {error}"
            ));
        }
        moved.push(file.name.clone());
    }
    Ok(Some(ObservabilityTrashReport {
        transaction_id,
        files: preview.files.len(),
        bytes: preview.files.iter().map(|file| file.bytes).sum(),
    }))
}

pub fn list_observability_trash(directory: impl AsRef<Path>) -> Vec<ObservabilityTrashEntry> {
    let trash_root = directory.as_ref().join(".trash");
    let Ok(entries) = fs::read_dir(trash_root) else {
        return Vec::new();
    };
    let mut values = entries
        .flatten()
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            if !file_type.is_dir() || file_type.is_symlink() {
                return None;
            }
            let bytes = fs::read(entry.path().join("manifest.json")).ok()?;
            if bytes.len() > 1024 * 1024 {
                return None;
            }
            let manifest = serde_json::from_slice::<ObservabilityTrashManifest>(&bytes).ok()?;
            validate_trash_manifest(&manifest).ok()?;
            Some(ObservabilityTrashEntry {
                transaction_id: manifest.transaction_id,
                created_at_ms: manifest.created_at_ms,
                files: manifest.files.len(),
                bytes: manifest.files.iter().map(|file| file.bytes).sum(),
            })
        })
        .collect::<Vec<_>>();
    values.sort_by(|left, right| right.created_at_ms.cmp(&left.created_at_ms));
    values
}

pub fn restore_observability_trash(
    directory: impl AsRef<Path>,
    transaction_id: &str,
) -> Result<ObservabilityTrashReport, String> {
    validate_transaction_id(transaction_id)?;
    let directory = directory.as_ref();
    let transaction = directory.join(".trash").join(transaction_id);
    let metadata = fs::symlink_metadata(&transaction)
        .map_err(|error| format!("observability trash transaction was not found: {error}"))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err("observability trash transaction is not a real directory".into());
    }
    let manifest_path = transaction.join("manifest.json");
    let bytes = fs::read(&manifest_path)
        .map_err(|error| format!("failed to read observability trash manifest: {error}"))?;
    if bytes.len() > 1024 * 1024 {
        return Err("observability trash manifest exceeds 1 MiB".into());
    }
    let manifest: ObservabilityTrashManifest = serde_json::from_slice(&bytes)
        .map_err(|_| "observability trash manifest is invalid JSON".to_string())?;
    validate_trash_manifest(&manifest)?;
    if manifest.transaction_id != transaction_id {
        return Err("observability trash transaction identity mismatch".into());
    }
    for file in &manifest.files {
        if directory.join(&file.name).exists() {
            return Err(format!("restore target already exists: {}", file.name));
        }
        let metadata = fs::symlink_metadata(transaction.join(&file.name))
            .map_err(|error| format!("trashed observability segment is missing: {error}"))?;
        if !metadata.file_type().is_file() || metadata.len() != file.bytes {
            return Err("trashed observability segment changed after cleanup".into());
        }
    }
    let mut restored = Vec::<String>::new();
    for file in &manifest.files {
        if let Err(error) = fs::rename(transaction.join(&file.name), directory.join(&file.name)) {
            for name in restored.iter().rev() {
                let _ = fs::rename(directory.join(name), transaction.join(name));
            }
            return Err(format!("failed to restore observability segment: {error}"));
        }
        restored.push(file.name.clone());
    }
    fs::remove_file(manifest_path)
        .and_then(|_| fs::remove_dir(&transaction))
        .map_err(|error| {
            format!("observability data was restored but trash cleanup failed: {error}")
        })?;
    Ok(ObservabilityTrashReport {
        transaction_id: transaction_id.to_string(),
        files: manifest.files.len(),
        bytes: manifest.files.iter().map(|file| file.bytes).sum(),
    })
}

fn validate_transaction_id(value: &str) -> Result<(), String> {
    if value.len() < 8
        || value.len() > 96
        || !value.starts_with("txn-")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("invalid observability trash transaction id".into());
    }
    Ok(())
}

fn validate_event_file_name(value: &str) -> Result<(), String> {
    if value.len() > 96
        || !value.starts_with("events-")
        || !value.ends_with(".jsonl")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
    {
        return Err("invalid observability event file name".into());
    }
    Ok(())
}

fn validate_trash_manifest(manifest: &ObservabilityTrashManifest) -> Result<(), String> {
    if manifest.version != 1 || manifest.files.is_empty() || manifest.files.len() > 100_000 {
        return Err("unsupported observability trash manifest".into());
    }
    validate_transaction_id(&manifest.transaction_id)?;
    let mut names = BTreeSet::new();
    for file in &manifest.files {
        validate_event_file_name(&file.name)?;
        if !names.insert(file.name.as_str()) {
            return Err("observability trash manifest contains an invalid file row".into());
        }
    }
    Ok(())
}

fn ensure_real_directory(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err("observability trash path is not a real directory".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .map_err(|error| format!("failed to create observability trash: {error}"))?;
        }
        Err(error) => return Err(format!("failed to inspect observability trash: {error}")),
    }
    secure_directory(path)
}

fn write_new_secure_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    secure_mode(&mut options);
    let mut file = options
        .open(path)
        .map_err(|error| format!("failed to create observability trash manifest: {error}"))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("failed to write observability trash manifest: {error}"))
}

fn rollback_trash_moves(directory: &Path, transaction: &Path, names: &[String]) {
    for name in names.iter().rev() {
        let _ = fs::rename(transaction.join(name), directory.join(name));
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

fn persist(
    directory: &Path,
    event: &ObservationEvent,
    settings: &ObservabilitySettings,
) -> Result<(String, u64), String> {
    fs::create_dir_all(directory)
        .map_err(|error| format!("failed to create observability directory: {error}"))?;
    secure_directory(directory)?;
    prune_observability_trash(
        directory,
        event.timestamp_ms,
        settings.trash_retention_days,
        settings.max_trash_bytes,
    )?;
    let preview = preview_observability_cleanup(
        directory,
        event.timestamp_ms,
        settings.retention_days,
        settings.max_storage_bytes,
    );
    let _ = trash_observability_cleanup(directory, &preview)?;

    let mut line = serde_json::to_vec(event)
        .map_err(|error| format!("failed to encode observability event: {error}"))?;
    line.push(b'\n');
    let day = event.timestamp_ms / MILLIS_PER_DAY;
    let segment_limit = MAX_SEGMENT_BYTES
        .min((settings.max_storage_bytes / 8).max(64 * 1024))
        .min(settings.max_storage_bytes);
    let (path, _) = writable_segment(directory, day, line.len() as u64, segment_limit)?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    secure_mode(&mut options);
    let mut file = options
        .open(&path)
        .map_err(|error| format!("failed to open observability segment: {error}"))?;
    file.write_all(&line)
        .and_then(|_| file.sync_data())
        .map_err(|error| format!("failed to append observability event: {error}"))?;
    let offset = file
        .metadata()
        .map(|metadata| metadata.len())
        .map_err(|error| format!("failed to checkpoint observability segment: {error}"))?;
    drop(file);
    let preview = preview_observability_cleanup(
        directory,
        event.timestamp_ms,
        settings.retention_days,
        settings.max_storage_bytes,
    );
    let _ = trash_observability_cleanup(directory, &preview)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "observability segment has no valid file name".to_string())?
        .to_string();
    Ok((name, offset))
}

fn writable_segment(
    directory: &Path,
    day: u64,
    incoming: u64,
    segment_limit: u64,
) -> Result<(PathBuf, u32), String> {
    let prefix = format!("events-{day:020}-");
    let mut latest = None;
    for entry in fs::read_dir(directory)
        .map_err(|error| format!("failed to list observability directory: {error}"))?
    {
        let entry =
            entry.map_err(|error| format!("failed to inspect observability entry: {error}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(segment) = name
            .strip_prefix(&prefix)
            .and_then(|value| value.strip_suffix(".jsonl"))
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        latest = Some(latest.unwrap_or(0).max(segment));
    }
    let mut segment = latest.unwrap_or(0);
    let mut path = directory.join(format!("{prefix}{segment:06}.jsonl"));
    let size = fs::metadata(&path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if size > 0 && size.saturating_add(incoming) > segment_limit {
        segment = segment.saturating_add(1);
        path = directory.join(format!("{prefix}{segment:06}.jsonl"));
    }
    Ok((path, segment))
}

fn prune_observability_trash(
    directory: &Path,
    now_ms: u64,
    retention_days: u16,
    max_bytes: u64,
) -> Result<(), String> {
    let trash_root = directory.join(".trash");
    let Ok(entries) = fs::read_dir(&trash_root) else {
        return Ok(());
    };
    let mut transactions = entries
        .flatten()
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            if !file_type.is_dir() || file_type.is_symlink() {
                return None;
            }
            let bytes = fs::read(entry.path().join("manifest.json")).ok()?;
            if bytes.len() > 1024 * 1024 {
                return None;
            }
            let manifest = serde_json::from_slice::<ObservabilityTrashManifest>(&bytes).ok()?;
            validate_trash_manifest(&manifest).ok()?;
            Some((
                entry.path(),
                manifest.created_at_ms,
                manifest.files.iter().map(|file| file.bytes).sum::<u64>(),
            ))
        })
        .collect::<Vec<_>>();
    transactions.sort_by_key(|(_, created_at_ms, _)| *created_at_ms);
    let retention_ms = u64::from(retention_days).saturating_mul(MILLIS_PER_DAY);
    let oldest = now_ms.saturating_sub(retention_ms);
    let mut total = transactions.iter().map(|(_, _, bytes)| *bytes).sum::<u64>();
    for (path, created_at_ms, bytes) in transactions {
        if created_at_ms < oldest || total > max_bytes {
            fs::remove_dir_all(path)
                .map_err(|error| format!("failed to expire observability trash: {error}"))?;
            total = total.saturating_sub(bytes);
        }
    }
    Ok(())
}

struct EventFile {
    path: PathBuf,
    name: String,
    day: u64,
    bytes: u64,
}

fn event_files(directory: &Path) -> Result<Vec<EventFile>, String> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("failed to list observability directory: {error}")),
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() || file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(day) = name
            .strip_prefix("events-")
            .and_then(|value| value.split_once('-').map(|(day, _)| day))
            .and_then(|value| value.parse::<u64>().ok())
        else {
            continue;
        };
        if !name.ends_with(".jsonl") {
            continue;
        }
        let bytes = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        files.push(EventFile {
            path: entry.path(),
            name,
            day,
            bytes,
        });
    }
    Ok(files)
}

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

fn empty_persistent_summary() -> PersistentSummary {
    PersistentSummary {
        version: SUMMARY_VERSION,
        summary: ObservabilitySummary::default(),
        last_sequence: 0,
        checkpoints: BTreeMap::new(),
    }
}

fn load_persistent_state(directory: &Path) -> PersistentSummary {
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

fn repair_incomplete_event_tails(directory: &Path) -> Result<(), String> {
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

fn compact_checkpoints(checkpoints: &mut BTreeMap<String, u64>) {
    while checkpoints.len() > MAX_SUMMARY_CHECKPOINTS {
        let Some(oldest) = checkpoints.keys().next().cloned() else {
            break;
        };
        checkpoints.remove(&oldest);
    }
}

fn active_storage_bytes(directory: &Path) -> u64 {
    event_files(directory)
        .unwrap_or_default()
        .into_iter()
        .fold(0_u64, |total, file| total.saturating_add(file.bytes))
}

fn persist_summary(directory: &Path, state: &PersistentSummary) -> Result<(), String> {
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
fn secure_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(unix)]
fn secure_directory(directory: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("failed to secure observability directory: {error}"))
}

#[cfg(not(unix))]
fn secure_mode(_options: &mut OpenOptions) {}

#[cfg(not(unix))]
fn secure_directory(_directory: &Path) -> Result<(), String> {
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
            exposed_model: "provider/model".into(),
            route_id: None,
            account_id: None,
            status_code: 200,
            outcome: "success".into(),
            failure_category: None,
            latency_ms: 42,
            attempts: 1,
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
        let _ = fs::remove_dir_all(directory);
    }
}
