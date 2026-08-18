//! Proxy-internal continuation cache for `previous_response_id`.
//!
//! Codex continues turns with `previous_response_id` + a delta `input`. Many upstreams
//! (including ChatGPT Codex) reject that field, so CODETAS expands it locally before
//! forwarding. This store is a short-lived restart/replay cache — not conversation
//! history. Design (gpt-5.6-sol review):
//! - lookup key remains `response_id`
//! - lifetime is session-scoped multi-tip (siblings kept; linear chains pruned)
//! - items are `Arc` so expand does not deep-clone under the global lock
//! - disk is per-session, debounced, bounded; never on the request hot path when a
//!   Tokio runtime is available
//! - OpenCodex-style global spill is intentionally not ported

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Cap on stored response entries across all sessions.
const MAX_STORED_RESPONSES: usize = 1_000;
/// Hard RAM byte cap across all resident entries (including a single huge tip).
const MAX_STORED_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
/// Refuse to retain any one entry above this size (store Unavailable stub only).
const MAX_ENTRY_BYTES: usize = MAX_STORED_RESPONSE_BYTES;
const RESPONSE_TTL: Duration = Duration::from_secs(60 * 60);
const ANON_SESSION_TTL: Duration = Duration::from_secs(30 * 60);
/// Keep the replaced linear parent briefly for tool-output repair / immediate retry.
const RETRY_PARENT_GRACE: Duration = Duration::from_secs(5 * 60);
const LEASE_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_LEASES: usize = 256;
const MAX_LEASES_PER_PARENT: u32 = 8;
const MAX_SESSION_BINDINGS: usize = 256;
const MAX_SESSIONS: usize = 512;
const MAX_SESSION_KEY_LEN: usize = 256;
const MAX_LIVE_TIPS_PER_SESSION: usize = 32;
const MAX_OWNED_PER_SESSION: usize = 64;
/// Exact/Lossy disk payload limit per entry.
const SNAPSHOT_ENTRY_MAX_BYTES: usize = 2 * 1024 * 1024;
const SESSION_FILE_MAX_BYTES: usize = 4 * 1024 * 1024;
const DISK_TOTAL_MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_LEGACY_FILE_BYTES: u64 = 40 * 1024 * 1024;
const MAX_SESSION_FILES_ON_LOAD: usize = 512;
const MAX_LOAD_TOTAL_BYTES: u64 = DISK_TOTAL_MAX_BYTES as u64;
const PERSISTED_STATE_VERSION: u8 = 2;
const SNAPSHOT_DEBOUNCE: Duration = Duration::from_millis(2_000);
const PRIVATE_LEASE_FIELD: &str = "_codetas_replay_lease";
const PRIVATE_SESSION_FIELD: &str = "_codetas_session_key";
const PRIVATE_FIDELITY_FIELD: &str = "_codetas_replay_fidelity";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReplayFidelity {
    Exact,
    Lossy,
    Unavailable,
}

/// Result of expanding `previous_response_id`. Callers must not treat Lossy as silent Exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandOutcome {
    /// No previous_response_id on the body.
    NotRequested,
    Exact,
    Lossy,
    Miss(&'static str),
}

impl ExpandOutcome {
    pub fn expanded(self) -> bool {
        matches!(self, Self::Exact | Self::Lossy)
    }

    pub fn miss_reason(self) -> Option<&'static str> {
        match self {
            Self::Miss(reason) => Some(reason),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct Entry {
    response_id: String,
    parent_response_id: Option<String>,
    session_key: String,
    items: Arc<Vec<Value>>,
    size_bytes: usize,
    created_at_unix_ms: u64,
    last_access_unix_ms: u64,
    fidelity: ReplayFidelity,
    /// In-flight expands that have not yet been consumed by `remember`.
    lease_count: u32,
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedEntry {
    response_id: String,
    parent_response_id: Option<String>,
    created_at_unix_ms: u64,
    last_access_unix_ms: u64,
    fidelity: ReplayFidelity,
    size_bytes: usize,
    /// Present for Exact and Lossy payloads that fit the disk budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    items: Option<Vec<Value>>,
}

/// Lightweight plan collected under the store lock; heavy clone/strip happens outside.
struct SessionPersistPlan {
    session_key: String,
    anonymous: bool,
    last_access_unix_ms: u64,
    live_tips: Vec<String>,
    entries: Vec<EntryPersistPlan>,
    dirty_generation: u64,
}

struct EntryPersistPlan {
    response_id: String,
    parent_response_id: Option<String>,
    created_at_unix_ms: u64,
    last_access_unix_ms: u64,
    fidelity: ReplayFidelity,
    size_bytes: usize,
    items: Arc<Vec<Value>>,
}

#[derive(Serialize, Deserialize)]
struct PersistedSessionFile {
    version: u8,
    session_key: String,
    anonymous: bool,
    last_access_unix_ms: u64,
    live_tips: Vec<String>,
    entries: Vec<PersistedEntry>,
}

struct SessionShard {
    key: String,
    anonymous: bool,
    live_tips: HashSet<String>,
    owned_ids: HashSet<String>,
    last_access_unix_ms: u64,
    dirty: bool,
    /// Bumped on every mutation so a concurrent write does not clear fresher dirt.
    dirty_generation: u64,
}

struct ReplayLease {
    parent_id: String,
    session_key: String,
    /// Fidelity of the expanded parent; Lossy must propagate to children.
    parent_fidelity: ReplayFidelity,
    /// Prefix length restored by expand; reserved for compaction provenance.
    #[allow(dead_code)]
    replayed_prefix_len: usize,
    created_at_unix_ms: u64,
}

struct StoreData {
    entries: HashMap<String, Entry>,
    sessions: HashMap<String, SessionShard>,
    /// parent_id -> child response ids currently retained
    children: HashMap<String, HashSet<String>>,
    leases: HashMap<u64, ReplayLease>,
    /// Gateway-issued session binding tokens (never accept raw client strings).
    session_bindings: HashMap<u64, (String, u64)>,
    total_bytes: usize,
}

struct PersistController {
    root: PathBuf,
    legacy_snapshot: Option<PathBuf>,
    queue: Mutex<PersistQueue>,
    write: Mutex<()>,
}

struct PersistQueue {
    dirty_sessions: HashSet<String>,
    scheduled: bool,
}

/// Process-wide counters for continuation-cache health.
#[derive(Debug, Default)]
pub struct ResponseStateMetrics {
    pub expand_hits: AtomicU64,
    pub expand_misses: AtomicU64,
    pub expand_lossy: AtomicU64,
    pub remembers: AtomicU64,
    pub rejected_oversize: AtomicU64,
    pub tip_prunes: AtomicU64,
    pub evictions: AtomicU64,
    pub persist_writes: AtomicU64,
    pub persist_failures: AtomicU64,
    pub oversized_disk_skips: AtomicU64,
    pub lock_wait_ns: AtomicU64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponseStateMetricsSnapshot {
    pub expand_hits: u64,
    pub expand_misses: u64,
    pub expand_lossy: u64,
    pub remembers: u64,
    pub rejected_oversize: u64,
    pub tip_prunes: u64,
    pub evictions: u64,
    pub persist_writes: u64,
    pub persist_failures: u64,
    pub oversized_disk_skips: u64,
    pub lock_wait_ns: u64,
    pub entry_count: usize,
    pub session_count: usize,
    pub resident_bytes: usize,
    pub lease_count: usize,
}

/// Proxy-internal continuation cache keyed by upstream response id.
#[derive(Clone)]
pub struct ResponseStateStore {
    inner: Arc<ResponseStateInner>,
}

struct ResponseStateInner {
    data: RwLock<StoreData>,
    persist: Option<PersistController>,
    metrics: ResponseStateMetrics,
}

impl Default for ResponseStateStore {
    fn default() -> Self {
        Self::new(None)
    }
}

fn input_items(input: &Value) -> Vec<Value> {
    match input {
        Value::Array(items) => items.clone(),
        Value::String(text) => vec![json!({"role": "user", "content": text})],
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    }
}

fn serialized_bytes(value: &Value) -> usize {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or(0)
}

fn measure_items(items: &[Value]) -> usize {
    items.iter().map(serialized_bytes).sum()
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn truncate_session_key(session_key: &str) -> String {
    let trimmed = session_key.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for ch in trimmed.chars() {
        let mut buf = [0u8; 4];
        let encoded = ch.encode_utf8(&mut buf);
        if out.len().saturating_add(encoded.len()) > MAX_SESSION_KEY_LEN {
            break;
        }
        out.push_str(encoded);
    }
    out
}

fn session_file_stem(session_key: &str) -> String {
    let digest = Sha256::digest(session_key.as_bytes());
    let hex = digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    // Keep a short sanitized prefix for operators; collision safety is the digest.
    let mut prefix = session_key
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '_'
            }
        })
        .take(24)
        .collect::<String>();
    if prefix.is_empty() {
        prefix = "session".into();
    }
    format!("{prefix}.{hex}")
}

fn resolve_storage(path: &Path) -> (PathBuf, Option<PathBuf>) {
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
    {
        let root = path.with_file_name("response-state");
        (root, Some(path.to_path_buf()))
    } else {
        (path.to_path_buf(), None)
    }
}

/// Best-effort removal of huge inline media so a tip can sometimes fit the disk budget.
fn strip_heavy_payloads(value: &Value) -> (Value, bool) {
    match value {
        Value::String(text) if is_heavy_data_url(text) || text.len() > 64 * 1024 => (
            Value::String(format!(
                "[codetas:omitted {} bytes of inline payload]",
                text.len()
            )),
            true,
        ),
        Value::Array(items) => {
            let mut changed = false;
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let (next, item_changed) = strip_heavy_payloads(item);
                changed |= item_changed;
                out.push(next);
            }
            (Value::Array(out), changed)
        }
        Value::Object(map) => {
            let mut changed = false;
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, child) in map {
                // image_url / input_image style blobs
                if key == "data" || key == "b64_json" || key == "image_url" {
                    if let Value::String(text) = child {
                        if text.len() > 2_048 {
                            changed = true;
                            out.insert(
                                key.clone(),
                                Value::String(format!(
                                    "[codetas:omitted {} bytes]",
                                    text.len()
                                )),
                            );
                            continue;
                        }
                    }
                }
                let (next, child_changed) = strip_heavy_payloads(child);
                changed |= child_changed;
                out.insert(key.clone(), next);
            }
            (Value::Object(out), changed)
        }
        other => (other.clone(), false),
    }
}

fn is_heavy_data_url(text: &str) -> bool {
    text.starts_with("data:image/") && text.contains(";base64,") && text.len() > 2_048
}

fn strip_items_for_disk(items: &[Value]) -> (Vec<Value>, bool) {
    let mut changed = false;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let (next, item_changed) = strip_heavy_payloads(item);
        changed |= item_changed;
        out.push(next);
    }
    (out, changed)
}

