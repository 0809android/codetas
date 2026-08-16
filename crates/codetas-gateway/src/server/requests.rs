use super::*;

pub(crate) async fn responses(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    let admission = match authorize_request(&state.settings, &headers, "responses:write").await {
        Ok(admission) => admission,
        Err(response) => return response,
    };
    responses_inner(state, headers, body, admission.trusts_turn_metadata()).await
}

pub(crate) async fn responses_inner(
    state: GatewayState,
    headers: HeaderMap,
    body: Value,
    trust_turn_metadata: bool,
) -> Response<Body> {
    responses_inner_with_media(state, headers, body, trust_turn_metadata, true).await
}

pub(crate) async fn responses_inner_without_media(
    state: GatewayState,
    headers: HeaderMap,
    body: Value,
    trust_turn_metadata: bool,
) -> Response<Body> {
    responses_inner_with_media(state, headers, body, trust_turn_metadata, false).await
}

enum ResponsesDispatch<T> {
    RemoteCompaction,
    Routed(T),
}

fn dispatch_before_routing<T>(body: &Value, route: impl FnOnce() -> T) -> ResponsesDispatch<T> {
    if request_is_remote_compaction(body) {
        ResponsesDispatch::RemoteCompaction
    } else {
        ResponsesDispatch::Routed(route())
    }
}

