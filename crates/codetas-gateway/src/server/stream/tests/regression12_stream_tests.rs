use super::*;

fn done(index: usize) -> Value {
    json!({"type":"response.output_item.done","output_index":index,
            "item":{"id":format!("msg_reg12_{index}"),"type":"message","role":"assistant",
                "status":"completed","content":[{"type":"output_text","text":format!("answer {index}")} ]}})
}
fn seed(ledger: &ObservabilityLedger) -> ObservationSeed {
    let mut settings = ObservabilitySettings::default();
    settings.request_log = true;
    settings.usage_log = true;
    ObservationSeed::without_candidate(
        ledger.clone(),
        settings,
        "regression12".into(),
        "test",
        true,
        Instant::now(),
    )
}
fn upstream(bytes: String) -> reqwest::Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(reqwest::Body::from(bytes))
        .unwrap()
        .into()
}
fn repaired(
    upstream: reqwest::Response,
    observation: ObservationSeed,
    store: Arc<ResponseStateStore>,
    body: Value,
    sparse: bool,
) -> Response<Body> {
    repairing_responses_stream(
        upstream,
        1024 * 1024,
        Duration::from_secs(1),
        observation,
        ResponsesItemIdRepair::new_with_policy(&Default::default(), true).unwrap(),
        store,
        body,
        true,
        sparse,
    )
}

#[tokio::test]
async fn regression12_repaired_eof_is_failure_and_keeps_usage() {
    for sparse in [false, true] {
        let ledger = ObservabilityLedger::new(None);
        let bytes = sse(
            "response.in_progress",
            &json!({"type":"response.in_progress",
                "response":{"usage":{"input_tokens":120000,"output_tokens":3,"input_tokens_details":{"cached_tokens":0}}}}),
        );
        let response = repaired(
            upstream(bytes),
            seed(&ledger),
            Arc::new(ResponseStateStore::new(None)),
            json!({}),
            sparse,
        );
        assert!(to_bytes(response.into_body(), 1024 * 1024).await.is_err());
        let summary = ledger.summary();
        assert_eq!(summary.failed_requests, 1);
        assert_eq!(summary.successful_requests, 0);
        assert_eq!(summary.input_tokens, 120000);
        assert_eq!(summary.output_tokens, 3);
    }
}

#[tokio::test]
async fn regression12_repaired_malformed_frame_keeps_prior_usage() {
    let ledger = ObservabilityLedger::new(None);
    let bytes = format!(
        "{}data: {{invalid\n\n",
        sse(
            "response.in_progress",
            &json!({
            "type":"response.in_progress","response":{"usage":{"input_tokens":101,"output_tokens":7}}})
        )
    );
    let response = repaired(
        upstream(bytes),
        seed(&ledger),
        Arc::new(ResponseStateStore::new(None)),
        json!({}),
        false,
    );
    let mut body = response.into_body().into_data_stream();
    assert!(body.next().await.unwrap().is_ok());
    assert!(body.next().await.unwrap().is_err());
    drop(body); // A downstream client normally stops polling on the first error.
    let summary = ledger.summary();
    assert_eq!(summary.failed_requests, 1);
    assert_eq!(summary.input_tokens, 101);
    assert_eq!(summary.output_tokens, 7);
}

#[tokio::test]
async fn regression12_http_history_backfills_done_without_changing_terminal() {
    for ids in [false, true] {
        for sparse in [false, true] {
            for tainted in [false, true] {
                let ledger = ObservabilityLedger::new(None);
                let store = Arc::new(ResponseStateStore::new(None));
                let request = json!({"model":"test","store":false,"input":[{"type":"message","role":"user","content":"question"}]});
                let mut bytes = sse("response.output_item.done", &done(0));
                if tainted {
                    bytes.push_str(&sse("response.output_text.delta", &json!({"type":"response.output_text.delta","output_index":0,"item_id":"other","delta":"bad"})));
                }
                bytes.push_str(&sse("response.output_item.done", &done(1)));
                bytes.push_str(&sse(
                    "response.completed",
                    &json!({"type":"response.completed",
                        "response":{"id":"resp_regression12","status":"completed","output":[]}}),
                ));
                let response = if ids {
                    repaired(
                        upstream(bytes),
                        seed(&ledger),
                        store.clone(),
                        request,
                        sparse,
                    )
                } else {
                    passthrough_stream(
                        upstream(bytes),
                        1024 * 1024,
                        Duration::from_secs(1),
                        seed(&ledger),
                        store.clone(),
                        request,
                        true,
                        sparse,
                    )
                };
                let output = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
                let events = drain_sse_values(&mut Vec::new(), &output).unwrap();
                assert_eq!(events.last().unwrap()["response"]["output"], json!([]));
                let mut next = json!({"previous_response_id":"resp_regression12","input":[{"type":"message","role":"user","content":"next"}]});
                assert!(store
                    .expand_previous_response_input_with_hint(&mut next, None)
                    .expanded());
                for text in ["answer 0", "answer 1"] {
                    assert!(next["input"].as_array().unwrap().iter().any(|item| item
                        .pointer("/content/0/text")
                        .and_then(Value::as_str)
                        == Some(text)));
                }
                assert_eq!(ledger.summary().successful_requests, 1);
            }
        }
    }
}

#[test]
fn regression12_continuation_done_is_independent_and_terminal_nonempty_wins() {
    let mut snapshot = ResponsesSnapshotAccumulator::default();
    snapshot.taint();
    snapshot.observe(&done(0));
    snapshot.observe(&done(0));
    let empty = json!({"id":"resp_test","status":"completed","output":[]});
    assert_eq!(
        snapshot.response_for_continuation(empty)["output"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let terminal = json!({"output":[{"type":"message","content":"authoritative"}]});
    assert_eq!(
        snapshot.response_for_continuation(terminal.clone()),
        terminal
    );
}
