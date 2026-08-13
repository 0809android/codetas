use crate::compaction::decode_summary;
use crate::translate::tool_input_to_value;
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
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
                        let input = item
                            .get("input")
                            .or_else(|| item.get("arguments"))
                            .map(tool_input_to_value)
                            .unwrap_or_else(|| json!({}));
                        let name = if subscription_oauth {
                            prefix_anthropic_oauth_tool_name(name)
                        } else {
                            name.to_string()
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
                        let content = output_to_text(item.get("output").unwrap_or(&Value::Null));
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
    if let Some(Value::Array(tools)) = object.get("tools") {
        let tools = tools
            .iter()
            .filter_map(|tool| match response_tool_to_anthropic(tool) {
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
                || choice.get("type").and_then(Value::as_str) == Some("function");
            if thinking_enabled && forced {
                return Err(
                    "Anthropic thinking cannot be combined with a forced tool choice".into(),
                );
            }
            let mut tool_choice = response_tool_choice_to_anthropic(choice)?;
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
    custom_tools: &BTreeSet<String>,
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
                let name = strip_anthropic_oauth_tool_name(
                    block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                );
                if custom_tools.contains(name) {
                    output.push(json!({
                        "id": format!("ctc_{}", Uuid::new_v4().simple()),
                        "type": "custom_tool_call",
                        "status": "completed",
                        "call_id": block.get("id").and_then(Value::as_str).unwrap_or("call_unknown"),
                        "name": name,
                        "input": serde_json::to_string(&input).unwrap_or_else(|_| "{}".into())
                    }));
                } else {
                    output.push(json!({
                        "id": format!("fc_{}", Uuid::new_v4().simple()),
                        "type": "function_call",
                        "status": "completed",
                        "call_id": block.get("id").and_then(Value::as_str).unwrap_or("call_unknown"),
                        "name": name,
                        "arguments": serde_json::to_string(&input).unwrap_or_else(|_| "{}".into())
                    }));
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

pub fn anthropic_stream_to_chat(
    value: &Value,
    input_tokens: &mut u64,
) -> Result<Option<Value>, String> {
    match value.get("type").and_then(Value::as_str) {
        Some("message_start") => {
            *input_tokens = value
                .pointer("/message/usage/input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            Ok(None)
        }
        Some("content_block_start") => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
            let block = value
                .get("content_block")
                .ok_or("Anthropic content block is missing")?;
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                Ok(Some(json!({"choices": [{"delta": {"tool_calls": [{
                    "index": index,
                    "id": block.get("id").and_then(Value::as_str).unwrap_or("call_unknown"),
                    "type": "function",
                    "function": {"name": strip_anthropic_oauth_tool_name(block.get("name").and_then(Value::as_str).unwrap_or("unknown")), "arguments": ""}
                }]}}]})))
            } else {
                Ok(None)
            }
        }
        Some("content_block_delta") => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
            let delta = value.get("delta").ok_or("Anthropic delta is missing")?;
            match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") => Ok(Some(json!({"choices": [{"delta": {
                    "content": delta.get("text").and_then(Value::as_str).unwrap_or_default()
                }}]}))),
                Some("input_json_delta") => {
                    Ok(Some(json!({"choices": [{"delta": {"tool_calls": [{
                        "index": index,
                        "function": {"arguments": delta.get("partial_json").and_then(Value::as_str).unwrap_or_default()}
                    }]}}]})))
                }
                Some("thinking_delta") => Ok(Some(json!({"choices": [{"delta": {
                    "reasoning_content": delta.get("thinking").and_then(Value::as_str).unwrap_or_default()
                }}]}))),
                Some("signature_delta") => Ok(Some(json!({"choices": [{"delta": {
                    "reasoning_signature": delta.get("signature").and_then(Value::as_str).unwrap_or_default()
                }}]}))),
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
                "usage": {"prompt_tokens": *input_tokens, "completion_tokens": output, "total_tokens": *input_tokens + output}
            })))
        }
        Some("message_stop" | "content_block_stop" | "ping") => Ok(None),
        Some("error") => Err(value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("Anthropic stream error")
            .into()),
        Some(_) => Ok(None),
        None => Ok(None),
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
    if name.starts_with(CLAUDE_OAUTH_TOOL_PREFIX)
        || matches!(
            name,
            "web_search" | "code_execution" | "text_editor" | "computer"
        )
    {
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

fn response_tool_to_anthropic(tool: &Value) -> Result<Option<Value>, String> {
    if tool.get("type").and_then(Value::as_str) != Some("function") {
        // Chat Completions has no wire representation for Responses custom,
        // namespace, or hosted tools either. Codex includes custom tools
        // opportunistically even when a turn does not require them, so
        // rejecting the complete request makes otherwise compatible providers
        // unusable. Omit the incompatible entries and keep function tools.
        return Ok(None);
    }
    Ok(Some(json!({
        "name": tool.get("name").and_then(Value::as_str).ok_or("function tool requires name")?,
        "description": tool.get("description").and_then(Value::as_str).unwrap_or_default(),
        "input_schema": tool.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"}))
    })))
}

fn response_tool_choice_to_anthropic(choice: &Value) -> Result<Value, String> {
    match choice.as_str() {
        Some("auto") => Ok(json!({"type": "auto"})),
        Some("required") => Ok(json!({"type": "any"})),
        Some("none") => Ok(json!({"type": "none"})),
        Some(_) => Err("unsupported Anthropic tool choice".into()),
        None if choice.get("type").and_then(Value::as_str) == Some("function") => Ok(json!({
            "type": "tool",
            "name": choice.get("name").and_then(Value::as_str).ok_or("function tool choice requires name")?
        })),
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
            json!({"command": "ls"})
        );
    }
}
