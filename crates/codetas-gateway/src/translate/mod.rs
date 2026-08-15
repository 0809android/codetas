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
pub fn prepare_translated_responses_request(
    body: &mut Value,
    strip_stateful: bool,
    preserve_tool_search: bool,
) {
    expand_local_compactions(body);
    if strip_stateful {
        if let Some(object) = body.as_object_mut() {
            object.remove("previous_response_id");
            object.remove("conversation");
        }
    }

    let extra_tools = collect_dynamic_tools(body);
    if !extra_tools.is_empty() {
        let Some(object) = body.as_object_mut() else {
            return;
        };
        if object.get("tools").is_none_or(Value::is_null) {
            object.insert("tools".into(), json!([]));
        }
        if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
            for tool in extra_tools {
                if !tools.iter().any(|existing| existing == &tool) {
                    tools.push(tool);
                }
            }
        }
    }

    let tool_map = response_tool_map(body);

    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    let mut rewritten = Vec::with_capacity(items.len());
    for item in items.iter() {
        if let Some(converted) =
            rewrite_translated_input_item(item, preserve_tool_search, &tool_map)
        {
            rewritten.push(converted);
        }
    }
    *items = rewritten;
    repair_translated_input_items(body);
}

fn collect_dynamic_tools(body: &Value) -> Vec<Value> {
    let Some(items) = body.get("input").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|item| {
            matches!(
                item.get("type").and_then(Value::as_str),
                Some("additional_tools" | "tool_search_output")
            )
        })
        .flat_map(|item| {
            item.get("tools")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .collect()
}

fn rewrite_translated_input_item(
    item: &Value,
    preserve_tool_search: bool,
    tool_map: &ResponseToolMap,
) -> Option<Value> {
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
            if preserve_tool_search {
                return Some(item.clone());
            }
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
            if preserve_tool_search {
                return Some(item.clone());
            }
            let call_id = item.get("call_id").cloned().unwrap_or_else(|| json!(""));
            let text = tool_search_output_to_text(item, tool_map);
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
        prepare_translated_responses_request(&mut request, true, true);
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
    fn reinjects_tool_search_results_and_lists_namespaced_wire_names() {
        let mut request = json!({
            "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}}],
            "input": [
                {"type": "tool_search_call", "call_id": "call_search", "arguments": {"query": "calendar"}},
                {"type": "tool_search_output", "call_id": "call_search", "tools": [
                    {"type": "function", "name": "lookup", "parameters": {"type": "object"}},
                    {"type": "namespace", "name": "calendar", "tools": [
                        {"type": "function", "name": "create_event", "inputSchema": {"type": "object"}}
                    ]}
                ]}
            ]
        });

        prepare_translated_responses_request(&mut request, true, true);
        assert_eq!(request["tools"].as_array().map(Vec::len), Some(2));
        assert_eq!(request["tools"][1]["type"], "namespace");
        assert_eq!(request["input"][0]["type"], "tool_search_call");
        assert_eq!(request["input"][1]["type"], "tool_search_output");

        let chat = responses_to_chat(&request, "model-a").expect("tool search history");
        assert_eq!(
            chat["messages"][0]["tool_calls"][0]["function"]["name"],
            "tool_search"
        );
        assert!(chat["messages"][1]["content"]
            .as_str()
            .is_some_and(|text| text.contains("calendar__create_event")));
    }

    #[test]
    fn reinjects_dynamic_tools_when_top_level_tools_is_null() {
        let mut request = json!({
            "tools": Value::Null,
            "input": [{
                "type": "additional_tools",
                "tools": [{
                    "type": "function",
                    "name": "exec",
                    "parameters": {"type": "object"}
                }]
            }]
        });

        prepare_translated_responses_request(&mut request, true, true);
        assert_eq!(request["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(request["tools"][0]["name"], "exec");
    }

    #[test]
    fn rewrites_tool_search_only_for_legacy_kiro_translation() {
        let mut request = json!({
            "input": [
                {"type": "tool_search_call", "call_id": "call_search", "arguments": {"query": "calendar"}},
                {"type": "tool_search_output", "call_id": "call_search", "tools": [
                    {"type": "function", "name": "create_event", "parameters": {"type": "object"}}
                ]}
            ]
        });

        prepare_translated_responses_request(&mut request, false, false);
        assert_eq!(request["input"][0]["type"], "function_call");
        assert_eq!(request["input"][0]["name"], "tool_search");
        assert_eq!(request["input"][1]["type"], "function_call_output");
        assert!(request["input"][1]["output"]
            .as_str()
            .is_some_and(|text| text.contains("create_event")));
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
    fn converts_custom_and_omits_hosted_tools_for_chat_adapter() {
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
        assert_eq!(chat["tools"].as_array().map(Vec::len), Some(2));
        assert_eq!(chat["tools"][0]["function"]["name"], "apply_patch");
        assert_eq!(
            chat["tools"][0]["function"]["parameters"]["required"][0],
            "input"
        );
        assert_eq!(chat["tools"][1]["function"]["name"], "lookup");
    }

    #[test]
    fn flattens_namespace_tools_and_restores_response_identity() {
        let request = json!({
            "input": [{
                "type": "function_call",
                "call_id": "call_1",
                "name": "send_message",
                "namespace": "collaboration",
                "arguments": "{\"message\":\"ping\"}"
            }],
            "tools": [{
                "type": "namespace",
                "name": "collaboration",
                "tools": [{
                    "type": "function",
                    "name": "send_message",
                    "description": "Send a message",
                    "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}}}
                }]
            }],
            "tool_choice": {"type": "function", "name": "send_message", "namespace": "collaboration"}
        });
        let chat = responses_to_chat(&request, "model-a").expect("namespace should flatten");
        assert_eq!(chat["tools"][0]["function"]["name"], "collaboration__send_message");
        assert_eq!(
            chat["tools"][0]["function"]["parameters"]["properties"]["message"]["type"],
            "string"
        );
        assert_eq!(
            chat["messages"][0]["tool_calls"][0]["function"]["name"],
            "collaboration__send_message"
        );
        assert_eq!(
            chat["tool_choice"]["function"]["name"],
            "collaboration__send_message"
        );

        let upstream = json!({
            "choices": [{"message": {"role": "assistant", "tool_calls": [{
                "id": "call_2",
                "type": "function",
                "function": {"name": "collaboration__send_message", "arguments": "{\"message\":\"pong\"}"}
            }]}}]
        });
        let response = chat_to_response(&upstream, "route/model-a", &response_tool_map(&request))
            .expect("namespace should restore");
        assert_eq!(response["output"][0]["name"], "send_message");
        assert_eq!(response["output"][0]["namespace"], "collaboration");
    }

    #[test]
    fn converts_tool_search_to_client_execution_call() {
        let request = json!({"tools": [{"type": "tool_search"}]});
        let chat_request = responses_to_chat(&request, "model-a").expect("tool search should map");
        assert_eq!(chat_request["tools"][0]["function"]["name"], "tool_search");

        let upstream = json!({
            "choices": [{"message": {"role": "assistant", "tool_calls": [{
                "id": "call_search",
                "type": "function",
                "function": {"name": "tool_search", "arguments": "{\"query\":\"calendar\"}"}
            }]}}]
        });
        let response = chat_to_response(&upstream, "route/model-a", &response_tool_map(&request))
            .expect("tool search response should map");
        assert_eq!(response["output"][0]["type"], "tool_search_call");
        assert_eq!(response["output"][0]["execution"], "client");
        assert_eq!(response["output"][0]["arguments"]["query"], "calendar");
    }

    #[test]
    fn allowed_tools_filters_chat_declarations_and_uses_standard_choice() {
        let request = json!({
            "tools": [
                {"type": "function", "name": "lookup", "parameters": {"type": "object"}},
                {"type": "namespace", "name": "calendar", "tools": [{
                    "type": "function",
                    "name": "create_event",
                    "inputSchema": {"type": "object"}
                }]},
                {"type": "custom", "name": "exec"}
            ],
            "tool_choice": {
                "type": "allowed_tools",
                "mode": "required",
                "tools": [{
                    "type": "function",
                    "name": "create_event",
                    "namespace": "calendar"
                }]
            }
        });

        let chat = responses_to_chat(&request, "model-a").expect("allowed tools request");
        let tools = chat["tools"].as_array().expect("filtered tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "calendar__create_event");
        assert_eq!(chat["tool_choice"], "required");
    }

    #[test]
    fn assigns_reversible_suffix_to_flattened_name_collisions() {
        let request = json!({
            "tools": [
                {"type": "function", "name": "plugins__lookup", "parameters": {"type": "object"}},
                {"type": "namespace", "name": "plugins", "tools": [
                    {"type": "function", "name": "lookup", "parameters": {"type": "object"}}
                ]}
            ]
        });
        let tool_map = response_tool_map(&request);
        let names = tool_map
            .iter()
            .map(|(wire_name, _, _)| wire_name.to_string())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 2);
        assert_eq!(names[0], "plugins__lookup");
        assert!(names[1].starts_with("plugins__lookup__"));
        assert_eq!(tool_map.identity(&names[0]).and_then(|item| item.namespace.as_deref()), None);
        assert_eq!(
            tool_map.identity(&names[1]).and_then(|item| item.namespace.as_deref()),
            Some("plugins")
        );
        let summary = tool_search_output_to_text(
            &json!({
                "tools": [{
                    "type": "namespace",
                    "name": "plugins",
                    "tools": [{
                        "type": "function",
                        "name": "lookup",
                        "parameters": {"type": "object"}
                    }]
                }]
            }),
            &tool_map,
        );
        assert!(summary.contains(&names[1]));
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
    fn keeps_reasoning_on_tool_search_calls() {
        let mut request = json!({
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Find a calendar tool"}]
                },
                {
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "I should load the calendar connector."}]
                },
                {
                    "type": "tool_search_call",
                    "call_id": "call_search",
                    "arguments": {"query": "calendar"}
                },
                {
                    "type": "tool_search_output",
                    "call_id": "call_search",
                    "tools": []
                }
            ],
            "tools": [{"type": "tool_search"}]
        });

        normalize_chat_reasoning_history(&mut request, true);
        let chat = responses_to_chat(&request, "deepseek-v4-flash")
            .expect("tool search history should translate");
        assert_eq!(
            chat["messages"][1]["reasoning_content"],
            "I should load the calendar connector."
        );
        assert_eq!(
            chat["messages"][1]["tool_calls"][0]["function"]["name"],
            "tool_search"
        );
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
        assert_eq!(chat["messages"][0]["content"], "I cannot help with that.");
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
        let response = chat_to_response(&chat, "route/model-a", &ResponseToolMap::default())
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
            "{\"input\":\"const r = await tools.list_dir(); text(r);\"}"
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
                    "function": {"name": "exec", "arguments": "{\"input\":\"pwd\"}"}
                }]
            }}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6}
        });
        let tool_map = response_tool_map(&json!({
            "tools": [
                {"type": "custom", "name": "exec"},
                {"type": "custom", "name": "apply_patch"}
            ]
        }));
        let response = chat_to_response(&chat, "route/model-a", &tool_map)
            .expect("response should translate");
        assert_eq!(response["output"][0]["type"], "custom_tool_call");
        assert_eq!(response["output"][0]["name"], "exec");
        assert_eq!(response["output"][0]["input"], "pwd");
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
        let response = chat_to_response(&chat, "route/model-a", &ResponseToolMap::default())
            .expect("response should translate");
        assert_eq!(response["output"][0]["type"], "function_call");
        assert_eq!(response["output"][0]["arguments"], "{\"q\":\"x\"}");
    }

    #[test]
    fn streaming_events_have_monotonic_sequence_numbers() {
        let (mut state, initial) =
            ChatStreamState::new("route/model-a".into(), ResponseToolMap::default());
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
            "{\"input\":{\"command\":\"ls\",\"cwd\":\"/tmp\"}}"
        );
    }

    #[test]
    fn streaming_classifies_late_name_as_custom_tool() {
        // Some Chat providers stream `{"index":0,"id":"call_x"}` first and the
        // tool name in a later chunk. The custom-tool classification must be
        // re-evaluated when the name arrives, not frozen at the first chunk.
        let tool_map = response_tool_map(&json!({
            "tools": [{"type": "custom", "name": "exec"}]
        }));
        let (mut state, initial) = ChatStreamState::new("route/model-a".into(), tool_map);
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
    fn streaming_treats_an_empty_initial_tool_name_as_unannounced() {
        let tool_map = response_tool_map(&json!({
            "tools": [{"type": "custom", "name": "exec"}]
        }));
        let (mut state, _) = ChatStreamState::new("route/model-a".into(), tool_map);
        let initial = state.push_chat_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": "call_empty_name",
                "function": {"name": "", "arguments": "{\"input\":\"pwd\"}"}
            }]}}]
        }));
        assert!(!event_payloads(&initial)
            .iter()
            .any(|payload| payload["type"] == "response.output_item.added"));

        let named = state.push_chat_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "function": {"name": "exec"}
            }]}}]
        }));
        let added = event_payloads(&named)
            .into_iter()
            .find(|payload| payload["type"] == "response.output_item.added")
            .expect("custom tool should be announced after its name arrives");
        assert_eq!(added["item"]["type"], "custom_tool_call");
        assert_eq!(added["item"]["name"], "exec");
        assert_eq!(added["item"]["input"], "");
    }

    #[test]
    fn streaming_late_name_undeclared_stays_function_call() {
        // A late-arriving name that is NOT a declared custom tool must keep
        // function_call semantics.
        let (mut state, initial) =
            ChatStreamState::new("route/model-a".into(), ResponseToolMap::default());
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
    fn streaming_late_custom_name_does_not_announce_partial_wrapper() {
        let tool_map = response_tool_map(&json!({
            "tools": [{"type": "custom", "name": "exec"}]
        }));
        let (mut state, _) = ChatStreamState::new("route/model-a".into(), tool_map);
        state.push_chat_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": "call_partial",
                "function": {"arguments": "{\"input\":\"hel"}
            }]}}]
        }));
        let events = state.push_chat_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "function": {"name": "exec"}
            }]}}]
        }));

        let added = event_payloads(&events)
            .into_iter()
            .find(|payload| payload["type"] == "response.output_item.added")
            .expect("custom item should be announced");
        assert_eq!(added["item"]["type"], "custom_tool_call");
        assert_eq!(added["item"]["input"], "");
    }

    #[test]
    fn actionable_function_call_requires_complete_json_arguments() {
        let (mut state, _) =
            ChatStreamState::new("route/model-a".into(), ResponseToolMap::default());
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

    #[test]
    fn streaming_ignores_empty_content_for_tool_only_turns() {
        let (mut state, initial) =
            ChatStreamState::new("route/model-a".into(), ResponseToolMap::default());
        let mut events = initial;
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"content": ""}}]
        })));
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": "call_empty",
                "function": {"name": "lookup", "arguments": "{}"}
            }]}}]
        })));
        events.extend(state.finish());

        let payloads = event_payloads(&events);
        assert!(!payloads.iter().any(|payload| {
            matches!(
                payload["type"].as_str(),
                Some("response.output_text.delta" | "response.output_text.done")
            )
        }));
        let completed = payloads
            .iter()
            .find(|payload| payload["type"] == "response.completed")
            .expect("response.completed should be emitted");
        assert!(completed["response"]["output"]
            .as_array()
            .is_some_and(|output| output.iter().all(|item| item["type"] != "message")));
    }

    #[test]
    fn streaming_emits_one_safe_progress_message_for_selected_tool_only_turn() {
        let policy = ToolProgressPolicy {
            emit_on_tool_call: true,
        };
        let (mut state, initial) = ChatStreamState::new_with_progress(
            "route/model-a".into(),
            ResponseToolMap::default(),
            policy,
        );
        let mut events = initial;
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"reasoning_content": "private reasoning"}}]
        })));
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"content": "", "tool_calls": [{
                "index": 0,
                "id": "call_progress",
                "function": {"name": "lookup", "arguments": "{}"}
            }]}}]
        })));
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 1,
                "id": "call_parallel",
                "function": {"name": "lookup", "arguments": "{}"}
            }]}}]
        })));
        events.extend(state.finish());

        let payloads = event_payloads(&events);
        let progress = payloads
            .iter()
            .filter(|payload| payload["type"] == "response.output_text.delta")
            .collect::<Vec<_>>();
        assert_eq!(progress.len(), 1);
        assert_eq!(progress[0]["delta"], TOOL_PROGRESS_MESSAGE);
        assert_ne!(progress[0]["delta"], "private reasoning");
        let progress_index = payloads
            .iter()
            .position(|payload| payload["type"] == "response.output_text.delta")
            .expect("progress delta should be emitted");
        let arguments_index = payloads
            .iter()
            .position(|payload| payload["type"] == "response.function_call_arguments.delta")
            .expect("function arguments delta should be emitted");
        assert!(progress_index < arguments_index);
    }

    #[test]
    fn streaming_real_content_suppresses_synthetic_progress() {
        let policy = ToolProgressPolicy {
            emit_on_tool_call: true,
        };
        let (mut state, initial) = ChatStreamState::new_with_progress(
            "route/model-a".into(),
            ResponseToolMap::default(),
            policy,
        );
        let mut events = initial;
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"content": "Checking files."}}]
        })));
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": "call_visible",
                "function": {"name": "lookup", "arguments": "{}"}
            }]}}]
        })));
        events.extend(state.finish());

        let deltas = event_payloads(&events)
            .into_iter()
            .filter(|payload| payload["type"] == "response.output_text.delta")
            .map(|payload| payload["delta"].as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>();
        assert_eq!(deltas, vec!["Checking files."]);
    }

    #[test]
    fn streaming_whitespace_content_does_not_suppress_synthetic_progress() {
        let policy = ToolProgressPolicy {
            emit_on_tool_call: true,
        };
        let (mut state, initial) = ChatStreamState::new_with_progress(
            "route/model-a".into(),
            ResponseToolMap::default(),
            policy,
        );
        let mut events = initial;
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"content": "  "}}]
        })));
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": "call_whitespace",
                "function": {"name": "lookup", "arguments": "{}"}
            }]}}]
        })));
        events.extend(state.finish());

        let deltas = event_payloads(&events)
            .into_iter()
            .filter(|payload| payload["type"] == "response.output_text.delta")
            .map(|payload| payload["delta"].as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            deltas,
            vec!["  ".to_string(), TOOL_PROGRESS_MESSAGE.to_string()]
        );
    }

    #[test]
    fn streaming_separates_real_content_after_synthetic_progress() {
        let policy = ToolProgressPolicy {
            emit_on_tool_call: true,
        };
        let (mut state, initial) = ChatStreamState::new_with_progress(
            "route/model-a".into(),
            ResponseToolMap::default(),
            policy,
        );
        let mut events = initial;
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": "call_progress_then_text",
                "function": {"name": "lookup", "arguments": "{}"}
            }]}}]
        })));
        events.extend(state.push_chat_chunk(&json!({
            "choices": [{"delta": {"content": "Finished."}}]
        })));
        events.extend(state.finish());

        let completed = event_payloads(&events)
            .into_iter()
            .find(|payload| payload["type"] == "response.completed")
            .expect("response.completed should be emitted");
        let message = completed["response"]["output"]
            .as_array()
            .and_then(|output| output.iter().find(|item| item["type"] == "message"))
            .expect("completed response should include a message");
        assert_eq!(
            message["content"][0]["text"],
            format!("{TOOL_PROGRESS_MESSAGE}\n\nFinished.")
        );
    }

    #[test]
    fn tool_progress_policy_emits_immediately_then_at_bounded_intervals() {
        let first = ToolProgressPolicy::from_request(&json!({
            "input": [{"type": "message", "role": "user", "content": "do work"}]
        }));
        assert!(first.emit_on_tool_call);

        let mut input = vec![
            json!({"type": "message", "role": "user", "content": "do work"}),
            json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": TOOL_PROGRESS_MESSAGE}]}),
        ];
        for index in 0..TOOL_PROGRESS_INTERVAL - 2 {
            input.push(json!({
                "type": "function_call",
                "call_id": format!("call_{index}"),
                "name": "lookup",
                "arguments": "{}"
            }));
            input.push(json!({
                "type": "function_call_output",
                "call_id": format!("call_{index}"),
                "output": "ok"
            }));
        }
        let suppressed = ToolProgressPolicy::from_request(&json!({"input": input.clone()}));
        assert!(!suppressed.emit_on_tool_call);

        let index = TOOL_PROGRESS_INTERVAL - 2;
        input.push(json!({
            "type": "function_call",
            "call_id": format!("call_{index}"),
            "name": "lookup",
            "arguments": "{}"
        }));
        input.push(json!({
            "type": "function_call_output",
            "call_id": format!("call_{index}"),
            "output": "ok"
        }));
        let due = ToolProgressPolicy::from_request(&json!({"input": input}));
        assert!(due.emit_on_tool_call);
    }

    #[test]
    fn tool_progress_policy_ignores_whitespace_only_assistant_messages() {
        let policy = ToolProgressPolicy::from_request(&json!({
            "input": [
                {"type": "message", "role": "user", "content": "do work"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "  "}]}
            ]
        }));
        assert!(policy.emit_on_tool_call);
    }

    fn event_payloads(events: &[String]) -> Vec<Value> {
        events
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
            .collect()
    }
}
