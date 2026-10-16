use crate::config::ObservabilitySettings;
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod cleanup;
mod cache_health;
mod events;
mod summary;
pub use cache_health::{read_cache_health, CacheHealthPolicy, CacheHealthSnapshot, SessionCacheHealth};
use cache_health::CacheHealthState;

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
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

const MILLIS_PER_DAY: u64 = 86_400_000;
const MAX_SEGMENT_BYTES: u64 = 1024 * 1024;
const SUMMARY_FILE: &str = "usage-totals.json";
const SUMMARY_VERSION: u8 = 2;
const MAX_SUMMARY_CHECKPOINTS: usize = 512;

#[derive(Clone,Debug)]
pub(crate) struct ReportedUpstreamUsage(pub TokenUsage);

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
/// Final physical response cache evidence, before retry usage is added for billing.
/// Kept out of provider wire JSON so unknown final usage cannot inherit older attempts.
pub struct CacheObservation {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_read_reported: bool,
}

impl CacheObservation {
    pub(crate) fn from_provider_json(value: &Value) -> Self {
        let usage = value.get("usage").filter(|value|value.is_object()).or_else(|| value.pointer("/response/usage").filter(|value|value.is_object()))
            .or_else(|| value.pointer("/message/usage").filter(|value|value.is_object()));
        if let Some(raw) = usage.filter(|u| u.get("prompt_tokens").is_none() && (u.get("cache_read_input_tokens").is_some()
            || u.get("cache_creation_input_tokens").is_some())) {
            let n = |key| raw.get(key).and_then(Value::as_u64).unwrap_or(0);
            return Self { input_tokens: n("input_tokens").saturating_add(n("cache_read_input_tokens"))
                .saturating_add(n("cache_creation_input_tokens")),
                cached_input_tokens: n("cache_read_input_tokens"),
                cache_read_reported: raw.get("cache_read_input_tokens").and_then(Value::as_u64).is_some() };
        }
        Self::from_usage(&TokenUsage::from_json(value))
    }
    pub(crate) fn from_usage(usage: &TokenUsage) -> Self {
        Self { input_tokens: usage.input_tokens, cached_input_tokens: usage.cached_input_tokens,
            cache_read_reported: usage.cache_read_reported }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    /// Raw Anthropic cache writes, retained while aggregating physical attempts.
    #[serde(default, skip_serializing_if="is_zero_counter")]
    pub cache_creation_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    /// False means the upstream did not report cache reads, not a measured 0%.
    #[serde(default)]
    pub cache_read_reported: bool,
}

fn is_zero_counter(value:&u64)->bool {*value==0}
fn is_false_flag(value:&bool)->bool { !*value }

impl TokenUsage {
    pub(crate) fn from_json(value: &Value) -> Self {
        if let Some(usage) = value
            .get("usageMetadata").filter(|value|value.is_object())
            .or_else(|| value.pointer("/response/usageMetadata").filter(|value|value.is_object()))
        {
            let input = number(usage, &["/promptTokenCount"]);
            let reasoning = number(usage, &["/thoughtsTokenCount"]);
            let visible_output = number(usage, &["/candidatesTokenCount"]);
            let output = visible_output.saturating_add(reasoning);
            return Self {
                input_tokens: input,
                output_tokens: output,
                cached_input_tokens: number(usage, &["/cachedContentTokenCount"]),
                cache_creation_input_tokens:0,
                reasoning_tokens: reasoning,
                total_tokens: number(usage, &["/totalTokenCount"])
                    .max(input.saturating_add(output)),
                cache_read_reported: usage.get("cachedContentTokenCount").and_then(Value::as_u64).is_some(),
            };
        }

        let Some(usage) = value
            .get("usage").filter(|value|value.is_object())
            .or_else(|| value.pointer("/response/usage").filter(|value|value.is_object()))
            .or_else(|| value.pointer("/message/usage").filter(|value|value.is_object()))
        else {
            return Self::default();
        };
        if let Some(iterations) = usage.get("iterations").and_then(Value::as_array) {
            let mut total = Self::default();
            let mut reported = false;
            let mut incomplete = false;
            for iteration in iterations {
                let known = ["/input_tokens", "/prompt_tokens", "/output_tokens", "/completion_tokens",
                    "/cache_read_input_tokens", "/cache_creation_input_tokens", "/total_tokens",
                    "/input_tokens_details/cached_tokens", "/prompt_tokens_details/cached_tokens",
                    "/input_tokens_details/cache_creation_tokens", "/output_tokens_details/reasoning_tokens",
                    "/completion_tokens_details/reasoning_tokens"]
                    .iter().any(|path| iteration.pointer(path).and_then(Value::as_u64).is_some());
                if !known { incomplete = true; continue; }
                let next = Self::from_usage_object(iteration);
                total.cache_read_reported = (total.input_tokens == 0 || total.cache_read_reported)
                    && (next.input_tokens == 0 || next.cache_read_reported)
                    && (total.cache_read_reported || next.cache_read_reported);
                total.input_tokens = total.input_tokens.saturating_add(next.input_tokens);
                total.output_tokens = total.output_tokens.saturating_add(next.output_tokens);
                total.cached_input_tokens = total.cached_input_tokens.saturating_add(next.cached_input_tokens);
                total.cache_creation_input_tokens = total.cache_creation_input_tokens.saturating_add(next.cache_creation_input_tokens);
                total.reasoning_tokens = total.reasoning_tokens.saturating_add(next.reasoning_tokens);
                total.total_tokens = total.total_tokens.saturating_add(next.total_tokens);
                reported = true;
            }
            if reported {
                // Iterations replace the aggregate; never charge both representations.
                total.cache_read_reported &= !incomplete;
                return total;
            }
        }
        Self::from_usage_object(usage)
    }

