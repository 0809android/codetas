//! Content-free, bounded per-session cache monitoring. Never stores prompts or tool output.
use super::ObservationEvent;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::{BufRead, BufReader},
    path::Path,
};

const FILE: &str = "cache-health.json";
const MAX_BYTES: u64 = 1024 * 1024;
const MAX_SESSIONS: usize = 128;
const RETENTION_MS: u64 = 7 * 86_400_000;
const CHECKPOINT_MS: u64 = 30_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheHealthPolicy {
    pub min_input_tokens: u64,
    pub window_requests: usize,
    pub threshold_percent: u8,
    pub recovery_percent: u8,
    pub cooldown_ms: u64,
}
impl Default for CacheHealthPolicy {
    fn default() -> Self {
        Self {
            min_input_tokens: 100_000,
            window_requests: 20,
            threshold_percent: 90,
            recovery_percent: 95,
            cooldown_ms: 30 * 60_000,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCacheHealth {
    pub session_id: String,
    pub provider_id: String,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_binding: Option<String>,
    pub model: String,
    pub successful_requests: u64,
    pub measured_requests: u64,
    pub unknown_cache_requests: u64,
    pub input_tokens: u64,
    pub measured_input_tokens: u64,
    pub cached_input_tokens: u64,
    pub last_input_tokens: u64,
    pub first_event_at_ms: u64,
    pub last_event_at_ms: u64,
    pub cache_hit_percent: Option<f64>,
    pub rolling_hit_percent: Option<f64>,
    pub rolling_requests: usize,
    pub active_alert: bool,
    pub alert_sequence: u64,
    pub last_alert_at_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheHealthSnapshot {
    pub monitor_id: String,
    pub monitoring: bool,
    pub policy: CacheHealthPolicy,
    pub sessions: Vec<SessionCacheHealth>,
    pub persistence_error: Option<String>,
    pub retention_days: u8,
    pub max_sessions: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    #[serde(default)]
    notification_pending: bool,
    row: SessionCacheHealth,
    samples: VecDeque<(u64, u64)>,
    recent_request_ids: VecDeque<String>,
}
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct CacheHealthState {
    version: u8,
    monitor_id: String,
    monitoring: bool,
    last_sequence: u64,
    next_alert: u64,
    entries: BTreeMap<String, Entry>,
    #[serde(skip)]
    last_checkpoint_ms: u64,
    #[serde(skip)]
    dirty: bool,
    #[serde(skip)]
    urgent_checkpoint: bool,
    #[serde(skip)]
    persistence_error: Option<String>,
}
impl Default for CacheHealthState {
    fn default() -> Self {
        Self {
            version: 1,
            monitor_id: uuid::Uuid::new_v4().to_string(),
            monitoring: true,
            last_sequence: 0,
            next_alert: 1,
            entries: BTreeMap::new(),
            last_checkpoint_ms: 0,
            dirty: false,
            urgent_checkpoint: false,
            persistence_error: None,
        }
    }
}
fn ratio(cached: u64, input: u64) -> Option<f64> {
    (input > 0).then(|| cached as f64 * 100.0 / input as f64)
}
fn release_pending(entry: &mut Entry, monitoring: bool, now: u64, policy: &CacheHealthPolicy, next: &mut u64) -> bool {
    let row = &mut entry.row;
    if !monitoring || !entry.notification_pending || !row.active_alert
        || entry.samples.len() != policy.window_requests
        || now.saturating_sub(row.last_event_at_ms) > 30 * 60_000
        || row.last_alert_at_ms.is_some_and(|at| now.saturating_sub(at) < policy.cooldown_ms) {
        return false;
    }
    row.alert_sequence = *next;
    *next = next.saturating_add(1);
    row.last_alert_at_ms = Some(now);
    entry.notification_pending = false;
    true
}

impl CacheHealthState {
    pub(super) fn set_monitoring(&mut self, enabled: bool) {
        if self.monitoring != enabled {
            self.dirty = true;
            self.urgent_checkpoint = true;
            for entry in self.entries.values_mut() {
                entry.samples.clear();
                self.urgent_checkpoint |= entry.row.active_alert;
                entry.row.active_alert = false;
                entry.notification_pending = false;
                entry.row.rolling_requests = 0;
                entry.row.rolling_hit_percent = None;
            }
        }
        self.monitoring = enabled;
    }
    pub(super) fn include(&mut self, event: &ObservationEvent, enabled: bool) -> bool {
        self.set_monitoring(enabled);
        self.last_sequence = self.last_sequence.max(event.ledger_sequence);
        if !enabled || event.usage_suppressed || event.attempt_only || event.shadow {
            return false;
        }
        let Some(thread) = event.client_thread_id.as_deref().or_else(|| event
            .request_diagnostic.as_ref().and_then(|d| d.client_thread_id.as_deref()))
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
            .map(|id| id.to_string())
        else {
            return false;
        };
        if event.outcome == "success"
            && event
                .recovery_kinds
                .iter()
                .any(|s| s.contains("compaction"))
        {
            for entry in self
                .entries
                .values_mut()
                .filter(|e| e.row.session_id == thread)
            {
                entry.samples.clear();
                entry.row.rolling_requests = 0;
                entry.row.rolling_hit_percent = None;
                self.urgent_checkpoint |= entry.row.active_alert;
                entry.row.active_alert = false;
                entry.notification_pending = false;
            }
            self.dirty = true;
            return false;
        }
        if event.outcome != "success"
            || event.status_code >= 400
            || event.send_count == 0
            || event.request_id.len() > 128
        {
            return false;
        }
        let Some(provider) = event
            .provider_id
            .as_deref()
            .filter(|s| !s.is_empty() && s.len() <= 128)
        else {
            return false;
        };
        let model = event
            .upstream_model
            .as_deref()
            .unwrap_or(&event.exposed_model);
        if model.is_empty() || model.len() > 256 {
            return false;
        }
        let account = event.account_id.as_deref().unwrap_or("");
        if account.len() > 128 {
            return false;
        }
        // Length-encoded JSON keys avoid delimiter collisions across models/accounts.
        let binding = event.request_diagnostic.as_ref().and_then(|d| d.auth_binding.as_deref()).unwrap_or("");
        if !binding.is_empty() && (binding.len() != 64 || !binding.bytes().all(|b| b.is_ascii_hexdigit())) { return false; }
        let key = serde_json::to_string(&(&thread, provider, model, account, binding)).unwrap();
        self.entries.retain(|_, e| {
            event.timestamp_ms.saturating_sub(e.row.last_event_at_ms) <= RETENTION_MS
        });
        if !self.entries.contains_key(&key) && self.entries.len() >= MAX_SESSIONS {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.row.last_event_at_ms)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        let entry = self.entries.entry(key).or_insert_with(|| Entry {
            notification_pending: false,
            row: SessionCacheHealth {
                session_id: thread,
                provider_id: provider.into(),
                account_id: (!account.is_empty()).then(|| account.to_string()),
                auth_binding: (!binding.is_empty()).then(|| binding.to_string()),
                model: model.into(),
                successful_requests: 0,
                measured_requests: 0,
                unknown_cache_requests: 0,
                input_tokens: 0,
                measured_input_tokens: 0,
                cached_input_tokens: 0,
                last_input_tokens: 0,
                first_event_at_ms: event.timestamp_ms,
                last_event_at_ms: event.timestamp_ms,
                cache_hit_percent: None,
                rolling_hit_percent: None,
                rolling_requests: 0,
                active_alert: false,
                alert_sequence: 0,
                last_alert_at_ms: None,
            },
            samples: VecDeque::new(),
            recent_request_ids: VecDeque::new(),
        });
        let request_digest = Sha256::digest(event.request_id.as_bytes());
        let request_key: String = request_digest[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if entry.recent_request_ids.contains(&request_key) {
            return false;
        }
        entry.recent_request_ids.push_back(request_key);
        while entry.recent_request_ids.len() > 64 {
            entry.recent_request_ids.pop_front();
        }
        let observation = event.cache_observation.clone()
            .unwrap_or_else(|| super::CacheObservation::from_usage(&event.usage));
        let row = &mut entry.row;
        if event.timestamp_ms.saturating_sub(row.last_event_at_ms) > 30 * 60_000 {
            entry.notification_pending = false;
            entry.samples.clear();
            row.rolling_requests = 0;
            row.rolling_hit_percent = None;
        }
        row.successful_requests = row.successful_requests.saturating_add(1);
        row.input_tokens = row.input_tokens.saturating_add(observation.input_tokens);
        row.last_input_tokens = observation.input_tokens;
        row.last_event_at_ms = row.last_event_at_ms.max(event.timestamp_ms);
        let policy = CacheHealthPolicy::default();
        self.dirty = true;
        if observation.input_tokens == 0 || !observation.cache_read_reported
            || observation.cached_input_tokens > observation.input_tokens
        {
            row.unknown_cache_requests = row.unknown_cache_requests.saturating_add(1);
            return false;
        }
        row.measured_requests = row.measured_requests.saturating_add(1);
        row.measured_input_tokens = row
            .measured_input_tokens
            .saturating_add(observation.input_tokens);
        row.cached_input_tokens = row
            .cached_input_tokens
            .saturating_add(observation.cached_input_tokens);
        row.cache_hit_percent = ratio(row.cached_input_tokens, row.measured_input_tokens);
        if observation.input_tokens < policy.min_input_tokens {
            return false;
        }
        entry
            .samples
            .push_back((observation.input_tokens, observation.cached_input_tokens));
        while entry.samples.len() > policy.window_requests {
            entry.samples.pop_front();
        }
        let (input, cached) = entry
            .samples
            .iter()
            .fold((0_u64, 0_u64), |(i, c), (ni, nc)| {
                (i.saturating_add(*ni), c.saturating_add(*nc))
            });
        row.rolling_requests = entry.samples.len();
        row.rolling_hit_percent = ratio(cached, input);
        if entry.samples.len() < policy.window_requests {
            return false;
        }
        // Resetting samples does not prove recovery. Only a measured recovery
        // or an explicit boundary/monitor stop resolves an existing warning.
        if u128::from(cached) * 100 >= u128::from(input) * u128::from(policy.recovery_percent) {
            self.urgent_checkpoint |= row.active_alert;
            row.active_alert = false;
            entry.notification_pending = false;
        } else if u128::from(cached) * 100 <= u128::from(input) * u128::from(policy.threshold_percent) {
            if !row.active_alert {
                self.urgent_checkpoint = true;
                row.active_alert = true;
                entry.notification_pending = true;
            }
        }
        if release_pending(entry, self.monitoring, event.timestamp_ms, &policy, &mut self.next_alert) {
            self.urgent_checkpoint = true;
            return true;
        }
        false
    }
    pub(super) fn snapshot(&self, now_ms: u64) -> CacheHealthSnapshot {
        let mut sessions: Vec<_> = self
            .entries
            .values()
            .filter(|e| now_ms.saturating_sub(e.row.last_event_at_ms) <= RETENTION_MS)
            .map(|e| e.row.clone())
            .collect();
        sessions.sort_by(|a, b| {
            b.active_alert
                .cmp(&a.active_alert)
                .then_with(|| b.last_event_at_ms.cmp(&a.last_event_at_ms))
        });
        CacheHealthSnapshot {
            monitor_id: self.monitor_id.clone(),
            monitoring: self.monitoring,
            policy: CacheHealthPolicy::default(),
            sessions,
            persistence_error: self.persistence_error.clone(),
            retention_days: 7,
            max_sessions: MAX_SESSIONS,
        }
    }
    fn checkpoint(directory: &Path) -> Self {
        let path = directory.join(FILE);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(_) => {
                return Self {
                    persistence_error: Some("cache monitor checkpoint could not be read".into()),
                    ..Default::default()
                }
            }
        };
        let state = (metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() <= MAX_BYTES)
            .then(|| fs::read(&path).ok())
            .flatten()
            .and_then(|b| serde_json::from_slice::<Self>(&b).ok())
            .filter(|s| {
                s.version == 1
                    && s.next_alert > 0
                    && s.entries.len() <= MAX_SESSIONS
                    && uuid::Uuid::parse_str(&s.monitor_id).is_ok()
                    && s.entries.values().all(|e| {
                        e.samples.len() <= 20
                            && e.recent_request_ids.len() <= 64
                            && e.recent_request_ids.iter().all(|id| id.len() <= 128)
                            && e.samples.iter().all(|(input, cached)| cached <= input)
                            && uuid::Uuid::parse_str(&e.row.session_id).is_ok()
                            && e.row.provider_id.len() <= 128
                            && e.row.model.len() <= 256
                    })
            });
        state.unwrap_or_else(|| Self { persistence_error: Some("cache monitor checkpoint is invalid or unavailable; live measurements will restore it".into()), ..Default::default() })
    }
    pub(super) fn load(directory: Option<&Path>) -> Self {
        let Some(directory) = directory else {
            return Self::default();
        };
        let mut state = Self::checkpoint(directory);
        let checkpoint = state.last_sequence;
        let mut events = Vec::new();
        for file in super::event_files(directory).unwrap_or_default() {
            if let Ok(file) = fs::File::open(file.path) {
                for line in BufReader::new(file).lines().map_while(Result::ok) {
                    if let Ok(event) = serde_json::from_str::<ObservationEvent>(&line) {
                        if event.ledger_sequence > checkpoint {
                            events.push(event);
                        }
                    }
                }
            }
        }
        events.sort_by_key(|e| e.ledger_sequence);
        for event in events {
            state.include(&event, state.monitoring);
        }
        state
    }
    pub(super) fn background_checkpoint_needed(&self, now: u64) -> bool {
        self.dirty || self.entries.values().any(|entry| entry.notification_pending
            && entry.row.active_alert && entry.samples.len() == CacheHealthPolicy::default().window_requests
            && now.saturating_sub(entry.row.last_event_at_ms) <= 30 * 60_000)
    }
    pub(super) fn release_pending_notifications(&mut self, now: u64) {
        let policy = CacheHealthPolicy::default();
        for entry in self.entries.values_mut() {
            if entry.notification_pending && (!self.monitoring || !entry.row.active_alert
                || now.saturating_sub(entry.row.last_event_at_ms) > 30 * 60_000) {
                entry.notification_pending = false;
                self.dirty = true;
            }
            if release_pending(entry, self.monitoring, now, &policy, &mut self.next_alert) {
                self.dirty = true;
                self.urgent_checkpoint = true;
            }
        }
    }
    pub(super) fn urgent_checkpoint_needed(&self) -> bool {
        self.dirty && self.urgent_checkpoint
    }
    pub(super) fn persist(&mut self, directory: &Path, now_ms: u64, force: bool) {
        self.release_pending_notifications(now_ms);
        if !self.dirty
            || (!(force || self.urgent_checkpoint)
                && now_ms.saturating_sub(self.last_checkpoint_ms) < CHECKPOINT_MS)
        {
            return;
        }
        let result = (|| -> Result<(), String> {
            super::cleanup::ensure_real_directory(directory)?;
            let path = directory.join(FILE);
            if fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink())
            {
                return Err("cache monitor checkpoint is not a regular owned file".into());
            }
            let bytes =
                serde_json::to_vec(self).map_err(|_| "cache monitor checkpoint encoding failed")?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err("cache monitor checkpoint exceeds its size limit".into());
            }
            let temp = directory.join(format!(".cache-health-{}.tmp", uuid::Uuid::new_v4()));
            super::cleanup::write_new_secure_file(&temp, &bytes)?;
            if let Err(error) = super::summary::replace_summary_file(&temp, &path) {
                let _ = fs::remove_file(temp);
                return Err(error.to_string());
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.last_checkpoint_ms = now_ms;
                self.dirty = false;
                self.urgent_checkpoint = false;
                self.persistence_error = None;
            }
            Err(_) => {
                self.persistence_error = Some("cache monitor checkpoint could not be saved".into())
            }
        }
    }
}

pub fn read_cache_health(directory: impl AsRef<Path>) -> CacheHealthSnapshot {
    CacheHealthState::checkpoint(directory.as_ref()).snapshot(ObservationEvent::now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ObservabilitySettings;
    use crate::observability::{ObservabilityLedger, RequestDiagnostic, TokenUsage};
    const NOW: u64 = 1_800_000_000_000;
    fn event(n: u64, input: u64, cached: u64) -> ObservationEvent {
        ObservationEvent {
                usage_suppressed: false,
            ledger_sequence: n,
            timestamp_ms: NOW + n * 1000,
            request_id: format!("request-{n}"),
            provider_id: Some("test".into()),
            upstream_model: Some("model".into()),
            upstream_endpoint: None,
            exposed_model: "test/model".into(),
            route_id: None,
            account_id: None,
            status_code: 200,
            outcome: "success".into(),
            failure_category: None,
            upstream_error: None,
            client_thread_id: None,
            cache_observation: None,
            request_diagnostic: Some(RequestDiagnostic {
                client_thread_id: Some("01a13ddb-8d21-7cb2-8f00-23a8a09af079".into()),
                ..Default::default()
            }),
            latency_ms: 1,
            attempts: 1,
            candidate_ordinal: 1,
            send_count: 1,
            recovery_kinds: vec![],
            recovery_kind: None,
            attempt_only: false,
            streaming: true,
            usage: TokenUsage {
                input_tokens: input,
                cached_input_tokens: cached,
                cache_read_reported: true,
                ..Default::default()
            },
            estimated_cost_usd: None,
            shadow: false,
            shadow_rule_id: None,
            parent_request_id: None,
        }
    }
    fn row(state: &CacheHealthState) -> SessionCacheHealth {
        state.snapshot(NOW + 100_000).sessions[0].clone()
    }

    #[test]
    fn cache_health_warms_up_then_alerts_at_inclusive_ninety_without_repeats() {
        let mut state = CacheHealthState::default();
        for n in 1..20 {
            assert!(!state.include(&event(n, 100_000, 90_000), true));
        }
        assert!(!row(&state).active_alert);
        assert!(state.include(&event(20, 100_000, 90_000), true));
        assert_eq!(row(&state).rolling_hit_percent, Some(90.0));
        let sequence = row(&state).alert_sequence;
        for n in 21..100 {
            assert!(!state.include(&event(n, 100_000, 0), true));
        }
        assert_eq!(row(&state).alert_sequence, sequence);
    }
    #[test]
    fn cache_health_uses_token_weighted_not_mean_request_percentages() {
        let mut state = CacheHealthState::default();
        for n in 1..20 {
            state.include(&event(n, 100_000, 99_000), true);
        }
        assert!(state.include(&event(20, 1_000_000, 0), true));
        assert!(row(&state).rolling_hit_percent.unwrap() < 65.0);
    }
    #[test]
    fn cache_health_excludes_unknown_cache_values_small_inputs_and_non_generating_events() {
        let mut state = CacheHealthState::default();
        for n in 1..=25 {
            let mut e = event(n, 100_000, 0);
            e.usage.cache_read_reported = false;
            assert!(!state.include(&e, true));
        }
        assert_eq!(row(&state).cache_hit_percent, None);
        assert_eq!(row(&state).unknown_cache_requests, 25);
        for n in 26..=50 {
            assert!(!state.include(&event(n, 99_999, 0), true));
        }
        assert_eq!(row(&state).rolling_requests, 0);
        for n in 51..=55 {
            let mut e = event(n, 100_000, 0);
            match n {
                51 => e.attempt_only = true,
                52 => e.shadow = true,
                53 => e.outcome = "failure".into(),
                54 => e.send_count = 0,
                _ => e.request_diagnostic = None,
            }
            assert!(!state.include(&e, true));
        }
        assert_eq!(row(&state).successful_requests, 50);
    }
    #[test]
    fn cache_health_separates_sessions_models_and_accounts_and_deduplicates_requests() {
        let mut state = CacheHealthState::default();
        for n in 1..=20 {
            let e = event(n, 100_000, 0);
            state.include(&e, true);
            assert!(!state.include(&e, true));
            let mut other = e.clone();
            other.request_id = format!("other-{n}");
            other.upstream_model = Some("other".into());
            other.usage.cached_input_tokens = 99_000;
            state.include(&other, true);
        }
        let rows = state.snapshot(NOW + 100_000).sessions;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].successful_requests, 20);
        assert_eq!(rows.iter().filter(|r| r.active_alert).count(), 1);
        let mut e = event(21, 100_000, 0);
        e.account_id = Some("another-account".into());
        state.include(&e, true);
        e.request_diagnostic.as_mut().unwrap().client_thread_id =
            Some(uuid::Uuid::new_v4().to_string());
        state.include(&e, true);
        assert_eq!(state.entries.len(), 4);
    }
    #[test]
    fn cache_health_recovers_at_ninety_five_and_throttles_new_episodes() {
        let mut state = CacheHealthState::default();
        for n in 1..=20 {
            state.include(&event(n, 100_000, 0), true);
        }
        assert!(row(&state).active_alert);
        for n in 21..=40 {
            state.include(&event(n, 100_000, 95_000), true);
        }
        assert!(!row(&state).active_alert);
        for n in 41..=60 {
            assert!(!state.include(&event(n, 100_000, 0), true));
        }
        assert!(row(&state).active_alert);
        assert_eq!(row(&state).alert_sequence, 1);
        let mut compact = event(61, 0, 0);
        compact.recovery_kinds = vec!["local-compaction".into()];
        compact.send_count = 0;
        state.include(&compact, true);
        assert!(!row(&state).active_alert);
        assert_eq!(row(&state).rolling_requests, 0);
        let mut fired = false;
        for n in 62..=81 {
            let mut e = event(n, 100_000, 0);
            e.timestamp_ms += 31 * 60_000;
            fired |= state.include(&e, true);
        }
        assert!(fired);
        assert_eq!(state.next_alert, 3);
    }
    #[test]
    fn cache_health_checkpoint_replays_only_new_events_and_is_content_free() {
        let directory = tempfile::tempdir().unwrap();
        let mut state = CacheHealthState::default();
        for n in 1..20 {
            state.include(&event(n, 100_000, 0), true);
        }
        state.persist(directory.path(), NOW + 20_000, true);
        let e = event(20, 100_000, 0);
        let line = serde_json::to_string(&e).unwrap();
        fs::write(
            directory
                .path()
                .join("events-00000000000000020833-000000.jsonl"),
            format!("{line}\n{line}\n"),
        )
        .unwrap();
        let mut restored = CacheHealthState::load(Some(directory.path()));
        assert_eq!(row(&restored).successful_requests, 20);
        assert!(row(&restored).active_alert);
        assert_eq!(restored.monitor_id, state.monitor_id);
        restored.persist(directory.path(), NOW + 30_000, true);
        let again = CacheHealthState::load(Some(directory.path()));
        assert_eq!(row(&again).successful_requests, 20);
        let text = fs::read_to_string(directory.path().join(FILE)).unwrap();
        assert!(!text.contains("tool_arguments"));
        assert!(!text.contains("prompt"));
    }
    #[tokio::test]
    async fn cache_health_is_continuously_collected_with_request_log_disabled_and_in_summary() {
        let directory = tempfile::tempdir().unwrap();
        let ledger = ObservabilityLedger::new(Some(directory.path().into()));
        for n in 1..=20 {
            ledger.record(event(n, 100_000, 0), ObservabilitySettings::default());
        }
        ledger.flush().await;
        let snapshot = ledger.summary().cache_health.unwrap();
        assert_eq!(snapshot.sessions[0].successful_requests, 20);
        assert!(snapshot.sessions[0].active_alert);
        let checkpoint = fs::read_to_string(directory.path().join("usage-totals.json")).unwrap();
        assert!(
            !checkpoint.contains("cacheHealth"),
            "live tables must not bloat the 64 KiB lifetime summary"
        );
        let recorded = read_cache_health(directory.path());
        assert_eq!(recorded.sessions.len(), 1);
    }
    #[test]
    fn cache_health_recovery_is_checkpointed_immediately_not_after_the_normal_interval() {
        let directory = tempfile::tempdir().unwrap();
        let mut state = CacheHealthState::default();
        for n in 1..=20 {
            state.include(&event(n, 100_000, 0), true);
        }
        state.persist(directory.path(), NOW + 20_000, false);
        assert!(read_cache_health(directory.path()).sessions[0].active_alert);
        for n in 21..=40 {
            state.include(&event(n, 100_000, 95_000), true);
        }
        state.persist(directory.path(), NOW + 40_000, false);
        assert!(!read_cache_health(directory.path()).sessions[0].active_alert);
    }

    #[test]
    fn cache_health_corrupt_checkpoint_is_reported_as_unavailable_not_healthy() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join(FILE), b"not valid JSON").unwrap();
        let snapshot = read_cache_health(directory.path());
        assert!(snapshot.persistence_error.is_some());
        assert!(snapshot.sessions.is_empty());
    }

