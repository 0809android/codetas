use super::*;

#[test]
fn image_rejection_classifier_ignores_non_image_invalid_requests() {
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"message":"The image is too large","type":"invalid_request_error"}}"#,
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"message":"invalid image_url parameter"}}"#,
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        r#"{"error":{"message":"图片尺寸超限"}}"#.as_bytes(),
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"temperature must be between 0 and 2","type":"invalid_request_error"}}"#,
            None,
        ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"message":"Unknown field `foo`","type":"invalid_request_error"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"unsupported mime type for audio","type":"invalid_request_error"}}"#,
            None,
        ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"message":"Unsupported media type","type":"invalid_request_error"}}"#,
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(b"", None));
    assert!(provider_error_looks_like_image_rejection(b"   ", None));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"type":"invalid_request_error"}}"#,
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"type":"invalid_request_error"},"request_id":"abc"}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"message":"revision required","type":"invalid_request_error"}}"#,
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"message":"too many images"}}"#,
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"image_too_large","message":"invalid request"}}"#,
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"invalid_image"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"Unknown field in image_generation","type":"invalid_request_error"}}"#,
            None,
        ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"message":"imagegen argument referenced_image_paths must be an array"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"referenced_image_paths"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"imagenet_model"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"preview_image_cache"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"account_image"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"image_cache"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"message":"Unknown field `image_format`"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"image_size"}}"#,
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"max_image_size"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"image_url_cache"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(b"{}", None,));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"image_parse_error"}}"#,
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"too_many_images"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"message":"cannot fetch https://example.test/cat.jpg"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"message":"cannot fetch https://example.test/image.png"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"type":"https://example.test/errors/vision-unavailable"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"url":"https://example.test/image.png"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"foo":1}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"message":"file image.png missing"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        b" \xef\xbb\xbf<html><title>Bad Request</title><p>image too large</p></html>",
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"invalid_image_url","message":"invalid request"}}"#,
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"message":"unsupported image/png"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"message":"invalid request","type":"invalid_request_error"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"computer_vision_unavailable","message":"model offline"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"computer_vision","message":"model offline"}}"#,
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"message":"vision input is not supported"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"image_generation_failed","message":"quota exceeded"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"image_edit_failed"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"failed_image_generation"}}"#,
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"message":"max_pixels exceeded"}}"#,
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"message":"image_generation ok but image_url is invalid"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"message":"unknown docker_image field","type":"invalid_request_error"}}"#,
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"invalid_image_format"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"invalid_imagegen"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        b"temperature must be between 0 and 2",
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        b"image too large",
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        b"<html><title>Bad Request</title></html>",
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"type":"invalid_request_error","request_id":"abc"},"id":"req-1"}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"["temperature must be between 0 and 2"]"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#""temperature must be between 0 and 2""#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"type":"insufficient_quota"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"type":"authentication_error"}}"#,
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"type":"invalid_image_url"}}"#,
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"image_parse_error"}}"#,
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"too_many_images"}}"#,
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"max_image_size"}}"#,
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        r#"{"error":{"message":"像素超限"}}"#.as_bytes(),
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"ok":false}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"success":false,"reason":"nope"}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"id":"req-1"}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"object":"error","created":1}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"errmsg":"temperature must be between 0 and 2"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"data":{"info":"unknown field foo"}}}"#,
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        br#"{"error":{"errmsg":"too many images"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"message":"fetch failed","url":"https://example.test/image.png"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"message":"crash","stack":"at image.rs:1"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"message":"unknown docker_image_layer field"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"non_image_field"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        br#"{"error":{"code":"not_image_related"}}"#,
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        b"<html><title>Bad Request</title><p>image too large</p></html>",
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        b"<!-- gateway --><html><title>Bad Request</title><p>image too large</p></html>",
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        b"<!-- a --><!-- b --><html><title>Bad Request</title><p>image too large</p></html>",
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        b"<!-- unclosed comment image too large",
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        b"<body><p>image too large</p></body>",
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        b"<!-- image too large -->",
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        b"<?xml version=\"1.0\"?><error>image too large</error>",
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        b"\xef\xbb\xbf<html><title>Bad Request</title><p>image too large</p></html>",
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        r#"{"error":{"message":"画像サイズ超過"}}"#.as_bytes(),
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        r#"{"error":{"message":"画像生成に失敗しました"}}"#.as_bytes(),
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        r#"{"error":{"message":"图像生成失败"}}"#.as_bytes(),
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        r#"{"error":{"message":"関連画像はありません"}}"#.as_bytes(),
        None,
    ));
    assert!(!provider_error_looks_like_image_rejection(
        r#"{"error":{"message":"大きな画像もあります"}}"#.as_bytes(),
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        r#"{"error":{"message":"画像が大きすぎます"}}"#.as_bytes(),
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        r#"{"error":{"message":"生成的图片尺寸过大"}}"#.as_bytes(),
        None,
    ));
    assert!(provider_error_looks_like_image_rejection(
        r#"{"error":{"message":"图片尺寸超限"}}"#.as_bytes(),
        None,
    ));
}