    fn from_usage_object(usage: &Value) -> Self {
        let input = number(usage, &["/input_tokens", "/prompt_tokens"]);
        let cache_creation = number(usage,&["/cache_creation_input_tokens", "/input_tokens_details/cache_creation_tokens"]);
        let input = if usage.get("prompt_tokens").is_none() && (usage.get("cache_read_input_tokens").is_some() || usage.get("cache_creation_input_tokens").is_some()) {
            input.saturating_add(number(usage,&["/cache_read_input_tokens"])).saturating_add(cache_creation)
        } else {input};
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
            cache_creation_input_tokens: cache_creation,
            reasoning_tokens: reasoning,
            total_tokens: number(usage, &["/total_tokens"]).max(input.saturating_add(output)),
            cache_read_reported: ["/input_tokens_details/cached_tokens", "/prompt_tokens_details/cached_tokens", "/cache_read_input_tokens"]
                .iter().any(|path| usage.pointer(path).and_then(Value::as_u64).is_some()),
        }
    }

    pub(crate) fn merge_max(&mut self, next: Self) {
        self.input_tokens = self.input_tokens.max(next.input_tokens);
        self.output_tokens = self.output_tokens.max(next.output_tokens);
        self.cached_input_tokens = self.cached_input_tokens.max(next.cached_input_tokens);
        self.cache_creation_input_tokens = self.cache_creation_input_tokens.max(next.cache_creation_input_tokens);
        self.reasoning_tokens = self.reasoning_tokens.max(next.reasoning_tokens);
        self.total_tokens = self.total_tokens.max(next.total_tokens).max(self.input_tokens.saturating_add(self.output_tokens));
        self.cache_read_reported |= next.cache_read_reported;

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

/// Content-free metadata from the final serialized upstream request.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestDiagnostic {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_binding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_model: Option<String>,
    #[serde(default)]
    pub client_thread_id: Option<String>,
    pub wire_bytes: u64,
    pub input_items: u64,
    pub tool_count: u64,
    pub store: Option<bool>,
    pub has_previous_response_id: bool,
    pub client_transport: String,
    pub upstream_transport: String,
    pub websocket_history_replayed: Option<bool>,
    pub upstream_status_code: u16,
    pub upstream_request_id: Option<String>,
    /// Set only when the request head (instructions or tool names) differs from the
    /// previous request of the same client thread; used to tell helper requests apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_change: Option<HeadChange>,
    /// Which Codex surface sent the request (sub-agent / helper marker headers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_kind: Option<ClientKind>,
    /// Tools that arrived as history items rather than in the base `tools` list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamic_tools: Option<DynamicToolStats>,
}

/// Identifier-only view of Codex's request markers; no paths or prompt text.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientKind {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_trigger: Option<String>,
    /// Top-level key names of the turn metadata, to learn what Codex sends.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turn_metadata_keys: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolStats {
    /// `additional_tools` / `tool_search_output` items in the request history.
    pub items: u64,
    /// Tools carried by those items.
    pub tools: u64,
}

