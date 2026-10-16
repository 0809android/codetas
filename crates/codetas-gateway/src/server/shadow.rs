use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AttemptFailureKind {
    Request,
    ContextWindow,
    Credential,
    Retryable,
}

impl AttemptFailureKind {
    pub(crate) fn category(self) -> &'static str {
        match self {
            Self::Request => "request_rejected",
            Self::ContextWindow => "context_window_exceeded",
            Self::Credential => "credential_unavailable",
            Self::Retryable => "provider_unreachable",
        }
    }

    pub(crate) fn feeds_cooldown(self) -> bool {
        matches!(self, Self::Retryable)
    }
}

#[derive(Clone)]
pub(crate) struct ObservationSeed {
    pub(crate) ledger: ObservabilityLedger,
    pub(crate) settings: ObservabilitySettings,
    pub(crate) request_id: String,
    client_thread_id: Option<String>,
    compaction_boundary: bool,
    pub(crate) provider_id: Option<String>,
    pub(crate) upstream_model: Option<String>,
    pub(crate) upstream_endpoint: Option<String>,
    pub(crate) exposed_model: String,
    pub(crate) route_id: Option<String>,
    pub(crate) account_id: Option<String>,
    pub(crate) streaming: bool,
    pub(crate) started: Instant,
    privacy_origin: Instant,
    pub(crate) attempts: u16,
    pub(crate) candidate_ordinal: u16,
    pub(crate) send_count: u16,
    pub(crate) recovery_kinds: Vec<String>,
    pub(crate) recovery_kind: Option<String>,
    pub(crate) retry_usage: TokenUsage,
    pub(crate) upstream_error: Option<UpstreamErrorDiagnostic>,
    pub(crate) request_diagnostic: Option<crate::observability::RequestDiagnostic>,
    cache_observation: Option<crate::observability::CacheObservation>,
    upstream_usage:Option<TokenUsage>,
    billing_observation:Option<TokenUsage>,
    pub(crate) shared_provider_retries: Option<SharedProviderRetryObservation>,
    pub(crate) attempt_only: bool,
    pub(crate) input_price_per_million: Option<f64>,
    pub(crate) output_price_per_million: Option<f64>,
}

impl ObservationSeed {
    pub(crate) fn without_candidate(
        ledger: ObservabilityLedger,
        settings: ObservabilitySettings,
        request_id: String,
        exposed_model: &str,
        streaming: bool,
        started: Instant,
    ) -> Self {
        Self {
            ledger,
            settings,
            request_id,
            client_thread_id: None,
            compaction_boundary: false,
            provider_id: None,
            upstream_model: None,
            upstream_endpoint: None,
            exposed_model: bounded_metadata(exposed_model),
            route_id: None,
            account_id: None,
            streaming,
            started,
            privacy_origin: started,
            attempts: 0,
            candidate_ordinal: 0,
            send_count: 0,
            recovery_kinds: Vec::new(),
            recovery_kind: None,
            retry_usage: TokenUsage::default(),
            upstream_error: None,
            request_diagnostic: None,
            cache_observation: None,
            upstream_usage:None,
            billing_observation:None,
            shared_provider_retries: None,
            attempt_only: false,
            input_price_per_million: None,
            output_price_per_million: None,
        }
    }

    pub(crate) fn for_candidate(
        ledger: ObservabilityLedger,
        settings: ObservabilitySettings,
        request_id: String,
        streaming: bool,
        started: Instant,
        attempts: u16,
        candidate: &RouteCandidate,
    ) -> Self {
        Self {
            ledger,
            settings,
            request_id,
            client_thread_id: None,
            compaction_boundary: false,
            provider_id: Some(candidate.provider.id.clone()),
            upstream_model: Some(bounded_metadata(&candidate.upstream_model)),
            upstream_endpoint: None,
            exposed_model: bounded_metadata(&candidate.exposed_model),
            route_id: candidate.route_id.clone(),
            account_id: candidate.account_id.clone(),
            streaming,
            started,
            privacy_origin: started,
            attempts,
            candidate_ordinal: attempts,
            send_count: 1,
            recovery_kinds: Vec::new(),
            recovery_kind: None,
            retry_usage: TokenUsage::default(),
            upstream_error: None,
            request_diagnostic: None,
            cache_observation: None,
            upstream_usage:None,
            billing_observation:None,
            shared_provider_retries: None,
            attempt_only: false,
            input_price_per_million: candidate.input_price_per_million,
            output_price_per_million: candidate.output_price_per_million,
        }
    }

    pub(crate) fn with_privacy_origin(mut self, origin: Instant) -> Self {
        self.privacy_origin = origin;
        self
    }

    pub(crate) fn with_compaction_session(mut self, headers: &HeaderMap) -> Self {
        self.client_thread_id = diagnostics::client_thread_id(Some(headers));
        self.compaction_boundary = true;
        self
    }

    pub(crate) fn record_provider_retries(&mut self, retry: &ProviderRetryObservation) {
        if let Some(diagnostic) = &retry.request_diagnostic { self.request_diagnostic = Some(diagnostic.clone()); }
        if let Some(billing)=&retry.known_billing {self.billing_observation=Some(billing.clone());}
        self.send_count = self.send_count.saturating_add(retry.additional_sends);
        self.recovery_kinds.extend(retry.recovery_kinds.clone());
        if self.recovery_kind.is_none() {
            self.recovery_kind = retry.recovery_kinds.first().cloned();
        }
        self.retry_usage = sum_token_usage(self.retry_usage.clone(), retry.usage.clone());
        if let Some(observation) = &retry.cache_observation { self.cache_observation = Some(observation.clone()); }
    }

    /// Record a gateway-owned recovery that is not an upstream provider retry
    /// (for example rebasing after a missing `previous_response_id`).
    ///
    /// `recovery_kinds` accumulates every distinct kind. `recovery_kind` keeps
    /// the first kind so earlier gateway recoveries are not clobbered by later
    /// ones (callers that need "last writer wins" should set the field directly).
    pub(crate) fn record_recovery(&mut self, kind: &str) {
        if kind.is_empty() {
            return;
        }
        if !self.recovery_kinds.iter().any(|existing| existing == kind) {
            self.recovery_kinds.push(kind.to_string());
        }
        if self.recovery_kind.is_none() {
            self.recovery_kind = Some(kind.to_string());
        }
    }

