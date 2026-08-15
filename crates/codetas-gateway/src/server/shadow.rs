use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttemptFailureKind {
    Request,
    Credential,
    Retryable,
}

impl AttemptFailureKind {
    pub(crate) fn category(self) -> &'static str {
        match self {
            Self::Request => "request_rejected",
            Self::Credential => "credential_unavailable",
            Self::Retryable => "provider_unreachable",
        }
    }
}

#[derive(Clone)]
pub(crate) struct ObservationSeed {
    pub(crate) ledger: ObservabilityLedger,
    pub(crate) settings: ObservabilitySettings,
    pub(crate) request_id: String,
    pub(crate) provider_id: Option<String>,
    pub(crate) upstream_model: Option<String>,
    pub(crate) exposed_model: String,
    pub(crate) route_id: Option<String>,
    pub(crate) account_id: Option<String>,
    pub(crate) streaming: bool,
    pub(crate) started: Instant,
    pub(crate) attempts: u16,
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
            provider_id: None,
            upstream_model: None,
            exposed_model: bounded_metadata(exposed_model),
            route_id: None,
            account_id: None,
            streaming,
            started,
            attempts: 0,
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
            provider_id: Some(candidate.provider.id.clone()),
            upstream_model: Some(bounded_metadata(&candidate.upstream_model)),
            exposed_model: bounded_metadata(&candidate.exposed_model),
            route_id: candidate.route_id.clone(),
            account_id: candidate.account_id.clone(),
            streaming,
            started,
            attempts,
            input_price_per_million: candidate.input_price_per_million,
            output_price_per_million: candidate.output_price_per_million,
        }
    }

    pub(crate) fn finish(
        self,
        status: StatusCode,
        failure_category: Option<&str>,
        usage: TokenUsage,
    ) {
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
        self.ledger.record(
            ObservationEvent {
                ledger_sequence: 0,
                timestamp_ms: ObservationEvent::now_ms(),
                request_id: self.request_id,
                provider_id: self.provider_id,
                upstream_model: self.upstream_model,
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
                latency_ms: self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                attempts: self.attempts,
                streaming: self.streaming,
                usage,
                estimated_cost_usd,
                shadow: false,
                shadow_rule_id: None,
                parent_request_id: None,
            },
            self.settings,
        );
    }
}

pub(crate) fn bounded_metadata(value: &str) -> String {
    value.chars().take(240).collect()
}

pub(crate) fn schedule_shadow_calls(
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
                        .candidates(&settings, target)
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
                let (status, failure, usage) =
                    match apply_candidate_model_policy(&mut shadow_body, &candidate, false) {
                        Err(_) => (
                            StatusCode::BAD_REQUEST,
                            Some("shadow_model_limit"),
                            TokenUsage::default(),
                        ),
                        Ok(()) => {
                            match send_candidate(&state, &shadow_body, &candidate, None).await {
                                Err(failure) => (
                                    failure.response.status(),
                                    Some(failure.kind.category()),
                                    TokenUsage::default(),
                                ),
                                Ok(response) if !response.status().is_success() => {
                                    let status = response.status();
                                    let _ = read_bounded(response, rule.max_response_bytes).await;
                                    (status, Some("provider_http_error"), TokenUsage::default())
                                }
                                Ok(response) => match candidate_response_value(
                                    response,
                                    &candidate,
                                    rule.max_response_bytes,
                                )
                                .await
                                {
                                    Ok(value) => {
                                        (StatusCode::OK, None, TokenUsage::from_json(&value))
                                    }
                                    Err(_) => (
                                        StatusCode::BAD_GATEWAY,
                                        Some("invalid_provider_response"),
                                        TokenUsage::default(),
                                    ),
                                },
                            }
                        }
                    };
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
                state.observability.record(
                    ObservationEvent {
                        ledger_sequence: 0,
                        timestamp_ms: ObservationEvent::now_ms(),
                        request_id,
                        provider_id: Some(candidate.provider.id.clone()),
                        upstream_model: Some(bounded_metadata(&candidate.upstream_model)),
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
                        latency_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                        attempts: 1,
                        streaming: false,
                        usage,
                        estimated_cost_usd,
                        shadow: true,
                        shadow_rule_id: Some(rule.id.clone()),
                        parent_request_id: Some(parent_request_id.clone()),
                    },
                    observation_settings.clone(),
                );
            }
        }
    });
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
) {
    if let Some(tool_name) = guard_repeated_function_tool_loop(body) {
        eprintln!(
            "CODETAS Gateway: blocked repeated successful function-tool loop for {}",
            bounded_metadata(&tool_name)
        );
    }
    let provider = &candidate.provider;
    let model = &candidate.upstream_model;
    let is_compaction = crate::compaction::request_is_remote_compaction(body);
    if protocol == ProviderProtocol::Responses
        && candidate.provider.transport == ProviderTransport::Standard
    {
        expand_local_compactions(body);
        if is_compaction {
            // Compaction requests carry the client's verbatim compaction envelope
            // (`compaction_trigger` + full history) that the ChatGPT backend expects
            // untouched. Sanitizing it — e.g. rewriting orphaned tool outputs into
            // user messages — changes the envelope and 400s upstream.
            crate::debug::log("sanitize SKIPPED for compaction request");
        } else {
            crate::debug::log(&format!(
                "sanitize PRE: tools={} input_items={}",
                body.get("tools").and_then(serde_json::Value::as_array).map(|a| a.len()).unwrap_or(0),
                body.get("input").and_then(serde_json::Value::as_array).map(|a| a.len()).unwrap_or(0)
            ));
            sanitize_responses_upstream_request(body, provider, model);
            crate::debug::log(&format!(
                "sanitize POST: tools={} input_items={}",
                body.get("tools").and_then(serde_json::Value::as_array).map(|a| a.len()).unwrap_or(0),
                body.get("input").and_then(serde_json::Value::as_array).map(|a| a.len()).unwrap_or(0)
            ));
        }
    } else {
        let supports_response_tool_kinds =
            candidate.provider.transport != ProviderTransport::Kiro;
        prepare_translated_responses_request(
            body,
            supports_response_tool_kinds,
            supports_response_tool_kinds,
        );
    }
    if protocol == ProviderProtocol::ChatCompletions {
        normalize_chat_reasoning_history(
            body,
            model_matches_any(model, &provider.preserve_reasoning_content_models),
        );
        if !model_supports_vision(provider, model) {
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
    }
    if protocol == ProviderProtocol::ChatCompletions && !provider.prompt_cache_key {
        object.remove("prompt_cache_key");
    }
}