impl ResponseStateStore {
    pub(crate) fn new(persistence_path: Option<PathBuf>) -> Self {
        let mut data = StoreData {
            entries: HashMap::new(),
            sessions: HashMap::new(),
            children: HashMap::new(),
            leases: HashMap::new(),
            session_bindings: HashMap::new(),
            total_bytes: 0,
        };
        let persist = persistence_path.map(|path| {
            let (root, legacy) = resolve_storage(&path);
            let controller = PersistController {
                root,
                legacy_snapshot: legacy,
                queue: Mutex::new(PersistQueue {
                    dirty_sessions: HashSet::new(),
                    scheduled: false,
                }),
                write: Mutex::new(()),
            };
            if let Err(error) = controller.load_into(&mut data) {
                crate::debug::log(&format!(
                    "cannot load response continuation state from {}: {error}",
                    controller.root.display()
                ));
            }
            controller
        });
        let store = Self {
            inner: Arc::new(ResponseStateInner {
                data: RwLock::new(data),
                persist,
                metrics: ResponseStateMetrics::default(),
            }),
        };
        store.prune_now();
        store
    }

    /// Attach a session/thread hint from request headers before expand/remember.
    ///
    /// Stores a gateway-issued binding token on the body — raw client strings are
    /// never trusted as session keys. Call only for root turns (or after a hit
    /// has already been validated); do not call speculatively before expand.
    pub fn attach_session_hint(&self, body: &mut Value, session_key: &str) {
        let key = truncate_session_key(session_key);
        if key.is_empty() {
            return;
        }
        // Do not allocate a binding unless it can be stored on the body.
        if !body.is_object() {
            return;
        }
        let now = unix_millis();
        let token = {
            let mut data = self.inner.data.write().unwrap();
            Self::issue_session_binding_locked(&mut data, &key, now)
        };
        if let Some(object) = body.as_object_mut() {
            object.insert(PRIVATE_SESSION_FIELD.into(), json!(token));
        }
    }

    /// Cache a completed upstream response under `response.id` for later replay.
    pub fn remember(&self, request_body: &Value, response: &Value, force: bool) {
        let Some(response_id) = response.get("id").and_then(Value::as_str) else {
            return;
        };
        if response_id.is_empty() {
            return;
        }
        let Some(output) = response.get("output").and_then(Value::as_array) else {
            return;
        };
        let status = response.get("status").and_then(Value::as_str).unwrap_or("");
        if status == "incomplete" {
            let reason = response
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str);
            if reason != Some("max_output_tokens") {
                return;
            }
        } else if status != "completed" {
            return;
        }
        if request_body.get("store") == Some(&json!(false)) && !force {
            return;
        }

        let mut items = input_items(&request_body.get("input").unwrap_or(&Value::Null));
        // Drop private control fields if a caller forgot to strip them before record.
        items.retain(|item| {
            !item
                .as_object()
                .is_some_and(|object| object.contains_key(PRIVATE_LEASE_FIELD))
        });
        items.extend(output.iter().cloned());
        let size_bytes = measure_items(&items);
        let now = unix_millis();
        let oversize = size_bytes > MAX_ENTRY_BYTES;

        // Only gateway-issued leases are trusted. Client-injected lease ids are ignored.
        let lease = self.take_lease_from_body(request_body);
        let binding_token = request_body
            .get(PRIVATE_SESSION_FIELD)
            .and_then(Value::as_u64);
        let parent_from_lease = lease.as_ref().map(|lease| lease.parent_id.clone());
        let session_from_lease = lease.as_ref().map(|lease| lease.session_key.clone());
        let fidelity_from_lease = lease.as_ref().map(|lease| lease.parent_fidelity);

