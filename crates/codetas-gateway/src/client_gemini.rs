use crate::translate::tool_input_to_value;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use uuid::Uuid;

pub(crate) fn gemini_request_to_responses(
    value: &Value,
    model: &str,
    streaming: bool,
) -> Result<Value, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "Gemini request must be a JSON object".to_string())?;
    if model.trim().is_empty() || model.len() > 240 || model.chars().any(char::is_control) {
        return Err("Gemini request path requires a valid model".into());
    }
    let model = model.strip_prefix("models/").unwrap_or(model);
    let contents = object
        .get("contents")
        .and_then(Value::as_array)
        .ok_or("Gemini request requires contents")?;
    let mut input = Vec::new();
    let mut call_ids = HashMap::<String, VecDeque<String>>::new();
    for (content_index, content) in contents.iter().enumerate() {
        let role = content
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        if !matches!(role, "user" | "model") {
            return Err(format!("unsupported Gemini content role: {role}"));
        }
        let parts = content
            .get("parts")
            .and_then(Value::as_array)
            .ok_or("Gemini content requires parts")?;
        let mut message_parts = Vec::new();
        for (part_index, part) in parts.iter().enumerate() {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                message_parts.push(json!({
                    "type": if role == "model" { "output_text" } else { "input_text" },
                    "text": text,
                }));
                continue;
            }
            if let Some(inline) = part.get("inlineData").or_else(|| part.get("inline_data")) {
                if role == "model" {
                    return Err("Gemini assistant image history cannot be translated safely".into());
                }
                let mime = inline
                    .get("mimeType")
                    .or_else(|| inline.get("mime_type"))
                    .and_then(Value::as_str)
                    .filter(|value| valid_image_mime_type(value))
                    .ok_or("Gemini inlineData requires an image mimeType")?;
                let data = inline
                    .get("data")
                    .and_then(Value::as_str)
                    .ok_or("Gemini inlineData requires base64 data")?;
                if data.is_empty()
                    || data.len() > 24 * 1024 * 1024
                    || data.bytes().any(|byte| byte.is_ascii_whitespace())
                    || STANDARD.decode(data.as_bytes()).is_err()
                {
                    return Err(
                        "Gemini inline image exceeds the CODETAS limit or is malformed".into(),
                    );
                }
                message_parts.push(json!({
                    "type": "input_image",
                    "image_url": format!("data:{mime};base64,{data}"),
                }));
                continue;
            }
            if let Some(call) = part
                .get("functionCall")
                .or_else(|| part.get("function_call"))
            {
                if !message_parts.is_empty() {
                    input.push(message_item(role, std::mem::take(&mut message_parts)));
                }
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("Gemini functionCall requires a name")?;
                let call_id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        format!(
                            "call_gemini_{content_index}_{part_index}_{}",
                            Uuid::new_v4().simple()
                        )
                    });
                call_ids
                    .entry(name.to_string())
                    .or_default()
                    .push_back(call_id.clone());
                let mut item = json!({
                    "type": "function_call",
                    "call_id": call_id,
                    "name": name,
                    "arguments": call.get("args").cloned().unwrap_or_else(|| json!({})).to_string(),
                });
                if let Some(signature) = part
                    .get("thoughtSignature")
                    .or_else(|| part.get("thought_signature"))
                    .or_else(|| call.get("thoughtSignature"))
                    .or_else(|| call.get("thought_signature"))
                    .cloned()
                {
                    item["provider_metadata"] =
                        json!({"gemini": {"thought_signature": signature}});
                }
                input.push(item);
                continue;
            }
            if let Some(result) = part
                .get("functionResponse")
                .or_else(|| part.get("function_response"))
            {
                if !message_parts.is_empty() {
                    input.push(message_item(role, std::mem::take(&mut message_parts)));
                }
                let name = result
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("Gemini functionResponse requires a name")?;
                let call_id = result
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .or_else(|| call_ids.get_mut(name).and_then(VecDeque::pop_front))
                    .ok_or_else(|| {
                        format!("Gemini functionResponse has no matching call for {name}")
                    })?;
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": result.get("response").cloned().unwrap_or(Value::Null),
                }));
                continue;
            }
            return Err("unsupported Gemini content part".into());
        }
        if !message_parts.is_empty() {
            input.push(message_item(role, message_parts));
        }
    }
    if input.is_empty() {
        return Err("Gemini request contents are empty".into());
    }
    let mut response = Map::new();
    response.insert("model".into(), Value::String(model.to_string()));
    response.insert("input".into(), Value::Array(input));
    response.insert("stream".into(), Value::Bool(streaming));
    if let Some(system) = object
        .get("systemInstruction")
        .or_else(|| object.get("system_instruction"))
    {
        let text = system
            .get("parts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n");
        if !text.is_empty() {
            response.insert("instructions".into(), Value::String(text));
        }
    }
    if let Some(tools) = gemini_tools(object.get("tools"))? {
        response.insert("tools".into(), tools);
    }
    if let Some(choice) = gemini_tool_choice(
        object
            .get("toolConfig")
            .or_else(|| object.get("tool_config")),
    )? {
        response.insert("tool_choice".into(), choice);
    }
    if let Some(config) = object
        .get("generationConfig")
        .or_else(|| object.get("generation_config"))
        .and_then(Value::as_object)
    {
        for (source, target) in [
            ("maxOutputTokens", "max_output_tokens"),
            ("temperature", "temperature"),
            ("topP", "top_p"),
        ] {
            if let Some(value) = config.get(source) {
                response.insert(target.into(), value.clone());
            }
        }
        if let Some(stops) = config.get("stopSequences") {
            response.insert("stop".into(), stops.clone());
        }
        if let Some(effort) = config
            .get("thinkingConfig")
            .or_else(|| config.get("thinking_config"))
            .and_then(|value| {
                value
                    .get("thinkingLevel")
                    .or_else(|| value.get("thinking_level"))
            })
            .and_then(Value::as_str)
        {
            response.insert(
                "reasoning".into(),
                json!({"effort": effort.to_ascii_lowercase()}),
            );
        }
    }
    Ok(Value::Object(response))
}

