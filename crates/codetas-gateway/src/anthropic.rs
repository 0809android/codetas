use crate::compaction::decode_summary;
use crate::translate::{
    default_tool_search_parameters, insert_tool_namespace, response_allowed_tool_wire_names,
    response_tool_map, response_tool_parameters, tool_input_to_value,
    tool_search_output_to_text, unwrap_custom_tool_arguments, ResponseToolKind, ResponseToolMap,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use uuid::Uuid;

const CLAUDE_CODE_SYSTEM_INSTRUCTION: &str =
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
const CLAUDE_OAUTH_TOOL_PREFIX: &str = "custom_";
const ANTHROPIC_OAUTH_BETA: &str = "claude-code-20250219,oauth-2025-04-20";

pub fn uses_anthropic_subscription_oauth(provider: &crate::config::ProviderDefinition) -> bool {
    if provider.protocol != crate::config::ProviderProtocol::AnthropicMessages {
        return false;
    }
    if !matches!(provider.id.as_str(), "anthropic" | "anthropic-apikey") {
        return false;
    }
    let reference = provider
        .credential
        .reference
        .as_deref()
        .or(provider.api_key_env.as_deref())
        .unwrap_or("");
    provider.credential.source == crate::config::CredentialSource::OAuth
        || provider.credential.transport == crate::config::CredentialTransport::Bearer
        || reference == "CLAUDE_CODE_OAUTH_TOKEN"
}

pub fn anthropic_subscription_oauth_headers() -> [(&'static str, &'static str); 2] {
    [
        ("anthropic-version", "2023-06-01"),
        ("anthropic-beta", ANTHROPIC_OAUTH_BETA),
    ]
}

pub fn responses_to_anthropic(body: &Value, model: &str) -> Result<Value, String> {
    responses_to_anthropic_with_oauth(body, model, false)
}

pub fn responses_to_anthropic_with_oauth(
    body: &Value,
    model: &str,
    subscription_oauth: bool,
) -> Result<Value, String> {
    let object = body
        .as_object()
        .ok_or_else(|| "Responses request must be a JSON object".to_string())?;
    let tool_map = response_tool_map(body);
    let mut messages = Vec::new();
    match object.get("input") {
        Some(Value::String(text)) => messages.push(json!({"role": "user", "content": text})),
        Some(Value::Array(items)) => {
            for item in items {
                let item = item
                    .as_object()
                    .ok_or("Responses input items must be objects")?;
                match item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("message")
                {
                    "message" => {
                        let role = match item.get("role").and_then(Value::as_str).unwrap_or("user")
                        {
                            "assistant" => "assistant",
                            _ => "user",
                        };
                        let content = response_content_to_anthropic(item.get("content"))?;
                        messages.push(json!({"role": role, "content": content}));
                    }
                    "function_call" | "custom_tool_call" => {
                        let item_type = item.get("type").and_then(Value::as_str);
                        let kind = if item_type == Some("custom_tool_call") {
                            ResponseToolKind::Custom
                        } else {
                            ResponseToolKind::Function
                        };
                        let call_id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or("call_unknown");
                        let name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        // custom_tool_call items carry the payload in `input`
                        // while function_call uses `arguments`. A structured
                        // payload is preserved as-is instead of collapsing to {}.
                        let payload = item
                            .get("input")
                            .or_else(|| item.get("arguments"))
                            .unwrap_or(&Value::Null);
                        let input = if item_type == Some("custom_tool_call") {
                            custom_tool_input_to_anthropic(payload)
                        } else {
                            tool_input_to_value(payload)
                        };
                        let namespace = item.get("namespace").and_then(Value::as_str);
                        let wire_name = tool_map
                            .wire_name_for_identity(name, namespace, kind)
                            .map(str::to_string)
                            .unwrap_or_else(|| ResponseToolMap::wire_name(namespace, name));
                        let name = if subscription_oauth {
                            prefix_anthropic_oauth_tool_name(&wire_name)
                        } else {
                            wire_name
                        };
                        messages.push(json!({
                            "role": "assistant",
                            "content": [{"type": "tool_use", "id": call_id, "name": name, "input": input}]
                        }));
                    }
                    "function_call_output" | "custom_tool_call_output" => {
                        let call_id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .ok_or("tool output requires call_id")?;
                        let output = item.get("output").unwrap_or(&Value::Null);
                        let content = match output {
                            Value::Null | Value::String(_) | Value::Array(_) => {
                                match response_content_to_anthropic(Some(output))? {
                                    Value::Array(parts) if parts.is_empty() => {
                                        Value::String(output_to_text(output))
                                    }
                                    content => content,
                                }
                            }
                            _ => Value::String(output_to_text(output)),
                        };
                        messages.push(json!({
                            "role": "user",
                            "content": [{"type": "tool_result", "tool_use_id": call_id, "content": content}]
                        }));
                    }
                    "tool_search_call" => {
                        let call_id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or("call_unknown");
                        let input = item
                            .get("arguments")
                            .map(tool_input_to_value)
                            .unwrap_or_else(|| json!({}));
                        let wire_name = tool_map
                            .wire_name_for_identity(
                                "tool_search",
                                None,
                                ResponseToolKind::ToolSearch,
                            )
                            .unwrap_or("tool_search");
                        let name = if subscription_oauth {
                            prefix_anthropic_oauth_tool_name(wire_name)
                        } else {
                            wire_name.to_string()
                        };
                        messages.push(json!({
                            "role": "assistant",
                            "content": [{"type": "tool_use", "id": call_id, "name": name, "input": input}]
                        }));
                    }
                    "tool_search_output" => {
                        let call_id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .ok_or("tool search output requires call_id")?;
                        let content =
                            tool_search_output_to_text(&Value::Object(item.clone()), &tool_map);
                        messages.push(json!({
                            "role": "user",
                            "content": [{"type": "tool_result", "tool_use_id": call_id, "content": content}]
                        }));
                    }
                    "reasoning" => {
                        if let Some(blocks) = item
                            .get("provider_metadata")
                            .and_then(|metadata| metadata.pointer("/anthropic/thinking_blocks"))
                            .and_then(Value::as_array)
                        {
                            messages.push(json!({"role": "assistant", "content": blocks}));
                        }
                    }
                    "compaction" => {
                        if let Ok(Some(summary)) = decode_summary(&Value::Object(item.clone())) {
                            messages.push(json!({
                                "role": "user",
                                "content": summary
                            }));
                        }
                    }
                    _ => {}
                }
            }
        }
        Some(Value::Null) | None => {}
        Some(_) => return Err("Responses input must be a string or array".into()),
    }

    let mut request = Map::new();
    request.insert("model".into(), json!(model));
    request.insert("messages".into(), json!(merge_adjacent_messages(messages)));
    let max_tokens = object
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(8_192);
    request.insert("max_tokens".into(), json!(max_tokens));
    request.insert(
        "stream".into(),
        json!(object
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false)),
    );
    if subscription_oauth {
        let mut system = vec![json!({
            "type": "text",
            "text": CLAUDE_CODE_SYSTEM_INSTRUCTION
        })];
        if let Some(instructions) = object.get("instructions").and_then(Value::as_str) {
            if !instructions.is_empty() {
                system.push(json!({"type": "text", "text": instructions}));
            }
        }
        request.insert("system".into(), Value::Array(system));
    } else if let Some(instructions) = object.get("instructions") {
        if !instructions.is_null() {
            request.insert("system".into(), instructions.clone());
        }
    }
    if let Some(value) = object.get("temperature") {
        request.insert("temperature".into(), value.clone());
    }
    if let Some(value) = object.get("top_p") {
        request.insert("top_p".into(), value.clone());
    }
    if let Some(format) = object
        .get("text")
        .and_then(Value::as_object)
        .and_then(|text| text.get("format"))
        .and_then(Value::as_object)
    {
        if format.get("type").and_then(Value::as_str) == Some("json_schema") {
            if let Some(schema) = format.get("schema") {
                request.insert("output_config".into(), json!({
                    "format": {"type": "json_schema", "schema": schema}
                }));
            }
        }
    }
    let allowed_tool_names = object
        .get("tool_choice")
        .and_then(|choice| response_allowed_tool_wire_names(choice, &tool_map));
    let tools = tool_map
        .iter()
        .filter(|(wire_name, _, _)| {
            allowed_tool_names
                .as_ref()
                .map(|allowed| allowed.contains(*wire_name))
                .unwrap_or(true)
        })
        .filter_map(|(wire_name, identity, declaration)| match response_tool_to_anthropic(
            wire_name,
            identity.kind,
            declaration,
        ) {
            Ok(Some(tool)) => Some(Ok(tool)),
            Ok(None) => None,
            Err(message) => Some(Err(message)),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !tools.is_empty() {
        let tools = if subscription_oauth {
            tools
                .into_iter()
                .map(prefix_anthropic_oauth_tool)
                .collect::<Vec<_>>()
        } else {
            tools
        };
        request.insert("tools".into(), json!(tools));
    }
    let reasoning_effort = object
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str);
    let thinking_enabled =
        reasoning_effort.is_some_and(|value| !matches!(value, "none" | "minimal"));
    if let Some(choice) = object.get("tool_choice") {
        if !choice.is_null() {
            let forced = choice.as_str() == Some("required")
                || matches!(
                    choice.get("type").and_then(Value::as_str),
                    Some("function" | "custom" | "tool_search")
                )
                || (choice.get("type").and_then(Value::as_str) == Some("allowed_tools")
                    && choice.get("mode").and_then(Value::as_str) == Some("required")
                    && allowed_tool_names.as_ref().is_some_and(|names| !names.is_empty()));
            if thinking_enabled && forced {
                return Err(
                    "Anthropic thinking cannot be combined with a forced tool choice".into(),
                );
            }
            let mut tool_choice = response_tool_choice_to_anthropic(choice, &tool_map)?;
            if subscription_oauth {
                if let Some(name) = tool_choice.get("name").and_then(Value::as_str) {
                    let prefixed = prefix_anthropic_oauth_tool_name(name);
                    tool_choice["name"] = Value::String(prefixed);
                }
            }
            request.insert("tool_choice".into(), tool_choice);
        }
    }
    if let Some(reasoning) = reasoning_effort {
        if thinking_enabled && max_tokens > 1_024 {
            let budget = thinking_budget(reasoning).min(max_tokens.saturating_sub(1));
            request.insert(
                "thinking".into(),
                json!({"type": "enabled", "budget_tokens": budget}),
            );
        }
    }
    Ok(Value::Object(request))
}

pub fn anthropic_to_response(
    value: &Value,
    exposed_model: &str,
    tool_map: &ResponseToolMap,
) -> Result<Value, String> {
    anthropic_to_response_with_oauth(value, exposed_model, tool_map, false)
}

pub fn anthropic_to_response_with_oauth(
    value: &Value,
    exposed_model: &str,
    tool_map: &ResponseToolMap,
    subscription_oauth: bool,
) -> Result<Value, String> {
    let blocks = value
        .get("content")
        .and_then(Value::as_array)
        .ok_or("Anthropic response has no content")?;
    let mut text = String::new();
    let mut output = Vec::new();
    let mut reasoning_summary = Vec::new();
    let mut thinking_blocks = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            Some("thinking") => {
                thinking_blocks.push(block.clone());
                if let Some(thinking) = block.get("thinking").and_then(Value::as_str) {
                    reasoning_summary.push(json!({"type": "summary_text", "text": thinking}));
                }
            }
            Some("redacted_thinking") => thinking_blocks.push(block.clone()),
            Some("tool_use") => {
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                let upstream_name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                // Only the subscription-OAuth transport wraps tool names with
                // `custom_`. Ordinary Anthropic API names must remain exact,
                // including tools whose real name already starts that way.
                let wire_name = if subscription_oauth {
                    strip_anthropic_oauth_tool_name(upstream_name)
                } else {
                    upstream_name
                };
                let identity = tool_map.identity(wire_name);
                let name = identity
                    .map(|identity| identity.name.as_str())
                    .unwrap_or(wire_name);
                let namespace = identity.and_then(|identity| identity.namespace.as_deref());
                let kind = identity
                    .map(|identity| identity.kind)
                    .unwrap_or(ResponseToolKind::Function);
                if kind == ResponseToolKind::Custom {
                    let arguments = serde_json::to_string(&input).unwrap_or_else(|_| "{}".into());
                    let mut item = json!({
                        "id": format!("ctc_{}", Uuid::new_v4().simple()),
                        "type": "custom_tool_call",
                        "status": "completed",
                        "call_id": block.get("id").and_then(Value::as_str).unwrap_or("call_unknown"),
                        "name": name,
                        "input": unwrap_custom_tool_arguments(&arguments)
                    });
                    insert_tool_namespace(&mut item, namespace);
                    output.push(item);
                } else if kind == ResponseToolKind::ToolSearch {
                    output.push(json!({
                        "id": format!("tsc_{}", Uuid::new_v4().simple()),
                        "type": "tool_search_call",
                        "status": "completed",
                        "call_id": block.get("id").and_then(Value::as_str).unwrap_or("call_unknown"),
                        "execution": "client",
                        "arguments": input
                    }));
                } else {
                    let mut item = json!({
                        "id": format!("fc_{}", Uuid::new_v4().simple()),
                        "type": "function_call",
                        "status": "completed",
                        "call_id": block.get("id").and_then(Value::as_str).unwrap_or("call_unknown"),
                        "name": name,
                        "arguments": serde_json::to_string(&input).unwrap_or_else(|_| "{}".into())
                    });
                    insert_tool_namespace(&mut item, namespace);
                    output.push(item);
                }
            }
            _ => {}
        }
    }
    let has_thinking = !thinking_blocks.is_empty();
    if has_thinking {
        output.insert(
            0,
            json!({
                "id": format!("rs_{}", Uuid::new_v4().simple()),
                "type": "reasoning",
                "summary": reasoning_summary,
                "provider_metadata": {"anthropic": {"thinking_blocks": thinking_blocks}}
            }),
        );
    }
    if !text.is_empty() {
        let message = json!({
            "id": format!("msg_{}", Uuid::new_v4().simple()),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}]
        });
        let insert_at = usize::from(has_thinking);
        output.insert(insert_at, message);
    }
    let stop_reason = value.get("stop_reason").and_then(Value::as_str);
    let incomplete = stop_reason == Some("max_tokens");
    Ok(response_object(
        exposed_model,
        if incomplete {
            "incomplete"
        } else {
            "completed"
        },
        output,
        anthropic_usage(value.get("usage")),
        incomplete.then(|| json!({"reason": "max_output_tokens"})),
    ))
}