/// Bounded description of how a request head changed within one client thread.
/// Holds a short instruction prefix and tool identifiers only, never tool
/// arguments or conversation content.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadChange {
    /// First request seen for the thread: only the instruction prefix is recorded.
    #[serde(default, skip_serializing_if = "is_false_flag")]
    pub first_in_thread: bool,
    #[serde(default, skip_serializing_if = "is_false_flag")]
    pub instruction_changed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_head: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_added: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_removed: Vec<String>,
    #[serde(default)]
    pub tools_added_total: u64,
    #[serde(default)]
    pub tools_removed_total: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservationEvent {
    /// A privacy-redacted event is never a cache measurement, including on replay.
    #[serde(default, skip_serializing_if = "is_false_flag")]
    pub usage_suppressed: bool,
    #[serde(default)]
    pub ledger_sequence: u64,
    pub timestamp_ms: u64,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_observation: Option<CacheObservation>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_diagnostic: Option<RequestDiagnostic>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_health: Option<CacheHealthSnapshot>,
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
        if !event.attempt_only {
        self.requests = self.requests.saturating_add(1);
        if event.outcome == "success" {
            self.successful_requests = self.successful_requests.saturating_add(1);
        } else {
            self.failed_requests = self.failed_requests.saturating_add(1);
        }
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
        if !event.attempt_only {
        self.total_latency_ms = self.total_latency_ms.saturating_add(event.latency_ms);
        self.max_latency_ms = self.max_latency_ms.max(event.latency_ms);
        }
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
        if !event.attempt_only {
        self.total_requests = self.total_requests.saturating_add(1);
        if event.outcome == "success" {
            self.successful_requests = self.successful_requests.saturating_add(1);
        } else {
            self.failed_requests = self.failed_requests.saturating_add(1);
        }
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

#[derive(Default)]
struct CacheCheckpointTask {
    generation: u64,
    task: Option<tokio::task::JoinHandle<()>>,
}

struct LedgerInner {
    directory: Option<PathBuf>,
    persistent: Mutex<PersistentSummary>,
    file_lock: Mutex<()>,
    next_sequence: AtomicU64,
    pending_writes: AtomicUsize,
    write_finished: tokio::sync::Notify,
    cache_health: Mutex<CacheHealthState>,
    usage_enabled: AtomicBool,
    last_usage_disabled: Mutex<Option<std::time::Instant>>,
    cache_checkpoint_task: Mutex<CacheCheckpointTask>,
    #[cfg(test)]
    cache_health_clock_ms: AtomicU64,
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
        let cache_health = CacheHealthState::load(directory.as_deref());
        Self {
            inner: Arc::new(LedgerInner {
                directory,
                persistent: Mutex::new(persistent),
                file_lock: Mutex::new(()),
                next_sequence: AtomicU64::new(next_sequence),
                pending_writes: AtomicUsize::new(0),
                write_finished: tokio::sync::Notify::new(),
                cache_health: Mutex::new(cache_health),
                usage_enabled: AtomicBool::new(true),
                last_usage_disabled: Mutex::new(None),
                cache_checkpoint_task: Mutex::new(CacheCheckpointTask::default()),
                #[cfg(test)]
                cache_health_clock_ms: AtomicU64::new(0),
            }),
        }
    }

    pub(crate) fn summary(&self) -> ObservabilitySummary {
        let mut summary = self.inner
            .persistent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .summary
            .clone();
        summary.cache_health = Some(self.cache_health_summary());
        summary
    }

    pub(crate) fn cache_health_summary(&self) -> CacheHealthSnapshot {
        let now = ObservationEvent::now_ms();
        #[cfg(test)]
        let now = match self.inner.cache_health_clock_ms.load(Ordering::Relaxed) {
            0 => now, injected => injected,
        };
        if self.inner.directory.is_some() {
            // Serialize release, durability and publication with every disk writer.
            return persist_cache_checkpoint_at(&self.inner, now, true).1;
        }
        let mut health = self.inner.cache_health.lock().unwrap_or_else(|p| p.into_inner());
        health.set_monitoring(self.inner.usage_enabled.load(Ordering::Acquire));
        health.release_pending_notifications(now);
        health.snapshot(now)
    }

    pub(crate) fn record(&self, event: ObservationEvent, settings: ObservabilitySettings) {
        self.record_started(event, settings, std::time::Instant::now());
    }

    pub(crate) fn record_started(&self, mut event: ObservationEvent, settings: ObservabilitySettings, started: std::time::Instant) {
        let privacy = self.inner.last_usage_disabled.lock().unwrap_or_else(|p| p.into_inner());
        let usage_allowed = settings.usage_log && self.inner.usage_enabled.load(Ordering::Acquire)
            && privacy.is_none_or(|disabled| started > disabled);
        if !settings.request_log && !usage_allowed { return; }
        if !usage_allowed {
            event.usage_suppressed = true;
            event.usage = TokenUsage::default();
            event.cache_observation = None;
            event.estimated_cost_usd = None;
        }
        let Some(directory) = self.inner.directory.clone() else {
            event.ledger_sequence = self.inner.next_sequence.fetch_add(1, Ordering::Relaxed);
            {
                let mut health = self.inner.cache_health.lock().unwrap_or_else(|p| p.into_inner());
                health.set_monitoring(self.inner.usage_enabled.load(Ordering::Acquire));
                if usage_allowed { health.include(&event, true); }
            }
            self.inner
                .persistent
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .summary
                .include(&event);
            return;
        };
        drop(privacy);
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
            let privacy = inner.last_usage_disabled.lock().unwrap_or_else(|p| p.into_inner());
            let usage_allowed = usage_allowed && inner.usage_enabled.load(Ordering::Acquire)
                && privacy.is_none_or(|disabled| started > disabled);
            if !settings.request_log && !usage_allowed { return; }
            if !usage_allowed {
                event.usage_suppressed = true;
                event.usage = TokenUsage::default();
                event.cache_observation = None;
                event.estimated_cost_usd = None;
            }
            // Allocate the sequence under the same lock that serializes file appends. This
            // keeps sequence/checkpoint order deterministic even when many completed requests
            // reach spawn_blocking concurrently.
            event.ledger_sequence = inner.next_sequence.fetch_add(1, Ordering::Relaxed);
            {
                let mut health = inner.cache_health.lock().unwrap_or_else(|p| p.into_inner());
                health.set_monitoring(inner.usage_enabled.load(Ordering::Acquire));
                let alert = usage_allowed && health.include(&event, true);
                health.persist(&directory, ObservationEvent::now_ms(), alert);
            }
            schedule_cache_checkpoint(&inner);
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

    pub(crate) async fn configure_cache_monitoring(&self, enabled: bool) {
        // This authority is current configuration, not a possibly stale request's settings.
        {
            let mut privacy = self.inner.last_usage_disabled.lock().unwrap_or_else(|p| p.into_inner());
            if !enabled { *privacy = Some(std::time::Instant::now()); }
            self.inner.usage_enabled.store(enabled, Ordering::Release);
        }
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let _guard = inner.file_lock.lock().unwrap_or_else(|p| p.into_inner());
            let mut health = inner.cache_health.lock().unwrap_or_else(|p| p.into_inner());
            health.set_monitoring(inner.usage_enabled.load(Ordering::Acquire));
            if let Some(directory) = inner.directory.as_deref() {
                health.persist(directory, ObservationEvent::now_ms(), true);
            }
        }).await.ok();
        schedule_cache_checkpoint(&self.inner);
    }

    pub(crate) async fn flush(&self) {
        loop {
            let notified = self.inner.write_finished.notified();
            if self.inner.pending_writes.load(Ordering::Acquire) == 0 { break; }
            notified.await;
        }
        {
            let mut slot = self.inner.cache_checkpoint_task.lock().unwrap_or_else(|p| p.into_inner());
            slot.generation = slot.generation.wrapping_add(1);
            if let Some(task) = slot.task.take() { task.abort(); }
        }
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || persist_cache_checkpoint(&inner)).await.ok();
    }

}