#[test]
fn cloud_code_assist_sse_envelope_is_terminal() {
    let value = json!({
        "response": {
            "candidates": [{"content": {"parts": [{"text": "pong"}]}, "finishReason": "STOP"}]
        }
    });
    assert!(provider_stream_event_is_terminal(
        crate::config::ProviderProtocol::GeminiGenerateContent,
        &value
    ));
    assert!(!provider_stream_event_is_terminal(
        crate::config::ProviderProtocol::GeminiGenerateContent,
        &json!({"response": {"candidates": [{"content": {"parts": [{"text": "po"}]}}]}})
    ));
}

#[test]
fn empty_completion_detection_has_positive_and_negative_cases() {
    assert!(completion_is_empty(&json!({"choices": []})));
    assert!(completion_is_empty(&json!({"output": []})));
    assert!(completion_is_empty(&json!({
        "status": "completed",
        "output": [{"type": "reasoning", "encrypted_content": "signature-only"}]
    })));
    assert!(completion_is_empty(&json!({
        "content": [{"type": "thinking", "thinking": "private"}],
        "stop_reason": "end_turn"
    })));
    assert!(completion_is_empty(&json!({
        "candidates": [{"content": {"parts": [{"thought": true, "text": "private"}]},
            "finishReason": "STOP"}]
    })));
    assert!(completion_is_empty(&json!({
        "choices": [{"message": {"content": "Still working…"}}]
    })));
    assert!(completion_is_empty(&json!({
        "status": "completed",
        "output": [{
            "type": "message",
            "content": [{"type": "output_text", "text": "Still working…"}]
        }]
    })));
    assert!(completion_is_empty(&json!({
        "status": "completed",
        "output": [{"type": "custom_tool_call", "name": "apply_patch", "input": "", "status": "incomplete"}]
    })));
    assert!(completion_is_empty(&json!({
        "status": "incomplete",
        "incomplete_details": {"reason": "empty_response"},
        "output": [{
            "type": "message",
            "content": [{"type": "output_text", "text": "Still working…"}]
        }]
    })));
    assert!(completion_is_empty(&json!({
        "status": "incomplete",
        "incomplete_details": {"reason": "invalid_tool_call"},
        "output": []
    })));
    assert!(!completion_is_empty(&json!({
        "status": "incomplete",
        "incomplete_details": {"reason": "max_output_tokens"},
        "output": [{"type": "message", "content": [{"type": "output_text", "text": "partial"}]}]
    })));
    assert!(!completion_is_empty(&json!({
        "status": "incomplete",
        "incomplete_details": {"reason": "invalid_tool_call"},
        "output": [{"type": "function_call", "name": "lookup", "arguments": "{}"}]
    })));
    assert!(completion_is_empty(&json!({
        "choices": [{"finish_reason": "tool_calls", "message": {
            "content": "Still working…",
            "tool_calls": [{
                "id": "call_empty",
                "function": {"name": "apply_patch", "arguments": ""}
            }]
        }}]
    })));
    assert!(!completion_is_empty(&json!({
        "choices": [{"finish_reason": "tool_calls", "message": {
            "tool_calls": [{
                "id": "call_lookup",
                "function": {"name": "lookup", "arguments": "{}"}
            }]
        }}]
    })));
    assert!(!completion_is_empty(
        &json!({"choices": [{"message": {"content": "ok"}}]})
    ));
    assert!(!completion_is_empty(&json!({
        "status": "completed", "output": [{"type": "function_call", "name": "lookup"}]
    })));
    assert!(!completion_is_empty(&json!({
        "content": [], "stop_reason": "max_tokens"
    })));
    assert!(!completion_is_empty(&json!({
        "candidates": [{"content": {"parts": []}, "finishReason": "MAX_TOKENS"}]
    })));
    for reason in ["OTHER", "MALFORMED_FUNCTION_CALL", "UNEXPECTED_TOOL_CALL"] {
        assert!(!completion_is_empty(&json!({
            "candidates": [{"content": {"parts": []}, "finishReason": reason}]
        })));
    }
    assert!(!completion_is_empty(
        &json!({"error": {"message": "failed"}})
    ));
}

