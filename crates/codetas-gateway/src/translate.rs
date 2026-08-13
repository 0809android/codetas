use crate::compaction::{decode_summary, expand_local_compactions};
use crate::compat::repair_translated_input_items;
use serde_json::{json, Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

/// Rewrite Codex/Responses-only history so Chat, Anthropic, Gemini, and Kiro
/// adapters do not reject the whole turn. ChatGPT passthrough must not use this.
pub fn prepare_translated_responses_request(body: &mut Value, strip_stateful: bool) {
    expand_local_compactions(body);
    if strip_stateful {
        if let Some(object) = body.as_object_mut() {
            object.remove("previous_response_id");
            object.remove("conversation");
        }
    }

    let extra_tools = collect_additional_tools(body);
    if !extra_tools.is_empty() {
        let tools = body
            .as_object_mut()
            .map(|object| object.entry("tools").or_insert_with(|| json!([])));
        if let Some(Value::Array(tools)) = tools {
            for tool in extra_tools {
                if !tools.iter().any(|existing| existing == &tool) {
                    tools.push(tool);
                }
            }
        }
    }

    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    let mut rewritten = Vec::with_capacity(items.len());
    for item in items.iter() {
        if let Some(converted) = rewrite_translated_input_item(item) {
            rewritten.push(converted);
        }
    }
    *items = rewritten;
    repair_translated_input_items(body);
}

fn collect_additional_tools(body: &Value) -> Vec<Value> {
    let Some(items) = body.get("input").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("additional_tools"))
        .flat_map(|item| {
            item.get("tools")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .collect()
}

fn rewrite_translated_input_item(item: &Value) -> Option<Value> {
    let item_type = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message");
    match item_type {
        "compaction_trigger" | "additional_tools" | "item_reference" | "web_search_call" => None,
        "compaction_summary" | "context_compaction" => {
            let mut next = item.clone();
            if let Some(object) = next.as_object_mut() {
                object.insert("type".into(), Value::String("compaction".into()));
            }
            Some(next)
        }
        "agent_message" => {
            let mut next = item.clone();
            if let Some(object) = next.as_object_mut() {
                object.insert("type".into(), Value::String("message".into()));
                object.insert("role".into(), Value::String("user".into()));
                if object.get("content").is_none() {
                    object.insert(
                        "content".into(),
                        json!([{"type": "input_text", "text": "(sub-agent message received)"}]),
                    );
                }
            }
            Some(next)
        }
        "local_shell_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .cloned()
                .unwrap_or_else(|| json!("call_unknown"));
            let command = item
                .pointer("/action/command")
                .cloned()
                .unwrap_or_else(|| json!([]));
            Some(json!({
                "type": "function_call",
                "call_id": call_id,
                "name": "shell",
                "arguments": json!({"command": command}).to_string()
            }))
        }
        "tool_search_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .cloned()
                .unwrap_or_else(|| json!("call_unknown"));
            let arguments = item.get("arguments").cloned().unwrap_or_else(|| json!({}));
            Some(json!({
                "type": "function_call",
                "call_id": call_id,
                "name": "tool_search",
                "arguments": payload_to_arguments(&arguments)
            }))
        }
        "tool_search_output" => {
            let call_id = item.get("call_id").cloned().unwrap_or_else(|| json!(""));
            let tools = item
                .get("tools")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let names = tools
                .iter()
                .filter_map(|tool| tool.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(", ");
            let text = if names.is_empty() {
                "Tool search returned no tools.".to_string()
            } else {
                format!("Tool search loaded: {names}")
            };
            Some(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": text
            }))
        }
        _ => Some(item.clone()),
    }
}

pub fn strip_translated_input_images(body: &mut Value) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    for item in items {
        replace_image_parts(item);
    }
}

fn replace_image_parts(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                replace_image_parts(item);
            }
        }
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("input_image")
                || object.get("type").and_then(Value::as_str) == Some("image_url")
            {
                object.clear();
                object.insert("type".into(), Value::String("input_text".into()));
                object.insert(
                    "text".into(),
                    Value::String("[image omitted for this model]".into()),
                );
                return;
            }
            for child in object.values_mut() {
                replace_image_parts(child);
            }
        }
        _ => {}
    }
}

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