#[derive(Default)]
pub struct AnthropicStreamState {
    pub input_tokens: u64,
    block_types: BTreeMap<u64, String>,
    thinking_blocks: BTreeMap<u64, Value>,
}

pub fn anthropic_stream_to_chat(
    value: &Value,
    state: &mut AnthropicStreamState,
    subscription_oauth: bool,
) -> Result<Option<Value>, String> {
    match value.get("type").and_then(Value::as_str) {
        Some("message_start") => {
            state.input_tokens = value
                .pointer("/message/usage/input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            state.block_types.clear();
            state.thinking_blocks.clear();
            Ok(None)
        }
        Some("content_block_start") => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
            let block = value
                .get("content_block")
                .ok_or("Anthropic content block is missing")?;
            let block_type = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            state.block_types.insert(index, block_type.to_string());
            if matches!(block_type, "thinking" | "redacted_thinking") {
                state.thinking_blocks.insert(index, block.clone());
            }
            if block_type == "tool_use" {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let name = if subscription_oauth {
                    strip_anthropic_oauth_tool_name(name)
                } else {
                    name
                };
                Ok(Some(json!({"choices": [{"delta": {"tool_calls": [{
                    "index": index,
                    "id": block.get("id").and_then(Value::as_str).unwrap_or("call_unknown"),
                    "type": "function",
                    "function": {"name": name, "arguments": ""}
                }]}}]})))
            } else {
                Ok(None)
            }
        }
        Some("content_block_delta") => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
            let delta = value.get("delta").ok_or("Anthropic delta is missing")?;
            let block_type = state.block_types.get(&index).map(String::as_str);
            match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") if block_type == Some("text") => Ok(Some(json!({"choices": [{"delta": {
                    "content": delta.get("text").and_then(Value::as_str).unwrap_or_default()
                }}]}))),
                Some("input_json_delta") if block_type == Some("tool_use") => {
                    Ok(Some(json!({"choices": [{"delta": {"tool_calls": [{
                        "index": index,
                        "function": {"arguments": delta.get("partial_json").and_then(Value::as_str).unwrap_or_default()}
                    }]}}]})))
                }
                Some("thinking_delta") if block_type == Some("thinking") => {
                    let thinking = delta
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if let Some(block) = state.thinking_blocks.get_mut(&index) {
                        append_json_string(&mut block["thinking"], thinking);
                    }
                    Ok(Some(json!({"choices": [{"delta": {
                        "reasoning_content": thinking
                    }}]})))
                }
                Some("signature_delta") if block_type == Some("thinking") => {
                    let signature = delta
                        .get("signature")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if let Some(block) = state.thinking_blocks.get_mut(&index) {
                        append_json_string(&mut block["signature"], signature);
                    }
                    Ok(Some(json!({"choices": [{"delta": {
                        "reasoning_signature": signature
                    }}]})))
                }
                Some(_) => Ok(None),
                None => Ok(None),
            }
        }
        Some("message_delta") => {
            let output = value
                .pointer("/usage/output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            Ok(Some(json!({
                "choices": [],
                "usage": {"prompt_tokens": state.input_tokens, "completion_tokens": output, "total_tokens": state.input_tokens + output}
            })))
        }
        Some("content_block_stop") => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
            state.block_types.remove(&index);
            Ok(state.thinking_blocks.remove(&index).map(|block| {
                json!({"choices": [{"delta": {
                    "codetas_anthropic_thinking_block": block
                }}]})
            }))
        }
        Some("message_stop") => {
            state.block_types.clear();
            state.thinking_blocks.clear();
            Ok(None)
        }
        Some("ping") => Ok(None),
        Some("error") => Err(value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("Anthropic stream error")
            .into()),
        Some(_) => Ok(None),
        None => Ok(None),
    }
}

