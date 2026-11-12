use super::*;

pub(crate) fn provider_stream_event_is_terminal(protocol: ProviderProtocol, value: &Value) -> bool {
    match protocol {
        ProviderProtocol::Responses => matches!(
            value.get("type").and_then(Value::as_str),
            Some("response.completed" | "response.failed" | "response.incomplete")
        ),
        ProviderProtocol::ChatCompletions => {
            value
                .get("choices")
                .and_then(Value::as_array)
                .is_some_and(|choices| {
                    choices.iter().any(|choice| {
                        choice
                            .get("finish_reason")
                            .is_some_and(|reason| !reason.is_null())
                    })
                })
                || value.get("error").is_some()
        }
        ProviderProtocol::AnthropicMessages => {
            matches!(
                value.get("type").and_then(Value::as_str),
                Some("message_stop" | "error")
            ) || value.get("type").and_then(Value::as_str) == Some("message_delta")
                && value
                    .pointer("/delta/stop_reason")
                    .is_some_and(|reason| !reason.is_null())
        }
        ProviderProtocol::GeminiGenerateContent => {
            let payload = unwrap_gemini_stream_payload(value);
            payload
                .get("candidates")
                .and_then(Value::as_array)
                .is_some_and(|candidates| {
                    candidates.iter().any(|candidate| {
                        candidate
                            .get("finishReason")
                            .is_some_and(|reason| !reason.is_null())
                    })
                })
                || payload.get("error").is_some()
                || value.get("error").is_some()
        }
    }
}

pub(super) fn provider_stream_event_is_visible_failure(
    protocol: ProviderProtocol,
    value: &Value,
) -> bool {
    match protocol {
        ProviderProtocol::Responses => {
            matches!(
                value.get("type").and_then(Value::as_str),
                Some("response.failed" | "response.incomplete")
            ) || value
                .pointer("/response/status")
                .and_then(Value::as_str)
                .is_some_and(|status| matches!(status, "failed" | "incomplete"))
                || value
                    .pointer("/response/error")
                    .is_some_and(|error| !error.is_null())
                || value
                    .pointer("/response/incomplete_details")
                    .is_some_and(|details| !details.is_null())
        }
        ProviderProtocol::ChatCompletions => {
            value.get("error").is_some()
                || value
                    .get("choices")
                    .and_then(Value::as_array)
                    .is_some_and(|choices| {
                        choices.iter().any(|choice| {
                            choice.get("error").is_some_and(|error| !error.is_null())
                                || choice
                                    .get("finish_reason")
                                    .and_then(Value::as_str)
                                    .is_some_and(|reason| !matches!(reason, "stop" | "tool_calls"))
                        })
                    })
        }
        ProviderProtocol::AnthropicMessages => {
            value.get("type").and_then(Value::as_str) == Some("error")
                || value
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| {
                        matches!(reason, "max_tokens" | "content_filter" | "refusal")
                    })
        }
        ProviderProtocol::GeminiGenerateContent => {
            let payload = unwrap_gemini_stream_payload(value);
            payload.get("error").is_some()
                || value.get("error").is_some()
                || payload
                    .get("candidates")
                    .and_then(Value::as_array)
                    .is_some_and(|candidates| {
                        candidates.iter().any(|candidate| {
                            candidate
                                .get("finishReason")
                                .and_then(Value::as_str)
                                .is_some_and(|reason| {
                                    gemini_finish_disposition(Some(reason))
                                        != GeminiFinishDisposition::Stop
                                })
                        })
                    })
        }
    }
}

pub(super) fn provider_stream_event_is_error(protocol: ProviderProtocol, value: &Value) -> bool {
    match protocol {
        ProviderProtocol::Responses => {
            value.get("type").and_then(Value::as_str) == Some("response.failed")
                || value
                    .pointer("/response/error")
                    .is_some_and(|error| !error.is_null())
        }
        ProviderProtocol::ChatCompletions => {
            value.get("error").is_some_and(|error| !error.is_null())
                || value
                    .get("choices")
                    .and_then(Value::as_array)
                    .is_some_and(|choices| {
                        choices.iter().any(|choice| {
                            choice.get("error").is_some_and(|error| !error.is_null())
                                || choice
                                    .get("finish_reason")
                                    .and_then(Value::as_str)
                                    .is_some_and(|reason| {
                                        !matches!(
                                            reason,
                                            "stop"
                                                | "tool_calls"
                                                | "length"
                                                | "max_tokens"
                                                | "content_filter"
                                        )
                                    })
                        })
                    })
        }
        ProviderProtocol::GeminiGenerateContent => {
            value.get("error").is_some_and(|error| !error.is_null())
        }
        ProviderProtocol::AnthropicMessages => {
            value.get("type").and_then(Value::as_str) == Some("error")
        }
    }
}

