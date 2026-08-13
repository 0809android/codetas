use crate::compaction::decode_summary;
use crate::translate::tool_input_to_value;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub fn responses_to_gemini(body: &Value, _model: &str) -> Result<Value, String> {
    let object = body
        .as_object()
        .ok_or_else(|| "Responses request must be a JSON object".to_string())?;
    let mut contents = Vec::new();
    let mut call_names = BTreeMap::<String, String>::new();
    match object.get("input") {
        Some(Value::String(text)) => {
            contents.push(json!({"role": "user", "parts": [{"text": text}]}))
        }
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
                        let role = if item.get("role").and_then(Value::as_str) == Some("assistant")
                        {
                            "model"
                        } else {
                            "user"
                        };
                        contents.push(json!({
                            "role": role,
                            "parts": response_content_to_gemini(item.get("content"))?
                        }));
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
                        call_names.insert(call_id.into(), name.into());
                        // custom_tool_call items carry the payload in `input`
                        // while function_call uses `arguments`. A structured
                        // payload is preserved as-is instead of collapsing to {}.
                        let args = item
                            .get("input")
                            .or_else(|| item.get("arguments"))
                            .map(tool_input_to_value)
                            .unwrap_or_else(|| json!({}));
                        let thought_signature = item
                            .get("provider_metadata")
                            .and_then(|metadata| metadata.pointer("/gemini/thought_signature"))
                            .cloned();
                        let mut function_call = json!({"name": name, "args": args, "id": call_id});
                        if let Some(signature) = thought_signature {
                            function_call["thoughtSignature"] = signature;
                        }
                        contents.push(json!({
                            "role": "model",
                            "parts": [{"functionCall": function_call}]
                        }));
                    }
                    "function_call_output" | "custom_tool_call_output" => {
                        let call_id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .ok_or("tool output requires call_id")?;
                        let name = call_names
                            .get(call_id)
                            .map(String::as_str)
                            .unwrap_or(call_id);
                        let output = item.get("output").cloned().unwrap_or(Value::Null);
                        contents.push(json!({
                            "role": "user",
                            "parts": [{"functionResponse": {"id": call_id, "name": name, "response": {"output": output}}}]
                        }));
                    }
                    "compaction" => {
                        if let Ok(Some(summary)) = decode_summary(&Value::Object(item.clone())) {
                            contents.push(json!({
                                "role": "user",
                                "parts": [{"text": summary}]
                            }));
                        }
                    }
                    "reasoning" => {}
                    _ => {}
                }
            }
        }
        Some(Value::Null) | None => {}
        Some(_) => return Err("Responses input must be a string or array".into()),
    }

    let mut request = Map::new();
    request.insert("contents".into(), json!(merge_adjacent_contents(contents)));
    if let Some(instructions) = object.get("instructions").and_then(Value::as_str) {
        request.insert(
            "systemInstruction".into(),
            json!({"parts": [{"text": instructions}]}),
        );
    }
    let mut generation = Map::new();
    if let Some(value) = object.get("temperature") {
        generation.insert("temperature".into(), value.clone());
    }
    if let Some(value) = object.get("top_p") {
        generation.insert("topP".into(), value.clone());
    }
    if let Some(value) = object.get("max_output_tokens") {
        generation.insert("maxOutputTokens".into(), value.clone());
    }
    if !generation.is_empty() {
        request.insert("generationConfig".into(), Value::Object(generation));
    }
    if let Some(Value::Array(tools)) = object.get("tools") {
        let declarations = tools
            .iter()
            .filter_map(|tool| match response_tool_to_gemini(tool) {
                Ok(Some(tool)) => Some(Ok(tool)),
                Ok(None) => None,
                Err(message) => Some(Err(message)),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if !declarations.is_empty() {
            request.insert(
                "tools".into(),
                json!([{"functionDeclarations": declarations}]),
            );
        }
    }
    if let Some(choice) = object.get("tool_choice") {
        if !choice.is_null() {
            request.insert("toolConfig".into(), response_tool_choice_to_gemini(choice)?);
        }
    }
    Ok(Value::Object(request))
}

pub fn gemini_to_response(
    value: &Value,
    exposed_model: &str,
    custom_tools: &BTreeSet<String>,
) -> Result<Value, String> {
    let value = unwrap_cloud_code_assist_response(value);
    let candidate = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
        .ok_or("Gemini response has no candidate")?;
    let parts = candidate
        .pointer("/content/parts")
        .and_then(Value::as_array)
        .ok_or("Gemini candidate has no parts")?;
    let mut text = String::new();
    let mut output = Vec::new();
    for part in parts {
        if let Some(value) = part.get("text").and_then(Value::as_str) {
            if part.get("thought").and_then(Value::as_bool) == Some(true) {
                output.push(json!({
                    "id": format!("rs_{}", Uuid::new_v4().simple()),
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": value}]
                }));
            } else {
                text.push_str(value);
            }
        }
        if let Some(call) = part.get("functionCall") {
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let call_id = call
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
            let arguments = serde_json::to_string(call.get("args").unwrap_or(&Value::Null))
                .unwrap_or_else(|_| "{}".into());
            let mut item = if custom_tools.contains(name) {
                json!({
                    "id": format!("ctc_{}", Uuid::new_v4().simple()),
                    "type": "custom_tool_call",
                    "status": "completed",
                    "call_id": call_id,
                    "name": name,
                    "input": arguments
                })
            } else {
                json!({
                    "id": format!("fc_{}", Uuid::new_v4().simple()),
                    "type": "function_call",
                    "status": "completed",
                    "call_id": call_id,
                    "name": name,
                    "arguments": arguments
                })
            };
            if let Some(signature) = call.get("thoughtSignature") {
                item["provider_metadata"] = json!({"gemini": {"thought_signature": signature}});
            }
            output.push(item);
        }
    }
    if !text.is_empty() {
        output.push(json!({
            "id": format!("msg_{}", Uuid::new_v4().simple()),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}]
        }));
    }
    let finish_reason = candidate.get("finishReason").and_then(Value::as_str);
    let incomplete = matches!(finish_reason, Some("MAX_TOKENS" | "SAFETY" | "RECITATION"));
    Ok(response_object(
        exposed_model,
        if incomplete {
            "incomplete"
        } else {
            "completed"
        },
        output,
        gemini_usage(value.get("usageMetadata")),
        incomplete.then(
            || json!({"reason": finish_reason.unwrap_or("provider_limit").to_ascii_lowercase()}),
        ),
    ))
}

