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
mod tests;