        let dirty_sessions = {
            let started = std::time::Instant::now();
            let mut data = self.inner.data.write().unwrap();
            self.inner.metrics.lock_wait_ns.fetch_add(
                u64::try_from(started.elapsed().as_nanos()).unwrap_or(0),
                Ordering::Relaxed,
            );
            self.expire_leases_locked(&mut data, now);
            Self::expire_session_bindings_locked(&mut data, now);

            if let Some(lease) = lease.as_ref() {
                if let Some(parent) = data.entries.get_mut(&lease.parent_id) {
                    parent.lease_count = parent.lease_count.saturating_sub(1);
                    parent.last_access_unix_ms = now;
                }
            }

            // Parent lineage is lease-only. A bare previous_response_id without a
            // gateway lease must not create child tips (and cannot upgrade Lossy).
            let parent_id = parent_from_lease.clone();

            let session_from_binding = binding_token.and_then(|token| {
                data.session_bindings
                    .remove(&token)
                    .map(|(key, _)| key)
            });

            let session_key = session_from_lease
                .or(session_from_binding)
                .or_else(|| {
                    parent_id
                        .as_ref()
                        .and_then(|parent| data.entries.get(parent).map(|entry| entry.session_key.clone()))
                })
                .unwrap_or_else(|| format!("anon:{response_id}"));
            let anonymous = session_key.starts_with("anon:");

            if data.entries.contains_key(response_id) {
                Self::delete_entry_locked(&mut data, response_id, &self.inner.metrics);
            }

            let (items_arc, fidelity, retained_bytes) = if oversize {
                self.inner
                    .metrics
                    .rejected_oversize
                    .fetch_add(1, Ordering::Relaxed);
                // Keep a tip stub so the chain knows the turn existed, but never
                // retain the oversized payload in RAM.
                (
                    Arc::new(Vec::new()),
                    ReplayFidelity::Unavailable,
                    0_usize,
                )
            } else {
                // Prefer lease fidelity; if somehow parent exists without lease
                // (should not), still never upgrade Lossy/Unavailable parents.
                let inherited = fidelity_from_lease.or_else(|| {
                    parent_id
                        .as_ref()
                        .and_then(|parent| data.entries.get(parent).map(|entry| entry.fidelity))
                });
                let fidelity = match inherited {
                    Some(ReplayFidelity::Lossy) => ReplayFidelity::Lossy,
                    Some(ReplayFidelity::Unavailable) => ReplayFidelity::Unavailable,
                    _ => ReplayFidelity::Exact,
                };
                (Arc::new(items), fidelity, size_bytes)
            };

            let entry = Entry {
                response_id: response_id.to_string(),
                parent_response_id: parent_id.clone(),
                session_key: session_key.clone(),
                items: items_arc,
                size_bytes: retained_bytes,
                created_at_unix_ms: now,
                last_access_unix_ms: now,
                fidelity,
                lease_count: 0,
            };
            data.total_bytes = data.total_bytes.saturating_add(retained_bytes);
            data.entries.insert(response_id.to_string(), entry);

            if let Some(parent) = parent_id.as_ref() {
                data.children
                    .entry(parent.clone())
                    .or_default()
                    .insert(response_id.to_string());
            }

            {
                let session = data.sessions.entry(session_key.clone()).or_insert_with(|| {
                    SessionShard {
                        key: session_key.clone(),
                        anonymous,
                        live_tips: HashSet::new(),
                        owned_ids: HashSet::new(),
                        last_access_unix_ms: now,
                        dirty: true,
                        dirty_generation: 0,
                    }
                });
                session.anonymous = anonymous;
                session.last_access_unix_ms = now;
                session.owned_ids.insert(response_id.to_string());
                session.live_tips.insert(response_id.to_string());
                session.dirty = true;
                session.dirty_generation = session.dirty_generation.saturating_add(1);
            }

            if let Some(parent) = parent_id.as_ref() {
                if let Some(session) = data.sessions.get_mut(&session_key) {
                    if session.live_tips.remove(parent) {
                        self.inner
                            .metrics
                            .tip_prunes
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
                Self::prune_linear_ancestors_locked(
                    &mut data,
                    parent,
                    now,
                    &self.inner.metrics,
                );
            }

            Self::enforce_session_caps_locked(&mut data, &session_key, &self.inner.metrics);
            Self::enforce_session_count_locked(&mut data, now, &self.inner.metrics);
            // Hard-cap after insert. If still over budget with only leased victims,
            // drop this new entry rather than unbounded growth.
            if !Self::enforce_global_limits_locked(&mut data, now, &self.inner.metrics) {
                if data.total_bytes > MAX_STORED_RESPONSE_BYTES
                    || data.entries.len() > MAX_STORED_RESPONSES
                {
                    Self::delete_entry_locked(&mut data, response_id, &self.inner.metrics);
                    self.inner
                        .metrics
                        .rejected_oversize
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            self.inner
                .metrics
                .remembers
                .fetch_add(1, Ordering::Relaxed);

            data.sessions
                .iter()
                .filter(|(_, session)| session.dirty)
                .map(|(key, _)| key.clone())
                .collect::<HashSet<_>>()
        };

        self.schedule_persist(dirty_sessions);
    }

    /// Expand a request's `previous_response_id` into the full replayed input.
    ///
    /// Returns `true` when stored items were prepended (Exact or Lossy).
    pub fn expand_previous_response_input(&self, body: &mut Value) -> bool {
        self.expand_previous_response_input_with_hint(body, None)
            .expanded()
    }

    pub fn expand_previous_response_input_with_hint(
        &self,
        body: &mut Value,
        session_hint: Option<&str>,
    ) -> ExpandOutcome {
        // Never trust client-supplied control fields.
        // Do NOT attach session_hint here — that leaked unused bindings on every
        // request. Roots attach once from requests.rs; hits bind after validation.
        let _ = session_hint;
        Self::strip_private_fields(body);
        let _ = self.prune_now();

        let Some(previous_id) = body
            .get("previous_response_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
        else {
            return ExpandOutcome::NotRequested;
        };

        let now = unix_millis();
        let expand = {
            let started = std::time::Instant::now();
            let mut data = self.inner.data.write().unwrap();
            self.inner.metrics.lock_wait_ns.fetch_add(
                u64::try_from(started.elapsed().as_nanos()).unwrap_or(0),
                Ordering::Relaxed,
            );
            self.expire_leases_locked(&mut data, now);
            Self::expire_session_bindings_locked(&mut data, now);
            Self::expire_entries_locked(&mut data, now, &self.inner.metrics);

            let Some(entry) = data.entries.get(&previous_id).cloned() else {
                self.inner
                    .metrics
                    .expand_misses
                    .fetch_add(1, Ordering::Relaxed);
                return ExpandOutcome::Miss("unknown_id");
            };
            if entry.fidelity == ReplayFidelity::Unavailable {
                self.inner
                    .metrics
                    .expand_misses
                    .fetch_add(1, Ordering::Relaxed);
                return ExpandOutcome::Miss("unavailable");
            }
            if entry.items.is_empty() {
                self.inner
                    .metrics
                    .expand_misses
                    .fetch_add(1, Ordering::Relaxed);
                return ExpandOutcome::Miss("empty");
            }

            let session_key = entry.session_key.clone();

            // Bound outstanding leases before issuing binding/lease (DoS).
            if data.leases.len() >= MAX_LEASES {
                self.expire_leases_locked(&mut data, now);
            }
            if data.leases.len() >= MAX_LEASES {
                self.inner
                    .metrics
                    .expand_misses
                    .fetch_add(1, Ordering::Relaxed);
                return ExpandOutcome::Miss("lease_limit");
            }
            if entry.lease_count >= MAX_LEASES_PER_PARENT {
                self.inner
                    .metrics
                    .expand_misses
                    .fetch_add(1, Ordering::Relaxed);
                return ExpandOutcome::Miss("lease_limit");
            }

            // Binding only after the hit is validated and lease room exists.
            let token = Self::issue_session_binding_locked(&mut data, &session_key, now);
            if let Some(object) = body.as_object_mut() {
                object.insert(PRIVATE_SESSION_FIELD.into(), json!(token));
            }

            if let Some(live) = data.entries.get_mut(&previous_id) {
                live.last_access_unix_ms = now;
                live.lease_count = live.lease_count.saturating_add(1);
            }
            if let Some(session) = data.sessions.get_mut(&session_key) {
                session.last_access_unix_ms = now;
                session.dirty = true;
                session.dirty_generation = session.dirty_generation.saturating_add(1);
            }

            // Unpredictable lease ids (not a monotonic counter a client can guess).
            let mut lease_id = uuid::Uuid::new_v4().as_u128() as u64;
            if lease_id == 0 {
                lease_id = 1;
            }
            while data.leases.contains_key(&lease_id) {
                lease_id = lease_id.wrapping_add(1).max(1);
            }
            data.leases.insert(
                lease_id,
                ReplayLease {
                    parent_id: previous_id.clone(),
                    session_key: session_key.clone(),
                    parent_fidelity: entry.fidelity,
                    replayed_prefix_len: entry.items.len(),
                    created_at_unix_ms: now,
                },
            );

            self.inner
                .metrics
                .expand_hits
                .fetch_add(1, Ordering::Relaxed);
            if entry.fidelity == ReplayFidelity::Lossy {
                self.inner
                    .metrics
                    .expand_lossy
                    .fetch_add(1, Ordering::Relaxed);
            }
            (entry.items, lease_id, entry.fidelity, session_key)
        };

        let (items_arc, lease_id, fidelity, session_key) = expand;

        // Deep clone happens outside the write lock.
        let mut items = (*items_arc).clone();
        let existing = input_items(&body.get("input").unwrap_or(&Value::Null));
        items.extend(existing);
        if let Some(object) = body.as_object_mut() {
            object.insert("input".into(), Value::Array(items));
            object.insert(PRIVATE_LEASE_FIELD.into(), json!(lease_id));
            object.insert(
                PRIVATE_FIDELITY_FIELD.into(),
                Value::String(
                    match fidelity {
                        ReplayFidelity::Exact => "exact",
                        ReplayFidelity::Lossy => "lossy",
                        ReplayFidelity::Unavailable => "unavailable",
                    }
                    .into(),
                ),
            );
        }

        self.schedule_persist(HashSet::from([session_key]));
        match fidelity {
            ReplayFidelity::Exact => ExpandOutcome::Exact,
            ReplayFidelity::Lossy => ExpandOutcome::Lossy,
            ReplayFidelity::Unavailable => ExpandOutcome::Miss("unavailable"),
        }
    }

    /// Find a tool call only inside the exact continuation entry that was replayed.
    pub fn find_tool_call_in_response(
        &self,
        response_id: &str,
        call_id: &str,
        item_type: &str,
    ) -> Option<Value> {
        if response_id.is_empty() || call_id.is_empty() {
            return None;
        }
        let started = std::time::Instant::now();
        let data = self.inner.data.read().unwrap();
        self.inner.metrics.lock_wait_ns.fetch_add(
            u64::try_from(started.elapsed().as_nanos()).unwrap_or(0),
            Ordering::Relaxed,
        );
        let entry = data.entries.get(response_id)?;
        entry.items.iter().find_map(|item| {
            (item.get("type").and_then(Value::as_str) == Some(item_type)
                && item.get("call_id").and_then(Value::as_str) == Some(call_id))
            .then(|| item.clone())
        })
    }

    /// Flush pending session snapshots. Shutdown and tests use this.
    pub(crate) fn flush(&self) {
        if let Some(persist) = &self.inner.persist {
            let (snapshot, keys, live) = self.collect_persist_plans(None, true);
            let files = materialize_session_files(snapshot, &self.inner.metrics);
            let written = persist.persist_snapshot(files, &live, true, &self.inner.metrics);
            self.finalize_persist(keys, written);
        }
    }

    pub fn metrics_snapshot(&self) -> ResponseStateMetricsSnapshot {
        let data = self.inner.data.read().unwrap();
        ResponseStateMetricsSnapshot {
            expand_hits: self.inner.metrics.expand_hits.load(Ordering::Relaxed),
            expand_misses: self.inner.metrics.expand_misses.load(Ordering::Relaxed),
            expand_lossy: self.inner.metrics.expand_lossy.load(Ordering::Relaxed),
            remembers: self.inner.metrics.remembers.load(Ordering::Relaxed),
            rejected_oversize: self.inner.metrics.rejected_oversize.load(Ordering::Relaxed),
            tip_prunes: self.inner.metrics.tip_prunes.load(Ordering::Relaxed),
            evictions: self.inner.metrics.evictions.load(Ordering::Relaxed),
            persist_writes: self.inner.metrics.persist_writes.load(Ordering::Relaxed),
            persist_failures: self.inner.metrics.persist_failures.load(Ordering::Relaxed),
            oversized_disk_skips: self
                .inner
                .metrics
                .oversized_disk_skips
                .load(Ordering::Relaxed),
            lock_wait_ns: self.inner.metrics.lock_wait_ns.load(Ordering::Relaxed),
            entry_count: data.entries.len(),
            session_count: data.sessions.len(),
            resident_bytes: data.total_bytes,
            lease_count: data.leases.len(),
        }
    }

    /// Strip private control fields before the body is forwarded upstream.
    pub fn strip_private_fields(body: &mut Value) {
        if let Some(object) = body.as_object_mut() {
            object.remove(PRIVATE_LEASE_FIELD);
            object.remove(PRIVATE_SESSION_FIELD);
            object.remove(PRIVATE_FIDELITY_FIELD);
        }
    }

    fn take_lease_from_body(&self, body: &Value) -> Option<ReplayLease> {
        let lease_id = body.get(PRIVATE_LEASE_FIELD).and_then(Value::as_u64)?;
        let started = std::time::Instant::now();
        let mut data = self.inner.data.write().unwrap();
        self.inner.metrics.lock_wait_ns.fetch_add(
            u64::try_from(started.elapsed().as_nanos()).unwrap_or(0),
            Ordering::Relaxed,
        );
        let now = unix_millis();
        // Expire first so a stale id cannot be consumed as valid lineage.
        self.expire_leases_locked(&mut data, now);
        let lease = data.leases.remove(&lease_id)?;
        if now.saturating_sub(lease.created_at_unix_ms) > duration_ms(LEASE_TTL) {
            if let Some(parent) = data.entries.get_mut(&lease.parent_id) {
                parent.lease_count = parent.lease_count.saturating_sub(1);
            }
            return None;
        }
        Some(lease)
    }

    fn prune_now(&self) -> bool {
        let now = unix_millis();
        let started = std::time::Instant::now();
        let mut data = self.inner.data.write().unwrap();
        self.inner.metrics.lock_wait_ns.fetch_add(
            u64::try_from(started.elapsed().as_nanos()).unwrap_or(0),
            Ordering::Relaxed,
        );
        let before_entries = data.entries.len();
        let before_bytes = data.total_bytes;
        self.expire_leases_locked(&mut data, now);
        Self::expire_entries_locked(&mut data, now, &self.inner.metrics);
        let _ = Self::enforce_global_limits_locked(&mut data, now, &self.inner.metrics);
        let dirty = data
            .sessions
            .iter()
            .filter(|(_, session)| session.dirty)
            .map(|(key, _)| key.clone())
            .collect::<HashSet<_>>();
        let changed =
            data.entries.len() != before_entries || data.total_bytes != before_bytes || !dirty.is_empty();
        drop(data);
        if changed {
            self.schedule_persist(dirty);
        }
        changed
    }

    fn schedule_persist(&self, mut dirty_sessions: HashSet<String>) {
        dirty_sessions.retain(|key| !key.is_empty());
        let Some(persist) = &self.inner.persist else {
            return;
        };
        if dirty_sessions.is_empty() {
            return;
        }
        {
            let mut queue = persist.queue.lock().unwrap();
            queue.dirty_sessions.extend(dirty_sessions);
        }
        if tokio::runtime::Handle::try_current().is_err() {
            // Tests and early startup: write synchronously.
            let (plans, keys, live) = self.collect_persist_plans(None, false);
            let files = materialize_session_files(plans, &self.inner.metrics);
            // Sync path is typically tests/shutdown-style; allow tombstones.
            let written = persist.persist_snapshot(files, &live, true, &self.inner.metrics);
            // Clear processed dirty keys from the queue on the sync path.
            if let Ok(mut queue) = persist.queue.lock() {
                for (key, _) in &keys {
                    queue.dirty_sessions.remove(key);
                }
            }
            self.finalize_persist(keys, written);
            return;
        }
        let should_spawn = {
            let mut queue = persist.queue.lock().unwrap();
            if queue.scheduled {
                false
            } else {
                queue.scheduled = true;
                true
            }
        };
        if !should_spawn {
            return;
        }
        let store = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SNAPSHOT_DEBOUNCE).await;
            store.flush_debounced().await;
        });
    }

    async fn flush_debounced(&self) {
        loop {
            let dirty = {
                let Some(persist) = &self.inner.persist else {
                    return;
                };
                let mut queue = persist.queue.lock().unwrap();
                if queue.dirty_sessions.is_empty() {
                    queue.scheduled = false;
                    return;
                }
                std::mem::take(&mut queue.dirty_sessions)
            };
            let (plans, keys, live) = self.collect_persist_plans(Some(&dirty), false);
            let inner = Arc::clone(&self.inner);
            let result = tokio::task::spawn_blocking(move || {
                let files = materialize_session_files(plans, &inner.metrics);
                if let Some(persist) = &inner.persist {
                    // Debounced dirty-only passes must not tombstone other live files.
                    persist.persist_snapshot(files, &live, false, &inner.metrics)
                } else {
                    HashSet::new()
                }
            })
            .await;
            let had_failures = match result {
                Ok(written) => {
                    let failed = keys
                        .iter()
                        .any(|(key, _)| !written.contains(key));
                    self.finalize_persist(keys, written);
                    failed
                }
                Err(_) => {
                    self.inner
                        .metrics
                        .persist_failures
                        .fetch_add(1, Ordering::Relaxed);
                    let retry = keys.into_iter().map(|(key, _)| key).collect();
                    self.schedule_persist(retry);
                    true
                }
            };
            // Back off whenever work remains after a pass with failures or skips.
            if had_failures {
                tokio::time::sleep(SNAPSHOT_DEBOUNCE).await;
            }
            // Continue while more sessions became dirty during the write.
        }
    }

    /// Collect Arc-backed plans under the lock. No deep clone / strip / serialize here.
    /// Also returns the full set of live session keys so disk GC does not delete
    /// healthy non-dirty session files.
    fn collect_persist_plans(
        &self,
        only: Option<&HashSet<String>>,
        all_dirty: bool,
    ) -> (Vec<SessionPersistPlan>, Vec<(String, u64)>, HashSet<String>) {
        let started = std::time::Instant::now();
        let data = self.inner.data.read().unwrap();
        self.inner.metrics.lock_wait_ns.fetch_add(
            u64::try_from(started.elapsed().as_nanos()).unwrap_or(0),
            Ordering::Relaxed,
        );
        let live: HashSet<String> = data.sessions.keys().cloned().collect();
        let keys: Vec<String> = if all_dirty {
            live.iter().cloned().collect()
        } else if let Some(only) = only {
            only.iter()
                .filter(|key| data.sessions.contains_key(*key))
                .cloned()
                .collect()
        } else {
            data.sessions
                .iter()
                .filter(|(_, session)| session.dirty)
                .map(|(key, _)| key.clone())
                .collect()
        };
        let mut plans = Vec::new();
        let mut attempted = Vec::new();
        for key in keys {
            if let Some(plan) = Self::build_session_plan_locked(&data, &key) {
                attempted.push((key, plan.dirty_generation));
                plans.push(plan);
            }
        }
        (plans, attempted, live)
    }

    fn finalize_persist(
        &self,
        attempted: Vec<(String, u64)>,
        written: HashSet<String>,
    ) {
        let started = std::time::Instant::now();
        let mut data = self.inner.data.write().unwrap();
        self.inner.metrics.lock_wait_ns.fetch_add(
            u64::try_from(started.elapsed().as_nanos()).unwrap_or(0),
            Ordering::Relaxed,
        );
        let mut retry = HashSet::new();
        for (key, generation) in attempted {
            if written.contains(&key) {
                if let Some(session) = data.sessions.get_mut(&key) {
                    if session.dirty_generation == generation {
                        session.dirty = false;
                    } else {
                        // Newer mutation landed during the write — keep dirty.
                        session.dirty = true;
                        retry.insert(key);
                    }
                }
            } else {
                if let Some(session) = data.sessions.get_mut(&key) {
                    session.dirty = true;
                }
                retry.insert(key);
            }
        }
        drop(data);
        if !retry.is_empty() {
            if let Some(persist) = &self.inner.persist {
                let mut queue = persist.queue.lock().unwrap();
                queue.dirty_sessions.extend(retry);
            }
        }
    }

    fn build_session_plan_locked(
        data: &StoreData,
        session_key: &str,
    ) -> Option<SessionPersistPlan> {
        let session = data.sessions.get(session_key)?;
        let mut ordered = session.owned_ids.iter().cloned().collect::<Vec<_>>();
        ordered.sort_by(|left, right| {
            let left_entry = data.entries.get(left);
            let right_entry = data.entries.get(right);
            let left_tip = session.live_tips.contains(left);
            let right_tip = session.live_tips.contains(right);
            right_tip
                .cmp(&left_tip)
                .then_with(|| {
                    right_entry
                        .map(|entry| entry.last_access_unix_ms)
                        .unwrap_or(0)
                        .cmp(&left_entry.map(|entry| entry.last_access_unix_ms).unwrap_or(0))
                })
                .then_with(|| left.cmp(right))
        });

        let mut entries = Vec::new();
        let mut kept_tips = HashSet::new();
        for id in ordered {
            let Some(entry) = data.entries.get(&id) else {
                continue;
            };
            entries.push(EntryPersistPlan {
                response_id: entry.response_id.clone(),
                parent_response_id: entry.parent_response_id.clone(),
                created_at_unix_ms: entry.created_at_unix_ms,
                last_access_unix_ms: entry.last_access_unix_ms,
                fidelity: entry.fidelity,
                size_bytes: entry.size_bytes,
                items: Arc::clone(&entry.items),
            });
            if session.live_tips.contains(&id) {
                kept_tips.insert(id);
            }
        }

        Some(SessionPersistPlan {
            session_key: session.key.clone(),
            anonymous: session.anonymous,
            last_access_unix_ms: session.last_access_unix_ms,
            // Only tips that still have a planned entry — no dangling ids.
            live_tips: kept_tips.into_iter().collect(),
            entries,
            dirty_generation: session.dirty_generation,
        })
    }

    fn expire_leases_locked(&self, data: &mut StoreData, now: u64) {
        let ttl = duration_ms(LEASE_TTL);
        let expired = data
            .leases
            .iter()
            .filter(|(_, lease)| now.saturating_sub(lease.created_at_unix_ms) > ttl)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in expired {
            if let Some(lease) = data.leases.remove(&id) {
                if let Some(parent) = data.entries.get_mut(&lease.parent_id) {
                    parent.lease_count = parent.lease_count.saturating_sub(1);
                }
            }
        }
    }

    fn expire_session_bindings_locked(data: &mut StoreData, now: u64) {
        let ttl = duration_ms(LEASE_TTL);
        data.session_bindings
            .retain(|_, (_, created)| now.saturating_sub(*created) <= ttl);
        while data.session_bindings.len() > MAX_SESSION_BINDINGS {
            let oldest = data
                .session_bindings
                .iter()
                .min_by_key(|(_, (_, created))| *created)
                .map(|(id, _)| *id);
            let Some(oldest) = oldest else {
                break;
            };
            data.session_bindings.remove(&oldest);
        }
    }

    fn issue_session_binding_locked(data: &mut StoreData, key: &str, now: u64) -> u64 {
        Self::expire_session_bindings_locked(data, now);
        let mut token = uuid::Uuid::new_v4().as_u128() as u64;
        if token == 0 {
            token = 1;
        }
        while data.session_bindings.contains_key(&token) {
            token = token.wrapping_add(1).max(1);
        }
        data.session_bindings
            .insert(token, (key.to_string(), now));
        // Hard cap: never leave more than MAX_SESSION_BINDINGS, excluding nothing
        // except we prefer dropping older tokens (including same-ms ties).
        while data.session_bindings.len() > MAX_SESSION_BINDINGS {
            let oldest = data
                .session_bindings
                .iter()
                .filter(|(id, _)| **id != token)
                .min_by_key(|(id, (_, created))| (*created, *id))
                .map(|(id, _)| *id);
            let Some(oldest) = oldest else {
                // Only the new token remains but len still > max — impossible unless max=0.
                break;
            };
            data.session_bindings.remove(&oldest);
        }
        debug_assert!(data.session_bindings.len() <= MAX_SESSION_BINDINGS);
        token
    }

    fn enforce_session_count_locked(
        data: &mut StoreData,
        now: u64,
        metrics: &ResponseStateMetrics,
    ) {
        // Always drop empty sessions when over the hard cap (or expired).
        let empty: Vec<String> = data
            .sessions
            .iter()
            .filter(|(_, session)| session.owned_ids.is_empty())
            .filter(|(_, session)| {
                now.saturating_sub(session.last_access_unix_ms) > duration_ms(ANON_SESSION_TTL)
                    || data.sessions.len() > MAX_SESSIONS
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in empty {
            data.sessions.remove(&key);
        }

        let mut skipped = HashSet::new();
        let mut guard = 0;
        while data.sessions.len() > MAX_SESSIONS && guard < MAX_SESSIONS.saturating_mul(4) {
            guard += 1;
            let victim = data
                .sessions
                .iter()
                .filter(|(key, _)| !skipped.contains(*key))
                .filter(|(_, session)| session.owned_ids.is_empty())
                .min_by_key(|(_, session)| session.last_access_unix_ms)
                .map(|(key, _)| key.clone())
                .or_else(|| {
                    data.sessions
                        .iter()
                        .filter(|(key, _)| !skipped.contains(*key))
                        .filter(|(_, session)| {
                            session.owned_ids.iter().all(|id| {
                                data.entries
                                    .get(id)
                                    .map(|entry| entry.lease_count == 0)
                                    .unwrap_or(true)
                            })
                        })
                        .min_by_key(|(_, session)| session.last_access_unix_ms)
                        .map(|(key, _)| key.clone())
                });
            let Some(victim) = victim else {
                break;
            };
            let owned: Vec<String> = data
                .sessions
                .get(&victim)
                .map(|session| session.owned_ids.iter().cloned().collect())
                .unwrap_or_default();
            let mut blocked = false;
            for id in owned {
                if data
                    .entries
                    .get(&id)
                    .is_some_and(|entry| entry.lease_count > 0)
                {
                    blocked = true;
                    break;
                }
                Self::delete_entry_locked(data, &id, metrics);
            }
            if blocked {
                skipped.insert(victim);
                continue;
            }
            if data
                .sessions
                .get(&victim)
                .is_some_and(|session| session.owned_ids.is_empty())
            {
                data.sessions.remove(&victim);
            } else {
                skipped.insert(victim);
            }
        }
        // Final sweep of empty sessions while over the hard cap.
        while data.sessions.len() > MAX_SESSIONS {
            let empty = data
                .sessions
                .iter()
                .filter(|(_, session)| session.owned_ids.is_empty())
                .min_by_key(|(_, session)| session.last_access_unix_ms)
                .map(|(key, _)| key.clone());
            let Some(empty) = empty else {
                break;
            };
            data.sessions.remove(&empty);
        }
    }

    fn expire_entries_locked(data: &mut StoreData, now: u64, metrics: &ResponseStateMetrics) {
        let default_ttl = duration_ms(RESPONSE_TTL);
        let anon_ttl = duration_ms(ANON_SESSION_TTL);
        let expired_ids = data
            .entries
            .iter()
            .filter_map(|(id, entry)| {
                let session_anon = data
                    .sessions
                    .get(&entry.session_key)
                    .map(|session| session.anonymous)
                    .unwrap_or(entry.session_key.starts_with("anon:"));
                let ttl = if session_anon { anon_ttl } else { default_ttl };
                let base = entry.last_access_unix_ms.max(entry.created_at_unix_ms);
                (now.saturating_sub(base) > ttl).then(|| id.clone())
            })
            .collect::<Vec<_>>();
        for id in expired_ids {
            Self::delete_entry_locked(data, &id, metrics);
        }

        let expired_sessions = data
            .sessions
            .iter()
            .filter_map(|(key, session)| {
                let ttl = if session.anonymous {
                    anon_ttl
                } else {
                    default_ttl
                };
                (now.saturating_sub(session.last_access_unix_ms) > ttl
                    && session.owned_ids.is_empty())
                .then(|| key.clone())
            })
            .collect::<Vec<_>>();
        for key in expired_sessions {
            data.sessions.remove(&key);
        }
        Self::enforce_session_count_locked(data, now, metrics);
    }

    fn prune_linear_ancestors_locked(
        data: &mut StoreData,
        start_parent: &str,
        now: u64,
        metrics: &ResponseStateMetrics,
    ) {
        let grace = duration_ms(RETRY_PARENT_GRACE);
        // Keep start_parent itself (retry parent). Prune older ancestors that are
        // not tips, not leased, and past grace / have no live descendants.
        let mut cursor = data
            .entries
            .get(start_parent)
            .and_then(|entry| entry.parent_response_id.clone());
        let mut guard = 0;
        while let Some(id) = cursor {
            guard += 1;
            if guard > 10_000 {
                break;
            }
            let Some(entry) = data.entries.get(&id).cloned() else {
                break;
            };
            cursor = entry.parent_response_id.clone();

            let is_tip = data
                .sessions
                .get(&entry.session_key)
                .is_some_and(|session| session.live_tips.contains(&id));
            if is_tip || entry.lease_count > 0 {
                continue;
            }
            let has_live_child = data
                .children
                .get(&id)
                .is_some_and(|children| children.iter().any(|child| data.entries.contains_key(child)));
            // Always drop grandparents once a grandchild exists; for the immediate
            // parent we already stopped above. Here `id` is grandparent+.
            let age_ok = now.saturating_sub(entry.created_at_unix_ms) > grace || has_live_child;
            if age_ok && !is_tip {
                // If it still has children that exist, only delete when none of those
                // children are tips and the node is not needed as retry parent of a tip.
                let child_is_tip = data.children.get(&id).is_some_and(|children| {
                    children.iter().any(|child| {
                        data.sessions.values().any(|session| session.live_tips.contains(child))
                    })
                });
                if !child_is_tip {
                    Self::delete_entry_locked(data, &id, metrics);
                    metrics.tip_prunes.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        // Additionally: if the retry parent itself has been superseded long enough and
        // the child tip is healthy, we still keep it for RETRY_PARENT_GRACE. Global
        // eviction may remove it earlier under memory pressure.
        let _ = start_parent;
    }

    fn enforce_session_caps_locked(
        data: &mut StoreData,
        session_key: &str,
        metrics: &ResponseStateMetrics,
    ) {
        let Some(session) = data.sessions.get(session_key) else {
            return;
        };
        // Bound live tips (fork fan-out).
        if session.live_tips.len() > MAX_LIVE_TIPS_PER_SESSION {
            let mut tips = session
                .live_tips
                .iter()
                .cloned()
                .map(|id| {
                    let access = data
                        .entries
                        .get(&id)
                        .map(|entry| entry.last_access_unix_ms)
                        .unwrap_or(0);
                    (access, id)
                })
                .collect::<Vec<_>>();
            tips.sort();
            let drop_count = tips.len() - MAX_LIVE_TIPS_PER_SESSION;
            let drop_ids = tips
                .into_iter()
                .take(drop_count)
                .map(|(_, id)| id)
                .collect::<Vec<_>>();
            if let Some(session) = data.sessions.get_mut(session_key) {
                for id in &drop_ids {
                    session.live_tips.remove(id);
                }
                session.dirty = true;
                session.dirty_generation = session.dirty_generation.saturating_add(1);
            }
        }

        while data
            .sessions
            .get(session_key)
            .is_some_and(|session| session.owned_ids.len() > MAX_OWNED_PER_SESSION)
        {
            let Some(victim) = data.sessions.get(session_key).and_then(|session| {
                session
                    .owned_ids
                    .iter()
                    .filter(|id| !session.live_tips.contains(*id))
                    .filter_map(|id| {
                        data.entries.get(id).map(|entry| {
                            (
                                entry.lease_count,
                                entry.last_access_unix_ms,
                                id.clone(),
                            )
                        })
                    })
                    .filter(|(leases, _, _)| *leases == 0)
                    .min()
                    .map(|(_, _, id)| id)
            }) else {
                // All remaining are tips or leased — drop oldest non-leased tip.
                let tip_victim = data.sessions.get(session_key).and_then(|session| {
                    session
                        .owned_ids
                        .iter()
                        .filter_map(|id| {
                            data.entries.get(id).map(|entry| {
                                (entry.lease_count, entry.last_access_unix_ms, id.clone())
                            })
                        })
                        .filter(|(leases, _, _)| *leases == 0)
                        .min()
                        .map(|(_, _, id)| id)
                });
                let Some(tip_victim) = tip_victim else {
                    break;
                };
                Self::delete_entry_locked(data, &tip_victim, metrics);
                continue;
            };
            Self::delete_entry_locked(data, &victim, metrics);
        }
    }

    /// Returns true when the store is within hard caps after eviction.
    fn enforce_global_limits_locked(
        data: &mut StoreData,
        now: u64,
        metrics: &ResponseStateMetrics,
    ) -> bool {
        Self::expire_entries_locked(data, now, metrics);
        while data.entries.len() > MAX_STORED_RESPONSES
            || data.total_bytes > MAX_STORED_RESPONSE_BYTES
        {
            let victim = data
                .entries
                .iter()
                .filter(|(_, entry)| entry.lease_count == 0)
                .filter(|(id, entry)| {
                    !data
                        .sessions
                        .get(&entry.session_key)
                        .is_some_and(|session| session.live_tips.contains(*id))
                })
                .min_by_key(|(_, entry)| (entry.last_access_unix_ms, entry.created_at_unix_ms))
                .map(|(id, _)| id.clone())
                .or_else(|| {
                    // Last resort: evict oldest non-leased tip (including the sole entry).
                    data.entries
                        .iter()
                        .filter(|(_, entry)| entry.lease_count == 0)
                        .min_by_key(|(_, entry)| (entry.last_access_unix_ms, entry.created_at_unix_ms))
                        .map(|(id, _)| id.clone())
                });
            let Some(victim) = victim else {
                // Only leased entries remain and we are still over budget.
                return false;
            };
            Self::delete_entry_locked(data, &victim, metrics);
            metrics.evictions.fetch_add(1, Ordering::Relaxed);
        }
        true
    }

    fn delete_entry_locked(data: &mut StoreData, id: &str, metrics: &ResponseStateMetrics) {
        let Some(entry) = data.entries.remove(id) else {
            return;
        };
        data.total_bytes = data.total_bytes.saturating_sub(entry.size_bytes);
        if let Some(parent) = entry.parent_response_id.as_ref() {
            if let Some(children) = data.children.get_mut(parent) {
                children.remove(id);
                if children.is_empty() {
                    data.children.remove(parent);
                }
            }
        }
        data.children.remove(id);
        if let Some(session) = data.sessions.get_mut(&entry.session_key) {
            session.owned_ids.remove(id);
            session.live_tips.remove(id);
            session.dirty = true;
            session.dirty_generation = session.dirty_generation.saturating_add(1);
        }
        // Drop leases targeting this id.
        data.leases.retain(|_, lease| lease.parent_id != id);
        let _ = metrics;
    }

    #[cfg(test)]
    fn force_created_at_for_test(&self, response_id: &str, created_at_unix_ms: u64) {
        let mut data = self.inner.data.write().unwrap();
        if let Some(entry) = data.entries.get_mut(response_id) {
            entry.created_at_unix_ms = created_at_unix_ms;
            entry.last_access_unix_ms = created_at_unix_ms;
        }
    }

    #[cfg(test)]
    fn entry_count_for_test(&self) -> usize {
        self.inner.data.read().unwrap().entries.len()
    }

    #[cfg(test)]
    fn live_tips_for_test(&self, session_key: &str) -> Vec<String> {
        let data = self.inner.data.read().unwrap();
        data.sessions
            .get(session_key)
            .map(|session| session.live_tips.iter().cloned().collect())
            .unwrap_or_default()
    }
}

impl Drop for ResponseStateInner {
    fn drop(&mut self) {
        if let Some(persist) = &self.persist {
            let data = self.data.read().unwrap();
            let live: HashSet<String> = data.sessions.keys().cloned().collect();
            let plans = live
                .iter()
                .filter_map(|key| ResponseStateStore::build_session_plan_locked(&data, key))
                .collect::<Vec<_>>();
            drop(data);
            let files = materialize_session_files(plans, &self.metrics);
            let _ = persist.persist_snapshot(files, &live, true, &self.metrics);
        }
    }
}

fn materialize_session_files(
    plans: Vec<SessionPersistPlan>,
    metrics: &ResponseStateMetrics,
) -> Vec<PersistedSessionFile> {
    let mut files = Vec::with_capacity(plans.len());
    for plan in plans {
        let mut entries = Vec::new();
        let mut file_bytes = 0_usize;
        let mut live_tips = HashSet::<String>::new();
        for entry in plan.entries {
            let mut fidelity = entry.fidelity;
            let mut items = None;
            match entry.fidelity {
                ReplayFidelity::Exact | ReplayFidelity::Lossy => {
                    if entry.size_bytes <= SNAPSHOT_ENTRY_MAX_BYTES
                        && !entry.items.is_empty()
                    {
                        // Re-persist Lossy payloads as-is so restart keeps them.
                        items = Some((*entry.items).clone());
                    } else if entry.fidelity == ReplayFidelity::Exact {
                        let (stripped, changed) = strip_items_for_disk(&entry.items);
                        let stripped_size = measure_items(&stripped);
                        if changed && stripped_size <= SNAPSHOT_ENTRY_MAX_BYTES {
                            items = Some(stripped);
                            fidelity = ReplayFidelity::Lossy;
                        } else {
                            fidelity = ReplayFidelity::Unavailable;
                            metrics
                                .oversized_disk_skips
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    } else {
                        // Lossy but still too large: demote to unavailable on disk.
                        fidelity = ReplayFidelity::Unavailable;
                        metrics
                            .oversized_disk_skips
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
                ReplayFidelity::Unavailable => {
                    items = None;
                }
            }
            let persisted_size = items
                .as_ref()
                .map(|values| measure_items(values))
                .unwrap_or(0);
            let persisted = PersistedEntry {
                response_id: entry.response_id.clone(),
                parent_response_id: entry.parent_response_id.clone(),
                created_at_unix_ms: entry.created_at_unix_ms,
                last_access_unix_ms: entry.last_access_unix_ms,
                fidelity,
                // Store resident payload size so reload accounting stays accurate.
                size_bytes: persisted_size,
                items,
            };
            let encoded = serde_json::to_vec(&persisted).unwrap_or_default();
            if file_bytes.saturating_add(encoded.len()) > SESSION_FILE_MAX_BYTES {
                // Do not record this tip id if its payload does not fit.
                continue;
            }
            file_bytes = file_bytes.saturating_add(encoded.len());
            if plan.live_tips.iter().any(|tip| tip == &entry.response_id)
                && fidelity != ReplayFidelity::Unavailable
            {
                live_tips.insert(entry.response_id.clone());
            }
            if fidelity != ReplayFidelity::Unavailable || persisted.items.is_some() {
                entries.push(persisted);
            } else if plan.live_tips.iter().any(|tip| tip == &entry.response_id) {
                // Persist an unavailable stub for live tips so restart miss is explicit.
                entries.push(persisted);
            }
        }
        files.push(PersistedSessionFile {
            version: PERSISTED_STATE_VERSION,
            session_key: plan.session_key,
            anonymous: plan.anonymous,
            last_access_unix_ms: plan.last_access_unix_ms,
            live_tips: live_tips.into_iter().collect(),
            entries,
        });
    }
    files
}

impl PersistController {
    fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    fn load_into(&self, data: &mut StoreData) -> Result<(), String> {
        fs::create_dir_all(self.sessions_dir()).map_err(|error| error.to_string())?;
        self.quarantine_legacy()?;

        let mut loaded = 0_usize;
        let mut loaded_bytes = 0_u64;
        let mut files = Vec::new();
        let entries = fs::read_dir(self.sessions_dir()).map_err(|error| error.to_string())?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let meta = entry.metadata().ok();
            let modified = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let size = meta.map(|m| m.len()).unwrap_or(0);
            files.push((modified, size, path));
        }
        // Newest first so recent sessions win under load caps.
        files.sort_by(|left, right| right.0.cmp(&left.0));
        for (_, size, path) in files {
            if loaded >= MAX_SESSION_FILES_ON_LOAD {
                break;
            }
            if loaded_bytes.saturating_add(size) > MAX_LOAD_TOTAL_BYTES {
                break;
            }
            match load_session_file(&path) {
                Ok(file) => {
                    merge_session_file(data, file);
                    loaded += 1;
                    loaded_bytes = loaded_bytes.saturating_add(size);
                }
                Err(error) => {
                    crate::debug::log(&format!(
                        "skipping corrupt response-state session {}: {error}",
                        path.display()
                    ));
                }
            }
        }
        crate::debug::log(&format!(
            "loaded {loaded} response-state session file(s) ({loaded_bytes} bytes) from {}",
            self.sessions_dir().display()
        ));
        Ok(())
    }

    fn quarantine_legacy(&self) -> Result<(), String> {
        let Some(legacy) = &self.legacy_snapshot else {
            return Ok(());
        };
        let metadata = match fs::symlink_metadata(legacy) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.to_string()),
        };
        if !metadata.is_file() {
            return Ok(());
        }
        // Always quarantine the monolithic v1 file. v2 lives in sessions/.
        let stamp = unix_millis();
        let quarantine = legacy.with_file_name(format!(
            "response-state.json.quarantine-{stamp}"
        ));
        match fs::rename(legacy, &quarantine) {
            Ok(()) => {
                crate::debug::log(&format!(
                    "quarantined legacy response-state snapshot {} -> {} ({} bytes)",
                    legacy.display(),
                    quarantine.display(),
                    metadata.len()
                ));
                if metadata.len() > MAX_LEGACY_FILE_BYTES {
                    crate::debug::log(
                        "legacy response-state exceeded load limit; starting with empty sessions",
                    );
                }
            }
            Err(error) => {
                crate::debug::log(&format!(
                    "could not quarantine legacy response-state {}: {error}",
                    legacy.display()
                ));
            }
        }
        Ok(())
    }

    /// Returns the set of session keys that were written successfully.
    fn persist_snapshot(
        &self,
        mut files: Vec<PersistedSessionFile>,
        live_sessions: &HashSet<String>,
        allow_tombstone: bool,
        metrics: &ResponseStateMetrics,
    ) -> HashSet<String> {
        let _guard = self.write.lock().unwrap();
        let mut written = HashSet::new();
        if let Err(error) = fs::create_dir_all(self.sessions_dir()) {
            metrics.persist_failures.fetch_add(1, Ordering::Relaxed);
            crate::debug::log(&format!(
                "cannot create response-state dir {}: {error}",
                self.sessions_dir().display()
            ));
            return written;
        }

        // Names of sessions that still exist in RAM — never delete these just
        // because this pass only rewrote a dirty subset.
        let live_names: HashSet<String> = live_sessions
            .iter()
            .map(|key| format!("{}.json", session_file_stem(key)))
            .collect();

        // Newest dirty sessions first within the write budget for this pass.
        files.sort_by(|left, right| {
            right
                .last_access_unix_ms
                .cmp(&left.last_access_unix_ms)
                .then_with(|| left.session_key.cmp(&right.session_key))
        });

        // Start from actual on-disk usage of currently live files.
        let mut budget_used = 0_u64;
        let mut existing_sizes: HashMap<String, u64> = HashMap::new();
        if let Ok(entries) = fs::read_dir(self.sessions_dir()) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                if live_names.contains(&name) {
                    existing_sizes.insert(name, meta.len());
                    budget_used = budget_used.saturating_add(meta.len());
                }
            }
        }

        for file in files {
            let encoded = match serde_json::to_vec(&file) {
                Ok(bytes) => bytes,
                Err(error) => {
                    metrics.persist_failures.fetch_add(1, Ordering::Relaxed);
                    crate::debug::log(&format!(
                        "cannot serialize response-state session {}: {error}",
                        file.session_key
                    ));
                    continue;
                }
            };
            let size = encoded.len() as u64;
            let stem = session_file_stem(&file.session_key);
            let name = format!("{stem}.json");
            let old = existing_sizes.get(&name).copied().unwrap_or(0);
            // Replace old size with new size in the budget projection.
            let projected = budget_used.saturating_sub(old).saturating_add(size);
            if projected > DISK_TOTAL_MAX_BYTES as u64 {
                metrics
                    .oversized_disk_skips
                    .fetch_add(1, Ordering::Relaxed);
                // Permanent for this generation: keep old file and clear dirty via
                // `written` so we do not spin forever. Next mutation dirties again.
                written.insert(file.session_key.clone());
                continue;
            }
            let path = self.sessions_dir().join(&name);
            match write_session_file_bytes_atomic(&path, &encoded) {
                Ok(()) => {
                    metrics.persist_writes.fetch_add(1, Ordering::Relaxed);
                    budget_used = projected;
                    existing_sizes.insert(name, size);
                    written.insert(file.session_key.clone());
                }
                Err(error) => {
                    // Keep the previous on-disk snapshot; do not mark written.
                    metrics.persist_failures.fetch_add(1, Ordering::Relaxed);
                    crate::debug::log(&format!(
                        "cannot persist response continuation state to {}: {error}",
                        path.display()
                    ));
                }
            }
        }

        // Full flushes may tombstone sessions no longer live in RAM. Dirty-only
        // passes must not, because `live_sessions` can be a slightly stale snapshot
        // relative to concurrent writers.
        if allow_tombstone {
            if let Ok(entries) = fs::read_dir(self.sessions_dir()) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                        continue;
                    }
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if live_names.contains(&name) {
                        continue;
                    }
                    let _ = fs::remove_file(&path);
                }
            }
        }

        let total_disk = dir_size(&self.sessions_dir()).unwrap_or(0);
        if total_disk > DISK_TOTAL_MAX_BYTES as u64 {
            // Never delete live session files in emergency GC.
            gc_disk_budget(
                &self.sessions_dir(),
                DISK_TOTAL_MAX_BYTES as u64,
                &live_names,
            );
        }
        written
    }
}

fn load_session_file(path: &Path) -> Result<PersistedSessionFile, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("session path is not a regular file".into());
    }
    if metadata.len() > (SESSION_FILE_MAX_BYTES as u64) + 64 * 1024 {
        return Err("session file exceeds the configured limit".into());
    }
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    let file: PersistedSessionFile =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if file.version != PERSISTED_STATE_VERSION {
        return Err(format!("unsupported state version {}", file.version));
    }
    Ok(file)
}

