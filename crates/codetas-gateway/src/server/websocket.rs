use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompactionMode {
    Local,
    Responses,
    CompactEndpoint,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompactionRequestKind {
    Standalone,
    NativeTrigger,
}

fn canonical_base_url(value: &str) -> Option<String> {
    let url = url::Url::parse(value.trim()).ok()?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let path = url.path().trim_end_matches('/');
    Some(format!("{}{}", url.origin().ascii_serialization(), path))
}

fn is_api_key_credential(source: CredentialSource) -> bool {
    matches!(
        source,
        CredentialSource::Environment | CredentialSource::Keychain | CredentialSource::Command
    )
}

pub(crate) fn candidate_compaction_mode(
    candidate: &RouteCandidate,
    request_kind: CompactionRequestKind,
) -> CompactionMode {
    let provider_credential_source = candidate.provider.credential.source;
    let credential_source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let is_responses = candidate.provider.transport == ProviderTransport::Standard
        && candidate
            .provider
            .protocol_for_model(&candidate.upstream_model)
            == ProviderProtocol::Responses
        && candidate.provider.azure_deployment.is_none();
    if !is_responses {
        return CompactionMode::Local;
    }
    let base_url = canonical_base_url(&candidate.provider.base_url);
    let canonical_chatgpt = provider_credential_source == CredentialSource::Forward
        && base_url.as_deref() == Some("https://chatgpt.com/backend-api/codex");
    let canonical_openai_api =
        matches!(candidate.provider.id.as_str(), "openai-api" | "openai-apikey")
            && is_api_key_credential(credential_source)
            && base_url.as_deref() == Some("https://api.openai.com/v1");
    match request_kind {
        CompactionRequestKind::Standalone if canonical_chatgpt || canonical_openai_api => {
            CompactionMode::CompactEndpoint
        }
        CompactionRequestKind::NativeTrigger if canonical_chatgpt || canonical_openai_api => {
            CompactionMode::Responses
        }
        _ => CompactionMode::Local,
    }
}

pub(crate) async fn responses_websocket(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response<Body> {
    let admission = match authorize_request(&state.settings, &headers, "responses:write").await {
        Ok(admission) => admission,
        Err(response) => return response,
    };
    websocket
        .max_message_size(16 * 1024 * 1024)
        .on_upgrade(move |socket| async move {
            responses_websocket_session(socket, state, headers, admission.trusts_turn_metadata())
                .await;
        })
        .into_response()
}

pub(crate) async fn responses_websocket_session(
    socket: WebSocket,
    state: GatewayState,
    headers: HeaderMap,
    trust_turn_metadata: bool,
) {
    let (mut socket_sender, mut socket_receiver) = socket.split();
    let (turn_sender, mut turn_receiver) = mpsc::channel::<WebSocketTurnEvent>(128);
    let mut local_contexts = HashMap::<String, Value>::new();
    let mut current_turn_id = 0_u64;
    let mut active_turn = None::<(u64, JoinHandle<()>)>;
    let idle = tokio::time::sleep(Duration::from_secs(60 * 60));
    tokio::pin!(idle);
    loop {
        tokio::select! {
            _ = &mut idle => break,
            incoming = socket_receiver.next() => {
                let message = match incoming {
                    Some(Ok(message)) => message,
                    _ => break,
                };
                idle.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(60 * 60));
                let text = match message {
                    Message::Text(text) => text,
                    Message::Ping(payload) => {
                        if socket_sender.send(Message::Pong(payload)).await.is_err() { break; }
                        continue;
                    }
                    Message::Pong(_) => continue,
                    Message::Close(_) => break,
                    Message::Binary(_) => {
                        if socket_sender.send(websocket_error_message(400, "binary messages are not supported")).await.is_err() { break; }
                        continue;
                    }
                };
                let mut event = match serde_json::from_str::<Value>(&text) {
                    Ok(event) => event,
                    Err(_) => {
                        if socket_sender.send(websocket_error_message(400, "WebSocket event is not valid JSON")).await.is_err() { break; }
                        continue;
                    }
                };
                match event.get("type").and_then(Value::as_str) {
                    Some("response.processed") => continue,
                    Some("response.create") => {}
                    _ => continue,
                }
                if let Some((_, task)) = active_turn.take() {
                    task.abort();
                }
                current_turn_id = current_turn_id.wrapping_add(1).max(1);
                let turn_id = current_turn_id;
                let generate = event.get("generate").and_then(Value::as_bool).unwrap_or(true);
                if let Some(object) = event.as_object_mut() {
                    object.remove("type");
                    object.remove("generate");
                    object.remove("background");
                    object.remove("stream");
                }
                let previous_id_opt = event
                    .get("previous_response_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(previous_id) = previous_id_opt.clone() {
                    if let Some(previous) = local_contexts.get(&previous_id) {
                        crate::debug::log(&format!(
                            "ws merge: id={} MERGED prev_input={}",
                            previous_id,
                            websocket_input_items(previous.get("input")).len()
                        ));
                        event = merge_websocket_context(previous, &event);
                        if let Some(object) = event.as_object_mut() {
                            object.remove("previous_response_id");
                        }
                    } else {
                        crate::debug::log(&format!("ws merge: id={} MISS contexts={}", previous_id, local_contexts.len()));
                    }
                }
                {
                    use std::collections::BTreeMap;
                    let items = websocket_input_items(event.get("input"));
                    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
                    for item in &items {
                        let t = item.get("type").and_then(Value::as_str).unwrap_or("?");
                        *counts.entry(t).or_default() += 1;
                    }
                    let summary = counts.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(" ");
                    crate::debug::log(&format!("ws event after merge: prev={:?} input=[{}]", previous_id_opt.is_some(), summary));
                }
                if !generate {
                    let id = format!("resp_{}", Uuid::new_v4().simple());
                    retain_websocket_context(&mut local_contexts, id.clone(), event.clone());
                    let mut created = websocket_response_object(&id, &event, "in_progress", Vec::new());
                    let completed = websocket_response_object(&id, &event, "completed", Vec::new());
                    created["output"] = Value::Array(Vec::new());
                    for value in [
                        json!({"type": "response.created", "sequence_number": 0, "response": created}),
                        json!({"type": "response.completed", "sequence_number": 1, "response": completed}),
                    ] {
                        if socket_sender.send(Message::Text(value.to_string().into())).await.is_err() { return; }
                    }
                    continue;
                }
                let request_context = event.clone();
                event["stream"] = Value::Bool(true);
                let task_sender = turn_sender.clone();
                let task_state = state.clone();
                let task_headers = headers.clone();
                let task = tokio::spawn(async move {
                    run_websocket_turn(
                        task_sender,
                        turn_id,
                        task_state,
                        task_headers,
                        event,
                        request_context,
                        trust_turn_metadata,
                    ).await;
                });
                active_turn = Some((turn_id, task));
            }
            output = turn_receiver.recv() => {
                let Some(output) = output else { break; };
                idle.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(60 * 60));
                let turn_id = output.turn_id();
                if turn_id != current_turn_id { continue; }
                match output {
                    WebSocketTurnEvent::Frame { value, .. } => {
                        if socket_sender.send(Message::Text(value.to_string().into())).await.is_err() { break; }
                    }
                    WebSocketTurnEvent::Error { status, message, .. } => {
                        if socket_sender.send(websocket_error_message(status, &message)).await.is_err() { break; }
                    }
                    WebSocketTurnEvent::Context { id, context, .. } => {
                        let ctx_input = websocket_input_items(context.get("input"));
                        {
                            use std::collections::BTreeMap;
                            let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
                            for item in &ctx_input {
                                let t = item.get("type").and_then(Value::as_str).unwrap_or("?");
                                *counts.entry(t).or_default() += 1;
                            }
                            let summary = counts.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(" ");
                            crate::debug::log(&format!("ws context saved: id={} input=[{}]", id, summary));
                        }
                        retain_websocket_context(&mut local_contexts, id, context);
                    }
                    WebSocketTurnEvent::Finished { .. } => {
                        if active_turn.as_ref().is_some_and(|(id, _)| *id == turn_id) {
                            active_turn.take();
                        }
                    }
                }
            }
        }
    }
    if let Some((_, task)) = active_turn {
        task.abort();
    }
}