fn append_json_string(target: &mut Value, suffix: &str) {
    if suffix.is_empty() {
        return;
    }
    if let Some(current) = target.as_str() {
        *target = Value::String(format!("{current}{suffix}"));
    } else {
        *target = Value::String(suffix.to_string());
    }
}

fn response_content_to_anthropic(content: Option<&Value>) -> Result<Value, String> {
    match content {
        None | Some(Value::Null) => Ok(json!([])),
        Some(Value::String(text)) => Ok(json!(text)),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                let part = part.as_object()?;
                match part.get("type").and_then(Value::as_str) {
                    Some("input_text" | "output_text" | "text") => Some(Ok(json!({
                        "type": "text",
                        "text": part.get("text").and_then(Value::as_str).unwrap_or_default()
                    }))),
                    Some("input_image" | "image_url") => {
                        let url = part
                            .get("image_url")
                            .and_then(|value| {
                                value
                                    .as_str()
                                    .or_else(|| value.get("url").and_then(Value::as_str))
                            })
                            .or_else(|| part.get("url").and_then(Value::as_str))?;
                        Some(Ok(if let Some((media_type, data)) = parse_data_url(url) {
                            json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}})
                        } else {
                            json!({"type": "image", "source": {"type": "url", "url": url}})
                        }))
                    }
                    _ => None,
                }
            })
            .collect::<Result<Vec<_>, String>>()
            .map(Value::Array),
        Some(_) => Err("message content must be string or array".into()),
    }
}

