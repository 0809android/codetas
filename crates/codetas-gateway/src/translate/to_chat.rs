use super::*;

pub fn responses_to_chat(body: &Value, upstream_model: &str) -> Result<Value, String> {
    responses_to_chat_with_options(body, upstream_model, false)
}

pub fn responses_to_chat_with_options(
    body: &Value,
    upstream_model: &str,
    require_reasoning_placeholder: bool,
) -> Result<Value, String> {
    let object = body
        .as_object()
        .ok_or_else(|| "Responses request must be a JSON object".to_string())?;
    let tool_map = response_tool_map(body);
    let mut messages = Vec::new();

    if let Some(instructions) = object.get("instructions").and_then(Value::as_str) {
        if !instructions.is_empty() {
            messages.push(json!({"role": "system", "content": instructions}));
        }
    }

    match object.get("input") {
        Some(Value::String(text)) => messages.push(json!({"role": "user", "content": text})),
        Some(Value::Array(items)) => {
            let mut history =
                ChatHistoryAssembler::new(messages, require_reasoning_placeholder);
            for item in items {
                let is_tool_output = matches!(
                    item.get("type").and_then(Value::as_str),
                    Some(
                        "function_call_output"
                            | "custom_tool_call_output"
                            | "tool_search_output"
                    )
                );
                if let Some(message) = response_item_to_chat_message(item, &tool_map)? {
                    if is_tool_output {
                        history.push_tool_result(
                            message,
                            output_to_chat_image_parts(
                                item.get("output").unwrap_or(&Value::Null),
                            ),
                        );
                    } else if message.get("role").and_then(Value::as_str) == Some("assistant") {
                        history.push_assistant(message);
                    } else {
                        history.push_barrier(message);
                    }
                }
            }
            messages = history.finish();
        }
        Some(Value::Null) | None => {}
        Some(_) => return Err("Responses input must be a string or array".into()),
    }

    let mut chat = Map::new();
    chat.insert("model".into(), Value::String(upstream_model.to_string()));
    chat.insert("messages".into(), Value::Array(messages));
    chat.insert(
        "stream".into(),
        Value::Bool(
            object
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
    );

    if object
        .get("tools")
        .is_some_and(|tools| !tools.is_null() && !tools.is_array())
    {
        return Err("Responses tools must be an array".into());
    }
    let allowed_tool_names = object
        .get("tool_choice")
        .and_then(|choice| response_allowed_tool_wire_names(choice, &tool_map));
    let converted = tool_map
        .iter()
        .filter(|(wire_name, _, _)| {
            allowed_tool_names
                .as_ref()
                .map(|allowed| allowed.contains(*wire_name))
                .unwrap_or(true)
        })
        .map(|(wire_name, identity, declaration)| {
            response_tool_to_chat(declaration, wire_name, identity.kind)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !converted.is_empty() {
        chat.insert("tools".into(), Value::Array(converted));
    }
    if let Some(tool_choice) = object.get("tool_choice") {
        if !tool_choice.is_null() {
            let converted = response_tool_choice_to_chat(tool_choice, &tool_map)
                .ok_or_else(|| "unsupported Responses tool_choice".to_string())?;
            chat.insert("tool_choice".into(), converted);
        }
    }
    copy_field(
        object,
        &mut chat,
        "parallel_tool_calls",
        "parallel_tool_calls",
    );
    copy_field(object, &mut chat, "temperature", "temperature");
    copy_field(object, &mut chat, "top_p", "top_p");
    copy_field(object, &mut chat, "prompt_cache_key", "prompt_cache_key");
    copy_field(object, &mut chat, "max_output_tokens", "max_tokens");
    if chat.get("stream").and_then(Value::as_bool) == Some(true) {
        chat.insert("stream_options".into(), json!({"include_usage": true}));
    }
    Ok(Value::Object(chat))
}

pub(crate) fn response_item_to_chat_message(
    item: &Value,
    tool_map: &ResponseToolMap,
) -> Result<Option<Value>, String> {
    let Some(object) = item.as_object() else {
        return Err("Responses input items must be objects".into());
    };
    match object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message")
    {
        "message" => {
            let content = response_content_to_chat(object.get("content"))?;
            let role = match object.get("role").and_then(Value::as_str).unwrap_or("user") {
                "developer" if chat_content_has_image(&content) => "user",
                "developer" => "system",
                value => value,
            };
            let mut message = json!({"role": role, "content": content});
            if let Some(reasoning) = object
                .get("codetas_reasoning_content")
                .and_then(Value::as_str)
            {
                message["reasoning_content"] = Value::String(reasoning.to_string());
            }
            Ok(Some(message))
        }
        "function_call_output" => {
            let call_id = object
                .get("call_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "function_call_output requires call_id".to_string())?;
            let output = output_to_text(object.get("output").unwrap_or(&Value::Null));
            Ok(Some(
                json!({"role": "tool", "tool_call_id": call_id, "content": output}),
            ))
        }
        "function_call" => {
            let call_id = object
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or("call_unknown");
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let namespace = object.get("namespace").and_then(Value::as_str);
            let wire_name = tool_map
                .wire_name_for_identity(name, namespace, ResponseToolKind::Function)
                .map(str::to_string)
                .unwrap_or_else(|| ResponseToolMap::wire_name(namespace, name));
            let arguments = object
                .get("arguments")
                .map(payload_to_arguments)
                .unwrap_or_else(|| "{}".to_string());
            let mut message = json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": {"name": wire_name, "arguments": arguments}
                }]
            });
            if let Some(reasoning) = object
                .get("codetas_reasoning_content")
                .and_then(Value::as_str)
            {
                message["reasoning_content"] = Value::String(reasoning.to_string());
            }
            Ok(Some(message))
        }
        "custom_tool_call" => {
            let call_id = object
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or("call_unknown");
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let namespace = object.get("namespace").and_then(Value::as_str);
            let wire_name = tool_map
                .wire_name_for_identity(name, namespace, ResponseToolKind::Custom)
                .map(str::to_string)
                .unwrap_or_else(|| ResponseToolMap::wire_name(namespace, name));
            // Codex App represents local tool invocations as custom_tool_call
            // items; the arguments live in the `input` field (a string), which
            // is the wire equivalent of function_call.arguments.
            let input = object
                .get("input")
                .or_else(|| object.get("arguments"))
                .cloned()
                .unwrap_or(Value::String(String::new()));
            let arguments = serde_json::to_string(&json!({"input": input}))
                .unwrap_or_else(|_| "{\"input\":\"\"}".to_string());
            let mut message = json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": {"name": wire_name, "arguments": arguments}
                }]
            });
            if let Some(reasoning) = object
                .get("codetas_reasoning_content")
                .and_then(Value::as_str)
            {
                message["reasoning_content"] = Value::String(reasoning.to_string());
            }
            Ok(Some(message))
        }
        "custom_tool_call_output" => {
            let call_id = object
                .get("call_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "custom_tool_call_output requires call_id".to_string())?;
            let output = output_to_text(object.get("output").unwrap_or(&Value::Null));
            Ok(Some(
                json!({"role": "tool", "tool_call_id": call_id, "content": output}),
            ))
        }
        "tool_search_call" => {
            let call_id = object
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or("call_unknown");
            let arguments = object
                .get("arguments")
                .map(payload_to_arguments)
                .unwrap_or_else(|| "{}".to_string());
            let wire_name = tool_map
                .wire_name_for_identity("tool_search", None, ResponseToolKind::ToolSearch)
                .unwrap_or("tool_search");
            let mut message = json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": {"name": wire_name, "arguments": arguments}
                }]
            });
            if let Some(reasoning) = object
                .get("codetas_reasoning_content")
                .and_then(Value::as_str)
            {
                message["reasoning_content"] = Value::String(reasoning.to_string());
            }
            Ok(Some(message))
        }
        "tool_search_output" => {
            let call_id = object
                .get("call_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "tool_search_output requires call_id".to_string())?;
            let output = tool_search_output_to_text(item, tool_map);
            Ok(Some(
                json!({"role": "tool", "tool_call_id": call_id, "content": output}),
            ))
        }
        "compaction" => match decode_summary(item) {
            Ok(Some(summary)) => Ok(Some(json!({"role": "system", "content": summary}))),
            _ => Ok(None),
        },
        "reasoning" => Ok(None),
        _ => Ok(None),
    }
}

