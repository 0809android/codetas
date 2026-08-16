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
    let finish_reason = choice.get("finish_reason").and_then(Value::as_str);
    let (status, incomplete_reason, provider_failure) = match finish_reason {
        None | Some("stop" | "tool_calls") => ("completed", None, None),
        Some("length" | "max_tokens") => {
            ("incomplete", Some("max_output_tokens"), None)
        }
        Some("content_filter") => ("incomplete", Some("content_filter"), None),
        Some(reason) => ("failed", None, Some(reason)),
    };
    let mut output = Vec::new();
    let provider_metadata = message.get("codetas_provider_metadata").cloned();

    if let Some(reasoning) = message
        .get("reasoning_content")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        let mut item = json!({
            "id": format!("rs_{}", Uuid::new_v4().simple()),
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": reasoning}],
        });
        if let Some(metadata) = provider_metadata.clone() {
            item["provider_metadata"] = metadata;
        }
        if let Some(signature) = message.get("reasoning_signature") {
            item["encrypted_content"] = signature.clone();
        }
        output.push(item);
    }

    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            let mut item = json!({
                "id": format!("msg_{}", Uuid::new_v4().simple()),
                "type": "message",
                "status": status,
                "role": "assistant",
                "content": [{"type": "output_text", "text": text, "annotations": []}]
            });
            if let Some(metadata) = provider_metadata.clone() {
                item["provider_metadata"] = metadata;
            }
            output.push(item);
        }
    }
    let tool_calls = message.get("tool_calls").and_then(Value::as_array);
    if finish_reason == Some("tool_calls") && tool_calls.is_none_or(Vec::is_empty) {
        return Err("Chat Completions ended with tool_calls but supplied no calls".into());
    }
    if let Some(tool_calls) = tool_calls {
        if tool_calls.is_empty() && finish_reason == Some("tool_calls") {
            return Err("Chat Completions ended with tool_calls but supplied no calls".into());
        }
        for call in tool_calls {
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .ok_or("Chat Completions function call requires a non-empty name")?;
            let call_id = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .ok_or("Chat Completions function call requires a non-empty id")?;
            let arguments = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .ok_or("Chat Completions function call requires arguments")?;
            serde_json::from_str::<Value>(arguments)
                .map_err(|_| "Chat Completions function call arguments are incomplete JSON")?;
            let identity = tool_map
                .identity(name)
                .cloned()
                .unwrap_or_else(|| ResponseToolIdentity {
                    name: name.to_string(),
                    namespace: None,
                    kind: ResponseToolKind::Function,
                });
            // A non-completed turn cannot prove that a tool call is terminal,
            // even if the currently buffered JSON happens to parse. Preserve
            // the response-level incomplete disposition without persisting a
            // callable continuation item.
            if status != "completed" {
                continue;
            }
            match identity.kind {
                ResponseToolKind::Custom => {
                    let mut item = json!({
                        "id": format!("ctc_{}", Uuid::new_v4().simple()),
                        "type": "custom_tool_call",
                        "status": status,
                        "call_id": call_id,
                        "name": identity.name,
                        "input": unwrap_custom_tool_arguments(arguments)
                    });
                    insert_tool_namespace(&mut item, identity.namespace.as_deref());
                    if let Some(metadata) = provider_metadata.clone() {
                        item["provider_metadata"] = metadata;
                    }
                    output.push(item);
                }
                ResponseToolKind::ToolSearch => {
                    let mut item = json!({
                        "id": format!("tsc_{}", Uuid::new_v4().simple()),
                        "type": "tool_search_call",
                        "status": status,
                        "call_id": call_id,
                        "execution": "client",
                        "arguments": arguments_to_value(arguments)
                    });
                    if let Some(metadata) = provider_metadata.clone() {
                        item["provider_metadata"] = metadata;
                    }
                    output.push(item);
                }
                ResponseToolKind::Function => {
                    let mut item = json!({
                        "id": format!("fc_{}", Uuid::new_v4().simple()),
                        "type": "function_call",
                        "status": status,
                        "call_id": call_id,
                        "name": identity.name,
                        "arguments": arguments
                    });
                    insert_tool_namespace(&mut item, identity.namespace.as_deref());
                    if let Some(metadata) = provider_metadata.clone() {
                        item["provider_metadata"] = metadata;
                    }
                    output.push(item);
                }
            }
        }
    }

    let completed_at = (status == "completed").then(|| unix_seconds());
    let incomplete_details = incomplete_reason.map(|reason| json!({"reason": reason}));
    let error = provider_failure.map(|reason| json!({
        "code": "provider_completion_failed",
        "message": format!("Chat provider stopped with {reason}")
    }));
    Ok(json!({
        "id": response_id,
        "object": "response",
        "created_at": unix_seconds(),
        "completed_at": completed_at,
        "status": status,
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
        "error": error,
        "incomplete_details": incomplete_details
    }))
}