async fn responses_inner_with_media(
    state: GatewayState,
    headers: HeaderMap,
    mut body: Value,
    trust_turn_metadata: bool,
    preprocess_media: bool,
) -> Response<Body> {
    let started = Instant::now();
    let request_id = Uuid::new_v4().to_string();
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let claims_subagent = is_subagent_request(&headers);
    let codex_client = is_codex_request(&headers);
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let routing = match dispatch_before_routing(&body, || async {
        let settings = state.settings.read().await;
        // Turn metadata is only a routing authority after the caller has
        // passed admission authentication. Without it, the header is advisory
        // and cannot select a different cost/model policy.
        let is_subagent = claims_subagent && trust_turn_metadata;
        let observability_settings = settings.observability.clone();
        let effort_cap = if is_subagent {
            settings
                .agents
                .subagent_effort_cap
                .clone()
                .or_else(|| settings.agents.effort_cap.clone())
        } else {
            settings.agents.effort_cap.clone()
        };
        let desktop_target = claude_desktop_target(&headers, &settings, &requested_model);
        let effective_model = desktop_target.as_deref().unwrap_or(&requested_model);
        let mut routing = state.routing.lock().await;
        let mut candidates =
            routing.candidates_for_request(&settings, effective_model, is_subagent);
        if desktop_target.is_some() {
            if let Ok(candidates) = candidates.as_mut() {
                for candidate in candidates {
                    candidate.exposed_model = requested_model.clone();
                }
            }
        }
        (candidates, observability_settings, effort_cap, is_subagent)
    }) {
        ResponsesDispatch::RemoteCompaction => {
            if let Some(items) = body.get("input").and_then(Value::as_array) {
                use std::collections::BTreeMap;
                let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
                for item in items {
                    let t = item.get("type").and_then(Value::as_str).unwrap_or("?");
                    *counts.entry(t).or_default() += 1;
                }
                let summary = counts
                    .iter()
                    .map(|(k, v)| format!("{k}:{v}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                crate::debug::log(&format!(
                    "COMPACTION request: input=[{}] total={}",
                    summary,
                    items.len()
                ));
            } else {
                crate::debug::log("COMPACTION request: NO input array");
            }
            return compact_response_from_responses(State(state), headers, Json(body)).await;
        }
        ResponsesDispatch::Routed(routing) => routing,
    };
    let (candidates, observability_settings, effort_cap, is_subagent) = routing.await;
    // Replay the locally cached continuation history for `previous_response_id`
    // before routing. The ChatGPT Codex backend rejects that field (see
    // `sanitize_responses_upstream_request`), so without this expansion the
    // upstream would only ever see the delta input the client appends each
    // turn — losing all earlier context and making a plan-mode model re-propose
    // `update_plan` forever. Compaction turns are excluded above.
    let had_previous_response = body
        .get("previous_response_id")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());
    let expanded_previous = state
        .response_state
        .expand_previous_response_input(&mut body);

    // Codex executes tools locally, so a follow-up request may carry only the tool-output
    // delta without its paired call. Re-pair every supported output kind from stored
    // continuation history instead of rewriting it into a user message or forwarding an
    // orphan that a provider may reject.
    if let Some(items) = body.get_mut("input").and_then(serde_json::Value::as_array_mut) {
        let mut repairs: Vec<(usize, serde_json::Value)> = Vec::new();
        for (index, item) in items.iter().enumerate() {
            let Some(call_type) = (match item.get("type").and_then(serde_json::Value::as_str) {
                Some("function_call_output") => Some("function_call"),
                Some("custom_tool_call_output") => Some("custom_tool_call"),
                Some("tool_search_output") => Some("tool_search_call"),
                Some("local_shell_call_output") => Some("local_shell_call"),
                _ => None,
            }) else {
                continue;
            };
            let call_id = item.get("call_id").and_then(serde_json::Value::as_str).unwrap_or("");
            let already_paired = items
                .iter()
                .any(|other| other.get("type").and_then(serde_json::Value::as_str) == Some(call_type)
                    && other.get("call_id").and_then(serde_json::Value::as_str) == Some(call_id));
            if already_paired {
                continue;
            }
            if let Some(call) = state.response_state.find_tool_call(call_id, call_type) {
                crate::debug::log(&format!(
                    "REPAIRED {call_type}: {call_id} (len={})",
                    call.to_string().len()
                ));
                repairs.push((index, call));
            }
        }
        if !repairs.is_empty() {
            for (index, call) in repairs.into_iter().rev() {
                items.insert(index, call);
            }
        }
    }
    let input_summary: String = body
        .get("input")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            use std::collections::BTreeMap;
            let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
            for item in items {
                let t = item.get("type").and_then(serde_json::Value::as_str).unwrap_or("?");
                *counts.entry(t).or_default() += 1;
            }
            counts
                .iter()
                .map(|(k, v)| format!("{k}:{v}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    crate::debug::log(&format!(
        "request model={} had_prev={} expanded={} tools={} input=[{}]",
        requested_model,
        had_previous_response,
        expanded_previous,
        body.get("tools").and_then(serde_json::Value::as_array).map(|a| a.len()).unwrap_or(0),
        input_summary
    ));
    if let Some(tools) = body.get("tools").and_then(serde_json::Value::as_array) {
        for tool in tools.iter().take(5) {
            let name = tool.get("name").and_then(serde_json::Value::as_str).unwrap_or("?");
            let desc = tool.get("description").and_then(serde_json::Value::as_str).unwrap_or("");
            if name == "exec" {
                crate::debug::log(&format!("  EXEC_DESC_BEGIN"));
                crate::debug::log(desc);
                crate::debug::log(&format!("  EXEC_DESC_END len={}", desc.len()));
            } else {
                let short: String = desc.chars().take(120).collect();
                crate::debug::log(&format!("  tool: {} | {}", name, short));
            }
        }
    }
    // If an old continuation cannot be recovered, record the successful turn as a new
    // local checkpoint. Its earlier context is necessarily incomplete, but rebasing here
    // prevents every subsequent turn from remaining permanently delta-only.
    let record_eligible = true;
    if let Some(cap) = effort_cap.as_deref() {
        cap_reasoning_effort(&mut body, cap);
    }
    let candidates = match candidates {
        Ok(candidates) => candidates,
        Err(message) => {
            ObservationSeed::without_candidate(
                state.observability.clone(),
                observability_settings,
                request_id,
                &requested_model,
                streaming,
                started,
            )
            .finish(
                StatusCode::BAD_REQUEST,
                Some("routing_rejected"),
                TokenUsage::default(),
            );
            return error_response(StatusCode::BAD_REQUEST, "invalid_request", &message);
        }
    };
    let mut last_failure = None;
    for (index, candidate) in candidates.iter().enumerate() {
        let attempts = (index + 1).min(usize::from(u16::MAX)) as u16;
        let has_next = index + 1 < candidates.len();
        let candidate_started = Instant::now();
        let mut candidate_body = body.clone();
        if let Err(error) =
            apply_candidate_model_policy(&mut candidate_body, candidate, is_subagent)
        {
            if has_next {
                last_failure = Some(candidate_policy_error_response(&error));
                continue;
            }
            ObservationSeed::for_candidate(
                state.observability.clone(),
                observability_settings.clone(),
                request_id.clone(),
                streaming,
                started,
                attempts,
                candidate,
            )
            .finish(
                StatusCode::BAD_REQUEST,
                Some("model_limit_exceeded"),
                TokenUsage::default(),
            );
            return candidate_policy_error_response(&error);
        }
        if preprocess_media {
            if let Err(response) = prepare_candidate_media_input(
                &state,
                &headers,
                &mut candidate_body,
                candidate,
            )
            .await
            {
                if has_next {
                    last_failure = Some(response);
                    continue;
                }
                return response;
            }
            if let Err(error) =
                apply_candidate_model_policy(&mut candidate_body, candidate, is_subagent)
            {
                if has_next {
                    last_failure = Some(candidate_policy_error_response(&error));
                    continue;
                }
                return candidate_policy_error_response(&error);
            }
        }
        let tool_map = response_tool_map(&candidate_body);
        let upstream = match send_candidate(&state, &mut candidate_body, candidate, Some(&headers))
            .await
        {
            Ok(upstream) => upstream,
            Err(failure) => {
                let provider_retry = failure
                    .response
                    .extensions()
                    .get::<ProviderRetryObservation>()
                    .cloned();
                if matches!(
                    failure.kind,
                    AttemptFailureKind::Credential | AttemptFailureKind::Retryable
                ) {
                    state.routing.lock().await.record_failure(candidate);
                }
                let can_retry = match failure.kind {
                    AttemptFailureKind::Credential => candidates[index + 1..].iter().any(|next| {
                        next.target_key == candidate.target_key && next.account_id.is_some()
                    }),
                    AttemptFailureKind::Retryable => has_next,
                    AttemptFailureKind::ContextWindow => has_next,
                    AttemptFailureKind::Request => false,
                };
                if can_retry {
                    let status = failure.response.status();
                    let mut observation = ObservationSeed::for_candidate(
                        state.observability.clone(),
                        observability_settings.clone(),
                        request_id.clone(),
                        streaming,
                        candidate_started,
                        attempts,
                        candidate,
                    );
                    if let Some(retry) = provider_retry.as_ref() {
                        observation.record_provider_retries(retry);
                    }
                    observation.as_attempt().finish(
                        status,
                        Some(failure.kind.category()),
                        TokenUsage::default(),
                    );
                    last_failure = Some(failure.response);
                    continue;
                }
                let status = failure.response.status();
                let mut observation = ObservationSeed::for_candidate(
                    state.observability.clone(),
                    observability_settings.clone(),
                    request_id.clone(),
                    streaming,
                    started,
                    attempts,
                    candidate,
                );
                if let Some(retry) = provider_retry.as_ref() {
                    observation.record_provider_retries(retry);
                }
                observation.finish(
                    status,
                    Some(failure.kind.category()),
                    TokenUsage::default(),
                );
                return failure.response;
            }
        };
        let provider_retry = upstream
            .extensions()
            .get::<ProviderRetryObservation>()
            .cloned();
        let shared_provider_retries = upstream
            .extensions()
            .get::<SharedProviderRetryObservation>()
            .cloned();
        if let Some(recovery_failure) = upstream
            .extensions()
            .get::<EmptyCompletionRecoveryFailure>()
            .cloned()
        {
            let status = upstream.status();
            state.routing.lock().await.record_failure(candidate);
            let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
            let bytes = read_bounded(upstream, 64 * 1024).await.unwrap_or_else(|_| {
                Bytes::from_static(b"{\"error\":{\"code\":\"empty_completion_retry_failed\",\"message\":\"The empty-completion retry failed.\"}}")
            });
            let mut observation = ObservationSeed::for_candidate(
                state.observability.clone(),
                observability_settings.clone(),
                request_id.clone(),
                streaming,
                started,
                attempts,
                candidate,
            );
            if let Some(retry) = provider_retry.as_ref() {
                let mut metadata = retry.clone();
                // The dedicated failure marker already carries the complete
                // aggregate usage in its body/EmptyCompletionRecoveryFailure.
                metadata.usage = TokenUsage::default();
                observation.record_provider_retries(&metadata);
            } else {
                observation.recovery_kind = Some("empty-completion".into());
                observation.recovery_kinds.push("empty-completion".into());
                observation.send_count = observation
                    .send_count
                    .saturating_add(recovery_failure.additional_sends);
            }
            observation.finish(
                status,
                Some(EMPTY_COMPLETION_RETRY_FAILED_CODE),
                recovery_failure.usage,
            );
            let mut response = Response::builder().status(status);
            if let (Some(headers), Some(content_type)) = (response.headers_mut(), content_type) {
                headers.insert(header::CONTENT_TYPE, content_type);
            }
            return response
                .body(Body::from(bytes))
                .unwrap_or_else(|_| error_response(
                    StatusCode::BAD_GATEWAY,
                    EMPTY_COMPLETION_RETRY_FAILED_CODE,
                    "The empty-completion retry failed.",
                ));
        }
        if !upstream.status().is_success() {
            let status = upstream.status();
            let account_retry = matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
                && candidates[index + 1..].iter().any(|next| {
                    next.target_key == candidate.target_key && next.account_id.is_some()
                });
            let transient = status == StatusCode::REQUEST_TIMEOUT
                || status == StatusCode::TOO_MANY_REQUESTS
                || status.is_server_error();
            let retry_after = validated_retry_after(upstream.headers());
            let classified = upstream_responses_error_classified(
                upstream,
                retry_after.as_ref().map(|value| &value.0),
            )
            .await;
            let response = classified.response;
            let provider_error_usage = classified.usage;
            if status == StatusCode::TOO_MANY_REQUESTS {
                state
                    .routing
                    .lock()
                    .await
                    .record_quota_exhausted(candidate, retry_after.and_then(|value| value.1));
            } else if account_retry || transient {
                state.routing.lock().await.record_failure(candidate);
            }
            if has_next && (account_retry || transient) {
                let mut observation = ObservationSeed::for_candidate(
                    state.observability.clone(),
                    observability_settings.clone(),
                    request_id.clone(),
                    streaming,
                    candidate_started,
                    attempts,
                    candidate,
                );
                if let Some(retry) = provider_retry.as_ref() {
                    observation.record_provider_retries(retry);
                }
                observation.as_attempt().finish(
                    status,
                    Some("provider_http_error"),
                    provider_error_usage,
                );
                last_failure = Some(response);
                continue;
            }
            let mut observation = ObservationSeed::for_candidate(
                state.observability.clone(),
                observability_settings.clone(),
                request_id.clone(),
                streaming,
                started,
                attempts,
                candidate,
            );
            if let Some(retry) = provider_retry.as_ref() {
                observation.record_provider_retries(retry);
            }
            observation.finish(
                status,
                Some("provider_http_error"),
                provider_error_usage,
            );
            return response;
        }

        let quota = if candidate.quota_threshold_percent == 0 {
            None
        } else {
            quota_usage_percent(upstream.headers())
        };
        state.routing.lock().await.record_success(candidate, quota);
        schedule_shadow_calls(
            state.clone(),
            body.clone(),
            request_id.clone(),
            requested_model.clone(),
            candidate.clone(),
        );
        let recovered_empty_completion = upstream
            .extensions()
            .get::<EmptyCompletionRecoverySuccess>()
            .copied();
        let mut observation = ObservationSeed::for_candidate(
            state.observability.clone(),
            observability_settings.clone(),
            request_id.clone(),
            streaming,
            started,
            attempts,
            candidate,
        );
        if let Some(retry) = provider_retry.as_ref() {
            observation.record_provider_retries(retry);
        }
        if let Some(retry) = shared_provider_retries {
            observation.record_shared_provider_retries(retry);
        }
        if let Some(recovery) = recovered_empty_completion {
            if provider_retry.is_none() {
                observation.recovery_kind = Some("empty-completion".into());
                observation.recovery_kinds.push("empty-completion".into());
                observation.send_count = observation
                    .send_count
                    .saturating_add(recovery.additional_sends);
            }
        }
        return adapt_successful_response(
            upstream,
            candidate,
            observation,
            tool_map,
            &state.response_state,
            &body,
            record_eligible,
            codex_client,
        )
        .await;
    }
    let response = last_failure.unwrap_or_else(|| {
        error_response(
            StatusCode::BAD_GATEWAY,
            "provider_unavailable",
            "no provider route completed the request",
        )
    });
    ObservationSeed::without_candidate(
        state.observability.clone(),
        observability_settings,
        request_id,
        &requested_model,
        streaming,
        started,
    )
    .finish(
        response.status(),
        Some("provider_unavailable"),
        TokenUsage::default(),
    );
    response
}