#[derive(Debug)]
struct PendingChatToolCall {
    source_id: String,
    wire_id: String,
    name: String,
    result: Option<Value>,
    images: Vec<Value>,
}

#[derive(Debug)]
struct PendingChatToolRound {
    calls: Vec<PendingChatToolCall>,
    deferred_messages: Vec<Value>,
}

/// Reconstruct valid Chat Completions history from Responses output items.
///
/// Responses streams may persist assistant text and tool calls in either order. Strict
/// Chat providers require one assistant `tool_calls` message followed immediately by one
/// `tool` message for every call. Keeping the whole round here avoids invalid histories such
/// as `tool_call -> assistant progress -> tool_result`, including parallel calls and tool
/// result images.
struct ChatHistoryAssembler {
    messages: Vec<Value>,
    system_message: Option<Value>,
    pending_assistant: Option<Value>,
    pending_assistant_calls: Vec<PendingChatToolCall>,
    pending_round: Option<PendingChatToolRound>,
    seen_call_ids: BTreeSet<String>,
    known_call_names: BTreeMap<String, String>,
    minted_call_id: u64,
    require_reasoning_placeholder: bool,
}

impl ChatHistoryAssembler {
    fn new(messages: Vec<Value>, require_reasoning_placeholder: bool) -> Self {
        let mut history = Self {
            messages: Vec::new(),
            system_message: None,
            pending_assistant: None,
            pending_assistant_calls: Vec::new(),
            pending_round: None,
            seen_call_ids: BTreeSet::new(),
            known_call_names: BTreeMap::new(),
            minted_call_id: 0,
            require_reasoning_placeholder,
        };
        for message in messages {
            if message.get("role").and_then(Value::as_str) == Some("system") {
                history.push_system(message);
            } else {
                history.messages.push(message);
            }
        }
        history
    }