#[test]
fn a_call_without_arguments_is_a_result_but_an_empty_patch_is_not() {
    let gemini = |name: &str| {
        json!({"candidates": [{
            "content": {"parts": [{"functionCall": {"name": name}}]},
            "finishReason": "STOP"
        }]})
    };
    let parameterless = gemini("read_thread");
    assert!(gemini_value_has_content(
        &parameterless,
        &ResponseToolMap::default()
    ));
    assert!(!provider_stream_event_is_empty_terminal(
        ProviderProtocol::GeminiGenerateContent,
        &parameterless
    ));
    assert!(!completion_is_empty(&parameterless));
    for name in ["apply_patch", "", " "] {
        assert!(
            !gemini_value_has_content(&gemini(name), &ResponseToolMap::default()),
            "{name:?}"
        );
        assert!(completion_is_empty(&gemini(name)), "{name:?}");
    }

    let chat = |name: &str, arguments: Value| {
        json!({"choices": [{"finish_reason": "tool_calls", "message": {"tool_calls": [{
            "id": "call_1", "function": {"name": name, "arguments": arguments}
        }]}}]})
    };
    for arguments in [json!(""), json!("  "), Value::Null] {
        let parameterless = chat("lookup", arguments.clone());
        assert!(chat_value_has_content(
            &parameterless,
            &ResponseToolMap::default()
        ));
        assert!(!completion_is_empty(&parameterless));
        let empty_patch = chat("apply_patch", arguments);
        assert!(!chat_value_has_content(
            &empty_patch,
            &ResponseToolMap::default()
        ));
        assert!(completion_is_empty(&empty_patch));
    }

    // A streamed name arrives before its arguments, so on its own it proves nothing.
    let fragment = json!({"choices": [{"delta": {"tool_calls": [{
        "index": 0, "function": {"name": "lookup", "arguments": ""}
    }]}}]});
    assert!(!chat_value_has_content(
        &fragment,
        &ResponseToolMap::default()
    ));
    assert!(chat_value_names_a_tool_call(
        &fragment,
        &ResponseToolMap::default()
    ));
    assert!(!chat_value_names_a_tool_call(
        &json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0, "function": {"name": "apply_patch", "arguments": ""}
        }]}}]}),
        &ResponseToolMap::default()
    ));
    assert!(!provider_stream_event_has_visible_content(
        ProviderProtocol::AnthropicMessages,
        &json!({
            "type": "content_block_start",
            "content_block": {"type": "tool_use", "name": "lookup"}
        })
    ));
}

#[test]
fn a_call_without_input_is_a_result_only_for_a_function() {
    let tools = response_tool_map(&json!({"tools": [
        {"type": "function", "name": "lookup", "parameters": {"type": "object"}},
        {"type": "custom", "name": "exec"},
        {"type": "tool_search"}
    ]}));
    let gemini = |name: &str| {
        json!({"candidates": [{
            "content": {"parts": [{"functionCall": {"name": name}}]},
            "finishReason": "STOP"
        }]})
    };
    let chat = |name: &str| {
        json!({"choices": [{"finish_reason": "tool_calls", "message": {"tool_calls": [{
            "id": "call_1", "function": {"name": name, "arguments": ""}
        }]}}]})
    };
    let delta = |name: &str| {
        json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0, "function": {"name": name, "arguments": ""}
        }]}}]})
    };
    // A declared function, and a name the request did not declare, which the
    // translators deliver as a function.
    for name in ["lookup", "undeclared"] {
        assert!(gemini_value_has_content(&gemini(name), &tools), "{name}");
        assert!(chat_value_has_content(&chat(name), &tools), "{name}");
        assert!(chat_value_names_a_tool_call(&delta(name), &tools), "{name}");
        assert!(
            !completion_is_empty_with_tools(&chat(name), &tools),
            "{name}"
        );
        assert!(
            !completion_is_empty_with_tools(&gemini(name), &tools),
            "{name}"
        );
    }
    for name in ["exec", "tool_search", "apply_patch"] {
        assert!(!gemini_value_has_content(&gemini(name), &tools), "{name}");
        assert!(!chat_value_has_content(&chat(name), &tools), "{name}");
        assert!(
            !chat_value_names_a_tool_call(&delta(name), &tools),
            "{name}"
        );
        assert!(
            completion_is_empty_with_tools(&chat(name), &tools),
            "{name}"
        );
        assert!(
            completion_is_empty_with_tools(&gemini(name), &tools),
            "{name}"
        );
    }
    // Without declarations only the name of a patch says it needs input.
    assert!(!completion_is_empty(&chat("exec")));
    assert!(completion_is_empty(&chat("apply_patch")));
}