pub(crate) fn claude_desktop_target(
    headers: &HeaderMap,
    settings: &GatewaySettings,
    requested_model: &str,
) -> Option<String> {
    let client = headers
        .get("x-codetas-client")
        .and_then(|value| value.to_str().ok())?;
    if client != "claude-desktop" || !settings.integrations.claude_desktop {
        return None;
    }
    settings
        .integrations
        .claude_desktop_aliases
        .get(requested_model)
        .cloned()
}

pub(crate) fn is_subagent_request(headers: &HeaderMap) -> bool {
    let Some(raw) = headers
        .get("x-codex-turn-metadata")
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 8 * 1024)
    else {
        return false;
    };
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|metadata| {
            metadata
                .get("thread_source")
                .or_else(|| metadata.pointer("/turn/thread_source"))
                .and_then(Value::as_str)
                .map(|source| source == "subagent")
        })
        .unwrap_or(false)
}

pub(crate) fn is_codex_request(headers: &HeaderMap) -> bool {
    [
        "x-codex-turn-metadata",
        "x-codex-turn-state",
        "x-codex-installation-id",
    ]
    .into_iter()
    .any(|name| headers.contains_key(name))
}

pub(crate) fn cap_reasoning_effort(body: &mut Value, cap: &str) {
    let requested = body
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(requested) = requested else { return };
    let Some(cap_rank) = reasoning_rank(cap) else {
        return;
    };
    if reasoning_rank(&requested).is_some_and(|rank| rank > cap_rank) {
        if let Some(reasoning) = body.get_mut("reasoning").and_then(Value::as_object_mut) {
            reasoning.insert("effort".into(), Value::String(cap.to_string()));
        }
    }
}

