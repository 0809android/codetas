use super::*;
use crate::anthropic_compaction as native;

#[derive(Clone)]
pub(crate) struct NativeAnthropicContext {
    pub scope: String,
    pub context_hash: String,
    pub history_rewritten: bool,
}

pub(crate) fn anthropic_checkpoint_scope(candidate: &RouteCandidate, auth_binding: &str) -> String {
    let credential = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential);
    native::digest(
        &json!({"provider":candidate.provider.id,"baseUrl":candidate.provider.base_url.trim_end_matches('/'),
        "model":candidate.provider.wire_model_id(&candidate.upstream_model),"account":candidate.account_id,
        "authBinding":auth_binding,"queryParams":candidate.provider.query_params,"credentialSource":credential.source,"credentialReference":credential.reference,"credentialTransport":credential.transport}),
    )
}
pub(crate) fn preserve_native_anthropic_history(body: &Value, candidate: &RouteCandidate, resolved_scope: Option<&str>) -> bool {
    // Configured account labels do not authenticate a caller-supplied credential.
    // Until Forward has a verified account binding, use the visible summary only.
    if candidate.credential.as_ref().unwrap_or(&candidate.provider.credential).source
        == CredentialSource::Forward
    {
        return false;
    }
    if candidate.provider.transport != ProviderTransport::Standard
        || candidate
            .provider
            .protocol_for_model(&candidate.upstream_model)
            != ProviderProtocol::AnthropicMessages
    {
        return false;
    }
    let Some(scope) = resolved_scope else { return false; };
    body.get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|i| native::is_item(i))
        .all(|item| {
            native::decode(item)
                .ok()
                .flatten()
                .is_some_and(|checkpoint| checkpoint.scope == scope)
        })
}
pub(crate) fn prepare_native_anthropic_history(
    body: &mut Value,
    candidate: &RouteCandidate,
    resolved_scope: Option<&str>,
) -> Result<(), String> {
    crate::compaction::validate_local_compactions(body)?;
    if preserve_native_anthropic_history(body, candidate, resolved_scope) {
        return Ok(());
    }
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    if !items.iter().any(native::is_item) {
        return Ok(());
    }
    let mut output = Vec::new();
    for item in items.drain(..) {
        if let Some(checkpoint) = native::decode(&item)? {
            output.extend(native::fallback(&checkpoint));
        } else {
            output.push(item);
        }
    }
    *items = output;
    if native_anthropic_route(candidate) {
        let mut wire = responses_to_anthropic_with_oauth(body, &candidate.provider.wire_model_id(&candidate.upstream_model), uses_anthropic_subscription_oauth(&candidate.provider))?;
        apply_provider_wire_compatibility(&mut wire, body, candidate, ProviderProtocol::AnthropicMessages)?;
        let expected = ThinkingProvenance::for_wire(&wire, resolved_scope, true);
        if let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) {
            for item in items {
                if !item.pointer("/provider_metadata/anthropic").is_some_and(|metadata| expected.permits(metadata)) {
                    native::strip_signed_thinking(std::slice::from_mut(item));
                }
            }
        }
    } else if let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) {
        native::strip_signed_thinking(items);
    }
    Ok(())
}

fn flatten_native_history(body: &Value) -> Result<Vec<Value>, String> {
    let mut body = body.clone();
    native::expand_retained(&mut body)?;
    Ok(body
        .get("input")
        .and_then(Value::as_array)
        .ok_or("native compaction requires input history")?
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) != Some("compaction_trigger"))
        .cloned()
        .collect())
}

fn native_task_user(item: &Value) -> bool {
    if item.get("type").is_none() && item.get("role").and_then(Value::as_str) == Some("user") {
        let mut normalized = item.clone();
        normalized["type"] = json!("message");
        crate::compaction::is_task_user_message(&normalized)
    } else {
        crate::compaction::is_task_user_message(item)
    }
}
fn native_wire_role(item: &Value) -> Option<&'static str> {
    match item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message")
    {
        "message" => Some(
            if item.get("role").and_then(Value::as_str) == Some("assistant") {
                "assistant"
            } else {
                "user"
            },
        ),
        "agent_message"
        | "function_call_output"
        | "custom_tool_call_output"
        | "tool_search_output" => Some("user"),
        "function_call" | "custom_tool_call" | "tool_search_call" | "local_shell_call" => {
            Some("assistant")
        }
        "compaction" if native::is_item(item) => Some("assistant"),
        "reasoning"
            if item
                .pointer("/provider_metadata/anthropic/thinking_blocks")
                .and_then(Value::as_array)
                .is_some_and(|blocks| !blocks.is_empty()) =>
        {
            Some("assistant")
        }
        _ => None,
    }
}
fn full_native_history_with_active_request(
    history: Vec<Value>,
    open: &HashSet<String>,
) -> Result<(Vec<Value>, Vec<Value>), String> {
    if !open.is_empty() {
        return Err("native compaction must wait for outstanding tool results".into());
    }
    let active = history
        .iter()
        .rev()
        .find(|item| native_task_user(item))
        .cloned()
        .ok_or("native compaction has no active user request")?;
    if !history
        .iter()
        .any(|item| native_wire_role(item) == Some("assistant"))
    {
        return Err("native compaction has no completed work to summarize".into());
    }
    // A single Codex user turn can contain hundreds of completed tool calls.
    // Summarize it in full rather than refusing compaction. Carry only the active
    // user's exact request as an intentional task reminder; no already-summarized
    // assistant/tool/thinking blocks are replayed or treated as kept thinking.
    Ok((history, vec![active]))
}

/// Native compaction must summarize a contiguous prefix. The retained suffix
/// starts at a complete user-turn boundary, never through a tool-call batch.
fn native_prefix_and_tail(body: &Value, budget: u64) -> Result<(Vec<Value>, Vec<Value>), String> {
    native_prefix_and_tail_with_overhead(body, budget, 0)
}
fn native_prefix_and_tail_with_overhead(
    body: &Value,
    budget: u64,
    retained_overhead: usize,
) -> Result<(Vec<Value>, Vec<Value>), String> {
    let mut history = flatten_native_history(body)?;
    // Validate occurrences without the local summary selector: native history
    // intentionally retains signed thinking and the preceding signed checkpoint.
    let mut open = HashSet::new();
    let mut boundaries = Vec::new();
    let mut previous_wire_role = None;
    for (index, item) in history.iter().enumerate() {
        let kind = item.get("type").and_then(Value::as_str);
        if matches!(
            kind,
            Some("function_call" | "custom_tool_call" | "tool_search_call" | "local_shell_call")
        ) {
            let id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or("native compaction tool call requires call_id")?;
            if !open.insert(id.to_string()) {
                return Err("native compaction contains duplicate open tool calls".into());
            }
            if kind == Some("function_call") {
                match item.get("arguments") {
                    Some(Value::String(text)) => {
                        serde_json::from_str::<Value>(text)
                            .map_err(|_| "native compaction function arguments are not JSON")?;
                    }
                    Some(Value::Object(_) | Value::Array(_)) => {}
                    _ => {
                        return Err(
                            "native compaction function arguments are missing or invalid".into(),
                        )
                    }
                }
            }
        } else if matches!(
            kind,
            Some(
                "function_call_output"
                    | "custom_tool_call_output"
                    | "tool_search_output"
                    | "local_shell_call_output"
            )
        ) {
            let id = item
                .get("call_id")
                .and_then(Value::as_str)
                .ok_or("native compaction tool result requires call_id")?;
            if !open.remove(id) {
                return Err("native compaction contains an orphan tool result".into());
            }
        } else if index > 0
            && open.is_empty()
            && native_task_user(item)
            && previous_wire_role == Some("assistant")
        {
            boundaries.push(index);
        }
        if let Some(role) = native_wire_role(item) {
            previous_wire_role = Some(role);
        }
    }
    let Some(mut split) = boundaries.last().copied() else {
        return full_native_history_with_active_request(history, &open);
    };
    for index in boundaries.into_iter().rev() {
        if crate::compaction::estimate_input_items_tokens(&history[index..]) <= budget
            && history.len() - index + retained_overhead <= 256
            && serde_json::to_vec(&history[index..])
                .is_ok_and(|bytes| bytes.len() <= native::MAX_RETAINED_BYTES)
        {
            split = index;
        } else {
            break;
        }
    }
    if estimate_input_items_tokens(&history[split..]) > budget
        || history.len() - split + retained_overhead > 256
        || !serde_json::to_vec(&history[split..])
            .is_ok_and(|bytes| bytes.len() <= native::MAX_RETAINED_BYTES)
    {
        return full_native_history_with_active_request(history, &open);
    }
    let tail = history.split_off(split);
    if tail.len() > 256 {
        return Err("native compaction retained history exceeds item limit".into());
    }
    Ok((history, tail))
}