pub fn gemini_stream_to_chat(value: &Value) -> Result<Option<Value>, String> {
    if let Some(error) = value.get("error") {
        return Err(error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Gemini stream error")
            .into());
    }
    let value = unwrap_cloud_code_assist_response(value);
    if let Some(error) = value.get("error") {
        return Err(error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Gemini stream error")
            .into());
    }
    let mut content = String::new();
    let mut reasoning_content = String::new();
    let mut tool_calls = Vec::new();
    if let Some(parts) = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
        .and_then(|candidate| candidate.pointer("/content/parts"))
        .and_then(Value::as_array)
    {
        for (index, part) in parts.iter().enumerate() {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if part.get("thought").and_then(Value::as_bool) == Some(true) {
                    reasoning_content.push_str(text);
                } else {
                    content.push_str(text);
                }
            }
            if let Some(call) = part.get("functionCall") {
                let call_id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
                tool_calls.push(json!({
                    "index": index,
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": call.get("name").and_then(Value::as_str).unwrap_or("unknown"),
                        "arguments": serde_json::to_string(call.get("args").unwrap_or(&Value::Null)).unwrap_or_else(|_| "{}".into())
                    }
                }));
            }
        }
    }
    let usage = value.get("usageMetadata").map(|usage| json!({
        "prompt_tokens": usage.get("promptTokenCount").and_then(Value::as_u64).unwrap_or(0),
        "completion_tokens": usage.get("candidatesTokenCount").and_then(Value::as_u64).unwrap_or(0),
        "total_tokens": usage.get("totalTokenCount").and_then(Value::as_u64).unwrap_or(0)
    }));
    if content.is_empty()
        && reasoning_content.is_empty()
        && tool_calls.is_empty()
        && usage.is_none()
    {
        return Ok(None);
    }
    let mut chunk = json!({"choices": [{"delta": {}}]});
    if !content.is_empty() {
        chunk["choices"][0]["delta"]["content"] = json!(content);
    }
    if !reasoning_content.is_empty() {
        chunk["choices"][0]["delta"]["reasoning_content"] = json!(reasoning_content);
    }
    if !tool_calls.is_empty() {
        chunk["choices"][0]["delta"]["tool_calls"] = Value::Array(tool_calls);
    }
    if let Some(usage) = usage {
        chunk["usage"] = usage;
    }
    Ok(Some(chunk))
}

fn unwrap_cloud_code_assist_response(value: &Value) -> &Value {
    value
        .get("response")
        .filter(|response| response.is_object())
        .unwrap_or(value)
}

fn response_content_to_gemini(content: Option<&Value>) -> Result<Vec<Value>, String> {
    match content {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![json!({"text": text})]),
        Some(Value::Array(parts)) => Ok(parts
            .iter()
            .filter_map(|part| {
                let part = part.as_object()?;
                match part.get("type").and_then(Value::as_str) {
                    Some("input_text" | "output_text" | "text") => Some(json!({
                        "text": part.get("text").and_then(Value::as_str).unwrap_or_default()
                    })),
                    Some("input_image" | "image_url") => {
                        let url = part
                            .get("image_url")
                            .and_then(|value| {
                                value
                                    .as_str()
                                    .or_else(|| value.get("url").and_then(Value::as_str))
                            })
                            .or_else(|| part.get("url").and_then(Value::as_str))?;
                        Some(if let Some((mime_type, data)) = parse_data_url(url) {
                            json!({"inlineData": {"mimeType": mime_type, "data": data}})
                        } else {
                            json!({"fileData": {"fileUri": url}})
                        })
                    }
                    _ => None,
                }
            })
            .collect()),
        Some(_) => Err("message content must be string or array".into()),
    }
}

