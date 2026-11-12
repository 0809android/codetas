use super::*;

#[test]
fn new_claude_models_get_adaptive_thinking_and_older_ones_keep_a_budget() {
    let request = |effort: &str| {
        json!({
            "input": [{"type": "message", "role": "user", "content": "hi"}],
            "max_output_tokens": 8192,
            "reasoning": {"effort": effort}
        })
    };
    for (model, effort, expected) in [
        ("claude-sonnet-5-5", "high", "high"),
        ("claude-sonnet-5-5", "xhigh", "high"),
        ("claude-opus-5-5", "xhigh", "xhigh"),
        ("anthropic-apikey/claude-opus-4-7", "medium", "medium"),
        ("claude-opus-5-5", "ultra", "max"),
    ] {
        let body = responses_to_anthropic(&request(effort), model).unwrap();
        assert_eq!(body["thinking"], json!({"type": "adaptive"}), "{model}");
        assert_eq!(
            body["output_config"]["effort"], expected,
            "{model} {effort}"
        );
        assert!(body["thinking"].get("budget_tokens").is_none());
    }
    for model in [
        "claude-sonnet-4-5",
        "claude-opus-4-6",
        "claude-3-7-sonnet",
        "gpt-test",
    ] {
        let body = responses_to_anthropic(&request("high"), model).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled", "{model}");
        assert!(body.get("output_config").is_none(), "{model}");
    }
    let none = responses_to_anthropic(&request("none"), "claude-sonnet-5-5").unwrap();
    assert!(none.get("thinking").is_none());
}

#[test]
fn adaptive_thinking_keeps_a_structured_output_format() {
    let body = responses_to_anthropic(
        &json!({
            "input": [{"type": "message", "role": "user", "content": "hi"}],
            "reasoning": {"effort": "low"},
            "text": {"format": {"type": "json_schema", "schema": {"type": "object"}}}
        }),
        "claude-sonnet-5-5",
    )
    .unwrap();
    assert_eq!(body["output_config"]["effort"], "low");
    assert_eq!(body["output_config"]["format"]["type"], "json_schema");
}

#[test]
fn closes_a_trailing_assistant_turn_with_a_user_turn() {
    // Anthropic answers this shape with 400:
    // "This model does not support assistant message prefill.
    //  The conversation must end with a user message."
    // Compaction replays the history it summarizes, which normally ends on
    // the assistant's last reply, so the closing user turn is required.
    let request = json!({
        "input": [
            {"type": "message", "role": "user", "content": "first"},
            {"type": "message", "role": "assistant", "content": "reply"}
        ]
    });
    let body = responses_to_anthropic(&request, "claude-sonnet-5")
        .expect("assistant-terminated history should translate");
    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3, "a closing user turn should be appended");
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"][0]["type"], "text");
    assert!(!messages[2]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .is_empty());
}

#[test]
fn leaves_a_user_terminated_conversation_unchanged() {
    let request = json!({
        "input": [
            {"type": "message", "role": "user", "content": "a"},
            {"type": "message", "role": "assistant", "content": "b"},
            {"type": "message", "role": "user", "content": "c"}
        ]
    });
    let body = responses_to_anthropic(&request, "claude-sonnet-5").expect("translate");
    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"][0]["text"], "c");
}

#[test]
fn closes_a_lone_assistant_turn() {
    let request = json!({
        "input": [{"type": "message", "role": "assistant", "content": "only"}]
    });
    let body = responses_to_anthropic(&request, "claude-sonnet-5").expect("translate");
    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1]["role"], "user");
}

#[test]
fn does_not_append_a_user_turn_to_an_empty_history() {
    let request = json!({"input": []});
    let body = responses_to_anthropic(&request, "claude-sonnet-5").expect("translate");
    assert_eq!(body["messages"].as_array().map(Vec::len), Some(0));
}