    pub(crate) fn with_recovery(mut self, kind: Option<&str>) -> Self {
        if let Some(kind) = kind {
            self.record_recovery(kind);
        }
        self
    }

    pub(crate) fn record_upstream_error(&mut self, response: &Response<Body>) {
        self.upstream_usage=response.extensions().get::<crate::observability::ReportedUpstreamUsage>().map(|usage|usage.0.clone());
        self.cache_observation = response.extensions().get::<crate::observability::CacheObservation>().cloned();
        self.request_diagnostic = response
            .extensions()
            .get::<crate::observability::RequestDiagnostic>()
            .cloned();
        self.upstream_error = response
            .extensions()
            .get::<UpstreamErrorDiagnostic>()
            .cloned();
    }

    pub(crate) fn record_reqwest_upstream_error(&mut self, response: &reqwest::Response) {
        self.cache_observation = response.extensions().get::<crate::observability::CacheObservation>().cloned();
        self.request_diagnostic = response
            .extensions()
            .get::<crate::observability::RequestDiagnostic>()
            .cloned();
        self.upstream_error = response
            .extensions()
            .get::<UpstreamErrorDiagnostic>()
            .cloned();
    }

    pub(crate) fn as_attempt(mut self) -> Self {
        self.attempt_only = true;
        self
    }

    pub(crate) fn record_shared_provider_retries(&mut self, retry: SharedProviderRetryObservation) {
        self.shared_provider_retries = Some(retry);
    }

    pub(crate) fn with_upstream_image_details(mut self, wire_model: &str, endpoint: &str) -> Self {
        self.upstream_model = Some(bounded_metadata(wire_model));
        self.upstream_endpoint = safe_endpoint_path(endpoint);
        self
    }

    pub(crate) fn finish(
        mut self,
        status: StatusCode,
        failure_category: Option<&str>,
        usage: TokenUsage,
    ) {
        let mut terminal_failure=false;
        if let Some(shared) = self.shared_provider_retries.take() {
            let retry = shared.0.lock().ok().map(|retry| retry.clone());
            if let Some(retry) = retry.as_ref() {
                terminal_failure=retry.terminal_failure;
                self.record_provider_retries(retry);
            }
        }
        let (status,failure_category)=if terminal_failure && status.is_success() && failure_category.is_none() {
            (StatusCode::BAD_GATEWAY,Some("provider_stream_error"))
        } else {(status,failure_category)};
        let usage=if (self.compaction_boundary || !status.is_success() || failure_category.is_some()) && usage.input_tokens==0 && usage.output_tokens==0 && usage.total_tokens==0 {
            self.upstream_usage.take().unwrap_or(usage)
        } else {usage};
        let mut usage=usage;
        if let Some(billing)=self.billing_observation.take() {usage.merge_max(billing);}
        let cache_observation = self.cache_observation.clone()
            .unwrap_or_else(|| crate::observability::CacheObservation::from_usage(&usage));
        let usage = sum_token_usage(self.retry_usage.clone(), usage);
        let estimated_cost_usd =
            if self.input_price_per_million.is_some() || self.output_price_per_million.is_some() {
                Some(
                    usage.input_tokens as f64 * self.input_price_per_million.unwrap_or(0.0)
                        / 1_000_000.0
                        + usage.output_tokens as f64 * self.output_price_per_million.unwrap_or(0.0)
                            / 1_000_000.0,
                )
            } else {
                None
            };
        if self.compaction_boundary && status.is_success() && failure_category.is_none() && !self.attempt_only {
            self.record_recovery("explicit-compaction");
        }
        if let Some(model) = self.request_diagnostic.as_ref().and_then(|d| d.wire_model.as_deref()) {
            self.upstream_model = Some(bounded_metadata(model));
        }
        let client_thread_id = self.client_thread_id.clone().or_else(|| self.request_diagnostic
            .as_ref().and_then(|d| d.client_thread_id.clone()));
        self.ledger.record_started(
            ObservationEvent {
                usage_suppressed: false,
                client_thread_id,
                cache_observation: Some(cache_observation),
                ledger_sequence: 0,
                timestamp_ms: ObservationEvent::now_ms(),
                request_id: self.request_id,
                provider_id: self.provider_id,
                upstream_model: self.upstream_model,
                upstream_endpoint: self.upstream_endpoint,
                exposed_model: self.exposed_model,
                route_id: self.route_id,
                account_id: self.account_id,
                status_code: status.as_u16(),
                outcome: if status.is_success() && failure_category.is_none() {
                    "success".into()
                } else {
                    "failure".into()
                },
                failure_category: failure_category.map(str::to_string),
                upstream_error: self.upstream_error,
                request_diagnostic: self.request_diagnostic,
                latency_ms: self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                attempts: self.attempts,
                candidate_ordinal: self.candidate_ordinal,
                send_count: self.send_count,
                recovery_kinds: self.recovery_kinds,
                recovery_kind: self.recovery_kind,
                attempt_only: self.attempt_only,
                streaming: self.streaming,
                usage,
                estimated_cost_usd,
                shadow: false,
                shadow_rule_id: None,
                parent_request_id: None,
            },
            self.settings,
            self.privacy_origin,
        );
    }
}

pub(crate) fn bounded_metadata(value: &str) -> String {
    value.chars().take(240).collect()
}