#[test]
fn held_tool_call_frames_are_recognised_and_others_are_not() {
    let frame = |value: Value| format!("data: {value}\n\n").into_bytes();
    for has_call in [
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0}]}}]}),
        json!({"candidates": [{"content": {"parts": [{"functionCall": {"name": "x"}}]}}]}),
        json!({"response": {"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "x"}}
        ]}}]}}),
    ] {
        assert!(
            held_frame_has_tool_call(&frame(has_call.clone())),
            "{has_call}"
        );
    }
    for no_call in [
        json!({"choices": [{"delta": {"reasoning_content": "plan"}}]}),
        json!({"choices": [{"delta": {"tool_calls": []}}]}),
        json!({"candidates": [{"content": {"parts": [{"thought": true, "text": "plan"}]}}]}),
    ] {
        assert!(
            !held_frame_has_tool_call(&frame(no_call.clone())),
            "{no_call}"
        );
    }
    assert!(!held_frame_has_tool_call(
        b": codetas-upstream-activity\n\n"
    ));
}

#[test]
fn list_form_chat_content_counts_only_for_its_text() {
    for (key, shape) in [("message", "buffered"), ("delta", "streamed")] {
        let value = |content: Value| json!({"choices": [{(key): {"content": content}}]});
        let answer = value(json!([
            {"type": "thinking", "thinking": [{"type": "text", "text": "plan"}]},
            {"type": "text", "text": "answer"}
        ]));
        assert!(
            chat_value_has_content(&answer, &ResponseToolMap::default()),
            "{shape}"
        );
        assert!(!completion_is_empty(&answer), "{shape}");
        assert!(provider_stream_event_is_reasoning(
            ProviderProtocol::ChatCompletions,
            &answer
        ));
        for empty in [
            json!([]),
            json!([{"type": "thinking", "thinking": "plan"}]),
            json!([{"type": "text", "text": "  "}]),
            json!([{"type": "text", "text": "Still working…"}]),
        ] {
            let empty = value(empty);
            assert!(
                !chat_value_has_content(&empty, &ResponseToolMap::default()),
                "{shape} {empty}"
            );
            assert!(completion_is_empty(&empty), "{shape} {empty}");
        }
    }
}

#[test]
fn antigravity_project_uses_selected_oauth_account_reference() {
    let mut provider = ProviderDefinition {
        google_mode: GoogleMode::CloudCodeAssist,
        credential: ProviderCredential {
            source: CredentialSource::OAuth,
            reference: Some("google-antigravity".into()),
            ..ProviderCredential::default()
        },
        ..ProviderDefinition::default()
    };
    assert_eq!(
        antigravity_project_oauth_reference(&provider, None),
        Some("google-antigravity")
    );
    for key in [
        "google-antigravity#account-a",
        "google-antigravity#account-b",
    ] {
        let account = ProviderCredential {
            reference: Some(key.into()),
            ..provider.credential.clone()
        };
        assert_eq!(
            antigravity_project_oauth_reference(&provider, Some(&account)),
            Some(key)
        );
    }
    provider.credential.reference = Some("google-antigravity#provider-account".into());
    assert_eq!(
        antigravity_project_oauth_reference(&provider, None),
        Some("google-antigravity#provider-account")
    );
    // An explicit non-Antigravity / non-OAuth account must not fall back
    // to the provider's default account.
    for (source, reference) in [
        (CredentialSource::OAuth, Some("anthropic#account")),
        (
            CredentialSource::OAuth,
            Some("google-antigravity-other#account"),
        ),
        (CredentialSource::OAuth, None),
        (
            CredentialSource::Environment,
            Some("google-antigravity#account"),
        ),
    ] {
        let account = ProviderCredential {
            source,
            reference: reference.map(str::to_string),
            ..ProviderCredential::default()
        };
        assert_eq!(
            antigravity_project_oauth_reference(&provider, Some(&account)),
            None
        );
    }
    provider.project = Some("explicit-project".into());
    assert_eq!(antigravity_project_oauth_reference(&provider, None), None);
    provider.project = None;
    provider.google_mode = GoogleMode::default();
    assert_eq!(antigravity_project_oauth_reference(&provider, None), None);
}