    #[test]
    fn cache_health_missing_entire_usage_preserves_the_eligible_window() {
        let mut state = CacheHealthState::default();
        for i in 0..19 { state.include(&super::regression12_cache_tests::event(i,100000,true),true); }
        let mut missing = super::regression12_cache_tests::event(19,0,false);
        missing.cache_observation = None;
        missing.usage = super::super::TokenUsage::default();
        assert!(!state.include(&missing,true));
        assert!(state.include(&super::regression12_cache_tests::event(20,100000,true),true));
        let row = state.snapshot(1100).sessions.remove(0);
        assert_eq!(row.rolling_requests,20);
        assert_eq!(row.unknown_cache_requests,1);
        assert!(row.active_alert);
    }

    #[test]
    fn cache_health_unknown_small_and_idle_samples_do_not_claim_recovery() {
        for kind in ["unknown", "small", "idle"] {
            let mut state = CacheHealthState::default();
            for i in 0..20 { state.include(&super::regression12_cache_tests::event(i,100000,true),true); }
            let mut next = super::regression12_cache_tests::event(20,100000,true);
            match kind {
                "unknown" => next.cache_observation.as_mut().unwrap().cache_read_reported = false,
                "small" => next.cache_observation.as_mut().unwrap().input_tokens = 99999,
                _ => next.timestamp_ms += 31 * 60_000,
            }
            assert!(!state.include(&next,true));
            let row = state.snapshot(next.timestamp_ms).sessions.remove(0);
            assert!(row.active_alert);
            assert_eq!(row.alert_sequence,1);
            assert_eq!(row.rolling_requests,if kind == "idle" {1} else {20});
        }
    }