pub(crate) async fn native_anthropic_compact_candidate(
    state: &GatewayState,
    headers: &HeaderMap,
    body: &Value,
    candidate: &RouteCandidate,
    kind: CompactionRequestKind,
) -> Result<(Value, TokenUsage, Option<ProviderRetryObservation>), AttemptFailure> {
    super::senders::validate_candidate_image_references(body, candidate)?;
    let mut source = body.clone();
    // Authentication is resolved once per physical send. Keep the opaque block
    // through prefix selection; the sender checks its binding before translation.
    crate::compaction::validate_local_compactions(&source)
        .map_err(|s| request_failure("invalid_compaction_history", &s))?;
    // Expand older local checkpoints, but keep any matching signed checkpoint.
    crate::compaction::expand_translated_compactions_preserving_anthropic(&mut source);
    native::expand_retained(&mut source)
        .map_err(|s| request_failure("invalid_compaction_history", &s))?;
    // Imported thinking has no trustworthy generation-time prefix proof. Discard
    // pre-boundary thinking rather than claiming that a context hash proves it.
    if let Some(items) = source.get_mut("input").and_then(Value::as_array_mut) {
        native::strip_signed_thinking(items);
        crate::compaction::normalize_completed_empty_function_arguments(items);
    }
    normalize_responses_tool_result_adjacency(&mut source);

    let image_report = crate::translate::normalize_translated_image_history_for_compaction(
        &mut source,
        ProviderProtocol::AnthropicMessages,
        ProviderTransport::Standard,
        candidate.provider.limits.max_request_bytes,
    )
    .await
    .map_err(image_history_failure)?;
    if image_report.omitted_images > 0 {
        source[native::HISTORY_REWRITTEN] = json!(true);
    }
    // Include declarations embedded in the preceding checkpoint before collecting
    // tools or selecting a new prefix; otherwise the second compaction loses them.
    source["input"] = json!(flatten_native_history(&source)
        .map_err(|s| request_failure("invalid_compaction_history", &s))?);
    let mut dynamic_tools = Vec::new();
    for tool in crate::translate::collect_dynamic_tools(&source) {
        if !dynamic_tools.contains(&tool) {
            dynamic_tools.push(tool);
        }
    }
    crate::translate::merge_dynamic_tools(&mut source);
    let tail_budget = state
        .settings
        .read()
        .await
        .local_compaction
        .tail_token_limit();
    let (mut prefix, mut tail) = native_prefix_and_tail_with_overhead(
        &source,
        tail_budget,
        usize::from(!dynamic_tools.is_empty()),
    )
    .map_err(|s| request_failure("invalid_compaction_request", &s))?;
    let metadata_bytes = serde_json::to_vec(&dynamic_tools)
        .map_err(|_| request_failure("invalid_compaction_request", "cannot encode dynamic tools"))?
        .len();
    if !serde_json::to_vec(&tail)
        .is_ok_and(|bytes| bytes.len().saturating_add(metadata_bytes) <= native::MAX_RETAINED_BYTES)
    {
        let split = native_prefix_and_tail_with_overhead(
            &source,
            0,
            usize::from(!dynamic_tools.is_empty()),
        )
        .map_err(|s| request_failure("invalid_compaction_request", &s))?;
        prefix = split.0;
        tail = split.1;
    }
    if !serde_json::to_vec(&tail)
        .is_ok_and(|bytes| bytes.len().saturating_add(metadata_bytes) <= native::MAX_RETAINED_BYTES)
    {
        return Err(request_failure(
            "invalid_compaction_request",
            "active request and tool declarations exceed native transport budget",
        ));
    }
    if !dynamic_tools.is_empty() {
        tail.insert(0, json!({"type":"additional_tools","tools":dynamic_tools}));
    }
    let mut request = source;
    request["input"] = json!(prefix);
    crate::translate::prepare_translated_responses_request_with_native_anthropic(
        &mut request,
        true,
        true,
        true,
    );
    request["input"]
        .as_array_mut()
        .expect("prepared prefix")
        .push(json!({"type":"compaction_trigger"}));
    request["stream"] = json!(false);
    request["max_output_tokens"] = json!(4096);
    request[native::REQUEST_FLAG] = json!(true);
    if let Some(object) = request.as_object_mut() {
        object.remove("previous_response_id");
        object.remove("conversation");
    }
    apply_candidate_model_policy(&mut request, candidate, false)
        .map_err(|e| request_failure("invalid_compaction_request", &e.to_string()))?;
    let upstream = send_candidate(state, &mut request, candidate, Some(headers)).await?;
    let retry = upstream
        .extensions()
        .get::<ProviderRetryObservation>()
        .cloned();
    if !upstream.status().is_success() {
        let status = upstream.status();
        let classified = upstream_responses_error_classified(upstream, None).await;
        let mut response = classified.response;
        if let Some(retry) = retry {
            response.extensions_mut().insert(retry);
        }
        return Err(AttemptFailure {
            response,
            kind: if classified.context_window_exceeded {
                AttemptFailureKind::ContextWindow
            } else if status.is_server_error()
                || status == StatusCode::TOO_MANY_REQUESTS
                || status == StatusCode::REQUEST_TIMEOUT
            {
                AttemptFailureKind::Retryable
            } else {
                AttemptFailureKind::Request
            },
        });
    }
    let context = upstream
        .extensions()
        .get::<NativeAnthropicContext>()
        .cloned();
    let value = read_native_compaction_json(upstream, candidate.provider.limits.max_response_bytes).await?;
    let usage = native_compaction_usage(&value);
    let invalid = |message: &str| {
        let mut failure = request_failure("invalid_compaction_response", message);
        // A billed 200/max_tokens/refusal still consumed tokens. Keep these in
        // the existing failure observation channel, never a success checkpoint.
        let mut billed = retry.clone().unwrap_or_default();
        billed.usage = sum_token_usage(billed.usage, usage.clone());
        failure.response.extensions_mut().insert(billed);
        failure
    };
    let context = context.ok_or_else(|| invalid("native compaction context was lost"))?;
    if value.get("stop_reason").and_then(Value::as_str) != Some("compaction") {
        return Err(invalid(
            "Anthropic did not produce a completed compaction block; original history is unchanged",
        ));
    }
    let content = value
        .get("content")
        .and_then(Value::as_array)
        .filter(|blocks| blocks.len() == 1)
        .ok_or_else(|| invalid("native compaction must contain one signed block"))?;
    native::validate_block(&content[0]).map_err(|s| invalid(&s))?;
    native::strip_signed_thinking(&mut tail);
    let checkpoint = native::Checkpoint {
        version: 1,
        scope: context.scope,
        context_hash: context.context_hash,
        block: content[0].clone(),
        retained: if kind == CompactionRequestKind::NativeTrigger {
            tail.clone()
        } else {
            Vec::new()
        },
    };
    let item = native::encode(&checkpoint).map_err(|s| invalid(&s))?;
    let mut output = vec![item];
    if kind == CompactionRequestKind::Standalone {
        output.extend(tail);
    }
    // Never install a checkpoint whose mandatory suffix already cannot be
    // replayed: that would turn the next automatic compact into an infinite loop.
    let reserved_output = effective_output_token_limit(
        body.get("max_output_tokens").and_then(Value::as_u64),
        candidate.max_output_tokens,
        64_000,
    );
    if usable_input_token_limit(candidate, reserved_output)
        .is_some_and(|limit| estimate_input_items_tokens(&output) > limit)
    {
        return Err(invalid("native checkpoint and retained history do not fit the model input budget; original history is unchanged"));
    }
    let mut replay = body.clone();
    replay["input"] = json!(output);
    if let Some(object) = replay.as_object_mut() {
        object.remove(native::REQUEST_FLAG);
        object.remove(native::HISTORY_REWRITTEN);
        // Release generation forcing, but never widen the physical tool catalog.
        if let Some(choice) = object.get_mut("tool_choice") {
            relax_replay_tool_choice(choice);
        }
    }
    apply_candidate_model_policy(&mut replay, candidate, false)
        .map_err(|error| invalid(&error.to_string()))?;
    crate::translate::prepare_translated_responses_request_with_native_anthropic(
        &mut replay,
        true,
        true,
        true,
    );
    let wire_model = candidate.provider.wire_model_id(&candidate.upstream_model);
    let mut wire = responses_to_anthropic_with_oauth(
        &replay,
        &wire_model,
        uses_anthropic_subscription_oauth(&candidate.provider),
    )
    .map_err(|s| invalid(&s))?;
    apply_provider_wire_compatibility(
        &mut wire,
        &replay,
        candidate,
        ProviderProtocol::AnthropicMessages,
    )
    .map_err(|s| invalid(&s))?;
    let wire_context = Value::Object(
        wire.as_object()
            .expect("wire object")
            .iter()
            .filter(|(key, _)| key.as_str() != "messages")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    );
    let context_tokens = (serde_json::to_vec(&wire_context)
        .map_err(|_| invalid("cannot estimate replay context"))?
        .len() as u64)
        .div_ceil(4);
    if usable_input_token_limit(candidate, reserved_output).is_some_and(|limit| {
        estimate_input_items_tokens(&output).saturating_add(context_tokens) > limit
    }) {
        return Err(invalid("native checkpoint including system/tools exceeds the replay input budget; original history is unchanged"));
    }
    normalize_translated_image_history(&mut wire,ProviderProtocol::AnthropicMessages,ProviderTransport::Standard,candidate.provider.limits.max_request_bytes)
        .await.map_err(|_|invalid("native checkpoint retained images cannot be replayed within provider limits; original history is unchanged"))?;
    crate::translate::serialize_preserving_image_history_async(&mut wire,candidate.provider.limits.max_request_bytes)
        .await.map_err(|_|invalid("native checkpoint restored request exceeds provider wire limits; original history is unchanged"))?;
    Ok((
        json!({"id":format!("cmpct_{}",Uuid::new_v4().simple()),"object":"response.compaction","created_at":ObservationEvent::now_ms()/1000,
        "model":candidate.exposed_model,"output":output,"usage":{"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens,"total_tokens":usage.total_tokens,"input_tokens_details":if usage.cache_read_reported {json!({"cached_tokens":usage.cached_input_tokens})} else {json!({})}}}),
        usage,
        retry,
    ))
}

fn relax_replay_tool_choice(choice: &mut Value) {
    if choice.get("type").and_then(Value::as_str) == Some("allowed_tools") {
        if choice.get("mode").and_then(Value::as_str) == Some("required") {
            choice["mode"] = json!("auto");
        }
    } else if choice.as_str() == Some("required") || matches!(
        choice.get("type").and_then(Value::as_str), Some("function" | "custom" | "tool_search")
    ) {
        *choice = json!("auto");
    }
}

fn native_compaction_usage(value: &Value) -> TokenUsage {
    TokenUsage::from_json(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderDefinition;
    type Capture = Arc<Mutex<Vec<(Value, HeaderMap)>>>;
    async fn fixture(
        oauth: bool,
        stop: &str,
    ) -> (
        GatewayState,
        RouteCandidate,
        Capture,
        tokio::task::JoinHandle<()>,
    ) {
        let capture: Capture = Arc::new(Mutex::new(Vec::new()));
        let stop = stop.to_string();
        let router=Router::new().route("/v1/messages",post({let capture=capture.clone();move |headers:HeaderMap,Json(body):Json<Value>| {
            let capture=capture.clone();let stop=stop.clone();async move {
                let sequence={let mut capture=capture.lock().await;capture.push((body.clone(),headers));capture.len()};
                if let Some(code)=stop.strip_prefix("http-").and_then(|s|s.parse::<u16>().ok()) {
                    return (StatusCode::from_u16(code).unwrap(),Json(json!({"error":{"type":"invalid_request_error","message":"fixture upstream rejection"}}))).into_response();
                }
                if body.get("compaction").is_some() {
                    Json(json!({"id":"native-test","type":"message","role":"assistant","model":"claude-sonnet-5-5", "stop_reason":if stop.starts_with("compaction") {"compaction"} else {stop.as_str()},
                        "content":if stop.starts_with("compaction") {vec![json!({"type":"compaction","content":" SQLite\n id/title/done \t 日本語とユーザー訂正。デプロイ禁止。 ","signature":"fake-sig+opaque/=\n","future_field":{"keep":true}})]} else {vec![]},
                        "usage":{"input_tokens":0,"output_tokens":0,"iterations":[{"type":"compaction","input_tokens":100,"output_tokens":30}]}})).into_response()
                } else if stop.starts_with("empty-anthropic-stream") {
                    let mut events=vec![json!({"type":"message_start","message":{"id":"m","role":"assistant","content":[],"usage":{"input_tokens":1000,"cache_read_input_tokens":99_000,"cache_creation_input_tokens":500,"output_tokens":0}}})];
                    if sequence>1 {events.extend([json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),json!({"type":"content_block_stop","index":0})]);}
                    if sequence>1 && stop.ends_with("nonterminal") {
                        events.push(json!({"type":"message_delta","delta":{},"usage":{"input_tokens":2000,"cache_read_input_tokens":188_000,"cache_creation_input_tokens":1000,"output_tokens":2}}));
                        events.push(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}));
                    } else {
                    events.push(if sequence==1 {json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}})} else {json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":2000,"cache_read_input_tokens":188_000,"cache_creation_input_tokens":1000,"output_tokens":3}})});
                    }
                    events.push(json!({"type":"message_stop"}));let wire=events.into_iter().map(|event|sse(event["type"].as_str().unwrap(),&event)).collect::<String>();
                    Response::builder().status(200).header(header::CONTENT_TYPE,"text/event-stream").body(Body::from(wire)).unwrap()
                } else if stop=="compaction-dynamic" {
                    let calls=body["tools"].as_array().unwrap().iter().filter(|tool|tool["name"]!="lookup").enumerate().map(|(i,tool)|json!({"type":"tool_use","id":format!("call-{i}"),"name":tool["name"],"input":{"input":"blue"}})).collect::<Vec<_>>();
                    Json(json!({"id":"normal","type":"message","role":"assistant","stop_reason":"tool_use","content":calls,"usage":{"input_tokens":100,"output_tokens":1,"cache_read_input_tokens":99}})).into_response()
                } else if stop=="empty-anthropic" {
                    Json(json!({"id":"normal","type":"message","role":"assistant","stop_reason":"end_turn","content":if sequence==1 {vec![]} else {vec![json!({"type":"text","text":"ok"})]},"usage":{"input_tokens":1000,"output_tokens":1,"cache_read_input_tokens":99_000,"cache_creation_input_tokens":500}})).into_response()
                } else {Json(json!({"id":"normal","type":"message","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"context retained"}],"usage":{"input_tokens":100,"output_tokens":1,"cache_read_input_tokens":99}})).into_response()}
            }
        }}));
        let router = router.layer(DefaultBodyLimit::max(16 * 1024 * 1024));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let mut settings = GatewaySettings::default();
        settings.security.dns_pinning = false;
        settings.security.require_local_token = false;
        let settings = Arc::new(RwLock::new(settings));
        let state = GatewayState {
            settings: settings.clone(),
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            routing: Arc::new(Mutex::new(RoutingRuntime::default())),
            observability: ObservabilityLedger::new(None),
            video_jobs: Arc::new(Mutex::new(HashMap::new())),
            instance_id: "native-anthropic-test".into(),
            response_state: Arc::new(ResponseStateStore::new(None)),
            memory: Arc::new(MemoryAdmission {
                settings,
                inflight: AtomicU32::new(0),
                reserved_bytes: AtomicU64::new(0),
                rejected: AtomicU64::new(0),
            }),
            pacing: Arc::new(ProviderPacing::default()),
        };
        let mut provider = ProviderDefinition::default();
        provider.id = "anthropic".into();
        provider.protocol = ProviderProtocol::AnthropicMessages;
        provider.base_url = format!("http://{address}");
        // Dedicated fixed test credential: never consult a real account or token.
        std::env::set_var("CODETAS_R13_NATIVE_FIXTURE_CREDENTIAL", "r13-fixed-test-credential");
        provider.credential.source = CredentialSource::Environment;
        provider.credential.reference = Some("CODETAS_R13_NATIVE_FIXTURE_CREDENTIAL".into());
        provider.limits.request_retries = 0;
        provider.enabled = true;
        provider.models = vec!["claude-sonnet-5-5".into(), "claude-opus-5-5".into()];
        if oauth {
            provider.credential.transport = crate::config::CredentialTransport::Bearer;
        }
        let candidate = RouteCandidate {
            capabilities: provider.capabilities.clone(),
            provider,
            upstream_model: "claude-sonnet-5-5".into(),
            exposed_model: "claude".into(),
            credential: None,
            account_id: None,
            target_key: "anthropic/claude".into(),
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
            routing_epoch: 0,
            routing_generation: 0,
            session_scope: None,
        };
        state.settings.write().await.providers = vec![candidate.provider.clone()];
        let candidate = state
            .routing
            .lock()
            .await
            .candidates_for_request(
                &*state.settings.read().await,
                "anthropic/claude-sonnet-5-5",
                false,
                None,
            )
            .unwrap()
            .remove(0);
        (state, candidate, capture, server)
    }
    fn request() -> Value {
        json!({"model":"claude","instructions":"Preserve SQLite and no deployment.","tools":[{"type":"function","name":"lookup","parameters":{"type":"object","properties":{}}}],"reasoning":{"effort":"high"},"input":[
            {"type":"message","role":"user","content":"TODO app: SQLite id/title/done. No deploy."},
            {"type":"message","role":"assistant","content":"I will keep these constraints."},
            {"type":"message","role":"user","content":"最新要求: titleを追加して"}
        ]})
    }
    #[tokio::test]
    async fn native_anthropic_json_signed_roundtrip_recompact_and_client_sse() {
        for oauth in [false, true] {
            for kind in [
                CompactionRequestKind::Standalone,
                CompactionRequestKind::NativeTrigger,
            ] {
                let (state, candidate, capture, server) = fixture(oauth, "compaction").await;
                let original = request();
                let result = native_anthropic_compact_candidate(
                    &state,
                    &HeaderMap::new(),
                    &original,
                    &candidate,
                    kind,
                )
                .await;
                let (compacted, usage, _) = match result {
                    Ok(value) => value,
                    Err(failure) => panic!(
                        "native fixture failure: {}",
                        String::from_utf8_lossy(
                            &to_bytes(failure.response.into_body(), 10000).await.unwrap()
                        )
                    ),
                };
                assert_eq!(usage.input_tokens, 100);
                assert_eq!(usage.output_tokens, 30);
                assert_eq!(usage.total_tokens, 130);
                let checkpoint = native::decode(&compacted["output"][0]).unwrap().unwrap();
                assert_eq!(checkpoint.block["signature"], "fake-sig+opaque/=\n");
                let mut continued = original.clone();
                let mut input = compacted["output"].as_array().unwrap().clone();
                input.push(
                    json!({"type":"message","role":"user","content":"続けて。デプロイはしない"}),
                );
                continued["input"] = json!(input);
                let upstream = send_candidate(&state, &mut continued, &candidate, None)
                    .await
                    .unwrap();
                assert!(upstream.status().is_success());
                let _ = bounded_json(upstream, 1_000_000).await.unwrap();
                {
                    let captured = capture.lock().await;
                    let first = &captured[0].0;
                    let next = &captured[1].0;
                    assert_eq!(first["compaction"], json!({"type":"summarize"}));
                    assert_eq!(first["max_tokens"], 4096);
                    assert_eq!(first["messages"].as_array().unwrap().len(), 2); // no synthetic continuation
                    assert_eq!(first["system"], next["system"]);
                    assert_eq!(first["tools"], next["tools"]);
                    assert_eq!(next["messages"][0]["content"][0], checkpoint.block);
                    assert!(next.to_string().contains("最新要求: titleを追加して"));
                    assert!(!next.to_string().contains(native::PREFIX));
                    for (_, headers) in captured.iter() {
                        let beta = headers.get("anthropic-beta").unwrap().to_str().unwrap();
                        assert!(beta.contains(native::BETA));
                        if oauth {
                            assert!(beta.contains("oauth-2025-04-20"));
                        }
                    }
                    if oauth {
                        assert_eq!(
                            first["system"][0]["text"],
                            "You are a Claude agent, built on Anthropic's Claude Agent SDK."
                        );
                    }
                }
                let (again, _, _) = native_anthropic_compact_candidate(
                    &state,
                    &HeaderMap::new(),
                    &continued,
                    &candidate,
                    kind,
                )
                .await
                .unwrap();
                let latest = native::decode(&again["output"][0]).unwrap().unwrap();
                assert_eq!(latest.block, checkpoint.block);
                let captured = capture.lock().await;
                assert_eq!(captured[2].0["messages"][0]["content"][0], checkpoint.block);
                drop(captured);
                let response =
                    super::super::protocol::compaction_client_response(StatusCode::OK, again, true);
                let encoded = to_bytes(response.into_body(), 1_000_000).await.unwrap();
                let text = String::from_utf8(encoded.to_vec()).unwrap();
                assert!(text.contains("response.completed"));
                assert!(text.contains(native::PREFIX));
                server.abort();
            }
        }
    }
    #[tokio::test]
    async fn native_anthropic_model_provider_account_switch_downgrades_without_signature() {
        let (state, candidate, capture, server) = fixture(false, "compaction").await;
        let original = request();
        let result = native_anthropic_compact_candidate(
            &state,
            &HeaderMap::new(),
            &original,
            &candidate,
            CompactionRequestKind::NativeTrigger,
        )
        .await;
        let (compacted, _, _) = match result {
            Ok(value) => value,
            Err(failure) => panic!(
                "native fixture failure: {}",
                String::from_utf8_lossy(
                    &to_bytes(failure.response.into_body(), 10000).await.unwrap()
                )
            ),
        };
        for switch in 0..4 {
            let mut different = candidate.clone();
            match switch {
                0 => different.upstream_model = "claude-opus-5-5".into(),
                1 => different.provider.id = "other".into(),
                2 => different.account_id = Some("other-account".into()),
                _ => different
                    .provider
                    .query_params
                    .insert("project".into(), "other".into())
                    .map(|_| ())
                    .unwrap_or(()),
            };
            state.settings.write().await.providers = vec![different.provider.clone()];
            let selected = state
                .routing
                .lock()
                .await
                .candidates_for_request(
                    &*state.settings.read().await,
                    &format!("{}/{}", different.provider.id, different.upstream_model),
                    false,
                    None,
                )
                .unwrap()
                .remove(0);
            different.routing_epoch = selected.routing_epoch;
            different.routing_generation = selected.routing_generation;
            different.target_key = selected.target_key;
            let mut body = original.clone();
            body["input"] = compacted["output"].clone();
            let upstream = send_candidate(&state, &mut body, &different, None)
                .await
                .unwrap();
            let _ = bounded_json(upstream, 1_000_000).await.unwrap();
            let captured = capture.lock().await;
            let wire = &captured.last().unwrap().0;
            assert!(!wire.to_string().contains("fake-sig"));
            assert!(!wire.to_string().contains(native::PREFIX));
            assert!(wire.to_string().contains("SQLite"));
            assert!(wire.to_string().contains("最新要求"));
        }
        server.abort();
    }
    #[tokio::test]
    async fn native_anthropic_missing_summary_is_an_error_not_a_checkpoint() {
        for stop in [
            "max_tokens",
            "end_turn",
            "refusal",
            "tool_use",
            "model_context_window_exceeded",
        ] {
            let (state, candidate, capture, server) = fixture(false, stop).await;
            let result = native_anthropic_compact_candidate(
                &state,
                &HeaderMap::new(),
                &request(),
                &candidate,
                CompactionRequestKind::NativeTrigger,
            )
            .await;
            let failure = result.expect_err(stop);
            let billed = failure
                .response
                .extensions()
                .get::<ProviderRetryObservation>()
                .expect("billed failure usage");
            assert_eq!(billed.usage.input_tokens, 100);
            assert_eq!(billed.usage.output_tokens, 30);
            assert_eq!(billed.additional_sends, 0);
            assert_eq!(capture.lock().await.len(), 1);
            server.abort();
        }
    }
    #[tokio::test]
    async fn native_anthropic_selection_is_canonical_and_does_not_change_other_routes() {
        let (_, mut candidate, _, server) = fixture(false, "compaction").await;
        assert_eq!(
            candidate_compaction_mode(&candidate, CompactionRequestKind::Standalone),
            CompactionMode::Local
        );
        candidate.provider.base_url = "https://api.anthropic.com/v1".into();
        for source in [
            CredentialSource::Environment,
            CredentialSource::Keychain,
            CredentialSource::Command,
            CredentialSource::OAuth,
        ] {
            candidate.provider.credential.source = source;
            for kind in [
                CompactionRequestKind::Standalone,
                CompactionRequestKind::NativeTrigger,
            ] {
                assert_eq!(
                    candidate_compaction_mode(&candidate, kind),
                    CompactionMode::AnthropicNative
                );
            }
        }
        for base in [
            "https://api.anthropic.com",
            "https://api.anthropic.com/v1/messages",
            "https://api.anthropic.com/v1/",
        ] {
            candidate.provider.base_url = base.into();
            assert_eq!(
                candidate_compaction_mode(&candidate, CompactionRequestKind::Standalone),
                CompactionMode::AnthropicNative
            );
        }
        candidate.upstream_model = "claude-unknown".into();
        assert_eq!(
            candidate_compaction_mode(&candidate, CompactionRequestKind::Standalone),
            CompactionMode::Local
        );
        candidate.upstream_model = "claude-sonnet-5-5".into();
        candidate.provider.protocol = ProviderProtocol::ChatCompletions;
        assert_eq!(
            candidate_compaction_mode(&candidate, CompactionRequestKind::Standalone),
            CompactionMode::Local
        );
        server.abort();
    }
    #[tokio::test]
    async fn native_anthropic_dynamic_tools_survive_but_unproven_thinking_does_not() {
        let (state, candidate, capture, server) = fixture(false, "compaction").await;
        let mut original = request();
        original["input"].as_array_mut().unwrap().insert(1,json!({"type":"additional_tools","tools":[{"type":"function","name":"extra","parameters":{"type":"object","properties":{}}}]}));
        original["input"].as_array_mut().unwrap().extend([
            json!({"type":"reasoning","summary":[],"provider_metadata":{"anthropic":{"thinking_blocks":[{"type":"thinking","thinking":"preserved thought","signature":"fake-thought-signature"}]}}}),
            json!({"type":"message","role":"assistant","content":"Latest answer"})
        ]);
        let result = native_anthropic_compact_candidate(
            &state,
            &HeaderMap::new(),
            &original,
            &candidate,
            CompactionRequestKind::NativeTrigger,
        )
        .await;
        let (compacted, _, _) = match result {
            Ok(value) => value,
            Err(failure) => panic!(
                "native fixture failure: {}",
                String::from_utf8_lossy(
                    &to_bytes(failure.response.into_body(), 10000).await.unwrap()
                )
            ),
        };
        let mut continued = original.clone();
        continued["input"] = compacted["output"].clone();
        continued["input"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"message","role":"user","content":"continue"}));
        for changed in [false, true] {
            let mut body = continued.clone();
            if changed {
                body["instructions"] = json!("Changed system instructions.");
            }
            let response = send_candidate(&state, &mut body, &candidate, None)
                .await
                .unwrap();
            let _ = bounded_json(response, 1_000_000).await.unwrap();
            let captured = capture.lock().await;
            let wire = &captured.last().unwrap().0;
            assert!(!wire.to_string().contains("fake-thought-signature"));
            assert_eq!(
                wire["messages"][0]["content"][0]["signature"],
                "fake-sig+opaque/=\n"
            );
            if !changed {
                assert_eq!(wire["tools"], captured[0].0["tools"]);
            }
        }
        server.abort();
    }
    #[test]
    fn native_anthropic_contiguous_split_never_cuts_a_tool_batch() {
        let body = json!({"input":[
            {"type":"message","role":"user","content":"old request"},
            {"type":"function_call","call_id":"c","name":"lookup","arguments":"{}"},
            {"type":"function_call_output","call_id":"c","output":"ok"},
            {"type":"message","role":"user","content":"same-role boundary must not split"},
            {"type":"message","role":"assistant","content":"complete"},
            {"type":"message","role":"user","content":"latest question"}
        ]});
        let (prefix, tail) = native_prefix_and_tail(&body, 20_000).unwrap();
        assert_eq!(prefix.len(), 5);
        assert_eq!(tail, vec![body["input"][5].clone()]);
        let mut orphan = body;
        orphan["input"][2]["call_id"] = json!("orphan");
        assert!(native_prefix_and_tail(&orphan, 20_000).is_err());
    }

    #[tokio::test]
    async fn native_anthropic_retained_images_still_obey_normal_input_admission() {
        let (_, mut candidate, _, server) = fixture(false, "compaction").await;
        let checkpoint = native::Checkpoint {
            version: 1,
            scope: anthropic_checkpoint_scope(&candidate, "fixture-resolved-auth"),
            context_hash: "a".repeat(64),
            block: json!({"type":"compaction","content":"previous work","signature":"fake"}),
            retained: vec![
                json!({"type":"message","role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,YQ=="}]}),
            ],
        };
        let mut body =
            json!({"input":[native::encode(&checkpoint).unwrap()],"max_output_tokens":1});
        let original = body.clone();
        assert_eq!(
            estimate_input_items(body["input"].as_array().unwrap()).image_count,
            1
        );
        candidate.max_input_tokens = Some(2000);
        candidate.context_window = Some(2000);
        assert!(apply_candidate_model_policy(&mut body, &candidate, false).is_err());
        assert_eq!(body, original);
        server.abort();
    }

    #[tokio::test]
    async fn native_anthropic_http_failures_remain_failures_without_local_checkpoint_or_quota_assumption(
    ) {
        for code in [400, 429, 500] {
            let (state, candidate, capture, server) = fixture(false, &format!("http-{code}")).await;
            let failure = native_anthropic_compact_candidate(
                &state,
                &HeaderMap::new(),
                &request(),
                &candidate,
                CompactionRequestKind::NativeTrigger,
            )
            .await
            .unwrap_err();
            assert_eq!(failure.response.status().as_u16(), code);
            assert!(failure
                .response
                .extensions()
                .get::<CompactionQuotaExhausted>()
                .is_none());
            let bytes = to_bytes(failure.response.into_body(), 10000).await.unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains(native::PREFIX));
            assert_eq!(capture.lock().await.len(), 1);
            server.abort();
        }
    }
    #[tokio::test]
    async fn native_anthropic_recompact_keeps_dynamic_tools_and_drops_invalid_old_thinking() {
        for changed in [false, true] {
            let (state, candidate, capture, server) = fixture(false, "compaction").await;
            let mut original = request();
            original["input"].as_array_mut().unwrap().insert(1,json!({"type":"additional_tools","tools":[{"type":"function","name":"extra","parameters":{"type":"object"}}]}));
            original["input"].as_array_mut().unwrap().extend([
                json!({"type":"reasoning","summary":[],"provider_metadata":{"anthropic":{"thinking_blocks":[{"type":"thinking","thinking":"old","signature":"fake-old-thinking"},{"type":"redacted_thinking","data":"fake-redacted"}]}}}),
                json!({"type":"message","role":"assistant","content":"tail answer"})
            ]);
            let (first, _, _) = native_anthropic_compact_candidate(
                &state,
                &HeaderMap::new(),
                &original,
                &candidate,
                CompactionRequestKind::NativeTrigger,
            )
            .await
            .unwrap();
            let mut next = original.clone();
            next["input"] = first["output"].clone();
            if changed {
                next["instructions"] = json!("changed system and policy");
            }
            next["input"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type":"message","role":"user","content":"next turn"}));
            let (second, _, _) = native_anthropic_compact_candidate(
                &state,
                &HeaderMap::new(),
                &next,
                &candidate,
                CompactionRequestKind::NativeTrigger,
            )
            .await
            .unwrap();
            let mut replay = next.clone();
            replay["input"] = second["output"].clone();
            replay["input"].as_array_mut().unwrap().push(
                json!({"type":"message","role":"user","content":"continue after second compact"}),
            );
            let upstream = send_candidate(&state, &mut replay, &candidate, None)
                .await
                .unwrap();
            let _ = bounded_json(upstream, 1_000_000).await.unwrap();
            let captured = capture.lock().await;
            let wire = &captured.last().unwrap().0;
            assert_eq!(wire["tools"], captured[1].0["tools"]);
            assert!(wire["tools"].to_string().contains("extra"));
            assert!(!wire.to_string().contains("fake-old-thinking"));
            assert!(!wire.to_string().contains("fake-redacted"));
            server.abort();
        }
    }
    #[tokio::test]
    async fn native_anthropic_rewritten_compaction_prefix_does_not_keep_bound_thinking() {
        for admission_rewrite in [false, true] {
            let (state, mut candidate, capture, server) = fixture(false, "compaction").await;
            let mut original = request();
            if admission_rewrite {
                original[native::HISTORY_REWRITTEN] = json!(true);
            } else {
                candidate.provider.limits.max_request_bytes = 5000;
                state.settings.write().await.providers = vec![candidate.provider.clone()];
                let fresh = state
                    .routing
                    .lock()
                    .await
                    .candidates_for_request(
                        &*state.settings.read().await,
                        "anthropic/claude-sonnet-5-5",
                        false,
                        None,
                    )
                    .unwrap()
                    .remove(0);
                candidate.routing_epoch = fresh.routing_epoch;
                candidate.routing_generation = fresh.routing_generation;
                original["input"][0]["content"] = json!([{"type":"input_image","image_url":format!("data:image/jpeg;base64,{}","A".repeat(12_000))}]);
            }
            original["input"].as_array_mut().unwrap().extend([json!({"type":"reasoning","summary":[],"provider_metadata":{"anthropic":{"thinking_blocks":[{"type":"thinking","thinking":"old","signature":"fake-bound-thinking"}]}}}),json!({"type":"message","role":"assistant","content":"answer"})]);
            let (compacted, _, _) = native_anthropic_compact_candidate(
                &state,
                &HeaderMap::new(),
                &original,
                &candidate,
                CompactionRequestKind::NativeTrigger,
            )
            .await
            .unwrap();
            assert!(!native::decode(&compacted["output"][0])
                .unwrap()
                .unwrap()
                .retained
                .to_vec()
                .iter()
                .any(|i| i.to_string().contains("fake-bound-thinking")));
            let captured = capture.lock().await;
            assert!(!captured[0]
                .0
                .to_string()
                .contains(native::HISTORY_REWRITTEN));
            server.abort();
        }
    }
    #[tokio::test]
    async fn native_anthropic_real_both_compact_dispatches_reach_send_and_fail_closed() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for trigger in [false, true] {
            let (mut state, mut candidate, _, unused_mock) = fixture(false, "compaction").await;
            // This local CONNECT proxy rejects before TLS. No DNS or Anthropic
            // traffic can occur, but the real canonical dispatch reaches send.
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0; 1024];
                while !bytes.windows(4).any(|b| b == b"\r\n\r\n") {
                    let n = socket.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                    assert!(bytes.len() < 16384);
                }
                socket.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                String::from_utf8(bytes)
                    .unwrap()
                    .lines()
                    .next()
                    .unwrap()
                    .to_string()
            });
            candidate.provider.base_url = "https://api.anthropic.com/v1".into();
            candidate.provider.limits.request_retries = 0;
            state.settings.write().await.providers = vec![candidate.provider];
            state.client = reqwest::Client::builder()
                .proxy(reqwest::Proxy::https(format!("http://{address}")).unwrap())
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap();
            let mut body = request();
            body["model"] = json!("anthropic/claude-sonnet-5-5");
            let response = if trigger {
                body["input"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"type":"compaction_trigger"}));
                responses(State(state), HeaderMap::new(), Json(body)).await
            } else {
                compact_response(State(state), HeaderMap::new(), Json(body)).await
            };
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            let bytes = to_bytes(response.into_body(), 10000).await.unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains(native::PREFIX));
            let line = tokio::time::timeout(Duration::from_secs(5), proxy)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(line, "CONNECT api.anthropic.com:443 HTTP/1.1");
            unused_mock.abort();
        }
    }
    #[tokio::test]
    async fn native_anthropic_does_not_install_an_unreplayable_mandatory_suffix() {
        let (state, mut candidate, _, server) = fixture(false, "compaction").await;
        candidate.max_input_tokens = Some(300);
        let mut original = request();
        original["input"][2]["content"] = json!("latest mandatory question ".repeat(200));
        assert!(native_anthropic_compact_candidate(
            &state,
            &HeaderMap::new(),
            &original,
            &candidate,
            CompactionRequestKind::NativeTrigger
        )
        .await
        .is_err());
        server.abort();
    }
    #[tokio::test]
    async fn native_anthropic_single_user_tool_heavy_turn_compacts_without_replaying_tools() {
        let (state, candidate, capture, server) = fixture(false, "compaction").await;
        let mut original = request();
        original["input"] = json!([
            {"type":"message","role":"user","content":"Active request: inspect files once and preserve SQLite; never deploy."},
            {"type":"function_call","call_id":"c","name":"lookup","arguments":"{}"},
            {"type":"function_call_output","call_id":"c","output":"file already inspected"},
            {"type":"message","role":"assistant","content":"inspected and ready to continue"}
        ]);
        let (compacted, _, _) = native_anthropic_compact_candidate(
            &state,
            &HeaderMap::new(),
            &original,
            &candidate,
            CompactionRequestKind::NativeTrigger,
        )
        .await
        .unwrap();
        let checkpoint = native::decode(&compacted["output"][0]).unwrap().unwrap();
        assert_eq!(checkpoint.retained, vec![original["input"][0].clone()]);
        let captured = capture.lock().await;
        assert!(captured[0].0.to_string().contains("file already inspected"));
        drop(captured);
        let mut replay = original.clone();
        replay["input"] = compacted["output"].clone();
        let response = send_candidate(&state, &mut replay, &candidate, None)
            .await
            .unwrap();
        let _ = bounded_json(response, 1_000_000).await.unwrap();
        let captured = capture.lock().await;
        let wire = &captured.last().unwrap().0;
        assert!(wire.to_string().contains("Active request"));
        assert!(!wire.to_string().contains("file already inspected"));
        server.abort();
    }
    #[test]
    fn native_anthropic_ignored_reasoning_is_not_an_assistant_split_boundary() {
        let body = json!({"input":[{"type":"message","role":"user","content":"first"},{"type":"reasoning","summary":[{"type":"summary_text","text":"not Anthropic signed thinking"}]},{"type":"message","role":"user","content":"latest"},{"type":"message","role":"assistant","content":"work"}]});
        let (prefix, tail) = native_prefix_and_tail(&body, 20_000).unwrap();
        assert_eq!(prefix.len(), 4);
        assert_eq!(tail, vec![body["input"][2].clone()]);
    }
    #[tokio::test]
    async fn native_anthropic_does_not_install_a_suffix_over_wire_limits() {
        let (state, mut candidate, _, server) = fixture(false, "compaction").await;
        candidate.provider.limits.max_request_bytes = 3500;
        state.settings.write().await.providers = vec![candidate.provider.clone()];
        let fresh = state
            .routing
            .lock()
            .await
            .candidates_for_request(
                &*state.settings.read().await,
                "anthropic/claude-sonnet-5-5",
                false,
                None,
            )
            .unwrap()
            .remove(0);
        candidate.routing_epoch = fresh.routing_epoch;
        candidate.routing_generation = fresh.routing_generation;
        let mut original = request();
        original["input"][2]["content"] = json!("mandatory task text ".repeat(700));
        let failure = native_anthropic_compact_candidate(
            &state,
            &HeaderMap::new(),
            &original,
            &candidate,
            CompactionRequestKind::NativeTrigger,
        )
        .await
        .unwrap_err();
        assert!(failure
            .response
            .extensions()
            .get::<ProviderRetryObservation>()
            .is_some());
        server.abort();
    }
    #[tokio::test]
    async fn native_anthropic_gateway_restores_custom_and_namespaced_response_tool_identity() {
        let (state, candidate, _, server) = fixture(false, "compaction-dynamic").await;
        let mut original = request();
        original["input"].as_array_mut().unwrap().insert(1,json!({"type":"additional_tools","tools":[{"type":"custom","name":"paint","format":{"type":"text"}},{"type":"namespace","name":"ns","tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}]}]}));
        let (compacted, _, _) = native_anthropic_compact_candidate(
            &state,
            &HeaderMap::new(),
            &original,
            &candidate,
            CompactionRequestKind::NativeTrigger,
        )
        .await
        .unwrap();
        let mut body = original.clone();
        body["model"] = json!("anthropic/claude-sonnet-5-5");
        body["input"] = compacted["output"].clone();
        body["input"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"message","role":"user","content":"call the dynamic tools"}));
        let response = responses(State(state), HeaderMap::new(), Json(body)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let value: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap())
                .unwrap();
        let output = value["output"].as_array().unwrap();
        assert!(output.iter().any(|item| item["type"] == "custom_tool_call"
            && item["name"] == "paint"
            && item["input"] == "blue"));
        assert!(output.iter().any(|item| item["type"] == "function_call"
            && item["name"] == "lookup"
            && item["namespace"] == "ns"));
        server.abort();
    }
    #[tokio::test]
    async fn native_anthropic_retained_pending_call_matches_new_result_before_any_orphan_repair() {
        let (state, candidate, capture, server) = fixture(false, "compaction").await;
        let mut original = request();
        original["input"].as_array_mut().unwrap().push(
            json!({"type":"function_call","call_id":"pending","name":"lookup","arguments":"{}"}),
        );
        let (compacted, _, _) = native_anthropic_compact_candidate(
            &state,
            &HeaderMap::new(),
            &original,
            &candidate,
            CompactionRequestKind::NativeTrigger,
        )
        .await
        .unwrap();
        let mut body = original.clone();
        body["input"] = compacted["output"].clone();
        body["input"].as_array_mut().unwrap().push(json!({"type":"function_call_output","call_id":"pending","output":"matched tool result"}));
        let response = send_candidate(&state, &mut body, &candidate, None)
            .await
            .unwrap();
        let _ = bounded_json(response, 1_000_000).await.unwrap();
        let captured = capture.lock().await;
        let messages = captured.last().unwrap().0["messages"].as_array().unwrap();
        assert!(messages
            .iter()
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .any(|block| block["type"] == "tool_result" && block["tool_use_id"] == "pending"));
        server.abort();
    }
    #[tokio::test]
    async fn native_anthropic_png_bytes_match_before_compact_in_prefix_and_after_restore() {
        use base64::Engine as _;
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, 2400, 800);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&vec![127; 2400 * 800 * 3]).unwrap();
        }
        let image = json!({"type":"input_image","image_url":format!("data:image/png;base64,{}",base64::engine::general_purpose::STANDARD.encode(png))});
        let (state, candidate, capture, server) = fixture(false, "compaction").await;
        let mut original = request();
        original["input"][0]["content"] =
            json!([{"type":"input_text","text":"old task"},image.clone()]);
        original["input"][2]["content"] =
            json!([{"type":"input_text","text":"current image task"},image]);
        original["input"].as_array_mut().unwrap().extend([json!({"type":"reasoning","summary":[],"provider_metadata":{"anthropic":{"thinking_blocks":[{"type":"thinking","thinking":"bound","signature":"fake-stable-png-thought"}]}}}),json!({"type":"message","role":"assistant","content":"current work"})]);
        let mut before = original.clone();
        let response = send_candidate(&state, &mut before, &candidate, None)
            .await
            .unwrap();
        let _ = bounded_json(response, 1_000_000).await.unwrap();
        let (compacted, _, _) = native_anthropic_compact_candidate(
            &state,
            &HeaderMap::new(),
            &original,
            &candidate,
            CompactionRequestKind::NativeTrigger,
        )
        .await
        .unwrap();
        let mut replay = original.clone();
        replay["input"] = compacted["output"].clone();
        replay["input"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"message","role":"user","content":"continue"}));
        let response = send_candidate(&state, &mut replay, &candidate, None)
            .await
            .unwrap();
        let _ = bounded_json(response, 1_000_000).await.unwrap();
        fn images(value: &Value) -> Vec<Value> {
            let mut out = Vec::new();
            if let Some(messages) = value["messages"].as_array() {
                for message in messages {
                    if let Some(blocks) = message["content"].as_array() {
                        out.extend(blocks.iter().filter(|b| b["type"] == "image").cloned());
                    }
                }
            }
            out
        }
        let captured = capture.lock().await;
        let before = images(&captured[0].0);
        assert_eq!(before.len(), 2);
        assert_eq!(images(&captured[1].0), vec![before[0].clone()]);
        assert_eq!(images(&captured[2].0), vec![before[1].clone()]);
        assert!(!captured[2]
            .0
            .to_string()
            .contains("fake-stable-png-thought"));
        server.abort();
    }
    #[tokio::test]
    async fn native_anthropic_item_limit_and_dynamic_declaration_overhead_use_full_compact() {
        let (state, candidate, _, server) = fixture(false, "compaction").await;
        let mut original = request();
        original["input"].as_array_mut().unwrap().insert(1,json!({"type":"additional_tools","tools":[{"type":"custom","name":"paint","format":{"type":"text"}}]}));
        for n in 0..130 {
            original["input"].as_array_mut().unwrap().extend([json!({"type":"function_call","call_id":format!("c-{n}"),"name":"lookup","arguments":"{}"}),json!({"type":"function_call_output","call_id":format!("c-{n}"),"output":"ok"})]);
        }
        let (compacted, _, _) = native_anthropic_compact_candidate(
            &state,
            &HeaderMap::new(),
            &original,
            &candidate,
            CompactionRequestKind::NativeTrigger,
        )
        .await
        .unwrap();
        let checkpoint = native::decode(&compacted["output"][0]).unwrap().unwrap();
        assert_eq!(checkpoint.retained.len(), 2);
        assert_eq!(checkpoint.retained[0]["type"], "additional_tools");
        assert_eq!(checkpoint.retained[1], original["input"][3]);
        server.abort();
    }
    #[tokio::test]
    async fn cache_health_anthropic_empty_retry_reports_full_billing_and_final_sample_separately() {
        let (state, mut candidate, capture, server) = fixture(false, "empty-anthropic").await;
        candidate.provider.empty_completion_retry_models = vec!["*".into()];
        candidate.provider.limits.empty_completion_retries = 1;
        state.settings.write().await.providers = vec![candidate.provider.clone()];
        let fresh = state
            .routing
            .lock()
            .await
            .candidates_for_request(
                &*state.settings.read().await,
                "anthropic/claude-sonnet-5-5",
                false,
                None,
            )
            .unwrap()
            .remove(0);
        candidate.routing_epoch = fresh.routing_epoch;
        candidate.routing_generation = fresh.routing_generation;
        let mut body = request();
        let upstream = send_candidate(&state, &mut body, &candidate, None)
            .await
            .unwrap();
        let final_sample = upstream
            .extensions()
            .get::<crate::observability::CacheObservation>()
            .unwrap()
            .clone();
        assert_eq!(final_sample.input_tokens, 100_500);
        assert_eq!(final_sample.cached_input_tokens, 99_000);
        let raw = bounded_json(upstream, 1_000_000).await.unwrap();
        assert_eq!(raw["usage"]["input_tokens"], 2000);
        assert_eq!(raw["usage"]["cache_read_input_tokens"], 198_000);
        assert_eq!(raw["usage"]["cache_creation_input_tokens"], 1000);
        let adapted = anthropic_to_response(&raw, "claude", &ResponseToolMap::default()).unwrap();
        let billing = TokenUsage::from_json(&adapted);
        assert_eq!(billing.input_tokens, 201_000);
        assert_eq!(billing.cached_input_tokens, 198_000);
        assert_eq!(capture.lock().await.len(), 2);
        server.abort();
    }
    #[tokio::test]
    async fn native_anthropic_replay_budget_includes_system_and_dynamic_tool_definitions() {
        let (state, mut candidate, _, server) = fixture(false, "compaction").await;
        candidate.max_input_tokens = Some(900);
        let mut original = request();
        original["instructions"] = json!("system ".repeat(350));
        original["input"][2]["content"] = json!("current task: ".repeat(100));
        let failure = native_anthropic_compact_candidate(
            &state,
            &HeaderMap::new(),
            &original,
            &candidate,
            CompactionRequestKind::NativeTrigger,
        )
        .await
        .unwrap_err();
        let bytes = to_bytes(failure.response.into_body(), 10000).await.unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("system/tools"));
        server.abort();
    }
    #[tokio::test]
    async fn native_anthropic_completed_empty_arguments_are_normalized_but_pending_or_bad_are_not()
    {
        for (arguments, completed) in [("", true), ("", false), ("not-json", true)] {
            let (state, candidate, _, server) = fixture(false, "compaction").await;
            let mut original = request();
            original["input"].as_array_mut().unwrap().push(
                json!({"type":"function_call","call_id":"c","name":"lookup","arguments":arguments}),
            );
            if completed {
                original["input"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"type":"function_call_output","call_id":"c","output":"done"}));
            }
            let result = native_anthropic_compact_candidate(
                &state,
                &HeaderMap::new(),
                &original,
                &candidate,
                CompactionRequestKind::NativeTrigger,
            )
            .await;
            if completed && arguments.is_empty() {
                let (value, _, _) = result.unwrap();
                let checkpoint = native::decode(&value["output"][0]).unwrap().unwrap();
                assert!(checkpoint
                    .retained
                    .iter()
                    .any(|item| item["type"] == "function_call" && item["arguments"] == "{}"));
            } else {
                assert!(result.is_err());
            }
            server.abort();
        }
    }
    #[tokio::test]
    async fn cache_health_anthropic_retry_delta_input_updates_keep_billing_sum_and_final_measurement(
    ) {
        for mode in [
            "empty-anthropic-stream",
            "empty-anthropic-stream-nonterminal",
        ] {
            let (state, mut candidate, capture, server) = fixture(false, mode).await;
            candidate.provider.empty_completion_retry_models = vec!["*".into()];
            candidate.provider.limits.empty_completion_retries = 1;
            state.settings.write().await.providers = vec![candidate.provider.clone()];
            let fresh = state
                .routing
                .lock()
                .await
                .candidates_for_request(
                    &*state.settings.read().await,
                    "anthropic/claude-sonnet-5-5",
                    false,
                    None,
                )
                .unwrap()
                .remove(0);
            candidate.routing_epoch = fresh.routing_epoch;
            candidate.routing_generation = fresh.routing_generation;
            let mut body = request();
            body["stream"] = json!(true);
            let mut headers = HeaderMap::new();
                headers.insert("session_id", HeaderValue::from_static("01a13ddb-8d21-7cb2-8f00-23a8a09af079"));
                let upstream = send_candidate(&state, &mut body, &candidate, Some(&headers))
                .await
                .unwrap();
            let shared = upstream
                .extensions()
                .get::<SharedProviderRetryObservation>()
                .unwrap()
                .clone();
            let ledger = ObservabilityLedger::new(None);
                ledger.configure_cache_monitoring(true).await;
                assert!(ledger.cache_health_summary().monitoring);
            let mut seed = ObservationSeed::for_candidate(
                ledger.clone(),
                ObservabilitySettings { request_log: true, usage_log: true, ..Default::default() },
                "anthropic-sparse-retry".into(),
                true,
                Instant::now(),
                1,
                &candidate,
            );
            seed.request_diagnostic = Some(crate::observability::RequestDiagnostic {
                client_thread_id: Some("01a13ddb-8d21-7cb2-8f00-23a8a09af079".into()),
                ..Default::default()
            });
            seed.record_shared_provider_retries(shared);
            let response = adapt_successful_response(
                upstream,
                &candidate,
                seed,
                ResponseToolMap::default(),
                &state.response_state,
                &body,
                false,
                false,
            )
            .await;
            let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
            assert!(String::from_utf8_lossy(&bytes).contains("response.completed"));
            assert_eq!(capture.lock().await.len(), 2);
            assert_eq!(ledger.summary().input_tokens, 291_500);
            assert_eq!(ledger.cache_health_summary().sessions.len(), 1);
                let row = &ledger.cache_health_summary().sessions[0];
            assert_eq!(row.last_input_tokens, 191_000);
            assert_eq!(row.cached_input_tokens, 188_000);
            server.abort();
        }
    }
    #[tokio::test]
    async fn native_anthropic_large_image_suffix_uses_full_summary_before_encoding_or_billing_failure(
    ) {
        let (state, candidate, capture, server) = fixture(false, "compaction").await;
        let mut original = request();
        original["input"].as_array_mut().unwrap().extend([json!({"type":"function_call","call_id":"image","name":"lookup","arguments":"{}"}),json!({"type":"function_call_output","call_id":"image","output":[{"type":"input_image","image_url":format!("data:image/jpeg;base64,{}","A".repeat(2_200_000))}]})]);
        let (value, _, _) = native_anthropic_compact_candidate(
            &state,
            &HeaderMap::new(),
            &original,
            &candidate,
            CompactionRequestKind::NativeTrigger,
        )
        .await
        .unwrap();
        let checkpoint = native::decode(&value["output"][0]).unwrap().unwrap();
        assert_eq!(checkpoint.retained, vec![original["input"][2].clone()]);
        let captured = capture.lock().await;
        assert_eq!(captured.len(), 1);
        assert_eq!(count_translated_input_images(&captured[0].0), 1);
        server.abort();
    }
    #[tokio::test]
    async fn native_anthropic_replay_reservation_uses_same_output_clamp_as_normal_model_policy() {
        let (state, mut candidate, _, server) = fixture(false, "compaction").await;
        candidate.max_output_tokens = Some(64_000);
        candidate.context_window = Some(200_000);
        let mut original = request();
        original["max_output_tokens"] = json!(200_000);
        let mut normal = original.clone();
        apply_candidate_model_policy(&mut normal, &candidate, false).unwrap();
        assert_eq!(normal["max_output_tokens"], 64_000);
        let result = native_anthropic_compact_candidate(
            &state,
            &HeaderMap::new(),
            &original,
            &candidate,
            CompactionRequestKind::NativeTrigger,
        )
        .await;
        assert!(result.is_ok());
        server.abort();
    }
    #[tokio::test]
    async fn cache_health_cloud_code_assist_retry_writes_nested_billing_without_fabricating_cache_reads(
    ) {
        for reported in [false, true] {
            for footer in [false, true] {
                let (state, mut candidate, _, unused) = fixture(false, "compaction").await;
                unused.abort();
                let count = Arc::new(AtomicU32::new(0));
                let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let router=Router::new().fallback(post({let count=count.clone();let captured=captured.clone();move |Json(body):Json<Value>| {let count=count.clone();let captured=captured.clone();async move {
                captured.lock().await.push(body);let n=count.fetch_add(1,Ordering::Relaxed);
                let mut metadata=json!({"promptTokenCount":100_000,"candidatesTokenCount":if n==0 {0} else {1},"totalTokenCount":if n==0 {100_000} else {100_001}});if reported {metadata["cachedContentTokenCount"]=json!(99_000);}
                let mut value=json!({"response":{"candidates":[{"content":{"role":"model","parts":if n==0 {vec![]} else {vec![json!({"text":"ok"})]}},"finishReason":"STOP"}],"usageMetadata":metadata}});
                let mut wire=String::new();let metadata=if footer {value["response"].as_object_mut().unwrap().remove("usageMetadata")} else {None};
                wire.push_str(&format!("data: {}\n\n",value));if let Some(metadata)=metadata {wire.push_str(&format!("data: {}\n\n",json!({"response":{"usageMetadata":metadata}})));}
                Response::builder().status(200).header(header::CONTENT_TYPE,"text/event-stream").body(Body::from(wire)).unwrap()
            }}}));
                let server = tokio::spawn(async move {
                    axum::serve(listener, router).await.unwrap();
                });
                candidate.provider.id = "cca-fixture".into();
                candidate.provider.protocol = ProviderProtocol::GeminiGenerateContent;
                candidate.provider.google_mode = GoogleMode::CloudCodeAssist;
                candidate.provider.project = Some("fixture-project".into());
                candidate.provider.base_url = format!("http://{address}");
                candidate.provider.models = vec!["gemini-test".into()];
                candidate.provider.empty_completion_retry_models = vec!["*".into()];
                candidate.provider.limits.empty_completion_retries = 1;
                state.settings.write().await.providers = vec![candidate.provider.clone()];
                candidate = state
                    .routing
                    .lock()
                    .await
                    .candidates_for_request(
                        &*state.settings.read().await,
                        "cca-fixture/gemini-test",
                        false,
                        None,
                    )
                    .unwrap()
                    .remove(0);
                let mut body = json!({"model":"cca-fixture/gemini-test","input":[{"type":"message","role":"user","content":"fixture"}],"stream":true});
                let mut headers = HeaderMap::new();
                headers.insert("session_id", HeaderValue::from_static("01a13ddb-8d21-7cb2-8f00-23a8a09af079"));
                let upstream = send_candidate(&state, &mut body, &candidate, Some(&headers))
                    .await
                    .unwrap();
                let shared = upstream
                    .extensions()
                    .get::<SharedProviderRetryObservation>()
                    .unwrap()
                    .clone();
                let ledger = ObservabilityLedger::new(None);
                ledger.configure_cache_monitoring(true).await;
                assert!(ledger.cache_health_summary().monitoring);
                let mut seed = ObservationSeed::for_candidate(
                    ledger.clone(),
                    ObservabilitySettings { request_log: true, usage_log: true, ..Default::default() },
                    "cca-retry".into(),
                    true,
                    Instant::now(),
                    1,
                    &candidate,
                );
                seed.request_diagnostic = Some(crate::observability::RequestDiagnostic {
                    client_thread_id: Some("01a13ddb-8d21-7cb2-8f00-23a8a09af079".into()),
                    ..Default::default()
                });
                seed.record_shared_provider_retries(shared);
                let response = adapt_successful_response(
                    upstream,
                    &candidate,
                    seed,
                    ResponseToolMap::default(),
                    &state.response_state,
                    &body,
                    false,
                    false,
                )
                .await;
                let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
                assert!(
                    String::from_utf8_lossy(&bytes).contains("response.completed"),
                    "CCA fixture output: {}; calls={}",
                    String::from_utf8_lossy(&bytes),
                    count.load(Ordering::Relaxed)
                );
                assert_eq!(count.load(Ordering::Relaxed), 2);
                assert!(captured.lock().await[0].get("request").is_some());
                assert_eq!(ledger.summary().input_tokens, 200_000);
                assert_eq!(ledger.cache_health_summary().sessions.len(), 1);
                let row = &ledger.cache_health_summary().sessions[0];
                assert_eq!(row.last_input_tokens, 100_000);
                assert_eq!(row.unknown_cache_requests, if reported { 0 } else { 1 });
                assert_eq!(row.cached_input_tokens, if reported { 99_000 } else { 0 });
                server.abort();
            }
        }
    }
    #[tokio::test]
    async fn synthetic_failed_repair_retains_every_reported_summary_call_usage() {
        let (state, candidate, capture, server) = fixture(false, "compaction").await;
        let failure = synthetic_compact_candidate(
            &state,
            &HeaderMap::new(),
            &request(),
            &candidate,
            CompactionRequestKind::NativeTrigger,
        )
        .await
        .unwrap_err();
        assert_eq!(capture.lock().await.len(), 2);
        let known = failure
            .response
            .extensions()
            .get::<crate::observability::ReportedUpstreamUsage>()
            .expect("known summary calls");
        assert_eq!(known.0.input_tokens, 398);
        let ledger = ObservabilityLedger::new(None);
        let mut seed = ObservationSeed::for_candidate(
            ledger.clone(),
            ObservabilitySettings::default(),
            "summary-repair-failure".into(),
            false,
            Instant::now(),
            1,
            &candidate,
        );
        seed.record_upstream_error(&failure.response);
        if let Some(retry) = failure
            .response
            .extensions()
            .get::<ProviderRetryObservation>()
        {
            seed.record_provider_retries(retry);
        }
        seed.finish(
            failure.response.status(),
            Some("invalid_compaction_response"),
            TokenUsage::default(),
        );
        assert_eq!(ledger.summary().input_tokens, 398);
        assert_eq!(ledger.summary().failed_requests, 1);
        server.abort();
    }
    #[tokio::test]
    async fn cancellation_between_empty_attempt_and_retry_keeps_known_billing_without_extra_send() {
        let (state, mut candidate, capture, server) =
            fixture(false, "empty-anthropic-stream").await;
        candidate.provider.empty_completion_retry_models = vec!["*".into()];
        candidate.provider.limits.empty_completion_retries = 1;
        state.settings.write().await.providers = vec![candidate.provider.clone()];
        let fresh = state
            .routing
            .lock()
            .await
            .candidates_for_request(
                &*state.settings.read().await,
                "anthropic/claude-sonnet-5-5",
                false,
                None,
            )
            .unwrap()
            .remove(0);
        candidate.routing_epoch = fresh.routing_epoch;
        candidate.routing_generation = fresh.routing_generation;
        let mut request = request();
        request["stream"] = json!(true);
        let upstream = send_candidate(&state, &mut request, &candidate, None)
            .await
            .unwrap();
        let shared = upstream
            .extensions()
            .get::<SharedProviderRetryObservation>()
            .unwrap()
            .clone();
        let ledger = ObservabilityLedger::new(None);
        let mut seed = ObservationSeed::for_candidate(
            ledger.clone(),
            ObservabilitySettings::default(),
            "cancel-before-retry".into(),
            true,
            Instant::now(),
            1,
            &candidate,
        );
        seed.record_shared_provider_retries(shared.clone());
        let completion = StreamObservation::new(seed);
        let mut bytes = upstream.bytes_stream();
        loop {
            let chunk = tokio::time::timeout(Duration::from_secs(2), bytes.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if chunk.as_ref() == EMPTY_COMPLETION_RECOVERY_MARKER {
                break;
            }
        }
        assert_eq!(shared.0.lock().unwrap().additional_sends, 0);
        drop(bytes);
        drop(completion);
        assert_eq!(capture.lock().await.len(), 1);
        assert_eq!(ledger.summary().failed_requests, 1);
        assert_eq!(ledger.summary().input_tokens, 100_500);
        server.abort();
    }
}