pub(crate) enum WebSocketTurnEvent {
    Frame {
        turn_id: u64,
        value: Value,
    },
    Error {
        turn_id: u64,
        status: u16,
        message: String,
    },
    Context {
        turn_id: u64,
        id: String,
        context: Value,
    },
    Finished {
        turn_id: u64,
    },
}

impl WebSocketTurnEvent {
    fn turn_id(&self) -> u64 {
        match self {
            Self::Frame { turn_id, .. }
            | Self::Error { turn_id, .. }
            | Self::Context { turn_id, .. }
            | Self::Finished { turn_id } => *turn_id,
        }
    }
}

pub(crate) async fn run_websocket_turn(
    sender: mpsc::Sender<WebSocketTurnEvent>,
    turn_id: u64,
    state: GatewayState,
    headers: HeaderMap,
    event: Value,
    request_context: Value,
    trust_turn_metadata: bool,
) {
    let response = responses_inner(state, headers, event, trust_turn_metadata).await;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let message = to_bytes(response.into_body(), 64 * 1024)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| {
                value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "CODETAS could not create the response".into());
        let _ = sender
            .send(WebSocketTurnEvent::Error {
                turn_id,
                status,
                message,
            })
            .await;
        let _ = sender.send(WebSocketTurnEvent::Finished { turn_id }).await;
        return;
    }
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if content_type.contains("application/json") {
        match to_bytes(response.into_body(), 64 * 1024 * 1024)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        {
            Some(value) => {
                for frame in websocket_json_response_events(&value) {
                    if frame.get("type").and_then(Value::as_str) == Some("response.completed") {
                        if let Some(response) = frame.get("response") {
                            if let Some(output) = response.get("output").and_then(Value::as_array) {
                                use std::collections::BTreeMap;
                                let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
                                for item in output {
                                    let t = item.get("type").and_then(Value::as_str).unwrap_or("?");
                                    *counts.entry(t).or_default() += 1;
                                }
                                let summary = counts.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(" ");
                                crate::debug::log(&format!("ws completed output=[{}]", summary));
                            }
                            if let Some(id) = response.get("id").and_then(Value::as_str) {
                                let _ = sender
                                    .send(WebSocketTurnEvent::Context {
                                        turn_id,
                                        id: id.to_string(),
                                        context: context_after_response(&request_context, response),
                                    })
                                    .await;
                            }
                        }
                    }
                    if sender
                        .send(WebSocketTurnEvent::Frame {
                            turn_id,
                            value: frame,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            None => {
                let _ = sender
                    .send(WebSocketTurnEvent::Error {
                        turn_id,
                        status: 502,
                        message: "Responses JSON was invalid or too large".into(),
                    })
                    .await;
            }
        }
        let _ = sender.send(WebSocketTurnEvent::Finished { turn_id }).await;
        return;
    }

    let mut source = response.into_body().into_data_stream();
    let mut pending = Vec::new();
    let mut terminal_seen = false;
    while let Some(chunk) = source.next().await {
        let bytes = match chunk {
            Ok(bytes) => bytes,
            Err(_) => {
                let _ = sender
                    .send(WebSocketTurnEvent::Error {
                        turn_id,
                        status: 502,
                        message: "Responses stream failed".into(),
                    })
                    .await;
                let _ = sender.send(WebSocketTurnEvent::Finished { turn_id }).await;
                return;
            }
        };
        let values = match drain_sse_values(&mut pending, &bytes) {
            Ok(values) => values,
            Err(_) => {
                let _ = sender
                    .send(WebSocketTurnEvent::Error {
                        turn_id,
                        status: 502,
                        message: "Responses stream was invalid".into(),
                    })
                    .await;
                let _ = sender.send(WebSocketTurnEvent::Finished { turn_id }).await;
                return;
            }
        };
        for mut value in values {
            let kind = value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let terminal = matches!(
                kind.as_str(),
                "response.completed" | "response.failed" | "response.incomplete"
            );
            if kind == "response.completed" {
                if let Some(response) = value.get("response") {
                    if let Some(output) = response.get("output").and_then(Value::as_array) {
                        use std::collections::BTreeMap;
                        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
                        for item in output {
                            let t = item.get("type").and_then(Value::as_str).unwrap_or("?");
                            *counts.entry(t).or_default() += 1;
                        }
                        let summary = counts.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(" ");
                        crate::debug::log(&format!("ws sse completed output=[{}] total={}", summary, output.len()));
                    }
                    if let Some(id) = response.get("id").and_then(Value::as_str) {
                        if sender
                            .send(WebSocketTurnEvent::Context {
                                turn_id,
                                id: id.to_string(),
                                context: context_after_response(&request_context, response),
                            })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
            terminal_seen = terminal;
            if sender
                .send(WebSocketTurnEvent::Frame { turn_id, value })
                .await
                .is_err()
            {
                return;
            }
            if terminal_seen {
                break;
            }
        }
        if terminal_seen {
            break;
        }
    }
    if !terminal_seen {
        let message = if pending.iter().all(u8::is_ascii_whitespace) {
            "Responses stream ended before a terminal event"
        } else {
            "Responses stream ended mid-frame"
        };
        let _ = sender
            .send(WebSocketTurnEvent::Error {
                turn_id,
                status: 502,
                message: message.into(),
            })
            .await;
    }
    let _ = sender.send(WebSocketTurnEvent::Finished { turn_id }).await;
}

fn push_websocket_json_event(events: &mut Vec<Value>, mut event: Value) {
    event["sequence_number"] = Value::from(events.len() as u64);
    events.push(event);
}

fn in_progress_json_output_item(item: &Value) -> Value {
    let mut added = item.clone();
    if !added.is_object() {
        return added;
    }
    added["status"] = Value::String("in_progress".into());
    let item_type = added
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    match item_type.as_str() {
        "message" => {
            added["role"] = Value::String("assistant".into());
            added["content"] = Value::Array(Vec::new());
        }
        "reasoning" => {
            added["summary"] = Value::Array(Vec::new());
            if added.get("content").is_some() {
                added["content"] = Value::Array(Vec::new());
            }
        }
        "function_call" => added["arguments"] = Value::String(String::new()),
        "custom_tool_call" => added["input"] = Value::String(String::new()),
        _ => {}
    }
    added
}

pub(crate) fn websocket_json_response_events(response: &Value) -> Vec<Value> {
    let mut canonical_response = response.clone();
    repair_responses_snapshot_json(&mut canonical_response, &Value::Null);
    let response = &canonical_response;
    let mut in_progress = response.clone();
    in_progress["status"] = Value::String("in_progress".into());
    in_progress["output"] = Value::Array(Vec::new());
    let mut events = Vec::new();
    push_websocket_json_event(&mut events, json!({
        "type": "response.created",
        "response": in_progress,
    }));
    let status = match response.get("status").and_then(Value::as_str) {
        Some("failed") => "failed",
        Some("incomplete") => "incomplete",
        _ => "completed",
    };
    if status != "completed" {
        let mut terminal = response.clone();
        terminal["status"] = Value::String(status.into());
        push_websocket_json_event(&mut events, json!({
            "type": format!("response.{status}"),
            "response": terminal,
        }));
        return events;
    }
    for (index, item) in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let item_id = item.get("id").cloned().unwrap_or(Value::Null);
        push_websocket_json_event(&mut events, json!({
            "type": "response.output_item.added",
            "output_index": index,
            "item": in_progress_json_output_item(item),
        }));
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for (content_index, part) in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    if part.get("type").and_then(Value::as_str) != Some("output_text") {
                        continue;
                    }
                    let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                    push_websocket_json_event(&mut events, json!({
                        "type": "response.content_part.added", "item_id": item_id.clone(),
                        "output_index": index, "content_index": content_index,
                        "part": {"type": "output_text", "text": "", "annotations": []},
                    }));
                    push_websocket_json_event(&mut events, json!({
                        "type": "response.output_text.delta", "item_id": item_id.clone(),
                        "output_index": index, "content_index": content_index,
                        "delta": text, "logprobs": [],
                    }));
                    push_websocket_json_event(&mut events, json!({
                        "type": "response.output_text.done", "item_id": item_id.clone(),
                        "output_index": index, "content_index": content_index,
                        "text": text, "logprobs": [],
                    }));
                    push_websocket_json_event(&mut events, json!({
                        "type": "response.content_part.done", "item_id": item_id.clone(),
                        "output_index": index, "content_index": content_index, "part": part,
                    }));
                }
            }
            Some("reasoning") => {
                for (summary_index, part) in item
                    .get("summary")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    if part.get("type").and_then(Value::as_str) != Some("summary_text") {
                        continue;
                    }
                    let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                    push_websocket_json_event(&mut events, json!({
                        "type": "response.reasoning_summary_part.added",
                        "item_id": item_id.clone(), "output_index": index,
                        "summary_index": summary_index,
                        "part": {"type": "summary_text", "text": ""},
                    }));
                    push_websocket_json_event(&mut events, json!({
                        "type": "response.reasoning_summary_text.delta",
                        "item_id": item_id.clone(), "output_index": index,
                        "summary_index": summary_index, "delta": text,
                    }));
                    push_websocket_json_event(&mut events, json!({
                        "type": "response.reasoning_summary_text.done",
                        "item_id": item_id.clone(), "output_index": index,
                        "summary_index": summary_index, "text": text,
                    }));
                    push_websocket_json_event(&mut events, json!({
                        "type": "response.reasoning_summary_part.done",
                        "item_id": item_id.clone(), "output_index": index,
                        "summary_index": summary_index, "part": part,
                    }));
                }
            }
            Some("function_call") => {
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                push_websocket_json_event(&mut events, json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": item_id.clone(), "output_index": index, "delta": arguments,
                }));
                push_websocket_json_event(&mut events, json!({
                    "type": "response.function_call_arguments.done",
                    "item_id": item_id.clone(), "output_index": index,
                    "arguments": arguments,
                }));
            }
            Some("custom_tool_call") => {
                let input = item.get("input").and_then(Value::as_str).unwrap_or_default();
                push_websocket_json_event(&mut events, json!({
                    "type": "response.custom_tool_call_input.delta",
                    "item_id": item_id.clone(), "output_index": index, "delta": input,
                }));
                push_websocket_json_event(&mut events, json!({
                    "type": "response.custom_tool_call_input.done",
                    "item_id": item_id.clone(), "output_index": index, "input": input,
                }));
            }
            _ => {}
        }
        push_websocket_json_event(&mut events, json!({
            "type": "response.output_item.done",
            "output_index": index,
            "item": item,
        }));
    }
    let mut terminal = response.clone();
    terminal["status"] = Value::String(status.into());
    push_websocket_json_event(&mut events, json!({
        "type": format!("response.{status}"),
        "response": terminal,
    }));
    events
}