    #[test]
    fn cache_health_suppressed_new_episode_notifies_once_when_cooldown_expires() {
        let mut state = CacheHealthState::default();
        for n in 1..=20 { state.include(&event(n, 100_000, 0), true); }
        for n in 21..=40 { state.include(&event(n, 100_000, 95_000), true); }
        for n in 41..=60 { assert!(!state.include(&event(n, 100_000, 0), true)); }
        assert_eq!(row(&state).alert_sequence, 1);
        let directory = tempfile::tempdir().unwrap();
        state.persist(directory.path(), NOW + 1_821_000, false);
        assert_eq!(row(&state).alert_sequence, 2);
        state.persist(directory.path(), NOW + 1_822_000, false);
        assert_eq!(row(&state).alert_sequence, 2);
    }

    #[tokio::test]
    async fn cache_health_logging_disable_is_durable_without_another_request() {
        let directory = tempfile::tempdir().unwrap();
        let ledger = ObservabilityLedger::new(Some(directory.path().into()));
        for n in 1..=20 { ledger.record(event(n, 100_000, 0), ObservabilitySettings::default()); }
        ledger.flush().await;
        ledger.configure_cache_monitoring(false).await;
        let stopped = read_cache_health(directory.path());
        assert!(!stopped.monitoring); assert!(!stopped.sessions[0].active_alert);
        // A response carrying the old logging snapshot cannot re-enable monitoring.
        ledger.record(event(21, 100_000, 0), ObservabilitySettings::default());
        ledger.flush().await;
        assert!(!ledger.cache_health_summary().monitoring);
        assert_eq!(ledger.cache_health_summary().sessions[0].successful_requests, 20);
        let restarted = ObservabilityLedger::new(Some(directory.path().into()));
        restarted.configure_cache_monitoring(false).await;
        assert!(!restarted.cache_health_summary().monitoring);
        assert!(!restarted.cache_health_summary().sessions[0].active_alert);
    }