fn response_item_to_chat_message(item: &Value) -> Result<Option<Value>, String> {
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

fn push_chat_message(messages: &mut Vec<Value>, mut message: Value) {
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

        if preserve && !pending_reasoning.is_empty() {
            let is_assistant_message = item.get("type").and_then(Value::as_str) == Some("message")
                && item.get("role").and_then(Value::as_str) == Some("assistant");
            if is_assistant_message && response_message_content_is_empty(&item) {
                continue;
            }

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

fn response_message_content_is_empty(item: &Value) -> bool {
    match item.get("content") {
        None | Some(Value::Null) => true,
        Some(Value::String(text)) => text.is_empty(),
        Some(Value::Array(parts)) => parts.iter().all(|part| {
            part.get("text")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        }),
        Some(_) => false,
    }
}

fn response_content_to_chat(content: Option<&Value>) -> Result<Value, String> {
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

fn response_tool_to_chat(tool: &Value) -> Result<Option<Value>, String> {
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

fn response_tool_choice_to_chat(value: &Value) -> Option<Value> {
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

fn copy_field(source: &Map<String, Value>, target: &mut Map<String, Value>, from: &str, to: &str) {
    if let Some(value) = source.get(from) {
        target.insert(to.to_string(), value.clone());
    }
}

fn value_to_text(value: &Value) -> String {
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
fn output_to_text(value: &Value) -> String {
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

pub fn chat_to_response(
    chat: &Value,
    exposed_model: &str,
    custom_tools: &BTreeSet<String>,
) -> Result<Value, String> {
    let choice = chat
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| "Chat Completions response has no choice".to_string())?;
    let message = choice
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| "Chat Completions response has no message".to_string())?;
    let response_id = response_id();
    let mut output = Vec::new();

    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            output.push(json!({
                "id": format!("msg_{}", Uuid::new_v4().simple()),
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text, "annotations": []}]
            }));
        }
    }
    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in tool_calls {
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let arguments = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            if custom_tools.contains(name) {
                output.push(json!({
                    "id": format!("ctc_{}", Uuid::new_v4().simple()),
                    "type": "custom_tool_call",
                    "status": "completed",
                    "call_id": call.get("id").and_then(Value::as_str).unwrap_or("call_unknown"),
                    "name": name,
                    "input": arguments
                }));
            } else {
                output.push(json!({
                    "id": format!("fc_{}", Uuid::new_v4().simple()),
                    "type": "function_call",
                    "status": "completed",
                    "call_id": call.get("id").and_then(Value::as_str).unwrap_or("call_unknown"),
                    "name": name,
                    "arguments": arguments
                }));
            }
        }
    }

    Ok(json!({
        "id": response_id,
        "object": "response",
        "created_at": unix_seconds(),
        "completed_at": unix_seconds(),
        "status": "completed",
        "instructions": Value::Null,
        "max_output_tokens": Value::Null,
        "model": exposed_model,
        "output": output,
        "parallel_tool_calls": true,
        "previous_response_id": Value::Null,
        "reasoning": {"effort": Value::Null, "summary": Value::Null},
        "store": false,
        "temperature": 1,
        "text": {"format": {"type": "text"}},
        "tool_choice": "auto",
        "tools": [],
        "top_p": 1,
        "truncation": "disabled",
        "usage": chat_usage_to_responses(chat.get("usage")),
        "user": Value::Null,
        "metadata": {},
        "error": Value::Null,
        "incomplete_details": Value::Null
    }))
}

#[derive(Clone)]
struct ToolState {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    output_index: usize,
    is_custom: bool,
    announced: bool,
}

pub struct ChatStreamState {
    response_id: String,
    exposed_model: String,
    message_id: String,
    text: String,
    message_started: bool,
    message_output_index: Option<usize>,
    reasoning_id: String,
    reasoning_text: String,
    reasoning_signature: String,
    reasoning_output_index: Option<usize>,
    next_output_index: usize,
    tools: BTreeMap<usize, ToolState>,
    usage: Value,
    sequence_number: u64,
    custom_tools: BTreeSet<String>,
}

impl ChatStreamState {
    pub fn new(exposed_model: String, custom_tools: BTreeSet<String>) -> (Self, Vec<String>) {
        let response_id = response_id();
        let mut state = Self {
            response_id: response_id.clone(),
            exposed_model: exposed_model.clone(),
            message_id: format!("msg_{}", Uuid::new_v4().simple()),
            text: String::new(),
            message_started: false,
            message_output_index: None,
            reasoning_id: format!("rs_{}", Uuid::new_v4().simple()),
            reasoning_text: String::new(),
            reasoning_signature: String::new(),
            reasoning_output_index: None,
            next_output_index: 0,
            tools: BTreeMap::new(),
            usage: Value::Null,
            sequence_number: 1,
            custom_tools,
        };
        let created = json!({
            "type": "response.created",
            "response": state.response_object("in_progress", Vec::new())
        });
        let created = state.event("response.created", created);
        let in_progress_payload = json!({
            "type": "response.in_progress",
            "response": state.response_object("in_progress", Vec::new())
        });
        let in_progress = state.event("response.in_progress", in_progress_payload);
        (state, vec![created, in_progress])
    }

    pub fn push_chat_chunk(&mut self, chunk: &Value) -> Vec<String> {
        let mut events = Vec::new();
        if let Some(usage) = chunk.get("usage") {
            if !usage.is_null() {
                self.usage = chat_usage_to_responses(Some(usage));
            }
        }
        let Some(delta) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("delta"))
        else {
            return events;
        };

        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            self.ensure_message_started(&mut events);
            self.text.push_str(text);
            let output_index = self.message_output_index.unwrap_or(0);
            let payload = json!({
                "type": "response.output_text.delta",
                "item_id": self.message_id,
                "output_index": output_index,
                "content_index": 0,
                "delta": text
            });
            events.push(self.event("response.output_text.delta", payload));
        }

        if let Some(text) = delta.get("reasoning_content").and_then(Value::as_str) {
            self.ensure_reasoning_started(&mut events);
            self.reasoning_text.push_str(text);
            let output_index = self.reasoning_output_index.unwrap_or(0);
            events.push(self.event(
                "response.reasoning_summary_text.delta",
                json!({
                    "type": "response.reasoning_summary_text.delta",
                    "item_id": self.reasoning_id,
                    "output_index": output_index,
                    "summary_index": 0,
                    "delta": text
                }),
            ));
        }

        if let Some(signature) = delta.get("reasoning_signature").and_then(Value::as_str) {
            self.ensure_reasoning_started(&mut events);
            self.reasoning_signature.push_str(signature);
            let output_index = self.reasoning_output_index.unwrap_or(0);
            events.push(self.event(
                "response.reasoning_signature.delta",
                json!({
                    "type": "response.reasoning_signature.delta",
                    "item_id": self.reasoning_id,
                    "output_index": output_index,
                    "delta": signature
                }),
            ));
        }

        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for tool_call in tool_calls {
                let index = tool_call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let is_new = !self.tools.contains_key(&index);
                if is_new {
                    let call_id = tool_call
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
                    let name = tool_call
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let output_index = self.next_output_index;
                    self.next_output_index += 1;
                    let is_custom = self.custom_tools.contains(&name);
                    let announced = name != "unknown";
                    let state = ToolState {
                        item_id: format!(
                            "{}_{}",
                            if is_custom { "ctc" } else { "fc" },
                            Uuid::new_v4().simple()
                        ),
                        call_id,
                        name,
                        arguments: String::new(),
                        output_index,
                        is_custom,
                        announced,
                    };
                    if announced {
                        events.push(self.event(
                            "response.output_item.added",
                            json!({
                                "type": "response.output_item.added",
                                "output_index": state.output_index,
                                "item": tool_item(&state, "in_progress")
                            }),
                        ));
                    }
                    self.tools.insert(index, state);
                }
                let mut added_payload: Option<Value> = None;
                let mut delta_payload: Option<Value> = None;
                if let Some(state) = self.tools.get_mut(&index) {
                    // A Chat provider may stream the tool name in a later chunk
                    // than the index/id. Re-evaluate the custom-tool
                    // classification when the name first appears instead of
                    // freezing it at the first chunk (where it was "unknown").
                    if let Some(name) = tool_call.pointer("/function/name").and_then(Value::as_str)
                    {
                        if state.name == "unknown" && !name.is_empty() {
                            state.name = name.to_string();
                            state.is_custom = self.custom_tools.contains(&state.name);
                        }
                    }
                    let just_announced = !state.announced && state.name != "unknown";
                    if just_announced {
                        state.announced = true;
                        state.item_id = format!(
                            "{}_{}",
                            if state.is_custom { "ctc" } else { "fc" },
                            Uuid::new_v4().simple()
                        );
                        added_payload = Some(json!({
                            "type": "response.output_item.added",
                            "output_index": state.output_index,
                            "item": tool_item(state, "in_progress")
                        }));
                    }
                    if let Some(arguments) = tool_call
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                    {
                        state.arguments.push_str(arguments);
                        if state.announced {
                            let event_type = if state.is_custom {
                                "response.custom_tool_call_input.delta"
                            } else {
                                "response.function_call_arguments.delta"
                            };
                            let delta = if just_announced {
                                // Announce chunk: the client has not seen any
                                // delta for this item yet, so emit everything
                                // accumulated (including pre-name chunks).
                                state.arguments.clone()
                            } else {
                                arguments.to_string()
                            };
                            delta_payload = Some(json!({
                                "type": event_type,
                                "item_id": state.item_id.clone(),
                                "output_index": state.output_index,
                                "delta": delta
                            }));
                        }
                    } else if just_announced && !state.arguments.is_empty() {
                        // Name arrived without arguments in this chunk; flush
                        // what accumulated while the name was still unknown.
                        let event_type = if state.is_custom {
                            "response.custom_tool_call_input.delta"
                        } else {
                            "response.function_call_arguments.delta"
                        };
                        delta_payload = Some(json!({
                            "type": event_type,
                            "item_id": state.item_id.clone(),
                            "output_index": state.output_index,
                            "delta": state.arguments.clone()
                        }));
                    }
                }
                if let Some(payload) = added_payload {
                    events.push(self.event("response.output_item.added", payload));
                }
                if let Some(payload) = delta_payload {
                    let event_type = payload
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("response.function_call_arguments.delta")
                        .to_string();
                    events.push(self.event(&event_type, payload));
                }
            }
        }
        events
    }

    pub fn finish(mut self) -> Vec<String> {
        let mut events = Vec::new();
        let mut indexed_output = Vec::new();
        if let Some(output_index) = self.reasoning_output_index {
            let item = self.reasoning_item("completed");
            events.push(self.event(
                "response.reasoning_summary_text.done",
                json!({
                    "type": "response.reasoning_summary_text.done",
                    "item_id": self.reasoning_id,
                    "output_index": output_index,
                    "summary_index": 0,
                    "text": self.reasoning_text
                }),
            ));
            events.push(self.event(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "item": item
                }),
            ));
            indexed_output.push((output_index, self.reasoning_item("completed")));
        }
        if let Some(message_output_index) = self.message_output_index {
            let text_done = json!({
                "type": "response.output_text.done",
                "item_id": self.message_id,
                "output_index": message_output_index,
                "content_index": 0,
                "text": self.text
            });
            events.push(self.event("response.output_text.done", text_done));
            let message = self.message_item("completed");
            let content_done = json!({
                "type": "response.content_part.done",
                "item_id": self.message_id,
                "output_index": message_output_index,
                "content_index": 0,
                "part": {"type": "output_text", "text": self.text, "annotations": []}
            });
            events.push(self.event("response.content_part.done", content_done));
            let item_done = json!({
                "type": "response.output_item.done",
                "output_index": message_output_index,
                "item": message
            });
            events.push(self.event("response.output_item.done", item_done));
            indexed_output.push((message_output_index, self.message_item("completed")));
        }
        let tool_states = self.tools.values().cloned().collect::<Vec<_>>();
        for state in &tool_states {
            // If the name never arrived (pathological provider), announce the
            // item now so the client has a matching output_item.added before
            // the done events.
            if !state.announced {
                events.push(self.event(
                    "response.output_item.added",
                    json!({
                        "type": "response.output_item.added",
                        "output_index": state.output_index,
                        "item": tool_item(state, "in_progress")
                    }),
                ));
            }
            let (done_type, done_field) = if state.is_custom {
                ("response.custom_tool_call_input.done", "input")
            } else {
                ("response.function_call_arguments.done", "arguments")
            };
            let arguments_done = json!({
                "type": done_type,
                "item_id": state.item_id,
                "name": state.name,
                "output_index": state.output_index,
                done_field: state.arguments
            });
            events.push(self.event(done_type, arguments_done));
            let item = tool_item(state, "completed");
            let item_done = json!({
                "type": "response.output_item.done",
                "output_index": state.output_index,
                "item": item
            });
            events.push(self.event("response.output_item.done", item_done));
            indexed_output.push((state.output_index, tool_item(state, "completed")));
        }
        indexed_output.sort_by_key(|(index, _)| *index);
        let output = indexed_output
            .into_iter()
            .map(|(_, item)| item)
            .collect::<Vec<_>>();
        let completed = json!({
            "type": "response.completed",
            "response": self.response_object("completed", output)
        });
        events.push(self.event("response.completed", completed));
        events
    }

    pub fn fail(&mut self, message: &str) -> String {
        let mut response = self.response_object("failed", Vec::new());
        response["error"] = json!({
            "code": "upstream_stream_error",
            "message": message
        });
        let payload = json!({
            "type": "response.failed",
            "response": response
        });
        self.event("response.failed", payload)
    }

    fn ensure_message_started(&mut self, events: &mut Vec<String>) {
        if self.message_started {
            return;
        }
        self.message_started = true;
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        self.message_output_index = Some(output_index);
        let item_added = json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": {
                "id": self.message_id,
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": []
            }
        });
        events.push(self.event("response.output_item.added", item_added));
        let content_added = json!({
            "type": "response.content_part.added",
            "item_id": self.message_id,
            "output_index": output_index,
            "content_index": 0,
            "part": {"type": "output_text", "text": "", "annotations": []}
        });
        events.push(self.event("response.content_part.added", content_added));
    }

    fn ensure_reasoning_started(&mut self, events: &mut Vec<String>) {
        if self.reasoning_output_index.is_some() {
            return;
        }
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        self.reasoning_output_index = Some(output_index);
        events.push(self.event(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": self.reasoning_item("in_progress")
            }),
        ));
    }

    fn reasoning_item(&self, status: &str) -> Value {
        let mut item = json!({
            "id": self.reasoning_id,
            "type": "reasoning",
            "status": status,
            "summary": [{"type": "summary_text", "text": self.reasoning_text}]
        });
        if !self.reasoning_signature.is_empty() {
            item["encrypted_content"] = Value::String(self.reasoning_signature.clone());
            item["provider_metadata"] = json!({"anthropic": {"thinking_blocks": [{
                "type": "thinking",
                "thinking": self.reasoning_text,
                "signature": self.reasoning_signature
            }]}});
        }
        item
    }

    fn message_item(&self, status: &str) -> Value {
        json!({
            "id": self.message_id,
            "type": "message",
            "status": status,
            "role": "assistant",
            "content": [{"type": "output_text", "text": self.text, "annotations": []}]
        })
    }

    fn response_object(&self, status: &str, output: Vec<Value>) -> Value {
        let completed_at = if status == "completed" {
            json!(unix_seconds())
        } else {
            Value::Null
        };
        json!({
            "id": self.response_id,
            "object": "response",
            "created_at": unix_seconds(),
            "completed_at": completed_at,
            "status": status,
            "instructions": Value::Null,
            "max_output_tokens": Value::Null,
            "model": self.exposed_model,
            "output": output,
            "parallel_tool_calls": true,
            "previous_response_id": Value::Null,
            "reasoning": {"effort": Value::Null, "summary": Value::Null},
            "store": false,
            "temperature": 1,
            "text": {"format": {"type": "text"}},
            "tool_choice": "auto",
            "tools": [],
            "top_p": 1,
            "truncation": "disabled",
            "usage": self.usage,
            "user": Value::Null,
            "metadata": {},
            "error": Value::Null,
            "incomplete_details": Value::Null
        })
    }

    fn event(&mut self, event: &str, mut payload: Value) -> String {
        if let Some(object) = payload.as_object_mut() {
            object.insert("sequence_number".into(), json!(self.sequence_number));
        }
        self.sequence_number += 1;
        sse(event, &payload)
    }
}