#[test]
fn antigravity_wire_models_use_agy_variant_ids() {
    let provider = crate::config::ProviderDefinition {
        id: "google-antigravity".into(),
        google_mode: GoogleMode::CloudCodeAssist,
        ..crate::config::ProviderDefinition::default()
    };
    let candidate = RouteCandidate {
        capabilities: provider.capabilities.clone(),
        provider,
        upstream_model: "gemini-3.7-flash".into(),
        exposed_model: "google-antigravity/gemini-3.7-flash".into(),
        credential: None,
        account_id: None,
        target_key: "google-antigravity/gemini-3.7-flash".into(),
        route_id: None,
        failure_threshold: 1,
        input_price_per_million: None,
        output_price_per_million: None,
        context_window: None,
        max_input_tokens: None,
        max_output_tokens: None,
        reasoning_efforts: Vec::new(),
        default_reasoning_effort: None,
        routing_epoch: 0,
        routing_generation: 0,
        session_scope: None,
    };
    assert_eq!(
        wire_model_for_request(&candidate, &json!({})),
        "gemini-3.7-flash-tiered"
    );
    assert_eq!(
        wire_model_for_request(&candidate, &json!({"reasoning": {"effort": "high"}})),
        "gemini-3.7-flash-tiered"
    );
    for model in ["claude-opus-5-5", "claude-sonnet-5-5"] {
        let mut claude = candidate.clone();
        claude.upstream_model = model.into();
        for effort in ["low", "medium", "high"] {
            assert_eq!(
                wire_model_for_request(&claude, &json!({"reasoning": {"effort": effort}})),
                format!("{model}-{effort}")
            );
        }
        assert_eq!(
            wire_model_for_request(&claude, &json!({})),
            format!("{model}-medium")
        );
        claude.upstream_model = format!("{model}-low");
        assert_eq!(
            wire_model_for_request(&claude, &json!({})),
            format!("{model}-low")
        );
    }
    let mut flash38 = candidate.clone();
    flash38.upstream_model = "gemini-3.8-flash".into();
    assert_eq!(
        wire_model_for_request(&flash38, &json!({})),
        "gemini-3.8-flash-tiered"
    );
    let mut argon = candidate.clone();
    argon.upstream_model = "gemini-4-argon".into();
    for (effort, wire) in [
        ("low", "gemini-4-argon-low"),
        ("medium", "gemini-4-argon-medium"),
        ("high", "gemini-4-argon-high"),
        ("xhigh", "gemini-4-argon-high"),
    ] {
        assert_eq!(
            wire_model_for_request(&argon, &json!({"reasoning": {"effort": effort}})),
            wire
        );
    }
    assert_eq!(
        wire_model_for_request(&argon, &json!({})),
        "gemini-4-argon-medium"
    );
    let mut flash = candidate.clone();
    flash.upstream_model = "gemini-3.6-flash".into();
    assert_eq!(
        wire_model_for_request(&flash, &json!({})),
        "gemini-3.6-flash-medium"
    );
    let mut pro = candidate.clone();
    pro.upstream_model = "gemini-3.1-pro".into();
    assert_eq!(wire_model_for_request(&pro, &json!({})), "gemini-pro-agent");
}