#[test]
fn subscription_oauth_wraps_system_and_prefixes_tools() {
    let request = json!({
        "instructions": "Be brief.",
        "input": [{"type": "message", "role": "user", "content": "hi"}],
        "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}}]
    });
    let body = responses_to_anthropic_with_oauth(&request, "claude-sonnet-5", true)
        .expect("oauth request");
    assert_eq!(body["system"][0]["text"], CLAUDE_CODE_SYSTEM_INSTRUCTION);
    assert_eq!(body["tools"][0]["name"], "custom_lookup");
    let provider = crate::config::ProviderDefinition {
        id: "anthropic".into(),
        protocol: crate::config::ProviderProtocol::AnthropicMessages,
        credential: crate::config::ProviderCredential {
            source: crate::config::CredentialSource::Environment,
            reference: Some("CLAUDE_CODE_OAUTH_TOKEN".into()),
            transport: crate::config::CredentialTransport::Bearer,
            ..crate::config::ProviderCredential::default()
        },
        ..crate::config::ProviderDefinition::default()
    };
    assert!(uses_anthropic_subscription_oauth(&provider));
}

#[test]
fn translates_function_tools_and_results() {
    let request = json!({
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]},
            {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"q\":\"a\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "done"}
        ],
        "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}}]
    });
    let translated =
        responses_to_anthropic(&request, "claude-test").expect("request should translate");
    assert_eq!(translated["model"], "claude-test");
    assert_eq!(translated["tools"][0]["name"], "lookup");
}

#[test]
fn carries_tool_result_images_as_anthropic_image_blocks() {
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
    let translated =
        responses_to_anthropic(&request, "claude-test").expect("request should translate");
    let content = &translated["messages"][1]["content"][0]["content"];
    assert_eq!(content[0]["text"], "rendered");
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["source"]["data"], "AA==");
}

