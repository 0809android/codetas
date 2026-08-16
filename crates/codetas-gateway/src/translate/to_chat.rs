use super::*;

pub fn responses_to_chat(body: &Value, upstream_model: &str) -> Result<Value, String> {
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
            let mut pending_tool_result_images = Vec::new();
            for item in items {
                let is_tool_output = matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("function_call_output" | "custom_tool_call_output")
                );
                if let Some(message) = response_item_to_chat_message(item, &tool_map)? {
                    if !is_tool_output && !pending_tool_result_images.is_empty() {
                        push_chat_tool_result_image_carrier(
                            &mut messages,
                            std::mem::take(&mut pending_tool_result_images),
                        );
                    }
                    push_chat_message(&mut messages, message);
                }
                if is_tool_output {
                    pending_tool_result_images.extend(output_to_chat_image_parts(
                        item.get("output").unwrap_or(&Value::Null),
                    ));
                }
            }
            if !pending_tool_result_images.is_empty() {
                push_chat_tool_result_image_carrier(
                    &mut messages,
                    pending_tool_result_images,
                );
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

pub(crate) fn push_chat_message(messages: &mut Vec<Value>, mut message: Value) {
    let is_tool_call = message.get("role").and_then(Value::as_str) == Some("assistant")
        && message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some();
    if is_tool_call {
        if let Some(previous) = messages.last_mut() {
            let previous_is_assistant =
                previous.get("role").and_then(Value::as_str) == Some("assistant");
            if previous_is_assistant {
                // Chat providers can return visible content, reasoning, and
                // tool_calls in one assistant message. Responses persists the
                // content and calls as adjacent output items, so reconstruct
                // the original single message before replaying tool results.
                let calls = message
                    .get_mut("tool_calls")
                    .and_then(Value::as_array_mut)
                    .map(std::mem::take)
                    .unwrap_or_default();
                if let Some(previous_calls) = previous
                    .get_mut("tool_calls")
                    .and_then(Value::as_array_mut)
                {
                    previous_calls.extend(calls);
                } else {
                    previous["tool_calls"] = Value::Array(calls);
                }
                let previous_has_reasoning = previous
                    .get("reasoning_content")
                    .and_then(Value::as_str)
                    .is_some_and(|reasoning| !reasoning.is_empty());
                if !previous_has_reasoning {
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

        if preserve {
            let accepts_reasoning = matches!(
                item.get("type").and_then(Value::as_str),
                Some("function_call" | "custom_tool_call" | "tool_search_call")
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