pub(crate) fn websocket_error_message(status: u16, message: &str) -> Message {
    Message::Text(
        json!({"type": "error", "status": status, "error": {"type": "invalid_request_error", "message": message}})
            .to_string()
            .into(),
    )
}

pub(crate) fn merge_websocket_context(previous: &Value, current: &Value) -> Value {
    let mut merged = previous.clone();
    let previous_input = websocket_input_items(merged.get("input"));
    if let (Some(target), Some(source)) = (merged.as_object_mut(), current.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    let mut input = previous_input;
    input.extend(websocket_input_items(current.get("input")));
    merged["input"] = Value::Array(input);
    merged
}

pub(crate) fn context_after_response(request: &Value, response: &Value) -> Value {
    let mut context = request.clone();
    let mut input = websocket_input_items(context.get("input"));
    let mut output = response
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for item in &mut output {
        let repairable = matches!(
            item.get("type").and_then(Value::as_str),
            Some("message" | "reasoning")
        );
        let synthetic = item
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(is_codetas_repaired_item_id);
        if repairable && synthetic {
            if let Some(object) = item.as_object_mut() {
                object.remove("id");
            }
        }
    }
    input.extend(output);
    context["input"] = Value::Array(input);
    context
}

pub(crate) fn websocket_input_items(input: Option<&Value>) -> Vec<Value> {
    match input {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::String(text)) => vec![json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": text}]
        })],
        _ => Vec::new(),
    }
}