async fn read_native_compaction_json(upstream: reqwest::Response, limit: u64) -> Result<Value, AttemptFailure> {
    let retry = upstream.extensions().get::<ProviderRetryObservation>().cloned();
    let diagnostic = upstream.extensions().get::<crate::observability::RequestDiagnostic>().cloned();
    super::stream::bounded_json_with_usage(upstream, limit).await.map_err(|(message, usage)| {
        let mut failure = failure_with_provider_metadata(request_failure("invalid_compaction_response", &message), retry.as_ref(), diagnostic.as_ref());
        failure.response.extensions_mut().insert(crate::observability::ReportedUpstreamUsage(usage));
        failure
    })
}

#[cfg(test)]
mod review11_native_failure_tests {
    use super::*;
    #[tokio::test]
    async fn native_read_and_json_failures_preserve_prior_retry_metadata() {
        for (body, limit) in [("not JSON", 65536), ("oversized", 4)] {
            let mut upstream = reqwest::Response::from(Response::builder().status(200)
                .body(reqwest::Body::from(body)).unwrap());
            upstream.extensions_mut().insert(ProviderRetryObservation {
                usage: TokenUsage { input_tokens: 123, output_tokens: 7, total_tokens: 130, ..Default::default() },
                additional_sends: 2, recovery_kinds: vec!["provider-5xx".into()], ..Default::default()
            });
            upstream.extensions_mut().insert(crate::observability::RequestDiagnostic { wire_bytes: 999, ..Default::default() });
            let failure = read_native_compaction_json(upstream, limit).await.unwrap_err();
            let retry = failure.response.extensions().get::<ProviderRetryObservation>().unwrap();
            assert_eq!(retry.usage.input_tokens, 123);
            assert_eq!(retry.usage.output_tokens, 7);
            assert_eq!(retry.additional_sends, 2);
            assert_eq!(failure.response.extensions().get::<crate::observability::RequestDiagnostic>().unwrap().wire_bytes, 999);
        }
    }
}