pub(crate) fn provider_stream_event_is_empty_terminal(
    protocol: ProviderProtocol,
    value: &Value,
) -> bool {
    provider_stream_event_is_empty_terminal_with_tools(protocol, value, &Default::default())
}

pub(super) fn provider_stream_event_is_empty_terminal_with_tools(
    protocol: ProviderProtocol,
    value: &Value,
    tools: &ResponseToolMap,
) -> bool {
    if protocol == ProviderProtocol::Responses {
        let response = value.get("response").unwrap_or(value);
        let reason = response
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str);
        if matches!(reason, Some("empty_response" | "invalid_tool_call"))
            && !responses_value_has_content(response)
        {
            return provider_stream_event_is_terminal(protocol, value);
        }
    }
    provider_stream_event_is_terminal(protocol, value)
        && !provider_stream_event_is_visible_failure(protocol, value)
        && match protocol {
            ProviderProtocol::Responses => value
                .get("response")
                .is_some_and(|response| !responses_value_has_content(response)),
            ProviderProtocol::ChatCompletions => !chat_value_has_content(value, tools),
            ProviderProtocol::AnthropicMessages => true,
            ProviderProtocol::GeminiGenerateContent => !gemini_value_has_content(value, tools),
        }
}

pub(crate) fn provider_stream_event_has_visible_content(
    protocol: ProviderProtocol,
    value: &Value,
) -> bool {
    provider_stream_event_has_visible_content_with_tools(protocol, value, &Default::default())
}

pub(super) fn provider_stream_event_has_visible_content_with_tools(
    protocol: ProviderProtocol,
    value: &Value,
    tools: &ResponseToolMap,
) -> bool {
    match protocol {
        ProviderProtocol::Responses => responses_event_has_visible_content(value),
        ProviderProtocol::ChatCompletions => chat_value_has_content(value, tools),
        ProviderProtocol::AnthropicMessages => match value.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                value.pointer("/content_block/type").and_then(Value::as_str) == Some("tool_use")
                    && json_tool_call_is_present(
                        value
                            .pointer("/content_block/name")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        value
                            .pointer("/content_block/input")
                            .unwrap_or(&Value::Null),
                    )
            }
            Some("content_block_delta") => {
                value.pointer("/delta/type").and_then(Value::as_str) == Some("input_json_delta")
                    && value
                        .pointer("/delta/partial_json")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.trim().is_empty())
                    || value
                        .pointer("/delta/text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| {
                            !text.trim().is_empty()
                                && !crate::translate::is_placeholder_progress_text(text)
                        })
            }
            _ => false,
        },
        ProviderProtocol::GeminiGenerateContent => gemini_value_has_content(value, tools),
    }
}

pub(super) fn provider_stream_event_is_reasoning(
    protocol: ProviderProtocol,
    value: &Value,
) -> bool {
    match protocol {
        ProviderProtocol::Responses => {
            value
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| {
                    kind.contains("reasoning")
                        || matches!(
                            kind,
                            "response.output_item.added" | "response.output_item.done"
                        ) && value.pointer("/item/type").and_then(Value::as_str)
                            == Some("reasoning")
                })
        }
        ProviderProtocol::AnthropicMessages => value
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| {
                kind == "content_block_start"
                    && matches!(
                        value.pointer("/content_block/type").and_then(Value::as_str),
                        Some("thinking" | "redacted_thinking")
                    )
                    || kind == "content_block_delta"
                        && matches!(
                            value.pointer("/delta/type").and_then(Value::as_str),
                            Some("thinking_delta" | "signature_delta")
                        )
            }),
        ProviderProtocol::GeminiGenerateContent => value
            .get("candidates")
            .and_then(Value::as_array)
            .is_some_and(|candidates| {
                candidates.iter().any(|candidate| {
                    candidate
                        .pointer("/content/parts")
                        .and_then(Value::as_array)
                        .is_some_and(|parts| {
                            parts.iter().any(|part| {
                                part.get("thought").and_then(Value::as_bool) == Some(true)
                                    || part.get("thoughtSignature").is_some()
                            })
                        })
                })
            }),
        ProviderProtocol::ChatCompletions => value
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(|choices| {
                choices.iter().any(|choice| {
                    choice
                        .get("delta")
                        .or_else(|| choice.get("message"))
                        .is_some_and(|message| {
                            message.get("reasoning_content").is_some()
                                || message.get("reasoning").is_some()
                                || message
                                    .get("content")
                                    .and_then(Value::as_array)
                                    .is_some_and(|chunks| {
                                        chunks.iter().any(|chunk| {
                                            chunk.get("type").and_then(Value::as_str)
                                                == Some("thinking")
                                        })
                                    })
                        })
                })
            }),
    }
}

