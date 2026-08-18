use crate::config::{
    effective_model_capabilities, image_model_is_available, model_has_image_generation_identity,
    AccountPoolStrategy, AccountReference, CredentialSource, GatewaySettings, ProviderCapabilities,
    ProviderCredential, ProviderDefinition, RouteDefinition, RoutePolicySettings, RouteStrategy,
    RouteTarget,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const DEFAULT_FAILURE_THRESHOLD: u8 = 3;
const COOLDOWN: Duration = Duration::from_secs(60);
const MAX_HARD_RETRY_AFTER_COOLDOWN: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_ROUTING_RUNTIME_KEYS: usize = 4_096;
const SOFT_FAILURE_RETENTION: Duration = Duration::from_secs(5 * 60);
static NEXT_ROUTING_RUNTIME_EPOCH: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RoutePurpose {
    Normal,
    ImageGeneration,
}

impl RoutePurpose {
    fn cursor_key(self, route_id: &str) -> String {
        let purpose = match self {
            Self::Normal => "normal",
            Self::ImageGeneration => "image-generation",
        };
        format!("{route_id}:{purpose}")
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RouteCandidate {
    pub provider: ProviderDefinition,
    pub upstream_model: String,
    pub exposed_model: String,
    pub credential: Option<ProviderCredential>,
    pub account_id: Option<String>,
    pub target_key: String,
    pub route_id: Option<String>,
    pub failure_threshold: u8,
    pub quota_threshold_percent: u8,
    pub input_price_per_million: Option<f64>,
    pub output_price_per_million: Option<f64>,
    pub context_window: Option<u64>,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub reasoning_efforts: Vec<String>,
    pub default_reasoning_effort: Option<String>,
    pub capabilities: ProviderCapabilities,
    pub routing_epoch: u64,
    pub routing_generation: u64,
    /// Request-scoped session/task identifier used to isolate soft failures
    /// (5xx / provider_unreachable) per session. Hard quota cooldowns (429)
    /// remain scoped to the credential/account via `failure_key` and are
    /// intentionally **not** namespaced by session.
    pub session_scope: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteDryRunCandidate {
    pub rank: usize,
    pub target: String,
    pub account_id: Option<String>,
    pub eligible: bool,
    pub health_percent: u8,
    pub score: i64,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteDryRunReport {
    pub requested_model: String,
    pub selected: Option<String>,
    pub candidates: Vec<RouteDryRunCandidate>,
}

pub fn dry_run_route(
    settings: &GatewaySettings,
    requested_model: &str,
    is_subagent: bool,
) -> RouteDryRunReport {
    RoutingRuntime::default().dry_run(settings, requested_model, is_subagent)
}

#[derive(Clone, Debug)]
struct FailureState {
    consecutive: u8,
    retry_after: Option<Instant>,
    last_activity: Instant,
}

impl Default for FailureState {
    fn default() -> Self {
        Self {
            consecutive: 0,
            retry_after: None,
            last_activity: Instant::now(),
        }
    }
}

#[derive(Clone, Debug)]
struct RoutingGenerationState {
    generation: u64,
    active_leases: u32,
}

impl Default for RoutingGenerationState {
    fn default() -> Self {
        Self {
            generation: 0,
            active_leases: 0,
        }
    }
}

#[derive(Clone)]
pub(crate) struct RoutingRuntime {
    epoch: u64,
    route_calls: HashMap<String, u64>,
    account_calls: HashMap<String, u64>,
    target_requests: HashMap<String, u64>,
    quota_usage_percent: HashMap<String, u8>,
    transient_quota_exhausted: HashSet<String>,
    hard_cooldowns: HashMap<String, Instant>,
    routing_generations: HashMap<String, RoutingGenerationState>,
    failures: HashMap<String, FailureState>,
}

impl Default for RoutingRuntime {
    fn default() -> Self {
        Self {
            epoch: NEXT_ROUTING_RUNTIME_EPOCH.fetch_add(1, Ordering::Relaxed),
            route_calls: HashMap::new(),
            account_calls: HashMap::new(),
            target_requests: HashMap::new(),
            quota_usage_percent: HashMap::new(),
            transient_quota_exhausted: HashSet::new(),
            hard_cooldowns: HashMap::new(),
            routing_generations: HashMap::new(),
            failures: HashMap::new(),
        }
    }
}

impl RoutingRuntime {
    #[cfg(test)]
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    #[cfg(test)]
    pub(crate) fn active_attempts(&self, candidate: &RouteCandidate) -> u32 {
        self.routing_generations
            .get(&failure_key(candidate))
            .map_or(0, |state| state.active_leases)
    }

    pub fn dry_run(
        &mut self,
        settings: &GatewaySettings,
        requested_model: &str,
        is_subagent: bool,
    ) -> RouteDryRunReport {
        // A dry-run is an observation of the next routing decision. Evaluate it
        // on a snapshot so round-robin cursors, failure cleanup, usage counters,
        // quota state, and account cursors in the live runtime remain untouched.
        self.clone()
            .dry_run_snapshot(settings, requested_model, is_subagent)
    }

    fn dry_run_snapshot(
        &mut self,
        settings: &GatewaySettings,
        requested_model: &str,
        is_subagent: bool,
    ) -> RouteDryRunReport {
        let actual = self
            .candidates_for_request(settings, requested_model, is_subagent, None)
            .unwrap_or_default();
        if let Some(route) = settings.routes.iter().find(|route| {
            route.id == requested_model || route.alias.as_deref() == Some(requested_model)
        }) {
            return self.dry_run_route(settings, route, requested_model, &actual);
        }
        let rows = if actual.is_empty() {
            let error = self
                .candidates_for_request(settings, requested_model, is_subagent, None)
                .err()
                .unwrap_or_else(|| "no available candidate".into());
            vec![RouteDryRunCandidate {
                rank: 1,
                target: requested_model.to_string(),
                account_id: None,
                eligible: false,
                health_percent: 0,
                score: i64::MIN,
                reasons: vec![error],
            }]
        } else {
            actual
                .into_iter()
                .enumerate()
                .map(|(index, candidate)| {
                    let quota = self.effective_quota_usage(&failure_key(&candidate));
                    let health_percent = self.candidate_health_percent(&candidate);
                    RouteDryRunCandidate {
                        rank: index + 1,
                        target: candidate.target_key.clone(),
                        account_id: candidate.account_id.clone(),
                        eligible: true,
                        health_percent,
                        score: policy_score(&candidate, quota, true),
                        reasons: (health_percent < 100)
                            .then(|| format!("health:{health_percent}%"))
                            .into_iter()
                            .collect(),
                    }
                })
                .collect::<Vec<_>>()
        };
        RouteDryRunReport {
            requested_model: requested_model.to_string(),
            selected: rows
                .iter()
                .find(|candidate| candidate.eligible)
                .map(|candidate| candidate.target.clone()),
            candidates: rows,
        }
    }

    fn dry_run_route(
        &mut self,
        settings: &GatewaySettings,
        route: &RouteDefinition,
        requested_model: &str,
        actual: &[RouteCandidate],
    ) -> RouteDryRunReport {
        let mut rows = Vec::new();
        let mut original_rank = 0_usize;
        for target in &route.targets {
            let Some((provider_id, model_id)) = target.model.split_once('/') else {
                original_rank += 1;
                rows.push(RouteDryRunCandidate { rank: original_rank, target: target.model.clone(), account_id: None, eligible: false, health_percent: 0, score: i64::MIN, reasons: vec!["target must use provider/model".into()] });
                continue;
            };
            let Some(provider) = settings.providers.iter().find(|item| item.id == provider_id).cloned() else {
                original_rank += 1;
                rows.push(RouteDryRunCandidate { rank: original_rank, target: target.model.clone(), account_id: None, eligible: false, health_percent: 0, score: i64::MIN, reasons: vec!["provider missing".into()] });
                continue;
            };
            let mut accounts = settings.account_pool.accounts.iter()
                .filter(|account| account.provider_id == provider_id)
                .cloned().collect::<Vec<_>>();
            accounts.sort_by_key(|account| std::cmp::Reverse(account.priority));
            let pinned = accounts.iter().position(|account| account.pinned);
            let highest_priority = accounts.first().map(|account| account.priority);
            let preferred = pinned.or_else(|| {
                settings.account_pool.active_accounts.get(provider_id)
                    .and_then(|active| accounts.iter().position(|account| {
                        account.id == *active && Some(account.priority) == highest_priority
                    }))
            });
            if let Some(index) = preferred {
                accounts.rotate_left(index);
            }
            let probes = if accounts.is_empty() { vec![None] } else { accounts.into_iter().map(Some).collect() };
            for account in probes {
                original_rank += 1;
                let mut probe_settings = settings.clone();
                probe_settings.account_pool.strategy = AccountPoolStrategy::FillFirst;
                probe_settings.account_pool.active_accounts.clear();
                if let Some(selected) = account.as_ref() {
                    probe_settings.account_pool.accounts.retain(|item| item.id == selected.id);
                    if let Some(item) = probe_settings.account_pool.accounts.first_mut() {
                        item.enabled = true;
                        item.paused = false;
                        item.pause_until_unix = None;
                    }
                } else {
                    probe_settings.account_pool.accounts.retain(|item| item.provider_id != provider_id);
                }
                let key = account.as_ref().map(|item| format!("{}#{}", target.model, item.id)).unwrap_or_else(|| target.model.clone());
                let mut probe_runtime = self.clone();
                probe_runtime.failures.remove(&key);
                probe_runtime.quota_usage_percent.remove(&key);
                probe_runtime.transient_quota_exhausted.remove(&key);
                probe_runtime.hard_cooldowns.remove(&key);
                let candidate = probe_runtime.expand_accounts(
                    &probe_settings, provider.clone(), model_id.to_string(), requested_model.to_string(),
                    Some(route.id.clone()), route.failure_threshold, route.default_reasoning_effort.clone(),
                    None,
                ).ok().and_then(|items| items.into_iter().next());
                let mut reasons = Vec::new();
                if !route.enabled { reasons.push("route disabled".into()); }
                if !provider.enabled { reasons.push("provider disabled".into()); }
                if let Some(account) = account.as_ref() {
                    if !account.enabled { reasons.push("account disabled".into()); }
                    if account.paused {
                        match account.pause_until_unix {
                            Some(until) if until > unix_now() => reasons.push(format!("account paused until {until}")),
                            None => reasons.push("account paused".into()),
                            Some(_) => {}
                        }
                    }
                }
                if self.is_cooling(&key) { reasons.push("cooldown".into()); }
                let quota = self.effective_quota_usage(&key);
                if settings.account_pool.auto_switch_threshold_percent > 0
                    && quota >= settings.account_pool.auto_switch_threshold_percent {
                    reasons.push(format!("quota:{quota}%"));
                }
                if let Some(candidate) = candidate.as_ref() {
                    if let Some(reason) = policy_exclusion(candidate, &route.policy) { reasons.push(reason); }
                } else {
                    reasons.push("candidate could not be materialized".into());
                }
                let actual_rank = actual.iter().position(|candidate| {
                    candidate.target_key == target.model
                        && candidate.account_id.as_deref()
                            == account.as_ref().map(|item| item.id.as_str())
                });
                let eligible = actual_rank.is_some();
                let health_percent = candidate
                    .as_ref()
                    .map(|candidate| self.candidate_health_percent(candidate))
                    .unwrap_or(0);
                if eligible {
                    reasons.clear();
                }
                if health_percent < 100 {
                    reasons.push(format!("health:{health_percent}%"));
                }
                let score = candidate.as_ref().map(|candidate| {
                    if route.strategy == RouteStrategy::Policy { route_policy_score(candidate, quota, health_percent, &route.policy) }
                    else { policy_score(candidate, quota, eligible) }
                }).unwrap_or(i64::MIN);
                rows.push(RouteDryRunCandidate { rank: original_rank, target: target.model.clone(), account_id: account.map(|item| item.id), eligible, health_percent, score, reasons });
            }
        }
        rows.sort_by_key(|row| {
            actual
                .iter()
                .position(|candidate| {
                    candidate.target_key == row.target
                        && candidate.account_id.as_deref() == row.account_id.as_deref()
                })
                .map(|position| (0, position))
                .unwrap_or((1, row.rank))
        });
        RouteDryRunReport {
            requested_model: requested_model.to_string(),
            selected: actual.first().map(|candidate| candidate.target_key.clone()),
            candidates: rows,
        }
    }

    pub fn candidates_for_request(
        &mut self,
        settings: &GatewaySettings,
        requested_model: &str,
        is_subagent: bool,
        session_scope: Option<&str>,
    ) -> Result<Vec<RouteCandidate>, String> {
        let model_specific = settings
            .agents
            .subagent_fallback_by_model
            .get(requested_model);
        if is_subagent
            && settings.agents.multi_agent_v2
            && (!settings.agents.subagent_models.is_empty()
                || model_specific.is_some_and(|models| !models.is_empty())
                || !settings.agents.subagent_fallback.is_empty())
        {
            let mut candidates = Vec::new();
            let mut last_error = None;
            // Primary subagent roster first, then request-model-specific
            // fallback, then the common fallback roster.
            for model in settings
                .agents
                .subagent_models
                .iter()
                .chain(model_specific.into_iter().flatten())
                .chain(settings.agents.subagent_fallback.iter())
            {
                match self.candidates_scoped(settings, model, session_scope) {
                    Ok(mut model_candidates) => {
                        if !requested_model.trim().is_empty() {
                            for candidate in &mut model_candidates {
                                candidate.exposed_model = requested_model.to_string();
                            }
                        }
                        candidates.extend(model_candidates);
                    }
                    Err(error) => last_error = Some(error),
                }
            }
            let mut seen = HashSet::new();
            candidates.retain(|candidate| {
                seen.insert((candidate.target_key.clone(), candidate.account_id.clone()))
            });
            if !candidates.is_empty() {
                return Ok(candidates);
            }
            return Err(last_error.unwrap_or_else(|| {
                "the configured subagent model roster has no available target".into()
            }));
        }
        self.candidates_scoped(settings, requested_model, session_scope)
    }

    /// Resolve an image request without ever consulting the chat
    /// `default_provider`. An explicit sidecar target is authoritative. With no
    /// sidecar override, only the Codex-login OpenAI forward provider is
    /// eligible for an unqualified built-in image model such as `imagegen-2`.
    pub fn candidates_for_image_generation(
        &mut self,
        settings: &GatewaySettings,
        configured_target: Option<&str>,
        requested_model: Option<&str>,
        session_scope: Option<&str>,
    ) -> Result<Vec<RouteCandidate>, String> {
        if let Some(target) = configured_target.map(str::trim).filter(|target| !target.is_empty()) {
            return self.image_capable_candidates_for_explicit_target(
                settings,
                target,
                session_scope,
            );
        }

        let requested_model = requested_model.map(str::trim).unwrap_or_default();
        if requested_model.is_empty() {
            return Ok(Vec::new());
        }
        if requested_model.contains('/')
            || settings.routes.iter().any(|route| {
                route.enabled
                    && (route.id == requested_model
                        || route.alias.as_deref() == Some(requested_model))
            })
        {
            return self.image_capable_candidates_for_explicit_target(
                settings,
                requested_model,
                session_scope,
            );
        }

        let Some(provider) = settings.providers.iter().find(|provider| {
            provider.id == "openai"
                && provider.credential.source == CredentialSource::Forward
                && image_model_is_available(settings, provider, requested_model)
        }) else {
            return Ok(Vec::new());
        };
        self.expand_accounts(
            settings,
            provider.clone(),
            requested_model.to_string(),
            requested_model.to_string(),
            None,
            DEFAULT_FAILURE_THRESHOLD,
            None,
            session_scope,
        )
    }

    fn image_capable_candidates_for_explicit_target(
        &mut self,
        settings: &GatewaySettings,
        target: &str,
        session_scope: Option<&str>,
    ) -> Result<Vec<RouteCandidate>, String> {
        let candidates = if let Some(route) = settings.routes.iter().find(|route| {
            route.enabled && (route.id == target || route.alias.as_deref() == Some(target))
        }) {
            self.route_candidates(
                settings,
                route,
                target,
                RoutePurpose::ImageGeneration,
                session_scope,
            )?
        } else {
            let (provider_id, model) = target.split_once('/').ok_or_else(|| {
                "image target must use an image route or provider/model".to_string()
            })?;
            let Some(provider) = settings
                .providers
                .iter()
                .find(|provider| provider.enabled && provider.id == provider_id)
            else {
                return Ok(Vec::new());
            };
            if model.trim().is_empty() {
                return Err("image provider/model target requires a model".into());
            }
            self.expand_accounts(
                settings,
                provider.clone(),
                model.to_string(),
                target.to_string(),
                None,
                DEFAULT_FAILURE_THRESHOLD,
                None,
                session_scope,
            )?
        };
        Ok(candidates
            .into_iter()
            .filter(|candidate| {
                image_model_is_available(
                    settings,
                    &candidate.provider,
                    &candidate.upstream_model,
                )
            })
            .collect())
    }

    pub fn candidates(
        &mut self,
        settings: &GatewaySettings,
        requested_model: &str,
    ) -> Result<Vec<RouteCandidate>, String> {
        self.candidates_scoped(settings, requested_model, None)
    }

    /// Candidate resolution with an optional session scope. When `Some`, soft
    /// failures observed for the resolved candidates are recorded (and looked
    /// up) under `session::target` keys so provider trouble in one session does
    /// not cool the same target for other sessions.
    pub fn candidates_scoped(
        &mut self,
        settings: &GatewaySettings,
        requested_model: &str,
        session_scope: Option<&str>,
    ) -> Result<Vec<RouteCandidate>, String> {
        let requested_model = requested_model.trim();
        if let Some(target) = helper_intercept_target(settings, requested_model) {
            let mut candidates = self.candidates_core(settings, target, session_scope)?;
            for candidate in &mut candidates {
                candidate.exposed_model = requested_model.to_string();
                candidate.route_id = Some("codetas-helper-intercept".into());
                candidate.default_reasoning_effort = Some(
                    candidate
                        .reasoning_efforts
                        .iter()
                        .filter_map(|effort| reasoning_rank(effort).map(|rank| (rank, effort)))
                        .min_by_key(|(rank, _)| *rank)
                        .map(|(_, effort)| effort.clone())
                        .unwrap_or_else(|| "low".into()),
                );
            }
            return Ok(candidates);
        }
        self.candidates_core(settings, requested_model, session_scope)
    }

    fn candidates_core(
        &mut self,
        settings: &GatewaySettings,
        requested_model: &str,
        session_scope: Option<&str>,
    ) -> Result<Vec<RouteCandidate>, String> {
        if requested_model == "codetas-sidecar/image" {
            return Err(
                "codetas-sidecar/image is image-generation-only and cannot be used for Responses routing"
                    .into(),
            );
        }
        if let Some(target) = sidecar_target(settings, requested_model) {
            if target == requested_model {
                return Err("sidecar target cannot reference itself".into());
            }
            let mut candidates = self.candidates_scoped(settings, target, session_scope)?;
            for candidate in &mut candidates {
                candidate.exposed_model = requested_model.to_string();
            }
            return Ok(candidates);
        }
        if let Some(route) = settings.routes.iter().find(|route| {
            route.enabled
                && (route.id == requested_model || route.alias.as_deref() == Some(requested_model))
        }) {
            return self.route_candidates(
                settings,
                route,
                requested_model,
                RoutePurpose::Normal,
                session_scope,
            );
        }

        let (provider, upstream_model) = direct_target(settings, requested_model)?;
        let exposed_model = if requested_model.is_empty() {
            format!("{}/{}", provider.id, upstream_model)
        } else {
            requested_model.to_string()
        };
        self.expand_accounts(
            settings,
            provider,
            upstream_model,
            exposed_model,
            None,
            DEFAULT_FAILURE_THRESHOLD,
            None,
            session_scope,
        )
    }

    pub fn record_success(&mut self, candidate: &RouteCandidate, quota_usage_percent: Option<u8>) {
        if candidate.routing_epoch != self.epoch {
            return;
        }
        ensure_runtime_capacity(&mut self.target_requests, &candidate.target_key);
        *self
            .target_requests
            .entry(candidate.target_key.clone())
            .or_default() += 1;
        let hard_key = failure_key(candidate);
        if self.is_hard_cooling(&hard_key)
            || candidate.routing_generation != self.routing_generation(&hard_key)
        {
            return;
        }
        self.failures.remove(&soft_failure_key(candidate));
        if let Some(percent) = quota_usage_percent {
            ensure_runtime_capacity(&mut self.quota_usage_percent, &hard_key);
            self.quota_usage_percent.insert(hard_key, percent.min(100));
        }
    }

    pub fn record_quota_exhausted(
        &mut self,
        candidate: &RouteCandidate,
        retry_after: Option<Duration>,
    ) {
        if candidate.routing_epoch != self.epoch {
            return;
        }
        let key = failure_key(candidate);
        let now = Instant::now();
        self.cleanup_inactive_routing_keys(now);
        if self.reserve_routing_key(&key).is_err() {
            // Production sends hold a live attempt lease through response
            // classification. This branch is only reachable for a synthetic
            // or detached stale candidate and must not evict active state.
            return;
        }
        let generation = self.routing_generations.entry(key.clone()).or_default();
        generation.generation = generation.generation.wrapping_add(1);
        self.transient_quota_exhausted.insert(key.clone());
        let deadline = now
            + retry_after
                .unwrap_or(COOLDOWN)
                .min(MAX_HARD_RETRY_AFTER_COOLDOWN);
        self.hard_cooldowns
            .entry(key.clone())
            .and_modify(|current| *current = (*current).max(deadline))
            .or_insert(deadline);
        // A quota exhaustion supersedes accumulated soft failures for the same
        // credential target across every session. Soft keys are either the
        // plain key (no session) or `session::key`; both are cleared here.
        self.failures.retain(|soft_key, _| {
            !(soft_key == &key || soft_key.ends_with(&format!("::{key}")))
        });
    }

    pub fn record_failure(&mut self, candidate: &RouteCandidate) {
        if candidate.routing_epoch != self.epoch {
            return;
        }
        // Hard cooldown and generation checks stay credential-scoped. Only the
        // soft failure counter itself is session-scoped.
        let hard_key = failure_key(candidate);
        if self.is_hard_cooling(&hard_key)
            || candidate.routing_generation != self.routing_generation(&hard_key)
        {
            return;
        }
        let soft_key = soft_failure_key(candidate);
        if !self.routing_generations.contains_key(&hard_key) {
            if self.reserve_routing_key(&hard_key).is_err() {
                return;
            }
            self.routing_generations.insert(
                hard_key,
                RoutingGenerationState {
                    generation: candidate.routing_generation,
                    active_leases: 0,
                },
            );
        }
        ensure_runtime_capacity(&mut self.failures, &soft_key);
        let failure = self.failures.entry(soft_key).or_default();
        failure.last_activity = Instant::now();
        failure.consecutive = failure.consecutive.saturating_add(1);
        if failure.consecutive >= candidate.failure_threshold.max(1) {
            let deadline = Instant::now() + COOLDOWN;
            failure.retry_after = Some(
                failure
                    .retry_after
                    .map(|current| current.max(deadline))
                    .unwrap_or(deadline),
            );
            failure.consecutive = 0;
        }
    }

    fn route_candidates(
        &mut self,
        settings: &GatewaySettings,
        route: &RouteDefinition,
        exposed_model: &str,
        purpose: RoutePurpose,
        session_scope: Option<&str>,
    ) -> Result<Vec<RouteCandidate>, String> {
        let mut targets = route.targets.clone();
        targets.retain(|target| {
            let Some((provider_id, model_id)) = target.model.split_once('/') else {
                return false;
            };
            let Some(provider) = settings
                .providers
                .iter()
                .find(|provider| provider.enabled && provider.id == provider_id)
            else {
                return false;
            };
            match purpose {
                RoutePurpose::Normal => {
                    !model_has_image_generation_identity(settings, provider, model_id)
                }
                RoutePurpose::ImageGeneration => {
                    image_model_is_available(settings, provider, model_id)
                }
            }
        });
        match route.strategy {
            RouteStrategy::Failover => {}
            RouteStrategy::WeightedRoundRobin => {
                let cursor_key = purpose.cursor_key(&route.id);
                targets = weighted_order(
                    targets,
                    next_sticky_index(
                        &mut self.route_calls,
                        &cursor_key,
                        route.sticky_requests,
                    ),
                );
            }
            RouteStrategy::LeastUsage => {
                targets.sort_by_key(|target| {
                    self.target_requests
                        .get(&target.model)
                        .copied()
                        .unwrap_or(0)
                });
            }
            RouteStrategy::Policy => {}
        }

        let available = targets
            .iter()
            .filter(|target| {
                let soft_key = scoped_soft_key(session_scope, &target.model);
                !self.is_cooling_scoped(&target.model, &soft_key)
            })
            .cloned()
            .collect::<Vec<_>>();
        if available.is_empty() && !targets.is_empty() {
            return Err(format!("route {} has all targets cooling down", route.id));
        }
        targets = available;

        let target_count = targets.len();
        let mut cooling_targets = 0_usize;
        let mut candidates = Vec::new();
        for target in targets {
            let (provider_id, model_id) = target
                .model
                .split_once('/')
                .ok_or_else(|| format!("route target must use provider/model: {}", target.model))?;
            let Some(provider) = settings
                .providers
                .iter()
                .find(|provider| provider.enabled && provider.id == provider_id)
                .cloned()
            else {
                continue;
            };
            let expanded = self.expand_accounts(
                settings,
                provider,
                model_id.to_string(),
                exposed_model.to_string(),
                Some(route.id.clone()),
                route.failure_threshold,
                route.default_reasoning_effort.clone(),
                session_scope,
            );
            match expanded {
                Ok(expanded) => {
                    for candidate in expanded {
                        if let Some(reason) = policy_exclusion(&candidate, &route.policy) {
                            crate::debug::log(&format!(
                                "policy route {} excluded {}: {reason}",
                                route.id, candidate.target_key
                            ));
                        } else {
                            candidates.push(candidate);
                        }
                    }
                }
                Err(message) if message.contains("cooling down") => {
                    cooling_targets = cooling_targets.saturating_add(1);
                }
                Err(_) => {}
            }
        }
        if candidates.is_empty() {
            if target_count > 0 && cooling_targets == target_count {
                return Err(format!("route {} has all targets cooling down", route.id));
            }
            return Err(format!(
                "route {} has no available provider account",
                route.id
            ));
        }
        if route.strategy == RouteStrategy::Policy {
            candidates.sort_by(|left, right| {
                let account_rank = |candidate: &RouteCandidate| {
                    candidate
                        .account_id
                        .as_deref()
                        .and_then(|id| {
                            settings.account_pool.accounts.iter().find(|account| {
                                account.provider_id == candidate.provider.id && account.id == id
                            })
                        })
                        .map(|account| (account.pinned, account.priority))
                        .unwrap_or((false, 0))
                };
                let left_account = account_rank(left);
                let right_account = account_rank(right);
                let left_quota = self.effective_quota_usage(&failure_key(left));
                let right_quota = self.effective_quota_usage(&failure_key(right));
                let left_health = self.candidate_health_percent(left);
                let right_health = self.candidate_health_percent(right);
                let policy_order = || {
                    route_policy_score(right, right_quota, right_health, &route.policy)
                        .cmp(&route_policy_score(left, left_quota, left_health, &route.policy))
                };
                if left.target_key == right.target_key {
                    right_account
                        .0
                        .cmp(&left_account.0)
                        .then_with(|| right_account.1.cmp(&left_account.1))
                        .then_with(policy_order)
                } else {
                    policy_order()
                }
            });
        }
        Ok(candidates)
    }

    #[allow(clippy::too_many_arguments)]
    fn expand_accounts(
        &mut self,
        settings: &GatewaySettings,
        provider: ProviderDefinition,
        upstream_model: String,
        exposed_model: String,
        route_id: Option<String>,
        failure_threshold: u8,
        route_default_effort: Option<String>,
        session_scope: Option<&str>,
    ) -> Result<Vec<RouteCandidate>, String> {
        let target_key = format!("{}/{}", provider.id, upstream_model);
        let metadata = settings.model_catalog.iter().find(|model| {
            model.enabled && model.provider_id == provider.id && model.model_id == upstream_model
        });
        let capabilities =
            effective_model_capabilities(&provider, metadata, &upstream_model);
        let provider_reasoning_efforts = provider
            .model_reasoning_efforts
            .get(&upstream_model)
            .cloned()
            .unwrap_or_default();
        let policy = (
            metadata.and_then(|model| model.input_price_per_million),
            metadata.and_then(|model| model.output_price_per_million),
            metadata
                .and_then(|model| model.context_window)
                .or_else(|| provider.model_context_windows.get(&upstream_model).copied()),
            metadata
                .and_then(|model| model.max_input_tokens)
                .or_else(|| {
                    provider
                        .model_max_input_tokens
                        .get(&upstream_model)
                        .copied()
                }),
            metadata
                .and_then(|model| model.max_output_tokens)
                .or_else(|| {
                    provider
                        .model_max_output_tokens
                        .get(&upstream_model)
                        .copied()
                }),
            metadata
                .filter(|model| !model.reasoning_efforts.is_empty())
                .map(|model| model.reasoning_efforts.clone())
                .unwrap_or(provider_reasoning_efforts),
            metadata
                .and_then(|model| model.default_reasoning_effort.clone())
                .or_else(|| {
                    provider
                        .model_default_reasoning_efforts
                        .get(&upstream_model)
                        .cloned()
                }),
        );
        let default_reasoning_effort = route_default_effort.or(policy.6);
        let configured_accounts = settings
            .account_pool
            .accounts
            .iter()
            .filter(|account| account.provider_id == provider.id)
            .cloned()
            .collect::<Vec<_>>();
        if configured_accounts.is_empty() {
            let account_id = provider_oauth_account_id(&provider);
            let routing_key = account_id
                .as_deref()
                .map(|account| format!("{target_key}#{account}"))
                .unwrap_or_else(|| target_key.clone());
            let soft_key = scoped_soft_key(session_scope, &routing_key);
            if self.is_cooling_scoped(&routing_key, &soft_key) {
                return Err(format!("provider target {target_key} is cooling down"));
            }
            let routing_generation = self.reserve_routing_key(&routing_key)?;
            return Ok(vec![RouteCandidate {
                provider,
                upstream_model,
                exposed_model,
                credential: None,
                account_id,
                target_key,
                route_id,
                failure_threshold,
                quota_threshold_percent: settings.account_pool.auto_switch_threshold_percent,
                input_price_per_million: policy.0,
                output_price_per_million: policy.1,
                context_window: policy.2,
                max_input_tokens: policy.3,
                max_output_tokens: policy.4,
                reasoning_efforts: policy.5,
                default_reasoning_effort,
                capabilities,
                routing_epoch: self.epoch,
                routing_generation,
                session_scope: session_scope.map(str::to_string),
            }]);
        }
        let mut accounts = configured_accounts
            .into_iter()
            .filter(|account| {
                account.enabled
                    && (!account.paused
                        || account.pause_until_unix.is_some_and(|until| until <= unix_now()))
            })
            .collect::<Vec<_>>();
        if accounts.is_empty() {
            return Err(format!(
                "all configured accounts for provider {} are disabled or paused",
                provider.id
            ));
        }

        accounts.retain(|account| {
            let key = format!("{target_key}#{}", account.id);
            let soft_key = scoped_soft_key(session_scope, &key);
            !self.is_cooling_scoped(&key, &soft_key)
                && (settings.account_pool.auto_switch_threshold_percent == 0
                    || self.effective_quota_usage(&key)
                        < settings.account_pool.auto_switch_threshold_percent)
        });
        if accounts.is_empty() {
            return Err(format!(
                "all configured accounts for provider {} are cooling down",
                provider.id
            ));
        }

        accounts = self.order_accounts_by_priority_tier(
            settings,
            &provider.id,
            &target_key,
            accounts,
        );

        let mut candidates = Vec::with_capacity(accounts.len());
        let mut capacity_error = None;
        for account in accounts {
                let key = format!("{target_key}#{}", account.id);
                let routing_generation = match self.reserve_routing_key(&key) {
                    Ok(generation) => generation,
                    Err(error) => {
                        capacity_error = Some(error);
                        continue;
                    }
                };
                candidates.push(RouteCandidate {
                    provider: provider.clone(),
                    upstream_model: upstream_model.clone(),
                    exposed_model: exposed_model.clone(),
                    credential: Some(account.credential),
                    account_id: Some(account.id),
                    target_key: target_key.clone(),
                    route_id: route_id.clone(),
                    failure_threshold,
                    quota_threshold_percent: settings.account_pool.auto_switch_threshold_percent,
                    input_price_per_million: policy.0,
                    output_price_per_million: policy.1,
                    context_window: policy.2,
                    max_input_tokens: policy.3,
                    max_output_tokens: policy.4,
                    reasoning_efforts: policy.5.clone(),
                    default_reasoning_effort: default_reasoning_effort.clone(),
                    capabilities: capabilities.clone(),
                    routing_epoch: self.epoch,
                    routing_generation,
                    session_scope: session_scope.map(str::to_string),
                });
        }
        if candidates.is_empty() {
            return Err(capacity_error.unwrap_or_else(|| {
                format!("provider target {target_key} has no routable account")
            }));
        }
        Ok(candidates)
    }

    fn order_accounts_by_priority_tier(
        &mut self,
        settings: &GatewaySettings,
        provider_id: &str,
        target_key: &str,
        mut accounts: Vec<AccountReference>,
    ) -> Vec<AccountReference> {
        let pinned_id = accounts
            .iter()
            .find(|account| account.pinned)
            .map(|account| account.id.clone());

        accounts.sort_by_key(|account| std::cmp::Reverse(account.priority));
        let highest_priority = accounts.first().map(|account| account.priority);
        let preferred_id = pinned_id.or_else(|| {
            settings.account_pool.active_accounts.get(provider_id).and_then(|active| {
                accounts.iter().find(|account| {
                    account.id == *active && Some(account.priority) == highest_priority
                }).map(|account| account.id.clone())
            })
        });
        let preferred = preferred_id.as_deref()
            .and_then(|id| accounts.iter().position(|account| account.id == id))
            .map(|index| accounts.remove(index));
        let preferred_capacity = if preferred.is_some() { 1 } else { 0 };
        let mut ordered = Vec::with_capacity(accounts.len() + preferred_capacity);
        let mut start = 0;
        while start < accounts.len() {
            let priority = accounts[start].priority;
            let end = accounts[start..].iter().position(|account| account.priority != priority)
                .map(|offset| start + offset).unwrap_or(accounts.len());
            let mut tier = accounts[start..end].to_vec();
            match settings.account_pool.strategy {
                AccountPoolStrategy::Quota => tier.sort_by_key(|account| {
                    let key = format!("{target_key}#{}", account.id);
                    self.effective_quota_usage(&key)
                }),
                AccountPoolStrategy::RoundRobin if tier.len() > 1 => {
                    let selection_key = format!("account:{provider_id}:priority:{priority}");
                    let primary = next_sticky_index(
                        &mut self.account_calls,
                        &selection_key,
                        settings.account_pool.sticky_requests,
                    ) as usize % tier.len();
                    tier.rotate_left(primary);
                }
                AccountPoolStrategy::RoundRobin | AccountPoolStrategy::FillFirst => {}
            }
            ordered.extend(tier);
            start = end;
        }
        if let Some(preferred) = preferred {
            ordered.insert(0, preferred);
        }
        ordered
    }

    fn is_cooling(&mut self, key: &str) -> bool {
        self.is_cooling_scoped(key, key)
    }

    /// Cooldown check split by scope: hard quota cooldowns (429) live under
    /// `hard_key` (credential/account granularity) while soft failures (5xx /
    /// provider_unreachable) live under `soft_key` (session-scoped when a
    /// session identifier is known, credential-scoped otherwise).
    fn is_cooling_scoped(&mut self, hard_key: &str, soft_key: &str) -> bool {
        self.cleanup_inactive_routing_keys(Instant::now());
        if self.is_hard_cooling(hard_key) {
            return true;
        }
        let Some(retry_after) = self
            .failures
            .get(soft_key)
            .and_then(|failure| failure.retry_after)
        else {
            return false;
        };
        if retry_after > Instant::now() {
            return true;
        }
        self.failures.remove(soft_key);
        self.maybe_remove_routing_generation(hard_key);
        false
    }

    fn is_hard_cooling(&mut self, key: &str) -> bool {
        let Some(deadline) = self.hard_cooldowns.get(key).copied() else {
            self.transient_quota_exhausted.remove(key);
            return false;
        };
        if deadline > Instant::now() {
            return true;
        }
        self.hard_cooldowns.remove(key);
        self.transient_quota_exhausted.remove(key);
        self.maybe_remove_routing_generation(key);
        false
    }

    fn cleanup_inactive_routing_keys(&mut self, now: Instant) {
        let expired = self
            .hard_cooldowns
            .iter()
            .filter_map(|(key, deadline)| (*deadline <= now).then(|| key.clone()))
            .collect::<Vec<_>>();
        for key in expired {
            self.hard_cooldowns.remove(&key);
            self.transient_quota_exhausted.remove(&key);
            self.maybe_remove_routing_generation(&key);
        }
        let expired_failures = self
            .failures
            .iter()
            .filter_map(|(key, failure)| {
                (now.saturating_duration_since(failure.last_activity) >= SOFT_FAILURE_RETENTION
                    && !self.hard_cooldowns.contains_key(key)
                    && self
                        .routing_generations
                        .get(key)
                        .is_none_or(|state| state.active_leases == 0))
                .then(|| key.clone())
            })
            .collect::<Vec<_>>();
        for key in expired_failures {
            self.failures.remove(&key);
            self.maybe_remove_routing_generation(&key);
        }
    }

    fn reserve_routing_key(&mut self, key: &str) -> Result<u64, String> {
        self.cleanup_inactive_routing_keys(Instant::now());
        if let Some(state) = self.routing_generations.get(key) {
            return Ok(state.generation);
        }
        if self.routing_generations.len() >= MAX_ROUTING_RUNTIME_KEYS {
            return Err(format!(
                "routing runtime key capacity reached before sending provider target {key}"
            ));
        }
        Ok(0)
    }

    pub(crate) fn begin_attempt(&mut self, candidate: &RouteCandidate) -> Result<(), String> {
        if candidate.routing_epoch != self.epoch {
            return Err("routing runtime epoch changed before provider send".into());
        }
        let key = failure_key(candidate);
        self.cleanup_inactive_routing_keys(Instant::now());
        if self.is_hard_cooling(&key) {
            return Err(format!("provider target {key} is cooling down"));
        }
        if let Some(state) = self.routing_generations.get_mut(&key) {
            if state.generation != candidate.routing_generation {
                return Err(format!("provider target {key} routing generation changed"));
            }
            state.active_leases = state.active_leases.saturating_add(1);
            return Ok(());
        }
        if self.routing_generations.len() >= MAX_ROUTING_RUNTIME_KEYS {
            return Err(format!(
                "routing runtime key capacity reached before sending provider target {key}"
            ));
        }
        self.routing_generations.insert(
            key,
            RoutingGenerationState {
                generation: candidate.routing_generation,
                active_leases: 1,
            },
        );
        Ok(())
    }

    pub(crate) fn end_attempt(&mut self, candidate: &RouteCandidate) {
        if candidate.routing_epoch != self.epoch {
            return;
        }
        let key = failure_key(candidate);
        if let Some(state) = self.routing_generations.get_mut(&key) {
            state.active_leases = state.active_leases.saturating_sub(1);
        }
        self.maybe_remove_routing_generation(&key);
    }

    fn maybe_remove_routing_generation(&mut self, key: &str) {
        let inactive = self
            .routing_generations
            .get(key)
            .is_some_and(|state| state.active_leases == 0)
            && !self.hard_cooldowns.contains_key(key)
            && !self.transient_quota_exhausted.contains(key)
            && !self.failures.contains_key(key);
        if inactive {
            self.routing_generations.remove(key);
        }
    }

    fn routing_generation(&self, key: &str) -> u64 {
        self.routing_generations
            .get(key)
            .map(|state| state.generation)
            .unwrap_or(0)
    }

    fn effective_quota_usage(&self, key: &str) -> u8 {
        if self.transient_quota_exhausted.contains(key) {
            100
        } else {
            self.quota_usage_percent.get(key).copied().unwrap_or(0)
        }
    }

    fn candidate_health_percent(&self, candidate: &RouteCandidate) -> u8 {
        let Some(failure) = self.failures.get(&soft_failure_key(candidate)) else {
            return 100;
        };
        if failure.retry_after.is_some_and(|retry_after| retry_after > Instant::now()) {
            return 0;
        }
        let threshold = candidate.failure_threshold.max(1);
        let remaining = threshold.saturating_sub(failure.consecutive.min(threshold));
        ((u16::from(remaining) * 100) / u16::from(threshold)) as u8
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn capability_enabled(capabilities: &ProviderCapabilities, name: &str) -> bool {
    match name {
        "streaming" => capabilities.streaming,
        "tools" => capabilities.tools,
        "parallelTools" => capabilities.tools && capabilities.parallel_tools,
        "vision" => capabilities.vision,
        "audio" => capabilities.audio,
        "reasoning" => capabilities.reasoning,
        "webSearch" => capabilities.web_search,
        "imageGeneration" => capabilities.image_generation,
        "videoGeneration" => capabilities.video_generation,
        "realtime" => capabilities.realtime,
        "websockets" => capabilities.websockets,
        "statefulResponses" => capabilities.stateful_responses,
        "structuredOutput" => capabilities.structured_output,
        "serviceTier" => capabilities.service_tier,
        "customTools" => capabilities.tools && capabilities.custom_tools,
        "toolSearch" => capabilities.tools && capabilities.tool_search,
        "mcpNamespaces" => capabilities.tools && capabilities.mcp_namespaces,
        "providerMetadata" => capabilities.provider_metadata,
        _ => false,
    }
}

fn policy_exclusion(candidate: &RouteCandidate, policy: &RoutePolicySettings) -> Option<String> {
    for capability in &policy.required_capabilities {
        if !capability_enabled(&candidate.capabilities, capability) {
            return Some(format!("missing capability {capability}"));
        }
    }
    if policy.max_input_price_per_million.is_some_and(|limit| {
        candidate
            .input_price_per_million
            .is_none_or(|price| price > limit)
    }) {
        return Some("input price exceeds policy".into());
    }
    if policy.max_output_price_per_million.is_some_and(|limit| {
        candidate
            .output_price_per_million
            .is_none_or(|price| price > limit)
    }) {
        return Some("output price exceeds policy".into());
    }
    None
}

fn normalized_cost(candidate: &RouteCandidate) -> u64 {
    let input = candidate.input_price_per_million.unwrap_or(1_000.0).max(0.0);
    let output = candidate.output_price_per_million.unwrap_or(1_000.0).max(0.0);
    ((input + output) * 1_000.0).min(u64::MAX as f64) as u64
}

fn route_policy_score(
    candidate: &RouteCandidate,
    quota: u8,
    health_percent: u8,
    policy: &RoutePolicySettings,
) -> i64 {
    let health = i64::from(policy.health_weight) * i64::from(health_percent) * 10;
    let quota_score =
        i64::from(policy.quota_weight) * i64::from(100_u8.saturating_sub(quota));
    let context = i64::from(policy.context_weight)
        * i64::try_from(candidate.context_window.unwrap_or(0) / 1_000).unwrap_or(i64::MAX);
    let cost = i64::try_from(normalized_cost(candidate)).unwrap_or(i64::MAX);
    health
        .saturating_add(quota_score)
        .saturating_add(context)
        .saturating_sub(i64::from(policy.cost_weight).saturating_mul(cost))
}

fn policy_score(candidate: &RouteCandidate, quota: u8, healthy: bool) -> i64 {
    let health = if healthy { 100_000 } else { -100_000 };
    health + i64::from(100_u8.saturating_sub(quota)) * 100
        - i64::try_from(normalized_cost(candidate)).unwrap_or(i64::MAX)
}

fn helper_intercept_target<'a>(
    settings: &'a GatewaySettings,
    requested_model: &str,
) -> Option<&'a str> {
    if !settings.helper_intercept.enabled {
        return None;
    }
    let matched = settings
        .helper_intercept
        .source_models
        .iter()
        .any(|source| {
            requested_model == source || requested_model.starts_with(&format!("{source}-"))
        });
    matched
        .then(|| settings.helper_intercept.target_model.as_deref())
        .flatten()
}

fn reasoning_rank(value: &str) -> Option<u8> {
    match value {
        "none" => Some(0),
        "minimal" => Some(1),
        "low" => Some(2),
        "medium" => Some(3),
        "high" => Some(4),
        "xhigh" => Some(5),
        "max" | "ultra" => Some(6),
        _ => None,
    }
}

fn sidecar_target<'a>(settings: &'a GatewaySettings, requested_model: &str) -> Option<&'a str> {
    match requested_model {
        "codetas-sidecar/web-search" => settings.sidecars.web_search_model.as_deref(),
        "codetas-sidecar/vision" => settings.sidecars.vision_model.as_deref(),
        "codetas-sidecar/image" => settings.sidecars.image_model.as_deref(),
        "codetas-sidecar/video" => settings.sidecars.video_model.as_deref(),
        _ => None,
    }
}

fn direct_target(
    settings: &GatewaySettings,
    requested_model: &str,
) -> Result<(ProviderDefinition, String), String> {
    if let Some((provider_id, model)) = requested_model.split_once('/') {
        if let Some(provider) = settings
            .providers
            .iter()
            .find(|provider| provider.enabled && provider.id == provider_id)
        {
            if model.trim().is_empty() {
                return Err("provider/model request requires a model".into());
            }
            if model_has_image_generation_identity(settings, provider, model) {
                return Err(format!(
                    "{provider_id}/{model} is image-generation-only and cannot be used for Responses routing"
                ));
            }
            return Ok((provider.clone(), model.to_string()));
        }
    }

    // When Codex uses its built-in `openai` provider with the root
    // `openai_base_url` loopback override, it sends native model ids without a
    // provider prefix. Route those models to the forward-auth OpenAI definition
    // before consulting CODETAS's default provider. Custom providers continue
    // to use provider/model ids.
    if let Some(provider) = settings.providers.iter().find(|provider| {
        provider.enabled
            && provider.id == "openai"
            && (provider.default_model.as_deref() == Some(requested_model)
                || provider.models.iter().any(|model| model == requested_model)
                || settings.model_catalog.iter().any(|model| {
                    model.enabled
                        && model.provider_id == "openai"
                        && model.model_id == requested_model
                }))
    }) {
        if model_has_image_generation_identity(settings, provider, requested_model) {
            return Err(format!(
                "openai/{requested_model} is image-generation-only and cannot be used for Responses routing"
            ));
        }
        return Ok((provider.clone(), requested_model.to_string()));
    }

    let default_id = settings
        .default_provider
        .as_deref()
        .ok_or_else(|| "no default provider is configured; use provider/model".to_string())?;
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.enabled && provider.id == default_id)
        .cloned()
        .ok_or_else(|| "the default provider is unavailable".to_string())?;
    let upstream_model = if requested_model.is_empty() {
        provider
            .default_model
            .clone()
            .ok_or_else(|| "the request and provider do not specify a model".to_string())?
    } else {
        requested_model.to_string()
    };
    if model_has_image_generation_identity(settings, &provider, &upstream_model) {
        return Err(format!(
            "{}/{upstream_model} is image-generation-only and cannot be used for Responses routing",
            provider.id
        ));
    }
    Ok((provider, upstream_model))
}

fn next_sticky_index(calls: &mut HashMap<String, u64>, key: &str, sticky: u32) -> u64 {
    ensure_runtime_capacity(calls, key);
    let calls = calls.entry(key.to_string()).or_default();
    let index = *calls / u64::from(sticky.max(1));
    *calls = calls.saturating_add(1);
    index
}

fn ensure_runtime_capacity<V>(values: &mut HashMap<String, V>, key: &str) {
    if values.len() >= MAX_ROUTING_RUNTIME_KEYS && !values.contains_key(key) {
        values.clear();
    }
}

fn weighted_order(targets: Vec<RouteTarget>, cursor: u64) -> Vec<RouteTarget> {
    if targets.len() < 2 {
        return targets;
    }
    let total = targets
        .iter()
        .map(|target| u64::from(target.weight.max(1)))
        .sum::<u64>();
    let slot = cursor % total.max(1);
    let mut cumulative = 0_u64;
    let selected = targets
        .iter()
        .position(|target| {
            cumulative += u64::from(target.weight.max(1));
            slot < cumulative
        })
        .unwrap_or(0);
    let mut ordered = targets;
    ordered.rotate_left(selected);
    ordered
}

fn failure_key(candidate: &RouteCandidate) -> String {
    candidate
        .account_id
        .as_deref()
        .map(|account| format!("{}#{account}", candidate.target_key))
        .unwrap_or_else(|| candidate.target_key.clone())
}

/// Soft-failure state key. When a session/task identifier is known, failures
/// are namespaced per session so one session's provider trouble does not cool
/// the same target for other sessions. Without a session identifier we fall
/// back to the credential-scoped key (historical behavior).
fn scoped_soft_key(session_scope: Option<&str>, hard_key: &str) -> String {
    match session_scope {
        Some(session) if !session.is_empty() => format!("{session}::{hard_key}"),
        _ => hard_key.to_string(),
    }
}

fn soft_failure_key(candidate: &RouteCandidate) -> String {
    scoped_soft_key(candidate.session_scope.as_deref(), &failure_key(candidate))
}

/// Cooldown duration surfaced to clients via `Retry-After` when routing is
/// rejected because a provider target is cooling down.
pub(crate) fn cooldown_retry_after_seconds() -> u64 {
    COOLDOWN.as_secs()
}

fn provider_oauth_account_id(provider: &ProviderDefinition) -> Option<String> {
    if !matches!(provider.id.as_str(), "kimi" | "kimi-code")
        || provider.credential.source != CredentialSource::OAuth
    {
        return None;
    }
    crate::oauth::oauth_account_id(&provider.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProviderDefinition, ProviderProtocol, RouteTarget};

    fn settings() -> GatewaySettings {
        let providers = ["one", "two"]
            .into_iter()
            .map(|id| ProviderDefinition {
                id: id.into(),
                name: id.into(),
                base_url: format!("https://{id}.example/v1"),
                protocol: ProviderProtocol::Responses,
                default_model: Some("model".into()),
                ..ProviderDefinition::default()
            })
            .collect();
        GatewaySettings {
            default_provider: Some("one".into()),
            providers,
            routes: vec![RouteDefinition {
                id: "reliable".into(),
                name: "Reliable".into(),
                strategy: RouteStrategy::Failover,
                targets: vec![
                    RouteTarget {
                        model: "one/model".into(),
                        weight: 1,
                    },
                    RouteTarget {
                        model: "two/model".into(),
                        weight: 1,
                    },
                ],
                ..RouteDefinition::default()
            }],
            ..GatewaySettings::default()
        }
    }

    fn account(id: &str, priority: i16) -> crate::config::AccountReference {
        crate::config::AccountReference {
            id: id.into(),
            provider_id: "one".into(),
            label: id.into(),
            priority,
            ..crate::config::AccountReference::default()
        }
    }

    #[test]
    fn expands_a_failover_route() {
        let mut runtime = RoutingRuntime::default();
        let candidates = runtime.candidates(&settings(), "reliable").expect("route");
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].target_key, "one/model");
        assert_eq!(candidates[1].target_key, "two/model");
    }

    #[test]
    fn dry_run_keeps_policy_and_account_exclusions_visible() {
        let mut settings = settings();
        settings.routes[0].strategy = RouteStrategy::Policy;
        settings.routes[0].policy.required_capabilities = vec!["tools".into()];
        settings.providers[1].capabilities.tools = false;
        settings.account_pool.accounts.push(crate::config::AccountReference {
            id: "paused".into(), provider_id: "two".into(), label: "Paused".into(),
            paused: true, ..crate::config::AccountReference::default()
        });
        let report = RoutingRuntime::default().dry_run(&settings, "reliable", false);
        assert_eq!(report.candidates.len(), 2);
        assert!(report.candidates.iter().any(|row| row.eligible && row.target == "one/model"));
        let excluded = report.candidates.iter().find(|row| row.target == "two/model").unwrap();
        assert!(excluded.reasons.iter().any(|reason| reason == "account paused"));
        assert!(excluded.reasons.iter().any(|reason| reason.contains("missing capability tools")));
    }

    #[test]
    fn routing_never_treats_tool_subcapabilities_as_available_without_tools() {
        let capabilities = ProviderCapabilities {
            tools: false,
            parallel_tools: true,
            custom_tools: true,
            tool_search: true,
            mcp_namespaces: true,
            ..ProviderCapabilities::default()
        };
        for name in ["parallelTools", "customTools", "toolSearch", "mcpNamespaces"] {
            assert!(!capability_enabled(&capabilities, name), "{name}");
        }
    }

    #[test]
    fn dry_run_does_not_advance_or_clean_live_runtime_state() {
        let mut settings = settings();
        settings.routes[0].strategy = RouteStrategy::WeightedRoundRobin;
        settings.routes[0].sticky_requests = 1;
        settings.account_pool.accounts = vec![account("a", 0), account("b", 0)];
        settings.account_pool.strategy = AccountPoolStrategy::RoundRobin;
        let mut runtime = RoutingRuntime::default();
        runtime.route_calls.insert("reliable:normal".into(), 3);
        runtime.account_calls.insert("account:one:priority:0".into(), 5);
        runtime.failures.insert("two/model".into(), FailureState {
            consecutive: 2,
            retry_after: Some(Instant::now() - Duration::from_secs(1)),
            ..FailureState::default()
        });
        let before_route_calls = runtime.route_calls.clone();
        let before_account_calls = runtime.account_calls.clone();
        let before_failure = runtime.failures["two/model"].clone();

        let _ = runtime.dry_run(&settings, "reliable", false);

        assert_eq!(runtime.route_calls, before_route_calls);
        assert_eq!(runtime.account_calls, before_account_calls);
        assert_eq!(runtime.failures["two/model"].consecutive, before_failure.consecutive);
        assert_eq!(
            runtime.failures["two/model"].retry_after,
            before_failure.retry_after
        );
    }

    #[test]
    fn dry_run_selection_matches_the_next_real_selection_for_every_strategy() {
        for strategy in [
            RouteStrategy::Failover,
            RouteStrategy::WeightedRoundRobin,
            RouteStrategy::LeastUsage,
            RouteStrategy::Policy,
        ] {
            let mut settings = settings();
            settings.routes[0].strategy = strategy;
            settings.routes[0].sticky_requests = 1;
            settings.routes[0].targets[0].weight = 1;
            settings.routes[0].targets[1].weight = 2;
            let mut runtime = RoutingRuntime::default();
            match strategy {
                RouteStrategy::WeightedRoundRobin => {
                    runtime.route_calls.insert("reliable:normal".into(), 1);
                }
                RouteStrategy::LeastUsage => {
                    runtime.target_requests.insert("one/model".into(), 10);
                }
                RouteStrategy::Policy => {
                    settings.routes[0].policy.quota_weight = 10;
                    runtime.quota_usage_percent.insert("one/model".into(), 90);
                }
                RouteStrategy::Failover => {}
            }

            let report = runtime.dry_run(&settings, "reliable", false);
            let next = runtime
                .candidates(&settings, "reliable")
                .expect("next route candidate");

            assert_eq!(
                report.selected.as_deref(),
                next.first().map(|candidate| candidate.target_key.as_str()),
                "{strategy:?}"
            );
            assert_eq!(
                report.candidates.iter().find(|candidate| candidate.eligible).map(|candidate| candidate.rank),
                Some(
                    settings.routes[0]
                        .targets
                        .iter()
                        .position(|target| target.model == next[0].target_key)
                        .expect("configured target")
                        + 1
                ),
                "{strategy:?}"
            );
        }
    }

    #[test]
    fn policy_health_weight_prefers_the_healthier_candidate_and_reports_signal() {
        let mut settings = settings();
        settings.routes[0].strategy = RouteStrategy::Policy;
        settings.routes[0].policy.health_weight = 100;
        settings.routes[0].policy.cost_weight = 0;
        settings.routes[0].policy.quota_weight = 0;
        settings.routes[0].policy.context_weight = 0;
        let mut runtime = RoutingRuntime::default();
        runtime.failures.insert(
            "one/model".into(),
            FailureState {
                consecutive: 2,
                retry_after: None,
                ..FailureState::default()
            },
        );

        let report = runtime.dry_run(&settings, "reliable", false);
        let actual = runtime.candidates(&settings, "reliable").expect("policy route");

        assert_eq!(actual[0].target_key, "two/model");
        assert_eq!(report.selected.as_deref(), Some("two/model"));
        let degraded = report
            .candidates
            .iter()
            .find(|candidate| candidate.target == "one/model")
            .expect("degraded candidate");
        assert!(degraded.health_percent < 100);
        assert!(degraded
            .reasons
            .iter()
            .any(|reason| reason.starts_with("health:")));
    }

    #[test]
    fn model_specific_subagent_fallback_works_without_a_primary_roster() {
        let mut settings = settings();
        settings.agents.multi_agent_v2 = true;
        settings
            .agents
            .subagent_fallback_by_model
            .insert("parent/model".into(), vec!["two/model".into()]);
        let mut runtime = RoutingRuntime::default();

        let candidates = runtime
            .candidates_for_request(&settings, "parent/model", true, None)
            .expect("model-specific subagent fallback");

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].target_key, "two/model");
        assert_eq!(candidates[0].exposed_model, "parent/model");
    }

    #[test]
    fn subagent_rosters_use_primary_then_model_specific_then_common_order() {
        let mut settings = settings();
        settings.agents.multi_agent_v2 = true;
        settings.agents.subagent_models = vec!["one/primary".into()];
        settings
            .agents
            .subagent_fallback_by_model
            .insert("parent/model".into(), vec!["two/specific".into()]);
        settings.agents.subagent_fallback = vec!["one/common".into()];
        let mut runtime = RoutingRuntime::default();

        let candidates = runtime
            .candidates_for_request(&settings, "parent/model", true, None)
            .expect("subagent rosters");
        let targets = candidates
            .iter()
            .map(|candidate| candidate.target_key.as_str())
            .collect::<Vec<_>>();

        assert_eq!(targets, vec!["one/primary", "two/specific", "one/common"]);
    }

    #[test]
    fn account_strategy_never_promotes_a_lower_priority_tier() {
        let mut settings = settings();
        settings.account_pool.accounts = vec![
            account("high-a", 100), account("low", 0), account("high-b", 100),
        ];
        settings.account_pool.strategy = AccountPoolStrategy::Quota;
        let mut runtime = RoutingRuntime::default();
        runtime.quota_usage_percent.insert("one/model#high-a".into(), 50);
        runtime.quota_usage_percent.insert("one/model#high-b".into(), 10);
        let candidates = runtime.candidates(&settings, "one/model").expect("accounts");
        assert_eq!(candidates.iter().map(|item| item.account_id.as_deref()).collect::<Vec<_>>(),
            vec![Some("high-b"), Some("high-a"), Some("low")]);

        settings.account_pool.strategy = AccountPoolStrategy::RoundRobin;
        let first = runtime.candidates(&settings, "one/model").expect("round robin");
        let second = runtime.candidates(&settings, "one/model").expect("round robin");
        assert_eq!(first.last().and_then(|item| item.account_id.as_deref()), Some("low"));
        assert_eq!(second.last().and_then(|item| item.account_id.as_deref()), Some("low"));

        settings
            .account_pool
            .active_accounts
            .insert("one".into(), "low".into());
        let active_low = runtime.candidates(&settings, "one/model").expect("active low");
        assert_ne!(active_low[0].account_id.as_deref(), Some("low"));
    }

    #[test]
    fn policy_scoring_cannot_promote_a_lower_account_priority_tier() {
        let mut settings = settings();
        settings.routes[0].strategy = RouteStrategy::Policy;
        settings.routes[0].targets.truncate(1);
        settings.account_pool.accounts = vec![account("high", 100), account("low", 0)];
        settings.account_pool.auto_switch_threshold_percent = 0;
        let mut runtime = RoutingRuntime::default();
        runtime
            .quota_usage_percent
            .insert("one/model#high".into(), 99);
        runtime
            .quota_usage_percent
            .insert("one/model#low".into(), 0);

        let candidates = runtime.candidates(&settings, "reliable").expect("policy route");
        assert_eq!(candidates[0].account_id.as_deref(), Some("high"));

        settings.account_pool.accounts[1].pinned = true;
        let pinned = runtime.candidates(&settings, "reliable").expect("pinned route");
        assert_eq!(pinned[0].account_id.as_deref(), Some("low"));
    }

    #[test]
    fn policy_score_ranks_providers_before_account_priority_and_dry_run_matches() {
        let mut settings = settings();
        settings.routes[0].strategy = RouteStrategy::Policy;
        settings.routes[0].policy.health_weight = 100;
        settings.routes[0].policy.cost_weight = 0;
        settings.routes[0].policy.quota_weight = 0;
        settings.routes[0].policy.context_weight = 0;
        let mut high = account("high", 100);
        high.provider_id = "one".into();
        let mut low = account("low", 0);
        low.provider_id = "two".into();
        settings.account_pool.accounts = vec![high, low];
        let mut runtime = RoutingRuntime::default();
        runtime.failures.insert(
            "one/model#high".into(),
            FailureState { consecutive: 2, retry_after: None, ..FailureState::default() },
        );

        let report = runtime.dry_run(&settings, "reliable", false);
        let candidates = runtime.candidates(&settings, "reliable").expect("policy route");

        assert_eq!(candidates[0].target_key, "two/model");
        assert_eq!(candidates[0].account_id.as_deref(), Some("low"));
        assert_eq!(report.selected.as_deref(), Some("two/model"));
    }

    #[test]
    fn pin_overrides_priority_until_unavailable_then_returns_to_highest_healthy_tier() {
        let mut settings = settings();
        let mut low = account("low", 0);
        low.pinned = true;
        settings.account_pool.accounts = vec![account("high", 100), low];
        settings.account_pool.auto_switch_threshold_percent = 80;
        let mut runtime = RoutingRuntime::default();
        let pinned = runtime.candidates(&settings, "one/model").expect("pinned");
        assert_eq!(pinned[0].account_id.as_deref(), Some("low"));

        runtime.quota_usage_percent.insert("one/model#low".into(), 100);
        let fallback = runtime.candidates(&settings, "one/model").expect("fallback");
        assert_eq!(fallback[0].account_id.as_deref(), Some("high"));
    }

    #[test]
    fn unavailable_high_priority_accounts_fall_through_to_the_next_tier() {
        let mut settings = settings();
        let mut paused = account("paused", 100);
        paused.paused = true;
        settings.account_pool.accounts = vec![
            paused, account("cooling", 100), account("quota", 100), account("low", 0),
        ];
        settings.account_pool.auto_switch_threshold_percent = 80;
        let mut runtime = RoutingRuntime::default();
        runtime.failures.insert("one/model#cooling".into(), FailureState {
            consecutive: 0,
            retry_after: Some(Instant::now() + Duration::from_secs(60)),
            ..FailureState::default()
        });
        runtime.quota_usage_percent.insert("one/model#quota".into(), 100);
        let candidates = runtime.candidates(&settings, "one/model").expect("lower tier");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].account_id.as_deref(), Some("low"));
    }

    #[test]
    fn transient_429_quota_expires_without_overwriting_persisted_usage() {
        let mut settings = settings();
        settings.account_pool.accounts = vec![account("only", 100)];
        settings.account_pool.auto_switch_threshold_percent = 80;
        let mut runtime = RoutingRuntime::default();
        let candidate = runtime
            .candidates(&settings, "one/model")
            .expect("single account")[0]
            .clone();
        runtime.record_success(&candidate, Some(25));
        runtime.record_quota_exhausted(&candidate, Some(Duration::ZERO));

        let recovered = runtime
            .candidates(&settings, "one/model")
            .expect("expired transient 429 must recover");
        assert_eq!(recovered[0].account_id.as_deref(), Some("only"));
        assert_eq!(
            runtime.effective_quota_usage("one/model#only"),
            25,
            "the real provider quota survives the transient exclusion"
        );

        let recovered_candidate = recovered[0].clone();
        runtime.record_success(&recovered_candidate, Some(100));
        runtime.record_quota_exhausted(&recovered_candidate, Some(Duration::ZERO));
        assert!(runtime.candidates(&settings, "one/model").is_err());
        assert_eq!(runtime.effective_quota_usage("one/model#only"), 100);
    }

    #[test]
    fn transient_429_fails_over_then_readmits_the_recovered_account() {
        let mut settings = settings();
        settings.account_pool.accounts =
            vec![account("primary", 100), account("backup", 100)];
        settings.account_pool.auto_switch_threshold_percent = 80;
        let mut runtime = RoutingRuntime::default();
        let primary = runtime
            .candidates(&settings, "one/model")
            .expect("account pool")[0]
            .clone();
        runtime.record_quota_exhausted(&primary, Some(Duration::from_secs(60)));

        let failover = runtime.candidates(&settings, "one/model").expect("backup");
        assert_eq!(failover.len(), 1);
        assert_eq!(failover[0].account_id.as_deref(), Some("backup"));

        let key = failure_key(&primary);
        runtime.hard_cooldowns.insert(
            key.clone(),
            Instant::now() - Duration::from_secs(1),
        );
        let recovered = runtime.candidates(&settings, "one/model").expect("recovered pool");
        assert!(recovered
            .iter()
            .any(|candidate| candidate.account_id.as_deref() == Some("primary")));
        assert!(!runtime.transient_quota_exhausted.contains(&key));
    }

    #[test]
    fn hard_429_cooldown_survives_stale_success_and_failure_and_keeps_max_deadline() {
        let mut settings = settings();
        settings.account_pool.accounts = vec![account("only", 100)];
        let mut runtime = RoutingRuntime::default();
        let old_attempt = runtime
            .candidates(&settings, "one/model")
            .expect("old attempt")[0]
            .clone();
        let key = failure_key(&old_attempt);

        runtime.record_quota_exhausted(&old_attempt, Some(Duration::from_secs(7_200)));
        let first_deadline = runtime.hard_cooldowns[&key];
        let remaining = first_deadline.saturating_duration_since(Instant::now());
        assert!(remaining > Duration::from_secs(7_100));
        assert!(remaining <= Duration::from_secs(7_200));

        runtime.record_success(&old_attempt, Some(0));
        runtime.record_failure(&old_attempt);
        runtime.record_quota_exhausted(&old_attempt, Some(Duration::from_secs(60)));

        assert_eq!(runtime.hard_cooldowns[&key], first_deadline);
        assert!(runtime.candidates(&settings, "one/model").is_err());
        assert!(runtime.transient_quota_exhausted.contains(&key));
        assert!(!runtime.failures.contains_key(&key));
    }

    #[test]
    fn delayed_real_429_is_hard_evidence_after_the_old_generation_expires() {
        let mut settings = settings();
        settings.account_pool.accounts = vec![account("only", 100)];
        let mut runtime = RoutingRuntime::default();
        let old_attempt = runtime
            .candidates(&settings, "one/model")
            .expect("old attempt")[0]
            .clone();
        let key = failure_key(&old_attempt);
        runtime.record_quota_exhausted(&old_attempt, Some(Duration::from_secs(1)));
        runtime.hard_cooldowns.insert(
            key.clone(),
            Instant::now() - Duration::from_secs(1),
        );
        let recovered = runtime
            .candidates(&settings, "one/model")
            .expect("recovered generation")[0]
            .clone();
        runtime.record_success(&recovered, None);

        runtime.record_quota_exhausted(&old_attempt, Some(Duration::from_secs(7_200)));

        let remaining = runtime.hard_cooldowns[&key]
            .saturating_duration_since(Instant::now());
        assert!(remaining > Duration::from_secs(7_100));
        assert!(runtime.transient_quota_exhausted.contains(&key));
        assert!(runtime.candidates(&settings, "one/model").is_err());
        assert_ne!(
            runtime.routing_generation(&key),
            old_attempt.routing_generation,
            "late hard evidence advances the generation fence"
        );
    }

    #[test]
    fn replaced_runtime_epoch_rejects_late_attempt_outcomes_for_the_same_key() {
        let mut settings = settings();
        settings.account_pool.accounts = vec![account("only", 100)];
        let mut old_runtime = RoutingRuntime::default();
        let old_candidate = old_runtime
            .candidates(&settings, "one/model")
            .expect("old runtime candidate")[0]
            .clone();
        old_runtime
            .begin_attempt(&old_candidate)
            .expect("old runtime attempt");

        let mut replacement = RoutingRuntime::default();
        let current_candidate = replacement
            .candidates(&settings, "one/model")
            .expect("replacement runtime candidate")[0]
            .clone();
        replacement
            .begin_attempt(&current_candidate)
            .expect("replacement runtime attempt");
        let key = failure_key(&current_candidate);
        let generation = replacement.routing_generation(&key);

        assert_ne!(old_candidate.routing_epoch, current_candidate.routing_epoch);
        assert!(replacement.begin_attempt(&old_candidate).is_err());
        replacement.end_attempt(&old_candidate);
        replacement.record_success(&old_candidate, Some(100));
        replacement.record_failure(&old_candidate);
        replacement.record_quota_exhausted(
            &old_candidate,
            Some(Duration::from_secs(7_200)),
        );

        assert_eq!(replacement.active_attempts(&current_candidate), 1);
        assert_eq!(replacement.routing_generation(&key), generation);
        assert!(!replacement.hard_cooldowns.contains_key(&key));
        assert!(!replacement.failures.contains_key(&key));
        assert!(!replacement.quota_usage_percent.contains_key(&key));
        replacement.end_attempt(&current_candidate);
    }

    #[test]
    fn routing_key_capacity_fails_closed_before_a_new_direct_target_is_returned() {
        let mut settings = settings();
        settings.routes.clear();
        settings.account_pool.accounts.clear();
        let mut runtime = RoutingRuntime::default();
        for index in 0..MAX_ROUTING_RUNTIME_KEYS {
            let model = format!("one/arbitrary-{index}");
            let candidates = runtime
                .candidates(&settings, &model)
                .expect("routing key within capacity");
            assert_eq!(candidates.len(), 1);
            runtime
                .begin_attempt(&candidates[0])
                .expect("live attempt reserves routing key capacity");
        }
        let error = runtime
            .candidates(&settings, "one/arbitrary-overflow")
            .expect_err("new routing key must fail closed before upstream send");
        assert!(error.contains("routing runtime key capacity reached before sending"));
        assert_eq!(runtime.routing_generations.len(), MAX_ROUTING_RUNTIME_KEYS);
    }

    #[test]
    fn expired_hard_key_reclaims_all_linked_runtime_maps_and_capacity() {
        let mut runtime = RoutingRuntime::default();
        for index in 0..MAX_ROUTING_RUNTIME_KEYS {
            let key = format!("provider/model-{index}");
            runtime.routing_generations.insert(
                key.clone(),
                RoutingGenerationState {
                    generation: 7,
                    active_leases: 0,
                },
            );
            runtime.transient_quota_exhausted.insert(key.clone());
            runtime
                .hard_cooldowns
                .insert(key, Instant::now() - Duration::from_secs(1));
        }
        assert_eq!(runtime.reserve_routing_key("provider/fresh"), Ok(0));
        assert!(runtime.routing_generations.is_empty());
        assert!(runtime.transient_quota_exhausted.is_empty());
        assert!(runtime.hard_cooldowns.is_empty());
    }

    #[test]
    fn active_hard_cooldowns_are_never_evicted_to_admit_a_new_key() {
        let mut runtime = RoutingRuntime::default();
        let deadline = Instant::now() + Duration::from_secs(7_200);
        for index in 0..MAX_ROUTING_RUNTIME_KEYS {
            let key = format!("provider/model-{index}");
            runtime.routing_generations.insert(
                key.clone(),
                RoutingGenerationState {
                    generation: 1,
                    active_leases: 0,
                },
            );
            runtime.transient_quota_exhausted.insert(key.clone());
            runtime.hard_cooldowns.insert(key, deadline);
        }
        let first = "provider/model-0";
        let error = runtime
            .reserve_routing_key("provider/overflow")
            .expect_err("active hard cooldown capacity must fail closed");
        assert!(error.contains("capacity reached before sending"));
        assert_eq!(runtime.routing_generations.len(), MAX_ROUTING_RUNTIME_KEYS);
        assert_eq!(runtime.hard_cooldowns.len(), MAX_ROUTING_RUNTIME_KEYS);
        assert_eq!(runtime.transient_quota_exhausted.len(), MAX_ROUTING_RUNTIME_KEYS);
        assert_eq!(runtime.hard_cooldowns.get(first), Some(&deadline));
    }

    #[test]
    fn inactive_soft_failures_and_generations_are_reclaimed_before_capacity_rejection() {
        let mut settings = settings();
        settings.routes.clear();
        settings.account_pool.accounts.clear();
        let mut runtime = RoutingRuntime::default();
        for index in 0..MAX_ROUTING_RUNTIME_KEYS {
            let candidate = runtime
                .candidates(&settings, &format!("one/failed-{index}"))
                .expect("soft failure candidate")[0]
                .clone();
            runtime.begin_attempt(&candidate).expect("soft failure lease");
            runtime.record_failure(&candidate);
            runtime.end_attempt(&candidate);
        }
        assert!(runtime
            .candidates(&settings, "one/overflow")
            .is_err());
        for failure in runtime.failures.values_mut() {
            failure.last_activity =
                Instant::now() - SOFT_FAILURE_RETENTION - Duration::from_secs(1);
        }
        assert!(runtime.candidates(&settings, "one/fresh").is_ok());
        assert!(runtime.routing_generations.is_empty());
        assert!(runtime.failures.is_empty());
    }

    #[test]
    fn live_attempt_lease_survives_cleanup_beyond_the_old_five_minute_timeout() {
        let mut settings = settings();
        settings.routes.clear();
        settings.account_pool.accounts.clear();
        let mut runtime = RoutingRuntime::default();
        let candidate = runtime
            .candidates(&settings, "one/long-running")
            .expect("long running candidate")[0]
            .clone();
        runtime.begin_attempt(&candidate).expect("live attempt lease");
        let key = failure_key(&candidate);

        runtime.cleanup_inactive_routing_keys(
            Instant::now() + Duration::from_secs(6 * 60),
        );

        assert_eq!(runtime.routing_generations[&key].active_leases, 1);
        runtime.record_quota_exhausted(&candidate, Some(Duration::from_secs(7_200)));
        assert!(runtime.hard_cooldowns.contains_key(&key));
        assert_ne!(runtime.routing_generation(&key), candidate.routing_generation);
    }

    #[test]
    fn direct_provider_and_all_cooled_route_targets_do_not_fail_open() {
        let mut direct_settings = settings();
        direct_settings.routes.clear();
        let mut runtime = RoutingRuntime::default();
        let direct = runtime
            .candidates(&direct_settings, "one/model")
            .expect("direct provider")[0]
            .clone();
        runtime.record_quota_exhausted(&direct, Some(Duration::from_secs(7_200)));
        let direct_error = runtime
            .candidates(&direct_settings, "one/model")
            .expect_err("direct cooled target must be excluded");
        assert!(direct_error.contains("cooling down"));

        let route_settings = settings();
        let mut route_runtime = RoutingRuntime::default();
        let candidates = route_runtime
            .candidates(&route_settings, "reliable")
            .expect("route candidates");
        for candidate in &candidates {
            route_runtime.record_quota_exhausted(
                candidate,
                Some(Duration::from_secs(7_200)),
            );
        }
        let route_error = route_runtime
            .candidates(&route_settings, "reliable")
            .expect_err("all cooled route targets must not be probed");
        assert!(route_error.contains("all targets cooling down"));
    }

    fn image_provider(id: &str, source: CredentialSource) -> ProviderDefinition {
        let mut provider = ProviderDefinition {
            id: id.into(),
            name: id.into(),
            base_url: if id == "openai" {
                "https://chatgpt.com/backend-api/codex".into()
            } else {
                "https://api.openai.com/v1".into()
            },
            models: vec!["gpt-5.6".into()],
            image_generation_models: vec!["imagegen-2".into(), "gpt-image-2".into()],
            credential: ProviderCredential {
                source,
                ..ProviderCredential::default()
            },
            capabilities: ProviderCapabilities {
                image_generation: true,
                ..ProviderCapabilities::default()
            },
            ..ProviderDefinition::default()
        };
        provider
            .model_wire_ids
            .insert("imagegen-2".into(), "gpt-image-2".into());
        provider
    }

    #[test]
    fn built_in_imagegen_alias_uses_codex_forward_and_real_wire_model() {
        let mut settings = GatewaySettings {
            default_provider: Some("chat-only".into()),
            providers: vec![
                image_provider("openai", CredentialSource::Forward),
                ProviderDefinition {
                    id: "chat-only".into(),
                    name: "Chat only".into(),
                    enabled: true,
                    ..ProviderDefinition::default()
                },
            ],
            ..GatewaySettings::default()
        };
        settings.providers[1].capabilities.image_generation = false;
        let candidates = RoutingRuntime::default()
            .candidates_for_image_generation(&settings, None, Some("imagegen-2"), None)
            .expect("Codex image route");

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].provider.id, "openai");
        assert_eq!(candidates[0].upstream_model, "imagegen-2");
        assert_eq!(
            candidates[0]
                .provider
                .wire_model_id(&candidates[0].upstream_model),
            "gpt-image-2"
        );
    }

    #[test]
    fn fully_qualified_image_sidecar_overrides_codex_forward_with_api_key_provider() {
        let settings = GatewaySettings {
            providers: vec![
                image_provider("openai", CredentialSource::Forward),
                image_provider("openai-api", CredentialSource::Environment),
            ],
            ..GatewaySettings::default()
        };
        let candidates = RoutingRuntime::default()
            .candidates_for_image_generation(
                &settings,
                Some("openai-api/gpt-image-2"),
                Some("imagegen-2"),
                None,
            )
            .expect("explicit image sidecar");

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].provider.id, "openai-api");
        assert_eq!(candidates[0].upstream_model, "gpt-image-2");
        assert_eq!(
            candidates[0].provider.credential.source,
            CredentialSource::Environment
        );
    }

    #[test]
    fn image_routing_never_falls_back_to_chat_default_provider() {
        let settings = GatewaySettings {
            default_provider: Some("opencode-go".into()),
            providers: vec![ProviderDefinition {
                id: "opencode-go".into(),
                name: "OpenCode Go".into(),
                models: vec!["imagegen-2".into()],
                capabilities: ProviderCapabilities {
                    image_generation: false,
                    ..ProviderCapabilities::default()
                },
                ..ProviderDefinition::default()
            }],
            ..GatewaySettings::default()
        };
        let candidates = RoutingRuntime::default()
            .candidates_for_image_generation(&settings, None, Some("imagegen-2"), None)
            .expect("unconfigured image route");

        assert!(candidates.is_empty());
        let explicit_unknown = RoutingRuntime::default()
            .candidates_for_image_generation(
                &settings,
                None,
                Some("missing-provider/gpt-image-2"),
                None,
            )
            .expect("unknown explicit image provider");
        assert!(explicit_unknown.is_empty());
    }

    #[test]
    fn image_only_models_are_rejected_by_normal_responses_routing() {
        let settings = GatewaySettings {
            default_provider: Some("openai-api".into()),
            providers: vec![image_provider(
                "openai-api",
                CredentialSource::Environment,
            )],
            ..GatewaySettings::default()
        };

        for model in [
            "openai-api/imagegen-2",
            "openai-api/gpt-image-2",
            "imagegen-2",
            "gpt-image-2",
        ] {
            let error = RoutingRuntime::default()
                .candidates(&settings, model)
                .expect_err("image-only model must not use Responses routing");
            assert!(error.contains("image-generation-only"));
        }

        let image = RoutingRuntime::default()
            .candidates_for_image_generation(
                &settings,
                Some("openai-api/gpt-image-2"),
                Some("imagegen-2"),
                None,
            )
            .expect("image endpoint route");
        assert_eq!(image.len(), 1);
        assert_eq!(image[0].upstream_model, "gpt-image-2");
    }

    #[test]
    fn image_only_routes_are_rejected_normally_but_work_for_image_requests() {
        let settings = GatewaySettings {
            providers: vec![image_provider(
                "openai-api",
                CredentialSource::Environment,
            )],
            routes: vec![RouteDefinition {
                id: "image-route".into(),
                name: "Image route".into(),
                alias: Some("img-route".into()),
                targets: vec![RouteTarget {
                    model: "openai-api/gpt-image-2".into(),
                    weight: 1,
                }],
                enabled: true,
                ..RouteDefinition::default()
            }],
            ..GatewaySettings::default()
        };

        assert!(RoutingRuntime::default()
            .candidates(&settings, "img-route")
            .is_err());
        let image = RoutingRuntime::default()
            .candidates_for_image_generation(&settings, Some("img-route"), Some("imagegen-2"), None)
            .expect("image route");
        assert_eq!(image.len(), 1);
        assert_eq!(image[0].upstream_model, "gpt-image-2");
    }

    #[test]
    fn unavailable_image_identity_never_falls_back_into_normal_routes() {
        let mut provider = image_provider("openai-api", CredentialSource::Environment);
        provider.capabilities.image_generation = false;
        let settings = GatewaySettings {
            providers: vec![provider],
            routes: vec![RouteDefinition {
                id: "unavailable-image".into(),
                name: "Unavailable image".into(),
                targets: vec![RouteTarget {
                    model: "openai-api/gpt-image-2".into(),
                    weight: 1,
                }],
                enabled: true,
                ..RouteDefinition::default()
            }],
            ..GatewaySettings::default()
        };

        assert!(RoutingRuntime::default()
            .candidates(&settings, "unavailable-image")
            .is_err());
        assert!(RoutingRuntime::default()
            .candidates_for_image_generation(
                &settings,
                Some("unavailable-image"),
                Some("gpt-image-2"),
                None,
            )
            .is_err());
    }

    #[test]
    fn normal_mixed_routes_drop_image_targets_and_keep_chat_targets() {
        let mut chat = image_provider("chat", CredentialSource::Environment);
        chat.image_generation_models.clear();
        chat.capabilities.image_generation = false;
        let settings = GatewaySettings {
            providers: vec![
                image_provider("openai-api", CredentialSource::Environment),
                chat,
            ],
            routes: vec![RouteDefinition {
                id: "mixed".into(),
                name: "Mixed".into(),
                targets: vec![
                    RouteTarget {
                        model: "openai-api/gpt-image-2".into(),
                        weight: 1,
                    },
                    RouteTarget {
                        model: "chat/gpt-5.6".into(),
                        weight: 1,
                    },
                ],
                enabled: true,
                ..RouteDefinition::default()
            }],
            ..GatewaySettings::default()
        };

        let normal = RoutingRuntime::default()
            .candidates(&settings, "mixed")
            .expect("normal route");
        assert_eq!(normal.len(), 1);
        assert_eq!(normal[0].target_key, "chat/gpt-5.6");

        let image = RoutingRuntime::default()
            .candidates_for_image_generation(&settings, Some("mixed"), Some("imagegen-2"), None)
            .expect("image route");
        assert_eq!(image.len(), 1);
        assert_eq!(image[0].target_key, "openai-api/gpt-image-2");
    }

    #[test]
    fn mixed_route_cursors_are_independent_for_normal_and_image_requests() {
        let mut normal_one = image_provider("normal-one", CredentialSource::Environment);
        normal_one.image_generation_models.clear();
        normal_one.capabilities.image_generation = false;
        let mut normal_two = normal_one.clone();
        normal_two.id = "normal-two".into();
        normal_two.name = "normal-two".into();
        let settings = GatewaySettings {
            providers: vec![
                image_provider("images", CredentialSource::Environment),
                normal_one,
                normal_two,
            ],
            routes: vec![RouteDefinition {
                id: "mixed-cursor".into(),
                name: "Mixed cursor".into(),
                strategy: RouteStrategy::WeightedRoundRobin,
                sticky_requests: 1,
                targets: vec![
                    RouteTarget { model: "images/gpt-image-2".into(), weight: 1 },
                    RouteTarget { model: "normal-one/gpt-5.6".into(), weight: 1 },
                    RouteTarget { model: "normal-two/gpt-5.6".into(), weight: 1 },
                ],
                enabled: true,
                ..RouteDefinition::default()
            }],
            ..GatewaySettings::default()
        };
        let mut runtime = RoutingRuntime::default();

        let normal_first = runtime.candidates(&settings, "mixed-cursor").expect("normal first");
        let image = runtime
            .candidates_for_image_generation(&settings, Some("mixed-cursor"), Some("gpt-image-2"), None)
            .expect("image request");
        let normal_second = runtime.candidates(&settings, "mixed-cursor").expect("normal second");

        assert_eq!(normal_first[0].target_key, "normal-one/gpt-5.6");
        assert_eq!(image[0].target_key, "images/gpt-image-2");
        assert_eq!(normal_second[0].target_key, "normal-two/gpt-5.6");
    }

    #[test]
    fn image_sidecar_pseudo_model_is_never_a_normal_responses_model() {
        let settings = GatewaySettings {
            sidecars: crate::config::SidecarSettings {
                image_model: Some("openai-api/gpt-image-2".into()),
                ..crate::config::SidecarSettings::default()
            },
            providers: vec![image_provider(
                "openai-api",
                CredentialSource::Environment,
            )],
            ..GatewaySettings::default()
        };

        let error = RoutingRuntime::default()
            .candidates(&settings, "codetas-sidecar/image")
            .expect_err("image sidecar must not enter normal routing");
        assert!(error.contains("image-generation-only"));
    }

    #[test]
    fn disabled_model_level_image_capability_excludes_provider() {
        let settings = GatewaySettings {
            providers: vec![image_provider("openai", CredentialSource::Forward)],
            model_catalog: vec![crate::config::ModelMetadata {
                provider_id: "openai".into(),
                model_id: "imagegen-2".into(),
                capabilities: ProviderCapabilities {
                    image_generation: false,
                    ..ProviderCapabilities::default()
                },
                ..crate::config::ModelMetadata::default()
            }],
            ..GatewaySettings::default()
        };
        let candidates = RoutingRuntime::default()
            .candidates_for_image_generation(&settings, None, Some("imagegen-2"), None)
            .expect("model-level exclusion");

        assert!(candidates.is_empty());
    }

    #[test]
    fn soft_failures_are_scoped_per_session_and_do_not_bleed_across_sessions() {
        let mut settings = settings();
        settings.routes.clear();
        let mut runtime = RoutingRuntime::default();
        let session_a = Some("session-a");
        let session_b = Some("session-b");

        // Three soft failures from session A push the target into a 60s
        // soft cooldown for session A only.
        for _ in 0..3 {
            let candidate = runtime
                .candidates_scoped(&settings, "one/model", session_a)
                .expect("session a candidate")[0]
                .clone();
            assert_eq!(candidate.session_scope.as_deref(), session_a);
            runtime.record_failure(&candidate);
        }
        let err_a = runtime
            .candidates_scoped(&settings, "one/model", session_a)
            .expect_err("session a must observe its own soft cooldown");
        assert!(err_a.contains("cooling down"));

        // Session B and un-scoped resolution stay available: a soft failure in
        // one session must not cool the same provider target globally.
        assert!(runtime
            .candidates_scoped(&settings, "one/model", session_b)
            .is_ok());
        assert!(runtime.candidates(&settings, "one/model").is_ok());

        // Session A's failures live under its session bucket, never under the
        // global credential key.
        assert!(runtime.failures.contains_key("session-a::one/model"));
        assert!(!runtime.failures.contains_key("one/model"));
    }

    #[test]
    fn session_scoped_soft_cooldown_is_hard_quota_aware() {
        let mut settings = settings();
        settings.routes.clear();
        let mut runtime = RoutingRuntime::default();
        let candidate = runtime
            .candidates_scoped(&settings, "one/model", Some("session-a"))
            .expect("candidate")[0]
            .clone();

        // A real 429 from the provider globally hard-cools the credential
        // target for every session, overriding session-scoped soft state.
        runtime.record_quota_exhausted(&candidate, Some(Duration::from_secs(7_200)));
        assert!(runtime
            .candidates_scoped(&settings, "one/model", Some("session-a"))
            .is_err());
        assert!(runtime
            .candidates_scoped(&settings, "one/model", Some("session-b"))
            .is_err());
        assert!(runtime.candidates(&settings, "one/model").is_err());
    }

    #[test]
    fn success_in_one_session_only_clears_that_sessions_soft_failures() {
        let mut settings = settings();
        settings.routes.clear();
        let mut runtime = RoutingRuntime::default();
        for _ in 0..3 {
            let candidate = runtime
                .candidates_scoped(&settings, "one/model", Some("session-a"))
                .expect("session a candidate")[0]
                .clone();
            runtime.record_failure(&candidate);
        }
        assert!(runtime
            .candidates_scoped(&settings, "one/model", Some("session-a"))
            .is_err());

        // A success (e.g. a follow-up send that succeeded in session B) must
        // not clear session A's soft failure count.
        let session_b_candidate = runtime
            .candidates_scoped(&settings, "one/model", Some("session-b"))
            .expect("session b candidate")[0]
            .clone();
        runtime.record_success(&session_b_candidate, None);
        assert!(runtime
            .candidates_scoped(&settings, "one/model", Some("session-a"))
            .is_err());
        assert!(runtime.failures.contains_key("session-a::one/model"));
        assert!(!runtime.failures.contains_key("session-b::one/model"));
    }
}