fn strip_anthropic_oauth_tool_name(name: &str) -> &str {
    name.strip_prefix(CLAUDE_OAUTH_TOOL_PREFIX).unwrap_or(name)
}

fn prefix_anthropic_oauth_tool_name(name: &str) -> String {
    if matches!(
        name,
        "web_search" | "code_execution" | "text_editor" | "computer"
    ) {
        name.to_string()
    } else {
        format!("{CLAUDE_OAUTH_TOOL_PREFIX}{name}")
    }
}

fn prefix_anthropic_oauth_tool(mut tool: Value) -> Value {
    if let Some(name) = tool.get("name").and_then(Value::as_str) {
        let prefixed = prefix_anthropic_oauth_tool_name(name);
        tool["name"] = Value::String(prefixed);
    }
    tool
}

fn response_tool_to_anthropic(
    wire_name: &str,
    kind: ResponseToolKind,
    tool: &Value,
) -> Result<Option<Value>, String> {
    let input_schema = match kind {
        ResponseToolKind::Custom => json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "The exact freeform input to pass to this tool."
                }
            },
            "required": ["input"],
            "additionalProperties": false
        }),
        ResponseToolKind::ToolSearch => response_tool_parameters(tool)
            .unwrap_or_else(default_tool_search_parameters),
        ResponseToolKind::Function => response_tool_parameters(tool)
            .unwrap_or_else(|| json!({"type": "object"})),
    };
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_else(|| match kind {
            ResponseToolKind::ToolSearch => "Search for additional tools relevant to the task.",
            _ => "",
        });
    Ok(Some(json!({
        "name": wire_name,
        "description": description,
        "input_schema": input_schema
    })))
}