#[test]
fn stringifies_structured_tool_result_output() {
    let request = json!({
        "input": [
            {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": {"count": 2, "ok": true}}
        ]
    });
    let translated =
        responses_to_anthropic(&request, "claude-test").expect("request should translate");
    assert_eq!(
        translated["messages"][1]["content"][0]["content"],
        "{\"count\":2,\"ok\":true}"
    );
}

#[test]
fn preserves_structured_custom_tool_input() {
    // A structured `input` (malformed client) must survive instead of
    // collapsing to {} — the tool_use block keeps the payload.
    let request = json!({
        "input": [
            {"type": "custom_tool_call", "call_id": "call_1", "name": "exec", "input": {"command": "ls"}}
        ]
    });
    let translated =
        responses_to_anthropic(&request, "claude-test").expect("request should translate");
    assert_eq!(
        translated["messages"][0]["content"][0]["input"],
        json!({"input": {"command": "ls"}})
    );
}

#[test]
fn flattens_namespace_tools_and_round_trips_the_identity() {
    let request = json!({
        "input": [{
            "type": "function_call",
            "call_id": "call_1",
            "namespace": "collaboration",
            "name": "send_message",
            "arguments": "{\"target\":\"/root\",\"message\":\"done\"}"
        }],
        "tools": [{
            "type": "namespace",
            "name": "collaboration",
            "description": "Agent coordination tools",
            "tools": [{
                "type": "function",
                "name": "send_message",
                "description": "Send a message",
                "inputSchema": {
                    "type": "object",
                    "properties": {"target": {"type": "string"}},
                    "required": ["target"]
                }
            }]
        }]
    });

    let translated = responses_to_anthropic(&request, "claude-test").expect("namespace request");
    assert_eq!(
        translated["tools"][0]["name"],
        "collaboration__send_message"
    );
    assert_eq!(
        translated["tools"][0]["input_schema"],
        request["tools"][0]["tools"][0]["inputSchema"]
    );
    assert_eq!(
        translated["messages"][0]["content"][0]["name"],
        "collaboration__send_message"
    );

    let upstream = json!({
        "content": [{
            "type": "tool_use",
            "id": "call_2",
            "name": "collaboration__send_message",
            "input": {"target": "/root", "message": "done"}
        }],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });
    let tool_map = response_tool_map(&request);
    let response = anthropic_to_response(&upstream, "claude-test", &tool_map).expect("response");
    assert_eq!(response["output"][0]["type"], "function_call");
    assert_eq!(response["output"][0]["name"], "send_message");
    assert_eq!(response["output"][0]["namespace"], "collaboration");
}

#[test]
fn exposes_custom_tools_and_unwraps_freeform_input() {
    let request = json!({
        "input": [{
            "type": "custom_tool_call",
            "call_id": "call_1",
            "name": "exec",
            "input": "const result = await tools.read(); text(result);"
        }],
        "tools": [{
            "type": "custom",
            "name": "exec",
            "description": "Run JavaScript against available tools",
            "format": {"type": "grammar", "syntax": "lark", "definition": "start: /.+/"}
        }]
    });

    let translated = responses_to_anthropic(&request, "claude-test").expect("custom tool request");
    assert_eq!(translated["tools"][0]["name"], "exec");
    assert_eq!(
        translated["tools"][0]["input_schema"]["properties"]["input"]["type"],
        "string"
    );
    assert_eq!(
        translated["messages"][0]["content"][0]["input"],
        json!({"input": "const result = await tools.read(); text(result);"})
    );

    let upstream = json!({
        "content": [{
            "type": "tool_use",
            "id": "call_2",
            "name": "exec",
            "input": {"input": "const result = await tools.read(); text(result);"}
        }],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });
    let tool_map = response_tool_map(&request);
    let response = anthropic_to_response(&upstream, "claude-test", &tool_map).expect("response");
    assert_eq!(response["output"][0]["type"], "custom_tool_call");
    assert_eq!(response["output"][0]["name"], "exec");
    assert_eq!(
        response["output"][0]["input"],
        "const result = await tools.read(); text(result);"
    );
}

#[test]
fn accepts_all_supported_function_schema_field_names() {
    let request = json!({
        "tools": [
            {
                "type": "function",
                "name": "parameters_tool",
                "parameters": {"type": "object", "required": ["a"]}
            },
            {
                "type": "function",
                "name": "snake_tool",
                "input_schema": {"type": "object", "required": ["b"]}
            },
            {
                "type": "function",
                "name": "camel_tool",
                "inputSchema": {"type": "object", "required": ["c"]}
            }
        ]
    });

    let translated = responses_to_anthropic(&request, "claude-test").expect("schema aliases");
    let tools = translated["tools"].as_array().expect("tools");
    let schema_for = |name: &str| {
        tools
            .iter()
            .find(|tool| tool["name"] == name)
            .map(|tool| tool["input_schema"].clone())
            .expect("translated tool")
    };
    assert_eq!(schema_for("parameters_tool")["required"][0], "a");
    assert_eq!(schema_for("snake_tool")["required"][0], "b");
    assert_eq!(schema_for("camel_tool")["required"][0], "c");
}

#[test]
fn assigns_reversible_names_to_flattened_namespace_collisions() {
    let request = json!({
        "input": [{
            "type": "function_call",
            "call_id": "call_1",
            "namespace": "plugins",
            "name": "lookup",
            "arguments": "{}"
        }],
        "tools": [
            {
                "type": "function",
                "name": "plugins__lookup",
                "parameters": {"type": "object"}
            },
            {
                "type": "namespace",
                "name": "plugins",
                "tools": [{
                    "type": "function",
                    "name": "lookup",
                    "inputSchema": {"type": "object"}
                }]
            }
        ]
    });

    let translated =
        responses_to_anthropic(&request, "claude-test").expect("colliding tools retained");
    let tools = translated["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["name"], "plugins__lookup");
    let namespaced_wire_name = tools[1]["name"].as_str().expect("wire name");
    assert!(namespaced_wire_name.starts_with("plugins__lookup__"));
    assert_eq!(
        translated["messages"][0]["content"][0]["name"],
        namespaced_wire_name
    );
}

#[test]
fn preserves_tool_search_kind_and_oauth_name_in_history_and_choice() {
    let request = json!({
        "input": [{
            "type": "tool_search_call",
            "call_id": "call_search",
            "arguments": {"query": "calendar"}
        }],
        "tools": [{"type": "tool_search"}],
        "tool_choice": {"type": "tool_search"}
    });

    let translated = responses_to_anthropic_with_oauth(&request, "claude-test", true)
        .expect("tool search request");
    assert_eq!(translated["tools"][0]["name"], "custom_tool_search");
    assert_eq!(
        translated["messages"][0]["content"][0]["name"],
        "custom_tool_search"
    );
    assert_eq!(translated["tool_choice"]["name"], "custom_tool_search");
}

#[test]
fn nonstream_custom_call_follows_the_stream_rule() {
    let tool_map = response_tool_map(&json!({"tools": [
        {"type": "custom", "name": "exec"},
        {"type": "custom", "name": "apply_patch"},
        {"type": "function", "name": "lookup", "parameters": {"type": "object"}}
    ]}));
    let patch = "*** Begin Patch\n*** Add File: a\n+ok\n*** End Patch\n";
    let tool_use = |name: &str, input: Value| json!({"type": "tool_use", "id": format!("call_{name}"), "name": name, "input": input});
    let respond = |content: Vec<Value>, stop_reason: &str| {
        anthropic_to_response(
            &json!({"content": content, "stop_reason": stop_reason}),
            "claude-test",
            &tool_map,
        )
        .expect("Anthropic response")
    };
    let custom_calls = |response: &Value| {
        response["output"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "custom_tool_call")
            .count()
    };

    for stop_reason in ["tool_use", "end_turn"] {
        let whole = respond(vec![tool_use("exec", json!({"input": "pwd"}))], stop_reason);
        assert_eq!(whole["status"], "completed");
        assert_eq!(whole["output"][0]["input"], "pwd");
    }
    for empty in [
        tool_use("exec", json!({})),
        tool_use("exec", json!({"input": ""})),
        tool_use("apply_patch", json!({})),
        tool_use("apply_patch", json!({"input": "*** Begin Patch\n+o"})),
    ] {
        let response = respond(vec![empty.clone()], "tool_use");
        assert_eq!(response["status"], "incomplete", "{empty}");
        assert_eq!(
            response["incomplete_details"]["reason"],
            "invalid_tool_call"
        );
        assert_eq!(custom_calls(&response), 0, "{empty}");
    }

    // An empty patch does not take a valid sibling or a real answer down with it.
    let sibling = respond(
        vec![
            tool_use("apply_patch", json!({})),
            tool_use("lookup", json!({})),
        ],
        "tool_use",
    );
    assert_eq!(sibling["status"], "completed");
    assert_eq!(sibling["output"].as_array().map(Vec::len), Some(1));
    let text = respond(
        vec![
            json!({"type": "text", "text": "Done."}),
            tool_use("apply_patch", json!({})),
        ],
        "end_turn",
    );
    assert_eq!(text["status"], "completed");
    assert_eq!(custom_calls(&text), 0);

    // A cut-off turn proves nothing but a patch's end marker. Its reason stays.
    for (stop_reason, reason) in [
        ("max_tokens", "max_output_tokens"),
        ("refusal", "content_filter"),
    ] {
        let cut = respond(vec![tool_use("exec", json!({"input": "pwd"}))], stop_reason);
        assert_eq!(cut["status"], "incomplete");
        assert_eq!(cut["incomplete_details"]["reason"], reason);
        assert_eq!(custom_calls(&cut), 0);
        let cut = respond(
            vec![tool_use("apply_patch", json!({"input": patch}))],
            stop_reason,
        );
        assert_eq!(cut["incomplete_details"]["reason"], reason);
        assert_eq!(cut["output"][0]["status"], "completed");
        assert_eq!(cut["output"][0]["input"], patch);
    }

    // A summarizer reads this with an empty tool map, where nothing is custom.
    let summarizer = anthropic_to_response(
        &json!({"content": [tool_use("exec", json!({}))], "stop_reason": "tool_use"}),
        "claude-test",
        &ResponseToolMap::default(),
    )
    .unwrap();
    assert_eq!(summarizer["status"], "completed");
    assert_eq!(summarizer["output"][0]["type"], "function_call");
}

#[test]
fn allowed_tools_filters_anthropic_declarations() {
    let request = json!({
        "tools": [
            {"type": "function", "name": "lookup", "parameters": {"type": "object"}},
            {"type": "custom", "name": "exec"}
        ],
        "tool_choice": {
            "type": "allowed_tools",
            "mode": "required",
            "tools": [{"type": "custom", "name": "exec"}]
        }
    });

    let translated =
        responses_to_anthropic(&request, "claude-test").expect("allowed tools request");
    assert_eq!(translated["tools"].as_array().map(Vec::len), Some(1));
    assert_eq!(translated["tools"][0]["name"], "exec");
    assert_eq!(translated["tool_choice"]["type"], "any");
}

#[test]
fn oauth_prefix_round_trip_preserves_names_already_starting_with_custom() {
    let request = json!({
        "tools": [{
            "type": "function",
            "name": "custom_lookup",
            "parameters": {"type": "object"}
        }]
    });
    let translated = responses_to_anthropic_with_oauth(&request, "claude-test", true)
        .expect("oauth tool request");
    assert_eq!(translated["tools"][0]["name"], "custom_custom_lookup");

    let upstream = json!({
        "content": [{
            "type": "tool_use",
            "id": "call_1",
            "name": "custom_custom_lookup",
            "input": {}
        }],
        "usage": {"input_tokens": 1, "output_tokens": 1}
    });
    let response = anthropic_to_response_with_oauth(
        &upstream,
        "claude-test",
        &response_tool_map(&request),
        true,
    )
    .expect("oauth tool response");
    assert_eq!(response["output"][0]["name"], "custom_lookup");
}

#[test]
fn non_oauth_response_preserves_names_starting_with_custom() {
    let request = json!({
        "tools": [{
            "type": "function",
            "name": "custom_lookup",
            "parameters": {"type": "object"}
        }]
    });
    let upstream = json!({
        "content": [{
            "type": "tool_use",
            "id": "call_1",
            "name": "custom_lookup",
            "input": {}
        }],
        "usage": {"input_tokens": 1, "output_tokens": 1}
    });

    let response = anthropic_to_response(&upstream, "claude-test", &response_tool_map(&request))
        .expect("ordinary Anthropic tool response");
    assert_eq!(response["output"][0]["name"], "custom_lookup");
}

#[test]
fn oauth_response_disambiguates_prefixed_and_real_custom_names() {
    let request = json!({
        "tools": [
            {"type": "function", "name": "lookup", "parameters": {"type": "object"}},
            {"type": "function", "name": "custom_lookup", "parameters": {"type": "object"}}
        ]
    });
    let tool_map = response_tool_map(&request);
    let lookup = anthropic_to_response_with_oauth(
        &json!({
            "content": [{
                "type": "tool_use",
                "id": "call_1",
                "name": "custom_lookup",
                "input": {}
            }],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }),
        "claude-test",
        &tool_map,
        true,
    )
    .expect("prefixed lookup response");
    assert_eq!(lookup["output"][0]["name"], "lookup");

    let custom_lookup = anthropic_to_response_with_oauth(
        &json!({
            "content": [{
                "type": "tool_use",
                "id": "call_2",
                "name": "custom_custom_lookup",
                "input": {}
            }],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }),
        "claude-test",
        &tool_map,
        true,
    )
    .expect("double-prefixed custom lookup response");
    assert_eq!(custom_lookup["output"][0]["name"], "custom_lookup");
}

#[test]
fn stream_only_strips_custom_prefix_for_subscription_oauth() {
    let event = json!({
        "type": "content_block_start",
        "index": 0,
        "content_block": {
            "type": "tool_use",
            "id": "call_1",
            "name": "custom_custom_lookup",
            "input": {}
        }
    });
    let mut stream_state = AnthropicStreamState::default();
    let ordinary = anthropic_stream_to_chat(&event, &mut stream_state, false)
        .expect("ordinary stream event")
        .expect("ordinary stream chunk");
    assert_eq!(
        ordinary["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "custom_custom_lookup"
    );

    let oauth = anthropic_stream_to_chat(&event, &mut stream_state, true)
        .expect("OAuth stream event")
        .expect("OAuth stream chunk");
    assert_eq!(
        oauth["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "custom_lookup"
    );
}

#[test]
fn parallel_tool_results_stay_first_when_a_notice_sits_between_them() {
    let messages = vec![
        json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "a", "name": "js", "input": {}},
            {"type": "tool_use", "id": "b", "name": "exec", "input": {}}
        ]}),
        json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a", "content": "one"}]}),
        json!({"role": "user", "content": [{"type": "text", "text": "<image_resize_notice>"}]}),
        json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "b", "content": "two"}]}),
    ];
    let merged = merge_adjacent_messages(messages);
    assert_eq!(merged.len(), 2);
    let kinds: Vec<_> = merged[1]["content"]
        .as_array()
        .expect("content")
        .iter()
        .map(|part| part["type"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(kinds, ["tool_result", "tool_result", "text"]);
    assert_eq!(merged[1]["content"][0]["tool_use_id"], "a");
    assert_eq!(merged[1]["content"][1]["tool_use_id"], "b");

    // Messages without results, or already ordered, are untouched.
    let plain = merge_adjacent_messages(vec![
        json!({"role": "user", "content": "hi"}),
        json!({"role": "user", "content": [{"type": "text", "text": "there"}]}),
    ]);
    assert_eq!(plain[0]["content"].as_array().map(Vec::len), Some(2));
}

#[test]
fn stream_parameterless_tool_use_finishes_with_empty_object_arguments() {
    let mut state = AnthropicStreamState::default();
    let step = |state: &mut AnthropicStreamState, event: Value| {
        anthropic_stream_to_chat(&event, state, false).expect("stream event")
    };
    step(
        &mut state,
        json!({
            "type": "content_block_start", "index": 1,
            "content_block": {"type": "tool_use", "id": "call_1", "name": "list_projects", "input": {}}
        }),
    );
    // An empty partial_json is not input and must not suppress the "{}" fallback.
    step(
        &mut state,
        json!({
            "type": "content_block_delta", "index": 1,
            "delta": {"type": "input_json_delta", "partial_json": ""}
        }),
    );
    let stop = step(
        &mut state,
        json!({"type": "content_block_stop", "index": 1}),
    )
    .expect("closing chunk for a parameterless tool call");
    assert_eq!(
        stop["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
        "{}"
    );

    // A tool call that streamed real input gets no extra arguments chunk.
    step(
        &mut state,
        json!({
            "type": "content_block_start", "index": 2,
            "content_block": {"type": "tool_use", "id": "call_2", "name": "lookup", "input": {}}
        }),
    );
    step(
        &mut state,
        json!({
            "type": "content_block_delta", "index": 2,
            "delta": {"type": "input_json_delta", "partial_json": "{\"q\":1}"}
        }),
    );
    assert!(step(
        &mut state,
        json!({"type": "content_block_stop", "index": 2})
    )
    .is_none());
}

#[test]
fn stream_redacted_thinking_is_emitted_as_verbatim_provider_metadata() {
    let start = json!({
        "type": "content_block_start",
        "index": 3,
        "content_block": {"type": "redacted_thinking", "data": "OPAQUE1"}
    });
    let mut stream_state = AnthropicStreamState::default();

    assert!(anthropic_stream_to_chat(&start, &mut stream_state, false)
        .expect("redacted block start")
        .is_none());
    let chunk = anthropic_stream_to_chat(
        &json!({"type": "content_block_stop", "index": 3}),
        &mut stream_state,
        false,
    )
    .expect("redacted block stop")
    .expect("redacted metadata chunk");

    assert_eq!(
        chunk["choices"][0]["delta"]["codetas_anthropic_thinking_block"],
        json!({"type": "redacted_thinking", "data": "OPAQUE1"})
    );
}

#[test]
fn stream_thinking_blocks_keep_independent_text_and_signatures() {
    let mut state = AnthropicStreamState::default();
    let mut completed = Vec::new();
    let events = [
        json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "redacted_thinking", "data": "OPAQUE1"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1,
                "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        json!({"type": "content_block_delta", "index": 1,
                "delta": {"type": "thinking_delta", "thinking": "first"}}),
        json!({"type": "content_block_delta", "index": 1,
                "delta": {"type": "signature_delta", "signature": "sig-1"}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "content_block_start", "index": 2,
                "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        json!({"type": "content_block_delta", "index": 2,
                "delta": {"type": "thinking_delta", "thinking": "second"}}),
        json!({"type": "content_block_delta", "index": 2,
                "delta": {"type": "signature_delta", "signature": "sig-2"}}),
        json!({"type": "content_block_stop", "index": 2}),
    ];

    for event in events {
        if let Some(chunk) =
            anthropic_stream_to_chat(&event, &mut state, false).expect("Anthropic stream event")
        {
            if let Some(block) = chunk
                .pointer("/choices/0/delta/codetas_anthropic_thinking_block")
                .cloned()
            {
                completed.push(block);
            }
        }
    }

    assert_eq!(
        completed,
        vec![
            json!({"type": "redacted_thinking", "data": "OPAQUE1"}),
            json!({"type": "thinking", "thinking": "first", "signature": "sig-1"}),
            json!({"type": "thinking", "thinking": "second", "signature": "sig-2"}),
        ]
    );
}