pub(crate) fn reasoning_rank(value: &str) -> Option<u8> {
    match value {
        "none" => Some(0),
        "minimal" => Some(1),
        "low" => Some(2),
        "medium" => Some(3),
        "high" => Some(4),
        "xhigh" => Some(5),
        "max" => Some(6),
        "ultra" => Some(7),
        _ => None,
    }
}

#[derive(Debug)]
pub(crate) enum CandidatePolicyError {
    InputBudget(String),
    Invalid(String),
}

impl std::fmt::Display for CandidatePolicyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InputBudget(message) | Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

fn candidate_policy_error_response(error: &CandidatePolicyError) -> Response<Body> {
    match error {
        CandidatePolicyError::InputBudget(message) => {
            context_window_exceeded_response(message)
        }
        CandidatePolicyError::Invalid(message) => {
            error_response(StatusCode::BAD_REQUEST, "model_limit_exceeded", message)
        }
    }
}

pub(crate) fn apply_candidate_model_policy(
    body: &mut Value,
    candidate: &RouteCandidate,
    is_subagent: bool,
) -> Result<(), CandidatePolicyError> {
    let requested_output = body.get("max_output_tokens").and_then(Value::as_u64);
    let effective_output = match (requested_output, candidate.max_output_tokens) {
        (Some(requested), Some(limit)) => {
            let value = requested.min(limit);
            body["max_output_tokens"] = json!(value);
            value
        }
        (None, Some(limit)) => {
            body["max_output_tokens"] = json!(limit);
            limit
        }
        (Some(requested), None) => requested,
        (None, None) => 0,
    };
    // Subagent turns inherit the parent conversation history through the
    // collaboration transport. When that history exceeds the subagent model's
    // context budget, shrink the input instead of failing the turn — the
    // parent's own budget stays untouched.
    if is_subagent {
        if shrink_subagent_input(body, candidate, effective_output)
            .map_err(CandidatePolicyError::Invalid)?
        {
            eprintln!(
                "CODETAS Gateway: shrunk subagent input to fit {}/{} budget",
                candidate.provider.id, candidate.upstream_model
            );
        }
    } else if !request_is_remote_compaction(body)
        || candidate_compaction_mode(candidate, CompactionRequestKind::NativeTrigger)
            == CompactionMode::Local
    {
        enforce_pathological_input_budget(body, candidate, effective_output)?;
    }

    let requested_effort = body
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mapped = if candidate.reasoning_efforts.is_empty() {
        candidate.default_reasoning_effort.clone()
    } else if let Some(requested) = requested_effort.as_deref() {
        if candidate
            .reasoning_efforts
            .iter()
            .any(|effort| effort == requested)
        {
            Some(requested.to_string())
        } else {
            let requested_rank = reasoning_rank(requested).unwrap_or(u8::MAX);
            candidate
                .reasoning_efforts
                .iter()
                .filter_map(|effort| reasoning_rank(effort).map(|rank| (rank, effort)))
                .filter(|(rank, _)| *rank <= requested_rank)
                .max_by_key(|(rank, _)| *rank)
                .or_else(|| {
                    candidate
                        .reasoning_efforts
                        .iter()
                        .filter_map(|effort| reasoning_rank(effort).map(|rank| (rank, effort)))
                        .min_by_key(|(rank, _)| *rank)
                })
                .map(|(_, effort)| effort.clone())
        }
    } else {
        candidate
            .default_reasoning_effort
            .clone()
            .or_else(|| candidate.reasoning_efforts.first().cloned())
    };
    if let Some(mapped) = mapped {
        if !body.get("reasoning").is_some_and(Value::is_object) {
            body["reasoning"] = json!({});
        }
        body["reasoning"]["effort"] = Value::String(mapped);
    }
    Ok(())
}

