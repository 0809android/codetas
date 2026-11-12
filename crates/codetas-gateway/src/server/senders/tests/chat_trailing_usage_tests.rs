use super::*;
#[tokio::test]
async fn cache_health_chat_guard_adapter_monitor_includes_late_usage_in_same_and_split_chunks() {
    for split in [false, true] {
        let mut settings = GatewaySettings::default();
        settings.security.dns_pinning = false;
        let settings = Arc::new(RwLock::new(settings));
        let ledger = ObservabilityLedger::new(None);
        let response_state = Arc::new(ResponseStateStore::new(None));
        let state = GatewayState {
            settings: settings.clone(),
            client: reqwest::Client::new(),
            routing: Arc::new(Mutex::new(RoutingRuntime::default())),
            observability: ledger.clone(),
            video_jobs: Arc::new(Mutex::new(HashMap::new())),
            instance_id: "chat-usage-test".into(),
            response_state: response_state.clone(),
            memory: Arc::new(MemoryAdmission {
                settings,
                inflight: AtomicU32::new(0),
                reserved_bytes: AtomicU64::new(0),
                rejected: AtomicU64::new(0),
            }),
            pacing: Arc::new(ProviderPacing::default()),
        };
        let mut provider = crate::config::ProviderDefinition::default();
        provider.id = "chat".into();
        provider.protocol = ProviderProtocol::ChatCompletions;
        let candidate = RouteCandidate {
            provider,
            upstream_model: "model".into(),
            exposed_model: "model".into(),
            credential: None,
            account_id: None,
            target_key: "chat/model".into(),
            route_id: None,
            failure_threshold: 3,
            input_price_per_million: None,
            output_price_per_million: None,
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            capabilities: crate::config::ProviderCapabilities::default(),
            routing_epoch: 0,
            routing_generation: 0,
            session_scope: None,
        };
        for n in 0..20 {
            let head = format!(
                "data: {}\n\ndata: {}\n\n",
                json!({"id":"chat-test","choices":[{"index":0,"delta":{"role":"assistant","content":"ok"},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
            );
            let tail = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[],"usage":{"prompt_tokens":100_000,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":0}}})
            );
            let chunks = if split {
                vec![head, tail]
            } else {
                vec![format!("{head}{tail}")]
            };
            let source = futures_util::stream::iter(
                chunks
                    .into_iter()
                    .map(|s| Ok::<_, std::io::Error>(Bytes::from(s))),
            );
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
            let mut observation = ObservationSeed::for_candidate(
                ledger.clone(),
                ObservabilitySettings::default(),
                format!("chat-{split}-{n}"),
                true,
                Instant::now(),
                1,
                &candidate,
            );
            observation.request_diagnostic = Some(crate::observability::RequestDiagnostic {
                client_thread_id: Some("01a13ddb-8d21-7cb2-8f00-23a8a09af079".into()),
                ..Default::default()
            });
            observation.record_shared_provider_retries(shared);
            let adapted = adapt_successful_response(
                guarded,
                &candidate,
                observation,
                ResponseToolMap::default(),
                &response_state,
                &json!({"model":"model","stream":true,"input":[]}),
                false,
                false,
            )
            .await;
            let bytes = to_bytes(adapted.into_body(), 1_000_000).await.unwrap();
            assert!(String::from_utf8_lossy(&bytes).contains("response.completed"));
        }
        let row = &ledger.cache_health_summary().sessions[0];
        assert_eq!(row.successful_requests, 20);
        assert_eq!(row.unknown_cache_requests, 0);
        assert_eq!(row.rolling_requests, 20);
        assert!(row.active_alert);
    }
}
