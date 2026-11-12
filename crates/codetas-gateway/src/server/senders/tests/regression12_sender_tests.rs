use super::*;

fn candidate_and_state() -> (RouteCandidate, GatewayState) {
    let mut settings = GatewaySettings::default();
    settings.observability.usage_log = true;
    settings.observability.request_log = true;
    settings.providers = vec![serde_json::from_value(json!({
        "id":"regression12", "name":"regression12", "enabled":true,
        "baseUrl":"https://api.anthropic.com/v1", "models":["claude-opus-5-5"],
        "protocol":ProviderProtocol::AnthropicMessages
    }))
    .unwrap()];
    let mut routing = RoutingRuntime::default();
    let candidate = routing
        .candidates_for_request(&settings, "regression12/claude-opus-5-5", false, None)
        .unwrap()
        .remove(0);
    let settings = Arc::new(RwLock::new(settings));
    let state = GatewayState {
        settings: settings.clone(),
        client: reqwest::Client::new(),
        routing: Arc::new(Mutex::new(routing)),
        observability: ObservabilityLedger::new(None),
        video_jobs: Arc::new(Mutex::new(HashMap::new())),
        instance_id: Uuid::new_v4().to_string(),
        response_state: Arc::new(ResponseStateStore::new(None)),
        memory: Arc::new(MemoryAdmission {
            settings,
            inflight: AtomicU32::new(0),
            reserved_bytes: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        }),
        pacing: Arc::new(ProviderPacing::default()),
    };
    (candidate, state)
}
fn response(status: StatusCode, value: Value) -> reqwest::Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::RETRY_AFTER, "0")
        .body(reqwest::Body::from(value.to_string()))
        .unwrap()
        .into()
}
fn seed(state: &GatewayState, candidate: &RouteCandidate, id: &str) -> ObservationSeed {
    let mut settings = ObservabilitySettings::default();
    settings.usage_log = true;
    settings.request_log = true;
    ObservationSeed::for_candidate(
        state.observability.clone(),
        settings,
        id.into(),
        false,
        Instant::now(),
        1,
        candidate,
    )
}

#[tokio::test]
async fn regression12_image_rejection_preserves_all_billing_once_and_diagnostics() {
    for status in [StatusCode::BAD_REQUEST, StatusCode::PAYLOAD_TOO_LARGE] {
        let (candidate, state) = candidate_and_state();
        let mut upstream = response(
            status,
            json!({"error":{"type":"invalid_request_error","message":"too many images"},
                "usage":{"input_tokens":11,"output_tokens":3}}),
        );
        upstream
            .extensions_mut()
            .insert(SentImageState { sent_images: 2 });
        upstream.extensions_mut().insert(ProviderRetryObservation {
            additional_sends: 2,
            recovery_kinds: vec!["rate-limit".into(), "provider-5xx".into()],
            usage: TokenUsage {
                input_tokens: 37,
                output_tokens: 5,
                total_tokens: 42,
                ..Default::default()
            },
            ..Default::default()
        });
        upstream
            .extensions_mut()
            .insert(crate::observability::RequestDiagnostic {
                wire_model: Some("wire-model".into()),
                wire_bytes: 123,
                upstream_status_code: status.as_u16(),
                ..Default::default()
            });
        let failure = require_compaction_on_image_rejection(upstream, status)
            .await
            .unwrap_err();
        let retry = failure
            .response
            .extensions()
            .get::<ProviderRetryObservation>()
            .unwrap();
        assert_eq!(retry.additional_sends, 2);
        assert_eq!(retry.usage.input_tokens, 48);
        assert_eq!(retry.usage.output_tokens, 8);
        assert_eq!(
            failure
                .response
                .extensions()
                .get::<crate::observability::RequestDiagnostic>()
                .unwrap()
                .wire_bytes,
            123
        );
        assert!(failure
            .response
            .extensions()
            .get::<UpstreamErrorDiagnostic>()
            .is_some());
        let mut observation = seed(&state, &candidate, "image-error");
        observation.record_provider_retries(retry);
        observation.record_upstream_error(&failure.response);
        observation.finish(
            failure.response.status(),
            Some("request_rejected"),
            TokenUsage::default(),
        );
        let summary = state.observability.summary();
        assert_eq!(summary.input_tokens, 48);
        assert_eq!(summary.output_tokens, 8);
        assert_eq!(summary.total_tokens, 56);
        assert_eq!(summary.failed_requests, 1);
    }
}