pub(crate) fn enforce_pathological_input_budget(
    body: &Value,
    candidate: &RouteCandidate,
    reserved_output_tokens: u64,
) -> Result<(), CandidatePolicyError> {
    let context_input = candidate
        .context_window
        .map(|context| context.saturating_sub(reserved_output_tokens));
    let limit = match (candidate.max_input_tokens, context_input) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(limit), None) | (None, Some(limit)) => Some(limit),
        (None, None) => None,
    };
    let Some(limit) = limit else { return Ok(()) };
    // Ordinary context management belongs to Codex and the provider. This
    // fail-open guard only rejects inputs that are implausibly larger than the
    // usable window; local compaction requests bypass this path entirely. The
    // 2.5x tolerance also stays above the estimator's observed serialization
    // overhead, so an approximation mismatch cannot preempt normal compaction.
    const ADMISSION_TOLERANCE_NUMERATOR: u64 = 5;
    const ADMISSION_TOLERANCE_DENOMINATOR: u64 = 2;
    let admission_limit = limit
        .saturating_mul(ADMISSION_TOLERANCE_NUMERATOR)
        / ADMISSION_TOLERANCE_DENOMINATOR;
    let estimate = body
        .get("input")
        .and_then(Value::as_array)
        .map(|items| estimate_input_items(items))
        .unwrap_or_default();
    if estimate.total_tokens > admission_limit {
        let details = format!(
            "text_estimated_tokens={} image_count={} image_estimated_tokens={} total_estimated_tokens={} context_limit={} admission_limit={} provider={} model={}",
            estimate.text_tokens,
            estimate.image_count,
            estimate.image_tokens,
            estimate.total_tokens,
            limit,
            admission_limit,
            candidate.provider.id,
            candidate.upstream_model
        );
        eprintln!("CODETAS Gateway: input budget rejected {details}");
        crate::debug::log(&format!("input budget rejected {details}"));
        return Err(CandidatePolicyError::InputBudget(format!(
            "request input is pathologically larger than the {}-token context budget for {}/{} ({})",
            limit, candidate.provider.id, candidate.upstream_model, details
        )));
    }
    Ok(())
}

const BASE64_IMAGE_PLACEHOLDER: &str = "<base64-image-payload>";
const LOW_DETAIL_IMAGE_TOKENS: u64 = 1_024;
const DEFAULT_IMAGE_TOKENS: u64 = 8_192;
const ORIGINAL_DETAIL_IMAGE_TOKENS: u64 = 16_384;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct InputTokenEstimate {
    pub(crate) text_tokens: u64,
    pub(crate) image_count: u64,
    pub(crate) image_tokens: u64,
    pub(crate) total_tokens: u64,
}

#[derive(Default)]
struct InputEstimateAccumulator {
    text_bytes: u64,
    image_count: u64,
    image_tokens: u64,
}

impl InputEstimateAccumulator {
    fn add_text_bytes(&mut self, bytes: u64) {
        self.text_bytes = self.text_bytes.saturating_add(bytes);
    }

    fn add_image(&mut self, detail: Option<&str>) {
        self.image_count = self.image_count.saturating_add(1);
        self.image_tokens = self
            .image_tokens
            .saturating_add(image_token_cost(detail));
    }

    fn finish(self) -> InputTokenEstimate {
        let text_tokens = self
            .text_bytes
            .saturating_add(APPROX_INPUT_BYTES_PER_TOKEN - 1)
            / APPROX_INPUT_BYTES_PER_TOKEN;
        InputTokenEstimate {
            text_tokens,
            image_count: self.image_count,
            image_tokens: self.image_tokens,
            total_tokens: text_tokens.saturating_add(self.image_tokens),
        }
    }
}

/// Compute a modality-aware input estimate shared by admission enforcement and
/// subagent shrinking. Normal JSON, URL text, and image metadata retain the
/// conservative bytes-per-token estimate. Valid base64 image payloads are
/// replaced by a small marker and charged a separate, bounded image cost.
pub(crate) fn estimate_input_items(items: &[Value]) -> InputTokenEstimate {
    let mut estimate = InputEstimateAccumulator::default();
    estimate.add_text_bytes(2); // JSON array brackets.
    let mut first = true;
    for item in items.iter().filter(|item| {
        item.get("type").and_then(Value::as_str) != Some("additional_tools")
    }) {
        if !first {
            estimate.add_text_bytes(1); // Comma between array items.
        }
        first = false;
        accumulate_json_value(item, &mut estimate);
    }
    estimate.finish()
}

/// Compute the total input token estimate for the given input items.
pub(crate) fn estimate_input_items_tokens(items: &[Value]) -> u64 {
    estimate_input_items(items).total_tokens
}

fn accumulate_json_value(value: &Value, estimate: &mut InputEstimateAccumulator) {
    match value {
        Value::Null => estimate.add_text_bytes(4),
        Value::Bool(true) => estimate.add_text_bytes(4),
        Value::Bool(false) => estimate.add_text_bytes(5),
        Value::Number(number) => estimate.add_text_bytes(number.to_string().len() as u64),
        Value::String(text) => estimate.add_text_bytes(serialized_json_string_bytes(text)),
        Value::Array(values) => {
            estimate.add_text_bytes(2);
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    estimate.add_text_bytes(1);
                }
                accumulate_json_value(value, estimate);
            }
        }
        Value::Object(object) => accumulate_json_object(object, estimate, false, None),
    }
}

fn accumulate_json_object(
    object: &serde_json::Map<String, Value>,
    estimate: &mut InputEstimateAccumulator,
    image_url_object: bool,
    inherited_detail: Option<&str>,
) {
    estimate.add_text_bytes(2);
    let image_content = matches!(
        object.get("type").and_then(Value::as_str),
        Some("input_image" | "image_url")
    );
    let detail = object
        .get("detail")
        .and_then(Value::as_str)
        .or(inherited_detail);
    for (index, (key, value)) in object.iter().enumerate() {
        if index > 0 {
            estimate.add_text_bytes(1);
        }
        estimate.add_text_bytes(serialized_json_string_bytes(key).saturating_add(1));
        match (key.as_str(), value) {
            ("image_url", Value::String(url)) if image_content => {
                accumulate_image_reference(url, detail, estimate);
            }
            ("image_url", Value::Object(nested)) if image_content => {
                accumulate_json_object(nested, estimate, true, detail);
            }
            ("url", Value::String(url)) if image_url_object => {
                accumulate_image_reference(url, detail, estimate);
            }
            _ => accumulate_json_value(value, estimate),
        }
    }
}

