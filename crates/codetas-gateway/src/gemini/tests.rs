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

    let translated = responses_to_gemini(&request, "gemini-test").expect("allowed tools request");
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
    let (mut state, _) =
        crate::translate::ChatStreamState::new("gemini-test".into(), ResponseToolMap::default());
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
    let arguments: Value = serde_json::from_str(output[0]["arguments"].as_str().unwrap()).unwrap();
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