fn persist_cache_checkpoint(inner: &Arc<LedgerInner>) -> bool {
    if inner.directory.is_none() { return false; }
    persist_cache_checkpoint_at(inner, ObservationEvent::now_ms(), false).0
}

fn persist_cache_checkpoint_at(inner: &Arc<LedgerInner>, now: u64, urgent_only: bool) -> (bool, CacheHealthSnapshot) {
    let _guard = inner.file_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut health = inner.cache_health.lock().unwrap_or_else(|p| p.into_inner());
    health.set_monitoring(inner.usage_enabled.load(Ordering::Acquire));
    health.release_pending_notifications(now);
    if !urgent_only || health.urgent_checkpoint_needed() {
        if let Some(directory) = inner.directory.as_deref() {
            health.persist(directory, now, !urgent_only);
        }
    }
    (health.background_checkpoint_needed(now), health.snapshot(now))
}

// Registration and the final dirty recheck share one lock. A writer either
// keeps this worker alive or observes an empty slot and starts its successor.
fn finish_cache_checkpoint_round(inner: &Arc<LedgerInner>, generation: u64) -> bool {
    let mut slot = inner.cache_checkpoint_task.lock().unwrap_or_else(|p| p.into_inner());
    if slot.generation != generation { return false; }
    if inner.cache_health.lock().unwrap_or_else(|p| p.into_inner())
        .background_checkpoint_needed(ObservationEvent::now_ms()) { return true; }
    slot.task.take();
    false
}

fn schedule_cache_checkpoint(inner: &Arc<LedgerInner>) {
    if inner.directory.is_none() { return; }
    let mut slot = inner.cache_checkpoint_task.lock().unwrap_or_else(|p| p.into_inner());
    if !inner.cache_health.lock().unwrap_or_else(|p| p.into_inner())
        .background_checkpoint_needed(ObservationEvent::now_ms()) { return; }
    if slot.task.as_ref().is_some_and(|task| !task.is_finished()) { return; }
    slot.generation = slot.generation.wrapping_add(1);
    let generation = slot.generation;
    let weak = Arc::downgrade(inner);
    slot.task = Some(tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let Some(inner) = weak.upgrade() else { break; };
            if !tokio::task::spawn_blocking(move || {
                if inner.cache_checkpoint_task.lock().unwrap_or_else(|p| p.into_inner()).generation != generation {
                    return false;
                }
                persist_cache_checkpoint(&inner);
                finish_cache_checkpoint_round(&inner, generation)
            }).await.unwrap_or(false) { break; }
        }
    }));
}

impl Drop for LedgerInner {
    fn drop(&mut self) {
        if let Some(task) = self.cache_checkpoint_task.get_mut().unwrap_or_else(|p| p.into_inner()).task.take() {
            task.abort();
        }
    }
}