    #[tokio::test(start_paused = true)]
    async fn cache_health_checkpoint_exit_race_rechecks_dirty_and_preserves_registration() {
        let directory = tempfile::tempdir().unwrap();
        let ledger = ObservabilityLedger::new(Some(directory.path().into()));
        ledger.record(event(1, 100_000, 99_000), ObservabilitySettings::default());
        ledger.flush().await;
        ledger.record(event(2, 100_000, 99_000), ObservabilitySettings::default());
        while ledger.inner.pending_writes.load(std::sync::atomic::Ordering::Acquire) > 0 { tokio::task::yield_now().await; }
        let generation = ledger.inner.cache_checkpoint_task.lock().unwrap().generation;
        // The background I/O has just returned "clean", but its task has not exited.
        assert!(!crate::observability::persist_cache_checkpoint(&ledger.inner));
        ledger.record(event(3, 100_000, 99_000), ObservabilitySettings::default());
        while ledger.inner.pending_writes.load(std::sync::atomic::Ordering::Acquire) > 0 { tokio::task::yield_now().await; }
        assert!(crate::observability::finish_cache_checkpoint_round(&ledger.inner, generation));
        assert_eq!(ledger.inner.cache_checkpoint_task.lock().unwrap().generation, generation);
        assert_eq!(read_cache_health(directory.path()).sessions[0].successful_requests, 2);
        tokio::task::yield_now().await;
        let task = ledger.inner.cache_checkpoint_task.lock().unwrap().task.take().unwrap();
        tokio::time::advance(std::time::Duration::from_secs(30)).await;
        task.await.unwrap();
        assert_eq!(read_cache_health(directory.path()).sessions[0].successful_requests, 3);
        // A stale completion must not unregister a new worker after flush/restart.
        ledger.record(event(4, 100_000, 99_000), ObservabilitySettings::default());
        while ledger.inner.pending_writes.load(std::sync::atomic::Ordering::Acquire) > 0 { tokio::task::yield_now().await; }
        let newer = ledger.inner.cache_checkpoint_task.lock().unwrap().generation;
        assert_ne!(newer, generation);
        assert!(!crate::observability::finish_cache_checkpoint_round(&ledger.inner, generation));
        assert_eq!(ledger.inner.cache_checkpoint_task.lock().unwrap().generation, newer);
        assert!(ledger.inner.cache_checkpoint_task.lock().unwrap().task.is_some());
        ledger.flush().await;
    }