fn responses_event_has_visible_content(value: &Value) -> bool {
    match value.get("type").and_then(Value::as_str) {
        Some("response.completed") => value
            .get("response")
            .is_some_and(responses_value_has_content),
        Some("response.output_text.delta" | "response.output_text.done") => value
            .get("delta")
            .or_else(|| value.get("text"))
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty()),
        Some(
            "response.function_call_arguments.delta"
            | "response.function_call_arguments.done"
            | "response.custom_tool_call_input.delta"
            | "response.custom_tool_call_input.done"
            | "response.tool_search_call_arguments.delta"
            | "response.tool_search_call_arguments.done"
            | "response.local_shell_call.in_progress"
            | "response.local_shell_call.completed"
            | "response.web_search_call.in_progress"
            | "response.web_search_call.searching"
            | "response.web_search_call.completed",
        ) => true,
        Some("response.output_item.added" | "response.output_item.done") => value
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|kind| {
                matches!(
                    kind,
                    "function_call"
                        | "custom_tool_call"
                        | "tool_search_call"
                        | "web_search_call"
                        | "local_shell_call"
                )
            }),
        _ => false,
    }
}

pub(super) fn responses_reasoning_semantic_history(value: &Value) -> Vec<Value> {
    let mut history = Vec::new();
    match value.get("type").and_then(Value::as_str) {
        Some("response.output_item.done")
            if value.pointer("/item/type").and_then(Value::as_str) == Some("reasoning") =>
        {
            if let Some(mut item) = value.get("item").cloned() {
                if let Some(object) = item.as_object_mut() {
                    object.remove("id");
                    object.remove("status");
                }
                history.push(item);
            }
        }
        Some("response.completed") => {
            if let Some(output) = value.pointer("/response/output").and_then(Value::as_array) {
                for item in output
                    .iter()
                    .filter(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
                {
                    let mut item = item.clone();
                    if let Some(object) = item.as_object_mut() {
                        object.remove("id");
                        object.remove("status");
                    }
                    history.push(item);
                }
            }
        }
        _ => {}
    }
    history
}

pub(super) fn canonical_reasoning_lifecycle_events(items: &[Value]) -> Vec<Value> {
    let mut events = Vec::new();
    for (output_index, item) in items.iter().enumerate() {
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        events.push(json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": added,
        }));
        events.push(json!({
            "type": "response.output_item.done",
            "output_index": output_index,
            "item": item,
        }));
    }
    events
}

pub(super) fn assign_responses_sequence(value: &mut Value, next: &mut u64) {
    value["sequence_number"] = json!(*next);
    *next = next.saturating_add(1);
}

pub(super) fn shift_responses_retry_event(value: &mut Value, offset: usize, history: &[Value]) {
    if offset == 0 {
        return;
    }
    if let Some(index) = value.get("output_index").and_then(Value::as_u64) {
        value["output_index"] = json!(index.saturating_add(offset as u64));
    }
    if matches!(
        value.get("type").and_then(Value::as_str),
        Some("response.completed" | "response.incomplete" | "response.failed")
    ) {
        if let Some(output) = value
            .pointer_mut("/response/output")
            .and_then(Value::as_array_mut)
        {
            let mut combined = history.to_vec();
            combined.append(output);
            *output = combined;
        }
    }
}

