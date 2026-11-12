use super::*;
#[tokio::test]
async fn cache_health_chat_empty_length_and_filter_keep_usage_without_retry() {
    for reason in ["length", "content_filter", "max_tokens"] {
        let settings = Arc::new(RwLock::new(GatewaySettings::default()));
        let state = GatewayState {
            settings: settings.clone(),
            client: reqwest::Client::new(),
            routing: Arc::new(Mutex::new(RoutingRuntime::default())),
            observability: ObservabilityLedger::new(None),
            video_jobs: Arc::new(Mutex::new(HashMap::new())),
            instance_id: "chat-terminal-test".into(),
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
        provider.protocol = ProviderProtocol::ChatCompletions;
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
        let raw = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]}),
            json!({"choices":[],"usage":{"prompt_tokens":100_000,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":99_000}}})
        );
        let upstream = Response::builder()
            .status(200)
            .body(reqwest::Body::from(raw))
            .unwrap();
        let guarded = guarded_provider_sse(
            state,
            json!({}),
            candidate,
            None,
            reqwest::Response::from(upstream),
            None,
            None,
        )
        .unwrap();
        let shared = guarded
            .extensions()
            .get::<SharedProviderRetryObservation>()
            .unwrap()
            .clone();
        let bytes = read_bounded(guarded, 1_000_000).await.unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains(reason));
        assert!(text.contains("100000"));
        assert!(!text.contains("codetas-recovery"));
        assert_eq!(shared.0.lock().unwrap().additional_sends, 0);
    }
}