    fn push_assistant(&mut self, mut message: Value) {
        if self.pending_round.is_some() {
            self.flush_tool_round();
        }

        let calls = message
            .get_mut("tool_calls")
            .and_then(Value::as_array_mut)
            .map(std::mem::take)
            .unwrap_or_default();
        let mut normalized_calls = Vec::with_capacity(calls.len());
        for mut call in calls {
            let source_id = call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let wire_id = self.unique_call_id(&source_id);
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            if !source_id.is_empty() {
                self.known_call_names
                    .entry(source_id.clone())
                    .or_insert_with(|| name.clone());
            }
            call["id"] = Value::String(wire_id.clone());
            normalized_calls.push(call);
            self.pending_assistant_calls.push(PendingChatToolCall {
                source_id,
                wire_id,
                name,
                result: None,
                images: Vec::new(),
            });
        }
        if !normalized_calls.is_empty() {
            message["tool_calls"] = Value::Array(normalized_calls);
            if self.require_reasoning_placeholder
                && message
                    .get("reasoning_content")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
            {
                message["reasoning_content"] = Value::String(" ".into());
            }
        }

        if let Some(assistant) = self.pending_assistant.as_mut() {
            merge_assistant_message(assistant, message);
        } else {
            self.pending_assistant = Some(message);
        }
    }

    fn push_barrier(&mut self, message: Value) {
        if message.get("role").and_then(Value::as_str) == Some("system") {
            self.push_system(message);
            return;
        }
        self.flush_assistant();
        if let Some(round) = self.pending_round.as_mut() {
            round.deferred_messages.push(message);
        } else {
            self.messages.push(message);
        }
    }

    fn push_tool_result(&mut self, mut message: Value, images: Vec<Value>) {
        self.flush_assistant();
        let source_id = message
            .get("tool_call_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        if let Some(round) = self.pending_round.as_mut() {
            if let Some(call) = round
                .calls
                .iter_mut()
                .find(|call| call.source_id == source_id && call.result.is_none())
            {
                message["tool_call_id"] = Value::String(call.wire_id.clone());
                call.result = Some(message);
                call.images = images;
                if round.calls.iter().all(|call| call.result.is_some()) {
                    self.flush_tool_round();
                }
                return;
            }
            self.flush_tool_round();
        }

        self.push_orphan_tool_result(message, images, &source_id);
    }

    fn finish(mut self) -> Vec<Value> {
        self.flush_assistant();
        self.flush_tool_round();
        if let Some(system) = self.system_message.take() {
            self.messages.insert(0, system);
        }
        self.messages
    }

    fn flush_assistant(&mut self) {
        let Some(message) = self.pending_assistant.take() else {
            return;
        };
        self.messages.push(message);
        if !self.pending_assistant_calls.is_empty() {
            self.pending_round = Some(PendingChatToolRound {
                calls: std::mem::take(&mut self.pending_assistant_calls),
                deferred_messages: Vec::new(),
            });
        }
    }