pub fn read_observability_summary(directory: impl AsRef<Path>) -> ObservabilitySummary {
    let mut summary = load_persistent_state(directory.as_ref()).summary;
    summary.cache_health = Some(read_cache_health(directory));
    summary
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

#[cfg(test)]
mod review13_usage_privacy_tests {
    use super::*;
    #[tokio::test]
    async fn review13_disabled_usage_removes_cache_evidence_from_persistence_and_replay() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ObservabilityLedger::new(Some(dir.path().to_path_buf()));
        let event = super::review14_privacy_regressions::event("usage-disabled");
        ledger.record(event, ObservabilitySettings { request_log:true, usage_log:false, ..Default::default() });
        ledger.flush().await;
        let events = read_recent_observability_events(dir.path(),0,10);
        assert_eq!(events.len(),1);
        assert_eq!(events[0].usage.total_tokens,0);
        assert!(events[0].cache_observation.is_none());
        assert!(events[0].estimated_cost_usd.is_none());
        let reopened = ObservabilityLedger::new(Some(dir.path().to_path_buf()));
        assert_eq!(reopened.summary().total_tokens,0);
        assert!(reopened.cache_health_summary().sessions.is_empty());
    }
}

#[cfg(test)]
mod review13_usage_opt_out_regressions {
    use super::*;
    #[tokio::test]
    async fn review13_usage_disabled_removes_cache_evidence_from_disk_and_replay() {
        let directory = tempfile::tempdir().unwrap();
        let ledger = ObservabilityLedger::new(Some(directory.path().into()));
        let mut settings = ObservabilitySettings::default();
        settings.request_log = true; settings.usage_log = false;
        let event = ObservationEvent {
                usage_suppressed: false,
            ledger_sequence: 0, timestamp_ms: ObservationEvent::now_ms(), request_id: "r13-disabled".into(),
            client_thread_id: Some(uuid::Uuid::new_v4().to_string()),
            cache_observation: Some(CacheObservation { input_tokens: 100_000, cached_input_tokens: 90_000, cache_read_reported: true }),
            provider_id: Some("test".into()), upstream_model: Some("test".into()), upstream_endpoint: None,
            exposed_model: "test".into(), route_id: None, account_id: None, status_code: 200,
            outcome: "success".into(), failure_category: None, upstream_error: None, request_diagnostic: None,
            latency_ms: 1, attempts: 1, candidate_ordinal: 1, send_count: 1, recovery_kinds: vec![], recovery_kind: None,
            attempt_only: false, streaming: false,
            usage: TokenUsage { input_tokens: 100_000, cached_input_tokens: 90_000, output_tokens: 7, total_tokens: 100_007, cache_read_reported: true, ..Default::default() },
            estimated_cost_usd: Some(1.0), shadow: false, shadow_rule_id: None, parent_request_id: None,
        };
        ledger.configure_cache_monitoring(false).await;
        ledger.record(event, settings); ledger.flush().await;
        let events = read_recent_observability_events(directory.path(), 0, 10);
        assert_eq!(events.len(), 1);
        assert!(events[0].cache_observation.is_none());
        assert_eq!(events[0].usage.total_tokens, 0);
        assert!(!events[0].usage.cache_read_reported);
        assert!(events[0].estimated_cost_usd.is_none());
        for file in event_files(directory.path()).unwrap() {
            let raw = fs::read_to_string(file.path).unwrap();
            assert!(!raw.contains("cacheObservation"));
            assert!(!raw.contains("100000"));
            assert!(!raw.contains("90000"));
        }
        let restored = ObservabilityLedger::new(Some(directory.path().into()));
        restored.configure_cache_monitoring(true).await;
        assert_eq!(restored.summary().total_tokens, 0);
        assert!(restored.cache_health_summary().sessions.iter().all(|s| s.input_tokens == 0 && s.measured_requests == 0));
        restored.flush().await;
    }
}

