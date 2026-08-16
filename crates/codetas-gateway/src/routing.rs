use crate::config::{
    effective_model_capabilities, image_model_is_available, model_is_image_generation_only,
    AccountPoolStrategy, AccountReference, CredentialSource, GatewaySettings, ProviderCapabilities,
    ProviderCredential, ProviderDefinition, RouteDefinition, RoutePolicySettings, RouteStrategy,
    RouteTarget,
};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const DEFAULT_FAILURE_THRESHOLD: u8 = 3;
const COOLDOWN: Duration = Duration::from_secs(60);
const MAX_RETRY_AFTER_COOLDOWN: Duration = Duration::from_secs(10 * 60);
const MAX_ROUTING_RUNTIME_KEYS: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RoutePurpose {
    Normal,
    ImageGeneration,
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
}

#[derive(Clone, Debug, Serialize)]
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

#[derive(Clone, Debug, Serialize)]
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

#[derive(Clone, Debug, Default)]
struct FailureState {
    consecutive: u8,
    retry_after: Option<Instant>,
}

#[derive(Clone, Default)]
pub(crate) struct RoutingRuntime {
    route_calls: HashMap<String, u64>,
    account_calls: HashMap<String, u64>,
    target_requests: HashMap<String, u64>,
    quota_usage_percent: HashMap<String, u8>,
    failures: HashMap<String, FailureState>,
}

