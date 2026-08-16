use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
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
}

#[derive(Clone)]
pub(crate) struct ObservationSeed {
    pub(crate) ledger: ObservabilityLedger,
    pub(crate) settings: ObservabilitySettings,
    pub(crate) request_id: String,
    pub(crate) provider_id: Option<String>,
    pub(crate) upstream_model: Option<String>,
    pub(crate) upstream_endpoint: Option<String>,
    pub(crate) exposed_model: String,
    pub(crate) route_id: Option<String>,
    pub(crate) account_id: Option<String>,
    pub(crate) streaming: bool,
    pub(crate) started: Instant,
    pub(crate) attempts: u16,
    pub(crate) candidate_ordinal: u16,
    pub(crate) send_count: u16,
    pub(crate) recovery_kinds: Vec<String>,
    pub(crate) recovery_kind: Option<String>,
    pub(crate) retry_usage: TokenUsage,
    pub(crate) shared_provider_retries: Option<SharedProviderRetryObservation>,
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
            upstream_endpoint: None,
            exposed_model: bounded_metadata(exposed_model),
            route_id: None,
            account_id: None,
            streaming,
            started,
            attempts: 0,
            candidate_ordinal: 0,
            send_count: 0,
            recovery_kinds: Vec::new(),
            recovery_kind: None,
            retry_usage: TokenUsage::default(),
            shared_provider_retries: None,
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
            upstream_endpoint: None,
            exposed_model: bounded_metadata(&candidate.exposed_model),
            route_id: candidate.route_id.clone(),
            account_id: candidate.account_id.clone(),
            streaming,
            started,
            attempts,
            candidate_ordinal: attempts,
            send_count: 1,
            recovery_kinds: Vec::new(),
            recovery_kind: None,
            retry_usage: TokenUsage::default(),
            shared_provider_retries: None,
            input_price_per_million: candidate.input_price_per_million,
            output_price_per_million: candidate.output_price_per_million,
        }
    }

    pub(crate) fn record_provider_retries(&mut self, retry: &ProviderRetryObservation) {
        self.send_count = self.send_count.saturating_add(retry.additional_sends);
        self.recovery_kinds.extend(retry.recovery_kinds.clone());
        if self.recovery_kind.is_none() {
            self.recovery_kind = retry.recovery_kinds.first().cloned();
        }
        self.retry_usage = sum_token_usage(self.retry_usage.clone(), retry.usage.clone());
    }

    pub(crate) fn record_shared_provider_retries(
        &mut self,
        retry: SharedProviderRetryObservation,
    ) {
        self.shared_provider_retries = Some(retry);
    }

    pub(crate) fn with_upstream_image_details(
        mut self,
        wire_model: &str,
        endpoint: &str,
    ) -> Self {
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
        if let Some(shared) = self.shared_provider_retries.take() {
            let retry = shared.0.lock().ok().map(|retry| retry.clone());
            if let Some(retry) = retry.as_ref() {
                self.record_provider_retries(retry);
            }
        }
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
        self.ledger.record(
            ObservationEvent {
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
                latency_ms: self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                attempts: self.attempts,
                candidate_ordinal: self.candidate_ordinal,
                send_count: self.send_count,
                recovery_kinds: self.recovery_kinds,
                recovery_kind: self.recovery_kind,
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
                            match send_candidate(&state, &mut shadow_body, &candidate, None).await {
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
                        latency_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                        attempts: 1,
                        candidate_ordinal: 1,
                        send_count: 1,
                        recovery_kinds: Vec::new(),
                        recovery_kind: None,
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
    // byte-semantic equivalents of what Codex sent. In particular, the loop
    // and terminal-continuation guards below append synthetic messages before
    // the ordinary Responses sanitizer gets a chance to skip compaction.
    // Branch before every compatibility transform; the compact handler is the
    // only place allowed to adjust required top-level transport fields.
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
        crate::debug::log(&format!(
            "sanitize PRE: tools={} input_items={}",
            body.get("tools").and_then(serde_json::Value::as_array).map(|a| a.len()).unwrap_or(0),
            body.get("input").and_then(serde_json::Value::as_array).map(|a| a.len()).unwrap_or(0)
        ));
        sanitize_responses_upstream_request(body, provider, model);
        if provider.requires_adjacent_responses_tool_results {
            normalize_responses_tool_result_adjacency(body);
        }
        crate::debug::log(&format!(
            "sanitize POST: tools={} input_items={}",
            body.get("tools").and_then(serde_json::Value::as_array).map(|a| a.len()).unwrap_or(0),
            body.get("input").and_then(serde_json::Value::as_array).map(|a| a.len()).unwrap_or(0)
        ));
    } else {
        let supports_response_tool_kinds =
            candidate.provider.transport != ProviderTransport::Kiro;
        prepare_translated_responses_request(
            body,
            supports_response_tool_kinds,
            supports_response_tool_kinds,
        );
        if protocol != ProviderProtocol::ChatCompletions {
            normalize_responses_tool_result_adjacency(body);
        }
    }
    if protocol == ProviderProtocol::ChatCompletions {
        normalize_chat_reasoning_history(
            body,
            model_matches_any(model, &provider.preserve_reasoning_content_models),
            model_requires_reasoning_placeholder(provider, model),
        );
        if strip_unsupported_images && !model_supports_vision(provider, model) {
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
        let mut body = repeated_wait_agent_history(false);

        apply_provider_request_compatibility(
            &mut body,
            &candidate,
            ProviderProtocol::Responses,
            true,
        );

        assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["tools"][0]["name"], "exec_command");
        assert!(body.get("tool_choice").is_none());
        assert!(body["input"].as_array().unwrap().iter().any(|item| {
            item.pointer("/content/0/text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("CODETAS stopped a repeated tool loop"))
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
}
