use crate::compaction::decode_summary;
use crate::translate::{
    default_tool_search_parameters, insert_tool_namespace, response_allowed_tool_wire_names,
    response_tool_map, response_tool_parameters, tool_input_to_value, tool_search_output_to_text,
    unwrap_custom_tool_arguments, ResponseToolKind, ResponseToolMap,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GeminiFinishDisposition {
    Stop,
    ContentFilter,
    MaxTokens,
    MalformedFunctionCall,
    ProviderFailure,
}

pub(crate) fn gemini_finish_disposition(reason: Option<&str>) -> GeminiFinishDisposition {
    match reason {
        None | Some("STOP") => GeminiFinishDisposition::Stop,
        Some("MAX_TOKENS") => GeminiFinishDisposition::MaxTokens,
        Some("SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII") => {
            GeminiFinishDisposition::ContentFilter
        }
        Some("MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL") => {
            GeminiFinishDisposition::MalformedFunctionCall
        }
        Some(_) => GeminiFinishDisposition::ProviderFailure,
    }
}

pub fn responses_to_gemini(body: &Value, _model: &str) -> Result<Value, String> {
    let object = body
        .as_object()
        .ok_or_else(|| "Responses request must be a JSON object".to_string())?;
    let mut contents = Vec::new();
    let mut call_names = BTreeMap::<String, String>::new();
    let tool_map = response_tool_map(body);
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
                        let kind = if item.get("type").and_then(Value::as_str)
                            == Some("custom_tool_call")
                        {
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
                        let namespace = item.get("namespace").and_then(Value::as_str);
                        let wire_name = tool_map
                            .wire_name_for_identity(name, namespace, kind)
                            .map(str::to_string)
                            .unwrap_or_else(|| ResponseToolMap::wire_name(namespace, name));
                        call_names.insert(call_id.into(), wire_name.clone());
                        // custom_tool_call items carry the payload in `input`
                        // while function_call uses `arguments`. A structured
                        // payload is preserved as-is instead of collapsing to {}.
                        let payload = item
                            .get("input")
                            .or_else(|| item.get("arguments"))
                            .unwrap_or(&Value::Null);
                        let args = if kind == ResponseToolKind::Custom {
                            json!({"input": payload})
                        } else {
                            tool_input_to_value(payload)
                        };
                        let thought_signature = item
                            .get("provider_metadata")
                            .and_then(|metadata| metadata.pointer("/gemini/thought_signature"))
                            .cloned();
                        let function_call =
                            json!({"name": wire_name, "args": args, "id": call_id});
                        let mut part = json!({"functionCall": function_call});
                        if let Some(signature) = thought_signature {
                            // Gemini signs the entire content part, not the
                            // nested functionCall object.
                            part["thoughtSignature"] = signature;
                        }
                        contents.push(json!({
                            "role": "model",
                            "parts": [part]
                        }));
                    }
                    "tool_search_call" => {
                        let call_id = item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("call_unknown");
                        let wire_name = tool_map
                            .wire_name_for_identity(
                                "tool_search",
                                None,
                                ResponseToolKind::ToolSearch,
                            )
                            .unwrap_or("tool_search");
                        call_names.insert(call_id.into(), wire_name.into());
                        let args = item
                            .get("arguments")
                            .map(tool_input_to_value)
                            .unwrap_or_else(|| json!({}));
                        contents.push(json!({
                            "role": "model",
                            "parts": [{"functionCall": {"name": wire_name, "args": args, "id": call_id}}]
                        }));
                    }
                    "function_call_output" | "custom_tool_call_output" | "tool_search_output" => {
                        let call_id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .ok_or("tool output requires call_id")?;
                        let name = call_names
                            .get(call_id)
                            .map(String::as_str)
                            .unwrap_or(call_id);
                        let output = if item.get("type").and_then(Value::as_str)
                            == Some("tool_search_output")
                        {
                            Value::String(tool_search_output_to_text(
                                &Value::Object(item.clone()),
                                &tool_map,
                            ))
                        } else {
                            item.get("output").cloned().unwrap_or(Value::Null)
                        };
                        let response_output = gemini_function_response_output(&output);
                        let image_parts = if output.is_array() {
                            response_content_to_gemini(Some(&output))?
                                .into_iter()
                                .filter(|part| {
                                    part.get("inlineData").is_some()
                                        || part.get("fileData").is_some()
                                })
                                .collect::<Vec<_>>()
                        } else {
                            Vec::new()
                        };
                        let mut parts = vec![json!({
                            "functionResponse": {
                                "id": call_id,
                                "name": name,
                                "response": {"output": response_output}
                            }
                        })];
                        parts.extend(image_parts);
                        contents.push(json!({"role": "user", "parts": parts}));
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
    if let Some(format) = object
        .get("text")
        .and_then(Value::as_object)
        .and_then(|text| text.get("format"))
        .and_then(Value::as_object)
    {
        match format.get("type").and_then(Value::as_str) {
            Some("json_object") => {
                generation.insert("responseMimeType".into(), json!("application/json"));
            }
            Some("json_schema") => {
                generation.insert("responseMimeType".into(), json!("application/json"));
                if let Some(schema) = format.get("schema") {
                    generation.insert("responseJsonSchema".into(), schema.clone());
                }
            }
            _ => {}
        }
    }
    if !generation.is_empty() {
        request.insert("generationConfig".into(), Value::Object(generation));
    }
    let allowed_tool_names = object
        .get("tool_choice")
        .and_then(|choice| response_allowed_tool_wire_names(choice, &tool_map));
    let declarations = tool_map
        .iter()
        .filter(|(wire_name, _, _)| {
            allowed_tool_names
                .as_ref()
                .map(|allowed| allowed.contains(*wire_name))
                .unwrap_or(true)
        })
        .map(|(wire_name, identity, declaration)| {
            response_tool_to_gemini(wire_name, identity.kind, declaration)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !declarations.is_empty() {
        request.insert(
            "tools".into(),
            json!([{"functionDeclarations": declarations}]),
        );
    }
    if let Some(choice) = object.get("tool_choice") {
        if !choice.is_null() {
            request.insert(
                "toolConfig".into(),
                response_tool_choice_to_gemini(choice, &tool_map)?,
            );
        }
    }
    Ok(Value::Object(request))
}

fn gemini_function_response_output(value: &Value) -> Value {
    let Value::Array(parts) = value else {
        return value.clone();
    };
    Value::Array(
        parts
            .iter()
            .map(|part| {
                if gemini_response_image_url(part).is_some() {
                    json!({
                        "type": "input_text",
                        "text": "[image attached separately]"
                    })
                } else {
                    part.clone()
                }
            })
            .collect(),
    )
}

fn gemini_response_image_url(part: &Value) -> Option<&str> {
    let object = part.as_object()?;
    if !matches!(
        object.get("type").and_then(Value::as_str),
        Some("input_image" | "image_url")
    ) {
        return None;
    }
    object
        .get("image_url")
        .and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("url").and_then(Value::as_str))
        })
        .or_else(|| object.get("url").and_then(Value::as_str))
}

pub fn gemini_to_response(
    value: &Value,
    exposed_model: &str,
    tool_map: &ResponseToolMap,
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
            let wire_name = call
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
            let identity = tool_map.identity(wire_name);
            let name = identity
                .map(|identity| identity.name.as_str())
                .unwrap_or(wire_name);
            let namespace = identity.and_then(|identity| identity.namespace.as_deref());
            let kind = identity
                .map(|identity| identity.kind)
                .unwrap_or(ResponseToolKind::Function);
            let mut item = match kind {
                ResponseToolKind::Custom => {
                    let mut item = json!({
                        "id": format!("ctc_{}", Uuid::new_v4().simple()),
                        "type": "custom_tool_call",
                        "status": "completed",
                        "call_id": call_id,
                        "name": name,
                        "input": unwrap_custom_tool_arguments(&arguments)
                    });
                    insert_tool_namespace(&mut item, namespace);
                    item
                }
                ResponseToolKind::ToolSearch => json!({
                    "id": format!("tsc_{}", Uuid::new_v4().simple()),
                    "type": "tool_search_call",
                    "status": "completed",
                    "call_id": call_id,
                    "execution": "client",
                    "arguments": call.get("args").cloned().unwrap_or_else(|| json!({}))
                }),
                ResponseToolKind::Function => {
                    let mut item = json!({
                        "id": format!("fc_{}", Uuid::new_v4().simple()),
                        "type": "function_call",
                        "status": "completed",
                        "call_id": call_id,
                        "name": name,
                        "arguments": arguments
                    });
                    insert_tool_namespace(&mut item, namespace);
                    item
                }
            };
            if let Some(signature) = part
                .get("thoughtSignature")
                // Read legacy CODETAS history as a compatibility fallback, but
                // always write the canonical part-level representation.
                .or_else(|| call.get("thoughtSignature"))
            {
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
    let disposition = gemini_finish_disposition(finish_reason);
    if disposition == GeminiFinishDisposition::MalformedFunctionCall {
        return Err("Gemini returned a malformed function call".into());
    }
    let incomplete = matches!(
        disposition,
        GeminiFinishDisposition::MaxTokens | GeminiFinishDisposition::ContentFilter
    );
    let failed = disposition == GeminiFinishDisposition::ProviderFailure;
    Ok(response_object(
        exposed_model,
        if failed {
            "failed"
        } else if incomplete {
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
                let mut tool_call = json!({
                    "index": index,
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": call.get("name").and_then(Value::as_str).unwrap_or("unknown"),
                        "arguments": serde_json::to_string(call.get("args").unwrap_or(&Value::Null)).unwrap_or_else(|_| "{}".into())
                    }
                });
                if let Some(signature) = part
                    .get("thoughtSignature")
                    .or_else(|| call.get("thoughtSignature"))
                {
                    tool_call["codetas_provider_metadata"] =
                        json!({"gemini": {"thought_signature": signature}});
                }
                tool_calls.push(tool_call);
            }
        }
    }
    let usage = value.get("usageMetadata").map(|usage| json!({
        "prompt_tokens": usage.get("promptTokenCount").and_then(Value::as_u64).unwrap_or(0),
        "completion_tokens": usage.get("candidatesTokenCount").and_then(Value::as_u64).unwrap_or(0),
        "total_tokens": usage.get("totalTokenCount").and_then(Value::as_u64).unwrap_or(0)
    }));
    let finish_reason = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
        .and_then(|candidate| candidate.get("finishReason"))
        .and_then(Value::as_str);
    if content.is_empty()
        && reasoning_content.is_empty()
        && tool_calls.is_empty()
        && usage.is_none()
        && finish_reason.is_none()
    {
        return Ok(None);
    }
    let mut chunk = json!({"choices": [{"delta": {}}]});
    match gemini_finish_disposition(finish_reason) {
        GeminiFinishDisposition::Stop => {}
        GeminiFinishDisposition::MaxTokens => chunk["choices"][0]["finish_reason"] = json!("length"),
        GeminiFinishDisposition::ContentFilter => {
            chunk["choices"][0]["finish_reason"] = json!("content_filter")
        }
        GeminiFinishDisposition::MalformedFunctionCall => {
            return Err("Gemini returned a malformed function call".into())
        }
        GeminiFinishDisposition::ProviderFailure => {
            return Err(format!("Gemini stopped with {}", finish_reason.unwrap_or("UNKNOWN")))
        }
    }
    if !tool_calls.is_empty()
        && matches!(gemini_finish_disposition(finish_reason), GeminiFinishDisposition::MaxTokens)
    {
        return Err("Gemini truncated a function call at the token limit".into());
    }
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
                let object = part.as_object()?;
                match object.get("type").and_then(Value::as_str) {
                    Some("input_text" | "output_text" | "text") => Some(json!({
                        "text": object.get("text").and_then(Value::as_str).unwrap_or_default()
                    })),
                    Some("input_image" | "image_url") => {
                        let url = gemini_response_image_url(part)?;
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

fn response_tool_to_gemini(
    wire_name: &str,
    kind: ResponseToolKind,
    tool: &Value,
) -> Result<Value, String> {
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
    Ok(json!({
        "name": wire_name,
        "description": tool.get("description").and_then(Value::as_str).unwrap_or_default(),
        "parameters": parameters
    }))
}

fn response_tool_choice_to_gemini(
    choice: &Value,
    tool_map: &ResponseToolMap,
) -> Result<Value, String> {
    match choice.as_str() {
        Some("auto") => Ok(json!({"functionCallingConfig": {"mode": "AUTO"}})),
        Some("required") => Ok(json!({"functionCallingConfig": {"mode": "ANY"}})),
        Some("none") => Ok(json!({"functionCallingConfig": {"mode": "NONE"}})),
        Some(_) => Err("unsupported Gemini tool choice".into()),
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
            Ok(json!({
                "functionCallingConfig": {
                    "mode": "ANY",
                    "allowedFunctionNames": [wire_name]
                }
            }))
        }
        None if choice.get("type").and_then(Value::as_str) == Some("tool_search") => {
            let wire_name = tool_map
                .wire_name_for_identity("tool_search", None, ResponseToolKind::ToolSearch)
                .unwrap_or("tool_search");
            Ok(json!({
                "functionCallingConfig": {
                    "mode": "ANY",
                    "allowedFunctionNames": [wire_name]
                }
            }))
        }
        None if choice.get("type").and_then(Value::as_str) == Some("allowed_tools") => {
            let allowed = response_allowed_tool_wire_names(choice, tool_map)
                .unwrap_or_default();
            let mode = if allowed.is_empty() {
                "NONE"
            } else if choice.get("mode").and_then(Value::as_str) == Some("required") {
                "ANY"
            } else {
                "AUTO"
            };
            Ok(json!({"functionCallingConfig": {"mode": mode}}))
        }
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
    fn carries_tool_result_images_beside_the_gemini_function_response() {
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
        let translated = responses_to_gemini(&request, "gemini-test")
            .expect("request should translate");
        let parts = translated["contents"][1]["parts"]
            .as_array()
            .expect("tool result parts");
        assert_eq!(
            parts[0]["functionResponse"]["response"]["output"][0],
            request["input"][1]["output"][0]
        );
        assert_eq!(
            parts[0]["functionResponse"]["response"]["output"][1],
            json!({"type": "input_text", "text": "[image attached separately]"})
        );
        assert!(!parts[0]["functionResponse"]["response"]["output"]
            .to_string()
            .contains("AA=="));
        assert_eq!(parts[1]["inlineData"]["data"], "AA==");
    }

    #[test]
    fn preserves_structured_tool_result_output() {
        let request = json!({
            "input": [
                {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": {"count": 2, "ok": true}}
            ]
        });
        let translated = responses_to_gemini(&request, "gemini-test")
            .expect("request should translate");
        assert_eq!(
            translated["contents"][1]["parts"][0]["functionResponse"]["response"]["output"],
            json!({"count": 2, "ok": true})
        );
    }

    #[test]
    fn preserves_structured_array_tool_result_output() {
        let request = json!({
            "input": [
                {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": [{"type": "text", "text": "record", "id": 1}, {"id": 2}]}
            ]
        });
        let translated = responses_to_gemini(&request, "gemini-test")
            .expect("request should translate");
        assert_eq!(
            translated["contents"][1]["parts"][0]["functionResponse"]["response"]["output"],
            json!([{"type": "text", "text": "record", "id": 1}, {"id": 2}])
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
            json!({"input": {"command": "ls"}})
        );
    }

    #[test]
    fn preserves_tool_search_history_result_and_choice() {
        let request = json!({
            "input": [
                {
                    "type": "tool_search_call",
                    "call_id": "call_search",
                    "arguments": {"query": "calendar"}
                },
                {
                    "type": "tool_search_output",
                    "call_id": "call_search",
                    "tools": [{
                        "type": "namespace",
                        "name": "calendar",
                        "tools": [{
                            "type": "function",
                            "name": "create_event",
                            "inputSchema": {"type": "object"}
                        }]
                    }]
                }
            ],
            "tools": [{"type": "tool_search"}],
            "tool_choice": {"type": "tool_search"}
        });

        let translated =
            responses_to_gemini(&request, "gemini-test").expect("tool search request");
        assert_eq!(
            translated["contents"][0]["parts"][0]["functionCall"]["name"],
            "tool_search"
        );
        assert_eq!(
            translated["contents"][1]["parts"][0]["functionResponse"]["name"],
            "tool_search"
        );
        assert!(translated["contents"][1]["parts"][0]["functionResponse"]["response"]
            ["output"]
            .as_str()
            .is_some_and(|text| text.contains("calendar__create_event")));
        assert_eq!(
            translated["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"][0],
            "tool_search"
        );
        let declarations = translated["tools"][0]["functionDeclarations"]
            .as_array()
            .expect("declarations");
        assert!(declarations
            .iter()
            .any(|tool| tool["name"] == "calendar__create_event"));
    }

    #[test]
    fn allowed_tools_filters_gemini_declarations() {
        let request = json!({
            "tools": [
                {"type": "function", "name": "lookup", "parameters": {"type": "object"}},
                {"type": "custom", "name": "exec"}
            ],
            "tool_choice": {
                "type": "allowed_tools",
                "mode": "required",
                "tools": [{"type": "function", "name": "lookup"}]
            }
        });

        let translated =
            responses_to_gemini(&request, "gemini-test").expect("allowed tools request");
        let declarations = translated["tools"][0]["functionDeclarations"]
            .as_array()
            .expect("declarations");
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0]["name"], "lookup");
        assert_eq!(
            translated["toolConfig"]["functionCallingConfig"]["mode"],
            "ANY"
        );
    }

    #[test]
    fn thought_signature_round_trips_at_the_gemini_part_level() {
        let response = json!({
            "candidates": [{
                "content": {"parts": [{
                    "thoughtSignature": "signed-part",
                    "functionCall": {"id": "call_1", "name": "lookup", "args": {"q": "x"}}
                }]},
                "finishReason": "STOP"
            }]
        });
        let normalized = gemini_to_response(&response, "gemini-test", &ResponseToolMap::default())
            .expect("Gemini response");
        assert_eq!(
            normalized["output"][0]["provider_metadata"]["gemini"]["thought_signature"],
            "signed-part"
        );

        let replay = responses_to_gemini(
            &json!({"input": normalized["output"].clone()}),
            "gemini-test",
        )
        .expect("Gemini replay");
        let part = &replay["contents"][0]["parts"][0];
        assert_eq!(part["thoughtSignature"], "signed-part");
        assert!(part["functionCall"].get("thoughtSignature").is_none());
    }

    #[test]
    fn legacy_nested_thought_signature_is_read_but_replayed_canonically() {
        let response = json!({
            "candidates": [{
                "content": {"parts": [{
                    "functionCall": {
                        "id": "call_legacy", "name": "lookup", "args": {},
                        "thoughtSignature": "legacy-signature"
                    }
                }]},
                "finishReason": "STOP"
            }]
        });
        let normalized = gemini_to_response(&response, "gemini-test", &ResponseToolMap::default())
            .expect("legacy Gemini response");
        assert_eq!(
            normalized["output"][0]["provider_metadata"]["gemini"]["thought_signature"],
            "legacy-signature"
        );
    }

    #[test]
    fn streamed_gemini_tool_call_preserves_the_part_signature() {
        let chunk = gemini_stream_to_chat(&json!({
            "candidates": [{"content": {"parts": [{
                "thoughtSignature": "stream-signature",
                "functionCall": {"id": "call_stream", "name": "lookup", "args": {}}
            }]}}]
        }))
        .expect("Gemini stream")
        .expect("translated chunk");

        assert_eq!(
            chunk["choices"][0]["delta"]["tool_calls"][0]["codetas_provider_metadata"]
                ["gemini"]["thought_signature"],
            "stream-signature"
        );
    }
}