pub(super) fn chat_value_has_content(value: &Value, tools: &ResponseToolMap) -> bool {
    value
        .get("choices")
        .and_then(Value::as_array)
        .is_some_and(|choices| {
            choices.iter().any(|choice| {
                let delta = choice.get("delta");
                delta
                    .or_else(|| choice.get("message"))
                    .is_some_and(|message| {
                        message
                            .get("content")
                            .and_then(Value::as_str)
                            .is_some_and(|text| {
                                !text.trim().is_empty()
                                    && !crate::translate::is_placeholder_progress_text(text)
                            })
                            || chat_content_list_has_text(message)
                            || message
                                .get("tool_calls")
                                .and_then(Value::as_array)
                                .is_some_and(|calls| {
                                    calls.iter().any(|call| {
                                        if delta.is_some() {
                                            chat_tool_call_has_visible_content(call)
                                        } else {
                                            chat_whole_tool_call_is_visible(call, tools)
                                        }
                                    })
                                })
                    })
            })
        })
}

/// Chat content may be a list of chunks, which split thinking from the answer. Only
/// the answer counts as visible.
fn chat_content_list_has_text(message: &Value) -> bool {
    message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|chunks| {
            chunks.iter().any(|chunk| {
                chunk.get("type").and_then(Value::as_str) == Some("text")
                    && chunk
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| {
                            !text.trim().is_empty()
                                && !crate::translate::is_placeholder_progress_text(text)
                        })
            })
        })
}

/// A function without parameters is a real result, and the translators give it `{}`.
/// Other tools mean nothing without their input, so such a call stays empty and gets
/// retried, and so does `apply_patch` under any declaration. A name the request did
/// not declare counts as a function, because that is how the translators deliver it.
fn tool_without_arguments_is_visible(name: &str, tools: &ResponseToolMap) -> bool {
    !name.trim().is_empty()
        && name != "apply_patch"
        && tools
            .identity(name)
            .is_none_or(|tool| tool.kind == crate::translate::ResponseToolKind::Function)
}

/// Whether a `data:` frame a Chat or Gemini attempt held back carries a tool call.
pub(super) fn held_frame_has_tool_call(frame: &[u8]) -> bool {
    let Some(value) = std::str::from_utf8(frame)
        .ok()
        .and_then(|frame| frame.trim().strip_prefix("data:"))
        .and_then(|data| serde_json::from_str::<Value>(data.trim()).ok())
    else {
        return false;
    };
    let chat_call = value
        .get("choices")
        .and_then(Value::as_array)
        .is_some_and(|choices| {
            choices.iter().any(|choice| {
                choice
                    .pointer("/delta/tool_calls")
                    .and_then(Value::as_array)
                    .is_some_and(|calls| !calls.is_empty())
            })
        });
    chat_call
        || unwrap_gemini_stream_payload(&value)
            .get("candidates")
            .and_then(Value::as_array)
            .is_some_and(|candidates| {
                candidates.iter().any(|candidate| {
                    candidate
                        .pointer("/content/parts")
                        .and_then(Value::as_array)
                        .is_some_and(|parts| {
                            parts.iter().any(|part| {
                                part.get("functionCall").is_some()
                                    || part.get("function_call").is_some()
                            })
                        })
                })
            })
}