pub(crate) fn retain_websocket_context(
    contexts: &mut HashMap<String, Value>,
    id: String,
    context: Value,
) {
    if serde_json::to_vec(&context).is_ok_and(|bytes| bytes.len() <= 16 * 1024 * 1024) {
        if contexts.len() >= 8 {
            contexts.clear();
        }
        contexts.insert(id, context);
    }
}

pub(crate) fn websocket_response_object(
    id: &str,
    request: &Value,
    status: &str,
    output: Vec<Value>,
) -> Value {
    json!({
        "id": id,
        "object": "response",
        "created_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0),
        "status": status,
        "model": request.get("model").cloned().unwrap_or(Value::Null),
        "output": output,
        "usage": Value::Null,
        "error": Value::Null,
        "incomplete_details": Value::Null
    })
}

#[cfg(test)]
mod snapshot_continuation_tests {
    use super::*;

    fn completed_with_empty_output() -> Value {
        json!({
            "type": "response.completed",
            "response": {"id": "resp_ws", "status": "completed", "output": []}
        })
    }

    fn snapshot_with_done_message() -> ResponsesSnapshotAccumulator {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        snapshot.observe(&json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {"id": "msg_ws", "type": "message", "status": "completed",
                "role": "assistant", "content": [{"type": "output_text", "text": "hello"}]}
        }));
        snapshot
    }

    #[test]
    fn generic_websocket_continuation_preserves_explicit_empty_output() {
        let mut terminal = completed_with_empty_output();

        let context = context_after_response(
            &json!({"input": [{"role": "user", "content": "hi"}]}),
            &terminal["response"],
        );

        assert!(terminal["response"]["output"]
            .as_array()
            .is_some_and(Vec::is_empty));
        assert_eq!(context["input"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn opt_in_sparse_repair_keeps_explicit_empty_terminal_authoritative() {
        let mut snapshot = snapshot_with_done_message();
        let mut terminal = completed_with_empty_output();

        snapshot.repair_terminal_event(&mut terminal);

        assert!(terminal["response"]["output"]
            .as_array()
            .is_some_and(Vec::is_empty));
    }

    #[test]
    fn websocket_continuation_fails_closed_on_delta_after_done() {
        let mut snapshot = snapshot_with_done_message();
        snapshot.observe(&json!({
            "type": "response.output_text.delta", "output_index": 0,
            "item_id": "msg_ws", "content_index": 0, "delta": "evil"
        }));

        let mut generic = completed_with_empty_output();
        snapshot.preserve_generic_terminal_event(&mut generic);
        assert!(generic["response"]["output"]
            .as_array()
            .is_some_and(Vec::is_empty));

        let mut sparse = json!({
            "type": "response.completed",
            "response": {"id": "resp_ws_sparse", "status": "completed"}
        });
        snapshot.repair_terminal_event(&mut sparse);
        assert!(sparse["response"].get("output").is_none());
    }

    #[test]
    fn websocket_continuation_fails_closed_on_lifecycle_type_mismatch() {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        snapshot.observe(&json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "item_ws", "type": "message", "status": "in_progress",
                "role": "assistant", "content": []}
        }));
        snapshot.observe(&json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {"id": "item_ws", "type": "function_call", "status": "completed",
                "call_id": "call_ws", "name": "lookup", "arguments": "{}"}
        }));

        let mut generic = completed_with_empty_output();
        snapshot.preserve_generic_terminal_event(&mut generic);
        assert!(generic["response"]["output"]
            .as_array()
            .is_some_and(Vec::is_empty));

        let mut sparse = json!({
            "type": "response.completed",
            "response": {"id": "resp_ws_sparse", "status": "completed"}
        });
        snapshot.repair_terminal_event(&mut sparse);
        assert!(sparse["response"].get("output").is_none());
    }

    #[test]
    fn websocket_raw_missing_ids_taint_before_default_id_repair() {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        let mut repair = ResponsesItemIdRepair::new_with_policy(
            &crate::config::ResponseItemIdRepairSettings::default(),
            true,
        )
        .expect("ID repair");
        let mut added = json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"type": "message", "status": "in_progress",
                "role": "assistant", "content": []}
        });

        snapshot.observe(&added);
        repair.repair_event(&mut added);
        let mut terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_ws_raw_id", "status": "completed"}
        });
        snapshot.closing_events_before_terminal(&terminal);
        repair.repair_event(&mut terminal);
        snapshot.repair_terminal_event(&mut terminal);

        assert!(terminal["response"].get("output").is_none());
    }

    #[test]
    fn websocket_snapshot_rejects_delta_only_items_and_index_gaps() {
        let mut delta_only = ResponsesSnapshotAccumulator::default();
        delta_only.observe(&json!({
            "type": "response.custom_tool_call_input.delta", "output_index": 0,
            "item_id": "custom_ws_unproven", "delta": "pwd"
        }));
        let mut delta_terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_ws_unproven", "status": "completed"}
        });
        delta_only.repair_terminal_event(&mut delta_terminal);
        assert!(delta_only.is_tainted());
        assert!(delta_terminal["response"].get("output").is_none());

        let mut gap = ResponsesSnapshotAccumulator::default();
        gap.observe(&json!({
            "type": "response.output_item.done", "output_index": 1,
            "item": {"id": "msg_ws_gap", "type": "message", "status": "completed",
                "role": "assistant", "content": []}
        }));
        let mut gap_terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_ws_gap", "status": "completed"}
        });
        gap.repair_terminal_event(&mut gap_terminal);
        assert!(gap.is_tainted());
        assert!(gap_terminal["response"].get("output").is_none());
    }

    #[test]
    fn websocket_sparse_repair_emits_closing_sequence_before_terminal() {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        snapshot.observe(&json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "msg_ws", "type": "message", "status": "in_progress",
                "role": "assistant", "content": []}
        }));
        let delta = json!({
            "type": "response.output_text.delta", "output_index": 0,
            "item_id": "msg_ws", "content_index": 0, "delta": "hello"
        });
        let mut injected = snapshot.injected_events_before(&delta);
        snapshot.observe(&delta);
        let mut terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_ws", "status": "completed"}
        });
        injected.extend(snapshot.closing_events_before_terminal(&terminal));
        snapshot.repair_terminal_event(&mut terminal);

        assert_eq!(
            injected.iter().filter_map(|event| event.get("type").and_then(Value::as_str)).collect::<Vec<_>>(),
            vec![
                "response.content_part.added",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
            ]
        );
        assert_eq!(terminal["response"]["output"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn websocket_sparse_repair_accepts_proven_index_without_item_id_and_missing_status() {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        snapshot.observe(&json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "msg_ws_optional", "type": "message"}
        }));
        let delta = json!({
            "type": "response.output_text.delta", "output_index": 0,
            "content_index": 0, "delta": "hello"
        });
        snapshot.injected_events_before(&delta);
        snapshot.observe(&delta);
        let mut terminal = json!({
            "type": "response.completed", "response": {"id": "resp_ws_optional"}
        });
        let closing = snapshot.closing_events_before_terminal(&terminal);
        snapshot.repair_terminal_event_with_request(
            &mut terminal,
            &json!({"parallel_tool_calls": false, "tool_choice": "none", "tools": []}),
        );

        assert!(!snapshot.is_tainted());
        assert_eq!(closing.last().and_then(|event| event.get("type")).and_then(Value::as_str),
            Some("response.output_item.done"));
        assert_eq!(terminal["response"]["status"], "completed");
        assert_eq!(terminal["response"]["parallel_tool_calls"], false);
    }

    #[test]
    fn websocket_non_stream_json_uses_canonical_snapshot_before_event_conversion() {
        let mut response = json!({
            "id": "resp_ws_json",
            "output": [{"id": "bad id", "type": "message",
                "content": [{"type": "output_text"}]}]
        });
        repair_responses_snapshot_json(
            &mut response,
            &json!({"parallel_tool_calls": true, "tool_choice": "auto", "tools": []}),
        );
        let mut id_repair = ResponsesItemIdRepair::new_with_policy(
            &crate::config::ResponseItemIdRepairSettings::default(),
            true,
        )
        .expect("ID repair");
        id_repair.repair_response(&mut response);
        let repaired_id = response["output"][0]["id"]
            .as_str()
            .expect("repaired ID")
            .to_string();
        let events = websocket_json_response_events(&response);

        assert_eq!(response["status"], "completed");
        assert_eq!(response["output"][0]["role"], "assistant");
        assert_eq!(response["output"][0]["content"][0]["annotations"], json!([]));
        assert_eq!(events.last().expect("terminal")["type"], "response.completed");
        assert_eq!(
            events.last().expect("terminal")["response"]["output"][0]["id"],
            repaired_id
        );
        assert!(events.iter().all(|event| {
            event
                .get("item_id")
                .and_then(Value::as_str)
                .is_none_or(|item_id| item_id == repaired_id)
        }));
    }

    #[test]
    fn completed_json_emits_canonical_item_lifecycles_for_all_supported_types() {
        let response = json!({
            "id": "resp_lifecycle", "status": "completed", "output": [
                {"id": "msg_one", "type": "message", "status": "completed",
                    "role": "assistant", "content": [{"type": "output_text",
                        "text": "hello", "annotations": []}]},
                {"id": "rs_one", "type": "reasoning", "status": "completed",
                    "summary": [{"type": "summary_text", "text": "checked"}]},
                {"id": "fc_one", "type": "function_call", "status": "completed",
                    "call_id": "call_one", "name": "lookup", "arguments": "{}"},
                {"id": "ct_one", "type": "custom_tool_call", "status": "completed",
                    "call_id": "call_two", "name": "exec", "input": "pwd"}
            ]
        });

        let events = websocket_json_response_events(&response);
        let types = events
            .iter()
            .filter_map(|event| event.get("type").and_then(Value::as_str))
            .collect::<Vec<_>>();

        assert_eq!(types.first().copied(), Some("response.created"));
        assert_eq!(types.last().copied(), Some("response.completed"));
        let message_lifecycle = [
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
        ];
        assert!(types
            .windows(message_lifecycle.len())
            .any(|window| window == message_lifecycle.as_slice()));
        assert!(types.contains(&"response.reasoning_summary_part.added"));
        assert!(types.contains(&"response.reasoning_summary_text.done"));
        assert!(types.contains(&"response.function_call_arguments.done"));
        assert!(types.contains(&"response.custom_tool_call_input.done"));
        let added_message = events
            .iter()
            .find(|event| {
                event.get("type").and_then(Value::as_str) == Some("response.output_item.added")
                    && event.pointer("/item/type").and_then(Value::as_str) == Some("message")
            })
            .expect("added message");
        assert_eq!(added_message["item"]["status"], "in_progress");
        assert!(added_message["item"]["content"]
            .as_array()
            .is_some_and(Vec::is_empty));
        assert!(events.iter().enumerate().all(|(index, event)| {
            event.get("sequence_number").and_then(Value::as_u64) == Some(index as u64)
        }));
    }

    #[test]
    fn failed_and_incomplete_json_do_not_synthesize_completed_item_closures() {
        for status in ["failed", "incomplete"] {
            let response = json!({
                "id": "resp_noncompleted", "status": status,
                "output": [{"id": "msg_partial", "type": "message",
                    "status": "in_progress", "role": "assistant", "content": []}]
            });
            let events = websocket_json_response_events(&response);
            let expected_terminal = format!("response.{status}");

            assert_eq!(events.len(), 2);
            assert_eq!(events[0]["type"], "response.created");
            assert_eq!(
                events[1]["type"].as_str(),
                Some(expected_terminal.as_str())
            );
            assert!(!events.iter().any(|event| {
                event.get("type").and_then(Value::as_str)
                    == Some("response.output_item.done")
            }));
        }
    }
}