    #[tokio::test(start_paused = true)]
    async fn cache_health_dirty_last_event_is_saved_by_deadline_without_new_requests() {
        let directory = tempfile::tempdir().unwrap();
        let ledger = ObservabilityLedger::new(Some(directory.path().into()));
        ledger.record(event(1, 100_000, 99_000), ObservabilitySettings::default());
        ledger.flush().await;
        ledger.record(event(2, 100_000, 99_000), ObservabilitySettings::default());
        while ledger.inner.pending_writes.load(std::sync::atomic::Ordering::Acquire) > 0 {
            tokio::task::yield_now().await;
        }
        tokio::task::yield_now().await;
        assert_eq!(read_cache_health(directory.path()).sessions[0].successful_requests, 1);
        let task = ledger.inner.cache_checkpoint_task.lock().unwrap().task.take().unwrap();
        let start = tokio::time::Instant::now();
        tokio::time::advance(std::time::Duration::from_secs(30)).await;
        task.await.unwrap();
        assert!(start.elapsed() <= std::time::Duration::from_secs(30));
        assert_eq!(read_cache_health(directory.path()).sessions[0].successful_requests, 2);
        ledger.record(event(3, 100_000, 99_000), ObservabilitySettings::default());
        ledger.flush().await;
        assert_eq!(read_cache_health(directory.path()).sessions[0].successful_requests, 3);
    }

