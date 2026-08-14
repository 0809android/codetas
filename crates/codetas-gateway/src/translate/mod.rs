use crate::compaction::{decode_summary, expand_local_compactions};
use crate::compat::repair_translated_input_items;
use serde_json::{json, Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

mod stream;
mod to_chat;
mod to_response;
mod util;

pub(crate) use stream::*;
pub(crate) use to_chat::*;
pub(crate) use to_response::*;
pub(crate) use util::*;

/// Rewrite Codex/Responses-only history so Chat, Anthropic, Gemini, and Kiro
/// adapters do not reject the whole turn. ChatGPT passthrough must not use this.
pub fn prepare_translated_responses_request(body: &mut Value, strip_stateful: bool) {
    expand_local_compactions(body);
    if strip_stateful {
        if let Some(object) = body.as_object_mut() {
            object.remove("previous_response_id");
            object.remove("conversation");
        }
    }

    let extra_tools = collect_additional_tools(body);
    if !extra_tools.is_empty() {
        let tools = body
            .as_object_mut()
            .map(|object| object.entry("tools").or_insert_with(|| json!([])));
        if let Some(Value::Array(tools)) = tools {
            for tool in extra_tools {
                if !tools.iter().any(|existing| existing == &tool) {
                    tools.push(tool);
                }
            }
        }
    }

    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    let mut rewritten = Vec::with_capacity(items.len());
    for item in items.iter() {
        if let Some(converted) = rewrite_translated_input_item(item) {
            rewritten.push(converted);
        }
    }
    *items = rewritten;
    repair_translated_input_items(body);
}

fn collect_additional_tools(body: &Value) -> Vec<Value> {
    let Some(items) = body.get("input").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("additional_tools"))
        .flat_map(|item| {
            item.get("tools")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .collect()
}

fn rewrite_translated_input_item(item: &Value) -> Option<Value> {
    let item_type = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message");
    match item_type {
        "compaction_trigger" | "additional_tools" | "item_reference" | "web_search_call" => None,
        "compaction_summary" | "context_compaction" => {
            let mut next = item.clone();
            if let Some(object) = next.as_object_mut() {
                object.insert("type".into(), Value::String("compaction".into()));
            }
            Some(next)
        }
        "agent_message" => {
            let mut next = item.clone();
            if let Some(object) = next.as_object_mut() {
                object.insert("type".into(), Value::String("message".into()));
                object.insert("role".into(), Value::String("user".into()));
                if object.get("content").is_none() {
                    object.insert(
                        "content".into(),
                        json!([{"type": "input_text", "text": "(sub-agent message received)"}]),
                    );
                }
            }
            Some(next)
        }
        "local_shell_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .cloned()
                .unwrap_or_else(|| json!("call_unknown"));
            let command = item
                .pointer("/action/command")
                .cloned()
                .unwrap_or_else(|| json!([]));
            Some(json!({
                "type": "function_call",
                "call_id": call_id,
                "name": "shell",
                "arguments": json!({"command": command}).to_string()
            }))
        }
        "tool_search_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .cloned()
                .unwrap_or_else(|| json!("call_unknown"));
            let arguments = item.get("arguments").cloned().unwrap_or_else(|| json!({}));
            Some(json!({
                "type": "function_call",
                "call_id": call_id,
                "name": "tool_search",
                "arguments": payload_to_arguments(&arguments)
            }))
        }
        "tool_search_output" => {
            let call_id = item.get("call_id").cloned().unwrap_or_else(|| json!(""));
            let tools = item
                .get("tools")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let names = tools
                .iter()
                .filter_map(|tool| tool.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(", ");
            let text = if names.is_empty() {
                "Tool search returned no tools.".to_string()
            } else {
                format!("Tool search loaded: {names}")
            };
            Some(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": text
            }))
        }
        _ => Some(item.clone()),
    }
}

pub fn strip_translated_input_images(body: &mut Value) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    for item in items {
        replace_image_parts(item);
    }
}