fn response_tool_to_gemini(tool: &Value) -> Result<Option<Value>, String> {
    if tool.get("type").and_then(Value::as_str) != Some("function") {
        // Codex includes custom tools opportunistically even when a turn does
        // not require them. Gemini's function declarations cannot represent
        // custom tools; omitting them keeps otherwise compatible requests
        // usable instead of rejecting the whole conversation.
        return Ok(None);
    }
    Ok(Some(json!({
        "name": tool.get("name").and_then(Value::as_str).ok_or("function tool requires name")?,
        "description": tool.get("description").and_then(Value::as_str).unwrap_or_default(),
        "parameters": tool.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"}))
    })))
}

fn response_tool_choice_to_gemini(choice: &Value) -> Result<Value, String> {
    match choice.as_str() {
        Some("auto") => Ok(json!({"functionCallingConfig": {"mode": "AUTO"}})),
        Some("required") => Ok(json!({"functionCallingConfig": {"mode": "ANY"}})),
        Some("none") => Ok(json!({"functionCallingConfig": {"mode": "NONE"}})),
        Some(_) => Err("unsupported Gemini tool choice".into()),
        None if choice.get("type").and_then(Value::as_str) == Some("function") => Ok(json!({
            "functionCallingConfig": {
                "mode": "ANY",
                "allowedFunctionNames": [choice.get("name").and_then(Value::as_str).ok_or("function tool choice requires name")?]
            }
        })),
        None => Err("unsupported Gemini tool choice".into()),
    }
}

fn merge_adjacent_contents(contents: Vec<Value>) -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::new();
    for content in contents {
        let same_role = merged.last().and_then(|last| last.get("role")) == content.get("role")
            && !has_thought_signature(merged.last())
            && !has_thought_signature(Some(&content));
        if same_role {
            let incoming = content
                .get("parts")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if let Some(parts) = merged
                .last_mut()
                .and_then(|last| last.get_mut("parts"))
                .and_then(Value::as_array_mut)
            {
                parts.extend(incoming);
            }
        } else {
            merged.push(content);
        }
    }
    merged
}

fn has_thought_signature(content: Option<&Value>) -> bool {
    content
        .and_then(|value| value.get("parts"))
        .and_then(Value::as_array)
        .is_some_and(|parts| {
            parts.iter().any(|part| {
                part.get("thoughtSignature").is_some()
                    || part.pointer("/functionCall/thoughtSignature").is_some()
            })
        })
}

fn parse_data_url(value: &str) -> Option<(&str, &str)> {
    let value = value.strip_prefix("data:")?;
    let (metadata, data) = value.split_once(',')?;
    Some((metadata.strip_suffix(";base64")?, data))
}

fn gemini_usage(usage: Option<&Value>) -> Value {
    let input = usage
        .and_then(|value| value.get("promptTokenCount"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .and_then(|value| value.get("candidatesTokenCount"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = usage
        .and_then(|value| value.get("thoughtsTokenCount"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = usage
        .and_then(|value| value.get("totalTokenCount"))
        .and_then(Value::as_u64)
        .unwrap_or(input + output + reasoning);
    json!({
        "input_tokens": input,
        "input_tokens_details": {"cached_tokens": usage.and_then(|value| value.get("cachedContentTokenCount")).and_then(Value::as_u64).unwrap_or(0)},
        "output_tokens": output + reasoning,
        "output_tokens_details": {"reasoning_tokens": reasoning},
        "total_tokens": total
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_text_and_function_tools() {
        let request = json!({
            "instructions": "Be concise",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}],
            "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}}]
        });
        let translated =
            responses_to_gemini(&request, "gemini-test").expect("request should translate");
        assert_eq!(translated["contents"][0]["parts"][0]["text"], "hello");
        assert_eq!(
            translated["tools"][0]["functionDeclarations"][0]["name"],
            "lookup"
        );
    }

    #[test]
    fn preserves_structured_custom_tool_input() {
        // A structured `input` (malformed client) must survive instead of
        // collapsing to {} — functionCall.args keeps the payload.
        let request = json!({
            "input": [
                {"type": "custom_tool_call", "call_id": "call_1", "name": "exec", "input": {"command": "ls"}}
            ]
        });
        let translated =
            responses_to_gemini(&request, "gemini-test").expect("request should translate");
        assert_eq!(
            translated["contents"][0]["parts"][0]["functionCall"]["args"],
            json!({"command": "ls"})
        );
    }
}