fn accumulate_image_reference(
    url: &str,
    detail: Option<&str>,
    estimate: &mut InputEstimateAccumulator,
) {
    estimate.add_image(detail);
    if let Some(prefix) = valid_base64_image_data_url_prefix(url) {
        let normalized_bytes = 2_u64
            .saturating_add(serialized_json_string_content_bytes(prefix))
            .saturating_add(serialized_json_string_content_bytes(
                BASE64_IMAGE_PLACEHOLDER,
            ));
        estimate.add_text_bytes(normalized_bytes);
    } else {
        // HTTP(S) image URLs and malformed data URLs remain ordinary text. The
        // latter is intentionally conservative and cannot hide arbitrary input
        // behind a data URL prefix.
        estimate.add_text_bytes(serialized_json_string_bytes(url));
    }
}

fn image_token_cost(detail: Option<&str>) -> u64 {
    match detail {
        Some(value) if value.eq_ignore_ascii_case("low") => LOW_DETAIL_IMAGE_TOKENS,
        Some(value) if value.eq_ignore_ascii_case("original") => {
            ORIGINAL_DETAIL_IMAGE_TOKENS
        }
        _ => DEFAULT_IMAGE_TOKENS,
    }
}

fn valid_base64_image_data_url_prefix(url: &str) -> Option<&str> {
    let comma = url.find(',')?;
    let (header, payload_with_comma) = url.split_at(comma);
    let payload = payload_with_comma.strip_prefix(',')?;
    let media_and_parameters = header.get(5..)?;
    if !header.get(..5)?.eq_ignore_ascii_case("data:") {
        return None;
    }
    let mut segments = media_and_parameters.split(';');
    let media_type = segments.next()?;
    if !media_type
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
        || !segments.any(|segment| segment.eq_ignore_ascii_case("base64"))
        || !is_valid_base64_payload(payload)
    {
        return None;
    }
    url.get(..=comma)
}

fn is_valid_base64_payload(payload: &str) -> bool {
    if payload.is_empty() || !payload.is_ascii() {
        return false;
    }
    let bytes = payload.as_bytes();
    let padding_start = bytes.iter().position(|byte| *byte == b'=').unwrap_or(bytes.len());
    let padding = bytes.len().saturating_sub(padding_start);
    if padding > 2
        || bytes[padding_start..].iter().any(|byte| *byte != b'=')
        || bytes[..padding_start].iter().any(|byte| {
            !byte.is_ascii_alphanumeric() && !matches!(*byte, b'+' | b'/' | b'-' | b'_')
        })
    {
        return false;
    }
    if padding > 0 {
        bytes.len() % 4 == 0
    } else {
        bytes.len() % 4 != 1
    }
}

fn serialized_json_string_bytes(value: &str) -> u64 {
    serialized_json_string_content_bytes(value).saturating_add(2)
}

fn serialized_json_string_content_bytes(value: &str) -> u64 {
    value.chars().fold(0_u64, |bytes, character| {
        let encoded = match character {
            '"' | '\\' | '\u{0008}' | '\u{000c}' | '\n' | '\r' | '\t' => 2,
            '\u{0000}'..='\u{001f}' => 6,
            _ => character.len_utf8() as u64,
        };
        bytes.saturating_add(encoded)
    })
}

/// Subagent turns inherit the parent conversation history through the
/// collaboration transport. When that history exceeds the subagent model's
/// context budget, drop redundant intermediate items (reasoning traces and
/// older custom tool outputs) to fit, instead of failing the turn. Returns
/// `true` when the input was modified, `false` when it already fits or cannot
/// be shrunk further.
pub(crate) fn shrink_subagent_input(
    body: &mut Value,
    candidate: &RouteCandidate,
    reserved_output_tokens: u64,
) -> Result<bool, String> {
    let context_input = candidate
        .context_window
        .map(|context| context.saturating_sub(reserved_output_tokens));
    let limit = match (candidate.max_input_tokens, context_input) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(limit), None) | (None, Some(limit)) => Some(limit),
        (None, None) => None,
    };
    let Some(limit) = limit else { return Ok(false) };
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return Ok(false);
    };
    if estimate_input_items_tokens(items) <= limit {
        return Ok(false);
    }
    // Drop in order of least value for a subagent continuation: reasoning
    // traces first, then older custom tool outputs. Keep messages, function
    // calls, and their outputs so the turn stays coherent.
    let mut changed = false;
    let mut removable = items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            matches!(
                item.get("type").and_then(Value::as_str),
                Some("reasoning" | "custom_tool_call_output")
            )
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    // Prefer dropping reasoning first: they are the largest and least
    // semantically essential for a continuation. Collect removable indices,
    // build a to-drop set in that order, then rebuild the array once with
    // `retain` so indices never go stale.
    removable.sort_by_key(|index| {
        items[*index].get("type").and_then(Value::as_str) != Some("reasoning")
    });
    let mut drop = std::collections::HashSet::new();
    while estimate_input_items_tokens(items) > limit {
        let Some(index) = removable.pop() else {
            break;
        };
        drop.insert(index);
        changed = true;
    }
    if changed {
        let retained = items
            .iter()
            .enumerate()
            .filter(|(index, _)| !drop.contains(index))
            .map(|(_, item)| item.clone())
            .collect::<Vec<_>>();
        *items = retained;
    }
    Ok(changed)
}