    #[test]
    fn cache_health_bounds_retained_sessions_and_does_not_record_disabled_usage() {
        let mut state = CacheHealthState::default();
        for n in 1..=150 {
            let mut e = event(n, 100_000, 0);
            e.request_diagnostic.as_mut().unwrap().client_thread_id =
                Some(uuid::Uuid::new_v4().to_string());
            state.include(&e, true);
        }
        assert_eq!(state.entries.len(), 128);
        state.include(&event(151, 100_000, 0), false);
        assert!(!state.monitoring);
        assert_eq!(state.entries.len(), 128);
    }
}

#[cfg(test)]
mod cache_counter_tests {
    use crate::observability::TokenUsage;
    use serde_json::json;
    #[test]
    fn missing_cache_counters_are_not_measured_zero_in_native_or_translated_usage() {
        for usage in [
            json!({"usage": {"input_tokens": 100_000}}),
            json!({"usageMetadata": {"promptTokenCount": 100_000}}),
            json!({"usage": crate::translate::chat_usage_to_responses(Some(&json!({"prompt_tokens": 100_000})))}),
        ] {
            assert!(!TokenUsage::from_json(&usage).cache_read_reported);
        }
        for usage in [
            json!({"usage": {"input_tokens": 100_000, "input_tokens_details": {"cached_tokens": 0}}}),
            json!({"usageMetadata": {"promptTokenCount": 100_000, "cachedContentTokenCount": 0}}),
            json!({"usage": crate::translate::chat_usage_to_responses(Some(&json!({
                "prompt_tokens": 100_000, "prompt_tokens_details": {"cached_tokens": 0}})))}),
        ] {
            assert!(TokenUsage::from_json(&usage).cache_read_reported);
        }
    }
    #[test]
    fn sparse_anthropic_final_usage_preserves_measured_cache_reads() {
        let mut state = crate::anthropic::AnthropicStreamState::default();
        crate::anthropic::anthropic_stream_to_chat(&json!({"type": "message_start", "message": {"usage": {
            "input_tokens": 20, "cache_read_input_tokens": 100_000, "cache_creation_input_tokens": 100
        }}}), &mut state, false).unwrap();
        let value = crate::anthropic::anthropic_stream_to_chat(&json!({"type": "message_delta",
            "delta": {"stop_reason": "end_turn"}, "usage": {"input_tokens": 30, "output_tokens": 1}
        }), &mut state, false).unwrap().unwrap();
        let usage = TokenUsage::from_json(&value);
        assert!(usage.cache_read_reported);
        assert_eq!(usage.cached_input_tokens, 100_000);
        assert_eq!(usage.input_tokens, 100_130);
    }
}

