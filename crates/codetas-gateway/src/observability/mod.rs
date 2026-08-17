use crate::config::ObservabilitySettings;
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod cleanup;
mod events;
mod summary;

pub(crate) use cleanup::validate_trash_manifest;
pub use cleanup::{
    list_observability_trash, preview_observability_cleanup, restore_observability_trash,
    trash_observability_cleanup,
};
pub use events::{
    read_observability_breakdown, read_observability_events_after_sequence,
    read_recent_observability_events,
};
pub(crate) use summary::{
    active_storage_bytes, compact_checkpoints, empty_persistent_summary, load_persistent_state,
    persist_summary, repair_incomplete_event_tails, secure_directory, secure_mode,
};

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
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
pub struct UpstreamErrorDiagnostic {
    pub status_code: u16,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub retry_after: Option<String>,
    #[serde(default)]
    pub error_type: Option<String>,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub error_message: Option<String>,
    pub body_bytes: u64,
    pub body_sha256: String,
    pub body_truncated: bool,
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
    #[serde(default)]
    pub upstream_endpoint: Option<String>,
    pub exposed_model: String,
    pub route_id: Option<String>,
    pub account_id: Option<String>,
    pub status_code: u16,
    pub outcome: String,
    pub failure_category: Option<String>,
    #[serde(default)]
    pub upstream_error: Option<UpstreamErrorDiagnostic>,
    pub latency_ms: u64,
    pub attempts: u16,
    #[serde(default)]
    pub candidate_ordinal: u16,
    #[serde(default)]
    pub send_count: u16,
    #[serde(default)]
    pub recovery_kinds: Vec<String>,
    #[serde(default)]
    pub recovery_kind: Option<String>,
    /// Candidate-attempt events remain available to bounded debug scopes, but do not
    /// contribute a second request to request-level summaries or breakdowns.
    #[serde(default)]
    pub attempt_only: bool,
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
pub(crate) struct PersistentSummary {
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
        if event.attempt_only {
            return;
        }
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
pub(crate) struct ObservabilityTrashManifest {
    version: u8,
    transaction_id: String,
    created_at_ms: u64,
    files: Vec<ObservabilityStorageFile>,
}

impl ObservabilitySummary {
    fn include(&mut self, event: &ObservationEvent) {
        if event.attempt_only {
            return;
        }
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