pub(crate) fn schedule_shadow_calls(
    privacy_origin: Instant,
    state: GatewayState,
    body: Value,
    parent_request_id: String,
    requested_model: String,
    primary: RouteCandidate,
) {
    tokio::spawn(async move {
        let (rules, observation_settings) = {
            let settings = state.settings.read().await;
            (
                settings
                    .shadows
                    .iter()
                    .filter(|rule| {
                        rule.enabled
                            && (rule.source_model == requested_model
                                || rule.source_model == primary.target_key)
                            && shadow_sampled(&parent_request_id, &rule.id, rule.sample_percent)
                    })
                    .cloned()
                    .collect::<Vec<_>>(),
                settings.observability.clone(),
            )
        };
        let mut launched = 0_usize;
        for rule in rules {
            for target in &rule.targets {
                if launched >= 8 {
                    return;
                }
                let candidate = {
                    let settings = state.settings.read().await;
                    let mut routing = state.routing.lock().await;
                    routing
                        .candidates_scoped(&settings, target, primary.session_scope.as_deref())
                        .ok()
                        .and_then(|candidates| candidates.into_iter().next())
                };
                let Some(mut candidate) = candidate else {
                    continue;
                };
                if candidate.target_key == primary.target_key
                    && candidate.account_id == primary.account_id
                {
                    continue;
                }
                launched += 1;
                candidate.provider.limits.request_timeout_ms = candidate
                    .provider
                    .limits
                    .request_timeout_ms
                    .min(rule.timeout_ms);
                candidate.provider.limits.max_response_bytes = candidate
                    .provider
                    .limits
                    .max_response_bytes
                    .min(rule.max_response_bytes);
                let mut shadow_body = body.clone();
                shadow_body["stream"] = Value::Bool(false);
                let started = Instant::now();
                let request_id = Uuid::new_v4().to_string();
                let (status, failure, usage, provider_retry) =
                    match apply_candidate_model_policy(&mut shadow_body, &candidate, false) {
                        Err(_) => (
                            StatusCode::BAD_REQUEST,
                            Some("shadow_model_limit"),
                            TokenUsage::default(),
                            None,
                        ),
                        Ok(()) => {
                            match send_candidate(&state, &mut shadow_body, &candidate, None).await {
                                Err(failure) => {
                                    let retry = failure
                                        .response
                                        .extensions()
                                        .get::<ProviderRetryObservation>()
                                        .cloned();
                                    (
                                        failure.response.status(),
                                        Some(failure.kind.category()),
                                        failure.response.extensions().get::<crate::observability::ReportedUpstreamUsage>().map(|usage| usage.0.clone()).unwrap_or_default(),
                                        retry,
                                    )
                                }
                                Ok(response) => shadow_response_result(response, &candidate, rule.max_response_bytes).await,
                            }
                        }
                    };
                let usage = shadow_billing_usage(usage, provider_retry.as_ref());
                let estimated_cost_usd = if candidate.input_price_per_million.is_some()
                    || candidate.output_price_per_million.is_some()
                {
                    Some(
                        usage.input_tokens as f64
                            * candidate.input_price_per_million.unwrap_or(0.0)
                            / 1_000_000.0
                            + usage.output_tokens as f64
                                * candidate.output_price_per_million.unwrap_or(0.0)
                                / 1_000_000.0,
                    )
                } else {
                    None
                };
                state.observability.record_started(
                    ObservationEvent {
                usage_suppressed: false,
                        ledger_sequence: 0,
                        timestamp_ms: ObservationEvent::now_ms(),
                        request_id,
                        provider_id: Some(candidate.provider.id.clone()),
                        upstream_model: Some(bounded_metadata(&candidate.upstream_model)),
                        upstream_endpoint: None,
                        exposed_model: bounded_metadata(target),
                        route_id: candidate.route_id.clone(),
                        account_id: candidate.account_id.clone(),
                        status_code: status.as_u16(),
                        outcome: if status.is_success() && failure.is_none() {
                            "success".into()
                        } else {
                            "failure".into()
                        },
                        failure_category: failure.map(str::to_string),
                        upstream_error: None,
                        request_diagnostic: None,
            cache_observation: None,
                        client_thread_id: None,
                        latency_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                        attempts: 1,
                        candidate_ordinal: 1,
                        send_count: 1_u16.saturating_add(
                            provider_retry
                                .as_ref()
                                .map_or(0, |retry| retry.additional_sends),
                        ),
                        recovery_kinds: provider_retry
                            .as_ref()
                            .map(|retry| retry.recovery_kinds.clone())
                            .unwrap_or_default(),
                        recovery_kind: provider_retry
                            .as_ref()
                            .and_then(|retry| retry.recovery_kinds.first().cloned()),
                        attempt_only: false,
                        streaming: false,
                        usage,
                        estimated_cost_usd,
                        shadow: true,
                        shadow_rule_id: Some(rule.id.clone()),
                        parent_request_id: Some(parent_request_id.clone()),
                    },
                    observation_settings.clone(), privacy_origin,
);
            }
        }
    });
}

pub(crate) fn safe_endpoint_path(endpoint: &str) -> Option<String> {
    let parsed = url::Url::parse(endpoint).ok()?;
    let segments = parsed.path_segments()?.collect::<Vec<_>>();
    match segments.as_slice() {
        [.., "images", "generations"] => Some("/images/generations".into()),
        [.., "images", "edits"] => Some("/images/edits".into()),
        _ => None,
    }
}

