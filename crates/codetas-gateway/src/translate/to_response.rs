use super::*;

pub fn chat_to_response(
    chat: &Value,
    exposed_model: &str,
    tool_map: &ResponseToolMap,
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
            let identity = tool_map
                .identity(name)
                .cloned()
                .unwrap_or_else(|| ResponseToolIdentity {
                    name: name.to_string(),
                    namespace: None,
                    kind: ResponseToolKind::Function,
                });
            match identity.kind {
                ResponseToolKind::Custom => {
                    let mut item = json!({
                        "id": format!("ctc_{}", Uuid::new_v4().simple()),
                        "type": "custom_tool_call",
                        "status": "completed",
                        "call_id": call.get("id").and_then(Value::as_str).unwrap_or("call_unknown"),
                        "name": identity.name,
                        "input": unwrap_custom_tool_arguments(arguments)
                    });
                    insert_tool_namespace(&mut item, identity.namespace.as_deref());
                    output.push(item);
                }
                ResponseToolKind::ToolSearch => output.push(json!({
                    "id": format!("tsc_{}", Uuid::new_v4().simple()),
                    "type": "tool_search_call",
                    "status": "completed",
                    "call_id": call.get("id").and_then(Value::as_str).unwrap_or("call_unknown"),
                    "execution": "client",
                    "arguments": arguments_to_value(arguments)
                })),
                ResponseToolKind::Function => {
                    let mut item = json!({
                        "id": format!("fc_{}", Uuid::new_v4().simple()),
                        "type": "function_call",
                        "status": "completed",
                        "call_id": call.get("id").and_then(Value::as_str).unwrap_or("call_unknown"),
                        "name": identity.name,
                        "arguments": arguments
                    });
                    insert_tool_namespace(&mut item, identity.namespace.as_deref());
                    output.push(item);
                }
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
