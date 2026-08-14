use super::*;

pub(crate) fn tool_item(state: &ToolState, status: &str) -> Value {
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

pub(crate) fn chat_usage_to_responses(usage: Option<&Value>) -> Value {
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

pub(crate) fn response_id() -> String {
    format!("resp_{}", Uuid::new_v4().simple())
}

pub(crate) fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}