/// Before auth resolution, only defer expansion. This is not permission to send
/// a signature: every physical send checks the resolved authentication below.
pub(crate) fn native_anthropic_route(candidate: &RouteCandidate) -> bool {
    candidate.provider.transport == ProviderTransport::Standard
        && candidate.provider.protocol_for_model(&candidate.upstream_model) == ProviderProtocol::AnthropicMessages
}

pub(crate) fn resolved_anthropic_checkpoint_scope(candidate: &RouteCandidate, request: &reqwest::Request) -> Option<String> {
    if !native_anthropic_route(candidate) || !matches!(
        candidate.credential.as_ref().unwrap_or(&candidate.provider.credential).source,
        CredentialSource::Environment | CredentialSource::Keychain | CredentialSource::Command | CredentialSource::OAuth
    ) { return None; }
    if request.headers().is_empty() && request.url().query().is_none() { return None; }
    Some(anthropic_checkpoint_scope(candidate, &diagnostics::resolved_auth_binding(request)))
}

#[cfg(test)]
pub(super) mod review13_resolved_auth_regressions {
    use super::*;
    use crate::ProviderDefinition;

    pub(crate) fn candidate() -> RouteCandidate {
        let mut provider = ProviderDefinition::default();
        provider.id = "r13-fixture".into();
        provider.base_url = "http://127.0.0.1:1".into();
        provider.protocol = ProviderProtocol::AnthropicMessages;
        provider.credential.source = CredentialSource::Environment;
        provider.credential.reference = Some("CODETAS_R13_BINDING_UNIT_CREDENTIAL".into());
        RouteCandidate {
            capabilities: provider.capabilities.clone(), provider,
            upstream_model: "claude-sonnet-5-5".into(), exposed_model: "test".into(),
            credential: None, account_id: Some("unchanged-account-label".into()),
            target_key: "r13/test".into(), route_id: None, failure_threshold: 3,
            quota_threshold_percent: 90, input_price_per_million: None, output_price_per_million: None,
            context_window: None, max_input_tokens: None, max_output_tokens: None,
            reasoning_efforts: Vec::new(), default_reasoning_effort: None,
            routing_epoch: 0, routing_generation: 0, session_scope: None,
        }
    }