#[test]
fn cloud_code_assist_retry_reuses_the_wire_request_id() {
    let provider = crate::config::ProviderDefinition {
        id: "cloud-code".into(),
        project: Some("project-one".into()),
        google_mode: GoogleMode::CloudCodeAssist,
        ..crate::config::ProviderDefinition::default()
    };
    let candidate = RouteCandidate {
        capabilities: provider.capabilities.clone(),
        provider,
        upstream_model: "gemini-3.1-pro".into(),
        exposed_model: "gemini-3.1-pro".into(),
        credential: None,
        account_id: None,
        target_key: "cloud-code/gemini-3.1-pro".into(),
        route_id: None,
        failure_threshold: 1,
        input_price_per_million: None,
        output_price_per_million: None,
        context_window: None,
        max_input_tokens: None,
        max_output_tokens: None,
        reasoning_efforts: Vec::new(),
        default_reasoning_effort: None,
        routing_epoch: 0,
        routing_generation: 0,
        session_scope: None,
    };
    let request = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
    let source = json!({"prompt_cache_key": "session-one"});
    let first = cloud_code_assist_envelope(
        request.clone(),
        &source,
        &candidate,
        "gemini-pro-agent",
        None,
        Some("agent-stable"),
    )
    .expect("first envelope");
    let retry = cloud_code_assist_envelope(
        request,
        &source,
        &candidate,
        "gemini-pro-agent",
        None,
        Some("agent-stable"),
    )
    .expect("retry envelope");
    assert_eq!(first, retry);
    assert_eq!(first["requestId"], "agent-stable");
}

#[test]
fn nonstream_empty_retry_failure_carries_usage_and_dedicated_marker() {
    let usage = TokenUsage {
        input_tokens: 13,
        output_tokens: 2,
        total_tokens: 15,
        ..TokenUsage::default()
    };
    let response = empty_completion_retry_failed_response(usage.clone(), 2)
        .expect("dedicated failure response");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let marker = response
        .extensions()
        .get::<EmptyCompletionRecoveryFailure>()
        .expect("recovery failure marker");
    assert_eq!(marker.usage.total_tokens, usage.total_tokens);
    assert_eq!(marker.additional_sends, 2);
    let retry = response
        .extensions()
        .get::<ProviderRetryObservation>()
        .expect("provider retry observation");
    assert_eq!(retry.additional_sends, 2);
    assert_eq!(
        retry.recovery_kinds,
        ["empty-completion", "empty-completion"]
    );
    // Failure marker/known billing owns the aggregate; retry usage is not additive here.
    assert_eq!(retry.usage.total_tokens, 0);
}

#[test]
fn streaming_guard_treats_reasoning_signatures_and_redaction_as_precontent() {
    for event in [
        json!({"type": "response.reasoning_text.delta", "delta": "private"}),
        json!({"type": "response.reasoning_signature.delta", "delta": "signature"}),
        json!({"type": "response.output_item.added", "item": {
                "type": "reasoning", "encrypted_content": "redacted"}}),
    ] {
        assert!(!provider_stream_event_has_visible_content(
            ProviderProtocol::Responses,
            &event
        ));
        assert!(provider_stream_event_is_reasoning(
            ProviderProtocol::Responses,
            &event
        ));
    }
    assert!(provider_stream_event_has_visible_content(
        ProviderProtocol::Responses,
        &json!({"type": "response.output_text.delta", "delta": "answer"})
    ));
    assert!(provider_stream_event_has_visible_content(
        ProviderProtocol::Responses,
        &json!({"type": "response.output_item.added", "item": {
                "type": "function_call", "name": "exec"}})
    ));
    assert!(provider_stream_event_has_visible_content(
        ProviderProtocol::Responses,
        &json!({"type": "response.completed", "response": {
            "status": "completed",
            "output": [{"type": "message", "content": [{
                "type": "output_text", "text": "terminal-only answer"
            }]}]
        }})
    ));

    let anthropic_redacted = json!({
        "type": "content_block_start",
        "content_block": {"type": "redacted_thinking", "data": "opaque"}
    });
    assert!(provider_stream_event_is_reasoning(
        ProviderProtocol::AnthropicMessages,
        &anthropic_redacted
    ));
    assert!(!provider_stream_event_has_visible_content(
        ProviderProtocol::AnthropicMessages,
        &anthropic_redacted
    ));
    assert!(provider_stream_event_is_empty_terminal(
        ProviderProtocol::AnthropicMessages,
        &json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}})
    ));

    let gemini_signature = json!({
        "candidates": [{"content": {"parts": [{
            "thought": true, "thoughtSignature": "opaque", "text": "private"
        }]}}]
    });
    assert!(provider_stream_event_is_reasoning(
        ProviderProtocol::GeminiGenerateContent,
        &gemini_signature
    ));
    assert!(!provider_stream_event_has_visible_content(
        ProviderProtocol::GeminiGenerateContent,
        &gemini_signature
    ));

    let first_reasoning = json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "id": "rs_first_attempt",
            "type": "reasoning",
            "status": "completed",
            "encrypted_content": "opaque-first"
        }
    });
    let mut history = responses_reasoning_semantic_history(&first_reasoning);
    assert_eq!(history.len(), 1);
    assert!(history[0].get("id").is_none());
    history[0]["id"] = json!("rs_gateway");
    history[0]["status"] = json!("completed");
    let mut retry_message = json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "id": "msg_retry",
            "type": "message",
            "status": "completed",
            "content": [{"type": "output_text", "text": "answer"}]
        }
    });
    shift_responses_retry_event(&mut retry_message, history.len(), &history);
    let lifecycle = canonical_reasoning_lifecycle_events(&history);
    assert_eq!(lifecycle[0]["output_index"], 0);
    assert_eq!(lifecycle[1]["item"]["id"], "rs_gateway");
    assert_eq!(retry_message["output_index"], 1);
    assert_eq!(
        retry_message.pointer("/item/id").and_then(Value::as_str),
        Some("msg_retry")
    );
}