fn replace_image_parts(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                replace_image_parts(item);
            }
        }
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("input_image")
                || object.get("type").and_then(Value::as_str) == Some("image_url")
            {
                object.clear();
                object.insert("type".into(), Value::String("input_text".into()));
                object.insert(
                    "text".into(),
                    Value::String("[image omitted for this model]".into()),
                );
                return;
            }
            for child in object.values_mut() {
                replace_image_parts(child);
            }
        }
        _ => {}
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepares_codex_control_items_for_translated_providers() {
        let mut request = json!({
            "previous_response_id": "resp_old",
            "conversation": "conv_1",
            "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}}],
            "input": [
                {"type": "compaction_trigger"},
                {"type": "additional_tools", "tools": [{"type": "function", "name": "exec", "parameters": {"type": "object"}}]},
                {"type": "agent_message", "content": [{"type": "input_text", "text": "child done"}]},
                {"type": "local_shell_call", "call_id": "call_shell", "action": {"command": ["pwd"]}},
                {"type": "function_call_output", "call_id": "call_shell", "output": "/tmp"},
                {"type": "function_call_output", "call_id": "call_missing", "output": "orphan"}
            ]
        });
        prepare_translated_responses_request(&mut request, true);
        assert!(request.get("previous_response_id").is_none());
        assert!(request.get("conversation").is_none());
        assert_eq!(request["tools"].as_array().map(Vec::len), Some(2));
        assert_eq!(request["input"][0]["type"], "message");
        assert_eq!(request["input"][1]["type"], "function_call");
        assert_eq!(request["input"][1]["name"], "shell");
        assert_eq!(request["input"][2]["type"], "function_call_output");
        assert_eq!(request["input"][3]["type"], "message");
        assert!(request["input"][3]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("orphan"));

        let chat = responses_to_chat(&request, "deepseek-v4-flash").expect("translated history");
        assert_eq!(chat["messages"][0]["role"], "user");
        assert_eq!(chat["messages"][1]["role"], "assistant");
        assert_eq!(chat["messages"][1]["content"], "");
        assert_eq!(chat["messages"][2]["role"], "tool");
    }

    #[test]
    fn converts_responses_request_to_chat() {
        let request = json!({
            "model": "route/model-a",
            "instructions": "Be concise.",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "Hello"}]
            }],
            "tools": [{
                "type": "function",
                "name": "lookup",
                "description": "Look something up",
                "parameters": {"type": "object", "properties": {}}
            }],
            "max_output_tokens": 200,
            "stream": true
        });

        let chat = responses_to_chat(&request, "model-a").expect("request should translate");
        assert_eq!(chat["model"], "model-a");
        assert_eq!(chat["messages"][0]["role"], "system");
        assert_eq!(chat["messages"][1]["content"], "Hello");
        assert_eq!(chat["tools"][0]["function"]["name"], "lookup");
        assert_eq!(chat["max_tokens"], 200);
        assert_eq!(chat["stream_options"]["include_usage"], true);
    }

    #[test]
    fn omits_unrepresentable_tools_for_chat_adapter() {
        let request = json!({
            "input": "Hello",
            "tools": [
                {"type": "custom", "name": "apply_patch", "format": {"type": "grammar"}},
                {"type": "namespace", "name": "plugins", "tools": []},
                {"type": "web_search"},
                {
                    "type": "function",
                    "name": "lookup",
                    "description": "Look something up",
                    "parameters": {"type": "object", "properties": {}}
                }
            ]
        });
        let chat = responses_to_chat(&request, "model-a").expect("request should translate");
        assert_eq!(chat["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(chat["tools"][0]["function"]["name"], "lookup");
    }

    #[test]
    fn keeps_reasoning_on_the_assistant_tool_call_message() {
        let mut request = json!({
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "List files"}]
                },
                {
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "I should inspect the directory."}]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": ""}]
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "exec_command",
                    "arguments": "{\"cmd\":\"ls\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "file.txt"
                }
            ]
        });

        normalize_chat_reasoning_history(&mut request, true);
        let chat =
            responses_to_chat(&request, "deepseek-v4-flash").expect("request should translate");
        assert_eq!(chat["messages"].as_array().map(Vec::len), Some(3));
        assert_eq!(chat["messages"][1]["role"], "assistant");
        assert_eq!(
            chat["messages"][1]["reasoning_content"],
            "I should inspect the directory."
        );
        assert_eq!(
            chat["messages"][1]["tool_calls"][0]["function"]["name"],
            "exec_command"
        );
        assert_eq!(chat["messages"][2]["role"], "tool");
    }

    #[test]
    fn removes_empty_assistant_messages_without_reasoning_preservation() {
        let mut request = json!({
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "List files"}]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": ""}]
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "exec_command",
                    "arguments": "{\"cmd\":\"ls\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "file.txt"
                }
            ]
        });

        normalize_chat_reasoning_history(&mut request, false);
        let chat = responses_to_chat(&request, "kimi-test").expect("request should translate");
        assert_eq!(chat["messages"].as_array().map(Vec::len), Some(3));
        assert_eq!(chat["messages"][0]["role"], "user");
        assert_eq!(chat["messages"][1]["role"], "assistant");
        assert!(chat["messages"][1]["tool_calls"].is_array());
        assert_eq!(chat["messages"][2]["role"], "tool");
    }

    #[test]
    fn keeps_non_empty_assistant_refusals() {
        let mut request = json!({
            "input": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "refusal", "refusal": "I cannot help with that."}]
            }]
        });

        normalize_chat_reasoning_history(&mut request, false);
        let chat = responses_to_chat(&request, "kimi-test").expect("request should translate");
        assert_eq!(
            chat["messages"][0]["content"],
            "I cannot help with that."
        );
    }

    #[test]
    fn removes_assistant_messages_with_only_unrepresentable_content() {
        let mut request = json!({
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Continue"}]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "unsupported_hosted_content"}]
                }
            ]
        });

        normalize_chat_reasoning_history(&mut request, false);
        let chat = responses_to_chat(&request, "kimi-test").expect("request should translate");
        assert_eq!(chat["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(chat["messages"][0]["role"], "user");
    }

    #[test]
    fn converts_chat_response_to_responses_object() {
        let chat = json!({
            "choices": [{"message": {"role": "assistant", "content": "Hello"}}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6}
        });
        let response = chat_to_response(&chat, "route/model-a", &BTreeSet::new())
            .expect("response should translate");
        assert_eq!(response["object"], "response");
        assert_eq!(response["model"], "route/model-a");
        assert_eq!(response["output"][0]["content"][0]["text"], "Hello");
        assert_eq!(response["usage"]["total_tokens"], 6);
    }

    #[test]
    fn converts_custom_tool_call_history_to_chat_and_back() {
        // Mirrors what Codex App replays on restart: a local tool invocation
        // stored as custom_tool_call / custom_tool_call_output input items.
        let request = json!({
            "model": "deepseek-v4-flash",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "list the files"}]},
                {"type": "custom_tool_call", "call_id": "call_1", "name": "exec", "input": "const r = await tools.list_dir(); text(r);"},
                {"type": "custom_tool_call_output", "call_id": "call_1", "output": [{"type": "output_text", "text": "src/"}], "status": "completed"}
            ]
        });
        let chat = responses_to_chat(&request, "deepseek-v4-flash")
            .expect("request with custom_tool_call items should translate");
        assert_eq!(chat["messages"].as_array().map(Vec::len), Some(3));
        assert_eq!(chat["messages"][1]["role"], "assistant");
        assert_eq!(
            chat["messages"][1]["tool_calls"][0]["function"]["arguments"],
            "const r = await tools.list_dir(); text(r);"
        );
        assert_eq!(chat["messages"][2]["role"], "tool");
        assert_eq!(chat["messages"][2]["tool_call_id"], "call_1");
        assert_eq!(chat["messages"][2]["content"], "src/");
    }

    #[test]
    fn emits_custom_tool_call_for_declared_custom_tools() {
        // When the request declares a tool as type "custom" (Codex App local
        // tools like exec / apply_patch), the response-side tool call must be
        // emitted as custom_tool_call so Codex routes it to its local handler.
        let chat = json!({
            "choices": [{"message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "exec", "arguments": "{\"command\":\"ls\"}"}
                }]
            }}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6}
        });
        let custom_tools = ["exec", "apply_patch"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let response = chat_to_response(&chat, "route/model-a", &custom_tools)
            .expect("response should translate");
        assert_eq!(response["output"][0]["type"], "custom_tool_call");
        assert_eq!(response["output"][0]["name"], "exec");
        assert_eq!(response["output"][0]["input"], "{\"command\":\"ls\"}");
        assert!(response["output"][0].get("arguments").is_none());
    }

    #[test]
    fn emits_function_call_for_undeclared_tools() {
        // Existing function tools must not regress: without a custom tool
        // declaration the same Chat response stays a function_call item.
        let chat = json!({
            "choices": [{"message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"q\":\"x\"}"}
                }]
            }}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6}
        });
        let response = chat_to_response(&chat, "route/model-a", &BTreeSet::new())
            .expect("response should translate");
        assert_eq!(response["output"][0]["type"], "function_call");
        assert_eq!(response["output"][0]["arguments"], "{\"q\":\"x\"}");
    }

    #[test]
    fn streaming_events_have_monotonic_sequence_numbers() {
        let (mut state, initial) = ChatStreamState::new("route/model-a".into(), BTreeSet::new());
        let mut events = initial;
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"content": "Hi"}}]
        })));
        events.extend(state.finish());

        let sequence_numbers = events
            .iter()
            .map(|event| {
                let data = event
                    .lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .expect("event should contain data");
                serde_json::from_str::<Value>(data).expect("event data should be JSON")
                    ["sequence_number"]
                    .as_u64()
                    .expect("event should have a sequence number")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            sequence_numbers,
            (1..=sequence_numbers.len() as u64).collect::<Vec<_>>()
        );
    }

    #[test]
    fn preserves_non_string_custom_tool_input_payload() {
        // A malformed client may send a structured `input` instead of the
        // spec'd string. The payload must survive the Chat conversion instead
        // of silently collapsing to "{}".
        let request = json!({
            "model": "deepseek-v4-flash",
            "input": [
                {"type": "custom_tool_call", "call_id": "call_1", "name": "exec", "input": {"command": "ls", "cwd": "/tmp"}}
            ]
        });
        let chat =
            responses_to_chat(&request, "deepseek-v4-flash").expect("request should translate");
        assert_eq!(
            chat["messages"][0]["tool_calls"][0]["function"]["arguments"],
            "{\"command\":\"ls\",\"cwd\":\"/tmp\"}"
        );
    }

    #[test]
    fn streaming_classifies_late_name_as_custom_tool() {
        // Some Chat providers stream `{"index":0,"id":"call_x"}` first and the
        // tool name in a later chunk. The custom-tool classification must be
        // re-evaluated when the name arrives, not frozen at the first chunk.
        let custom_tools = ["exec"].into_iter().map(str::to_string).collect();
        let (mut state, initial) = ChatStreamState::new("route/model-a".into(), custom_tools);
        let mut events = initial;
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_x"}]}}]
        })));
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"name": "exec", "arguments": "{\"cmd\":\"ls\"}"}}]}}]
        })));
        events.extend(state.finish());

        let event_payloads = events
            .iter()
            .map(|event| {
                serde_json::from_str::<Value>(
                    event
                        .lines()
                        .find_map(|line| line.strip_prefix("data: "))
                        .expect("event should contain data"),
                )
                .expect("event data should be JSON")
            })
            .collect::<Vec<_>>();
        let added = event_payloads
            .iter()
            .find(|payload| payload["type"] == "response.output_item.added")
            .expect("an output_item.added event should exist");
        assert_eq!(added["item"]["type"], "custom_tool_call");
        assert_eq!(added["item"]["name"], "exec");
        assert!(event_payloads
            .iter()
            .any(|payload| { payload["type"] == "response.custom_tool_call_input.delta" }));
        let completed_item = event_payloads
            .iter()
            .find(|payload| {
                payload["type"] == "response.output_item.done"
                    && payload["item"]["type"] == "custom_tool_call"
            })
            .expect("custom_tool_call output_item.done should exist");
        assert_eq!(completed_item["item"]["input"], "{\"cmd\":\"ls\"}");
    }

    #[test]
    fn streaming_late_name_undeclared_stays_function_call() {
        // A late-arriving name that is NOT a declared custom tool must keep
        // function_call semantics.
        let (mut state, initial) = ChatStreamState::new("route/model-a".into(), BTreeSet::new());
        let mut events = initial;
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_y"}]}}]
        })));
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"name": "lookup", "arguments": "{\"q\":\"x\"}"}}]}}]
        })));
        events.extend(state.finish());

        let event_payloads = events
            .iter()
            .map(|event| {
                serde_json::from_str::<Value>(
                    event
                        .lines()
                        .find_map(|line| line.strip_prefix("data: "))
                        .expect("event should contain data"),
                )
                .expect("event data should be JSON")
            })
            .collect::<Vec<_>>();
        let added = event_payloads
            .iter()
            .find(|payload| payload["type"] == "response.output_item.added")
            .expect("an output_item.added event should exist");
        assert_eq!(added["item"]["type"], "function_call");
        assert_eq!(added["item"]["name"], "lookup");
        assert!(event_payloads
            .iter()
            .any(|payload| { payload["type"] == "response.function_call_arguments.delta" }));
    }

    #[test]
    fn actionable_function_call_requires_complete_json_arguments() {
        let (mut state, _) = ChatStreamState::new("route/model-a".into(), BTreeSet::new());
        state.push_chat_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": "call_z",
                "function": {"name": "lookup", "arguments": "{\"q\":"}
            }]}}]
        }));
        assert!(!state.has_actionable_function_call());

        state.push_chat_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "function": {"arguments": "\"x\"}"}
            }]}}]
        }));
        assert!(state.has_actionable_function_call());
    }
}