fn valid_image_mime_type(value: &str) -> bool {
    let Some(subtype) = value.strip_prefix("image/") else {
        return false;
    };
    !subtype.is_empty()
        && value.len() <= 128
        && subtype
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-'))
}

fn message_item(role: &str, content: Vec<Value>) -> Value {
    json!({
        "type": "message",
        "role": if role == "model" { "assistant" } else { "user" },
        "content": content,
    })
}

fn gemini_tools(value: Option<&Value>) -> Result<Option<Value>, String> {
    let Some(groups) = value.and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut tools = Vec::new();
    for group in groups {
        let declarations = group
            .get("functionDeclarations")
            .or_else(|| group.get("function_declarations"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for declaration in declarations {
            tools.push(json!({
                "type": "function",
                "name": declaration.get("name").and_then(Value::as_str).ok_or("Gemini function declaration requires a name")?,
                "description": declaration.get("description").cloned().unwrap_or(Value::Null),
                "parameters": declaration.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"})),
            }));
        }
    }
    Ok((!tools.is_empty()).then_some(Value::Array(tools)))
}

fn gemini_tool_choice(value: Option<&Value>) -> Result<Option<Value>, String> {
    let Some(config) = value.and_then(|value| {
        value
            .get("functionCallingConfig")
            .or_else(|| value.get("function_calling_config"))
    }) else {
        return Ok(None);
    };
    let mode = config
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("AUTO")
        .to_ascii_uppercase();
    let allowed = config
        .get("allowedFunctionNames")
        .or_else(|| config.get("allowed_function_names"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    match mode.as_str() {
        "AUTO" => Ok(Some(Value::String("auto".into()))),
        "NONE" => Ok(Some(Value::String("none".into()))),
        "ANY" if allowed.len() == 1 => Ok(Some(json!({
            "type": "function",
            "name": allowed[0].as_str().ok_or("Gemini allowedFunctionNames must contain strings")?,
        }))),
        "ANY" => Ok(Some(Value::String("required".into()))),
        _ => Err(format!("unsupported Gemini function calling mode: {mode}")),
    }
}

pub(crate) fn responses_to_gemini_response(value: &Value) -> Result<Value, String> {
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .ok_or("Responses result has no output array")?;
    let mut parts = Vec::new();
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for content in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(text) = content.get("text").and_then(Value::as_str) {
                        parts.push(json!({"text": text}));
                    }
                }
            }
            Some("function_call" | "custom_tool_call") => {
                // custom_tool_call items carry the payload in `input`
                // while function_call uses `arguments`. A structured
                // payload is preserved as-is instead of collapsing to {}.
                let arguments = item
                    .get("input")
                    .or_else(|| item.get("arguments"))
                    .map(tool_input_to_value)
                    .unwrap_or_else(|| json!({}));
                let mut part = json!({
                    "functionCall": {
                        "id": item.get("call_id").cloned().unwrap_or(Value::Null),
                        "name": item.get("name").and_then(Value::as_str).ok_or("Responses function call requires a name")?,
                        "args": arguments,
                    }
                });
                if let Some(signature) = item
                    .pointer("/provider_metadata/gemini/thought_signature")
                    .cloned()
                {
                    part["thoughtSignature"] = signature;
                }
                parts.push(part);
            }
            Some("reasoning") => {}
            Some(_) | None => {}
        }
    }
    let usage = value.get("usage").cloned().unwrap_or_else(|| json!({}));
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    let incomplete_reason = value
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str);
    let finish_reason = match (status, incomplete_reason) {
        ("incomplete", Some("max_output_tokens" | "max_tokens" | "length")) => "MAX_TOKENS",
        (
            "incomplete",
            Some("content_filter" | "safety" | "blocked" | "prohibited_content"),
        ) => "SAFETY",
        ("incomplete", _) => "OTHER",
        ("failed", _) => "OTHER",
        _ => "STOP",
    };
    Ok(json!({
        "candidates": [{
            "index": 0,
            "content": {"role": "model", "parts": parts},
            "finishReason": finish_reason,
        }],
        "usageMetadata": {
            "promptTokenCount": usage.get("input_tokens").cloned().unwrap_or(json!(0)),
            "candidatesTokenCount": usage
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .saturating_sub(
                    usage.pointer("/output_tokens_details/reasoning_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                ),
            "totalTokenCount": usage.get("total_tokens").cloned().unwrap_or(json!(0)),
            "cachedContentTokenCount": usage.pointer("/input_tokens_details/cached_tokens").cloned().unwrap_or(json!(0)),
        },
        "modelVersion": value.get("model").cloned().unwrap_or(Value::Null),
        "responseId": value.get("id").cloned().unwrap_or(Value::Null),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_gemini_function_signature_round_trips_at_part_level() {
        let request = json!({
            "contents": [{"role": "model", "parts": [{
                "thoughtSignature": "signed-part",
                "functionCall": {"id": "call_1", "name": "lookup", "args": {"q": "x"}}
            }]}]
        });
        let translated = gemini_request_to_responses(&request, "gemini-test", false)
            .expect("translate request");
        let item = &translated["input"][0];
        assert_eq!(
            item.pointer("/provider_metadata/gemini/thought_signature"),
            Some(&json!("signed-part"))
        );

        let response = responses_to_gemini_response(&json!({
            "id": "resp_gateway",
            "model": "gemini-test",
            "status": "completed",
            "output": [item],
            "usage": {}
        }))
        .expect("translate response");
        let part = &response["candidates"][0]["content"]["parts"][0];
        assert_eq!(part["thoughtSignature"], "signed-part");
        assert!(part["functionCall"].get("thoughtSignature").is_none());
    }

    #[test]
    fn public_gemini_reads_legacy_nested_signature_but_writes_canonical_part() {
        let request = json!({
            "contents": [{"role": "model", "parts": [{
                "functionCall": {
                    "id": "call_legacy",
                    "name": "lookup",
                    "args": {},
                    "thoughtSignature": "legacy"
                }
            }]}]
        });
        let translated = gemini_request_to_responses(&request, "gemini-test", true)
            .expect("translate legacy request");
        assert_eq!(
            translated["input"][0]["provider_metadata"]["gemini"]["thought_signature"],
            "legacy"
        );
    }

    #[test]
    fn public_gemini_maps_incomplete_reason_to_finish_reason() {
        for (reason, expected) in [
            ("max_output_tokens", "MAX_TOKENS"),
            ("content_filter", "SAFETY"),
            ("safety", "SAFETY"),
            ("provider_limit", "OTHER"),
        ] {
            let response = responses_to_gemini_response(&json!({
                "status": "incomplete",
                "incomplete_details": {"reason": reason},
                "output": [],
                "usage": {}
            }))
            .expect("Gemini response");
            assert_eq!(response["candidates"][0]["finishReason"], expected);
        }
    }
}