#[test]
fn stream_single_thinking_block_is_finalized_verbatim() {
    let mut state = AnthropicStreamState::default();
    for event in [
        json!({"type": "content_block_start", "index": 4,
                "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        json!({"type": "content_block_delta", "index": 4,
                "delta": {"type": "thinking_delta", "thinking": "one thought"}}),
        json!({"type": "content_block_delta", "index": 4,
                "delta": {"type": "signature_delta", "signature": "sig-one"}}),
    ] {
        anthropic_stream_to_chat(&event, &mut state, false).expect("thinking event");
    }

    let chunk = anthropic_stream_to_chat(
        &json!({"type": "content_block_stop", "index": 4}),
        &mut state,
        false,
    )
    .expect("thinking stop")
    .expect("finalized thinking block");
    assert_eq!(
        chunk["choices"][0]["delta"]["codetas_anthropic_thinking_block"],
        json!({"type": "thinking", "thinking": "one thought", "signature": "sig-one"})
    );
}

#[test]
fn stream_reasoning_deltas_are_scoped_to_thinking_blocks() {
    let mut stream_state = AnthropicStreamState::default();
    anthropic_stream_to_chat(
        &json!({
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}
        }),
        &mut stream_state,
        false,
    )
    .expect("text block start");

    let stray = anthropic_stream_to_chat(
        &json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "signature_delta", "signature": "STRAY"}
        }),
        &mut stream_state,
        false,
    )
    .expect("stray signature event");

    assert!(stray.is_none());
}