#[cfg(test)]
pub(super) mod review14_privacy_regressions {
    use super::*;
    fn settings() -> ObservabilitySettings {
        ObservabilitySettings { request_log: true, usage_log: true, ..Default::default() }
    }
    pub(super) fn event(id: &str) -> ObservationEvent {
        serde_json::from_value(serde_json::json!({
            "timestampMs":ObservationEvent::now_ms(),"requestId":id,"clientThreadId":"01a13ddb-8d21-7cb2-8f00-23a8a09af079",
            "providerId":"fixture","upstreamModel":"wire-model","exposedModel":"alias","accountId":"label",
            "statusCode":200,"outcome":"success","latencyMs":1,"attempts":1,"sendCount":1,"streaming":false,
            "inputTokens":100000,"outputTokens":7,"cachedInputTokens":80000,"reasoningTokens":0,"totalTokens":100007,
            "cacheReadReported":true,"estimatedCostUsd":1.5,
            "cacheObservation":{"inputTokens":100000,"cachedInputTokens":80000,"cacheReadReported":true}
        })).unwrap()
    }
    #[tokio::test]
    async fn review14_inflight_usage_does_not_survive_off_or_reenable() {
        for persistent in [false,true] {
            let dir=tempfile::tempdir().unwrap();
            let ledger=ObservabilityLedger::new(persistent.then(||dir.path().to_path_buf()));
            let started=std::time::Instant::now();
            ledger.configure_cache_monitoring(false).await;
            ledger.record_started(event("off"),settings(),started);
            ledger.configure_cache_monitoring(true).await;
            ledger.record_started(event("stale-after-on"),settings(),started);
            ledger.flush().await;
            assert_eq!(ledger.summary().input_tokens,0);
            assert!(ledger.cache_health_summary().sessions.is_empty());
            assert_eq!(ledger.summary().estimated_cost_usd,0.0);
            if persistent {
                let events=read_recent_observability_events(dir.path(),0,10);
                assert_eq!(events.len(),2);
                for e in events {assert_eq!(e.usage.total_tokens,0);assert!(e.cache_observation.is_none());assert!(e.estimated_cost_usd.is_none());}
            }
            ledger.record(event("fresh"),settings());
            ledger.flush().await;
            assert_eq!(ledger.summary().input_tokens,100000);
        }
    }
    #[tokio::test]
    async fn review14_usage_only_request_is_not_saved_while_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ObservabilityLedger::new(Some(dir.path().to_path_buf()));
        ledger.configure_cache_monitoring(false).await;
        ledger.record(event("usage-only"), ObservabilitySettings { request_log:false, usage_log:true, ..Default::default() });
        ledger.flush().await;
        assert!(read_recent_observability_events(dir.path(),0,10).is_empty());
        assert_eq!(ledger.summary().total_tokens,0);
    }
    #[tokio::test]
    async fn review14_queued_save_cannot_restore_usage_after_off_on() {
        let dir=tempfile::tempdir().unwrap();
        let ledger=ObservabilityLedger::new(Some(dir.path().to_path_buf()));
        let lock=ledger.inner.file_lock.lock().unwrap();
        ledger.record(event("queued"),settings());
        let off=ledger.configure_cache_monitoring(false);
        tokio::pin!(off);
        tokio::select! {biased; _=&mut off=>panic!("file lock must block checkpoint"),_=tokio::task::yield_now()=>{}}
        assert!(!ledger.inner.usage_enabled.load(Ordering::Acquire));
        let on=ledger.configure_cache_monitoring(true);
        tokio::pin!(on);
        tokio::select! {biased; _=&mut on=>panic!("file lock must block checkpoint"),_=tokio::task::yield_now()=>{}}
        drop(lock);
        off.await;on.await;ledger.flush().await;
        let events=read_recent_observability_events(dir.path(),0,10);
        assert_eq!(events.len(),1); assert_eq!(events[0].usage.total_tokens,0);
        assert!(events[0].cache_observation.is_none());assert!(events[0].estimated_cost_usd.is_none());
        assert!(events[0].usage_suppressed);
        assert_eq!(ObservabilityLedger::new(Some(dir.path().to_path_buf())).summary().input_tokens,0);
    }
}


#[cfg(test)]
mod review15_queued_privacy_regressions {
    use super::*;