#[tokio::test]
async fn regression12_pacing_failure_keeps_one_prior_physical_attempt() {
    for status in [StatusCode::TOO_MANY_REQUESTS, StatusCode::BAD_GATEWAY] {
        let (mut candidate, state) = candidate_and_state();
        candidate.provider.credential.source = CredentialSource::Environment;
        candidate.credential = None;
        candidate.provider.limits.request_retries = 1;
        candidate.provider.limits.retry_on_429 = true;
        candidate.provider.limits.max_429_retries = 1;
        candidate.provider.limits.request_pacing_ms = 60_000;
        let sends = Arc::new(AtomicU32::new(0));
        let sent = sends.clone();
        let pacing = state.pacing.clone();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            retry_provider_request(&state, &candidate, false, move || {
                let pacing = pacing.clone();
                let sent = sent.clone();
                async move {
                    assert_eq!(sent.fetch_add(1, Ordering::Relaxed), 0);
                    tokio::spawn(async move {
                        loop {
                            if pacing
                                .state
                                .lock()
                                .await
                                .queues
                                .values()
                                .any(|q| !q.waiters.is_empty())
                            {
                                reset_provider_pacing(&pacing).await;
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                    });
                    Ok(response(
                        status,
                        json!({"usage":{"input_tokens":17,"output_tokens":4}}),
                    ))
                }
            }),
        )
        .await
        .unwrap();
        let failure = result.unwrap_err();
        if failure
            .response
            .extensions()
            .get::<ProviderRetryObservation>()
            .is_none()
        {
            let bytes = to_bytes(failure.response.into_body(), 65536).await.unwrap();
            panic!(
                "missing retry evidence after {} sends: {}",
                sends.load(Ordering::Relaxed),
                String::from_utf8_lossy(&bytes)
            );
        }
        let retry = failure
            .response
            .extensions()
            .get::<ProviderRetryObservation>()
            .unwrap();
        assert_eq!(sends.load(Ordering::Relaxed), 1);
        assert_eq!(retry.additional_sends, 0);
        assert_eq!(retry.usage.input_tokens, 17);
        let mut observation = seed(&state, &candidate, "pacing-error");
        observation.record_provider_retries(retry);
        observation.finish(
            failure.response.status(),
            Some("request_rejected"),
            TokenUsage::default(),
        );
        assert_eq!(state.observability.summary().total_tokens, 21);
    }
}

#[test]
fn chat_wire_request_applies_the_mistral_rewrites_only_for_mistral() {
    let (mut candidate, _) = candidate_and_state();
    let tools = json!([{"type": "custom", "name": "apply_patch", "description": "freeform"}]);
    let compaction = json!({"tools": tools, "input": [
        {"type": "message", "role": "user", "content": "hello"},
        {"type": "message", "role": "assistant", "content": "done"},
        {"type": "message", "role": "developer", "content": "summarize"}
    ]});
    let retry = json!({"input": [
        {"type": "message", "role": "user", "content": "hello"},
        {"type": "message", "role": "assistant", "content": "done"}
    ]});

    candidate.provider.base_url = "https://api.mistral.ai/v1".into();
    let promoted = chat_wire_request(&compaction, &candidate, "m").unwrap();
    let messages = promoted["messages"].as_array().unwrap();
    assert_eq!(messages.last().unwrap()["role"], "user");
    assert_eq!(messages.last().unwrap()["content"], "summarize");
    assert!(promoted["tools"][0]["function"]["description"]
        .as_str()
        .unwrap()
        .contains("*** Begin Patch"));
    let continued = chat_wire_request(&retry, &candidate, "m").unwrap();
    let messages = continued["messages"].as_array().unwrap();
    assert_eq!(messages.last().unwrap()["role"], "user");
    assert_eq!(messages.last().unwrap()["content"], "Continue.");

    candidate.provider.base_url = "https://proxy.example/v1".into();
    let other = chat_wire_request(&compaction, &candidate, "m").unwrap();
    // The translation itself keeps a mid-thread instruction in place.
    let messages = other["messages"].as_array().unwrap();
    assert_eq!(messages.last().unwrap()["role"], "user");
    assert_eq!(messages.last().unwrap()["content"], "summarize");
    assert_eq!(other["tools"][0]["function"]["description"], "freeform");
    let unchanged = chat_wire_request(&retry, &candidate, "m").unwrap();
    assert_eq!(
        unchanged["messages"].as_array().unwrap().last().unwrap()["role"],
        "assistant"
    );
}

#[test]
fn scoped_native_history_moves_context_after_the_checkpoint_on_the_copy_only() {
    let (mut candidate, _) = candidate_and_state();
    candidate.credential = None;
    candidate.provider.credential.source = CredentialSource::Environment;
    let scope = anthropic_checkpoint_scope(&candidate, "test-auth-binding");
    let checkpoint = crate::anthropic_compaction::Checkpoint {
        version: 1,
        scope: scope.clone(),
        context_hash: "a".repeat(64),
        block: json!({"type":"compaction","content":"visible summary","signature":"opaque"}),
        retained: vec![json!({"type":"message","role":"user","content":"tail"})],
    };
    let body = json!({"input": [
        {"type": "message", "role": "developer", "content": "permissions"},
        crate::anthropic_compaction::encode(&checkpoint).unwrap()
    ]});

    let scoped = scoped_native_history(&body, &candidate, Some(&scope)).unwrap();
    let items = scoped["input"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert!(crate::anthropic_compaction::is_item(&items[0]));
    assert_eq!(items[1]["content"], "permissions");
    assert_eq!(items[2]["content"], "tail");
    assert_eq!(body["input"][0]["content"], "permissions");

    // A checkpoint this route cannot replay keeps the history in place, because
    // the fallback text is an ordinary message that is not dropped.
    let downgraded = scoped_native_history(&body, &candidate, None).unwrap();
    let items = downgraded["input"].as_array().unwrap();
    assert_eq!(items[0]["content"], "permissions");
    assert!(!items.iter().any(crate::anthropic_compaction::is_item));
}

#[tokio::test]
async fn chat_stream_with_a_named_call_and_no_arguments_is_not_retried() {
    // Declared as a function, and not declared at all.
    for body in [
        json!({"tools": [{"type": "function", "name": "lookup"}]}),
        json!({}),
    ] {
        let (mut candidate, state) = candidate_and_state();
        candidate.provider.protocol = ProviderProtocol::ChatCompletions;
        candidate.provider.base_url = "http://127.0.0.1:9".into();
        candidate.provider.limits.empty_completion_retries = 1;
        let wire = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": 0, "id": "call_1", "type": "function",
                    "function": {"name": "lookup", "arguments": ""}
                }]}, "finish_reason": null}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]})
        );
        let raw = Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(reqwest::Body::from(wire))
            .unwrap();
        let guarded = guarded_provider_sse(
            state.clone(),
            body,
            candidate,
            None,
            reqwest::Response::from(raw),
            None,
            None,
        )
        .unwrap();
        let text = guarded.text().await.unwrap();
        assert!(text.contains("\"lookup\""), "{text}");
        assert!(text.contains("\"finish_reason\":\"tool_calls\""), "{text}");
        assert!(!text.contains(EMPTY_COMPLETION_RETRY_FAILED_CODE), "{text}");
    }
}