    fn flush_tool_round(&mut self) {
        let Some(round) = self.pending_round.take() else {
            return;
        };
        let mut images = Vec::new();
        for call in round.calls {
            let PendingChatToolCall {
                wire_id,
                name,
                result,
                images: call_images,
                ..
            } = call;
            self.messages.push(result.unwrap_or_else(|| {
                json!({
                    "role": "tool",
                    "tool_call_id": wire_id,
                    "content": format!(
                        "[codetas] no tool result was recorded for {:?}; execution status unknown — do not treat this as success, failure, or user-provided input.",
                        name
                    )
                })
            }));
            images.extend(call_images);
        }
        if !images.is_empty() {
            push_chat_tool_result_image_carrier(&mut self.messages, images);
        }
        self.messages.extend(round.deferred_messages);
    }

    fn push_orphan_tool_result(
        &mut self,
        mut message: Value,
        images: Vec<Value>,
        source_id: &str,
    ) {
        let wire_id = self.unique_call_id(source_id);
        let name = self
            .known_call_names
            .get(source_id)
            .map(String::as_str)
            .unwrap_or("tool_result");
        let name = safe_chat_tool_name(name);
        message["tool_call_id"] = Value::String(wire_id.clone());
        let mut assistant = json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": wire_id,
                "type": "function",
                "function": {"name": name, "arguments": "{}"}
            }]
        });
        if self.require_reasoning_placeholder {
            assistant["reasoning_content"] = Value::String(" ".into());
        }
        self.messages.push(assistant);
        self.messages.push(message);
        if !images.is_empty() {
            push_chat_tool_result_image_carrier(&mut self.messages, images);
        }
    }

    fn unique_call_id(&mut self, requested: &str) -> String {
        if !requested.is_empty() && self.seen_call_ids.insert(requested.to_string()) {
            return requested.to_string();
        }
        loop {
            self.minted_call_id = self.minted_call_id.saturating_add(1);
            let candidate = format!("call_codetas_{}", self.minted_call_id);
            if self.seen_call_ids.insert(candidate.clone()) {
                return candidate;
            }
        }
    }

    fn push_system(&mut self, message: Value) {
        let next = chat_content_to_text(message.get("content").unwrap_or(&Value::Null));
        if next.is_empty() {
            return;
        }
        if let Some(system) = self.system_message.as_mut() {
            let existing = chat_content_to_text(system.get("content").unwrap_or(&Value::Null));
            system["content"] = Value::String(if existing.is_empty() {
                next
            } else {
                format!("{existing}\n\n{next}")
            });
        } else {
            self.system_message = Some(json!({"role": "system", "content": next}));
        }
    }
}

fn safe_chat_tool_name(name: &str) -> String {
    let sanitized = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "tool_result".into()
    } else {
        sanitized
    }
}

fn merge_assistant_message(target: &mut Value, mut incoming: Value) {
    let incoming_content = incoming
        .as_object_mut()
        .and_then(|object| object.remove("content"))
        .unwrap_or(Value::Null);
    merge_chat_content(target, incoming_content);

    if let Some(calls) = incoming
        .as_object_mut()
        .and_then(|object| object.remove("tool_calls"))
        .and_then(|calls| calls.as_array().cloned())
    {
        if let Some(existing) = target.get_mut("tool_calls").and_then(Value::as_array_mut) {
            existing.extend(calls);
        } else if !calls.is_empty() {
            target["tool_calls"] = Value::Array(calls);
        }
    }

    if let Some(reasoning) = incoming
        .get("reasoning_content")
        .and_then(Value::as_str)
        .filter(|reasoning| !reasoning.is_empty())
        .map(str::to_string)
    {
        let existing = target
            .get("reasoning_content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if existing.trim().is_empty() && !reasoning.trim().is_empty() {
            target["reasoning_content"] = Value::String(reasoning);
        } else if !existing.trim().is_empty()
            && !reasoning.trim().is_empty()
            && existing != reasoning
        {
            target["reasoning_content"] = Value::String(format!("{existing}\n{reasoning}"));
        } else if existing.is_empty() {
            target["reasoning_content"] = Value::String(reasoning);
        }
    }
}