#[cfg(test)]
mod regression12_cache_tests {
    use super::*;
    use crate::observability::{CacheObservation, TokenUsage};

    pub(super) fn event(index: u64, input: u64, reported: bool) -> ObservationEvent {
        ObservationEvent {
                usage_suppressed: false,
            ledger_sequence:index, timestamp_ms:1000+index, request_id:format!("regression12-{index}"),
            client_thread_id:Some("01a13ddb-8d21-7cb2-8f00-23a8a09af079".into()),
            cache_observation:Some(CacheObservation {input_tokens:input,cached_input_tokens:0,cache_read_reported:reported}),
            provider_id:Some("test".into()), upstream_model:Some("wire-model".into()), upstream_endpoint:None,
            exposed_model:"test/model".into(),route_id:None,account_id:None,status_code:200,outcome:"success".into(),
            failure_category:None,upstream_error:None,request_diagnostic:None,latency_ms:1,attempts:1,
            candidate_ordinal:1,send_count:1,recovery_kinds:vec![],recovery_kind:None,attempt_only:false,
            streaming:false,usage:TokenUsage::default(),estimated_cost_usd:None,shadow:false,shadow_rule_id:None,parent_request_id:None,
        }
    }
    #[test]
    fn regression12_ineligible_samples_do_not_erase_valid_window() {
        let mut state = CacheHealthState::default();
        for index in 0..19 { assert!(!state.include(&event(index,100000,true),true)); }
        assert!(!state.include(&event(19,100000,false),true));
        assert!(!state.include(&event(20,99999,true),true));
        assert!(!state.include(&event(21,0,false),true));
        let mut invalid = event(22,100000,true);
        invalid.cache_observation.as_mut().unwrap().cached_input_tokens = 100001;
        assert!(!state.include(&invalid,true));
        let row = state.snapshot(1100).sessions.remove(0);
        assert_eq!(row.rolling_requests,19);
        assert_eq!(row.rolling_hit_percent,Some(0.0));
        assert_eq!(row.unknown_cache_requests,3);
        assert!(state.include(&event(23,100000,true),true));
        let row = state.snapshot(1100).sessions.remove(0);
        assert_eq!(row.rolling_requests,20);
        assert!(row.active_alert);
        assert!(!state.include(&event(24,100000,false),true));
        assert!(state.snapshot(1100).sessions[0].active_alert);
    }
    #[test]
    fn regression12_explicit_boundaries_still_reset_window() {
        for boundary in ["compact","monitor","idle"] {
            let mut state = CacheHealthState::default();
            for index in 0..19 { state.include(&event(index,100000,true),true); }
            let mut next = event(21,100000,true);
            match boundary {
                "compact" => { let mut e=event(20,0,false);e.recovery_kinds.push("explicit-compaction".into());state.include(&e,true); }
                "monitor" => {state.set_monitoring(false);state.set_monitoring(true);}
                _ => {next.timestamp_ms += 31*60_000;}
            }
            assert!(!state.include(&next,true));
            assert_eq!(state.snapshot(next.timestamp_ms).sessions[0].rolling_requests,1);
        }
    }
}