    #[tokio::test]
    async fn review13_resolved_auth_preserves_same_credential_and_downgrades_rotation() {
        let mut candidate = candidate();
        let client = reqwest::Client::new();
        let endpoint = candidate.provider.endpoint_for_model_streaming(&candidate.upstream_model, false);
        std::env::set_var("CODETAS_R13_BINDING_UNIT_CREDENTIAL", "r13-fixed-account-a");
        let first = apply_provider_auth(client.post(&endpoint), &candidate.provider, None).await.unwrap().build().unwrap();
        let scope = resolved_anthropic_checkpoint_scope(&candidate, &first).unwrap();
        let checkpoint = native::Checkpoint { version: 1, scope: scope.clone(),
            context_hash: native::digest(&json!({})),
            block: json!({"type":"compaction","content":"keep task facts","signature":"test-only-signature"}),
            retained: vec![json!({"role":"user","content":"keep latest request"})],
        };
        let original = json!({"input":[native::encode(&checkpoint).unwrap()]});
        let mut same = original.clone();
        let again = apply_provider_auth(client.post(&endpoint), &candidate.provider, None).await.unwrap().build().unwrap();
        let same_scope = resolved_anthropic_checkpoint_scope(&candidate, &again).unwrap();
        assert_eq!(scope, same_scope);
        prepare_native_anthropic_history(&mut same, &candidate, Some(&same_scope)).unwrap();
        assert_eq!(same, original);
        std::env::set_var("CODETAS_R13_BINDING_UNIT_CREDENTIAL", "r13-fixed-account-b");
        let rotated = apply_provider_auth(client.post(&endpoint), &candidate.provider, None).await.unwrap().build().unwrap();
        for source in [CredentialSource::Environment, CredentialSource::Keychain, CredentialSource::Command, CredentialSource::OAuth] {
            candidate.provider.credential.source = source;
            let old = resolved_anthropic_checkpoint_scope(&candidate, &first).unwrap();
            let new = resolved_anthropic_checkpoint_scope(&candidate, &rotated).unwrap();
            assert_ne!(old, new);
            let mut checkpoint = checkpoint.clone(); checkpoint.scope = old;
            let mut changed = json!({"input":[native::encode(&checkpoint).unwrap()]});
            prepare_native_anthropic_history(&mut changed, &candidate, Some(&new)).unwrap();
            assert!(!changed.to_string().contains("test-only-signature"));
            assert!(!changed["input"].as_array().unwrap().iter().any(native::is_item));
            assert!(changed.to_string().contains("keep task facts"));
            assert!(changed.to_string().contains("keep latest request"));
        }
        for source in [CredentialSource::None, CredentialSource::Forward] {
            candidate.provider.credential.source = source;
            assert!(resolved_anthropic_checkpoint_scope(&candidate, &first).is_none());
            let mut body = original.clone();
            prepare_native_anthropic_history(&mut body, &candidate, None).unwrap();
            assert!(!body["input"].as_array().unwrap().iter().any(native::is_item));
        }
        candidate.provider.credential.source = CredentialSource::Environment;
        let unauthenticated = client.post(endpoint).build().unwrap();
        assert!(resolved_anthropic_checkpoint_scope(&candidate, &unauthenticated).is_none());
        std::env::remove_var("CODETAS_R13_BINDING_UNIT_CREDENTIAL");
    }
}