impl RoutingRuntime {
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
            .candidates_for_request(settings, requested_model, is_subagent)
            .unwrap_or_default();
        if let Some(route) = settings.routes.iter().find(|route| {
            route.id == requested_model || route.alias.as_deref() == Some(requested_model)
        }) {
            return self.dry_run_route(settings, route, requested_model, &actual);
        }
        let rows = if actual.is_empty() {
            let error = self
                .candidates_for_request(settings, requested_model, is_subagent)
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
                    let quota = self
                        .quota_usage_percent
                        .get(&failure_key(&candidate))
                        .copied()
                        .unwrap_or(0);
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
            let preferred = accounts.iter().position(|account| account.pinned).or_else(|| {
                settings.account_pool.active_accounts.get(provider_id)
                    .and_then(|active| accounts.iter().position(|account| account.id == *active))
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
                let candidate = probe_runtime.expand_accounts(
                    &probe_settings, provider.clone(), model_id.to_string(), requested_model.to_string(),
                    Some(route.id.clone()), route.failure_threshold, route.default_reasoning_effort.clone(),
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
                let quota = self.quota_usage_percent.get(&key).copied().unwrap_or(0);
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
                match self.candidates(settings, model) {
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
        self.candidates(settings, requested_model)
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
    ) -> Result<Vec<RouteCandidate>, String> {
        if let Some(target) = configured_target.map(str::trim).filter(|target| !target.is_empty()) {
            return self.image_capable_candidates_for_explicit_target(settings, target);
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
            return self.image_capable_candidates_for_explicit_target(settings, requested_model);
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
        )
    }

    fn image_capable_candidates_for_explicit_target(
        &mut self,
        settings: &GatewaySettings,
        target: &str,
    ) -> Result<Vec<RouteCandidate>, String> {
        let candidates = if let Some(route) = settings.routes.iter().find(|route| {
            route.enabled && (route.id == target || route.alias.as_deref() == Some(target))
        }) {
            self.route_candidates(settings, route, target, RoutePurpose::ImageGeneration)?
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
        let requested_model = requested_model.trim();
        if let Some(target) = helper_intercept_target(settings, requested_model) {
            let mut candidates = self.candidates_core(settings, target)?;
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
        self.candidates_core(settings, requested_model)
    }

    fn candidates_core(
        &mut self,
        settings: &GatewaySettings,
        requested_model: &str,
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
            let mut candidates = self.candidates(settings, target)?;
            for candidate in &mut candidates {
                candidate.exposed_model = requested_model.to_string();
            }
            return Ok(candidates);
        }
        if let Some(route) = settings.routes.iter().find(|route| {
            route.enabled
                && (route.id == requested_model || route.alias.as_deref() == Some(requested_model))
        }) {
            return self.route_candidates(settings, route, requested_model, RoutePurpose::Normal);
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
        )
    }

    pub fn record_success(&mut self, candidate: &RouteCandidate, quota_usage_percent: Option<u8>) {
        ensure_runtime_capacity(&mut self.target_requests, &candidate.target_key);
        *self
            .target_requests
            .entry(candidate.target_key.clone())
            .or_default() += 1;
        self.failures.remove(&failure_key(candidate));
        if let Some(percent) = quota_usage_percent {
            let key = failure_key(candidate);
            ensure_runtime_capacity(&mut self.quota_usage_percent, &key);
            self.quota_usage_percent.insert(key, percent.min(100));
        }
    }

    pub fn record_quota_exhausted(
        &mut self,
        candidate: &RouteCandidate,
        retry_after: Option<Duration>,
    ) {
        let key = failure_key(candidate);
        ensure_runtime_capacity(&mut self.quota_usage_percent, &key);
        self.quota_usage_percent.insert(key.clone(), 100);
        ensure_runtime_capacity(&mut self.failures, &key);
        let failure = self.failures.entry(key).or_default();
        failure.consecutive = 0;
        failure.retry_after = Some(
            Instant::now()
                + retry_after
                    .unwrap_or(COOLDOWN)
                    .min(MAX_RETRY_AFTER_COOLDOWN),
        );
    }

    pub fn record_failure(&mut self, candidate: &RouteCandidate) {
        let key = failure_key(candidate);
        ensure_runtime_capacity(&mut self.failures, &key);
        let failure = self.failures.entry(key).or_default();
        failure.consecutive = failure.consecutive.saturating_add(1);
        if failure.consecutive >= candidate.failure_threshold.max(1) {
            failure.retry_after = Some(Instant::now() + COOLDOWN);
            failure.consecutive = 0;
        }
    }

    fn route_candidates(
        &mut self,
        settings: &GatewaySettings,
        route: &RouteDefinition,
        exposed_model: &str,
        purpose: RoutePurpose,
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
                    !model_is_image_generation_only(settings, provider, model_id)
                }
                RoutePurpose::ImageGeneration => {
                    image_model_is_available(settings, provider, model_id)
                }
            }
        });
        match route.strategy {
            RouteStrategy::Failover => {}
            RouteStrategy::WeightedRoundRobin => {
                targets = weighted_order(
                    targets,
                    next_sticky_index(&mut self.route_calls, &route.id, route.sticky_requests),
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
            .filter(|target| !self.is_cooling(&target.model))
            .cloned()
            .collect::<Vec<_>>();
        if !available.is_empty() {
            targets = available;
        }

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
            );
            if let Ok(expanded) = expanded {
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
        }
        if candidates.is_empty() {
            return Err(format!(
                "route {} has no available provider account",
                route.id
            ));
        }
        if route.strategy == RouteStrategy::Policy {
            candidates.sort_by(|left, right| {
                let left_quota = self
                    .quota_usage_percent
                    .get(&failure_key(left))
                    .copied()
                    .unwrap_or(0);
                let right_quota = self
                    .quota_usage_percent
                    .get(&failure_key(right))
                    .copied()
                    .unwrap_or(0);
                let left_health = self.candidate_health_percent(left);
                let right_health = self.candidate_health_percent(right);
                route_policy_score(right, right_quota, right_health, &route.policy)
                    .cmp(&route_policy_score(left, left_quota, left_health, &route.policy))
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
            return Ok(vec![RouteCandidate {
                provider,
                upstream_model,
                exposed_model,
                credential: None,
                account_id: None,
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
            !self.is_cooling(&key)
                && (settings.account_pool.auto_switch_threshold_percent == 0
                    || self.quota_usage_percent.get(&key).copied().unwrap_or(0)
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

        Ok(accounts
            .into_iter()
            .map(|account| RouteCandidate {
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
            })
            .collect())
    }

    fn order_accounts_by_priority_tier(
        &mut self,
        settings: &GatewaySettings,
        provider_id: &str,
        target_key: &str,
        mut accounts: Vec<AccountReference>,
    ) -> Vec<AccountReference> {
        let preferred_id = accounts
            .iter()
            .find(|account| account.pinned)
            .map(|account| account.id.clone())
            .or_else(|| settings.account_pool.active_accounts.get(provider_id).cloned());
        let preferred = preferred_id.as_deref()
            .and_then(|id| accounts.iter().position(|account| account.id == id))
            .map(|index| accounts.remove(index));

        accounts.sort_by_key(|account| std::cmp::Reverse(account.priority));
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
                    self.quota_usage_percent.get(&key).copied().unwrap_or(0)
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
        let Some(failure) = self.failures.get_mut(key) else {
            return false;
        };
        match failure.retry_after {
            Some(retry_after) if retry_after > Instant::now() => true,
            Some(_) => {
                failure.retry_after = None;
                failure.consecutive = 0;
                false
            }
            None => false,
        }
    }

    fn candidate_health_percent(&self, candidate: &RouteCandidate) -> u8 {
        let Some(failure) = self.failures.get(&failure_key(candidate)) else {
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
            if model_is_image_generation_only(settings, provider, model) {
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
        if model_is_image_generation_only(settings, provider, requested_model) {
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
    if model_is_image_generation_only(settings, &provider, &upstream_model) {
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
        runtime.route_calls.insert("reliable".into(), 3);
        runtime.account_calls.insert("account:one:priority:0".into(), 5);
        runtime.failures.insert("two/model".into(), FailureState {
            consecutive: 2,
            retry_after: Some(Instant::now() - Duration::from_secs(1)),
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
                    runtime.route_calls.insert("reliable".into(), 1);
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
            .candidates_for_request(&settings, "parent/model", true)
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
            .candidates_for_request(&settings, "parent/model", true)
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
        });
        runtime.quota_usage_percent.insert("one/model#quota".into(), 100);
        let candidates = runtime.candidates(&settings, "one/model").expect("lower tier");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].account_id.as_deref(), Some("low"));
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
            .candidates_for_image_generation(&settings, None, Some("imagegen-2"))
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
            .candidates_for_image_generation(&settings, None, Some("imagegen-2"))
            .expect("unconfigured image route");

        assert!(candidates.is_empty());
        let explicit_unknown = RoutingRuntime::default()
            .candidates_for_image_generation(
                &settings,
                None,
                Some("missing-provider/gpt-image-2"),
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
            .candidates_for_image_generation(&settings, Some("img-route"), Some("imagegen-2"))
            .expect("image route");
        assert_eq!(image.len(), 1);
        assert_eq!(image[0].upstream_model, "gpt-image-2");
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
            .candidates_for_image_generation(&settings, Some("mixed"), Some("imagegen-2"))
            .expect("image route");
        assert_eq!(image.len(), 1);
        assert_eq!(image[0].target_key, "openai-api/gpt-image-2");
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
            .candidates_for_image_generation(&settings, None, Some("imagegen-2"))
            .expect("model-level exclusion");

        assert!(candidates.is_empty());
    }
}