#[cfg(test)]
mod compaction_mode_tests {
    use super::*;
    use crate::config::ProviderDefinition;

    fn candidate(id: &str, base_url: &str, source: CredentialSource) -> RouteCandidate {
        let mut provider = ProviderDefinition::default();
        provider.id = id.into();
        provider.base_url = base_url.into();
        provider.protocol = ProviderProtocol::Responses;
        provider.transport = ProviderTransport::Standard;
        provider.credential.source = source;
        let capabilities = provider.capabilities.clone();
        RouteCandidate {
            provider,
            upstream_model: "gpt-test".into(),
            exposed_model: "gpt-test".into(),
            credential: None,
            account_id: None,
            target_key: "test/gpt-test".into(),
            route_id: None,
            failure_threshold: 0,
            quota_threshold_percent: 0,
            input_price_per_million: None,
            output_price_per_million: None,
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            capabilities,
        }
    }

    #[test]
    fn standalone_and_native_compaction_use_distinct_canonical_endpoints() {
        let mut pooled_chatgpt = candidate(
            "openai",
            "https://chatgpt.com/backend-api/codex/",
            CredentialSource::Forward,
        );
        pooled_chatgpt.credential = Some(ProviderCredential {
            source: CredentialSource::OAuth,
            ..ProviderCredential::default()
        });
        for (route, compact_endpoint, responses_endpoint) in [
            (
                candidate(
                    "custom-forward-id",
                    "https://chatgpt.com/backend-api/codex///",
                    CredentialSource::Forward,
                ),
                "https://chatgpt.com/backend-api/codex/responses/compact",
                "https://chatgpt.com/backend-api/codex/responses",
            ),
            (
                candidate(
                    "openai-api",
                    "https://api.openai.com/v1/",
                    CredentialSource::Environment,
                ),
                "https://api.openai.com/v1/responses/compact",
                "https://api.openai.com/v1/responses",
            ),
            (
                pooled_chatgpt,
                "https://chatgpt.com/backend-api/codex/responses/compact",
                "https://chatgpt.com/backend-api/codex/responses",
            ),
        ] {
            assert_eq!(route.provider.compact_endpoint(), compact_endpoint);
            assert_eq!(
                route.provider.endpoint_for_model(&route.upstream_model),
                responses_endpoint
            );
            assert_eq!(
                candidate_compaction_mode(&route, CompactionRequestKind::Standalone),
                CompactionMode::CompactEndpoint
            );
            assert_eq!(
                candidate_compaction_mode(&route, CompactionRequestKind::NativeTrigger),
                CompactionMode::Responses
            );
        }
    }

    #[test]
    fn responses_shaped_gateways_and_non_key_credentials_use_local_compaction() {
        for (id, base_url, source) in [
            ("openai-api", "https://gateway.example/v1", CredentialSource::Environment),
            ("gateway", "https://api.openai.com/v1", CredentialSource::Environment),
            ("openai-api", "https://api.openai.com/v1", CredentialSource::OAuth),
            ("openai", "https://gateway.example/backend-api/codex", CredentialSource::Forward),
            ("openai", "https://user@chatgpt.com/backend-api/codex", CredentialSource::Forward),
            ("openai", "https://chatgpt.com/backend-api/codex?mode=compact", CredentialSource::Forward),
            ("openai", "https://chatgpt.com/backend-api/codex#compact", CredentialSource::Forward),
        ] {
            for request_kind in [
                CompactionRequestKind::Standalone,
                CompactionRequestKind::NativeTrigger,
            ] {
                assert_eq!(
                    candidate_compaction_mode(&candidate(id, base_url, source), request_kind),
                    CompactionMode::Local,
                    "{id} at {base_url} must not be treated as native compaction",
                );
            }
        }
    }
}
