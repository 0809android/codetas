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
    mut body: Value,
    trust_turn_metadata: bool,
) -> Response<Body> {
    let started = Instant::now();
    let request_id = Uuid::new_v4().to_string();
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let claims_subagent = is_subagent_request(&headers);
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let (candidates, observability_settings, effort_cap) = {
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
        (candidates, observability_settings, effort_cap)
    };
    if request_is_remote_compaction(&body) {
        if let Some(object) = body.as_object_mut() {
            object.insert("stream".into(), Value::Bool(false));
        }
        return compact_response(State(state), headers, Json(body)).await;
    }
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

    // Codex executes client-side tools (`exec`) locally, so a follow-up request carries
    // only the `custom_tool_call_output` delta without the paired call. The ChatGPT
    // backend 400s orphaned tool outputs, and rewriting them into user messages makes a
    // plan-mode model re-propose `update_plan` forever. Re-pair each orphaned output
    // with its stored `custom_tool_call` instead (mirrors opencodex, which expands the
    // prior response into the request so pairs stay intact).
    if let Some(items) = body.get_mut("input").and_then(serde_json::Value::as_array_mut) {
        let mut repairs: Vec<(usize, serde_json::Value)> = Vec::new();
        for (index, item) in items.iter().enumerate() {
            if item.get("type").and_then(serde_json::Value::as_str) != Some("custom_tool_call_output") {
                continue;
            }
            let call_id = item.get("call_id").and_then(serde_json::Value::as_str).unwrap_or("");
            let already_paired = items
                .iter()
                .any(|other| other.get("type").and_then(serde_json::Value::as_str) == Some("custom_tool_call")
                    && other.get("call_id").and_then(serde_json::Value::as_str) == Some(call_id));
            if already_paired {
                continue;
            }
            if let Some(call) = state.response_state.find_custom_tool_call(call_id) {
                crate::debug::log(&format!("REPAIRED call: {} (len={})", call_id, call.to_string().len()));
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
    // A delta-only request whose own continuation could not be expanded must not
    // be recorded: storing it would replay a truncated conversation (opencodex
    // applies the same guard). Requests without `previous_response_id` are always
    // eligible — they carry the full input themselves.
    let record_eligible = !had_previous_response || expanded_previous;
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
        let mut candidate_body = body.clone();
        if let Err(message) =
            apply_candidate_model_policy(&mut candidate_body, candidate, claims_subagent)
        {
            if has_next {
                last_failure = Some(error_response(
                    StatusCode::BAD_REQUEST,
                    "model_limit_exceeded",
                    &message,
                ));
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
            return error_response(StatusCode::BAD_REQUEST, "model_limit_exceeded", &message);
        }
        let upstream = match send_candidate(&state, &candidate_body, candidate, Some(&headers))
            .await
        {
            Ok(upstream) => upstream,
            Err(failure) => {
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
                    AttemptFailureKind::Request => false,
                };
                if can_retry {
                    last_failure = Some(failure.response);
                    continue;
                }
                let status = failure.response.status();
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
                    status,
                    Some(failure.kind.category()),
                    TokenUsage::default(),
                );
                return failure.response;
            }
        };
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
            let response =
                upstream_error(upstream, retry_after.as_ref().map(|value| &value.0)).await;
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
                last_failure = Some(response);
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
            .finish(status, Some("provider_http_error"), TokenUsage::default());
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
        let observation = ObservationSeed::for_candidate(
            state.observability.clone(),
            observability_settings.clone(),
            request_id.clone(),
            streaming,
            started,
            attempts,
            candidate,
        );
        let custom_tools = custom_tool_names(&candidate_body);
        return adapt_successful_response(
            upstream,
            candidate,
            observation,
            custom_tools,
            &state.response_state,
            &body,
            record_eligible,
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

pub(crate) fn apply_candidate_model_policy(
    body: &mut Value,
    candidate: &RouteCandidate,
    is_subagent: bool,
) -> Result<(), String> {
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
    if is_subagent && shrink_subagent_input(body, candidate, effective_output)? {
        eprintln!(
            "CODETAS Gateway: shrunk subagent input to fit {}/{} budget",
            candidate.provider.id, candidate.upstream_model
        );
    } else {
        enforce_candidate_input_budget(body, candidate, effective_output)?;
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

pub(crate) fn enforce_candidate_input_budget(
    body: &Value,
    candidate: &RouteCandidate,
    reserved_output_tokens: u64,
) -> Result<(), String> {
    let context_input = candidate
        .context_window
        .map(|context| context.saturating_sub(reserved_output_tokens));
    let limit = match (candidate.max_input_tokens, context_input) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(limit), None) | (None, Some(limit)) => Some(limit),
        (None, None) => None,
    };
    let Some(limit) = limit else { return Ok(()) };
    // Responses-lite requests carry their tool definitions as `additional_tools`
    // items inside `input` (Codex Desktop's responses-lite shape). Those tool
    // definitions are not conversation history and must not be counted against
    // the input token budget — they are constant overhead already accounted for
    // by the model's own tool budget. Exclude them before measuring.
    let input_bytes = body
        .get("input")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) != Some("additional_tools"))
                .collect::<Vec<_>>()
        })
        .map(|filtered| serde_json::to_vec(&filtered))
        .transpose()
        .map_err(|_| "request input cannot be encoded for limit enforcement".to_string())?
        .map(|bytes| bytes.len() as u64)
        .unwrap_or(0);
    // The budget is expressed in tokens, but the request only exposes the input
    // as serialized bytes. Comparing raw byte length against a token limit
    // rejects valid requests at roughly a quarter of the real context window, so
    // approximate the token count with a conservative bytes-per-token ratio.
    let estimated_input_tokens =
        input_bytes.saturating_add(APPROX_INPUT_BYTES_PER_TOKEN - 1) / APPROX_INPUT_BYTES_PER_TOKEN;
    if estimated_input_tokens > limit {
        return Err(format!(
            "request input exceeds the conservative {}-token budget for {}/{}",
            limit, candidate.provider.id, candidate.upstream_model
        ));
    }
    Ok(())
}

/// Compute the input token estimate for the given input items (same
/// approximation as `enforce_candidate_input_budget`).
pub(crate) fn estimate_input_items_tokens(items: &[Value]) -> u64 {
    let bytes = items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) != Some("additional_tools"))
        .collect::<Vec<_>>();
    let input_bytes = serde_json::to_vec(&bytes)
        .map(|v| v.len() as u64)
        .unwrap_or(0);
    input_bytes.saturating_add(APPROX_INPUT_BYTES_PER_TOKEN - 1) / APPROX_INPUT_BYTES_PER_TOKEN
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