fn tool_item(state: &ToolState, status: &str) -> Value {
    if state.is_custom {
        json!({
            "id": state.item_id,
            "type": "custom_tool_call",
            "status": status,
            "call_id": state.call_id,
            "name": state.name,
            "input": state.arguments
        })
    } else {
        json!({
            "id": state.item_id,
            "type": "function_call",
            "status": status,
            "call_id": state.call_id,
            "name": state.name,
            "arguments": state.arguments
        })
    }
}

pub fn sse(event: &str, payload: &Value) -> String {
    format!(
        "event: {event}\ndata: {}\n\n",
        serde_json::to_string(payload).unwrap_or_else(|_| "{}".into())
    )
}

/// Collects the names of tools declared as custom tools in a Responses request.
///
/// Codex App declares local tools (shell, apply_patch, MCP tools, ...) with
/// `type: "custom"`. Those tool names must round-trip as `custom_tool_call`
/// output items so Codex can route the call to its local tool handler instead
/// of treating it as a hosted function.
pub fn custom_tool_names(body: &Value) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return names;
    };
    for tool in tools {
        let Some(object) = tool.as_object() else {
            continue;
        };
        if object.get("type").and_then(Value::as_str) != Some("custom") {
            continue;
        }
        if let Some(name) = object.get("name").and_then(Value::as_str) {
            names.insert(name.to_string());
        }
    }
    names
}