pub(crate) fn shadow_sampled(parent_request_id: &str, rule_id: &str, sample_percent: u8) -> bool {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in parent_request_id.bytes().chain(rule_id.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash % 100 < u64::from(sample_percent)
}

pub(crate) fn apply_provider_request_compatibility(
    body: &mut Value,
    candidate: &RouteCandidate,
    protocol: ProviderProtocol,
    strip_unsupported_images: bool,
) {
    // A remote compaction request is an upstream-owned envelope. Its complete
    // input history, tools, and terminal `compaction_trigger` must remain
    // intact here. In particular, the loop and terminal-continuation guards
    // below append synthetic messages before the ordinary Responses sanitizer
    // gets a chance to skip compaction. Branch before every compatibility
    // transform; compact send/trigger sanitizers own those hops. Normal
    // Responses turns expand local `codetas1:` / `ocx1:` envelopes in the
    // canonical sanitizer so a direct call cannot leak CODETAS transport.
    if crate::compaction::request_is_remote_compaction(body) {
        crate::debug::log("provider compatibility SKIPPED for remote compaction envelope");
        return;
    }
    if let Some(tool_name) = guard_repeated_function_tool_loop(body) {
        eprintln!(
            "CODETAS Gateway: blocked repeated successful function-tool loop for {}",
            bounded_metadata(&tool_name)
        );
    }
    let provider = &candidate.provider;
    let model = &candidate.upstream_model;
    if model_matches_any(model, &provider.terminal_continuation_guard_models) {
        inject_terminal_continuation_guard(body);
    }
    if protocol == ProviderProtocol::Responses
        && candidate.provider.transport == ProviderTransport::Standard
    {
        expand_local_compactions(body);
        crate::debug::log_always(&format!(
            "sanitize PRE provider={} model={} previous_response_id={} tools={} input_items={}",
            provider.id,
            model,
            body.get("previous_response_id")
                .and_then(Value::as_str)
                .is_some(),
            body.get("tools")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0),
            body.get("input")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0)
        ));
        sanitize_responses_upstream_request(body, provider, model);
        if provider.requires_adjacent_responses_tool_results {
            normalize_responses_tool_result_adjacency(body);
        }
        crate::debug::log_always(&format!(
            "sanitize POST provider={} model={} previous_response_id={} tools={} input_items={}",
            provider.id,
            model,
            body.get("previous_response_id")
                .and_then(Value::as_str)
                .is_some(),
            body.get("tools")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0),
            body.get("input")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0)
        ));
    } else {
        crate::debug::log_always(&format!(
            "translate PRE provider={} model={} protocol={:?} previous_response_id={} tools={} input_items={}",
            provider.id,
            model,
            protocol,
            body.get("previous_response_id").and_then(Value::as_str).is_some(),
            body.get("tools").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0),
            body.get("input").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0)
        ));
        let supports_response_tool_kinds = candidate.provider.transport != ProviderTransport::Kiro;
        let preserve_anthropic = native_anthropic_route(candidate);
        crate::translate::prepare_translated_responses_request_with_native_anthropic(
            body, supports_response_tool_kinds, supports_response_tool_kinds, preserve_anthropic,
        );
        if protocol != ProviderProtocol::ChatCompletions {
            normalize_responses_tool_result_adjacency(body);
        }
        crate::debug::log_always(&format!(
            "translate POST provider={} model={} protocol={:?} previous_response_id={} tools={} input_items={}",
            provider.id,
            model,
            protocol,
            body.get("previous_response_id").and_then(Value::as_str).is_some(),
            body.get("tools").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0),
            body.get("input").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0)
        ));
    }
    if protocol == ProviderProtocol::ChatCompletions {
        normalize_chat_reasoning_history(
            body,
            model_matches_any(model, &provider.preserve_reasoning_content_models),
            model_requires_reasoning_placeholder(provider, model),
        );
        if strip_unsupported_images && !candidate.capabilities.vision {
            strip_translated_input_images(body);
        }
    }
    let Some(object) = body.as_object_mut() else {
        return;
    };

    if model_matches_any(model, &provider.no_reasoning_models) {
        object.remove("reasoning");
    } else if let Some(effort) = object
        .get("reasoning")
        .and_then(Value::as_object)
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str)
        .map(str::to_string)
    {
        let mapped = map_value_ignore_case(&provider.model_reasoning_effort_map, model)
            .and_then(|mapping| mapping.get(&effort).cloned())
            .or_else(|| provider.reasoning_effort_map.get(&effort).cloned());
        if let Some(mapped) = mapped {
            if let Some(reasoning) = object.get_mut("reasoning").and_then(Value::as_object_mut) {
                reasoning.insert("effort".into(), Value::String(mapped));
            }
        }
    }
    if model_matches_any(model, &provider.no_temperature_models) {
        object.remove("temperature");
    }
    if model_matches_any(model, &provider.no_top_p_models) {
        object.remove("top_p");
    }
    if model_matches_any(model, &provider.no_penalty_models) {
        object.remove("presence_penalty");
        object.remove("frequency_penalty");
    }
    if let Some(parallel) = provider.parallel_tool_calls {
        object.insert("parallel_tool_calls".into(), Value::Bool(parallel));
    } else if protocol == ProviderProtocol::ChatCompletions
        && is_kimi_chat_endpoint(&provider.base_url)
        && !object
            .get("parallel_tool_calls")
            .is_some_and(Value::is_boolean)
    {
        // Match OpenCodex: Kimi defaults to parallel tool calls, but an
        // explicit caller value remains untouched.
        object.insert("parallel_tool_calls".into(), Value::Bool(true));
    }
    if protocol == ProviderProtocol::ChatCompletions && !provider.prompt_cache_key {
        object.remove("prompt_cache_key");
    }
}

pub(crate) fn inject_terminal_continuation_guard(body: &mut Value) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    let follows_tool_result = items.last().is_some_and(|item| {
        matches!(
            item.get("type").and_then(Value::as_str),
            Some(
                "function_call_output"
                    | "custom_tool_call_output"
                    | "tool_search_output"
                    | "local_shell_call_output"
            )
        )
    });
    if !follows_tool_result {
        return;
    }
    items.push(json!({
        "type": "message",
        "role": "developer",
        "content": [{
            "type": "input_text",
            "text": "Continue after the completed tool result. Either invoke the next required tool or provide a non-empty final answer; do not terminate silently."
        }]
    }));
}

#[cfg(test)]
mod remote_compaction_compatibility_tests {
    use super::*;
    use crate::config::{ProviderCapabilities, ProviderDefinition};
    use crate::read_recent_observability_events;

    fn candidate_with_terminal_guard() -> RouteCandidate {
        RouteCandidate {
            provider: ProviderDefinition {
                id: "openai".into(),
                name: "OpenAI".into(),
                transport: ProviderTransport::Standard,
                terminal_continuation_guard_models: vec!["*".into()],
                ..ProviderDefinition::default()
            },
            upstream_model: "gpt-5.6-sol".into(),
            exposed_model: "gpt-5.6-sol".into(),
            credential: None,
            account_id: None,
            target_key: "openai/gpt-5.6-sol".into(),
            route_id: None,
            failure_threshold: 3,
            quota_threshold_percent: 90,
            input_price_per_million: None,
            output_price_per_million: None,
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            capabilities: ProviderCapabilities::default(),
            routing_epoch: 0,
            routing_generation: 0,
            session_scope: None,
        }
    }

