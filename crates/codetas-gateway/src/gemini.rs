use crate::compaction::decode_summary;
use crate::translate::{
    arguments_update_same_call, custom_call_is_deliverable, default_tool_search_parameters,
    insert_tool_namespace, response_allowed_tool_wire_names, response_tool_map,
    response_tool_parameters, tool_input_to_value, tool_search_output_to_text,
    unwrap_custom_tool_arguments, ResponseToolKind, ResponseToolMap, TurnEnd,
    UndeliveredCustomCalls,
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

/// Gemini omits `args` for a call without parameters but Codex only accepts a
/// JSON object. `null` (or a missing field) becomes `{}` so the tool itself
/// rejects bad input instead of the gateway aborting the whole turn with
/// `invalid_tool_call`.
fn gemini_function_arguments(call: &Value) -> String {
    match call.get("args") {
        None | Some(Value::Null) => "{}".into(),
        Some(Value::String(raw)) if serde_json::from_str::<Value>(raw).is_ok() => raw.clone(),
        Some(args) => serde_json::to_string(args).unwrap_or_else(|_| "{}".into()),
    }
}

pub fn responses_to_gemini(body: &Value, _model: &str) -> Result<Value, String> {
    crate::translate::validate_translated_image_references(body)?;
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
                        let thought_signature = Value::Object(item.clone())
                            .pointer("/provider_metadata/gemini/thought_signature")
                            .cloned()
                            .or_else(|| {
                                Value::Object(item.clone())
                                    .pointer("/codetas_provider_metadata/gemini/thought_signature")
                                    .cloned()
                            });
                        let function_call = json!({"name": wire_name, "args": args, "id": call_id});
                        if let Some(signature) = thought_signature.filter(|value| {
                            value.as_str().is_some_and(|text| !text.trim().is_empty())
                                || value.is_object()
                                || value.is_array()
                        }) {
                            let mut part = json!({"functionCall": function_call});
                            // Gemini signs the entire content part, not the
                            // nested functionCall object.
                            part["thoughtSignature"] = signature;
                            contents.push(json!({
                                "role": "model",
                                "parts": [part]
                            }));
                        } else {
                            contents.push(json!({
                                "role": "user",
                                "parts": [{
                                    "text": format!(
                                        "[prior tool call {} {}]",
                                        wire_name,
                                        serde_json::to_string(&args).unwrap_or_else(|_| "{}".into())
                                    )
                                }]
                            }));
                        }
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
                            "role": "user",
                            "parts": [{
                                "text": format!(
                                    "[prior tool search {} {}]",
                                    wire_name,
                                    serde_json::to_string(&args).unwrap_or_else(|_| "{}".into())
                                )
                            }]
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
    let finish_reason = candidate.get("finishReason").and_then(Value::as_str);
    let disposition = gemini_finish_disposition(finish_reason);
    let end = match disposition {
        GeminiFinishDisposition::Stop if finish_reason.is_some() => TurnEnd::ToolCalls,
        GeminiFinishDisposition::Stop => TurnEnd::Normal,
        GeminiFinishDisposition::MaxTokens | GeminiFinishDisposition::ContentFilter => {
            TurnEnd::CutOff
        }
        GeminiFinishDisposition::MalformedFunctionCall
        | GeminiFinishDisposition::ProviderFailure => TurnEnd::Failed,
    };
    let mut text = String::new();
    let mut output = Vec::new();
    let mut undelivered = UndeliveredCustomCalls::default();
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
            let arguments = gemini_function_arguments(call);
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
                    if !custom_call_is_deliverable(name, &arguments, end) {
                        undelivered.note(name, &arguments);
                        continue;
                    }
                    let mut item = json!({
                        "id": format!("ctc_{}", Uuid::new_v4().simple()),
                        "type": "custom_tool_call",
                        "status": "completed",
                        "call_id": call_id,
                        "name": name,
                        "input": unwrap_custom_tool_arguments(name, &arguments)
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
    if disposition == GeminiFinishDisposition::MalformedFunctionCall {
        return Err("Gemini returned a malformed function call".into());
    }
    let cut_off = end == TurnEnd::CutOff;
    let failed = disposition == GeminiFinishDisposition::ProviderFailure;
    let invalid_turn = !cut_off && !failed && undelivered.invalidate_turn(&output, &text);
    Ok(response_object(
        exposed_model,
        if failed {
            "failed"
        } else if cut_off || invalid_turn {
            "incomplete"
        } else {
            "completed"
        },
        output,
        gemini_usage(value.get("usageMetadata")),
        if invalid_turn {
            Some(json!({"reason": "invalid_tool_call"}))
        } else {
            cut_off.then(|| {
                json!({"reason": finish_reason.unwrap_or("provider_limit").to_ascii_lowercase()})
            })
        },
    ))
}

/// The tool calls seen on one stream. Gemini sends whole `functionCall` parts, may
/// put each parallel call in its own chunk, and may repeat a call on a later chunk,
/// so its part positions cannot serve as the Chat tool index.
#[derive(Default)]
pub struct GeminiStreamState {
    calls: Vec<GeminiStreamCall>,
    /// `calls` before this index came from earlier chunks. Only those can be repeated:
    /// two parts of one chunk are two calls, even when they are identical.
    earlier_chunks: usize,
}

struct GeminiStreamCall {
    id: String,
    id_from_provider: bool,
    name: String,
    args: Value,
    signed: bool,
}

impl GeminiStreamState {
    /// The Chat delta for one `functionCall` part, or `None` for a repeat that adds
    /// nothing. Executing a repeat would run the same call twice.
    fn tool_call_delta(&mut self, call: &Value, signature: Option<&Value>) -> Option<Value> {
        let name = call
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let provider_id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        let arguments = gemini_function_arguments(call);
        let args = serde_json::from_str::<Value>(&arguments).unwrap_or(Value::Null);

        let same_id = provider_id.and_then(|id| {
            self.calls
                .iter()
                .position(|known| known.id_from_provider && known.id == id)
        });
        // Two different provider ids are two calls, whatever their arguments.
        let may_match = |known: &GeminiStreamCall| {
            known.name == name && (provider_id.is_none() || !known.id_from_provider)
        };
        let is_echo = args.as_object().is_some_and(Map::is_empty);
        let earlier = &self.calls[..self.earlier_chunks];
        let repeated = match same_id {
            Some(index) => (self.calls[index].args == args || is_echo).then_some(index),
            None => earlier
                .iter()
                .rposition(|known| may_match(known) && (known.args == args || is_echo)),
        };
        if let Some(index) = repeated {
            let known = &mut self.calls[index];
            if known.signed || signature.is_none() {
                return None;
            }
            // Replaying the call needs a signature that first arrived on the repeat.
            known.signed = true;
            return Some(gemini_tool_call_delta(
                index,
                &known.id,
                &known.name,
                None,
                signature,
            ));
        }
        let updated = same_id.or_else(|| {
            let earlier = &self.calls[..self.earlier_chunks];
            earlier.iter().rposition(|known| {
                may_match(known) && arguments_update_same_call(&known.args, &args)
            })
        });
        if let Some(index) = updated {
            let known = &mut self.calls[index];
            known.args = args;
            known.signed |= signature.is_some();
            return Some(gemini_tool_call_delta(
                index,
                &known.id,
                &known.name,
                Some(&arguments),
                signature,
            ));
        }
        let index = self.calls.len();
        let id = provider_id
            .map(str::to_string)
            .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
        let delta = gemini_tool_call_delta(index, &id, name, Some(&arguments), signature);
        self.calls.push(GeminiStreamCall {
            id,
            id_from_provider: provider_id.is_some(),
            name: name.to_string(),
            args,
            signed: signature.is_some(),
        });
        Some(delta)
    }
}

fn gemini_tool_call_delta(
    index: usize,
    id: &str,
    name: &str,
    arguments: Option<&str>,
    signature: Option<&Value>,
) -> Value {
    let mut function = json!({"name": name});
    if let Some(arguments) = arguments {
        function["arguments"] = json!(arguments);
    }
    let mut delta = json!({
        "index": index,
        "id": id,
        "type": "function",
        "function": function
    });
    if let Some(signature) = signature {
        delta["codetas_provider_metadata"] = json!({"gemini": {"thought_signature": signature}});
    }
    delta
}

pub fn gemini_stream_to_chat(
    value: &Value,
    state: &mut GeminiStreamState,
) -> Result<Option<Value>, String> {
    let payload = value
        .get("response")
        .filter(|value| value.is_object())
        .unwrap_or(value);
    let blocked = payload
        .pointer("/promptFeedback/blockReason")
        .and_then(Value::as_str)
        .is_some_and(|reason| !matches!(reason, "" | "BLOCK_REASON_UNSPECIFIED"));
    if !blocked
        && payload.get("error").is_none_or(Value::is_null)
        && value.get("error").is_none_or(Value::is_null)
        && payload.get("usageMetadata").is_some_and(Value::is_object)
        && payload
            .get("candidates")
            .and_then(Value::as_array)
            .is_none_or(|candidates| candidates.is_empty())
    {
        let usage = crate::observability::TokenUsage::from_json(value);
        return Ok(Some(
            json!({"choices":[],"usage":{"prompt_tokens":usage.input_tokens,"completion_tokens":usage.output_tokens,"total_tokens":usage.total_tokens,
            "prompt_tokens_details":if usage.cache_read_reported {json!({"cached_tokens":usage.cached_input_tokens})} else {json!({})},"completion_tokens_details":{"reasoning_tokens":usage.reasoning_tokens}}}),
        ));
    }
    gemini_stream_to_chat_inner(value, state)
}

fn gemini_stream_to_chat_inner(
    value: &Value,
    state: &mut GeminiStreamState,
) -> Result<Option<Value>, String> {
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
    state.earlier_chunks = state.calls.len();
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
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if part.get("thought").and_then(Value::as_bool) == Some(true) {
                    reasoning_content.push_str(text);
                } else {
                    content.push_str(text);
                }
            }
            if let Some(call) = part.get("functionCall") {
                let signature = part
                    .get("thoughtSignature")
                    .or_else(|| call.get("thoughtSignature"));
                tool_calls.extend(state.tool_call_delta(call, signature));
            }
        }
    }
    let usage = value.get("usageMetadata").map(|usage| {
        let prompt = usage
            .get("promptTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let visible = usage
            .get("candidatesTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let reasoning = usage
            .get("thoughtsTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        json!({
            "prompt_tokens": prompt,
            "completion_tokens": visible.saturating_add(reasoning),
            "total_tokens": usage.get("totalTokenCount").and_then(Value::as_u64).unwrap_or(0),
            "prompt_tokens_details": usage.get("cachedContentTokenCount").and_then(Value::as_u64)
                .map(|n| json!({"cached_tokens": n})).unwrap_or_else(|| json!({})),
            "completion_tokens_details": {"reasoning_tokens": reasoning}
        })
    });
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
        GeminiFinishDisposition::Stop => {
            // Gemini ends tool turns with STOP, and the call may have arrived on an
            // earlier chunk, while the translator only treats `tool_calls` as the
            // authoritative end of a custom tool's input.
            if finish_reason.is_some() {
                chunk["choices"][0]["finish_reason"] = json!(if state.calls.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                });
            }
        }
        GeminiFinishDisposition::MaxTokens => {
            chunk["choices"][0]["finish_reason"] = json!("length")
        }
        GeminiFinishDisposition::ContentFilter => {
            chunk["choices"][0]["finish_reason"] = json!("content_filter")
        }
        GeminiFinishDisposition::MalformedFunctionCall => {
            return Err("Gemini returned a malformed function call".into())
        }
        GeminiFinishDisposition::ProviderFailure => {
            return Err(format!(
                "Gemini stopped with {}",
                finish_reason.unwrap_or("UNKNOWN")
            ))
        }
    }
    if !tool_calls.is_empty()
        && matches!(
            gemini_finish_disposition(finish_reason),
            GeminiFinishDisposition::MaxTokens
        )
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
        ResponseToolKind::ToolSearch => {
            response_tool_parameters(tool).unwrap_or_else(default_tool_search_parameters)
        }
        ResponseToolKind::Function => {
            response_tool_parameters(tool).unwrap_or_else(|| json!({"type": "object"}))
        }
    };
    Ok(json!({
        "name": wire_name,
        "description": tool.get("description").and_then(Value::as_str).unwrap_or_default(),
        "parameters": sanitize_gemini_schema(parameters)
    }))
}