fn chat_usage_to_responses(usage: Option<&Value>) -> Value {
    let prompt = usage
        .and_then(|value| value.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion = usage
        .and_then(|value| value.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .and_then(|value| value.pointer("/prompt_tokens_details/cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = usage
        .and_then(|value| value.pointer("/completion_tokens_details/reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = usage
        .and_then(|value| value.get("total_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(prompt + completion);
    json!({
        "input_tokens": prompt,
        "input_tokens_details": {"cached_tokens": cached},
        "output_tokens": completion,
        "output_tokens_details": {"reasoning_tokens": reasoning},
        "total_tokens": total
    })
}

fn response_id() -> String {
    format!("resp_{}", Uuid::new_v4().simple())
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepares_codex_control_items_for_translated_providers() {
        let mut request = json!({
            "previous_response_id": "resp_old",
            "conversation": "conv_1",
            "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}}],
            "input": [
                {"type": "compaction_trigger"},
                {"type": "additional_tools", "tools": [{"type": "function", "name": "exec", "parameters": {"type": "object"}}]},
                {"type": "agent_message", "content": [{"type": "input_text", "text": "child done"}]},
                {"type": "local_shell_call", "call_id": "call_shell", "action": {"command": ["pwd"]}},
                {"type": "function_call_output", "call_id": "call_shell", "output": "/tmp"},
                {"type": "function_call_output", "call_id": "call_missing", "output": "orphan"}
            ]
        });
        prepare_translated_responses_request(&mut request, true);
        assert!(request.get("previous_response_id").is_none());
        assert!(request.get("conversation").is_none());
        assert_eq!(request["tools"].as_array().map(Vec::len), Some(2));
        assert_eq!(request["input"][0]["type"], "message");
        assert_eq!(request["input"][1]["type"], "function_call");
        assert_eq!(request["input"][1]["name"], "shell");
        assert_eq!(request["input"][2]["type"], "function_call_output");
        assert_eq!(request["input"][3]["type"], "message");
        assert!(request["input"][3]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("orphan"));

        let chat = responses_to_chat(&request, "deepseek-v4-flash").expect("translated history");
        assert_eq!(chat["messages"][0]["role"], "user");
        assert_eq!(chat["messages"][1]["role"], "assistant");
        assert_eq!(chat["messages"][1]["content"], "");
        assert_eq!(chat["messages"][2]["role"], "tool");
    }

    #[test]
    fn converts_responses_request_to_chat() {
        let request = json!({
            "model": "route/model-a",
            "instructions": "Be concise.",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "Hello"}]
            }],
            "tools": [{
                "type": "function",
                "name": "lookup",
                "description": "Look something up",
                "parameters": {"type": "object", "properties": {}}
            }],
            "max_output_tokens": 200,
            "stream": true
        });

        let chat = responses_to_chat(&request, "model-a").expect("request should translate");
        assert_eq!(chat["model"], "model-a");
        assert_eq!(chat["messages"][0]["role"], "system");
        assert_eq!(chat["messages"][1]["content"], "Hello");
        assert_eq!(chat["tools"][0]["function"]["name"], "lookup");
        assert_eq!(chat["max_tokens"], 200);
        assert_eq!(chat["stream_options"]["include_usage"], true);
    }

    #[test]
    fn omits_unrepresentable_tools_for_chat_adapter() {
        let request = json!({
            "input": "Hello",
            "tools": [
                {"type": "custom", "name": "apply_patch", "format": {"type": "grammar"}},
                {"type": "namespace", "name": "plugins", "tools": []},
                {"type": "web_search"},
                {
                    "type": "function",
                    "name": "lookup",
                    "description": "Look something up",
                    "parameters": {"type": "object", "properties": {}}
                }
            ]
        });
        let chat = responses_to_chat(&request, "model-a").expect("request should translate");
        assert_eq!(chat["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(chat["tools"][0]["function"]["name"], "lookup");
    }

    #[test]
    fn keeps_reasoning_on_the_assistant_tool_call_message() {
        let mut request = json!({
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "List files"}]
                },
                {
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "I should inspect the directory."}]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": ""}]
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "exec_command",
                    "arguments": "{\"cmd\":\"ls\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "file.txt"
                }
            ]
        });

        normalize_chat_reasoning_history(&mut request, true);
        let chat =
            responses_to_chat(&request, "deepseek-v4-flash").expect("request should translate");
        assert_eq!(chat["messages"].as_array().map(Vec::len), Some(3));
        assert_eq!(chat["messages"][1]["role"], "assistant");
        assert_eq!(
            chat["messages"][1]["reasoning_content"],
            "I should inspect the directory."
        );
        assert_eq!(
            chat["messages"][1]["tool_calls"][0]["function"]["name"],
            "exec_command"
        );
        assert_eq!(chat["messages"][2]["role"], "tool");
    }

    #[test]
    fn converts_chat_response_to_responses_object() {
        let chat = json!({
            "choices": [{"message": {"role": "assistant", "content": "Hello"}}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6}
        });
        let response = chat_to_response(&chat, "route/model-a", &BTreeSet::new())
            .expect("response should translate");
        assert_eq!(response["object"], "response");
        assert_eq!(response["model"], "route/model-a");
        assert_eq!(response["output"][0]["content"][0]["text"], "Hello");
        assert_eq!(response["usage"]["total_tokens"], 6);
    }

    #[test]
    fn converts_custom_tool_call_history_to_chat_and_back() {
        // Mirrors what Codex App replays on restart: a local tool invocation
        // stored as custom_tool_call / custom_tool_call_output input items.
        let request = json!({
            "model": "deepseek-v4-flash",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "list the files"}]},
                {"type": "custom_tool_call", "call_id": "call_1", "name": "exec", "input": "const r = await tools.list_dir(); text(r);"},
                {"type": "custom_tool_call_output", "call_id": "call_1", "output": [{"type": "output_text", "text": "src/"}], "status": "completed"}
            ]
        });
        let chat = responses_to_chat(&request, "deepseek-v4-flash")
            .expect("request with custom_tool_call items should translate");
        assert_eq!(chat["messages"].as_array().map(Vec::len), Some(3));
        assert_eq!(chat["messages"][1]["role"], "assistant");
        assert_eq!(
            chat["messages"][1]["tool_calls"][0]["function"]["arguments"],
            "const r = await tools.list_dir(); text(r);"
        );
        assert_eq!(chat["messages"][2]["role"], "tool");
        assert_eq!(chat["messages"][2]["tool_call_id"], "call_1");
        assert_eq!(chat["messages"][2]["content"], "src/");
    }

    #[test]
    fn emits_custom_tool_call_for_declared_custom_tools() {
        // When the request declares a tool as type "custom" (Codex App local
        // tools like exec / apply_patch), the response-side tool call must be
        // emitted as custom_tool_call so Codex routes it to its local handler.
        let chat = json!({
            "choices": [{"message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "exec", "arguments": "{\"command\":\"ls\"}"}
                }]
            }}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6}
        });
        let custom_tools = ["exec", "apply_patch"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let response = chat_to_response(&chat, "route/model-a", &custom_tools)
            .expect("response should translate");
        assert_eq!(response["output"][0]["type"], "custom_tool_call");
        assert_eq!(response["output"][0]["name"], "exec");
        assert_eq!(response["output"][0]["input"], "{\"command\":\"ls\"}");
        assert!(response["output"][0].get("arguments").is_none());
    }

    #[test]
    fn emits_function_call_for_undeclared_tools() {
        // Existing function tools must not regress: without a custom tool
        // declaration the same Chat response stays a function_call item.
        let chat = json!({
            "choices": [{"message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"q\":\"x\"}"}
                }]
            }}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6}
        });
        let response = chat_to_response(&chat, "route/model-a", &BTreeSet::new())
            .expect("response should translate");
        assert_eq!(response["output"][0]["type"], "function_call");
        assert_eq!(response["output"][0]["arguments"], "{\"q\":\"x\"}");
    }

    #[test]
    fn streaming_events_have_monotonic_sequence_numbers() {
        let (mut state, initial) = ChatStreamState::new("route/model-a".into(), BTreeSet::new());
        let mut events = initial;
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"content": "Hi"}}]
        })));
        events.extend(state.finish());

        let sequence_numbers = events
            .iter()
            .map(|event| {
                let data = event
                    .lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .expect("event should contain data");
                serde_json::from_str::<Value>(data).expect("event data should be JSON")
                    ["sequence_number"]
                    .as_u64()
                    .expect("event should have a sequence number")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            sequence_numbers,
            (1..=sequence_numbers.len() as u64).collect::<Vec<_>>()
        );
    }

    #[test]
    fn preserves_non_string_custom_tool_input_payload() {
        // A malformed client may send a structured `input` instead of the
        // spec'd string. The payload must survive the Chat conversion instead
        // of silently collapsing to "{}".
        let request = json!({
            "model": "deepseek-v4-flash",
            "input": [
                {"type": "custom_tool_call", "call_id": "call_1", "name": "exec", "input": {"command": "ls", "cwd": "/tmp"}}
            ]
        });
        let chat =
            responses_to_chat(&request, "deepseek-v4-flash").expect("request should translate");
        assert_eq!(
            chat["messages"][0]["tool_calls"][0]["function"]["arguments"],
            "{\"command\":\"ls\",\"cwd\":\"/tmp\"}"
        );
    }

    #[test]
    fn streaming_classifies_late_name_as_custom_tool() {
        // Some Chat providers stream `{"index":0,"id":"call_x"}` first and the
        // tool name in a later chunk. The custom-tool classification must be
        // re-evaluated when the name arrives, not frozen at the first chunk.
        let custom_tools = ["exec"].into_iter().map(str::to_string).collect();
        let (mut state, initial) = ChatStreamState::new("route/model-a".into(), custom_tools);
        let mut events = initial;
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_x"}]}}]
        })));
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"name": "exec", "arguments": "{\"cmd\":\"ls\"}"}}]}}]
        })));
        events.extend(state.finish());

        let event_payloads = events
            .iter()
            .map(|event| {
                serde_json::from_str::<Value>(
                    event
                        .lines()
                        .find_map(|line| line.strip_prefix("data: "))
                        .expect("event should contain data"),
                )
                .expect("event data should be JSON")
            })
            .collect::<Vec<_>>();
        let added = event_payloads
            .iter()
            .find(|payload| payload["type"] == "response.output_item.added")
            .expect("an output_item.added event should exist");
        assert_eq!(added["item"]["type"], "custom_tool_call");
        assert_eq!(added["item"]["name"], "exec");
        assert!(event_payloads
            .iter()
            .any(|payload| { payload["type"] == "response.custom_tool_call_input.delta" }));
        let completed_item = event_payloads
            .iter()
            .find(|payload| {
                payload["type"] == "response.output_item.done"
                    && payload["item"]["type"] == "custom_tool_call"
            })
            .expect("custom_tool_call output_item.done should exist");
        assert_eq!(completed_item["item"]["input"], "{\"cmd\":\"ls\"}");
    }

    #[test]
    fn streaming_late_name_undeclared_stays_function_call() {
        // A late-arriving name that is NOT a declared custom tool must keep
        // function_call semantics.
        let (mut state, initial) = ChatStreamState::new("route/model-a".into(), BTreeSet::new());
        let mut events = initial;
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_y"}]}}]
        })));
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"name": "lookup", "arguments": "{\"q\":\"x\"}"}}]}}]
        })));
        events.extend(state.finish());

        let event_payloads = events
            .iter()
            .map(|event| {
                serde_json::from_str::<Value>(
                    event
                        .lines()
                        .find_map(|line| line.strip_prefix("data: "))
                        .expect("event should contain data"),
                )
                .expect("event data should be JSON")
            })
            .collect::<Vec<_>>();
        let added = event_payloads
            .iter()
            .find(|payload| payload["type"] == "response.output_item.added")
            .expect("an output_item.added event should exist");
        assert_eq!(added["item"]["type"], "function_call");
        assert_eq!(added["item"]["name"], "lookup");
        assert!(event_payloads
            .iter()
            .any(|payload| { payload["type"] == "response.function_call_arguments.delta" }));
    }
}