fn merge_session_file(data: &mut StoreData, file: PersistedSessionFile) {
    let now = unix_millis();
    let session = data
        .sessions
        .entry(file.session_key.clone())
        .or_insert_with(|| SessionShard {
            key: file.session_key.clone(),
            anonymous: file.anonymous,
            live_tips: HashSet::new(),
            owned_ids: HashSet::new(),
            last_access_unix_ms: file.last_access_unix_ms,
            dirty: false,
            dirty_generation: 0,
        });
    session.anonymous = file.anonymous;
    session.last_access_unix_ms = session.last_access_unix_ms.max(file.last_access_unix_ms);
    let declared_tips: HashSet<String> = file.live_tips.into_iter().collect();

    for persisted in file.entries {
        if data.entries.contains_key(&persisted.response_id) {
            continue;
        }
        let items = match persisted.fidelity {
            ReplayFidelity::Exact | ReplayFidelity::Lossy => {
                Arc::new(persisted.items.unwrap_or_default())
            }
            ReplayFidelity::Unavailable => Arc::new(Vec::new()),
        };
        // Always recompute from resident payload so accounting matches RAM.
        let size_bytes = measure_items(&items);
        if persisted.fidelity == ReplayFidelity::Unavailable || items.is_empty() {
            // Skip empty/unavailable shells — expand would miss anyway.
            continue;
        }
        let entry = Entry {
            response_id: persisted.response_id.clone(),
            parent_response_id: persisted.parent_response_id.clone(),
            session_key: file.session_key.clone(),
            items,
            size_bytes,
            created_at_unix_ms: persisted.created_at_unix_ms,
            last_access_unix_ms: persisted.last_access_unix_ms.max(1).min(now),
            fidelity: persisted.fidelity,
            lease_count: 0,
        };
        if let Some(parent) = entry.parent_response_id.clone() {
            data.children
                .entry(parent)
                .or_default()
                .insert(entry.response_id.clone());
        }
        data.total_bytes = data.total_bytes.saturating_add(size_bytes);
        if let Some(session) = data.sessions.get_mut(&file.session_key) {
            session.owned_ids.insert(entry.response_id.clone());
            if declared_tips.contains(&entry.response_id) {
                session.live_tips.insert(entry.response_id.clone());
            }
        }
        data.entries.insert(entry.response_id.clone(), entry);
    }
}