fn response_tool_choice_to_anthropic(
    choice: &Value,
    tool_map: &ResponseToolMap,
) -> Result<Value, String> {
    match choice.as_str() {
        Some("auto") => Ok(json!({"type": "auto"})),
        Some("required") => Ok(json!({"type": "any"})),
        Some("none") => Ok(json!({"type": "none"})),
        Some(_) => Err("unsupported Anthropic tool choice".into()),
        None
            if matches!(
                choice.get("type").and_then(Value::as_str),
                Some("function" | "custom")
            ) =>
        {
            let name = choice
                .get("name")
                .and_then(Value::as_str)
                .ok_or("function tool choice requires name")?;
            let namespace = choice.get("namespace").and_then(Value::as_str);
            let kind = if choice.get("type").and_then(Value::as_str) == Some("custom") {
                ResponseToolKind::Custom
            } else {
                ResponseToolKind::Function
            };
            let wire_name = tool_map
                .wire_name_for_identity(name, namespace, kind)
                .map(str::to_string)
                .unwrap_or_else(|| ResponseToolMap::wire_name(namespace, name));
            Ok(json!({"type": "tool", "name": wire_name}))
        }
        None if choice.get("type").and_then(Value::as_str) == Some("tool_search") => {
            let wire_name = tool_map
                .wire_name_for_identity("tool_search", None, ResponseToolKind::ToolSearch)
                .unwrap_or("tool_search");
            Ok(json!({"type": "tool", "name": wire_name}))
        }
        None if choice.get("type").and_then(Value::as_str) == Some("allowed_tools") => {
            let allowed = response_allowed_tool_wire_names(choice, tool_map)
                .unwrap_or_default();
            if allowed.is_empty() {
                Ok(json!({"type": "none"}))
            } else if choice.get("mode").and_then(Value::as_str) == Some("required") {
                Ok(json!({"type": "any"}))
            } else {
                Ok(json!({"type": "auto"}))
            }
        }
        None => Err("unsupported Anthropic tool choice".into()),
    }
}

fn merge_adjacent_messages(messages: Vec<Value>) -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::new();
    for message in messages {
        let same_role = merged.last().and_then(|last| last.get("role")) == message.get("role");
        if same_role {
            let incoming = normalize_content(message.get("content"));
            if let Some(parts) = merged
                .last_mut()
                .and_then(|last| last.get_mut("content"))
                .and_then(Value::as_array_mut)
            {
                parts.extend(incoming);
            }
        } else {
            let mut message = message;
            let content = Value::Array(normalize_content(message.get("content")));
            message["content"] = content;
            merged.push(message);
        }
    }
    merged
}

fn normalize_content(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::Array(parts)) => parts.clone(),
        Some(Value::String(text)) => vec![json!({"type": "text", "text": text})],
        _ => Vec::new(),
    }
}

fn parse_data_url(value: &str) -> Option<(&str, &str)> {
    let value = value.strip_prefix("data:")?;
    let (metadata, data) = value.split_once(',')?;
    let media_type = metadata.strip_suffix(";base64")?;
    Some((media_type, data))
}

fn thinking_budget(effort: &str) -> u64 {
    match effort {
        "low" => 1_024,
        "medium" => 4_096,
        "high" => 8_192,
        "xhigh" => 16_384,
        _ => 32_768,
    }
}

fn anthropic_usage(usage: Option<&Value>) -> Value {
    let input = usage
        .and_then(|value| value.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .and_then(|value| value.get("cache_read_input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .and_then(|value| value.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "input_tokens": input,
        "input_tokens_details": {"cached_tokens": cached},
        "output_tokens": output,
        "output_tokens_details": {"reasoning_tokens": 0},
        "total_tokens": input + output
    })
}

fn response_object(
    model: &str,
    status: &str,
    output: Vec<Value>,
    usage: Value,
    incomplete: Option<Value>,
) -> Value {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0);
    json!({
        "id": format!("resp_{}", Uuid::new_v4().simple()),
        "object": "response",
        "created_at": now,
        "completed_at": if status == "completed" { json!(now) } else { Value::Null },
        "status": status,
        "model": model,
        "output": output,
        "parallel_tool_calls": true,
        "usage": usage,
        "error": Value::Null,
        "incomplete_details": incomplete
    })
}

/// Extracts tool-result text from a Responses `output` value.
///
/// `custom_tool_call_output` items carry either a plain string or an array of
/// output content parts (`input_text` / `output_text`), which Anthropic's
/// `tool_result` renders as plain text. Joining the parts preserves the tool
/// result Codex needs to continue the conversation.
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