#[test]
fn stream_terminal_reason_preserves_incomplete_disposition() {
    let mut state = AnthropicStreamState::default();
    let chunk = anthropic_stream_to_chat(
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": "max_tokens"},
            "usage": {"output_tokens": 9}
        }),
        &mut state,
        false,
    )
    .expect("terminal chunk")
    .expect("translated terminal");
    assert_eq!(chunk["choices"][0]["finish_reason"], "length");

    let filtered = anthropic_stream_to_chat(
        &json!({"type": "message_delta", "delta": {"stop_reason": "refusal"}}),
        &mut state,
        false,
    )
    .expect("filter terminal")
    .expect("translated filter terminal");
    assert_eq!(filtered["choices"][0]["finish_reason"], "content_filter");
}
#[test]
fn cache_reads_and_writes_count_toward_the_prompt_and_report_cached_tokens() {
    // Anthropic reports only the uncached tail as `input_tokens`.
    let usage = json!({
        "input_tokens": 12,
        "cache_read_input_tokens": 9000,
        "cache_creation_input_tokens": 300,
        "output_tokens": 40
    });
    let converted = anthropic_usage(Some(&usage));
    assert_eq!(converted["input_tokens"], 9312);
    assert_eq!(converted["input_tokens_details"]["cached_tokens"], 9000);
    assert_eq!(converted["total_tokens"], 9352);
    // Without caching the numbers are unchanged.
    let plain = anthropic_usage(Some(&json!({"input_tokens": 100, "output_tokens": 5})));
    assert_eq!(plain["input_tokens"], 100);
    assert!(plain["input_tokens_details"].get("cached_tokens").is_none());
}