#[cfg(test)]
mod review14_checkpoint_regressions {
    use super::*;
    #[test]
    fn review14_forward_defers_then_downgrades_suffix_thinking_only_with_checkpoint() {
        let mut candidate = super::review13_resolved_auth_regressions::candidate();
        candidate.provider.credential.source = CredentialSource::Forward;
        assert!(native_anthropic_route(&candidate));
        let thinking = json!({"type":"reasoning","summary":[{"type":"summary_text","text":"visible reasoning"}],
            "provider_metadata":{"anthropic":{"thinking_blocks":[{"type":"thinking","thinking":"old","signature":"old-signature"}]}}});
        let checkpoint = native::Checkpoint {version:1, scope:"a".repeat(64), context_hash:"b".repeat(64),
            block:json!({"type":"compaction","content":"task facts","signature":"checkpoint-signature"}), retained:vec![thinking.clone()]};
        let mut body = json!({"input":[native::encode(&checkpoint).unwrap(),thinking.clone(),{"role":"user","content":"continue"}]});
        apply_provider_request_compatibility(&mut body,&candidate,ProviderProtocol::AnthropicMessages,false);
        assert!(body["input"].as_array().unwrap().iter().any(native::is_item));
        prepare_native_anthropic_history(&mut body,&candidate,None).unwrap();
        assert!(!body.to_string().contains("old-signature"));
        assert!(body.to_string().contains("visible reasoning"));
        assert!(body.to_string().contains("task facts"));
        let mut fresh = json!({"input":[thinking]});
        let original = fresh.clone();
        prepare_native_anthropic_history(&mut fresh,&candidate,None).unwrap();
        assert_eq!(fresh,original);
    }
    #[test]
    fn review14_non_anthropic_downgrade_clears_signatures_on_both_sides() {
        let thinking = json!({"type":"reasoning","provider_metadata":{"anthropic":{"thinking_blocks":[{"signature":"old"}]}}});
        let checkpoint = native::Checkpoint {version:1,scope:"a".repeat(64),context_hash:"b".repeat(64),
            block:json!({"type":"compaction","content":"facts","signature":"sig"}),retained:vec![]};
        let mut body=json!({"input":[thinking.clone(),native::encode(&checkpoint).unwrap(),thinking]});
        crate::compaction::expand_translated_compactions(&mut body);
        assert!(body["input"].as_array().unwrap().iter().all(|item| item.pointer("/provider_metadata/anthropic").is_none()));
    }
}