fn merge_chat_content(target: &mut Value, incoming: Value) {
    if chat_content_is_empty(&incoming) {
        return;
    }
    let existing = target
        .as_object_mut()
        .and_then(|object| object.remove("content"))
        .unwrap_or(Value::Null);
    let merged = match (existing, incoming) {
        (Value::Null, next) => next,
        (Value::String(current), Value::String(next)) => Value::String(format!("{current}{next}")),
        (current, next) => {
            let mut parts = chat_content_parts(current);
            parts.extend(chat_content_parts(next));
            Value::Array(parts)
        }
    };
    target["content"] = merged;
}

fn chat_content_is_empty(content: &Value) -> bool {
    match content {
        Value::Null => true,
        Value::String(text) => text.is_empty(),
        Value::Array(parts) => parts.is_empty(),
        _ => false,
    }
}

fn chat_content_to_text(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => output_to_text(other),
    }
}

fn chat_content_has_image(content: &Value) -> bool {
    content.as_array().is_some_and(|parts| {
        parts.iter().any(|part| {
            part.get("type").and_then(Value::as_str) == Some("image_url")
        })
    })
}

fn chat_content_parts(content: Value) -> Vec<Value> {
    match content {
        Value::Null => Vec::new(),
        Value::String(text) if text.is_empty() => Vec::new(),
        Value::String(text) => vec![json!({"type": "text", "text": text})],
        Value::Array(parts) => parts,
        other => vec![json!({"type": "text", "text": other.to_string()})],
    }
}

pub(crate) fn normalize_chat_reasoning_history(
    body: &mut Value,
    preserve: bool,
    require_tool_call_placeholder: bool,
) {
    let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    let mut normalized = Vec::with_capacity(input.len());
    let mut pending_reasoning = String::new();

    for mut item in input.drain(..) {
        if item.get("type").and_then(Value::as_str) == Some("reasoning") {
            if preserve {
                for field in ["summary", "content"] {
                    if let Some(parts) = item.get(field).and_then(Value::as_array) {
                        for part in parts {
                            if let Some(value) = part
                                .get("text")
                                .or_else(|| part.get("summary_text"))
                                .and_then(Value::as_str)
                            {
                                if !pending_reasoning.is_empty() && !value.is_empty() {
                                    pending_reasoning.push('\n');
                                }
                                pending_reasoning.push_str(value);
                            }
                        }
                    }
                }
            }
            continue;
        }

        let is_tool_output = matches!(
            item.get("type").and_then(Value::as_str),
            Some(
                "function_call_output"
                    | "custom_tool_call_output"
                    | "tool_search_output"
                    | "local_shell_call_output"
            )
        );
        if preserve && is_tool_output && !pending_reasoning.is_empty() {
            if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                attach_pending_reasoning_to_call_owner(
                    &mut normalized,
                    call_id,
                    &mut pending_reasoning,
                );
            }
            pending_reasoning.clear();
        }

        let is_assistant_message = item.get("type").and_then(Value::as_str) == Some("message")
            && item.get("role").and_then(Value::as_str) == Some("assistant");
        if is_assistant_message && response_message_content_is_empty(&item) {
            continue;
        }

        if preserve {
            let is_tool_call = matches!(
                item.get("type").and_then(Value::as_str),
                Some("function_call" | "custom_tool_call" | "tool_search_call")
            );
            let accepts_reasoning = is_tool_call || is_assistant_message;
            if accepts_reasoning {
                if let Some(object) = item.as_object_mut() {
                    let mut reasoning = std::mem::take(&mut pending_reasoning);
                    if reasoning.is_empty() && require_tool_call_placeholder && is_tool_call {
                        reasoning.push(' ');
                    }
                    if !reasoning.is_empty() {
                        object.insert(
                            "codetas_reasoning_content".into(),
                            Value::String(reasoning),
                        );
                    }
                }
            } else if !is_tool_output {
                pending_reasoning.clear();
            }
        }
        normalized.push(item);
    }
    *input = normalized;
}

fn attach_pending_reasoning_to_call_owner(
    items: &mut [Value],
    call_id: &str,
    pending_reasoning: &mut String,
) {
    if call_id.is_empty() || pending_reasoning.is_empty() {
        return;
    }
    for item in items.iter_mut().rev() {
        let is_tool_call = matches!(
            item.get("type").and_then(Value::as_str),
            Some("function_call" | "custom_tool_call" | "tool_search_call")
        );
        if !is_tool_call || item.get("call_id").and_then(Value::as_str) != Some(call_id) {
            continue;
        }
        if let Some(object) = item.as_object_mut() {
            object.insert(
                "codetas_reasoning_content".into(),
                Value::String(std::mem::take(pending_reasoning)),
            );
        }
        return;
    }
}

