use super::*;
#[tokio::test]
async fn known_gemini_failure_before_footer_cannot_become_success_on_client_cancel() {
    let settings = Arc::new(RwLock::new(GatewaySettings::default()));
    let ledger = ObservabilityLedger::new(None);
    let state = GatewayState {
        settings: settings.clone(),
        client: reqwest::Client::new(),
        routing: Arc::new(Mutex::new(RoutingRuntime::default())),
        observability: ledger.clone(),
        video_jobs: Arc::new(Mutex::new(HashMap::new())),
        instance_id: "physical-observation-test".into(),
        response_state: Arc::new(ResponseStateStore::new(None)),
        memory: Arc::new(MemoryAdmission {
            settings,
            inflight: AtomicU32::new(0),
            reserved_bytes: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        }),
        pacing: Arc::new(ProviderPacing::default()),
    };
    let mut provider = crate::config::ProviderDefinition::default();
    provider.id = "test".into();
    provider.protocol = ProviderProtocol::GeminiGenerateContent;
    provider.base_url = "http://127.0.0.1:9".into();
    provider.credential.source = CredentialSource::None;
    let candidate = RouteCandidate {
        capabilities: provider.capabilities.clone(),
        provider,
        upstream_model: "model".into(),
        exposed_model: "model".into(),
        credential: None,
        account_id: None,
        target_key: "test/model".into(),
        route_id: None,
        failure_threshold: 3,
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
    let (sender, mut receiver) = mpsc::channel::<Bytes>(4);
    let source = stream! {while let Some(bytes)=receiver.recv().await {yield Ok::<Bytes,std::io::Error>(bytes);}};
    let first = json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"id":"c","name":"lookup","args":{"x":1}}}]}}],"usageMetadata":{"promptTokenCount":100_000,"candidatesTokenCount":1,"cachedContentTokenCount":0}});
    sender
        .send(Bytes::from(format!("data: {}\n\n", first)))
        .await
        .unwrap();
    let raw = Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(reqwest::Body::wrap_stream(source))
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
    let shared = guarded
        .extensions()
        .get::<SharedProviderRetryObservation>()
        .unwrap()
        .clone();
    let mut seed = ObservationSeed::for_candidate(
        ledger.clone(),
        ObservabilitySettings::default(),
        "known-failure".into(),
        true,
        Instant::now(),
        1,
        &candidate,
    );
    seed.request_diagnostic = Some(crate::observability::RequestDiagnostic {
        client_thread_id: Some("01a13ddb-8d21-7cb2-8f00-23a8a09af079".into()),
        ..Default::default()
    });
    seed.record_shared_provider_retries(shared.clone());
    let response = adapt_successful_response(
        guarded,
        &candidate,
        seed,
        ResponseToolMap::default(),
        &state.response_state,
        &json!({"input":[],"stream":true}),
        false,
        false,
    )
    .await;
    let mut body = response.into_body().into_data_stream();
    loop {
        let bytes = tokio::time::timeout(Duration::from_secs(2), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if String::from_utf8_lossy(&bytes).contains("response.function_call_arguments.delta") {
            break;
        }
    }
    sender
        .send(Bytes::from(format!(
            "data: {}\n\n",
            json!({"candidates":[{"finishReason":"MAX_TOKENS"}]})
        )))
        .await
        .unwrap();
    loop {
        match tokio::time::timeout(Duration::from_millis(50), body.next()).await {
            Err(_) => break,
            Ok(Some(Ok(bytes))) => {
                assert!(!String::from_utf8_lossy(&bytes).contains("response.failed"))
            }
            _ => panic!("unexpected stream termination before footer"),
        }
    }
    assert!(shared.0.lock().unwrap().terminal_failure);
    drop(body);
    assert_eq!(ledger.summary().failed_requests, 1);
    assert_eq!(ledger.summary().successful_requests, 0);
    assert_eq!(ledger.summary().input_tokens, 100_000);
    assert!(ledger.cache_health_summary().sessions.is_empty());
}