/// Generation-time evidence carried with reasoning, never authentication material.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThinkingProvenance {
    version: u8,
    scope: Option<String>,
    context_hash: String,
    boundary: String,
}
impl ThinkingProvenance {
    fn permits(&self, metadata: &Value) -> bool {
        match metadata.get("generation_provenance") {
            Some(value) => serde_json::from_value::<Self>(value.clone()).ok()
                .is_some_and(|proof| self.scope.is_some() && proof == *self),
            None => self.boundary == "plain",
        }
    }

    pub(crate) fn for_wire(wire: &Value, scope: Option<&str>, had_checkpoint: bool) -> Self {
        let block = wire.get("messages").and_then(Value::as_array).into_iter().flatten()
            .flat_map(|m| m.get("content").and_then(Value::as_array).into_iter().flatten())
            .find(|block| block.get("type").and_then(Value::as_str) == Some("compaction"));
        Self { version: 1, scope: scope.map(str::to_owned), context_hash: native::context_hash(wire),
            boundary: block.map(|block| format!("native:{}", native::digest(block)))
                .unwrap_or_else(|| if had_checkpoint { "downgraded" } else { "plain" }.into()) }
    }

    pub(crate) fn annotate(&self, value: &mut Value) {
        if value.get("type").and_then(Value::as_str) == Some("reasoning") {
            if let Some(metadata) = value.pointer_mut("/provider_metadata/anthropic").and_then(Value::as_object_mut) {
                metadata.insert("generation_provenance".into(), serde_json::to_value(self).expect("provenance JSON"));
            }
        }
        // Only response/output lifecycle slots, never tools or arbitrary user data.
        if let Some(items) = value.get_mut("output").and_then(Value::as_array_mut) {
            for item in items { self.annotate(item); }
        }
        for key in ["item", "response"] {
            if let Some(item) = value.get_mut(key) { self.annotate(item); }
        }
    }

    pub(crate) fn annotate_sse(&self, event: String) -> String {
        let Some(data) = event.lines().find_map(|line| line.strip_prefix("data: ")) else { return event; };
        let Ok(mut value) = serde_json::from_str::<Value>(data) else { return event; };
        self.annotate(&mut value);
        sse(value.get("type").and_then(Value::as_str).unwrap_or("message"), &value)
    }
}

/// Compare each signed block against its own generation evidence. A checkpoint's
/// context hash cannot authenticate thinking generated later on another route.
pub(crate) fn filter_wire_thinking(wire: &mut Value, source: &Value, expected: &ThinkingProvenance) {
    let mut permitted = Vec::<Value>::new();
    for item in source.get("input").and_then(Value::as_array).into_iter().flatten() {
        let Some(metadata) = item.pointer("/provider_metadata/anthropic") else { continue; };
        let valid = expected.permits(metadata);
        if valid {
            permitted.extend(metadata.get("thinking_blocks").and_then(Value::as_array).into_iter().flatten().cloned());
        }
    }
    if let Some(messages) = wire.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages.iter_mut() {
            if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
                blocks.retain(|block| !matches!(block.get("type").and_then(Value::as_str), Some("thinking" | "redacted_thinking"))
                    || permitted.iter().any(|allowed| allowed == block));
            }
        }
        messages.retain(|m| m.get("content").and_then(Value::as_array).is_none_or(|blocks| !blocks.is_empty()));
    }
}

pub(crate) fn current_thinking_provenance(initial: &Option<ThinkingProvenance>, shared: &Option<SharedProviderRetryObservation>) -> Option<ThinkingProvenance> {
    shared.as_ref().and_then(|shared| shared.0.lock().ok().and_then(|state| state.thinking_provenance.clone()))
        .or_else(|| initial.clone())
}


#[cfg(test)]
mod review15_thinking_regressions {
    use super::*;

    fn wire(source: &Value, scope: &str) -> (Value, ThinkingProvenance) {
        let candidate = review13_resolved_auth_regressions::candidate();
        let had_checkpoint = source["input"].as_array().unwrap().iter().any(native::is_item);
        let mut prepared = source.clone();
        native::expand_retained(&mut prepared).unwrap();
        prepare_native_anthropic_history(&mut prepared, &candidate, Some(scope)).unwrap();
        crate::translate::prepare_translated_responses_request_with_native_anthropic(
            &mut prepared, true, true, true,
        );
        let mut wire = responses_to_anthropic_with_oauth(&prepared, &candidate.upstream_model, false).unwrap();
        apply_provider_wire_compatibility(&mut wire, &prepared, &candidate, ProviderProtocol::AnthropicMessages).unwrap();
        let proof = ThinkingProvenance::for_wire(&wire, Some(scope), had_checkpoint);
        filter_wire_thinking(&mut wire, &prepared, &proof);
        (wire, proof)
    }

    fn source() -> Value {
        let mut source = json!({"model":"test","store":false,"instructions":"Keep task facts.",
            "reasoning":{"effort":"high"},"input":[{"type":"message","role":"user","content":"continue"}]});
        let (base, _) = wire(&source, &"a".repeat(64));
        let checkpoint = native::Checkpoint {
            version: 1, scope: "a".repeat(64), context_hash: native::context_hash(&base),
            block: json!({"type":"compaction","content":"task facts","signature":"checkpoint-a"}),
            retained: vec![],
        };
        source["input"].as_array_mut().unwrap().insert(0, native::encode(&checkpoint).unwrap());
        source[native::HISTORY_REWRITTEN] = json!(false);
        source
    }

    fn provider_json(signature: &str) -> Value {
        json!({"id":"fixture-message","type":"message","role":"assistant","stop_reason":"end_turn",
            "content":[{"type":"thinking","thinking":"fixture thought","signature":signature},
                {"type":"redacted_thinking","data":format!("redacted-{signature}")},
                {"type":"text","text":"fixture answer"}],
            "usage":{"input_tokens":100,"output_tokens":8}})
    }