pub(crate) fn response_message_content_is_empty(item: &Value) -> bool {
    match item.get("content") {
        None | Some(Value::Null) => true,
        Some(Value::String(text)) => text.is_empty(),
        Some(Value::Array(parts)) => parts.iter().all(|part| match part {
            Value::String(text) => text.is_empty(),
            Value::Object(object) => match object.get("type").and_then(Value::as_str) {
                Some("input_text" | "output_text" | "text") => object
                    .get("text")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty),
                Some("refusal") => object
                    .get("refusal")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty),
                Some("input_image" | "image_url") => false,
                _ => true,
            },
            _ => false,
        }),
        Some(_) => false,
    }
}

pub(crate) fn response_content_to_chat(content: Option<&Value>) -> Result<Value, String> {
    match content {
        None | Some(Value::Null) => Ok(Value::String(String::new())),
        Some(Value::String(text)) => Ok(Value::String(text.clone())),
        Some(Value::Array(parts)) => {
            let mut converted = Vec::new();
            for part in parts {
                let Some(object) = part.as_object() else {
                    return Err("message content parts must be objects".into());
                };
                match object.get("type").and_then(Value::as_str) {
                    Some("input_text" | "output_text" | "text") => {
                        if let Some(text) = object.get("text").and_then(Value::as_str) {
                            converted.push(json!({"type": "text", "text": text}));
                        }
                    }
                    Some("refusal") => {
                        if let Some(text) = object.get("refusal").and_then(Value::as_str) {
                            converted.push(json!({"type": "text", "text": text}));
                        }
                    }
                    Some("input_image" | "image_url") => {
                        let url = object
                            .get("image_url")
                            .and_then(|value| {
                                value
                                    .as_str()
                                    .or_else(|| value.get("url").and_then(Value::as_str))
                            })
                            .or_else(|| object.get("url").and_then(Value::as_str));
                        if let Some(url) = url {
                            converted.push(json!({"type": "image_url", "image_url": {"url": url}}));
                        } else {
                            return Err("image content requires image_url".into());
                        }
                    }
                    _ => {}
                }
            }
            if converted.len() == 1 {
                if let Some(text) = converted[0].get("text").and_then(Value::as_str) {
                    return Ok(Value::String(text.to_string()));
                }
            }
            Ok(Value::Array(converted))
        }
        Some(_) => Err("message content must be a string or array".into()),
    }
}

pub(crate) fn response_tool_to_chat(
    tool: &Value,
    wire_name: &str,
    kind: ResponseToolKind,
) -> Result<Value, String> {
    let object = tool
        .as_object()
        .ok_or_else(|| "Responses tools must be objects".to_string())?;
    let description = tool
        .get("description")
        .or_else(|| tool.pointer("/function/description"))
        .cloned()
        .unwrap_or(Value::String(String::new()));
    let parameters = match kind {
        ResponseToolKind::Custom => json!({
            "type": "object",
            "properties": {"input": {"type": "string"}},
            "required": ["input"]
        }),
        ResponseToolKind::ToolSearch => response_tool_parameters(tool)
            .unwrap_or_else(default_tool_search_parameters),
        ResponseToolKind::Function => response_tool_parameters(tool)
            .unwrap_or_else(|| json!({"type": "object"})),
    };
    let mut function = json!({
        "name": wire_name,
        "description": description,
        "parameters": parameters,
    });
    if let Some(strict) = object.get("strict") {
        function["strict"] = strict.clone();
    }
    Ok(json!({"type": "function", "function": function}))
}

