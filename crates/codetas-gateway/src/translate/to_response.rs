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
    let flattened = flatten_message_content_list(message);
    let message = flattened.as_ref().unwrap_or(message);
    let response_id = response_id();
    let finish_reason = choice.get("finish_reason").and_then(Value::as_str);
    let (mut status, mut incomplete_reason, provider_failure) = match finish_reason {
        None | Some("stop" | "tool_calls") => ("completed", None, None),
        Some("length" | "max_tokens") => ("incomplete", Some("max_output_tokens"), None),
        Some("content_filter") => ("incomplete", Some("content_filter"), None),
        Some(reason) => ("failed", None, Some(reason)),
    };
    let end = match finish_reason {
        Some("tool_calls") => TurnEnd::ToolCalls,
        None | Some("stop") => TurnEnd::Normal,
        Some("length" | "max_tokens" | "content_filter") => TurnEnd::CutOff,
        Some(_) => TurnEnd::Failed,
    };
    let mut output = Vec::new();
    let mut skipped_invalid_custom = false;
    let provider_metadata = message.get("codetas_provider_metadata").cloned();

    if let Some(reasoning) = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
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
            // A failed turn delivers no call. Everything below is the stream's rule, so
            // the two paths give Codex the same calls for the same provider result.
            if end == TurnEnd::Failed {
                continue;
            }
            let incoming_arguments = chat_tool_call_arguments(call);
            let arguments = match &incoming_arguments {
                ChatToolArguments::Absent => "",
                ChatToolArguments::Text(arguments) => arguments.as_ref(),
                ChatToolArguments::Invalid => {
                    if incomplete_reason.is_none() {
                        status = "incomplete";
                        incomplete_reason = Some("invalid_tool_call");
                    }
                    continue;
                }
            };
            let identity =
                tool_map
                    .identity(name)
                    .cloned()
                    .unwrap_or_else(|| ResponseToolIdentity {
                        name: name.to_string(),
                        namespace: None,
                        kind: ResponseToolKind::Function,
                    });
            // No arguments on a call that ended normally means none were coming, while
            // unparsable ones are never run. A cut-off turn proves nothing about a call
            // except that a whole JSON object was closed; the response-level incomplete
            // disposition stays either way.
            let cut_off = end == TurnEnd::CutOff;
            let arguments = if identity.kind == ResponseToolKind::Function
                && !cut_off
                && matches!(arguments.trim(), "" | "null")
            {
                "{}"
            } else {
                arguments
            };
            // The provider's own tool-call parser may glue a parameter into the
            // previous string value; move it back so the call runs as intended.
            let repaired = (identity.kind == ResponseToolKind::Function)
                .then(|| repair_leaked_dsml_arguments(arguments))
                .flatten();
            let arguments = repaired.as_deref().unwrap_or(arguments);
            if identity.kind != ResponseToolKind::Custom
                && !serde_json::from_str::<Value>(arguments)
                    .is_ok_and(|value| !cut_off || value.is_object())
            {
                if incomplete_reason.is_none() {
                    status = "incomplete";
                    incomplete_reason = Some("invalid_tool_call");
                }
                continue;
            }
            match identity.kind {
                ResponseToolKind::Custom => {
                    if !custom_call_is_deliverable(&identity.name, arguments, end) {
                        let is_empty_apply_patch = identity.name == "apply_patch"
                            && !custom_tool_input_is_present(&identity.name, arguments);
                        if is_empty_apply_patch {
                            skipped_invalid_custom = true;
                        } else if incomplete_reason.is_none() {
                            status = "incomplete";
                            incomplete_reason = Some("invalid_tool_call");
                        }
                        continue;
                    }
                    let mut item = json!({
                        "id": format!("ctc_{}", Uuid::new_v4().simple()),
                        "type": "custom_tool_call",
                        "status": "completed",
                        "call_id": call_id,
                        "name": identity.name,
                        "input": unwrap_custom_tool_arguments(&identity.name, arguments)
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
                        "status": "completed",
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
                        "status": "completed",
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

    let has_actionable_tool = output.iter().any(|item| {
        matches!(
            item.get("type").and_then(Value::as_str),
            Some("function_call" | "custom_tool_call" | "tool_search_call")
        )
    });
    let message_text = message.get("content").and_then(Value::as_str).unwrap_or("");
    if provider_failure.is_none() && incomplete_reason.is_none() && !has_actionable_tool {
        let has_real_text =
            !message_text.trim().is_empty() && !is_placeholder_progress_text(message_text);
        if skipped_invalid_custom && !has_real_text {
            status = "incomplete";
            incomplete_reason = Some("invalid_tool_call");
        } else if is_placeholder_progress_text(message_text) {
            status = "incomplete";
            incomplete_reason = Some("empty_response");
        }
    }
    let completed_at = (status == "completed").then(unix_seconds);
    let incomplete_details = incomplete_reason.map(|reason| json!({"reason": reason}));
    let error = provider_failure.map(|reason| {
        json!({
            "code": "provider_completion_failed",
            "message": format!("Chat provider stopped with {reason}")
        })
    });
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
