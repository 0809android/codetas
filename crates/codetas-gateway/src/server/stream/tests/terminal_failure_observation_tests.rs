use super::*;
fn observation(ledger: &ObservabilityLedger, id: &str) -> ObservationSeed {
    let mut seed = ObservationSeed::without_candidate(
        ledger.clone(),
        ObservabilitySettings::default(),
        id.into(),
        "test/model",
        false,
        Instant::now(),
    );
    seed.provider_id = Some("test".into());
    seed.upstream_model = Some("model".into());
    seed.send_count = 1;
    seed.request_diagnostic = Some(crate::observability::RequestDiagnostic {
        client_thread_id: Some("01a13ddb-8d21-7cb2-8f00-23a8a09af079".into()),
        ..Default::default()
    });
    seed
}
#[tokio::test]
async fn cache_health_chat_json_extension_is_measured_without_empty_retry_guard() {
    for cached in [0, 99_000] {
        let ledger = ObservabilityLedger::new(None);
        for n in 0..20 {
            let value = json!({"id":"chat","choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":100_000,"completion_tokens":1,"cache_read_input_tokens":cached}});
            let upstream = Response::builder()
                .status(200)
                .body(reqwest::Body::from(value.to_string()))
                .unwrap();
            let response = chat_json_response(
                reqwest::Response::from(upstream),
                "model",
                1_000_000,
                observation(&ledger, &format!("chat-{n}")),
                ResponseToolMap::default(),
                Arc::new(ResponseStateStore::new(None)),
                json!({"model":"model","input":[]}),
                false,
                false,
            )
            .await;
            let _ = to_bytes(response.into_body(), 1_000_000).await.unwrap();
        }
        let row = &ledger.cache_health_summary().sessions[0];
        assert_eq!(row.measured_requests, 20);
        assert_eq!(row.unknown_cache_requests, 0);
        assert_eq!(row.active_alert, cached == 0);
    }
}
#[tokio::test]
async fn explicit_failed_terminal_is_committed_before_close_even_after_actionable_tool_delta() {
    for tool in [false, true] {
        for io_error in [false, true] {
            let ledger = ObservabilityLedger::new(None);
            let delta = if tool {
                json!({"tool_calls":[{"index":0,"id":"c","type":"function","function":{"name":"lookup","arguments":"{}"}}]})
            } else {
                json!({"role":"assistant","content":"work"})
            };
            let first = format!(
                "data: {}\n\n",
                json!({"id":"chat","choices":[{"index":0,"delta":delta,"finish_reason":null}],"usage":{"prompt_tokens":100_000,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":0}}})
            );
            let end = if io_error {
                Err(std::io::Error::other("fixture stream error"))
            } else {
                Ok(Bytes::from(format!(
                    "data: {}\n\n",
                    json!({"error":{"message":"fixture upstream rejection"}})
                )))
            };
            let stream = futures_util::stream::iter(vec![
                Ok::<Bytes, std::io::Error>(Bytes::from(first)),
                end,
            ]);
            let upstream = Response::builder()
                .status(200)
                .body(reqwest::Body::wrap_stream(stream))
                .unwrap();
            let response = translated_stream_response(
                reqwest::Response::from(upstream),
                "model".into(),
                StreamAdapter::Chat,
                1_000_000,
                Duration::from_secs(5),
                observation(&ledger, "failed"),
                false,
                ResponseToolMap::default(),
                ToolProgressPolicy::default(),
                Arc::new(ResponseStateStore::new(None)),
                json!({"model":"model","input":[]}),
                false,
                false,
                false,
            );
            let mut body = response.into_body().into_data_stream();
            let mut failed = false;
            while let Some(chunk) = body.next().await {
                if String::from_utf8_lossy(&chunk.unwrap()).contains("response.failed") {
                    failed = true;
                    break;
                }
            }
            assert!(failed);
            drop(body);
            let summary = ledger.summary();
            assert_eq!(summary.failed_requests, 1);
            assert_eq!(summary.successful_requests, 0);
            assert_eq!(summary.input_tokens, 100_000);
            assert!(ledger.cache_health_summary().sessions.is_empty());
        }
    }
}
