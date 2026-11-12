use super::super::adapt::review16_regressions::{candidate, response, seed};
use super::*;
pub(super) fn state(ledger: &ObservabilityLedger) -> GatewayState {
    let settings = Arc::new(RwLock::new(GatewaySettings::default()));
    let memory = Arc::new(MemoryAdmission {
        settings: settings.clone(),
        inflight: AtomicU32::new(0),
        reserved_bytes: AtomicU64::new(0),
        rejected: AtomicU64::new(0),
    });
    GatewayState {
        settings,
        client: reqwest::Client::new(),
        routing: Arc::new(Mutex::new(RoutingRuntime::default())),
        observability: ledger.clone(),
        video_jobs: Arc::new(Mutex::new(HashMap::new())),
        instance_id: "test".into(),
        response_state: Arc::new(ResponseStateStore::new(None)),
        memory,
        pacing: Arc::new(ProviderPacing::default()),
    }
}
#[tokio::test]
async fn review16_physical_guards_limits_keep_billing_once_and_fail_downstream() {
    for protocol in [
        ProviderProtocol::Responses,
        ProviderProtocol::ChatCompletions,
        ProviderProtocol::AnthropicMessages,
        ProviderProtocol::GeminiGenerateContent,
    ] {
        let mut c = candidate();
        c.provider.protocol = protocol;
        let prefix = match protocol {
            ProviderProtocol::Responses => sse(
                "response.completed",
                &json!({"type":"response.completed","response":{"id":"physical","status":"completed","output":[],"usage":{"input_tokens":17,"output_tokens":3}}}),
            ),
            ProviderProtocol::AnthropicMessages => sse(
                "message_start",
                &json!({"type":"message_start","message":{"usage":{"input_tokens":17,"output_tokens":3}}}),
            ),
            ProviderProtocol::GeminiGenerateContent => sse(
                "message",
                &json!({"usageMetadata":{"promptTokenCount":17,"candidatesTokenCount":3},"candidates":[{"finishReason":"STOP"}]}),
            ),
            _ => sse(
                "message",
                &json!({"usage":{"prompt_tokens":17,"completion_tokens":3},"choices":[]}),
            ),
        };
        c.provider.limits.max_response_bytes = prefix.len() as u64;
        let ledger = ObservabilityLedger::new(None);
        let state = state(&ledger);
        let mut upstream = response(
            vec![Ok(Bytes::from(format!("{prefix}excess")))],
            StatusCode::OK,
        );
        upstream.extensions_mut().insert(ProviderRetryObservation {
            usage: TokenUsage {
                input_tokens: 7,
                total_tokens: 7,
                ..Default::default()
            },
            additional_sends: 1,
            ..Default::default()
        });
        let request = json!({"input":"test","stream":true,"store":false});
        let guarded = guarded_provider_sse(
            state,
            request.clone(),
            c.clone(),
            None,
            upstream,
            None,
            None,
        )
        .unwrap();
        let shared = guarded
            .extensions()
            .get::<SharedProviderRetryObservation>()
            .unwrap()
            .clone();
        let mut observation = seed(&ledger, &c, true);
        observation.record_provider_retries(
            guarded
                .extensions()
                .get::<ProviderRetryObservation>()
                .unwrap(),
        );
        observation.record_shared_provider_retries(shared.clone());
        let client = adapt_successful_response(
            guarded,
            &c,
            observation,
            ResponseToolMap::default(),
            &Arc::new(ResponseStateStore::new(None)),
            &request,
            true,
            false,
        )
        .await;
        let mut source = client.into_body().into_data_stream();
        let mut text = Vec::new();
        while let Some(chunk) = source.next().await {
            if let Ok(bytes) = chunk {
                text.extend_from_slice(&bytes);
            } else {
                break;
            }
        }
        drop(source);
        assert!(
            !String::from_utf8_lossy(&text).contains("response.completed"),
            "{protocol:?}"
        );
        assert!(shared.0.lock().unwrap().terminal_failure);
        assert_eq!(ledger.summary().input_tokens, 24, "{protocol:?}");
        assert_eq!(ledger.summary().output_tokens, 3);
        assert_eq!(ledger.summary().successful_requests, 0);
        assert_eq!(ledger.summary().failed_requests, 1);
        assert!(ledger.cache_health_summary().sessions.is_empty());
    }
}