/// Past the retry buffer nothing can be taken back, so the rest of the stream has
/// to reach the client, including frames that carry no content.
#[tokio::test]
async fn chat_stream_past_the_retry_buffer_keeps_its_answer_finish_and_usage() {
    let (mut candidate, state) = candidate_and_state();
    candidate.provider.protocol = ProviderProtocol::ChatCompletions;
    candidate.capabilities.streaming = true;
    let frame = |value: Value| format!("data: {value}\n\n");
    let delta = |delta: Value, finish: Value| {
        frame(json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}))
    };
    let mut wire = delta(json!({"reasoning_content": "r"}), Value::Null)
        .repeat(EMPTY_COMPLETION_MAX_BUFFERED_EVENTS + 1);
    wire.push_str(&delta(json!({"content": "answer"}), Value::Null));
    wire.push_str(&delta(json!({}), json!("stop")));
    wire.push_str(&frame(json!({"choices": [], "usage": {
        "prompt_tokens": 17, "completion_tokens": 3, "total_tokens": 20
    }})));
    wire.push_str("data: [DONE]\n\n");
    let raw = Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(reqwest::Body::from(wire))
        .unwrap();
    let guarded = guarded_provider_sse(
        state.clone(),
        json!({}),
        candidate.clone(),
        None,
        reqwest::Response::from(raw),
        None,
        None,
    )
    .unwrap();
    let seed = ObservationSeed::without_candidate(
        ObservabilityLedger::new(None),
        ObservabilitySettings::default(),
        "past-the-buffer".into(),
        "model",
        true,
        Instant::now(),
    );
    let adapted = adapt_successful_response(
        guarded,
        &candidate,
        seed,
        ResponseToolMap::default(),
        &state.response_state,
        &json!({"model": "model", "stream": true, "input": []}),
        false,
        false,
    )
    .await;
    let bytes = to_bytes(adapted.into_body(), 10_000_000).await.unwrap();
    let events = drain_sse_values(&mut Vec::new(), &bytes).unwrap();
    let terminal = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .expect("the answer ends as completed");
    let output = terminal["response"]["output"].as_array().unwrap();
    let message = output
        .iter()
        .find(|item| item["type"] == "message")
        .expect("the answer reaches the client");
    assert_eq!(message["content"][0]["text"], "answer");
    assert_eq!(terminal["response"]["usage"]["input_tokens"], 17);
    assert_eq!(terminal["response"]["usage"]["output_tokens"], 3);
}