#[test]
fn responses_multi_retry_reasoning_keeps_every_attempt_in_order() {
    let mut history = Vec::new();
    for (index, text) in ["first", "second", "third"].into_iter().enumerate() {
        let event = json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "id": format!("provider_reasoning_{index}"),
                "type": "reasoning",
                "status": "completed",
                "summary": [{"type": "summary_text", "text": text}],
                "encrypted_content": format!("signature-{index}")
            }
        });
        let mut semantic = responses_reasoning_semantic_history(&event);
        semantic[0]["id"] = json!(format!("rs_gateway_{index}"));
        semantic[0]["status"] = json!("completed");
        history.append(&mut semantic);
    }
    let mut retry_message = json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "sequence_number": 0,
        "item": {"id": "msg_final", "type": "message", "status": "completed"}
    });
    shift_responses_retry_event(&mut retry_message, history.len(), &history);
    let mut sequence = 0;
    let mut lifecycle = canonical_reasoning_lifecycle_events(&history);
    for event in &mut lifecycle {
        assign_responses_sequence(event, &mut sequence);
    }
    assign_responses_sequence(&mut retry_message, &mut sequence);

    assert_eq!(lifecycle.len(), 6);
    assert_eq!(lifecycle[0]["item"]["summary"][0]["text"], "first");
    assert_eq!(lifecycle[2]["item"]["summary"][0]["text"], "second");
    assert_eq!(lifecycle[4]["item"]["summary"][0]["text"], "third");
    assert!(lifecycle.iter().all(|event| !event["item"]["id"]
        .as_str()
        .unwrap_or_default()
        .starts_with("provider_")));
    assert_eq!(retry_message["output_index"], 3);
    assert_eq!(retry_message["sequence_number"], 6);
}