pub(crate) fn quota_usage_percent(headers: &HeaderMap) -> Option<u8> {
    [
        (
            "x-ratelimit-limit-requests",
            "x-ratelimit-remaining-requests",
        ),
        ("x-ratelimit-limit-tokens", "x-ratelimit-remaining-tokens"),
        ("ratelimit-limit", "ratelimit-remaining"),
    ]
    .into_iter()
    .filter_map(|(limit_name, remaining_name)| {
        let limit = headers
            .get(limit_name)?
            .to_str()
            .ok()?
            .parse::<f64>()
            .ok()?;
        let remaining = headers
            .get(remaining_name)?
            .to_str()
            .ok()?
            .parse::<f64>()
            .ok()?;
        if !limit.is_finite() || !remaining.is_finite() || limit <= 0.0 {
            return None;
        }
        let used = ((limit - remaining).max(0.0) / limit * 100.0).ceil();
        Some(used.clamp(0.0, 100.0) as u8)
    })
    .max()
}

#[cfg(test)]
mod input_budget_tests {
    use super::*;
    use crate::config::ProviderDefinition;
    use std::cell::Cell;

    fn candidate(context_window: u64) -> RouteCandidate {
        let mut provider = ProviderDefinition::default();
        provider.id = "openai".into();
        let capabilities = provider.capabilities.clone();
        RouteCandidate {
            provider,
            upstream_model: "gpt-5.6-sol".into(),
            exposed_model: "gpt-5.6-sol".into(),
            credential: None,
            account_id: None,
            target_key: "test".into(),
            route_id: None,
            failure_threshold: 0,
            quota_threshold_percent: 0,
            input_price_per_million: None,
            output_price_per_million: None,
            context_window: Some(context_window),
            max_input_tokens: None,
            max_output_tokens: None,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            capabilities,
        }
    }