fn custom_tool_input_to_anthropic(value: &Value) -> Value {
    match value {
        Value::String(input) => json!({"input": input}),
        Value::Null => json!({"input": ""}),
        other => json!({"input": other}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_oauth_wraps_system_and_prefixes_tools() {
        let request = json!({
            "instructions": "Be brief.",
            "input": [{"type": "message", "role": "user", "content": "hi"}],
            "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}}]
        });
        let body = responses_to_anthropic_with_oauth(&request, "claude-sonnet-5", true)
            .expect("oauth request");
        assert_eq!(body["system"][0]["text"], CLAUDE_CODE_SYSTEM_INSTRUCTION);
        assert_eq!(body["tools"][0]["name"], "custom_lookup");
        let provider = crate::config::ProviderDefinition {
            id: "anthropic".into(),
            protocol: crate::config::ProviderProtocol::AnthropicMessages,
            credential: crate::config::ProviderCredential {
                source: crate::config::CredentialSource::Environment,
                reference: Some("CLAUDE_CODE_OAUTH_TOKEN".into()),
                transport: crate::config::CredentialTransport::Bearer,
                ..crate::config::ProviderCredential::default()
            },
            ..crate::config::ProviderDefinition::default()
        };
        assert!(uses_anthropic_subscription_oauth(&provider));
    }

    #[test]
    fn translates_function_tools_and_results() {
        let request = json!({
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]},
                {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"q\":\"a\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "done"}
            ],
            "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}}]
        });
        let translated =
            responses_to_anthropic(&request, "claude-test").expect("request should translate");
        assert_eq!(translated["model"], "claude-test");
        assert_eq!(translated["tools"][0]["name"], "lookup");
    }

    #[test]
    fn carries_tool_result_images_as_anthropic_image_blocks() {
        let request = json!({
            "input": [
                {"type": "custom_tool_call", "call_id": "call_1", "name": "view_image", "input": "{}"},
                {
                    "type": "custom_tool_call_output",
                    "call_id": "call_1",
                    "output": [
                        {"type": "input_text", "text": "rendered"},
                        {"type": "input_image", "image_url": "data:image/png;base64,AA=="}
                    ]
                }
            ]
        });
        let translated = responses_to_anthropic(&request, "claude-test")
            .expect("request should translate");
        let content = &translated["messages"][1]["content"][0]["content"];
        assert_eq!(content[0]["text"], "rendered");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["data"], "AA==");
    }

    #[test]
    fn stringifies_structured_tool_result_output() {
        let request = json!({
            "input": [
                {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": {"count": 2, "ok": true}}
            ]
        });
        let translated = responses_to_anthropic(&request, "claude-test")
            .expect("request should translate");
        assert_eq!(
            translated["messages"][1]["content"][0]["content"],
            "{\"count\":2,\"ok\":true}"
        );
    }

    #[test]
    fn preserves_structured_custom_tool_input() {
        // A structured `input` (malformed client) must survive instead of
        // collapsing to {} — the tool_use block keeps the payload.
        let request = json!({
            "input": [
                {"type": "custom_tool_call", "call_id": "call_1", "name": "exec", "input": {"command": "ls"}}
            ]
        });
        let translated =
            responses_to_anthropic(&request, "claude-test").expect("request should translate");
        assert_eq!(
            translated["messages"][0]["content"][0]["input"],
            json!({"input": {"command": "ls"}})
        );
    }

    #[test]
    fn flattens_namespace_tools_and_round_trips_the_identity() {
        let request = json!({
            "input": [{
                "type": "function_call",
                "call_id": "call_1",
                "namespace": "collaboration",
                "name": "send_message",
                "arguments": "{\"target\":\"/root\",\"message\":\"done\"}"
            }],
            "tools": [{
                "type": "namespace",
                "name": "collaboration",
                "description": "Agent coordination tools",
                "tools": [{
                    "type": "function",
                    "name": "send_message",
                    "description": "Send a message",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"target": {"type": "string"}},
                        "required": ["target"]
                    }
                }]
            }]
        });

        let translated =
            responses_to_anthropic(&request, "claude-test").expect("namespace request");
        assert_eq!(translated["tools"][0]["name"], "collaboration__send_message");
        assert_eq!(
            translated["tools"][0]["input_schema"],
            request["tools"][0]["tools"][0]["inputSchema"]
        );
        assert_eq!(
            translated["messages"][0]["content"][0]["name"],
            "collaboration__send_message"
        );

        let upstream = json!({
            "content": [{
                "type": "tool_use",
                "id": "call_2",
                "name": "collaboration__send_message",
                "input": {"target": "/root", "message": "done"}
            }],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let tool_map = response_tool_map(&request);
        let response =
            anthropic_to_response(&upstream, "claude-test", &tool_map).expect("response");
        assert_eq!(response["output"][0]["type"], "function_call");
        assert_eq!(response["output"][0]["name"], "send_message");
        assert_eq!(response["output"][0]["namespace"], "collaboration");
    }

    #[test]
    fn exposes_custom_tools_and_unwraps_freeform_input() {
        let request = json!({
            "input": [{
                "type": "custom_tool_call",
                "call_id": "call_1",
                "name": "exec",
                "input": "const result = await tools.read(); text(result);"
            }],
            "tools": [{
                "type": "custom",
                "name": "exec",
                "description": "Run JavaScript against available tools",
                "format": {"type": "grammar", "syntax": "lark", "definition": "start: /.+/"}
            }]
        });

        let translated =
            responses_to_anthropic(&request, "claude-test").expect("custom tool request");
        assert_eq!(translated["tools"][0]["name"], "exec");
        assert_eq!(
            translated["tools"][0]["input_schema"]["properties"]["input"]["type"],
            "string"
        );
        assert_eq!(
            translated["messages"][0]["content"][0]["input"],
            json!({"input": "const result = await tools.read(); text(result);"})
        );

        let upstream = json!({
            "content": [{
                "type": "tool_use",
                "id": "call_2",
                "name": "exec",
                "input": {"input": "const result = await tools.read(); text(result);"}
            }],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let tool_map = response_tool_map(&request);
        let response =
            anthropic_to_response(&upstream, "claude-test", &tool_map).expect("response");
        assert_eq!(response["output"][0]["type"], "custom_tool_call");
        assert_eq!(response["output"][0]["name"], "exec");
        assert_eq!(
            response["output"][0]["input"],
            "const result = await tools.read(); text(result);"
        );
    }

    #[test]
    fn accepts_all_supported_function_schema_field_names() {
        let request = json!({
            "tools": [
                {
                    "type": "function",
                    "name": "parameters_tool",
                    "parameters": {"type": "object", "required": ["a"]}
                },
                {
                    "type": "function",
                    "name": "snake_tool",
                    "input_schema": {"type": "object", "required": ["b"]}
                },
                {
                    "type": "function",
                    "name": "camel_tool",
                    "inputSchema": {"type": "object", "required": ["c"]}
                }
            ]
        });

        let translated =
            responses_to_anthropic(&request, "claude-test").expect("schema aliases");
        let tools = translated["tools"].as_array().expect("tools");
        let schema_for = |name: &str| {
            tools
                .iter()
                .find(|tool| tool["name"] == name)
                .map(|tool| tool["input_schema"].clone())
                .expect("translated tool")
        };
        assert_eq!(schema_for("parameters_tool")["required"][0], "a");
        assert_eq!(schema_for("snake_tool")["required"][0], "b");
        assert_eq!(schema_for("camel_tool")["required"][0], "c");
    }

    #[test]
    fn assigns_reversible_names_to_flattened_namespace_collisions() {
        let request = json!({
            "input": [{
                "type": "function_call",
                "call_id": "call_1",
                "namespace": "plugins",
                "name": "lookup",
                "arguments": "{}"
            }],
            "tools": [
                {
                    "type": "function",
                    "name": "plugins__lookup",
                    "parameters": {"type": "object"}
                },
                {
                    "type": "namespace",
                    "name": "plugins",
                    "tools": [{
                        "type": "function",
                        "name": "lookup",
                        "inputSchema": {"type": "object"}
                    }]
                }
            ]
        });

        let translated =
            responses_to_anthropic(&request, "claude-test").expect("colliding tools retained");
        let tools = translated["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "plugins__lookup");
        let namespaced_wire_name = tools[1]["name"].as_str().expect("wire name");
        assert!(namespaced_wire_name.starts_with("plugins__lookup__"));
        assert_eq!(
            translated["messages"][0]["content"][0]["name"],
            namespaced_wire_name
        );
    }

    #[test]
    fn preserves_tool_search_kind_and_oauth_name_in_history_and_choice() {
        let request = json!({
            "input": [{
                "type": "tool_search_call",
                "call_id": "call_search",
                "arguments": {"query": "calendar"}
            }],
            "tools": [{"type": "tool_search"}],
            "tool_choice": {"type": "tool_search"}
        });

        let translated = responses_to_anthropic_with_oauth(&request, "claude-test", true)
            .expect("tool search request");
        assert_eq!(translated["tools"][0]["name"], "custom_tool_search");
        assert_eq!(
            translated["messages"][0]["content"][0]["name"],
            "custom_tool_search"
        );
        assert_eq!(translated["tool_choice"]["name"], "custom_tool_search");
    }

    #[test]
    fn allowed_tools_filters_anthropic_declarations() {
        let request = json!({
            "tools": [
                {"type": "function", "name": "lookup", "parameters": {"type": "object"}},
                {"type": "custom", "name": "exec"}
            ],
            "tool_choice": {
                "type": "allowed_tools",
                "mode": "required",
                "tools": [{"type": "custom", "name": "exec"}]
            }
        });

        let translated =
            responses_to_anthropic(&request, "claude-test").expect("allowed tools request");
        assert_eq!(translated["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(translated["tools"][0]["name"], "exec");
        assert_eq!(translated["tool_choice"]["type"], "any");
    }

    #[test]
    fn oauth_prefix_round_trip_preserves_names_already_starting_with_custom() {
        let request = json!({
            "tools": [{
                "type": "function",
                "name": "custom_lookup",
                "parameters": {"type": "object"}
            }]
        });
        let translated = responses_to_anthropic_with_oauth(&request, "claude-test", true)
            .expect("oauth tool request");
        assert_eq!(translated["tools"][0]["name"], "custom_custom_lookup");

        let upstream = json!({
            "content": [{
                "type": "tool_use",
                "id": "call_1",
                "name": "custom_custom_lookup",
                "input": {}
            }],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let response = anthropic_to_response_with_oauth(
            &upstream,
            "claude-test",
            &response_tool_map(&request),
            true,
        )
        .expect("oauth tool response");
        assert_eq!(response["output"][0]["name"], "custom_lookup");
    }

    #[test]
    fn non_oauth_response_preserves_names_starting_with_custom() {
        let request = json!({
            "tools": [{
                "type": "function",
                "name": "custom_lookup",
                "parameters": {"type": "object"}
            }]
        });
        let upstream = json!({
            "content": [{
                "type": "tool_use",
                "id": "call_1",
                "name": "custom_lookup",
                "input": {}
            }],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });

        let response = anthropic_to_response(
            &upstream,
            "claude-test",
            &response_tool_map(&request),
        )
        .expect("ordinary Anthropic tool response");
        assert_eq!(response["output"][0]["name"], "custom_lookup");
    }

    #[test]
    fn oauth_response_disambiguates_prefixed_and_real_custom_names() {
        let request = json!({
            "tools": [
                {"type": "function", "name": "lookup", "parameters": {"type": "object"}},
                {"type": "function", "name": "custom_lookup", "parameters": {"type": "object"}}
            ]
        });
        let tool_map = response_tool_map(&request);
        let lookup = anthropic_to_response_with_oauth(
            &json!({
                "content": [{
                    "type": "tool_use",
                    "id": "call_1",
                    "name": "custom_lookup",
                    "input": {}
                }],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }),
            "claude-test",
            &tool_map,
            true,
        )
        .expect("prefixed lookup response");
        assert_eq!(lookup["output"][0]["name"], "lookup");

        let custom_lookup = anthropic_to_response_with_oauth(
            &json!({
                "content": [{
                    "type": "tool_use",
                    "id": "call_2",
                    "name": "custom_custom_lookup",
                    "input": {}
                }],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }),
            "claude-test",
            &tool_map,
            true,
        )
        .expect("double-prefixed custom lookup response");
        assert_eq!(custom_lookup["output"][0]["name"], "custom_lookup");
    }

    #[test]
    fn stream_only_strips_custom_prefix_for_subscription_oauth() {
        let event = json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "type": "tool_use",
                "id": "call_1",
                "name": "custom_custom_lookup",
                "input": {}
            }
        });
        let mut stream_state = AnthropicStreamState::default();
        let ordinary = anthropic_stream_to_chat(&event, &mut stream_state, false)
            .expect("ordinary stream event")
            .expect("ordinary stream chunk");
        assert_eq!(
            ordinary["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
            "custom_custom_lookup"
        );

        let oauth = anthropic_stream_to_chat(&event, &mut stream_state, true)
            .expect("OAuth stream event")
            .expect("OAuth stream chunk");
        assert_eq!(
            oauth["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
            "custom_lookup"
        );
    }

    #[test]
    fn stream_redacted_thinking_is_emitted_as_verbatim_provider_metadata() {
        let start = json!({
            "type": "content_block_start",
            "index": 3,
            "content_block": {"type": "redacted_thinking", "data": "OPAQUE1"}
        });
        let mut stream_state = AnthropicStreamState::default();

        assert!(anthropic_stream_to_chat(&start, &mut stream_state, false)
            .expect("redacted block start")
            .is_none());
        let chunk = anthropic_stream_to_chat(
            &json!({"type": "content_block_stop", "index": 3}),
            &mut stream_state,
            false,
        )
        .expect("redacted block stop")
        .expect("redacted metadata chunk");

        assert_eq!(
            chunk["choices"][0]["delta"]["codetas_anthropic_thinking_block"],
            json!({"type": "redacted_thinking", "data": "OPAQUE1"})
        );
    }

    #[test]
    fn stream_thinking_blocks_keep_independent_text_and_signatures() {
        let mut state = AnthropicStreamState::default();
        let mut completed = Vec::new();
        let events = [
            json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "redacted_thinking", "data": "OPAQUE1"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1,
                "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 1,
                "delta": {"type": "thinking_delta", "thinking": "first"}}),
            json!({"type": "content_block_delta", "index": 1,
                "delta": {"type": "signature_delta", "signature": "sig-1"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2,
                "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 2,
                "delta": {"type": "thinking_delta", "thinking": "second"}}),
            json!({"type": "content_block_delta", "index": 2,
                "delta": {"type": "signature_delta", "signature": "sig-2"}}),
            json!({"type": "content_block_stop", "index": 2}),
        ];

        for event in events {
            if let Some(chunk) = anthropic_stream_to_chat(&event, &mut state, false)
                .expect("Anthropic stream event")
            {
                if let Some(block) = chunk
                    .pointer("/choices/0/delta/codetas_anthropic_thinking_block")
                    .cloned()
                {
                    completed.push(block);
                }
            }
        }

        assert_eq!(
            completed,
            vec![
                json!({"type": "redacted_thinking", "data": "OPAQUE1"}),
                json!({"type": "thinking", "thinking": "first", "signature": "sig-1"}),
                json!({"type": "thinking", "thinking": "second", "signature": "sig-2"}),
            ]
        );
    }

    #[test]
    fn stream_single_thinking_block_is_finalized_verbatim() {
        let mut state = AnthropicStreamState::default();
        for event in [
            json!({"type": "content_block_start", "index": 4,
                "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 4,
                "delta": {"type": "thinking_delta", "thinking": "one thought"}}),
            json!({"type": "content_block_delta", "index": 4,
                "delta": {"type": "signature_delta", "signature": "sig-one"}}),
        ] {
            anthropic_stream_to_chat(&event, &mut state, false).expect("thinking event");
        }

        let chunk = anthropic_stream_to_chat(
            &json!({"type": "content_block_stop", "index": 4}),
            &mut state,
            false,
        )
        .expect("thinking stop")
        .expect("finalized thinking block");
        assert_eq!(
            chunk["choices"][0]["delta"]["codetas_anthropic_thinking_block"],
            json!({"type": "thinking", "thinking": "one thought", "signature": "sig-one"})
        );
    }

    #[test]
    fn stream_reasoning_deltas_are_scoped_to_thinking_blocks() {
        let mut stream_state = AnthropicStreamState::default();
        anthropic_stream_to_chat(
            &json!({
                "type": "content_block_start", "index": 0,
                "content_block": {"type": "text", "text": ""}
            }),
            &mut stream_state,
            false,
        )
        .expect("text block start");

        let stray = anthropic_stream_to_chat(
            &json!({
                "type": "content_block_delta", "index": 0,
                "delta": {"type": "signature_delta", "signature": "STRAY"}
            }),
            &mut stream_state,
            false,
        )
        .expect("stray signature event");

        assert!(stray.is_none());
    }
}
