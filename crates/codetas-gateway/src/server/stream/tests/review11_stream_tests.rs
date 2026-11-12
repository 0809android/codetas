use super::*;
fn seed(ledger: &ObservabilityLedger) -> ObservationSeed {
    let mut seed = ObservationSeed::without_candidate(
        ledger.clone(),
        ObservabilitySettings::default(),
        "review11-stream".into(),
        "fixture/model",
        true,
        Instant::now(),
    );
    seed.provider_id = Some("fixture".into());
    seed.send_count = 1;
    seed.request_diagnostic = Some(crate::observability::RequestDiagnostic {
        client_thread_id: Some("01a13ddb-8d21-7cb2-8f00-23a8a09af079".into()),
        ..Default::default()
    });
    seed
}
fn upstream(body: String) -> reqwest::Response {
    reqwest::Response::from(
        Response::builder()
            .status(200)
            .body(reqwest::Body::from(body))
            .unwrap(),
    )
}
#[tokio::test]
async fn nonstream_chat_answer_with_no_text_reaches_the_client_as_incomplete() {
    let ledger = ObservabilityLedger::new(None);
    let response = chat_json_response(
        upstream(
            json!({
                "choices": [{"finish_reason": "stop", "message": {
                    "role": "assistant", "content": ""
                }}],
                "usage": {"prompt_tokens": 123, "completion_tokens": 7}
            })
            .to_string(),
        ),
        "fixture/model",
        65536,
        seed(&ledger),
        ResponseToolMap::default(),
        Arc::new(ResponseStateStore::new(None)),
        json!({}),
        false,
        false,
    )
    .await;
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["status"], "incomplete");
    assert_eq!(body["incomplete_details"]["reason"], "empty_response");
    assert_eq!(ledger.summary().failed_requests, 1);
}
#[tokio::test]
async fn nonstream_gemini_thought_only_answer_reaches_the_client_as_incomplete() {
    let ledger = ObservabilityLedger::new(None);
    let response = adapted_json_response(
        upstream(
            json!({"candidates": [{
                "content": {"parts": [{"thought": true, "text": "plan"}]},
                "finishReason": "STOP"
            }]})
            .to_string(),
        ),
        "fixture/model",
        gemini_to_response,
        65536,
        seed(&ledger),
        false,
        ResponseToolMap::default(),
        Arc::new(ResponseStateStore::new(None)),
        json!({}),
        false,
    )
    .await;
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["status"], "incomplete");
    assert_eq!(body["incomplete_details"]["reason"], "empty_response");
}
fn tool_call_chunk(name: &str) -> String {
    format!(
        "data: {}\n\n",
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
                "index": 0, "id": "call_1", "type": "function",
                "function": {"name": name, "arguments": "{}"}
            }]}, "finish_reason": null}]})
    )
}
#[tokio::test]
async fn translated_tool_turn_keeps_the_real_session_tip() {
    let ledger = ObservabilityLedger::new(None);
    let store = Arc::new(ResponseStateStore::new(None));
    let mut body = json!({"model": "model", "input": [
        {"type": "message", "role": "user", "content": "go"}
    ]});
    store.attach_session_hint(&mut body, "thread-translated");
    let wire = format!(
        "{}data: {}\n\n",
        tool_call_chunk("lookup"),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]})
    );
    let response = translated_stream_response(
        upstream(wire),
        "fixture/model".into(),
        StreamAdapter::Chat,
        65536,
        Duration::from_secs(1),
        seed(&ledger),
        false,
        ResponseToolMap::default(),
        ToolProgressPolicy::default(),
        Arc::clone(&store),
        body,
        true,
        false,
        false,
    );
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("response.completed"));
    let tip = store
        .unique_live_tip("thread-translated")
        .expect("the real session keeps its tip");
    let mut next = json!({"previous_response_id": tip, "input": []});
    assert!(store.expand_previous_response_input(&mut next));
    assert_eq!(next["input"][1]["type"], "function_call");
}
#[tokio::test]
async fn failed_translated_stream_forgets_its_snapshot_and_keeps_the_parent() {
    let ledger = ObservabilityLedger::new(None);
    let store = Arc::new(ResponseStateStore::new(None));
    let mut root = json!({"input": [{"type": "message", "role": "user", "content": "go"}]});
    store.attach_session_hint(&mut root, "thread-failed");
    store.remember(
        &root,
        &json!({"id": "resp_parent", "status": "completed", "output": [
            {"type": "message", "role": "assistant", "content": "ok"}
        ]}),
        true,
    );
    let mut body = json!({"previous_response_id": "resp_parent", "input": [
        {"type": "message", "role": "user", "content": "next"}
    ]});
    assert!(store.expand_previous_response_input(&mut body));
    let wire = format!(
        "{}data: {}\n\n",
        tool_call_chunk("lookup"),
        json!({"error": {"message": "fixture upstream rejection"}})
    );
    let response = translated_stream_response(
        upstream(wire),
        "fixture/model".into(),
        StreamAdapter::Chat,
        65536,
        Duration::from_secs(1),
        seed(&ledger),
        false,
        ResponseToolMap::default(),
        ToolProgressPolicy::default(),
        Arc::clone(&store),
        body,
        true,
        false,
        false,
    );
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("response.failed"));
    assert_eq!(
        store.unique_live_tip("thread-failed").as_deref(),
        Some("resp_parent")
    );
}
/// The WebSocket turn stops reading at the terminal event and drops the stream,
/// so nothing after the `response.failed` yield is guaranteed to run.
#[tokio::test]
async fn translated_failure_settles_before_a_client_that_stops_at_response_failed() {
    type Chunk = Result<Bytes, std::io::Error>;
    let frame = |text: String| Ok::<Bytes, std::io::Error>(Bytes::from(text));
    let error_frame = format!(
        "data: {}\n\n",
        json!({"error": {"message": "fixture upstream rejection"}})
    );
    for (name, ending, never_ends) in [
        ("no terminal", Vec::<Chunk>::new(), false),
        ("error frame", vec![frame(error_frame)], false),
        ("bad frame", vec![frame("data: {bad}\n\n".into())], false),
        (
            "read error",
            vec![Err(std::io::Error::other("fixture"))],
            false,
        ),
        ("idle", Vec::<Chunk>::new(), true),
    ] {
        let ledger = ObservabilityLedger::new(None);
        let store = Arc::new(ResponseStateStore::new(None));
        let mut root = json!({"input": [{"type": "message", "role": "user", "content": "go"}]});
        store.attach_session_hint(&mut root, "thread-stops-reading");
        store.remember(
            &root,
            &json!({"id": "resp_parent", "status": "completed", "output": [
                {"type": "message", "role": "assistant", "content": "ok"}
            ]}),
            true,
        );
        let mut body = json!({"previous_response_id": "resp_parent", "input": [
            {"type": "message", "role": "user", "content": "next"}
        ]});
        assert!(store.expand_previous_response_input(&mut body));

        let mut chunks = vec![frame(tool_call_chunk("lookup"))];
        chunks.extend(ending);
        let source = futures_util::stream::iter(chunks);
        let source: Pin<Box<dyn futures_util::Stream<Item = Chunk> + Send>> = if never_ends {
            Box::pin(source.chain(futures_util::stream::pending()))
        } else {
            Box::pin(source)
        };
        let response = translated_stream_response(
            reqwest::Response::from(
                Response::builder()
                    .status(200)
                    .body(reqwest::Body::wrap_stream(source))
                    .unwrap(),
            ),
            "fixture/model".into(),
            StreamAdapter::Chat,
            65536,
            Duration::from_millis(100),
            seed(&ledger),
            false,
            ResponseToolMap::default(),
            ToolProgressPolicy::default(),
            Arc::clone(&store),
            body,
            true,
            false,
            false,
        );
        let mut stream = response.into_body().into_data_stream();
        let mut failed = false;
        while let Some(chunk) = stream.next().await {
            if String::from_utf8_lossy(&chunk.unwrap()).contains("response.failed") {
                failed = true;
                break;
            }
        }
        assert!(failed, "{name}");
        assert_eq!(
            store.unique_live_tip("thread-stops-reading").as_deref(),
            Some("resp_parent"),
            "{name}"
        );
        let mut retry = json!({"previous_response_id": "resp_parent", "input": []});
        assert!(store.expand_previous_response_input(&mut retry), "{name}");
        drop(stream);
    }
}
#[tokio::test]
async fn translated_gemini_stream_keeps_calls_from_separate_chunks_apart() {
    let ledger = ObservabilityLedger::new(None);
    let chunk = |call: Value, finish: Option<&str>| {
        let mut candidate = json!({"content": {"parts": [{"functionCall": call}]}});
        if let Some(finish) = finish {
            candidate["finishReason"] = json!(finish);
        }
        format!("data: {}\n\n", json!({"candidates": [candidate]}))
    };
    let wire = [
        chunk(
            json!({"name": "read_file", "args": {"path": "a_long_path"}}),
            None,
        ),
        chunk(json!({"name": "exec_command", "args": {"cmd": "ls"}}), None),
        chunk(
            json!({"name": "read_file", "args": {"path": "a_long_path"}}),
            Some("STOP"),
        ),
    ]
    .concat();
    let response = translated_stream_response(
        upstream(wire),
        "fixture/model".into(),
        StreamAdapter::Gemini,
        65536,
        Duration::from_secs(1),
        seed(&ledger),
        false,
        ResponseToolMap::default(),
        ToolProgressPolicy::default(),
        Arc::new(ResponseStateStore::new(None)),
        json!({"input": []}),
        false,
        false,
        false,
    );
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    let terminal = drain_sse_values(&mut Vec::new(), &bytes)
        .unwrap()
        .into_iter()
        .find(|event| event["type"] == "response.completed")
        .expect("completed terminal");
    let output = terminal["response"]["output"].as_array().unwrap();
    assert_eq!(output.len(), 2);
    assert_eq!(output[0]["name"], "read_file");
    assert_eq!(output[0]["arguments"], "{\"path\":\"a_long_path\"}");
    assert_eq!(output[1]["name"], "exec_command");
    assert_eq!(output[1]["arguments"], "{\"cmd\":\"ls\"}");
}
#[tokio::test]
async fn chat_conversion_failure_retains_raw_billed_usage() {
    let ledger = ObservabilityLedger::new(None);
    let response = chat_json_response(
        upstream(json!({"usage":{"prompt_tokens":123,"completion_tokens":7}}).to_string()),
        "fixture/model",
        65536,
        seed(&ledger),
        ResponseToolMap::default(),
        Arc::new(ResponseStateStore::new(None)),
        json!({}),
        false,
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(ledger.summary().input_tokens, 123);
    assert_eq!(ledger.summary().output_tokens, 7);
    assert_eq!(ledger.summary().failed_requests, 1);
}
#[tokio::test]
async fn responses_eof_without_terminal_is_failed_and_never_cache_success() {
    for repair in [false, true] {
        let ledger = ObservabilityLedger::new(None);
        let event = json!({"type":"response.created","response":{"id":"fixture","status":"in_progress","output":[],
                "usage":{"input_tokens":100000,"output_tokens":7,"input_tokens_details":{"cached_tokens":90000}}}});
        let response = passthrough_stream(
            upstream(sse("response.created", &event)),
            65536,
            Duration::from_secs(1),
            seed(&ledger),
            Arc::new(ResponseStateStore::new(None)),
            json!({"input":[]}),
            false,
            repair,
        );
        assert!(to_bytes(response.into_body(), 65536).await.is_err());
        assert_eq!(ledger.summary().failed_requests, 1);
        assert_eq!(ledger.summary().successful_requests, 0);
        assert_eq!(ledger.summary().input_tokens, 100000);
        assert!(ledger.cache_health_summary().sessions.is_empty());
    }
}
#[tokio::test]
async fn malformed_sse_suffix_cannot_erase_same_chunk_usage() {
    for split in [false, true] {
        let first = sse(
            "message",
            &json!({"usage":{"prompt_tokens":123,"completion_tokens":7}}),
        );
        let bad = "data: {bad json}\n\n";
        let chunks = if split {
            vec![first, bad.into()]
        } else {
            vec![format!("{first}{bad}")]
        };
        let source = futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|s| Ok::<Bytes, std::io::Error>(Bytes::from(s))),
        );
        let upstream = reqwest::Response::from(
            Response::builder()
                .body(reqwest::Body::wrap_stream(source))
                .unwrap(),
        );
        let ledger = ObservabilityLedger::new(None);
        let response = translated_stream_response(
            upstream,
            "fixture/model".into(),
            StreamAdapter::Chat,
            65536,
            Duration::from_secs(1),
            seed(&ledger),
            false,
            ResponseToolMap::default(),
            ToolProgressPolicy::default(),
            Arc::new(ResponseStateStore::new(None)),
            json!({"input":[]}),
            false,
            false,
            false,
        );
        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("response.failed"));
        assert_eq!(ledger.summary().input_tokens, 123);
        assert_eq!(ledger.summary().output_tokens, 7);
        assert_eq!(ledger.summary().failed_requests, 1);
    }
}
#[test]
fn responses_usage_prefix_survives_later_json_error() {
    let mut usage = TokenUsage::default();
    let mut terminal = None;
    let bytes = format!(
        "{}data: {{bad}}\n\n",
        sse(
            "response.created",
            &json!({"type":"response.created","response":{"usage":{"input_tokens":123}}})
        )
    );
    assert!(inspect_sse_usage_checked(
        &mut Vec::new(),
        bytes.as_bytes(),
        &mut usage,
        &mut terminal,
        &mut ResponsesSnapshotAccumulator::default()
    )
    .is_err());
    assert_eq!(usage.input_tokens, 123);
}