/// A whole call from a non-streamed message or a named delta that ended its stream.
/// A streamed delta is judged by [`chat_tool_call_has_visible_content`] instead,
/// because its name usually arrives before its arguments.
fn chat_whole_tool_call_is_visible(call: &Value, tools: &ResponseToolMap) -> bool {
    let arguments = call
        .pointer("/function/arguments")
        .or_else(|| call.get("arguments"))
        .or_else(|| call.get("args"))
        .or_else(|| call.get("input"));
    let no_arguments = arguments.is_none_or(|arguments| {
        arguments.is_null()
            || arguments
                .as_str()
                .is_some_and(|text| text.trim().is_empty())
    });
    if no_arguments {
        return tool_without_arguments_is_visible(
            call.pointer("/function/name")
                .or_else(|| call.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(""),
            tools,
        );
    }
    chat_tool_call_has_visible_content(call)
}

/// Whether a streamed Chat chunk names a call that would count as visible if no
/// arguments ever follow it.
pub(super) fn chat_value_names_a_tool_call(value: &Value, tools: &ResponseToolMap) -> bool {
    value
        .get("choices")
        .and_then(Value::as_array)
        .is_some_and(|choices| {
            choices.iter().any(|choice| {
                choice
                    .pointer("/delta/tool_calls")
                    .and_then(Value::as_array)
                    .is_some_and(|calls| {
                        calls.iter().any(|call| {
                            tool_without_arguments_is_visible(
                                call.pointer("/function/name")
                                    .and_then(Value::as_str)
                                    .unwrap_or(""),
                                tools,
                            )
                        })
                    })
            })
        })
}

fn chat_tool_call_has_visible_content(call: &Value) -> bool {
    let name = call
        .pointer("/function/name")
        .or_else(|| call.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    json_tool_call_is_present(
        name,
        call.pointer("/function/arguments")
            .or_else(|| call.get("arguments"))
            .or_else(|| call.get("args"))
            .or_else(|| call.get("input"))
            .unwrap_or(&Value::Null),
    )
}

fn json_tool_call_is_present(name: &str, arguments: &Value) -> bool {
    let arguments = match arguments {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    crate::translate::custom_tool_input_is_present(name, &arguments)
}

pub(super) fn gemini_value_has_content(value: &Value, tools: &ResponseToolMap) -> bool {
    let value = unwrap_gemini_stream_payload(value);
    value
        .get("candidates")
        .and_then(Value::as_array)
        .is_some_and(|candidates| {
            candidates.iter().any(|candidate| {
                candidate
                    .pointer("/content/parts")
                    .and_then(Value::as_array)
                    .is_some_and(|parts| {
                        parts
                            .iter()
                            .any(|part| gemini_part_has_visible_content(part, tools))
                    })
            })
        })
}

fn gemini_part_has_visible_content(part: &Value, tools: &ResponseToolMap) -> bool {
    if part.get("thought").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    if part
        .get("text")
        .and_then(Value::as_str)
        .is_some_and(|text| {
            !text.trim().is_empty() && !crate::translate::is_placeholder_progress_text(text)
        })
    {
        return true;
    }
    part.get("functionCall")
        .or_else(|| part.get("function_call"))
        .and_then(Value::as_object)
        .is_some_and(|call| {
            let name = call.get("name").and_then(Value::as_str).unwrap_or("");
            let arguments = call
                .get("args")
                .or_else(|| call.get("arguments"))
                .or_else(|| call.get("input"))
                .unwrap_or(&Value::Null);
            // Gemini omits `args` for a call without parameters.
            if arguments.is_null() {
                return tool_without_arguments_is_visible(name, tools);
            }
            json_tool_call_is_present(name, arguments)
        })
}

pub(super) fn provider_sse(protocol: ProviderProtocol, event_type: &str, value: &Value) -> String {
    match protocol {
        ProviderProtocol::Responses | ProviderProtocol::AnthropicMessages => sse(event_type, value),
        ProviderProtocol::ChatCompletions | ProviderProtocol::GeminiGenerateContent => format!(
            "data: {}\n\n",
            serde_json::to_string(value).unwrap_or_else(|_| "{}".into())
        ),
    }
}

pub(super) fn provider_error_event_type(protocol: ProviderProtocol) -> &'static str {
    match protocol {
        ProviderProtocol::Responses => "response.failed",
        ProviderProtocol::AnthropicMessages => "error",
        ProviderProtocol::ChatCompletions | ProviderProtocol::GeminiGenerateContent => "message",
    }
}

pub(super) fn empty_completion_failed_event_for_protocol(
    protocol: ProviderProtocol,
    usage: TokenUsage,
    response_id: Option<&str>,
) -> Value {
    let message = "The model returned an empty completion and the retry failed or was empty again.";
    match protocol {
        ProviderProtocol::Responses => json!({
            "type": "response.failed",
            "response": {
                "id": response_id
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("resp_{}", Uuid::new_v4().simple())),
                "status": "failed",
                "output": [],
                "error": {
                    "type": "upstream_error",
                    "code": EMPTY_COMPLETION_RETRY_FAILED_CODE,
                    "message": message
                },
                "usage": token_usage_json(&usage)
            }
        }),
        ProviderProtocol::ChatCompletions | ProviderProtocol::GeminiGenerateContent => json!({
            "error": {
                "type": "upstream_error",
                "code": EMPTY_COMPLETION_RETRY_FAILED_CODE,
                "message": message
            },
            "usage": token_usage_json(&usage)
        }),
        ProviderProtocol::AnthropicMessages => json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "code": EMPTY_COMPLETION_RETRY_FAILED_CODE,
                "message": message
            },
            "usage": token_usage_json(&usage)
        }),
    }
}

#[cfg(test)]
pub(super) fn empty_completion_retry_failed_response(
    usage: TokenUsage,
    additional_sends: u16,
) -> Result<reqwest::Response, AttemptFailure> {
    empty_completion_retry_failed_response_with_observation(usage, additional_sends, None)
}