    #[tokio::test]
    async fn review15_queued_seed_and_later_failover_remain_redacted_on_disk_replay() {
        let directory = tempfile::tempdir().unwrap();
        let ledger = ObservabilityLedger::new(Some(directory.path().to_path_buf()));
        let settings = ObservabilitySettings { request_log: true, usage_log: true, ..Default::default() };
        ledger.configure_cache_monitoring(settings.usage_log).await;
        let logical_started = std::time::Instant::now();
        let make_seed = |candidate_started| crate::server::ObservationSeed::without_candidate(
            ledger.clone(), settings.clone(), "queued-logical-request".into(), "fixture", false, candidate_started,
        ).with_privacy_origin(logical_started);
        let usage = TokenUsage::from_json(&serde_json::json!({"usage":{"input_tokens":100000,
            "output_tokens":7,"input_tokens_details":{"cached_tokens":90000}}}));
        let lock = ledger.inner.file_lock.lock().unwrap();
        let mut queued = make_seed(logical_started);
        queued.input_price_per_million = Some(1.0);
        queued.as_attempt().finish(axum::http::StatusCode::BAD_GATEWAY, Some("provider_http_error"), usage.clone());
        let off = ledger.configure_cache_monitoring(false);
        tokio::pin!(off);
        tokio::select! { biased; _ = &mut off => panic!("checkpoint must wait for file lock"), _ = tokio::task::yield_now() => {} }
        assert!(!ledger.inner.usage_enabled.load(Ordering::Acquire));
        let on = ledger.configure_cache_monitoring(true);
        tokio::pin!(on);
        tokio::select! { biased; _ = &mut on => panic!("checkpoint must wait for file lock"), _ = tokio::task::yield_now() => {} }
        assert!(ledger.inner.usage_enabled.load(Ordering::Acquire));
        let later_candidate_started = std::time::Instant::now();
        let mut later = make_seed(later_candidate_started);
        later.input_price_per_million = Some(1.0);
        later.as_attempt().finish(axum::http::StatusCode::BAD_GATEWAY, Some("provider_http_error"), usage.clone());
        make_seed(logical_started).finish(axum::http::StatusCode::OK, None, usage);
        drop(lock);
        off.await; on.await; ledger.flush().await;
        let events = read_recent_observability_events(directory.path(), 0, 10);
        assert_eq!(events.len(), 3);
        assert_eq!(events.iter().filter(|event| event.attempt_only).count(), 2);
        for event in events {
            assert!(event.usage_suppressed);
            assert_eq!(event.usage.total_tokens, 0);
            assert!(event.cache_observation.is_none());
            assert!(event.estimated_cost_usd.is_none());
        }
        fs::remove_file(directory.path().join(SUMMARY_FILE)).unwrap();
        // OFF/ON can both publish before the file lock is released, so no
        // cache checkpoint need exist when every queued event is suppressed.
        if let Err(error) = fs::remove_file(directory.path().join("cache-health.json")) {
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        }
        let replay = ObservabilityLedger::new(Some(directory.path().to_path_buf()));
        assert_eq!(replay.summary().total_requests, 1);
        assert_eq!(replay.summary().total_tokens, 0);
        assert_eq!(replay.summary().estimated_cost_usd, 0.0);
        assert!(replay.cache_health_summary().sessions.is_empty());
        replay.flush().await;
    }
}


#[cfg(test)]
mod review17_memory_poll_regressions {
    use super::*;
    const BASE: u64 = 10_000_000;
    fn record_window(ledger: &ObservabilityLedger, label: &str, at: u64, cached: u64) {
        for n in 0..20 {
            let mut event = super::review14_privacy_regressions::event(&format!("{label}-{n}"));
            event.timestamp_ms = at;
            event.cache_observation.as_mut().unwrap().cached_input_tokens = cached;
            ledger.record(event, ObservabilitySettings { request_log: true, usage_log: true, ..Default::default() });
        }
    }
    fn clock(ledger: &ObservabilityLedger, at: u64) {
        ledger.inner.cache_health_clock_ms.store(at, Ordering::Relaxed);
    }
    fn pending() -> ObservabilityLedger {
        let ledger = ObservabilityLedger::new(None);
        clock(&ledger, BASE);
        record_window(&ledger, "warning", BASE, 80000);
        assert_eq!(ledger.cache_health_summary().sessions[0].alert_sequence, 1);
        record_window(&ledger, "recovery", BASE + 60000, 100000);
        assert!(!ledger.cache_health_summary().sessions[0].active_alert);
        record_window(&ledger, "recurrence", BASE + 120000, 80000);
        clock(&ledger, BASE + 120000);
        assert!(ledger.cache_health_summary().sessions[0].active_alert);
        assert_eq!(ledger.cache_health_summary().sessions[0].alert_sequence, 1);
        ledger
    }
    #[test]
    fn review17_memory_only_normal_summary_polls_release_once_without_io_or_runtime() {
        for full_summary in [false, true] {
            let ledger = pending();
            let cooldown = CacheHealthPolicy::default().cooldown_ms;
            clock(&ledger, BASE + cooldown - 1);
            assert_eq!(ledger.cache_health_summary().sessions[0].alert_sequence, 1);
            clock(&ledger, BASE + cooldown);
            let snapshot = if full_summary { ledger.summary().cache_health.unwrap() } else { ledger.cache_health_summary() };
            assert_eq!(snapshot.sessions[0].alert_sequence, 2);
            assert_eq!(snapshot.sessions[0].last_alert_at_ms, Some(BASE + cooldown));
            for _ in 0..3 {
                assert_eq!(ledger.summary().cache_health.unwrap().sessions[0].alert_sequence, 2);
                assert_eq!(ledger.cache_health_summary().sessions[0].alert_sequence, 2);
            }
            assert!(ledger.inner.directory.is_none());
            assert!(ledger.inner.cache_checkpoint_task.lock().unwrap().task.is_none());
            assert_eq!(ledger.inner.pending_writes.load(Ordering::Relaxed), 0);
        }
    }
    #[tokio::test]
    async fn review17_memory_poll_cancels_recovered_disabled_and_stale_pending() {
        for mode in ["recovery", "disable", "stale", "stale-before-record"] {
            let ledger = pending();
            let cooldown = CacheHealthPolicy::default().cooldown_ms;
            match mode {
                "recovery" => record_window(&ledger, "second-recovery", BASE + 180000, 100000),
                "disable" => { ledger.configure_cache_monitoring(false).await; ledger.configure_cache_monitoring(true).await; },
                "stale" => { clock(&ledger, BASE + 120000 + cooldown + 1); ledger.cache_health_summary(); },
                _ => {}
            }
            if mode.starts_with("stale") {
                // New unknown observations must not resurrect an expired pending toast.
                let mut event = super::review14_privacy_regressions::event("fresh-unknown");
                event.timestamp_ms = BASE + 120000 + cooldown + 2;
                event.cache_observation.as_mut().unwrap().cache_read_reported = false;
                ledger.record(event, ObservabilitySettings { request_log: true, usage_log: true, ..Default::default() });
                clock(&ledger, BASE + 120000 + cooldown + 3);
            } else { clock(&ledger, BASE + cooldown); }
            let snapshot = ledger.cache_health_summary();
            assert_eq!(snapshot.sessions[0].alert_sequence, 1, "{mode}");
            let state = serde_json::to_value(&*ledger.inner.cache_health.lock().unwrap()).unwrap();
            assert!(state["entries"].as_object().unwrap().values().all(|entry| entry["notification_pending"] == false));
            ledger.flush().await;
            assert_eq!(ledger.summary().cache_health.unwrap().sessions[0].alert_sequence, 1);
        }
    }
}