#[test]
fn regression12_forward_never_proves_signature_account() {
    let (mut candidate, _) = candidate_and_state();
    candidate.credential = None;
    candidate.provider.credential.source = CredentialSource::Environment;
    let checkpoint = crate::anthropic_compaction::Checkpoint {
        version: 1,
        scope: anthropic_checkpoint_scope(&candidate, "test-auth-binding"),
        context_hash: "a".repeat(64),
        block: json!({"type":"compaction","content":"visible summary","signature":"opaque-test-signature"}),
        retained: vec![],
    };
    let mut body = json!({"input":[crate::anthropic_compaction::encode(&checkpoint).unwrap()]});
    assert!(preserve_native_anthropic_history(
        &body,
        &candidate,
        Some(&anthropic_checkpoint_scope(&candidate, "test-auth-binding"))
    ));
    for account in [None, Some("configured-label".to_string())] {
        candidate.provider.credential.source = CredentialSource::Forward;
        candidate.account_id = account;
        let mut forwarded = checkpoint.clone();
        forwarded.scope = anthropic_checkpoint_scope(&candidate, "test-auth-binding");
        body["input"] = json!([crate::anthropic_compaction::encode(&forwarded).unwrap()]);
        assert!(!preserve_native_anthropic_history(
            &body,
            &candidate,
            Some(&anthropic_checkpoint_scope(&candidate, "test-auth-binding"))
        ));
        prepare_native_anthropic_history(
            &mut body,
            &candidate,
            Some(&anthropic_checkpoint_scope(&candidate, "test-auth-binding")),
        )
        .unwrap();
        assert!(!body.to_string().contains("opaque-test-signature"));
        assert!(body.to_string().contains("visible summary"));
    }
}

#[tokio::test]
async fn regression12_wire_model_survives_buffering_and_separates_cache_windows() {
    let (mut candidate, state) = candidate_and_state();
    candidate.provider.google_mode = GoogleMode::CloudCodeAssist;
    let thread = Uuid::new_v4().to_string();
    for index in 0..20 {
        let effort = if index % 2 == 0 { "low" } else { "high" };
        let wire = wire_model_for_request(&candidate, &json!({"reasoning":{"effort":effort}}));
        assert_eq!(wire, format!("claude-opus-5-5-{effort}"));
        let mut upstream = response(
            StatusCode::OK,
            json!({"usage":{"input_tokens":100000,"input_tokens_details":{"cached_tokens":0}}}),
        );
        let mut headers = HeaderMap::new();
        headers.insert("session_id", HeaderValue::from_str(&thread).unwrap());
        diagnostics::attach_wire_diagnostic(
            &mut upstream,
            &json!({"model":wire}),
            10,
            Some(&headers),
        );
        let upstream = buffer_provider_response(upstream, 1024)
            .await
            .unwrap()
            .into_response()
            .unwrap();
        let mut observation = seed(&state, &candidate, &format!("wire-{index}"));
        observation.record_reqwest_upstream_error(&upstream);
        observation.finish(StatusCode::OK,None,TokenUsage::from_json(&json!({"usage":{"input_tokens":100000,"input_tokens_details":{"cached_tokens":0}}})));
    }
    let health = state.observability.cache_health_summary();
    assert_eq!(health.sessions.len(), 2);
    for row in health.sessions {
        assert_eq!(row.rolling_requests, 10);
        assert!(!row.active_alert);
        assert!(row.model.ends_with("-low") || row.model.ends_with("-high"));
    }
}