pub(super) fn empty_completion_retry_failed_response_with_observation(
    usage: TokenUsage,
    additional_sends: u16,
    retry_observation: Option<ProviderRetryObservation>,
) -> Result<reqwest::Response, AttemptFailure> {
    let mut observation = retry_observation.unwrap_or_default();
    // The failure body/marker owns all known consumption. Metadata must not
    // charge the preceding HTTP attempts for a second time.
    let usage = sum_token_usage(std::mem::take(&mut observation.usage), usage);
    observation.known_billing = Some(usage.clone());
    empty_completion_retry_failed_response_with_observation_inner(
        usage,
        additional_sends,
        Some(observation),
    )
}

fn empty_completion_retry_failed_response_with_observation_inner(
    usage: TokenUsage,
    additional_sends: u16,
    mut retry_observation: Option<ProviderRetryObservation>,
) -> Result<reqwest::Response, AttemptFailure> {
    let value = json!({
        "error": {
            "type": "upstream_error",
            "code": EMPTY_COMPLETION_RETRY_FAILED_CODE,
            "message": "The model returned an empty completion and the retry failed or was empty again."
        },
        "usage": token_usage_json(&usage)
    });
    let mut response = axum::http::Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .header(header::CONTENT_TYPE, "application/json")
        .body(reqwest::Body::from(value.to_string()))
        .map(reqwest::Response::from)
        .map_err(|_| request_failure("gateway_error", "failed to build empty completion error"))?;
    response
        .extensions_mut()
        .insert(EmptyCompletionRecoveryFailure {
            usage: usage.clone(),
            additional_sends,
        });
    let observation = retry_observation.get_or_insert_with(ProviderRetryObservation::default);
    observation.additional_sends = observation
        .additional_sends
        .saturating_add(additional_sends);
    observation.recovery_kinds.extend(std::iter::repeat_n(
        "empty-completion".to_string(),
        usize::from(additional_sends),
    ));
    observation.known_billing = Some(usage);
    response.extensions_mut().insert(observation.clone());
    Ok(response)
}

pub(super) async fn response_body_usage(response: Response<Body>) -> TokenUsage {
    let mut usage = response
        .extensions()
        .get::<crate::observability::ReportedUpstreamUsage>()
        .map(|usage| usage.0.clone())
        .unwrap_or_default();
    let body_usage = to_bytes(response.into_body(), 64 * 1024)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .as_ref()
        .map(TokenUsage::from_json)
        .unwrap_or_default();
    usage.merge_max(body_usage);
    usage
}

pub(super) async fn response_body_usage_from_reqwest(response: reqwest::Response) -> TokenUsage {
    match super::stream::bounded_json_with_usage(response, 64 * 1024).await {
        Ok(value) => TokenUsage::from_json(&value),
        Err((_, usage)) => usage,
    }
}

pub(crate) fn sum_token_usage(left: TokenUsage, right: TokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: left.input_tokens.saturating_add(right.input_tokens),
        output_tokens: left.output_tokens.saturating_add(right.output_tokens),
        cached_input_tokens: left
            .cached_input_tokens
            .saturating_add(right.cached_input_tokens),
        cache_creation_input_tokens: left
            .cache_creation_input_tokens
            .saturating_add(right.cache_creation_input_tokens),
        reasoning_tokens: left.reasoning_tokens.saturating_add(right.reasoning_tokens),
        total_tokens: left.total_tokens.saturating_add(right.total_tokens),
        cache_read_reported: (left.input_tokens == 0 || left.cache_read_reported)
            && (right.input_tokens == 0 || right.cache_read_reported)
            && (left.cache_read_reported || right.cache_read_reported),
    }
}

pub(crate) fn token_usage_json(usage: &TokenUsage) -> Value {
    let mut value = json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "total_tokens": usage.total_tokens.max(
            usage.input_tokens.saturating_add(usage.output_tokens)
        ),
        "output_tokens_details": {"reasoning_tokens": usage.reasoning_tokens}
    });
    // Codex cannot parse details without `cached_tokens`, so an unknown cache read
    // leaves the object out, cache creation included.
    crate::translate::set_cached_input_tokens(
        &mut value,
        usage
            .cache_read_reported
            .then_some(usage.cached_input_tokens),
    );
    if usage.cache_read_reported && usage.cache_creation_input_tokens > 0 {
        value["input_tokens_details"]["cache_creation_tokens"] =
            json!(usage.cache_creation_input_tokens);
    }
    value
}