#[cfg(test)]
mod review18_regressions {
    use super::*;
    async fn samples(ledger: &ObservabilityLedger, prefix: &str, at: u64, cached: u64) {
        for i in 0..20 {
            let mut event = super::review14_privacy_regressions::event(&format!("{prefix}-{i}"));
            event.timestamp_ms = at + i;
            event.usage.cached_input_tokens = cached;
            event.cache_observation.as_mut().unwrap().cached_input_tokens = cached;
            ledger.record(event, ObservabilitySettings { request_log:true, usage_log:true, ..Default::default() });
        }
        ledger.flush().await;
    }
    #[tokio::test]
    async fn review18_disk_poll_release_is_durable_before_cursor_and_never_reuses_sequence() {
        let directory = tempfile::tempdir().unwrap();
        let ledger = ObservabilityLedger::new(Some(directory.path().to_path_buf()));
        let base = ObservationEvent::now_ms(); let cooldown = CacheHealthPolicy::default().cooldown_ms;
        samples(&ledger,"first",base,80000).await;
        samples(&ledger,"recover",base+100,100000).await;
        samples(&ledger,"pending",base+200,80000).await;
        assert_eq!(read_cache_health(directory.path()).sessions[0].alert_sequence,1);
        ledger.inner.cache_health_clock_ms.store(base+cooldown+20,Ordering::Relaxed);
        let snapshot = ledger.cache_health_summary();
        let cursor = snapshot.sessions[0].alert_sequence;
        assert_eq!(cursor,2);
        assert_eq!(read_cache_health(directory.path()).sessions[0].alert_sequence,cursor);
        let path = directory.path().join("cache-health.json");
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        let bytes = fs::read(&path).unwrap();
        for i in 1..4 {
            ledger.inner.cache_health_clock_ms.store(base+cooldown+20+i*5000,Ordering::Relaxed);
            assert_eq!(ledger.cache_health_summary().sessions[0].alert_sequence,cursor);
        }
        assert_eq!(fs::read(&path).unwrap(),bytes);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(),before);
        // No flush after publication: only the poll itself may make the cursor durable.
        drop(ledger);
        let restored = ObservabilityLedger::new(Some(directory.path().to_path_buf()));
        let stale = base+2*cooldown+1000;
        restored.inner.cache_health_clock_ms.store(stale,Ordering::Relaxed);
        let snapshot = restored.cache_health_summary();
        assert_eq!(snapshot.monitor_id,read_cache_health(directory.path()).monitor_id);
        assert_eq!(snapshot.sessions[0].alert_sequence,cursor);
        samples(&restored,"recovered-after-restart",stale,100000).await;
        samples(&restored,"new-warning",stale+100,80000).await;
        assert!(restored.cache_health_summary().sessions[0].alert_sequence>cursor);
        assert!(read_cache_health(directory.path()).sessions[0].alert_sequence>cursor);
        restored.configure_cache_monitoring(false).await;
        assert!(!restored.cache_health_summary().monitoring);
        assert!(!read_cache_health(directory.path()).monitoring);
    }
    #[tokio::test]
    async fn review18_memory_poll_still_releases_pending_without_disk() {
        let ledger = ObservabilityLedger::new(None); let base = ObservationEvent::now_ms();
        samples(&ledger,"first",base,80000).await;
        samples(&ledger,"recover",base+100,100000).await;
        samples(&ledger,"pending",base+200,80000).await;
        ledger.inner.cache_health_clock_ms.store(base+CacheHealthPolicy::default().cooldown_ms+20,Ordering::Relaxed);
        assert_eq!(ledger.cache_health_summary().sessions[0].alert_sequence,2);
    }
}