fn write_session_file_atomic(path: &Path, file: &PersistedSessionFile) -> Result<u64, String> {
    let content = serde_json::to_vec(file).map_err(|error| error.to_string())?;
    write_session_file_bytes_atomic(path, &content)?;
    Ok(content.len() as u64)
}

fn write_session_file_bytes_atomic(path: &Path, content: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "session path has no parent directory".to_string())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = parent.join(format!(
        ".response-session-{}-{}.tmp",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut out = options.open(&temporary).map_err(|error| error.to_string())?;
    if let Err(error) = out.write_all(content).and_then(|_| out.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    drop(out);
    // Never delete the destination before rename. On Unix, rename replaces
    // atomically. If rename fails, keep the previous snapshot and drop the temp.
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    #[cfg(unix)]
    {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

fn dir_size(path: &Path) -> Result<u64, String> {
    let mut total = 0_u64;
    for entry in fs::read_dir(path).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let metadata = entry.metadata().map_err(|error| error.to_string())?;
        if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

fn gc_disk_budget(dir: &Path, budget: u64, protect_names: &HashSet<String>) {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if protect_names.contains(&name) {
            continue;
        }
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        files.push((modified, metadata.len(), path));
    }
    files.sort_by_key(|(modified, _, _)| *modified);
    let mut total: u64 = dir_size(dir).unwrap_or(0);
    for (_, size, path) in files {
        if total <= budget {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

/// Extract a stable, bounded, opaque session/thread key from common Codex /
/// client headers. The raw header value is never used as a routing-map key:
/// hashing prevents delimiter injection into scoped keys and keeps logs/state
/// from retaining caller-controlled identifiers.
pub fn session_key_from_headers(headers: &axum::http::HeaderMap) -> Option<String> {
    const KEYS: &[&str] = &[
        "x-codex-thread-id",
        "x-thread-id",
        "thread-id",
        "x-session-id",
        "session-id",
        "x-codex-session-id",
    ];
    for key in KEYS {
        if let Some(value) = headers
            .get(*key)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if let Some(key) = opaque_session_key(value) {
                return Some(key);
            }
        }
    }
    // Parent thread is a fallback for subagents that omit their own id.
    headers
        .get("x-codex-parent-thread-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(opaque_session_key)
}

fn opaque_session_key(value: &str) -> Option<String> {
    if value.len() > MAX_SESSION_KEY_LEN || value.chars().any(char::is_control) {
        return None;
    }
    let digest = Sha256::digest(value.as_bytes());
    Some(format!("session-{:x}", digest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};
    use serde_json::json;

    #[test]
    fn session_header_is_opaque_and_stable() {
        let mut headers = HeaderMap::new();
        headers.insert("x-codex-thread-id", HeaderValue::from_static("thread-1"));
        let first = session_key_from_headers(&headers).expect("session key");
        let second = session_key_from_headers(&headers).expect("session key");
        assert_eq!(first, second);
        assert_ne!(first, "thread-1");
        assert!(first.starts_with("session-"));
    }

    #[test]
    fn invalid_primary_session_header_falls_back_to_parent() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-codex-thread-id",
            HeaderValue::from_str(&"x".repeat(MAX_SESSION_KEY_LEN + 1)).unwrap(),
        );
        headers.insert(
            "x-codex-parent-thread-id",
            HeaderValue::from_static("parent-1"),
        );
        let key = session_key_from_headers(&headers).expect("parent session key");
        assert_ne!(key, "parent-1");
        assert!(key.starts_with("session-"));
    }

    #[test]
    fn remember_and_expand_restores_full_input() {
        let store = ResponseStateStore::default();
        let request = json!({
            "model": "gpt-5.6-sol",
            "store": false,
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
        });
        let response = json!({
            "id": "resp_1",
            "status": "completed",
            "output": [{"type": "function_call", "call_id": "call_1", "name": "update_plan"}]
        });
        store.remember(&request, &response, false);
        let mut body = json!({
            "previous_response_id": "resp_1",
            "input": [{"type": "function_call_output", "call_id": "call_1", "output": "Plan updated"}]
        });
        assert!(!store.expand_previous_response_input(&mut body));

        store.remember(&request, &response, true);
        assert!(store.expand_previous_response_input(&mut body));
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[2]["type"], "function_call_output");
    }

    #[test]
    fn tool_call_lookup_is_scoped_to_one_response() {
        let store = ResponseStateStore::default();
        store.remember(
            &json!({"input": []}),
            &json!({
                "id": "resp_other",
                "status": "completed",
                "output": [{"type": "custom_tool_call", "call_id": "call_reused"}]
            }),
            true,
        );
        assert!(store
            .find_tool_call_in_response("resp_other", "call_reused", "custom_tool_call")
            .is_some());
        assert!(store
            .find_tool_call_in_response("resp_missing", "call_reused", "custom_tool_call")
            .is_none());
    }

    #[test]
    fn only_completed_responses_are_recorded() {
        let store = ResponseStateStore::default();
        let request = json!({"input": [{"type": "message", "role": "user", "content": "hi"}]});
        store.remember(
            &request,
            &json!({"id": "resp_failed", "status": "failed", "output": []}),
            true,
        );
        store.remember(
            &request,
            &json!({"id": "resp_incomplete", "status": "incomplete", "output": [], "incomplete_details": {"reason": "content_filter"}}),
            true,
        );
        let mut body = json!({"previous_response_id": "resp_failed", "input": []});
        assert!(!store.expand_previous_response_input(&mut body));
        let mut body = json!({"previous_response_id": "resp_incomplete", "input": []});
        assert!(!store.expand_previous_response_input(&mut body));
    }

    #[test]
    fn expansion_of_unknown_id_leaves_body_untouched() {
        let store = ResponseStateStore::default();
        let mut body = json!({"previous_response_id": "resp_unknown", "input": [1, 2, 3]});
        assert!(!store.expand_previous_response_input(&mut body));
        assert_eq!(body["input"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn string_input_is_expanded_as_user_message() {
        let store = ResponseStateStore::default();
        store.remember(
            &json!({"input": "plain string"}),
            &json!({"id": "resp_1", "status": "completed", "output": []}),
            true,
        );
        let mut body = json!({"previous_response_id": "resp_1", "input": []});
        assert!(store.expand_previous_response_input(&mut body));
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"], "plain string");
    }

    #[test]
    fn expansion_prepends_before_delta_input() {
        let store = ResponseStateStore::default();
        store.remember(
            &json!({"input": [{"type": "message", "role": "user", "content": "a"}]}),
            &json!({"id": "resp_1", "status": "completed", "output": [{"type": "reasoning", "id": "rs_1"}]}),
            true,
        );
        let mut body = json!({
            "previous_response_id": "resp_1",
            "input": [{"type": "message", "role": "user", "content": "b"}]
        });
        assert!(store.expand_previous_response_input(&mut body));
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["content"], "a");
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[2]["content"], "b");
    }

    #[test]
    fn remember_replaces_existing_entry_and_evicts_oldest() {
        let store = ResponseStateStore::default();
        store.remember(
            &json!({"input": []}),
            &json!({"id": "resp_1", "status": "completed", "output": [1]}),
            true,
        );
        store.remember(
            &json!({"input": []}),
            &json!({"id": "resp_1", "status": "completed", "output": [2]}),
            true,
        );
        let mut body = json!({"previous_response_id": "resp_1", "input": []});
        assert!(store.expand_previous_response_input(&mut body));
        assert_eq!(body["input"][0], 2);
    }

    #[test]
    fn linear_chain_prunes_grandparents_but_keeps_retry_parent() {
        let store = ResponseStateStore::default();
        store.attach_session_hint(
            &mut json!({}), // unused; set via remember path
            "thread-a",
        );
        let mut req1 = json!({"input": [{"type": "message", "role": "user", "content": "t1"}]});
        store.attach_session_hint(&mut req1, "thread-a");
        store.remember(
            &req1,
            &json!({"id": "resp_1", "status": "completed", "output": [{"type": "message", "content": "a1"}]}),
            true,
        );

        let mut cont2 = json!({
            "previous_response_id": "resp_1",
            "input": [{"type": "message", "role": "user", "content": "t2"}]
        });
        assert!(store
            .expand_previous_response_input_with_hint(&mut cont2, Some("thread-a"))
            .expanded());
        store.remember(
            &cont2,
            &json!({"id": "resp_2", "status": "completed", "output": [{"type": "message", "content": "a2"}]}),
            true,
        );

        let mut cont3 = json!({
            "previous_response_id": "resp_2",
            "input": [{"type": "message", "role": "user", "content": "t3"}]
        });
        assert!(store
            .expand_previous_response_input_with_hint(&mut cont3, Some("thread-a"))
            .expanded());
        store.remember(
            &cont3,
            &json!({"id": "resp_3", "status": "completed", "output": [{"type": "message", "content": "a3"}]}),
            true,
        );

        // Grandparent should be pruned; parent kept for retry; tip is resp_3.
        let mut body = json!({"previous_response_id": "resp_1", "input": []});
        assert!(!store.expand_previous_response_input(&mut body));
        let mut body = json!({"previous_response_id": "resp_2", "input": []});
        assert!(store.expand_previous_response_input(&mut body));
        let mut body = json!({"previous_response_id": "resp_3", "input": []});
        assert!(store.expand_previous_response_input(&mut body));
        assert!(store.entry_count_for_test() <= 2);
    }

    #[test]
    fn sibling_branches_remain_expandable() {
        let store = ResponseStateStore::default();
        let mut root = json!({"input": [{"type": "message", "role": "user", "content": "root"}]});
        store.attach_session_hint(&mut root, "thread-fork");
        store.remember(
            &root,
            &json!({"id": "resp_root", "status": "completed", "output": []}),
            true,
        );

        let mut branch_a = json!({
            "previous_response_id": "resp_root",
            "input": [{"type": "message", "role": "user", "content": "A"}]
        });
        assert!(store
            .expand_previous_response_input_with_hint(&mut branch_a, Some("thread-fork"))
            .expanded());
        store.remember(
            &branch_a,
            &json!({"id": "resp_a", "status": "completed", "output": [{"type": "message", "content": "a"}]}),
            true,
        );

        let mut branch_b = json!({
            "previous_response_id": "resp_root",
            "input": [{"type": "message", "role": "user", "content": "B"}]
        });
        assert!(store
            .expand_previous_response_input_with_hint(&mut branch_b, Some("thread-fork"))
            .expanded());
        store.remember(
            &branch_b,
            &json!({"id": "resp_b", "status": "completed", "output": [{"type": "message", "content": "b"}]}),
            true,
        );

        let mut body_a = json!({"previous_response_id": "resp_a", "input": []});
        let mut body_b = json!({"previous_response_id": "resp_b", "input": []});
        assert!(store.expand_previous_response_input(&mut body_a));
        assert!(store.expand_previous_response_input(&mut body_b));
        let tips = store.live_tips_for_test("thread-fork");
        assert!(tips.contains(&"resp_a".to_string()));
        assert!(tips.contains(&"resp_b".to_string()));
    }

    #[test]
    fn anonymous_roots_do_not_evict_each_other() {
        let store = ResponseStateStore::default();
        store.remember(
            &json!({"input": [{"type": "message", "role": "user", "content": "A"}]}),
            &json!({"id": "resp_a", "status": "completed", "output": []}),
            true,
        );
        store.remember(
            &json!({"input": [{"type": "message", "role": "user", "content": "B"}]}),
            &json!({"id": "resp_b", "status": "completed", "output": []}),
            true,
        );
        store.remember(
            &json!({"input": [{"type": "message", "role": "user", "content": "C"}]}),
            &json!({"id": "resp_c", "status": "completed", "output": []}),
            true,
        );
        let mut body = json!({"previous_response_id": "resp_a", "input": []});
        assert!(store.expand_previous_response_input(&mut body));
        let mut body = json!({"previous_response_id": "resp_b", "input": []});
        assert!(store.expand_previous_response_input(&mut body));
    }

    #[test]
    fn persisted_state_survives_store_recreation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("response-state.json");
        {
            let store = ResponseStateStore::new(Some(path.clone()));
            let mut request = json!({"input": [{"type": "message", "role": "user", "content": "before restart"}]});
            store.attach_session_hint(&mut request, "thread-persist");
            store.remember(
                &request,
                &json!({"id": "resp_persisted", "status": "completed", "output": [{"type": "function_call", "call_id": "call_persisted", "name": "lookup", "arguments": "{}"}]}),
                true,
            );
            store.flush();
        }

        let restored = ResponseStateStore::new(Some(path));
        let mut body = json!({
            "previous_response_id": "resp_persisted",
            "input": [{"type": "function_call_output", "call_id": "call_persisted", "output": "ok"}]
        });
        assert!(restored.expand_previous_response_input(&mut body));
        assert_eq!(body["input"][0]["content"], "before restart");
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["type"], "function_call_output");
    }

    #[test]
    fn flush_persists_without_dropping_the_store() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("response-state.json");
        let store = ResponseStateStore::new(Some(path.clone()));
        store.remember(
            &json!({"input": [{"type": "message", "role": "user", "content": "flushed"}]}),
            &json!({"id": "resp_flush", "status": "completed", "output": []}),
            true,
        );
        store.flush();

        let restored = ResponseStateStore::new(Some(path));
        let mut body = json!({"previous_response_id": "resp_flush", "input": []});
        assert!(restored.expand_previous_response_input(&mut body));
        assert_eq!(body["input"][0]["content"], "flushed");
    }

    #[test]
    fn expired_entries_are_not_expanded() {
        let store = ResponseStateStore::default();
        store.remember(
            &json!({"input": [{"type": "message", "role": "user", "content": "stale"}]}),
            &json!({"id": "resp_stale", "status": "completed", "output": []}),
            true,
        );
        store.force_created_at_for_test("resp_stale", 1);
        let mut body = json!({"previous_response_id": "resp_stale", "input": []});
        assert!(!store.expand_previous_response_input(&mut body));
        assert!(body["input"].as_array().unwrap().is_empty());
    }

    #[test]
    fn legacy_monolithic_file_is_quarantined() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("response-state.json");
        fs::write(
            &path,
            br#"{"version":1,"responses":{"resp_old":{"created_at_unix_ms":1,"items":[],"size_bytes":0}}}"#,
        )
        .unwrap();
        let _store = ResponseStateStore::new(Some(path.clone()));
        assert!(!path.exists());
        let quarantined = fs::read_dir(directory.path())
            .unwrap()
            .flatten()
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("response-state.json.quarantine-")
            });
        assert!(quarantined);
    }

    #[test]
    fn strip_heavy_payloads_redacts_data_urls() {
        let value = json!({
            "type": "function_call_output",
            "output": format!("data:image/png;base64,{}", "a".repeat(4096))
        });
        let (stripped, changed) = strip_heavy_payloads(&value);
        assert!(changed);
        let text = stripped["output"].as_str().unwrap();
        assert!(text.contains("omitted"));
        assert!(!text.contains("base64,aaaa"));
    }

    #[test]
    fn metrics_track_hits_and_misses() {
        let store = ResponseStateStore::default();
        store.remember(
            &json!({"input": [{"type": "message", "role": "user", "content": "m"}]}),
            &json!({"id": "resp_m", "status": "completed", "output": []}),
            true,
        );
        let mut hit = json!({"previous_response_id": "resp_m", "input": []});
        assert_eq!(
            store.expand_previous_response_input_with_hint(&mut hit, None),
            ExpandOutcome::Exact
        );
        let mut miss = json!({"previous_response_id": "resp_missing", "input": []});
        assert_eq!(
            store.expand_previous_response_input_with_hint(&mut miss, None),
            ExpandOutcome::Miss("unknown_id")
        );
        let snap = store.metrics_snapshot();
        assert_eq!(snap.expand_hits, 1);
        assert_eq!(snap.expand_misses, 1);
        assert_eq!(snap.remembers, 1);
        assert_eq!(snap.entry_count, 1);
    }

    #[test]
    fn client_injected_lease_is_ignored() {
        let store = ResponseStateStore::default();
        store.remember(
            &json!({"input": [{"type": "message", "role": "user", "content": "a"}]}),
            &json!({"id": "resp_a", "status": "completed", "output": []}),
            true,
        );
        // Forged lease must not steal parent lineage.
        let forged = json!({
            "input": [{"type": "message", "role": "user", "content": "b"}],
            "_codetas_replay_lease": 1u64,
            "_codetas_session_key": "stolen"
        });
        store.remember(
            &forged,
            &json!({"id": "resp_b", "status": "completed", "output": []}),
            true,
        );
        let mut body = json!({"previous_response_id": "resp_b", "input": []});
        assert!(store.expand_previous_response_input(&mut body));
        // Root without valid lease becomes its own anon session, not "stolen".
        assert!(store.live_tips_for_test("stolen").is_empty());
        // Forged binding must not create a real session named "stolen".
        assert!(store.entry_count_for_test() >= 1);
    }

    #[test]
    fn oversized_entry_is_not_kept_as_exact_payload() {
        let store = ResponseStateStore::default();
        let huge = "x".repeat(MAX_ENTRY_BYTES + 1024);
        store.remember(
            &json!({"input": [{"type": "message", "role": "user", "content": huge}]}),
            &json!({"id": "resp_huge", "status": "completed", "output": []}),
            true,
        );
        let mut body = json!({"previous_response_id": "resp_huge", "input": []});
        assert_eq!(
            store.expand_previous_response_input_with_hint(&mut body, None),
            ExpandOutcome::Miss("unavailable")
        );
        assert!(store.metrics_snapshot().rejected_oversize >= 1);
    }

    #[test]
    fn lossy_entries_survive_flush_cycle() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("response-state.json");
        {
            let store = ResponseStateStore::new(Some(path.clone()));
            let mut request = json!({"input": [{"type": "message", "role": "user", "content": "hi"}]});
            store.attach_session_hint(&mut request, "thread-lossy");
            // Force a lossy disk form by remembering a moderate entry then rewriting fidelity.
            store.remember(
                &request,
                &json!({"id": "resp_lossy", "status": "completed", "output": [{"type": "message", "content": "ok"}]}),
                true,
            );
            {
                let mut data = store.inner.data.write().unwrap();
                if let Some(entry) = data.entries.get_mut("resp_lossy") {
                    entry.fidelity = ReplayFidelity::Lossy;
                }
            }
            store.flush();
        }
        let restored = ResponseStateStore::new(Some(path));
        let mut body = json!({"previous_response_id": "resp_lossy", "input": []});
        assert_eq!(
            restored.expand_previous_response_input_with_hint(&mut body, None),
            ExpandOutcome::Lossy
        );
        assert_eq!(body["input"][0]["content"], "hi");
    }

    #[test]
    fn expand_strips_client_private_fields_before_issuing_lease() {
        let store = ResponseStateStore::default();
        store.remember(
            &json!({"input": [{"type": "message", "role": "user", "content": "z"}]}),
            &json!({"id": "resp_z", "status": "completed", "output": []}),
            true,
        );
        let mut body = json!({
            "previous_response_id": "resp_z",
            "input": [],
            "_codetas_replay_lease": 999u64,
            "_codetas_session_key": "attacker"
        });
        assert_eq!(
            store.expand_previous_response_input_with_hint(&mut body, Some("thread-z")),
            ExpandOutcome::Exact
        );
        // Gateway-issued lease replaces the forged one.
        assert_ne!(body["_codetas_replay_lease"], 999);
        // Session is an opaque binding token, not a client string.
        assert!(body["_codetas_session_key"].as_u64().is_some());
    }

    #[test]
    fn lossy_parent_propagates_to_child() {
        let store = ResponseStateStore::default();
        let mut request = json!({"input": [{"type": "message", "role": "user", "content": "parent"}]});
        store.attach_session_hint(&mut request, "thread-lossy-child");
        store.remember(
            &request,
            &json!({"id": "resp_parent", "status": "completed", "output": [{"type": "message", "content": "p"}]}),
            true,
        );
        {
            let mut data = store.inner.data.write().unwrap();
            data.entries.get_mut("resp_parent").unwrap().fidelity = ReplayFidelity::Lossy;
        }
        let mut cont = json!({
            "previous_response_id": "resp_parent",
            "input": [{"type": "message", "role": "user", "content": "child"}]
        });
        assert_eq!(
            store.expand_previous_response_input_with_hint(&mut cont, None),
            ExpandOutcome::Lossy
        );
        store.remember(
            &cont,
            &json!({"id": "resp_child", "status": "completed", "output": [{"type": "message", "content": "c"}]}),
            true,
        );
        let mut body = json!({"previous_response_id": "resp_child", "input": []});
        assert_eq!(
            store.expand_previous_response_input_with_hint(&mut body, None),
            ExpandOutcome::Lossy
        );
    }

    #[test]
    fn session_bindings_stay_bounded_under_churn() {
        let store = ResponseStateStore::default();
        for i in 0..(MAX_SESSION_BINDINGS * 3) {
            let mut body = json!({});
            store.attach_session_hint(&mut body, &format!("thread-{i}"));
            // Drop body without remember — binding would leak without a cap.
        }
        let bindings = store.inner.data.read().unwrap().session_bindings.len();
        assert!(bindings <= MAX_SESSION_BINDINGS);
    }

    #[test]
    fn expand_does_not_attach_unused_session_binding_on_miss() {
        let store = ResponseStateStore::default();
        let before = store.inner.data.read().unwrap().session_bindings.len();
        let mut body = json!({"previous_response_id": "resp_missing", "input": []});
        assert_eq!(
            store.expand_previous_response_input_with_hint(&mut body, Some("thread-miss")),
            ExpandOutcome::Miss("unknown_id")
        );
        let after = store.inner.data.read().unwrap().session_bindings.len();
        assert_eq!(before, after);
    }

    #[test]
    fn lease_less_parent_id_does_not_create_child_lineage() {
        let store = ResponseStateStore::default();
        store.remember(
            &json!({"input": [{"type": "message", "role": "user", "content": "p"}]}),
            &json!({"id": "resp_p", "status": "completed", "output": []}),
            true,
        );
        {
            let mut data = store.inner.data.write().unwrap();
            data.entries.get_mut("resp_p").unwrap().fidelity = ReplayFidelity::Lossy;
        }
        // No expand/lease — bare previous_response_id must not link as child.
        store.remember(
            &json!({
                "previous_response_id": "resp_p",
                "input": [{"type": "message", "role": "user", "content": "c"}]
            }),
            &json!({"id": "resp_c", "status": "completed", "output": []}),
            true,
        );
        let data = store.inner.data.read().unwrap();
        let child = data.entries.get("resp_c").unwrap();
        assert!(child.parent_response_id.is_none());
        // Not an upgraded Lossy child of resp_p; independent root tip.
        assert_eq!(child.session_key, "anon:resp_c");
    }

    #[test]
    fn session_binding_cap_holds_with_identical_timestamps() {
        let store = ResponseStateStore::default();
        let now = unix_millis();
        {
            let mut data = store.inner.data.write().unwrap();
            for i in 0..(MAX_SESSION_BINDINGS + 32) {
                // Force same timestamp to stress oldest selection ties.
                data.session_bindings.insert(i as u64 + 1, (format!("t{i}"), now));
            }
            assert!(data.session_bindings.len() > MAX_SESSION_BINDINGS);
            let _ = ResponseStateStore::issue_session_binding_locked(&mut data, "fresh", now);
            assert!(data.session_bindings.len() <= MAX_SESSION_BINDINGS);
        }
    }

    #[test]
    fn partial_flush_does_not_delete_other_session_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("response-state.json");
        let store = ResponseStateStore::new(Some(path.clone()));
        let mut a = json!({"input": [{"type": "message", "role": "user", "content": "A"}]});
        store.attach_session_hint(&mut a, "thread-a");
        store.remember(
            &a,
            &json!({"id": "resp_a", "status": "completed", "output": []}),
            true,
        );
        let mut b = json!({"input": [{"type": "message", "role": "user", "content": "B"}]});
        store.attach_session_hint(&mut b, "thread-b");
        store.remember(
            &b,
            &json!({"id": "resp_b", "status": "completed", "output": []}),
            true,
        );
        store.flush();

        // Dirty only A and flush again.
        let mut a2 = json!({"previous_response_id": "resp_a", "input": [{"type": "message", "role": "user", "content": "A2"}]});
        assert!(store.expand_previous_response_input(&mut a2));
        store.remember(
            &a2,
            &json!({"id": "resp_a2", "status": "completed", "output": []}),
            true,
        );
        store.flush();

        let restored = ResponseStateStore::new(Some(path));
        let mut body_b = json!({"previous_response_id": "resp_b", "input": []});
        assert!(restored.expand_previous_response_input(&mut body_b));
        let mut body_a = json!({"previous_response_id": "resp_a2", "input": []});
        assert!(restored.expand_previous_response_input(&mut body_a));
    }

    #[test]
    fn expired_lease_cannot_be_consumed() {
        let store = ResponseStateStore::default();
        store.remember(
            &json!({"input": [{"type": "message", "role": "user", "content": "p"}]}),
            &json!({"id": "resp_p", "status": "completed", "output": []}),
            true,
        );
        let mut cont = json!({"previous_response_id": "resp_p", "input": []});
        assert!(store.expand_previous_response_input(&mut cont));
        let lease_id = cont["_codetas_replay_lease"].as_u64().unwrap();
        {
            let mut data = store.inner.data.write().unwrap();
            data.leases.get_mut(&lease_id).unwrap().created_at_unix_ms = 1;
        }
        store.remember(
            &cont,
            &json!({"id": "resp_child", "status": "completed", "output": []}),
            true,
        );
        let data = store.inner.data.read().unwrap();
        let child = data.entries.get("resp_child").unwrap();
        // Expired lease must not grant parent lineage / fidelity inheritance.
        assert!(child.parent_response_id.is_none());
        assert_eq!(child.fidelity, ReplayFidelity::Exact);
    }
}