pub(crate) fn response_tool_choice_to_chat(
    value: &Value,
    tool_map: &ResponseToolMap,
) -> Option<Value> {
    if value.is_string() {
        return Some(value.clone());
    }
    let object = value.as_object()?;
    match object.get("type").and_then(Value::as_str) {
        Some("function" | "custom") => {
            let name = object.get("name")?.as_str()?;
            let namespace = object.get("namespace").and_then(Value::as_str);
            let kind = if object.get("type").and_then(Value::as_str) == Some("custom") {
                ResponseToolKind::Custom
            } else {
                ResponseToolKind::Function
            };
            let wire_name = tool_map
                .wire_name_for_identity(name, namespace, kind)
                .map(str::to_string)
                .unwrap_or_else(|| ResponseToolMap::wire_name(namespace, name));
            Some(json!({"type": "function", "function": {"name": wire_name}}))
        }
        Some("tool_search") => Some(json!({
            "type": "function",
            "function": {"name": tool_map
                .wire_name_for_identity("tool_search", None, ResponseToolKind::ToolSearch)
                .unwrap_or("tool_search")}
        })),
        Some("allowed_tools") => {
            let allowed = response_allowed_tool_wire_names(value, tool_map)?;
            if allowed.is_empty() {
                Some(Value::String("none".into()))
            } else if object.get("mode").and_then(Value::as_str) == Some("required") {
                Some(Value::String("required".into()))
            } else {
                Some(Value::String("auto".into()))
            }
        }
        _ => None,
    }
}

pub(crate) fn copy_field(
    source: &Map<String, Value>,
    target: &mut Map<String, Value>,
    from: &str,
    to: &str,
) {
    if let Some(value) = source.get(from) {
        target.insert(to.to_string(), value.clone());
    }
}

pub(crate) fn value_to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Converts a Responses tool-call payload into the JSON-encoded arguments
/// string Chat Completions expects. `custom_tool_call.input` and
/// `function_call.arguments` are strings by spec, but a malformed client may
/// send a structured value (object/array/number); JSON-encoding it preserves
/// the payload instead of silently collapsing to "{}".
pub(crate) fn payload_to_arguments(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_string()),
    }
}

/// Converts a Responses tool-call payload (`input` for custom_tool_call,
/// `arguments` for function_call) into the JSON value an object-typed adapter
/// (Anthropic tool_use.input, Gemini functionCall.args, ...) expects. Strings
/// are parsed as JSON when possible; a structured payload is preserved as-is
/// instead of silently collapsing to `{}`.
pub(crate) fn tool_input_to_value(value: &Value) -> Value {
    match value {
        Value::String(text) => {
            serde_json::from_str::<Value>(text).unwrap_or_else(|_| json!({"input": text}))
        }
        other => other.clone(),
    }
}

/// Extracts tool-result text from a Responses `output` value.
///
/// Custom tool outputs (`custom_tool_call_output`) carry either a plain string
/// or an array of output content parts (`input_text` / `output_text`), unlike
/// function tool outputs which are usually a plain string. Joining the textual
/// parts preserves the meaning Codex needs to continue the conversation.
pub(crate) fn output_to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => {
            let mut text = String::new();
            let mut image_count = 0_usize;
            for part in parts {
                let part = part.as_object();
                let kind = part
                    .and_then(|object| object.get("type"))
                    .and_then(Value::as_str);
                if matches!(kind, Some("input_text" | "output_text" | "text")) {
                    if let Some(part_text) = part
                        .and_then(|object| object.get("text"))
                        .and_then(Value::as_str)
                    {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(part_text);
                    }
                } else if matches!(kind, Some("input_image" | "image_url")) {
                    image_count += 1;
                }
            }
            if text.is_empty() && image_count > 0 {
                "[image]".into()
            } else if text.is_empty() {
                serde_json::to_string(value).unwrap_or_default()
            } else {
                text
            }
        }
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn output_to_chat_image_parts(value: &Value) -> Vec<Value> {
    let Some(parts) = value.as_array() else {
        return Vec::new();
    };
    parts
        .iter()
        .filter_map(|part| {
            let object = part.as_object()?;
            if !matches!(
                object.get("type").and_then(Value::as_str),
                Some("input_image" | "image_url")
            ) {
                return None;
            }
            let url = object
                .get("image_url")
                .and_then(|value| {
                    value
                        .as_str()
                        .or_else(|| value.get("url").and_then(Value::as_str))
                })
                .or_else(|| object.get("url").and_then(Value::as_str))?;
            let mut image = json!({"type": "image_url", "image_url": {"url": url}});
            if let Some(detail) = object.get("detail").and_then(Value::as_str) {
                image["image_url"]["detail"] = Value::String(detail.into());
            }
            Some(image)
        })
        .collect()
}

fn push_chat_tool_result_image_carrier(messages: &mut Vec<Value>, images: Vec<Value>) {
    let mut content = vec![json!({
        "type": "text",
        "text": "[codetas] image output from the preceding tool result(s):"
    })];
    content.extend(images);
    messages.push(json!({"role": "user", "content": content}));
}