    fn provider_stream(signature: &str) -> String {
        let events = vec![
            json!({"type":"message_start","message":{"id":"fixture-message","role":"assistant","content":[],"usage":{"input_tokens":100,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"fixture thought"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":signature}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":format!("redacted-{signature}")}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"fixture answer"}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":8}}),
            json!({"type":"message_stop"}),
        ];
        events.iter().map(|event| sse(event["type"].as_str().unwrap(), event)).collect()
    }

    async fn adapt(source: &Value, scope: &str, signature: &str, streaming: bool) -> (Value, Vec<Value>, Arc<ResponseStateStore>) {
        let (_, proof) = wire(source, scope);
        let payload = if streaming { provider_stream(signature) } else { provider_json(signature).to_string() };
        let mut upstream = reqwest::Response::from(Response::builder().status(200)
            .header(header::CONTENT_TYPE, if streaming { "text/event-stream" } else { "application/json" })
            .body(reqwest::Body::from(payload)).unwrap());
        let initial = if streaming { wire(source, &"a".repeat(64)).1 } else { proof.clone() };
        upstream.extensions_mut().insert(initial);
        // Exercise the same shared provenance lookup as guarded retry streams,
        // including an account change after the initial response was wrapped.
        let shared = SharedProviderRetryObservation::default();
        shared.0.lock().unwrap().thinking_provenance = Some(proof);
        upstream.extensions_mut().insert(shared);
        let mut candidate = review13_resolved_auth_regressions::candidate();
        candidate.capabilities.streaming = true;
        let store = Arc::new(ResponseStateStore::new(None));
        let observation = ObservationSeed::for_candidate(ObservabilityLedger::new(None),
            ObservabilitySettings::default(), "thinking-regression".into(), streaming, Instant::now(), 1, &candidate);
        let response = adapt_successful_response(upstream, &candidate, observation, response_tool_map(source),
            &store, source, true, false).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        if streaming {
            let events = super::super::stream::drain_sse_values(&mut Vec::new(), &bytes).unwrap();
            let terminal = events.iter().find(|event| event["type"] == "response.completed").expect("completed event")["response"].clone();
            (terminal, events, store)
        } else {
            (serde_json::from_slice(&bytes).unwrap(), vec![], store)
        }
    }

    fn assert_replay(source: &Value, response: &Value, events: &[Value], store: &ResponseStateStore, scope: &str, signature: &str, keep: bool) {
        let reasoning = response["output"].as_array().unwrap().iter().find(|item| item["type"] == "reasoning").unwrap();
        let proof = reasoning.pointer("/provider_metadata/anthropic/generation_provenance").expect("generation evidence");
        assert_eq!(proof["scope"], scope);
        assert_eq!(proof["boundary"].as_str().unwrap().starts_with("native:"), keep);
        for event in events.iter().filter(|event| event["type"] == "response.output_item.done" && event["item"]["type"] == "reasoning") {
            assert_eq!(event["item"].pointer("/provider_metadata/anthropic/generation_provenance"), Some(proof));
            assert_eq!(event["item"]["provider_metadata"], reasoning["provider_metadata"]);
        }
        if !events.is_empty() {
            assert!(events.iter().any(|event| event["type"] == "response.output_item.done" && event["item"]["type"] == "reasoning"));
        }
        let mut echo = source.clone();
        echo["input"].as_array_mut().unwrap().extend(response["output"].as_array().unwrap().clone());
        echo["input"].as_array_mut().unwrap().push(json!({"type":"message","role":"user","content":"next"}));
        let mut saved = json!({"model":"test","instructions":"Keep task facts.","reasoning":{"effort":"high"},
            "previous_response_id":response["id"],"input":[{"type":"message","role":"user","content":"next"}]});
        assert!(store.expand_previous_response_input_with_hint(&mut saved, None).expanded());
        let saved_reasoning = saved["input"].as_array().unwrap().iter().find(|item| item["type"] == "reasoning").unwrap();
        assert_eq!(saved_reasoning["provider_metadata"], reasoning["provider_metadata"]);
        for replay in [echo, saved] {
            let (wire, _) = wire(&replay, &"a".repeat(64));
            let encoded = wire.to_string();
            assert_eq!(encoded.contains(signature), keep, "A return must reject only mismatched thinking");
            assert_eq!(encoded.contains(&format!("redacted-{signature}")), keep);
            assert!(encoded.contains("checkpoint-a"));
            assert!(encoded.contains("fixture answer"));
            assert!(!encoded.contains("generation_provenance"));
            assert!(!encoded.contains("__codetas"));
        }
    }

    #[tokio::test]
    async fn review15_json_downgrade_return_and_matching_thinking_replay() {
        for (scope, signature, keep) in [("b", "thinking-b", false), ("a", "thinking-a", true)] {
            let source = source();
            let scope = scope.repeat(64);
            let (response, events, store) = adapt(&source, &scope, signature, false).await;
            assert_replay(&source, &response, &events, &store, &scope, signature, keep);
        }
    }

    #[tokio::test]
    async fn review15_stream_done_final_and_saved_provenance_agree() {
        for (scope, signature, keep) in [("b", "stream-thinking-b", false), ("a", "stream-thinking-a", true)] {
            let mut source = source(); source["stream"] = json!(true);
            let scope = scope.repeat(64);
            let (response, events, store) = adapt(&source, &scope, signature, true).await;
            assert_replay(&source, &response, &events, &store, &scope, signature, keep);
        }
    }

    #[tokio::test]
    async fn review15_plain_new_thinking_is_retained_on_matching_route() {
        let source = json!({"model":"test","input":[{"type":"message","role":"user","content":"continue"}]});
        let scope = "a".repeat(64);
        let (response, _, _) = adapt(&source, &scope, "plain-new-thinking", false).await;
        let mut replay = source.clone();
        replay["input"].as_array_mut().unwrap().extend(response["output"].as_array().unwrap().clone());
        let (wire, _) = wire(&replay, &scope);
        assert!(wire.to_string().contains("plain-new-thinking"));
        assert!(!wire.to_string().contains("generation_provenance"));
    }
}


#[cfg(test)]
mod review16_thinking_regressions {
    use super::*;
    fn wire(source: &Value, c: &RouteCandidate) -> Value {
        let mut wire = responses_to_anthropic_with_oauth(source, &c.upstream_model, false).unwrap();
        apply_provider_wire_compatibility(&mut wire, source, c, ProviderProtocol::AnthropicMessages).unwrap(); wire
    }
    fn thinking(proof: Option<&ThinkingProvenance>, signature: &str) -> Value {
        let mut value=json!({"type":"reasoning","summary":[],"provider_metadata":{"anthropic":{"thinking_blocks":[{"type":"thinking","thinking":"work","signature":signature}]}}});
        if let Some(proof)=proof { proof.annotate(&mut value); } value
    }
    #[test]
    fn review16_native_a_downgrade_b_repeat_b_return_a_preserves_only_matching_thinking() {
        let c=review13_resolved_auth_regressions::candidate();
        let a="a".repeat(64); let b="b".repeat(64);
        let checkpoint=native::Checkpoint { version:1,scope:a.clone(),context_hash:native::context_hash(&json!({})),block:json!({"type":"compaction","content":"task facts","signature":"checkpoint-signature"}),retained:vec![] };
        let original=json!({"input":[native::encode(&checkpoint).unwrap(),{"role":"user","content":"continue"}]});
        let a_proof=ThinkingProvenance::for_wire(&wire(&original,&c),Some(&a),true);
        let mut first_b=original.clone(); first_b["input"].as_array_mut().unwrap().push(thinking(Some(&a_proof),"old-a"));
        prepare_native_anthropic_history(&mut first_b,&c,Some(&b)).unwrap();
        assert!(!first_b.to_string().contains("old-a"));
        let b_proof=ThinkingProvenance::for_wire(&wire(&first_b,&c),Some(&b),true);
        let mut replay=original.clone();
        replay["input"].as_array_mut().unwrap().extend([thinking(Some(&a_proof),"normal-a"),thinking(Some(&b_proof),"new-b"),thinking(None,"unknown")]);
        let mut again_b=replay.clone(); prepare_native_anthropic_history(&mut again_b,&c,Some(&b)).unwrap();
        assert!(again_b.to_string().contains("new-b")); assert!(!again_b.to_string().contains("normal-a")); assert!(!again_b.to_string().contains("unknown"));
        let mut b_wire=wire(&again_b,&c); filter_wire_thinking(&mut b_wire,&again_b,&b_proof);
        assert!(b_wire.to_string().contains("new-b")); assert!(!b_wire.to_string().contains("generation_provenance"));
        let mut back_a=replay; prepare_native_anthropic_history(&mut back_a,&c,Some(&a)).unwrap();
        let mut a_wire=wire(&back_a,&c); filter_wire_thinking(&mut a_wire,&back_a,&a_proof);
        assert!(a_wire.to_string().contains("normal-a")); assert!(!a_wire.to_string().contains("new-b")); assert!(!a_wire.to_string().contains("unknown"));
        assert_eq!(native::decode(&back_a["input"][0]).unwrap().unwrap().block,checkpoint.block);
        let mut plain=json!({"input":[{"role":"user","content":"plain"}]});
        let plain_proof=ThinkingProvenance::for_wire(&wire(&plain,&c),Some(&b),false);
        plain["input"].as_array_mut().unwrap().push(thinking(Some(&plain_proof),"plain-b"));
        prepare_native_anthropic_history(&mut plain,&c,Some(&b)).unwrap();
        let mut plain_wire=wire(&plain,&c);filter_wire_thinking(&mut plain_wire,&plain,&plain_proof);
        assert!(plain_wire.to_string().contains("plain-b"));
        let mut changed=plain_proof.clone();changed.context_hash="c".repeat(64);
        filter_wire_thinking(&mut plain_wire,&plain,&changed); assert!(!plain_wire.to_string().contains("plain-b"));
    }
}


#[cfg(test)]
mod review17_native_usage_regressions {
    use super::*;
    use super::super::adapt::review16_regressions::{candidate, response, seed};

    fn value() -> Value {
        json!({"type":"message","stop_reason":"compaction","content":[{"type":"compaction",
            "content":"fixture summary","signature":"fixture-signature"}],
            "usage":{"input_tokens":0,"output_tokens":0,"iterations":[
                {"input_tokens":17,"cache_read_input_tokens":80,"cache_creation_input_tokens":3,
                    "output_tokens":7,"output_tokens_details":{"reasoning_tokens":2},"total_tokens":120},
                {"input_tokens":5,"cache_read_input_tokens":10,"cache_creation_input_tokens":2,
                    "output_tokens":3,"output_tokens_details":{"reasoning_tokens":1}}
            ]}})
    }
    fn check(usage: &TokenUsage) {
        assert_eq!(usage.input_tokens, 117);
        assert_eq!(usage.output_tokens, 10);
        assert_eq!(usage.cached_input_tokens, 90);
        assert_eq!(usage.cache_creation_input_tokens, 5);
        assert_eq!(usage.reasoning_tokens, 3);
        assert_eq!(usage.total_tokens, 140);
        assert!(usage.cache_read_reported);
    }
    #[tokio::test]
    async fn review17_native_success_and_all_prefix_failure_wrappers_share_iterations() {
        for mode in ["success", "trailing", "limit", "io", "content-length"] {
            let prefix = value().to_string();
            let limit = if matches!(mode, "limit" | "content-length") { prefix.len() as u64 } else { 8192 };
            let chunks = match mode {
                "success" => vec![Ok(Bytes::from(prefix))],
                "io" => vec![Ok(Bytes::from(prefix)), Err(std::io::Error::other("fixture"))],
                _ => vec![Ok(Bytes::from(format!("{prefix} trailing junk")))],
            };
            let mut upstream = response(chunks, StatusCode::OK);
            upstream.extensions_mut().insert(ProviderRetryObservation {
                usage: TokenUsage { input_tokens: 5, output_tokens: 1, total_tokens: 6, ..Default::default() },
                additional_sends: 1, ..Default::default()
            });
            if mode == "success" {
                let decoded = read_native_compaction_json(upstream, limit).await.unwrap();
                let usage = native_compaction_usage(&decoded);
                check(&usage);
                let ledger = ObservabilityLedger::new(None);
                let mut headers = HeaderMap::new();
                headers.insert("session_id", HeaderValue::from_static("01a13ddb-8d21-7cb2-8f00-23a8a09af079"));
                seed(&ledger, &candidate(), false).with_compaction_session(&headers)
                    .finish(StatusCode::OK, None, usage);
                assert_eq!(ledger.summary().total_tokens, 140);
                assert_eq!(ledger.summary().successful_requests, 1);
                assert!(ledger.cache_health_summary().sessions.is_empty());
                continue;
            }
            let failure = if mode == "content-length" {
                upstream.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from_str(&(limit + 14).to_string()).unwrap());
                super::super::senders::reject_oversized_provider_response(upstream, limit, false).await
            } else { read_native_compaction_json(upstream, limit).await.unwrap_err() };
            check(&failure.response.extensions().get::<crate::observability::ReportedUpstreamUsage>().unwrap().0);
            let ledger = ObservabilityLedger::new(None);
            let mut headers = HeaderMap::new();
            headers.insert("session_id", HeaderValue::from_static("01a13ddb-8d21-7cb2-8f00-23a8a09af079"));
            let mut observation = seed(&ledger, &candidate(), false).with_compaction_session(&headers);
            observation.record_provider_retries(failure.response.extensions().get::<ProviderRetryObservation>().unwrap());
            observation.record_upstream_error(&failure.response);
            observation.finish(failure.response.status(), Some("invalid_compaction_response"), TokenUsage::default());
            assert_eq!(ledger.summary().total_tokens, 146, "{mode}");
            assert_eq!(ledger.summary().input_tokens, 122);
            assert_eq!(ledger.summary().failed_requests, 1);
            assert!(ledger.cache_health_summary().sessions.is_empty());
        }
    }
    #[test]
    fn review17_native_iterations_aggregate_fallback_unknown_and_saturation() {
        let mut v = value();
        v["usage"]["input_tokens"] = json!(999999);
        v["usage"]["output_tokens"] = json!(999999);
        v["usage"]["total_tokens"] = json!(9999999);
        check(&native_compaction_usage(&v));
        for iterations in [json!([]), json!([null, false, {}, {"input_tokens":"unknown"}]), json!(null)] {
            let v = json!({"usage":{"input_tokens":17,"output_tokens":3,"iterations":iterations}});
            let usage = native_compaction_usage(&v);
            assert_eq!(usage.input_tokens, 17); assert_eq!(usage.total_tokens, 20);
            assert!(!usage.cache_read_reported);
        }
        let v = json!({"usage":{"iterations":[{"input_tokens":17}, {"input_tokens":2,"cache_read_input_tokens":3}]}});
        let usage = native_compaction_usage(&v);
        assert_eq!(usage.input_tokens, 22); assert_eq!(usage.cached_input_tokens, 3);
        assert!(!usage.cache_read_reported);
        let v = json!({"usage":{"iterations":[null,{"input_tokens":2,"cache_read_input_tokens":3}]}});
        assert!(!native_compaction_usage(&v).cache_read_reported);
        let raw = json!({"input_tokens":u64::MAX,"cache_read_input_tokens":u64::MAX,
            "cache_creation_input_tokens":u64::MAX,"output_tokens":u64::MAX,
            "output_tokens_details":{"reasoning_tokens":u64::MAX},"total_tokens":u64::MAX});
        let usage = native_compaction_usage(&json!({"usage":{"iterations":[raw,raw]}}));
        assert_eq!(usage.input_tokens, u64::MAX); assert_eq!(usage.output_tokens, u64::MAX);
        assert_eq!(usage.cached_input_tokens, u64::MAX); assert_eq!(usage.cache_creation_input_tokens, u64::MAX);
        assert_eq!(usage.reasoning_tokens, u64::MAX); assert_eq!(usage.total_tokens, u64::MAX);
        for envelope in [json!({"usage":value()["usage"]}), json!({"response":{"usage":value()["usage"]}}),
            json!({"message":{"usage":value()["usage"]}})] { check(&TokenUsage::from_json(&envelope)); }
        let ordinary = TokenUsage::from_json(&json!({"usage":{"prompt_tokens":100,"completion_tokens":7,
            "cache_read_input_tokens":80,"cache_creation_input_tokens":3}}));
        assert_eq!(ordinary.input_tokens, 100); assert_eq!(ordinary.total_tokens, 107);
        let gemini = TokenUsage::from_json(&json!({"usageMetadata":{"promptTokenCount":17,
            "candidatesTokenCount":3,"thoughtsTokenCount":2}}));
        assert_eq!(gemini.input_tokens, 17); assert_eq!(gemini.output_tokens, 5); assert_eq!(gemini.total_tokens, 22);
    }
}


#[cfg(test)]
mod review18_regressions {
    use super::*;
    #[test]
    fn review18_replay_choice_relaxes_forcing_without_widening_tools() {
        for choice in [json!("auto"), json!("none"), json!("required"),
            json!({"type":"function","name":"small"}), json!({"type":"custom","name":"free"}),
            json!({"type":"tool_search"}),
            json!({"type":"allowed_tools","mode":"auto","tools":[{"type":"function","name":"small"}]}),
            json!({"type":"allowed_tools","mode":"required","tools":[{"type":"function","name":"small"}]}),
            json!({"type":"allowed_tools","mode":"required","tools":[]})] {
            let mut body = json!({"input":"continue","tools":[{"type":"function","name":"small"},
                {"type":"function","name":"excluded","description":"x".repeat(100000)},
                {"type":"custom","name":"free"},{"type":"tool_search"}],"tool_choice":choice});
            let before = responses_to_anthropic_with_oauth(&body,"test",false).unwrap();
            relax_replay_tool_choice(&mut body["tool_choice"]);
            body["reasoning"] = json!({"effort":"high"});
            let after = responses_to_anthropic_with_oauth(&body,"test",false).unwrap();
            assert_eq!(before.get("tools"), after.get("tools"));
            assert!(!matches!(after["tool_choice"]["type"].as_str(), Some("any" | "tool")));
            if choice == "none" { assert_eq!(after["tool_choice"]["type"], "none"); }
        }
    }
    #[tokio::test]
    async fn review18_native_compact_replay_keeps_allowed_subset_with_large_excluded_tool() {
        use super::super::senders::review18_regressions::{mock, state, registered_candidate};
        for mode in ["auto", "required"] {
            let block = json!({"type":"compaction","content":"completed task facts","signature":"fixture-signed-block"});
            let (url, mut received, _task) = mock(json!({"stop_reason":"compaction","content":[block],
                "usage":{"input_tokens":21,"output_tokens":4}}), StatusCode::OK).await;
            let state = state(); let mut c = super::review13_resolved_auth_regressions::candidate();
            c.provider.base_url = url; c.provider.credential.source = CredentialSource::None;
            c.provider.credential.reference = None;
            c.provider.limits.max_request_bytes = 16000; c.max_input_tokens = Some(4000);
            let c = registered_candidate(&state, c).await;
            let body = json!({"model":"test","max_output_tokens":128,"reasoning":{"effort":"high"},
                "tools":[{"type":"function","name":"small","parameters":{"type":"object"}},
                    {"type":"function","name":"excluded","description":"x".repeat(100000)}],
                "tool_choice":{"type":"allowed_tools","mode":mode,"tools":[{"type":"function","name":"small"}]},
                "input":[{"role":"user","content":"Implement the requested change"},
                    {"role":"assistant","content":"Implemented task facts"},
                    {"role":"user","content":"Now validate the change"},{"type":"compaction_trigger"}]});
            let (value, usage, _) = match native_anthropic_compact_candidate(&state,&HeaderMap::new(),&body,&c,CompactionRequestKind::NativeTrigger).await {
                Ok(result) => result,
                Err(failure) => {
                    let status = failure.response.status();
                    let bytes = to_bytes(failure.response.into_body(), 65536).await.unwrap();
                    panic!("compact failed: status={status}; body={}", String::from_utf8_lossy(&bytes));
                }
            };
            assert_eq!(usage.input_tokens, 21);
            let compact_wire = received.try_recv().unwrap();
            assert_eq!(compact_wire["tools"].as_array().unwrap().len(), 1);
            let checkpoint = native::decode(&value["output"][0]).unwrap().unwrap();
            assert_eq!(checkpoint.block, block);
            let mut replay = body.clone(); replay["input"] = value["output"].clone();
            relax_replay_tool_choice(&mut replay["tool_choice"]);
            crate::translate::prepare_translated_responses_request_with_native_anthropic(&mut replay,true,true,true);
            let wire = responses_to_anthropic_with_oauth(&replay,&c.upstream_model,false).unwrap();
            assert_eq!(wire["tools"], compact_wire["tools"]);
            assert_eq!(wire["messages"][0]["content"][0], block);
            assert!(serde_json::to_vec(&wire).unwrap().len() < 16000);
            assert!(received.try_recv().is_err());
        }
    }
}