fn sanitize_gemini_schema(value: Value) -> Value {
    const REJECTED: &[&str] = &[
        "additionalProperties",
        "$schema",
        "$id",
        "$ref",
        "$defs",
        "definitions",
        "encrypted",
        "patternProperties",
        "propertyNames",
        "dependentSchemas",
        "dependentRequired",
        "unevaluatedProperties",
        "unevaluatedItems",
        "if",
        "then",
        "else",
        "not",
        "contentEncoding",
        "contentMediaType",
    ];
    fn sanitize_map(value: Value) -> Value {
        match value {
            Value::Object(values) => Value::Object(
                values
                    .into_iter()
                    .map(|(name, schema)| (name, sanitize_gemini_schema(schema)))
                    .collect(),
            ),
            other => sanitize_gemini_schema(other),
        }
    }
    match value {
        Value::Array(values) => {
            Value::Array(values.into_iter().map(sanitize_gemini_schema).collect())
        }
        Value::Object(values) => {
            let mut object = values
                .into_iter()
                .filter(|(key, value)| {
                    !REJECTED.contains(&key.as_str())
                        && !(key == "required" && value.as_array().is_some_and(Vec::is_empty))
                })
                .map(|(key, value)| {
                    let value = if matches!(key.as_str(), "properties" | "$defs" | "definitions") {
                        sanitize_map(value)
                    } else {
                        sanitize_gemini_schema(value)
                    };
                    (key, value)
                })
                .collect::<Map<_, _>>();
            match object.get("type") {
                Some(Value::String(kind))
                    if matches!(
                        kind.as_str(),
                        "object" | "array" | "string" | "number" | "integer" | "boolean"
                    ) => {}
                Some(Value::Array(kinds)) => {
                    let first = kinds.iter().find_map(Value::as_str).and_then(|kind| {
                        matches!(
                            kind,
                            "object" | "array" | "string" | "number" | "integer" | "boolean"
                        )
                        .then_some(kind.to_string())
                    });
                    match first {
                        Some(kind) => {
                            object.insert("type".into(), Value::String(kind));
                        }
                        None => {
                            object.remove("type");
                        }
                    }
                }
                Some(_) => {
                    object.remove("type");
                }
                None => {}
            }
            if let Some(Value::Array(values)) = object.get("enum").cloned() {
                let strings = values
                    .into_iter()
                    .filter_map(|item| match item {
                        Value::String(text) => Some(Value::String(text)),
                        Value::Number(number) => Some(Value::String(number.to_string())),
                        Value::Bool(flag) => Some(Value::String(flag.to_string())),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if strings.is_empty() {
                    object.remove("enum");
                } else {
                    if object.get("type").and_then(Value::as_str) != Some("string") {
                        object.insert("type".into(), Value::String("string".into()));
                    }
                    object.insert("enum".into(), Value::Array(strings));
                }
            }
            Value::Object(object)
        }
        value => value,
    }
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
        None if matches!(
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
            let allowed = response_allowed_tool_wire_names(choice, tool_map).unwrap_or_default();
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
    if ends_with_model_turn(&merged) {
        merged.push(json!({
            "role": "user",
            "parts": [{"text": "Continue."}]
        }));
    }
    merged
}

fn ends_with_model_turn(contents: &[Value]) -> bool {
    contents
        .last()
        .and_then(|content| content.get("role"))
        .and_then(Value::as_str)
        == Some("model")
}

fn ends_with_unanswered_model_function_call(contents: &[Value]) -> bool {
    contents.last().is_some_and(|content| {
        content.get("role").and_then(Value::as_str) == Some("model")
            && content
                .get("parts")
                .and_then(Value::as_array)
                .is_some_and(|parts| {
                    parts.iter().any(|part| part.get("functionCall").is_some())
                        && !parts
                            .iter()
                            .any(|part| part.get("functionResponse").is_some())
                })
    })
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
    let mut converted = json!({
        "input_tokens": input,
        "output_tokens": output + reasoning,
        "output_tokens_details": {"reasoning_tokens": reasoning},
        "total_tokens": total
    });
    crate::translate::set_cached_input_tokens(
        &mut converted,
        usage
            .and_then(|value| value.get("cachedContentTokenCount"))
            .and_then(Value::as_u64),
    );
    converted
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
    fn strips_unknown_gemini_tool_schema_fields() {
        let request = json!({
            "input": "hello",
            "tools": [{
                "type": "function",
                "name": "read_file",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "encrypted": true
                        }
                    },
                    "additionalProperties": false
                }
            }]
        });
        let translated =
            responses_to_gemini(&request, "gemini-test").expect("request should translate");
        let parameters = &translated["tools"][0]["functionDeclarations"][0]["parameters"];
        assert_eq!(parameters["properties"]["path"]["type"], "string");
        assert!(parameters["properties"]["path"].get("encrypted").is_none());
        assert!(parameters.get("additionalProperties").is_none());
    }

    #[test]
    fn stringifies_numeric_gemini_enums() {
        let request = json!({
            "input": "hello",
            "tools": [{
                "type": "function",
                "name": "apply_patch",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "mode": {
                            "enum": [1, "replace"]
                        }
                    }
                }
            }]
        });
        let translated =
            responses_to_gemini(&request, "gemini-test").expect("request should translate");
        let mode =
            &translated["tools"][0]["functionDeclarations"][0]["parameters"]["properties"]["mode"];
        assert_eq!(mode["type"], "string");
        assert_eq!(mode["enum"], json!(["1", "replace"]));
    }

    #[test]
    fn carries_tool_result_images_beside_the_gemini_function_response() {
        let request = json!({
            "input": [
                {"type": "custom_tool_call", "call_id": "call_1", "name": "view_image", "input": "{}", "provider_metadata": {"gemini": {"thought_signature": "sig"}}},
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
        let translated =
            responses_to_gemini(&request, "gemini-test").expect("request should translate");
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
                {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{}", "provider_metadata": {"gemini": {"thought_signature": "sig"}}},
                {"type": "function_call_output", "call_id": "call_1", "output": {"count": 2, "ok": true}}
            ]
        });
        let translated =
            responses_to_gemini(&request, "gemini-test").expect("request should translate");
        assert_eq!(
            translated["contents"][1]["parts"][0]["functionResponse"]["response"]["output"],
            json!({"count": 2, "ok": true})
        );
    }

    #[test]
    fn preserves_structured_array_tool_result_output() {
        let request = json!({
            "input": [
                {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{}", "provider_metadata": {"gemini": {"thought_signature": "sig"}}},
                {"type": "function_call_output", "call_id": "call_1", "output": [{"type": "text", "text": "record", "id": 1}, {"id": 2}]}
            ]
        });
        let translated =
            responses_to_gemini(&request, "gemini-test").expect("request should translate");
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
                {"type": "custom_tool_call", "call_id": "call_1", "name": "exec", "input": {"command": "ls"}, "provider_metadata": {"gemini": {"thought_signature": "sig"}}}
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

        let translated = responses_to_gemini(&request, "gemini-test").expect("tool search request");
        // The prior tool_search call is replayed as text (it has no thought
        // signature); the result stays a functionResponse in the same turn.
        assert!(translated["contents"][0]["parts"][0]["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("[prior tool search tool_search ")));
        assert_eq!(
            translated["contents"][0]["parts"][1]["functionResponse"]["name"],
            "tool_search"
        );
        assert!(
            translated["contents"][0]["parts"][1]["functionResponse"]["response"]["output"]
                .as_str()
                .is_some_and(|text| text.contains("calendar__create_event"))
        );
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
    fn appends_a_user_turn_when_history_ends_on_a_function_call() {
        let translated = responses_to_gemini(
            &json!({
                "input": [
                    {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{}"}
                ]
            }),
            "gemini-test",
        )
        .expect("request should translate");
        assert_eq!(translated["contents"].as_array().map(Vec::len), Some(1));
        assert_eq!(translated["contents"][0]["role"], "user");
        assert!(translated["contents"][0]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("lookup"));
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
    fn streamed_gemini_call_without_arguments_becomes_an_empty_object() {
        let chunk = gemini_stream_to_chat(
            &json!({
                "candidates": [{
                    "content": {"parts": [{
                        "functionCall": {"id": "call_empty", "name": "read_thread"}
                    }]},
                    "finishReason": "STOP"
                }]
            }),
            &mut GeminiStreamState::default(),
        )
        .expect("Gemini stream")
        .expect("translated chunk");

        assert_eq!(chunk["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{}"
        );
    }

    #[test]
    fn gemini_call_without_arguments_is_completed_with_empty_object() {
        let normalized = gemini_to_response(
            &json!({
                "candidates": [{
                    "content": {"parts": [{
                        "functionCall": {"id": "call_empty", "name": "read_thread"}
                    }]},
                    "finishReason": "STOP"
                }]
            }),
            "gemini-test",
            &ResponseToolMap::default(),
        )
        .expect("Gemini response");

        assert_eq!(normalized["status"], "completed");
        assert_eq!(normalized["output"][0]["arguments"], "{}");
    }

    #[test]
    fn streamed_gemini_tool_call_preserves_the_part_signature() {
        let chunk = gemini_stream_to_chat(
            &json!({
                "candidates": [{"content": {"parts": [{
                    "thoughtSignature": "stream-signature",
                    "functionCall": {"id": "call_stream", "name": "lookup", "args": {}}
                }]}}]
            }),
            &mut GeminiStreamState::default(),
        )
        .expect("Gemini stream")
        .expect("translated chunk");

        assert_eq!(
            chunk["choices"][0]["delta"]["tool_calls"][0]["codetas_provider_metadata"]["gemini"]
                ["thought_signature"],
            "stream-signature"
        );
    }

    fn finish_gemini_stream(chunks: &[Value]) -> Value {
        let mut gemini = GeminiStreamState::default();
        let (mut state, _) = crate::translate::ChatStreamState::new(
            "gemini-test".into(),
            ResponseToolMap::default(),
        );
        for chunk in chunks {
            if let Some(chat) = gemini_stream_to_chat(chunk, &mut gemini).expect("Gemini stream") {
                state.push_chat_chunk(&chat);
            }
        }
        state.finish_with_response().1
    }

    fn call_chunk(call: Value, finish: bool) -> Value {
        let mut chunk = json!({"candidates": [{"content": {"parts": [{"functionCall": call}]}}]});
        if finish {
            chunk["candidates"][0]["finishReason"] = json!("STOP");
        }
        chunk
    }

    #[test]
    fn gemini_calls_in_separate_chunks_stay_separate() {
        let response = finish_gemini_stream(&[
            call_chunk(json!({"name": "read_file", "args": {"path": "a"}}), false),
            call_chunk(
                json!({"name": "exec_command", "args": {"cmd": "make test"}}),
                true,
            ),
        ]);
        let output = response["output"].as_array().expect("output");
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["name"], "read_file");
        assert_eq!(output[0]["arguments"], "{\"path\":\"a\"}");
        assert_eq!(output[1]["name"], "exec_command");
        assert_eq!(output[1]["arguments"], "{\"cmd\":\"make test\"}");
        assert_ne!(output[0]["call_id"], output[1]["call_id"]);
    }

    #[test]
    fn gemini_second_call_to_a_tool_is_kept_when_shorter_or_equal_in_length() {
        for (first, second) in [
            (json!({"path": "a_much_longer_path"}), json!({"path": "b"})),
            (json!({"path": "aa"}), json!({"path": "bb"})),
        ] {
            let response = finish_gemini_stream(&[
                call_chunk(json!({"name": "read_file", "args": first.clone()}), false),
                call_chunk(json!({"name": "read_file", "args": second.clone()}), true),
            ]);
            let output = response["output"].as_array().expect("output");
            assert_eq!(output.len(), 2, "{first} then {second}");
            assert_eq!(output[0]["arguments"], first.to_string());
            assert_eq!(output[1]["arguments"], second.to_string());
        }
    }

    #[test]
    fn gemini_repeat_of_a_call_is_dropped_wherever_it_appears() {
        let first = call_chunk(json!({"name": "read_file", "args": {"path": "a"}}), false);
        for repeat in [
            // The same part position, and behind a thought part.
            call_chunk(json!({"name": "read_file", "args": {"path": "a"}}), true),
            json!({"candidates": [{"content": {"parts": [
                {"thought": true, "text": "again"},
                {"functionCall": {"name": "read_file", "args": {"path": "a"}}}
            ]}, "finishReason": "STOP"}]}),
            // Echoes that carry no arguments.
            call_chunk(json!({"name": "read_file"}), true),
            call_chunk(json!({"name": "read_file", "args": {}}), true),
        ] {
            let response = finish_gemini_stream(&[first.clone(), repeat.clone()]);
            let calls = response["output"]
                .as_array()
                .expect("output")
                .iter()
                .filter(|item| item["type"] == "function_call")
                .collect::<Vec<_>>();
            assert_eq!(calls.len(), 1, "{repeat}");
            assert_eq!(calls[0]["arguments"], "{\"path\":\"a\"}");
        }
    }

    #[test]
    fn gemini_object_that_keeps_every_key_and_adds_more_updates_the_call() {
        let response = finish_gemini_stream(&[
            call_chunk(json!({"name": "read_file", "args": {"path": "a"}}), false),
            call_chunk(
                json!({"name": "read_file", "args": {"path": "a", "limit": 10}}),
                true,
            ),
        ]);
        let output = response["output"].as_array().expect("output");
        assert_eq!(output.len(), 1);
        let arguments: Value =
            serde_json::from_str(output[0]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(arguments, json!({"path": "a", "limit": 10}));
    }

    #[test]
    fn gemini_identical_arguments_under_different_ids_are_two_calls() {
        let response = finish_gemini_stream(&[
            call_chunk(
                json!({"id": "call_a", "name": "read_file", "args": {"path": "a"}}),
                false,
            ),
            call_chunk(
                json!({"id": "call_b", "name": "read_file", "args": {"path": "a"}}),
                true,
            ),
        ]);
        let output = response["output"].as_array().expect("output");
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["call_id"], "call_a");
        assert_eq!(output[1]["call_id"], "call_b");
    }

    #[test]
    fn gemini_repeat_keeps_the_first_id_and_a_signature_that_arrives_late() {
        let response = finish_gemini_stream(&[
            call_chunk(
                json!({"id": "call_a", "name": "read_file", "args": {"path": "a"}}),
                false,
            ),
            json!({"candidates": [{"content": {"parts": [{
                "thoughtSignature": "late-signature",
                "functionCall": {"id": "call_a", "name": "read_file", "args": {"path": "a"}}
            }]}, "finishReason": "STOP"}]}),
        ]);
        let output = response["output"].as_array().expect("output");
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["call_id"], "call_a");
        assert_eq!(
            output[0]["provider_metadata"]["gemini"]["thought_signature"],
            "late-signature"
        );
    }

    #[test]
    fn gemini_calls_in_one_chunk_are_never_merged_or_dropped() {
        for (first, second) in [
            (json!({"path": "a"}), json!({"path": "a"})),
            (json!({"path": "a"}), json!({"path": "a", "limit": 10})),
            (json!({"path": "a"}), json!({})),
            (json!({}), json!({})),
        ] {
            let response = finish_gemini_stream(&[json!({"candidates": [{"content": {"parts": [
                {"functionCall": {"name": "read_file", "args": first.clone()}},
                {"functionCall": {"name": "read_file", "args": second.clone()}}
            ]}, "finishReason": "STOP"}]})]);
            let output = response["output"].as_array().expect("output");
            assert_eq!(output.len(), 2, "{first} then {second}");
            assert_eq!(output[0]["arguments"], first.to_string());
            assert_eq!(output[1]["arguments"], second.to_string());
            assert_ne!(output[0]["call_id"], output[1]["call_id"]);
        }

        // The repeat on a later chunk is still recognised, for both of them.
        let parallel = json!({"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "read_file", "args": {"path": "a"}}},
            {"functionCall": {"name": "read_file", "args": {"path": "a"}}}
        ]}}]});
        let mut repeat = parallel.clone();
        repeat["candidates"][0]["finishReason"] = json!("STOP");
        let response = finish_gemini_stream(&[parallel, repeat]);
        assert_eq!(response["output"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn gemini_empty_arguments_are_not_the_base_of_an_update() {
        let response = finish_gemini_stream(&[
            call_chunk(json!({"name": "list_dir", "args": {}}), false),
            call_chunk(json!({"name": "list_dir", "args": {"path": "src"}}), true),
        ]);
        let output = response["output"].as_array().expect("output");
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["arguments"], "{}");
        assert_eq!(output[1]["arguments"], "{\"path\":\"src\"}");
    }

    #[test]
    fn nonstream_gemini_custom_call_follows_the_stream_rule() {
        let tool_map = crate::translate::response_tool_map(&json!({"tools": [
            {"type": "custom", "name": "exec"},
            {"type": "custom", "name": "apply_patch"},
            {"type": "function", "name": "lookup", "parameters": {"type": "object"}}
        ]}));
        let patch = "*** Begin Patch\n*** Add File: a\n+ok\n*** End Patch\n";
        let call = |name: &str, args: Option<Value>| {
            let mut call = json!({"id": format!("call_{name}"), "name": name});
            if let Some(args) = args {
                call["args"] = args;
            }
            json!({"functionCall": call})
        };
        let respond = |parts: Vec<Value>, finish: &str| {
            gemini_to_response(
                &json!({"candidates": [{"content": {"parts": parts}, "finishReason": finish}]}),
                "gemini-test",
                &tool_map,
            )
            .expect("Gemini response")
        };
        let custom_calls = |response: &Value| {
            response["output"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["type"] == "custom_tool_call")
                .count()
        };

        let whole = respond(vec![call("exec", Some(json!({"input": "pwd"})))], "STOP");
        assert_eq!(whole["status"], "completed");
        assert_eq!(whole["output"][0]["status"], "completed");
        assert_eq!(whole["output"][0]["input"], "pwd");

        for empty in [
            call("exec", None),
            call("exec", Some(json!({}))),
            call("exec", Some(json!({"input": ""}))),
            call("apply_patch", None),
            call("apply_patch", Some(json!({"input": "*** Begin Patch\n+o"}))),
        ] {
            let response = respond(vec![empty.clone()], "STOP");
            assert_eq!(response["status"], "incomplete", "{empty}");
            assert_eq!(
                response["incomplete_details"]["reason"],
                "invalid_tool_call"
            );
            assert_eq!(custom_calls(&response), 0, "{empty}");
        }

        // An empty patch does not take a valid sibling or a real answer down with it,
        // but any other bad call still fails the turn, as it does in the stream.
        let sibling = respond(
            vec![call("apply_patch", None), call("lookup", Some(json!({})))],
            "STOP",
        );
        assert_eq!(sibling["status"], "completed");
        assert_eq!(sibling["output"].as_array().map(Vec::len), Some(1));
        assert_eq!(sibling["output"][0]["type"], "function_call");
        let text = respond(
            vec![json!({"text": "Done."}), call("apply_patch", None)],
            "STOP",
        );
        assert_eq!(text["status"], "completed");
        assert_eq!(custom_calls(&text), 0);
        let bad = respond(
            vec![call("exec", None), call("lookup", Some(json!({})))],
            "STOP",
        );
        assert_eq!(bad["status"], "incomplete");
        assert_eq!(bad["incomplete_details"]["reason"], "invalid_tool_call");
        assert_eq!(bad["output"].as_array().map(Vec::len), Some(1));

        // A cut-off turn proves nothing but a patch's end marker. Its reason stays.
        let cut = respond(
            vec![call("exec", Some(json!({"input": "pwd"})))],
            "MAX_TOKENS",
        );
        assert_eq!(cut["status"], "incomplete");
        assert_eq!(cut["incomplete_details"]["reason"], "max_tokens");
        assert_eq!(custom_calls(&cut), 0);
        let cut = respond(
            vec![call("apply_patch", Some(json!({"input": patch})))],
            "MAX_TOKENS",
        );
        assert_eq!(cut["status"], "incomplete");
        assert_eq!(cut["output"][0]["status"], "completed");
        assert_eq!(cut["output"][0]["input"], patch);

        // A summarizer reads this with an empty tool map, where nothing is custom.
        let summarizer = gemini_to_response(
            &json!({"candidates": [{"content": {"parts": [call("exec", None)]},
                "finishReason": "STOP"}]}),
            "gemini-test",
            &ResponseToolMap::default(),
        )
        .unwrap();
        assert_eq!(summarizer["status"], "completed");
        assert_eq!(summarizer["output"][0]["type"], "function_call");
    }

    #[test]
    fn gemini_tool_calls_finish_covers_a_call_on_an_earlier_chunk() {
        let mut gemini = GeminiStreamState::default();
        gemini_stream_to_chat(
            &call_chunk(json!({"name": "read_file", "args": {"path": "a"}}), false),
            &mut gemini,
        )
        .unwrap();
        let terminal = gemini_stream_to_chat(
            &json!({"candidates": [{"content": {"parts": []}, "finishReason": "STOP"}]}),
            &mut gemini,
        )
        .unwrap()
        .unwrap();
        assert_eq!(terminal["choices"][0]["finish_reason"], "tool_calls");

        let text_only = gemini_stream_to_chat(
            &json!({"candidates": [{"content": {"parts": [{"text": "ok"}]}, "finishReason": "STOP"}]}),
            &mut GeminiStreamState::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(text_only["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn streamed_gemini_usage_includes_thoughts_in_completion_and_preserves_total() {
        let chunk = gemini_stream_to_chat(
            &json!({
                "usageMetadata": {
                    "promptTokenCount": 7,
                    "candidatesTokenCount": 5,
                    "thoughtsTokenCount": 3,
                    "cachedContentTokenCount": 2,
                    "totalTokenCount": 19
                }
            }),
            &mut GeminiStreamState::default(),
        )
        .expect("Gemini stream")
        .expect("usage chunk");

        assert_eq!(chunk["usage"]["prompt_tokens"], 7);
        assert_eq!(chunk["usage"]["completion_tokens"], 8);
        assert_eq!(
            chunk["usage"]["completion_tokens_details"]["reasoning_tokens"],
            3
        );
        assert_eq!(chunk["usage"]["total_tokens"], 19);
    }
}