fn write_raw_anthropic_usage(value: &mut Value, usage: &TokenUsage) {
    if !value.get("usage").is_some_and(Value::is_object) {
        value["usage"] = json!({});
    }
    let raw = value.get_mut("usage").and_then(Value::as_object_mut);
    if let Some(raw) = raw {
        raw.insert(
            "input_tokens".into(),
            json!(usage
                .input_tokens
                .saturating_sub(if usage.cache_read_reported {
                    usage.cached_input_tokens
                } else {
                    0
                })
                .saturating_sub(usage.cache_creation_input_tokens)),
        );
        raw.insert("output_tokens".into(), json!(usage.output_tokens));
        if usage.cache_read_reported {
            raw.insert(
                "cache_read_input_tokens".into(),
                json!(usage.cached_input_tokens),
            );
        } else {
            raw.remove("cache_read_input_tokens");
        }
        if usage.cache_creation_input_tokens > 0 || raw.contains_key("cache_creation_input_tokens")
        {
            raw.insert(
                "cache_creation_input_tokens".into(),
                json!(usage.cache_creation_input_tokens),
            );
        }
    }
}
pub(super) fn write_token_usage(value: &mut Value, usage: TokenUsage) {
    if value.get("response").is_some_and(|response| {
        response.get("usageMetadata").is_some() || response.get("candidates").is_some()
    }) {
        write_token_usage(&mut value["response"], usage);
        return;
    }
    if value.get("type").and_then(Value::as_str) == Some("message")
        || value.get("content").is_some() && value.get("stop_reason").is_some()
    {
        write_raw_anthropic_usage(value, &usage);
        return;
    }
    let canonical = token_usage_json(&usage);
    if value
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| {
            matches!(
                kind,
                "response.completed" | "response.failed" | "response.incomplete"
            )
        })
    {
        value["response"]["usage"] = canonical;
    } else if value.get("candidates").is_some() || value.get("usageMetadata").is_some() {
        value["usageMetadata"] = json!({
            "promptTokenCount": usage.input_tokens,
            "candidatesTokenCount": usage.output_tokens.saturating_sub(usage.reasoning_tokens),
            "thoughtsTokenCount": usage.reasoning_tokens,
            "totalTokenCount": usage.total_tokens.max(
                usage.input_tokens.saturating_add(usage.output_tokens)
            )
        });
        if usage.cache_read_reported {
            value["usageMetadata"]["cachedContentTokenCount"] = json!(usage.cached_input_tokens);
        }
    } else if value.get("choices").is_some() {
        value["usage"] = json!({
            "prompt_tokens": usage.input_tokens,
            "completion_tokens": usage.output_tokens,
            "total_tokens": usage.total_tokens.max(
                usage.input_tokens.saturating_add(usage.output_tokens)
            ),
            "prompt_tokens_details": if usage.cache_read_reported { json!({"cached_tokens": usage.cached_input_tokens}) } else { json!({}) },
            "completion_tokens_details": {"reasoning_tokens": usage.reasoning_tokens}
        });
    } else {
        value["usage"] = canonical;
    }
}

pub(super) fn write_anthropic_stream_start_usage(value: &mut Value, usage: TokenUsage) {
    write_raw_anthropic_usage(&mut value["message"], &usage);
}

pub(super) fn write_stream_token_usage(
    protocol: ProviderProtocol,
    value: &mut Value,
    usage: TokenUsage,
) {
    if protocol == ProviderProtocol::AnthropicMessages
        && value.get("type").and_then(Value::as_str) == Some("message_delta")
    {
        let has_input_update = value
            .get("usage")
            .and_then(Value::as_object)
            .is_some_and(|raw| {
                [
                    "input_tokens",
                    "cache_read_input_tokens",
                    "cache_creation_input_tokens",
                ]
                .iter()
                .any(|key| raw.contains_key(*key))
            });
        if has_input_update {
            write_raw_anthropic_usage(value, &usage);
        } else {
            value["usage"]["output_tokens"] = json!(usage.output_tokens);
        }
        return;
    }
    write_token_usage(value, usage);
}

pub(crate) fn completion_is_empty(value: &Value) -> bool {
    completion_is_empty_with_tools(value, &Default::default())
}