#[cfg(test)]
mod review14_partition_regressions {
    use super::*;
    #[test]
    fn review14_actual_auth_partitions_windows_without_changing_account_label() {
        let mut state=CacheHealthState::default();
        for index in 0..20 {
            for binding in ['a','b'] {
                let mut event=super::super::review14_privacy_regressions::event(&format!("{binding}-{index}"));
                event.request_diagnostic=Some(super::super::RequestDiagnostic {auth_binding:Some(binding.to_string().repeat(64)),..Default::default()});
                state.include(&event,true);
            }
        }
        let snapshot=state.snapshot(ObservationEvent::now_ms());assert_eq!(snapshot.sessions.len(),2);
        for row in &snapshot.sessions {assert_eq!(row.account_id.as_deref(),Some("label"));assert_eq!(row.rolling_requests,20);assert_eq!(row.successful_requests,20);assert!(row.auth_binding.is_some());}
        let mut legacy=serde_json::to_value(&snapshot.sessions[0]).unwrap();legacy.as_object_mut().unwrap().remove("authBinding");
        assert!(serde_json::from_value::<SessionCacheHealth>(legacy).unwrap().auth_binding.is_none());
    }
}

#[cfg(test)]
mod review15_pending_regressions {
    use super::*;
    fn sample(state: &mut CacheHealthState, n: u64, cached: u64, at: u64) -> bool {
        let mut event = super::super::review14_privacy_regressions::event(&format!("r15-{n}"));
        event.timestamp_ms = at;
        event.cache_observation.as_mut().unwrap().cached_input_tokens = cached;
        state.include(&event, true)
    }
    fn recurrence() -> CacheHealthState {
        let mut state = CacheHealthState::default();
        for n in 0..20 { sample(&mut state, n, 80_000, 1_000 + n); }
        assert_eq!(state.next_alert, 2);
        for n in 20..40 { sample(&mut state, n, 99_000, 1_000 + n); }
        assert!(!state.entries.values().next().unwrap().row.active_alert);
        for n in 40..60 { sample(&mut state, n, 80_000, 1_000 + n); }
        for n in 60..80 { sample(&mut state, n, 92_000, 1_000 + n); }
        let entry = state.entries.values().next().unwrap();
        assert!(entry.notification_pending && entry.row.active_alert);
        assert_eq!(entry.row.rolling_hit_percent, Some(92.0));
        state
    }
    #[test]
    fn review15_pending_releases_in_hysteresis_on_update_and_background() {
        for background in [false, true] {
            let mut state = recurrence();
            let due = 1_019 + CacheHealthPolicy::default().cooldown_ms;
            state.release_pending_notifications(due - 1);
            assert_eq!(state.next_alert, 2);
            if background { state.release_pending_notifications(due); }
            else { assert!(sample(&mut state, 81, 92_000, due)); }
            assert_eq!(state.next_alert, 3);
            assert!(!state.entries.values().next().unwrap().notification_pending);
            state.release_pending_notifications(due + 1);
            assert_eq!(state.next_alert, 3);
        }
    }
    #[test]
    fn review15_pending_recovery_disable_and_staleness_prevent_release() {
        for mode in 0..3 {
            let mut state = recurrence();
            let due = 1_019 + CacheHealthPolicy::default().cooldown_ms;
            if mode == 0 { state.set_monitoring(false); }
            if mode == 1 { for n in 80..100 { sample(&mut state, n, 99_000, 2_000 + n); } }
            state.release_pending_notifications(if mode == 2 { due + 60_001 } else { due });
            assert_eq!(state.next_alert, 2);
        }
    }
}
