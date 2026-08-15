use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompactionMode {
    Local,
    Responses,
    CompactEndpoint,
}

pub(crate) fn candidate_compaction_mode(candidate: &RouteCandidate) -> CompactionMode {
    let credential_source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let is_openai = matches!(
        candidate.provider.id.as_str(),
        "openai" | "openai-api" | "openai-apikey"
    );
    let is_responses = candidate.provider.transport == ProviderTransport::Standard
        && candidate
            .provider
            .protocol_for_model(&candidate.upstream_model)
            == ProviderProtocol::Responses
        && candidate.provider.azure_deployment.is_none();
    if !is_openai || !is_responses {
        return CompactionMode::Local;
    }
    if credential_source == CredentialSource::Forward {
        // ChatGPT (Codex subscription) のコンパクションは通常の /responses に送る
        // （専用の /responses/compact は chatgpt.com には存在しない）。
        CompactionMode::Responses
    } else {
        CompactionMode::CompactEndpoint
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
    // Reconstruct `output` for the context cache: the ChatGPT backend streams output
    // items via `response.output_item.done` and emits `response.completed` with an
    // empty `output`. Without reconstruction the next turn's `previous_response_id`
    // context would drop the tool calls (e.g. `exec`), orphaning their outputs and
    // 400ing / looping.
    let mut completed_items: std::collections::BTreeMap<usize, serde_json::Value> = Default::default();
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
            if kind == "response.output_item.done" {
                if let (Some(index), Some(item)) = (
                    value.get("output_index").and_then(Value::as_u64),
                    value.get("item").cloned(),
                ) {
                    completed_items.insert(index as usize, item);
                }
            }
            if kind == "response.completed" {
                if !completed_items.is_empty() {
                    if let Some(object) = value.get_mut("response").and_then(Value::as_object_mut) {
                        let items: Vec<Value> = completed_items.values().cloned().collect();
                        object.insert("output".into(), Value::Array(items));
                    }
                }
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
            terminal_seen = matches!(
                kind.as_str(),
                "response.completed" | "response.failed" | "response.incomplete"
            );
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

pub(crate) fn websocket_json_response_events(response: &Value) -> Vec<Value> {
    let mut in_progress = response.clone();
    in_progress["status"] = Value::String("in_progress".into());
    in_progress["output"] = Value::Array(Vec::new());
    let mut events = vec![json!({
        "type": "response.created",
        "sequence_number": 0,
        "response": in_progress,
    })];
    for (index, item) in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        events.push(json!({
            "type": "response.output_item.added",
            "sequence_number": events.len(),
            "output_index": index,
            "item": item,
        }));
        if item.get("type").and_then(Value::as_str) == Some("message") {
            for (content_index, part) in item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    events.push(json!({
                        "type": "response.output_text.delta",
                        "sequence_number": events.len(),
                        "item_id": item.get("id").cloned().unwrap_or(Value::Null),
                        "output_index": index,
                        "content_index": content_index,
                        "delta": text,
                    }));
                    events.push(json!({
                        "type": "response.output_text.done",
                        "sequence_number": events.len(),
                        "item_id": item.get("id").cloned().unwrap_or(Value::Null),
                        "output_index": index,
                        "content_index": content_index,
                        "text": text,
                    }));
                }
            }
        }
        events.push(json!({
            "type": "response.output_item.done",
            "sequence_number": events.len(),
            "output_index": index,
            "item": item,
        }));
    }
    let status = match response.get("status").and_then(Value::as_str) {
        Some("failed") => "failed",
        Some("incomplete") => "incomplete",
        _ => "completed",
    };
    let mut terminal = response.clone();
    terminal["status"] = Value::String(status.into());
    events.push(json!({
        "type": format!("response.{status}"),
        "sequence_number": events.len(),
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