pub(super) fn completion_is_empty_with_tools(value: &Value, tools: &ResponseToolMap) -> bool {
    let value = if value.get("response").is_some_and(|response| {
        response.get("candidates").is_some() || response.get("usageMetadata").is_some()
    }) {
        &value["response"]
    } else {
        value
    };
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return false;
    }
    let incomplete_reason = value
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str);
    if matches!(
        incomplete_reason,
        Some("empty_response" | "invalid_tool_call")
    ) && !responses_value_has_content(value)
    {
        // Translated streams mark placeholder-only / empty apply_patch turns
        // incomplete. Retry them; do not treat the incomplete flag as content.
        return true;
    }
    if value
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| matches!(status, "failed" | "incomplete"))
        || value
            .get("incomplete_details")
            .is_some_and(|details| !details.is_null())
    {
        return false;
    }
    if value.get("output").is_some_and(Value::is_array)
        || value.get("status").and_then(Value::as_str) == Some("completed")
    {
        return !responses_value_has_content(value);
    }
    if value.get("stop_reason").and_then(Value::as_str) == Some("end_turn") {
        return !value
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|content| {
                content.iter().any(|part| {
                    part.get("type").and_then(Value::as_str) == Some("text")
                        && part
                            .get("text")
                            .and_then(Value::as_str)
                            .is_some_and(|text| !text.is_empty())
                        || part.get("type").and_then(Value::as_str) == Some("tool_use")
                })
            });
    }
    if let Some(choices) = value.get("choices").and_then(Value::as_array) {
        return choices.is_empty()
            || choices.iter().all(|choice| {
                if choice
                    .get("finish_reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| {
                        matches!(reason, "length" | "max_tokens" | "content_filter")
                    })
                {
                    return false;
                }
                let message = choice.get("message").or_else(|| choice.get("delta"));
                message.is_none_or(|message| {
                    message
                        .get("content")
                        .and_then(Value::as_str)
                        .is_none_or(|text| {
                            text.is_empty() || crate::translate::is_placeholder_progress_text(text)
                        })
                        && !chat_content_list_has_text(message)
                        && message
                            .get("tool_calls")
                            .and_then(Value::as_array)
                            .is_none_or(|calls| {
                                calls
                                    .iter()
                                    .all(|call| !chat_whole_tool_call_is_visible(call, tools))
                            })
                })
            });
    }
    if let Some(content) = value.get("content").and_then(Value::as_array) {
        if value
            .get("stop_reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| matches!(reason, "max_tokens" | "content_filter" | "refusal"))
        {
            return false;
        }
        return !content
            .iter()
            .any(|part| match part.get("type").and_then(Value::as_str) {
                Some("text") => part
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty()),
                Some("tool_use") => true,
                _ => false,
            });
    }
    if let Some(candidates) = value.get("candidates").and_then(Value::as_array) {
        if candidates.iter().any(|candidate| {
            candidate
                .get("finishReason")
                .and_then(Value::as_str)
                .is_some_and(|reason| {
                    gemini_finish_disposition(Some(reason)) != GeminiFinishDisposition::Stop
                })
        }) || value.get("promptFeedback").is_some_and(|feedback| {
            feedback
                .get("blockReason")
                .is_some_and(|reason| !reason.is_null())
        }) {
            return false;
        }
        return !candidates.iter().any(|candidate| {
            candidate
                .pointer("/content/parts")
                .and_then(Value::as_array)
                .is_some_and(|parts| {
                    parts
                        .iter()
                        .any(|part| gemini_part_has_visible_content(part, tools))
                })
        });
    }
    false
}

fn responses_value_has_content(value: &Value) -> bool {
    value
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|output| {
            output
                .iter()
                .any(|item| match item.get("type").and_then(Value::as_str) {
                    Some("message" | "agent_message") => item
                        .get("content")
                        .and_then(Value::as_array)
                        .is_some_and(|parts| {
                            parts.iter().any(|part| {
                                matches!(
                                    part.get("type").and_then(Value::as_str),
                                    Some("output_text" | "text")
                                ) && part
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .is_some_and(|text| {
                                        !text.is_empty()
                                            && !crate::translate::is_placeholder_progress_text(text)
                                    })
                            })
                        }),
                    Some("custom_tool_call") => {
                        item.get("status").and_then(Value::as_str) != Some("incomplete")
                            && crate::translate::custom_tool_input_is_actionable(
                                item.get("name").and_then(Value::as_str).unwrap_or(""),
                                item.get("input").and_then(Value::as_str).unwrap_or(""),
                            )
                    }
                    Some(
                        "function_call" | "tool_search_call" | "web_search_call"
                        | "local_shell_call",
                    ) => true,
                    _ => false,
                })
        })
}
