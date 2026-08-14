use super::*;

pub fn responses_to_chat(body: &Value, upstream_model: &str) -> Result<Value, String> {
    let object = body
        .as_object()
        .ok_or_else(|| "Responses request must be a JSON object".to_string())?;
    let mut messages = Vec::new();

    if let Some(instructions) = object.get("instructions").and_then(Value::as_str) {
        if !instructions.is_empty() {
            messages.push(json!({"role": "system", "content": instructions}));
        }
    }

    match object.get("input") {
        Some(Value::String(text)) => messages.push(json!({"role": "user", "content": text})),
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(message) = response_item_to_chat_message(item)? {
                    push_chat_message(&mut messages, message);
                }
            }
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

    if let Some(tools) = object.get("tools") {
        match tools {
            Value::Array(tools) => {
                let converted = tools
                    .iter()
                    .map(response_tool_to_chat)
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>();
                if !converted.is_empty() {
                    chat.insert("tools".into(), Value::Array(converted));
                }
            }
            Value::Null => {}
            _ => return Err("Responses tools must be an array".into()),
        }
    }
    if let Some(tool_choice) = object.get("tool_choice") {
        if !tool_choice.is_null() {
            let converted = response_tool_choice_to_chat(tool_choice)
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

pub(crate) fn response_item_to_chat_message(item: &Value) -> Result<Option<Value>, String> {
    let Some(object) = item.as_object() else {
        return Err("Responses input items must be objects".into());
    };
    match object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message")
    {
        "message" => {
            let role = match object.get("role").and_then(Value::as_str).unwrap_or("user") {
                "developer" => "system",
                value => value,
            };
            let content = response_content_to_chat(object.get("content"))?;
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
            let output = value_to_text(object.get("output").unwrap_or(&Value::Null));
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
                    "function": {"name": name, "arguments": arguments}
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
            // Codex App represents local tool invocations as custom_tool_call
            // items; the arguments live in the `input` field (a string), which
            // is the wire equivalent of function_call.arguments.
            let arguments = object
                .get("input")
                .map(payload_to_arguments)
                .or_else(|| object.get("arguments").map(payload_to_arguments))
                .unwrap_or_else(|| "{}".to_string());
            let mut message = json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments}
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
        "compaction" => match decode_summary(item) {
            Ok(Some(summary)) => Ok(Some(json!({"role": "system", "content": summary}))),
            _ => Ok(None),
        },
        "reasoning" => Ok(None),
        _ => Ok(None),
    }
}

pub(crate) fn push_chat_message(messages: &mut Vec<Value>, mut message: Value) {
    let is_tool_call = message.get("role").and_then(Value::as_str) == Some("assistant")
        && message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some();
    if is_tool_call {
        if let Some(previous) = messages.last_mut() {
            let previous_is_tool_call = previous.get("role").and_then(Value::as_str)
                == Some("assistant")
                && previous
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .is_some();
            if previous_is_tool_call {
                if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
                    previous["tool_calls"]
                        .as_array_mut()
                        .expect("tool_calls was checked above")
                        .append(calls);
                }
                if previous.get("reasoning_content").is_none() {
                    if let Some(reasoning) = message.get("reasoning_content") {
                        previous["reasoning_content"] = reasoning.clone();
                    }
                }
                return;
            }
        }
    }
    messages.push(message);
}

pub(crate) fn normalize_chat_reasoning_history(body: &mut Value, preserve: bool) {
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
                                pending_reasoning.push_str(value);
                            }
                        }
                    }
                }
            }
            continue;
        }

        let is_assistant_message = item.get("type").and_then(Value::as_str) == Some("message")
            && item.get("role").and_then(Value::as_str) == Some("assistant");
        if is_assistant_message && response_message_content_is_empty(&item) {
            continue;
        }

        if preserve && !pending_reasoning.is_empty() {
            let accepts_reasoning = matches!(
                item.get("type").and_then(Value::as_str),
                Some("function_call" | "custom_tool_call")
            ) || is_assistant_message;
            if accepts_reasoning {
                if let Some(object) = item.as_object_mut() {
                    object.insert(
                        "codetas_reasoning_content".into(),
                        Value::String(std::mem::take(&mut pending_reasoning)),
                    );
                }
            } else {
                pending_reasoning.clear();
            }
        }
        normalized.push(item);
    }
    *input = normalized;
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

pub(crate) fn response_tool_to_chat(tool: &Value) -> Result<Option<Value>, String> {
    let object = tool
        .as_object()
        .ok_or_else(|| "Responses tools must be objects".to_string())?;
    let tool_type = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "Responses tools require a type".to_string())?;
    if tool_type != "function" {
        // Chat Completions has no wire representation for Responses custom,
        // namespace, or hosted tools. Codex includes these opportunistically
        // even when a turn does not require them, so rejecting the complete
        // request makes otherwise compatible providers unusable. Preserve all
        // representable function tools and omit only the incompatible entries.
        return Ok(None);
    }
    let name = object
        .get("name")
        .cloned()
        .ok_or_else(|| "function tool requires a name".to_string())?;
    let description = object
        .get("description")
        .cloned()
        .unwrap_or(Value::String(String::new()));
    let parameters = object
        .get("parameters")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object"}));
    let mut function = json!({
        "name": name,
        "description": description,
        "parameters": parameters,
    });
    if let Some(strict) = object.get("strict") {
        function["strict"] = strict.clone();
    }
    Ok(Some(json!({"type": "function", "function": function})))
}

pub(crate) fn response_tool_choice_to_chat(value: &Value) -> Option<Value> {
    if value.is_string() {
        return Some(value.clone());
    }
    let object = value.as_object()?;
    if object.get("type").and_then(Value::as_str) == Some("function") {
        let name = object.get("name")?.clone();
        return Some(json!({"type": "function", "function": {"name": name}}));
    }
    None
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
                }
            }
            if text.is_empty() {
                serde_json::to_string(value).unwrap_or_default()
            } else {
                text
            }
        }
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}
