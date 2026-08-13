use crate::config::{
    AccountPoolStrategy, GatewaySettings, ProviderCredential, ProviderDefinition, RouteDefinition,
    RouteStrategy, RouteTarget,
};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

const DEFAULT_FAILURE_THRESHOLD: u8 = 3;
const COOLDOWN: Duration = Duration::from_secs(60);
const MAX_RETRY_AFTER_COOLDOWN: Duration = Duration::from_secs(10 * 60);
const MAX_ROUTING_RUNTIME_KEYS: usize = 4_096;

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
}

#[derive(Clone, Debug, Default)]
struct FailureState {
    consecutive: u8,
    retry_after: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct RoutingRuntime {
    route_calls: HashMap<String, u64>,
    account_calls: HashMap<String, u64>,
    target_requests: HashMap<String, u64>,
    quota_usage_percent: HashMap<String, u8>,
    failures: HashMap<String, FailureState>,
}

impl RoutingRuntime {
    pub fn candidates_for_request(
        &mut self,
        settings: &GatewaySettings,
        requested_model: &str,
        is_subagent: bool,
    ) -> Result<Vec<RouteCandidate>, String> {
        if is_subagent
            && settings.agents.multi_agent_v2
            && !settings.agents.subagent_models.is_empty()
        {
            let mut candidates = Vec::new();
            let mut last_error = None;
            for model in settings
                .agents
                .subagent_models
                .iter()
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
            return self.route_candidates(settings, route, requested_model);
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
    ) -> Result<Vec<RouteCandidate>, String> {
        let mut targets = route.targets.clone();
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
                candidates.extend(expanded);
            }
        }
        if candidates.is_empty() {
            return Err(format!(
                "route {} has no available provider account",
                route.id
            ));
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
        let mut accounts = settings
            .account_pool
            .accounts
            .iter()
            .filter(|account| account.enabled && account.provider_id == provider.id)
            .cloned()
            .collect::<Vec<_>>();
        if accounts.is_empty() {
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
            }]);
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

        let selection_key = format!("account:{}", provider.id);
        if let Some(active) = settings.account_pool.active_accounts.get(&provider.id) {
            if let Some(index) = accounts.iter().position(|account| &account.id == active) {
                accounts.rotate_left(index);
            }
        } else {
            match settings.account_pool.strategy {
                AccountPoolStrategy::Quota => accounts.sort_by_key(|account| {
                    let key = format!("{target_key}#{}", account.id);
                    self.quota_usage_percent.get(&key).copied().unwrap_or(0)
                }),
                AccountPoolStrategy::RoundRobin => {
                    let primary = next_sticky_index(
                        &mut self.account_calls,
                        &selection_key,
                        settings.account_pool.sticky_requests,
                    ) as usize
                        % accounts.len();
                    accounts.rotate_left(primary);
                }
                AccountPoolStrategy::FillFirst => {}
            }
        }

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
            })
            .collect())
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

    #[test]
    fn expands_a_failover_route() {
        let mut runtime = RoutingRuntime::default();
        let candidates = runtime.candidates(&settings(), "reliable").expect("route");
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].target_key, "one/model");
        assert_eq!(candidates[1].target_key, "two/model");
    }
}