    fn image_output(urls: impl IntoIterator<Item = (String, &'static str)>) -> Value {
        let output = urls
            .into_iter()
            .map(|(image_url, detail)| {
                json!({
                    "type": "input_image",
                    "image_url": image_url,
                    "detail": detail
                })
            })
            .collect::<Vec<_>>();
        json!({
            "type": "custom_tool_call_output",
            "call_id": "call_images",
            "output": output
        })
    }

    fn base64_image(payload_bytes: usize) -> String {
        format!("data:image/png;base64,{}", "A".repeat(payload_bytes))
    }

    #[test]
    fn remote_compaction_dispatch_does_not_resolve_stateful_candidates_first() {
        let resolver_calls = Cell::new(0_u8);
        let body = json!({
            "model": "gpt-5.6-sol",
            "input": [
                {"type": "message", "role": "user", "content": "preserve me"},
                {"type": "compaction_trigger"}
            ]
        });

        let dispatch = dispatch_before_routing(&body, || {
            resolver_calls.set(resolver_calls.get().saturating_add(1));
        });

        assert!(matches!(dispatch, ResponsesDispatch::RemoteCompaction));
        assert_eq!(resolver_calls.get(), 0);
    }

    #[test]
    fn ordinary_responses_dispatch_resolves_candidates_once() {
        let resolver_calls = Cell::new(0_u8);
        let body = json!({"model": "gpt-5.6-sol", "input": "hello"});

        let dispatch = dispatch_before_routing(&body, || {
            resolver_calls.set(resolver_calls.get().saturating_add(1));
        });

        assert!(matches!(dispatch, ResponsesDispatch::Routed(())));
        assert_eq!(resolver_calls.get(), 1);
    }

    #[test]
    fn rejects_only_pathologically_large_text_input() {
        let body = json!({
            "input": [{
                "type": "message",
                "role": "user",
                "content": "x".repeat(20_000)
            }]
        });
        let error = enforce_pathological_input_budget(&body, &candidate(1_000), 0)
            .expect_err("large text input should be rejected")
            .to_string();

        assert!(error.contains("text_estimated_tokens="));
        assert!(error.contains("image_count=0"));
        assert!(error.contains("image_estimated_tokens=0"));
        assert!(error.contains("total_estimated_tokens="));
        assert!(error.contains("context_limit=1000"));
        assert!(error.contains("admission_limit=2500"));
        assert!(error.contains("provider=openai"));
        assert!(error.contains("model=gpt-5.6-sol"));
    }

    #[tokio::test]
    async fn input_budget_rejection_uses_context_window_error_shape() {
        let body = json!({
            "input": [{
                "type": "message",
                "role": "user",
                "content": "x".repeat(20_000)
            }]
        });
        let error = enforce_pathological_input_budget(&body, &candidate(1_000), 0)
            .expect_err("large text input should be rejected");
        let response = candidate_policy_error_response(&error);

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(response.into_body(), 16 * 1024)
            .await
            .expect("error response should be readable");
        let payload: Value =
            serde_json::from_slice(&bytes).expect("error response should be valid JSON");
        assert_eq!(payload["error"]["type"], "invalid_request_error");
        assert_eq!(payload["error"]["code"], "context_length_exceeded");
        assert_eq!(payload["error"]["param"], "input");
        assert!(payload["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("exceeds the context window")));
    }

    #[test]
    fn openai_remote_compaction_routes_skip_token_admission() {
        let mut route = candidate(1_000);
        route.provider.credential.source = CredentialSource::Forward;
        route.provider.base_url = "https://chatgpt.com/backend-api/codex".into();
        let mut body = json!({
            "input": [
                {"type": "compaction_trigger"},
                {
                    "type": "message",
                    "role": "user",
                    "content": "x".repeat(20_000)
                }
            ]
        });

        assert!(apply_candidate_model_policy(&mut body, &route, false).is_ok());
    }

    #[test]
    fn openai_api_native_compaction_routes_skip_token_admission() {
        let mut route = candidate(1_000);
        route.provider.id = "openai-api".into();
        route.provider.credential.source = CredentialSource::Environment;
        route.provider.base_url = "https://api.openai.com/v1".into();
        let mut body = json!({
            "input": [
                {"type": "compaction_trigger"},
                {
                    "type": "message",
                    "role": "user",
                    "content": "x".repeat(20_000)
                }
            ]
        });

        assert_eq!(
            candidate_compaction_mode(&route, CompactionRequestKind::NativeTrigger),
            CompactionMode::Responses
        );
        assert!(apply_candidate_model_policy(&mut body, &route, false).is_ok());
    }

    #[test]
    fn local_compaction_routes_keep_only_the_pathological_guard() {
        let mut route = candidate(1_000);
        route.provider.id = "kiro".into();
        route.provider.transport = ProviderTransport::Kiro;
        let mut body = json!({
            "input": [{
                "type": "message",
                "role": "user",
                "content": "x".repeat(20_000)
            }]
        });

        assert!(apply_candidate_model_policy(&mut body, &route, false).is_err());
    }

    #[test]
    fn eight_megabyte_base64_image_is_not_counted_as_text() {
        let body = json!({
            "input": [
                {"type": "message", "role": "user", "content": "inspect this image"},
                image_output([(base64_image(8 * 1024 * 1024), "original")])
            ]
        });
        let estimate = estimate_input_items(body["input"].as_array().unwrap());

        assert_eq!(estimate.image_count, 1);
        assert_eq!(estimate.image_tokens, ORIGINAL_DETAIL_IMAGE_TOKENS);
        assert!(estimate.text_tokens < 1_000);
        assert!(enforce_pathological_input_budget(&body, &candidate(272_000), 0).is_ok());
    }

    #[test]
    fn four_base64_images_do_not_produce_a_multi_million_token_estimate() {
        let images = [
            6 * 1024 * 1024,
            4 * 1024 * 1024,
            2 * 1024 * 1024,
            1 * 1024 * 1024,
        ]
        .into_iter()
        .map(|size| (base64_image(size), "original"));
        let body = json!({
            "input": [
                {"type": "message", "role": "user", "content": "compare all four"},
                image_output(images)
            ]
        });
        let estimate = estimate_input_items(body["input"].as_array().unwrap());

        assert_eq!(estimate.image_count, 4);
        assert_eq!(estimate.image_tokens, 4 * ORIGINAL_DETAIL_IMAGE_TOKENS);
        assert!(estimate.total_tokens < 100_000);
        assert!(enforce_pathological_input_budget(&body, &candidate(272_000), 0).is_ok());
    }

    #[test]
    fn additional_tools_remain_excluded_from_the_estimate() {
        let message = json!({"type": "message", "role": "user", "content": "hello"});
        let without_tools = vec![message.clone()];
        let with_tools = vec![
            message,
            json!({
                "type": "additional_tools",
                "tools": [{
                    "type": "function",
                    "name": "huge_tool",
                    "description": "z".repeat(1_000_000)
                }]
            }),
        ];

        assert_eq!(
            estimate_input_items(&without_tools),
            estimate_input_items(&with_tools)
        );
    }

    #[test]
    fn malformed_data_url_is_counted_as_text_without_panicking() {
        let malformed = format!("data:image/png;base64,{}", "%".repeat(20_000));
        let body = json!({"input": [image_output([(malformed, "high")])]});
        let estimate = estimate_input_items(body["input"].as_array().unwrap());

        assert_eq!(estimate.image_count, 1);
        assert!(estimate.text_tokens > 1_000);
        assert!(enforce_pathological_input_budget(&body, &candidate(1_000), 0).is_err());
    }

    #[test]
    fn url_and_base64_image_forms_are_both_estimated() {
        let body = json!({
            "input": [{
                "type": "message",
                "role": "user",
                "content": [
                    {
                        "type": "image_url",
                        "image_url": {
                            "url": format!("https://example.com/image.png?{}", "q".repeat(400)),
                            "detail": "low"
                        }
                    },
                    {
                        "type": "input_image",
                        "image_url": base64_image(1024 * 1024),
                        "detail": "high"
                    }
                ]
            }]
        });
        let estimate = estimate_input_items(body["input"].as_array().unwrap());

        assert_eq!(estimate.image_count, 2);
        assert_eq!(
            estimate.image_tokens,
            LOW_DETAIL_IMAGE_TOKENS + DEFAULT_IMAGE_TOKENS
        );
        assert!(estimate.text_tokens > 100);
        assert!(estimate.text_tokens < 1_000);
    }

    #[test]
    fn base64_image_does_not_trigger_subagent_shrinking_by_payload_length() {
        let mut body = json!({
            "input": [
                {"type": "reasoning", "summary": "keep this small history"},
                image_output([(base64_image(1024 * 1024), "original")])
            ]
        });

        let changed = shrink_subagent_input(&mut body, &candidate(20_000), 0).unwrap();

        assert!(!changed);
        assert_eq!(body["input"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn text_byte_estimate_matches_compact_json_serialization() {
        let items = vec![json!({
            "type": "message",
            "content": "quotes: \" slash: \\ newline:\n 日本語"
        })];
        let expected_bytes = serde_json::to_vec(&items).unwrap().len() as u64;
        let expected_tokens = expected_bytes
            .saturating_add(APPROX_INPUT_BYTES_PER_TOKEN - 1)
            / APPROX_INPUT_BYTES_PER_TOKEN;

        assert_eq!(estimate_input_items(&items).text_tokens, expected_tokens);
    }
}