    fn repeated_wait_agent_history(remote_compaction: bool) -> Value {
        let mut input = vec![json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "Keep monitoring the delegated work"}]
        })];
        for index in 0..8 {
            let call_id = format!("call_wait_{index}");
            input.push(json!({
                "type": "function_call",
                "id": format!("fc_{index}"),
                "call_id": call_id,
                "name": "wait_agent",
                "arguments": "{\"timeout_ms\":60000}"
            }));
            input.push(json!({
                "type": "function_call_output",
                "id": format!("fco_{index}"),
                "call_id": call_id,
                "output": "No agent updates yet"
            }));
        }
        if remote_compaction {
            input.push(json!({
                "type": "compaction_trigger",
                "id": "compact_1"
            }));
        }
        json!({
            "model": "gpt-5.6-sol",
            "stream": true,
            "store": false,
            "tools": [
                {"type": "function", "name": "wait_agent", "parameters": {"type": "object"}},
                {"type": "function", "name": "exec_command", "parameters": {"type": "object"}}
            ],
            "tool_choice": {"type": "function", "name": "wait_agent"},
            "input": input
        })
    }

    #[test]
    fn remote_compaction_bypasses_every_history_and_tool_transform() {
        let candidate = candidate_with_terminal_guard();
        let mut body = repeated_wait_agent_history(true);
        let original = body.clone();

        apply_provider_request_compatibility(
            &mut body,
            &candidate,
            ProviderProtocol::Responses,
            true,
        );

        assert_eq!(body, original);
        assert_eq!(body["input"].as_array().map(Vec::len), Some(18));
        assert_eq!(body["input"][17]["type"], "compaction_trigger");
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
        assert!(!body["input"].as_array().unwrap().iter().any(|item| {
            item.get("role").and_then(Value::as_str) == Some("developer")
                || item
                    .pointer("/content/0/text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains("CODETAS stopped a repeated tool loop"))
        }));
    }

    #[test]
    fn ordinary_request_still_activates_repeated_tool_guard() {
        let candidate = candidate_with_terminal_guard();
        // wait/poll calls are exempt from the guard (a delegated process may
        // need unbounded waits), so drive it with a repeated shared-execution
        // call that is neither a wait nor a readonly inspection.
        let mut body = repeated_wait_agent_history(false);
        for item in body["input"].as_array_mut().unwrap() {
            if item.get("name").and_then(Value::as_str) == Some("wait_agent") {
                item["name"] = json!("exec_command");
                item["arguments"] = json!("{\"cmd\":\"cargo build\"}");
            }
        }
        body["tool_choice"] = json!({"type": "function", "name": "exec_command"});

        apply_provider_request_compatibility(
            &mut body,
            &candidate,
            ProviderProtocol::Responses,
            true,
        );

        // The guard must leave the shared execution surface available so the
        // model can recover by writing/finalizing instead of being forced into
        // a tool-less dead end.
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
        assert!(body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| { tool.get("name").and_then(Value::as_str) == Some("wait_agent") }));
        assert!(body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| { tool.get("name").and_then(Value::as_str) == Some("exec_command") }));
        assert!(body.get("tool_choice").is_none());
        assert!(body["input"].as_array().unwrap().iter().any(|item| {
            item.pointer("/content/0/text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("CODETAS detected a repeated shared execution loop"))
        }));
    }

    #[test]
    fn remote_trigger_bypasses_terminal_guard_even_when_tool_output_is_last() {
        let candidate = candidate_with_terminal_guard();
        let mut body = repeated_wait_agent_history(false);
        body["input"].as_array_mut().unwrap().insert(
            1,
            json!({"type": "compaction_trigger", "id": "compact_early"}),
        );
        let original = body.clone();

        apply_provider_request_compatibility(
            &mut body,
            &candidate,
            ProviderProtocol::Responses,
            true,
        );

        assert_eq!(body, original);
        assert_eq!(
            body["input"].as_array().unwrap().last().unwrap()["type"],
            "function_call_output"
        );
    }

    #[test]
    fn image_observation_keeps_wire_model_and_only_safe_endpoint_path() {
        let candidate = candidate_with_terminal_guard();
        let seed = ObservationSeed::for_candidate(
            ObservabilityLedger::new(None),
            ObservabilitySettings::default(),
            "request-image".into(),
            false,
            Instant::now(),
            1,
            &candidate,
        )
        .with_upstream_image_details(
            "gpt-image-2",
            "https://user:secret@api.openai.com/v1/secret-token/images/generations?api_key=secret#hidden",
        );

        assert_eq!(seed.upstream_model.as_deref(), Some("gpt-image-2"));
        assert_eq!(
            seed.upstream_endpoint.as_deref(),
            Some("/images/generations")
        );
        assert!(!seed.upstream_endpoint.unwrap().contains("secret"));
    }

    #[test]
    fn image_edit_observation_uses_only_allowlisted_resource_identifier() {
        assert_eq!(
            safe_endpoint_path(
                "https://api.example/v1/private-account/images/edits?signature=secret"
            )
            .as_deref(),
            Some("/images/edits")
        );
        assert_eq!(
            safe_endpoint_path("https://api.example/v1/private-account/custom/image"),
            None
        );
    }

    #[test]
    fn recovery_kind_keeps_first_and_accumulates_kinds() {
        let ledger = ObservabilityLedger::new(None);
        let settings = ObservabilitySettings::default();
        let mut observation = ObservationSeed::without_candidate(
            ledger,
            settings,
            "req-continuation".into(),
            "gpt-test",
            true,
            Instant::now(),
        )
        .with_recovery(Some("continuation-rebase:unknown_id"));
        observation.record_recovery("continuation-rebase:unknown_id");
        observation.record_recovery("empty-completion");
        assert_eq!(
            observation.recovery_kind.as_deref(),
            Some("continuation-rebase:unknown_id")
        );
        assert_eq!(
            observation.recovery_kinds,
            vec![
                "continuation-rebase:unknown_id".to_string(),
                "empty-completion".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn candidate_attempt_latency_is_distinct_from_end_to_end_request_latency() {
        let directory = std::env::temp_dir().join(format!(
            "codetas-observation-latency-{}-{}",
            std::process::id(),
            ObservationEvent::now_ms(),
        ));
        let ledger = ObservabilityLedger::new(Some(directory.clone()));
        let mut settings = ObservabilitySettings::default();
        settings.request_log = true;
        let candidate = candidate_with_terminal_guard();
        let request_started = Instant::now() - Duration::from_millis(100);
        let candidate_started = Instant::now();
        ObservationSeed::for_candidate(
            ledger.clone(),
            settings.clone(),
            "request-latency".into(),
            false,
            candidate_started,
            1,
            &candidate,
        )
        .as_attempt()
        .finish(
            StatusCode::BAD_GATEWAY,
            Some("provider_unreachable"),
            TokenUsage::default(),
        );
        ObservationSeed::for_candidate(
            ledger.clone(),
            settings,
            "request-latency".into(),
            false,
            request_started,
            2,
            &candidate,
        )
        .finish(StatusCode::OK, None, TokenUsage::default());
        ledger.flush().await;

        let events = read_recent_observability_events(&directory, 0, 10);
        let attempt = events
            .iter()
            .find(|event| event.attempt_only)
            .expect("attempt event");
        let terminal = events
            .iter()
            .find(|event| !event.attempt_only)
            .expect("terminal event");
        assert!(attempt.latency_ms < terminal.latency_ms);
        assert!(terminal.latency_ms >= 100);
        let _ = fs::remove_dir_all(directory);
    }
}

#[cfg(test)]
mod compact_cache_boundary_tests {
    use super::*;
    fn normal(ledger: &ObservabilityLedger, n: u64) {
        let mut seed = ObservationSeed::without_candidate(ledger.clone(), ObservabilitySettings::default(),
            format!("normal-{n}"), "test/model", false, Instant::now());
        seed.provider_id = Some("test".into()); seed.upstream_model = Some("model".into()); seed.send_count = 1;
        seed.request_diagnostic = Some(crate::observability::RequestDiagnostic {
            client_thread_id: Some("01a13ddb-8d21-7cb2-8f00-23a8a09af079".into()), ..Default::default()
        });
        seed.finish(StatusCode::OK, None, TokenUsage { input_tokens: 100_000, cache_read_reported: true, ..Default::default() });
    }
    #[tokio::test]
    async fn real_compaction_observation_resets_window_without_any_upstream_diagnostic() {
        let ledger = ObservabilityLedger::new(None);
        let mut headers = HeaderMap::new();
        headers.insert("session_id", HeaderValue::from_static("01a13ddb-8d21-7cb2-8f00-23a8a09af079"));
        for n in 1..=19 { normal(&ledger, n); }
        assert_eq!(ledger.cache_health_summary().sessions[0].rolling_requests, 19);
        ObservationSeed::without_candidate(ledger.clone(), ObservabilitySettings::default(),
            "compact-success".into(), "test/model", false, Instant::now())
            .with_compaction_session(&headers).finish(StatusCode::OK, None, TokenUsage::default());
        assert_eq!(ledger.cache_health_summary().sessions[0].rolling_requests, 0);
        normal(&ledger, 20);
        let row = &ledger.cache_health_summary().sessions[0];
        assert_eq!(row.rolling_requests, 1); assert!(!row.active_alert);
        for n in 21..=39 { normal(&ledger, n); }
        assert!(ledger.cache_health_summary().sessions[0].active_alert);
        ObservationSeed::without_candidate(ledger.clone(), ObservabilitySettings::default(),
            "compact-failure".into(), "test/model", false, Instant::now())
            .with_compaction_session(&headers).finish(StatusCode::BAD_GATEWAY, Some("provider_unreachable"), TokenUsage::default());
        assert!(ledger.cache_health_summary().sessions[0].active_alert);
    }
}

#[cfg(test)]
mod final_cache_observation_tests {
    use super::*;
    fn seed(ledger:&ObservabilityLedger,n:u64)->ObservationSeed {
        let mut seed=ObservationSeed::without_candidate(ledger.clone(),ObservabilitySettings::default(),format!("final-{n}"),"test/model",false,Instant::now());
        seed.provider_id=Some("test".into());seed.upstream_model=Some("model".into());seed.send_count=1;
        seed.request_diagnostic=Some(crate::observability::RequestDiagnostic{client_thread_id:Some("01a13ddb-8d21-7cb2-8f00-23a8a09af079".into()),..Default::default()});seed
    }
    #[tokio::test]
    async fn cache_health_retry_billing_cannot_fill_missing_final_usage() {
        let ledger=ObservabilityLedger::new(None);
        for n in 0..19 { seed(&ledger,n).finish(StatusCode::OK,None,TokenUsage{input_tokens:100_000,cache_read_reported:true,..Default::default()}); }
        let mut last=seed(&ledger,19);
        last.retry_usage=TokenUsage{input_tokens:100_000,cache_read_reported:true,..Default::default()};
        last.finish(StatusCode::OK,None,TokenUsage::default());
        let row=&ledger.cache_health_summary().sessions[0];
        assert_eq!(row.successful_requests,20);assert_eq!(row.unknown_cache_requests,1);
        assert_eq!(row.rolling_requests,19);assert!(!row.active_alert);
        assert_eq!(row.measured_requests,19);
        assert_eq!(row.rolling_hit_percent,Some(0.0));
    }
    #[tokio::test]
    async fn cache_health_final_sample_is_not_diluted_by_billable_retry() {
        let ledger=ObservabilityLedger::new(None);
        for n in 0..20 {
            let mut current=seed(&ledger,n);
            current.retry_usage=TokenUsage{input_tokens:100_000,cache_read_reported:true,..Default::default()};
            current.finish(StatusCode::OK,None,TokenUsage{input_tokens:100_000,cached_input_tokens:99_000,cache_read_reported:true,..Default::default()});
        }
        let row=&ledger.cache_health_summary().sessions[0];
        assert_eq!(row.measured_input_tokens,2_000_000);
        assert_eq!(row.rolling_hit_percent,Some(99.0));assert!(!row.active_alert);
    }
}

#[cfg(test)]
mod failover_billing_tests {
    use super::*;
    fn seed(ledger:&ObservabilityLedger,provider:&str)->ObservationSeed {
        let mut settings=ObservabilitySettings::default();settings.request_log=true;settings.usage_log=true;
        let mut seed=ObservationSeed::without_candidate(ledger.clone(),settings,"same-logical-request".into(),"route/model",false,Instant::now());seed.provider_id=Some(provider.into());seed.upstream_model=Some("model".into());seed.send_count=1;seed.request_diagnostic=Some(crate::observability::RequestDiagnostic{client_thread_id:Some("01a13ddb-8d21-7cb2-8f00-23a8a09af079".into()),..Default::default()});seed
    }
    #[tokio::test]
    async fn failover_charges_each_candidate_once_without_counting_two_requests_or_cache_samples() {
        let directory=tempfile::tempdir().unwrap();let ledger=ObservabilityLedger::new(Some(directory.path().into()));
        let response=Response::builder().status(502).header(header::CONTENT_TYPE,"application/json").body(reqwest::Body::from(json!({"error":{"message":"fixture failed"},"usage":{"prompt_tokens":100_000,"completion_tokens":1,"cache_read_input_tokens":0}}).to_string())).unwrap();
        let classified=upstream_responses_error_classified(reqwest::Response::from(response),None).await;
        let mut first=seed(&ledger,"a");first.retry_usage=TokenUsage{input_tokens:1000,total_tokens:1000,..Default::default()};first.record_upstream_error(&classified.response);first.as_attempt().finish(StatusCode::BAD_GATEWAY,Some("provider_http_error"),TokenUsage::default());
        seed(&ledger,"b").finish(StatusCode::OK,None,TokenUsage{input_tokens:100_000,output_tokens:1,cached_input_tokens:99_000,total_tokens:100_001,cache_read_reported:true,..Default::default()});
        ledger.flush().await;
        assert_eq!(ledger.summary().total_requests,1,"logical request count after writer flush");assert_eq!(ledger.summary().successful_requests,1);assert_eq!(ledger.summary().input_tokens,201_000);
        let row=&ledger.cache_health_summary().sessions[0];assert_eq!(row.provider_id,"b");assert_eq!(row.successful_requests,1);assert_eq!(row.input_tokens,100_000);
        let summary=crate::observability::read_observability_summary(directory.path());assert_eq!(summary.total_requests,1);assert_eq!(summary.input_tokens,201_000);
        let breakdown=crate::observability::read_observability_breakdown(directory.path(),0,500);let a=breakdown.providers.iter().find(|row|row.key=="a").unwrap();assert_eq!(a.requests,0);assert_eq!(a.input_tokens,101_000);
    }
}


#[cfg(test)]
mod review15_privacy_regressions {
    use super::*;

    #[tokio::test]
    async fn review15_failover_privacy_uses_request_origin_not_candidate_latency() {
        for persistent in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let ledger = ObservabilityLedger::new(persistent.then(|| directory.path().to_path_buf()));
            let settings = ObservabilitySettings { request_log: true, usage_log: true, ..Default::default() };
            // Match gateway startup: apply configured privacy before requests begin.
            ledger.configure_cache_monitoring(settings.usage_log).await;
            let logical_started = Instant::now() - Duration::from_secs(60);
            ledger.configure_cache_monitoring(false).await;
            ledger.configure_cache_monitoring(true).await;
            let candidate_started = Instant::now();
            let mut candidate = super::super::anthropic_compact::review13_resolved_auth_regressions::candidate();
            candidate.input_price_per_million = Some(2.0);
            candidate.output_price_per_million = Some(3.0);
            let usage = TokenUsage::from_json(&json!({"usage":{"input_tokens":100000,"output_tokens":9,
                "input_tokens_details":{"cached_tokens":80000}}}));
            for (ordinal, attempt_only) in [(2, true), (3, false)] {
                let mut seed = ObservationSeed::for_candidate(ledger.clone(), settings.clone(),
                    "logical-request".into(), false,
                    if attempt_only { candidate_started } else { logical_started }, ordinal, &candidate)
                    .with_privacy_origin(logical_started);
                seed.request_diagnostic = Some(crate::observability::RequestDiagnostic {
                    client_thread_id: Some("01a13ddb-8d21-7cb2-8f00-23a8a09af079".into()), ..Default::default()
                });
                seed.record_provider_retries(&ProviderRetryObservation {
                    usage: usage.clone(), known_billing: Some(usage.clone()),
                    cache_observation: Some(crate::observability::CacheObservation::from_usage(&usage)),
                    ..Default::default()
                });
                if attempt_only { seed.as_attempt().finish(StatusCode::BAD_GATEWAY, Some("provider_http_error"), usage.clone()); }
                else { seed.finish(StatusCode::OK, None, usage.clone()); }
            }
            ledger.flush().await;
            assert_eq!(ledger.summary().total_requests, 1);
            assert_eq!(ledger.summary().total_tokens, 0);
            assert_eq!(ledger.summary().estimated_cost_usd, 0.0);
            assert!(ledger.cache_health_summary().sessions.is_empty());
            if persistent {
                let events = crate::observability::read_recent_observability_events(directory.path(), 0, 10);
                assert_eq!(events.len(), 2);
                let latency_ceiling = candidate_started.elapsed().as_millis() as u64;
                for event in &events {
                    assert!(event.usage_suppressed);
                    assert_eq!(event.usage.total_tokens, 0);
                    assert!(!event.usage.cache_read_reported);
                    assert!(event.cache_observation.is_none());
                    assert!(event.estimated_cost_usd.is_none());
                    if event.attempt_only {
                        assert_eq!(event.candidate_ordinal, 2);
                        assert!(event.latency_ms <= latency_ceiling);
                    } else {
                        assert_eq!(event.candidate_ordinal, 3);
                        assert!(event.latency_ms >= 60000);
                    }
                }
                // Force raw event replay, rather than just trusting saved totals.
                std::fs::remove_file(directory.path().join("usage-totals.json")).unwrap();
                std::fs::remove_file(directory.path().join("cache-health.json")).unwrap();
                let replay = ObservabilityLedger::new(Some(directory.path().to_path_buf()));
                assert_eq!(replay.summary().total_requests, 1);
                assert_eq!(replay.summary().total_tokens, 0);
                assert_eq!(replay.summary().estimated_cost_usd, 0.0);
                assert!(replay.cache_health_summary().sessions.is_empty());
                replay.flush().await;
            }
            // A genuinely new logical request after re-enable remains measurable.
            ObservationSeed::for_candidate(ledger.clone(), settings, "fresh-request".into(), false,
                Instant::now(), 1, &candidate).finish(StatusCode::OK, None, usage);
            ledger.flush().await;
            assert_eq!(ledger.summary().input_tokens, 100000);
            assert!(ledger.summary().estimated_cost_usd > 0.0);
        }
    }
}

fn shadow_billing_usage(mut usage: TokenUsage, retry: Option<&ProviderRetryObservation>) -> TokenUsage {
    if let Some(retry) = retry {
        if let Some(known) = &retry.known_billing { usage.merge_max(known.clone()); }
        usage = sum_token_usage(retry.usage.clone(), usage);
    }
    usage
}
async fn shadow_response_result(response: reqwest::Response, candidate: &RouteCandidate, limit: u64)
    -> (StatusCode, Option<&'static str>, TokenUsage, Option<ProviderRetryObservation>) {
    let status = response.status();
    let retry = response.extensions().get::<ProviderRetryObservation>().cloned();
    if !status.is_success() {
        let usage = match super::stream::bounded_json_with_usage(response, limit).await {
            Ok(value) => TokenUsage::from_json(&value), Err((_, usage)) => usage,
        };
        return (status, Some("provider_http_error"), usage, retry);
    }
    match candidate_response_value(response, candidate, limit).await {
        Ok(value) => (status, response_terminal_failure_category(&value), TokenUsage::from_json(&value), retry),
        Err((_, usage)) => (StatusCode::BAD_GATEWAY, Some("invalid_provider_response"), usage, retry),
    }
}


#[cfg(test)]
mod review16_shadow_regressions {
    use super::*;
    use super::super::adapt::review16_regressions::{candidate,response,frame,metadata};
    #[tokio::test]
    async fn review16_shadow_http_and_kiro_failures_bill_final_and_retry_once() {
        for mode in ["http","kiro-limit","kiro-io","kiro-convert"] {
            let mut c=candidate(); let mut limit=4096;
            let (mut upstream,status)=if mode=="http" {
                (response(vec![Ok(Bytes::from(json!({"error":{"message":"fixture"},"usage":{"input_tokens":17,"output_tokens":3}}).to_string()))],StatusCode::BAD_REQUEST),StatusCode::BAD_REQUEST)
            } else {
                c.provider.transport=ProviderTransport::Kiro;
                let prefix=metadata();
                let chunks=if mode=="kiro-io" {vec![Ok(Bytes::from(prefix)),Err(std::io::Error::other("fixture"))]} else {
                    if mode=="kiro-limit" {limit=prefix.len() as u64;}
                    let mut bytes=prefix;bytes.extend(frame("toolUseEvent",json!({"toolUseId":"invalid","name":"unknown","input":"{" ,"stop":true})));vec![Ok(Bytes::from(bytes))]
                };
                let mut upstream=response(chunks,StatusCode::OK);
                let (_,context)=crate::kiro::responses_to_kiro(&json!({"input":"test"}),"test",None).unwrap();upstream.extensions_mut().insert(context);
                (upstream,StatusCode::BAD_GATEWAY)
            };
            upstream.extensions_mut().insert(ProviderRetryObservation {usage:TokenUsage{input_tokens:7,total_tokens:7,..Default::default()},known_billing:Some(TokenUsage{input_tokens:17,output_tokens:3,total_tokens:20,..Default::default()}),additional_sends:1,..Default::default()});
            let (actual,failure,usage,retry)=shadow_response_result(upstream,&c,limit).await;
            assert_eq!(actual,status,"{mode}");assert!(failure.is_some());
            assert_eq!(usage.input_tokens,17,"{mode}");assert_eq!(usage.output_tokens,3);
            let billed=shadow_billing_usage(usage,retry.as_ref());assert_eq!(billed.input_tokens,24);assert_eq!(billed.output_tokens,3);assert!(!billed.cache_read_reported);
            let ledger=ObservabilityLedger::new(None);
            let mut event:ObservationEvent=serde_json::from_value(json!({"timestampMs":ObservationEvent::now_ms(),"requestId":mode,"clientThreadId":"01a13ddb-8d21-7cb2-8f00-23a8a09af079","providerId":"fixture","upstreamModel":"test","exposedModel":"test","statusCode":actual.as_u16(),"outcome":"failure","latencyMs":1,"attempts":1,"sendCount":2,"streaming":false,"inputTokens":0,"outputTokens":0,"cachedInputTokens":0,"reasoningTokens":0,"totalTokens":0,"estimatedCostUsd":0.024,"shadow":true})).unwrap();
            event.usage=billed;ledger.record(event,ObservabilitySettings{usage_log:true,request_log:true,..Default::default()});
            assert_eq!(ledger.summary().input_tokens,24);assert!(ledger.cache_health_summary().sessions.is_empty());
        }
        assert!(!shadow_billing_usage(TokenUsage::default(),Some(&ProviderRetryObservation::default())).cache_read_reported);
    }
}