#[test]
fn stream_usage_includes_cache_counters_from_start_and_final_delta() {
    let mut state = AnthropicStreamState::default();
    anthropic_stream_to_chat(
        &json!({"type": "message_start", "message": {"usage": {
            "input_tokens": 20,
            "cache_read_input_tokens": 5000,
            "cache_creation_input_tokens": 100
        }}}),
        &mut state,
        false,
    )
    .expect("message start");
    let chunk = anthropic_stream_to_chat(
        &json!({"type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 7}}),
        &mut state,
        false,
    )
    .expect("message delta")
    .expect("usage chunk");
    assert_eq!(chunk["usage"]["prompt_tokens"], 5120);
    assert_eq!(
        chunk["usage"]["prompt_tokens_details"]["cached_tokens"],
        5000
    );
    assert_eq!(chunk["usage"]["total_tokens"], 5127);

    // Cumulative counters in the final delta replace the start values.
    let chunk = anthropic_stream_to_chat(
            &json!({"type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"input_tokens": 30, "cache_read_input_tokens": 6000, "cache_creation_input_tokens": 0, "output_tokens": 9}}),
            &mut state,
            false,
        )
        .expect("message delta")
        .expect("usage chunk");
    assert_eq!(chunk["usage"]["prompt_tokens"], 6030);
    assert_eq!(
        chunk["usage"]["prompt_tokens_details"]["cached_tokens"],
        6000
    );
}