#[test]
fn streaming_guard_retries_only_silent_completed_terminals() {
    let empty = json!({
        "type": "response.completed",
        "response": {"status": "completed", "output": [{"type": "reasoning"}]}
    });
    assert!(provider_stream_event_is_empty_terminal(
        ProviderProtocol::Responses,
        &empty
    ));
    for terminal in [
        json!({"type": "response.incomplete", "response": {
                "status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}}}),
        json!({"type": "response.incomplete", "response": {
                "status": "incomplete", "incomplete_details": {"reason": "content_filter"}}}),
        json!({"type": "response.failed", "response": {"status": "failed"}}),
    ] {
        assert!(provider_stream_event_is_visible_failure(
            ProviderProtocol::Responses,
            &terminal
        ));
        assert!(!provider_stream_event_is_empty_terminal(
            ProviderProtocol::Responses,
            &terminal
        ));
    }
    let failed = empty_completion_failed_event_for_protocol(
        ProviderProtocol::Responses,
        TokenUsage::default(),
        Some("resp_retry"),
    );
    assert_eq!(
        failed
            .pointer("/response/error/code")
            .and_then(Value::as_str),
        Some(EMPTY_COMPLETION_RETRY_FAILED_CODE)
    );
    assert_eq!(
        failed.pointer("/response/id").and_then(Value::as_str),
        Some("resp_retry")
    );

    let usage = sum_token_usage(
        TokenUsage {
            input_tokens: 3,
            output_tokens: 0,
            total_tokens: 3,
            ..TokenUsage::default()
        },
        TokenUsage {
            input_tokens: 5,
            output_tokens: 2,
            total_tokens: 7,
            ..TokenUsage::default()
        },
    );
    assert_eq!(usage.input_tokens, 8);
    assert_eq!(usage.output_tokens, 2);
    assert_eq!(usage.total_tokens, 10);

    let mut gemini_usage = json!({"candidates": []});
    write_token_usage(
        &mut gemini_usage,
        TokenUsage {
            input_tokens: 10,
            output_tokens: 9,
            reasoning_tokens: 4,
            total_tokens: 19,
            ..TokenUsage::default()
        },
    );
    assert_eq!(gemini_usage["usageMetadata"]["candidatesTokenCount"], 5);
    assert_eq!(gemini_usage["usageMetadata"]["thoughtsTokenCount"], 4);

    assert!(provider_stream_event_is_error(
        ProviderProtocol::Responses,
        &json!({"type": "response.failed", "response": {
            "status": "failed", "error": {"message": "upstream failed"}
        }})
    ));
    assert!(!provider_stream_event_is_error(
        ProviderProtocol::Responses,
        &json!({"type": "response.incomplete", "response": {
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"}
        }})
    ));
    let empty_patch = json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call_empty",
            "function": {"name": "apply_patch", "arguments": ""}
        }]}, "finish_reason": "tool_calls"}]
    });
    assert!(!chat_value_has_content(
        &empty_patch,
        &ResponseToolMap::default()
    ));
    assert!(provider_stream_event_is_empty_terminal(
        ProviderProtocol::ChatCompletions,
        &empty_patch
    ));
    let empty_gemini = json!({
        "candidates": [{"content": {"parts": [{
            "functionCall": {"name": "apply_patch", "args": {}}
        }]}, "finishReason": "STOP"}]
    });
    assert!(!gemini_value_has_content(
        &empty_gemini,
        &ResponseToolMap::default()
    ));
    assert!(provider_stream_event_is_empty_terminal(
        ProviderProtocol::GeminiGenerateContent,
        &empty_gemini
    ));
    let empty_anthropic_start = json!({
        "type": "content_block_start",
        "content_block": {"type": "tool_use", "name": "apply_patch", "input": {}}
    });
    assert!(!provider_stream_event_has_visible_content(
        ProviderProtocol::AnthropicMessages,
        &empty_anthropic_start
    ));
    let complete_patch = "*** Begin Patch\n*** Add File: docs/a.html\n+ok\n*** End Patch\n";
    let complete_patch_event = json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call_patch",
            "function": {"name": "apply_patch", "arguments": complete_patch}
        }]}, "finish_reason": "tool_calls"}]
    });
    assert!(chat_value_has_content(
        &complete_patch_event,
        &ResponseToolMap::default()
    ));
    assert!(!provider_stream_event_is_empty_terminal(
        ProviderProtocol::ChatCompletions,
        &complete_patch_event
    ));
    assert!(provider_stream_event_is_empty_terminal(
        ProviderProtocol::Responses,
        &json!({"type": "response.incomplete", "response": {
            "status": "incomplete",
            "incomplete_details": {"reason": "invalid_tool_call"},
            "output": []
        }})
    ));
    for failed_chat in [
        json!({"choices": [{"delta": {}, "finish_reason": "error"}]}),
        json!({"choices": [{"delta": {}, "error": {"message": "failed"}}]}),
    ] {
        assert!(provider_stream_event_is_visible_failure(
            ProviderProtocol::ChatCompletions,
            &failed_chat,
        ));
        assert!(provider_stream_event_is_error(
            ProviderProtocol::ChatCompletions,
            &failed_chat,
        ));
        assert!(!provider_stream_event_is_empty_terminal(
            ProviderProtocol::ChatCompletions,
            &failed_chat,
        ));
    }

    let mut anthropic_start = json!({
        "type": "message_start",
        "message": {"usage": {"input_tokens": 5}}
    });
    write_anthropic_stream_start_usage(&mut anthropic_start, usage.clone());
    assert_eq!(
        anthropic_start
            .pointer("/message/usage/input_tokens")
            .and_then(Value::as_u64),
        Some(8)
    );
}

#[test]
fn chat_metadata_is_forwarded_only_when_provider_opts_in() {
    let body = json!({"input": [{"type": "reasoning", "provider_metadata": {"vendor": {"signature": "opaque"}}}]});
    assert!(chat_compatible_request(&body, false)
        .pointer("/input/0/provider_metadata")
        .is_none());
    assert!(chat_compatible_request(&body, true)
        .pointer("/input/0/provider_metadata")
        .is_some());
}
