use super::super::adapt::review16_regressions::{candidate, metadata, response};
use super::*;
#[tokio::test]
async fn review16_declared_length_rejections_preserve_usage_and_retries_once() {
    for kiro in [false, true] {
        let prefix = if kiro {
            metadata()
        } else {
            sse("response.completed",&json!({"type":"response.completed","response":{"usage":{"input_tokens":17,"output_tokens":3}}})).into_bytes()
        };
        let limit = prefix.len() as u64;
        let mut bytes = prefix;
        bytes.extend_from_slice(b"excess");
        let failure = reject_oversized_provider_response(
            response(vec![Ok(Bytes::from(bytes))], StatusCode::OK),
            limit,
            kiro,
        )
        .await;
        assert_eq!(failure.response.status(), StatusCode::BAD_GATEWAY);
        let usage = response_body_usage(failure.response).await;
        assert_eq!(usage.input_tokens, 17);
        assert_eq!(usage.output_tokens, 3);
        assert!(!usage.cache_read_reported);
    }
    let ledger = ObservabilityLedger::new(None);
    let state = super::review16_physical_guard_regressions::state(&ledger);
    let mut c = candidate();
    c.provider.limits.request_retries = 1;
    c.provider.limits.request_pacing_ms = 0;
    let mut settings = GatewaySettings::default();
    settings.providers.push(c.provider.clone());
    let target = format!("{}/{}", c.provider.id, c.upstream_model);
    let c = state
        .routing
        .lock()
        .await
        .candidates_scoped(&settings, &target, None)
        .unwrap()
        .remove(0);
    let sends = std::sync::atomic::AtomicU32::new(0);
    let result = retry_provider_request(&state, &c, false, || {
        let n = sends.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        async move {
            if n == 0 {
                let prefix = json!({"usage":{"input_tokens":17,"output_tokens":3}}).to_string();
                let limit = prefix.len() as u64;
                Err(reject_oversized_provider_response(
                    response(
                        vec![Ok(Bytes::from(format!("{prefix}excess")))],
                        StatusCode::OK,
                    ),
                    limit,
                    false,
                )
                .await)
            } else {
                Ok(response(
                    vec![Ok(Bytes::from(
                        json!({"usage":{"input_tokens":11,"output_tokens":2}}).to_string(),
                    ))],
                    StatusCode::OK,
                ))
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(sends.load(std::sync::atomic::Ordering::Relaxed), 2);
    let retry = result
        .extensions()
        .get::<ProviderRetryObservation>()
        .unwrap()
        .clone();
    assert_eq!(retry.usage.input_tokens, 17);
    assert_eq!(retry.additional_sends, 1);
    let mut seed = super::super::adapt::review16_regressions::seed(&ledger, &c, false);
    seed.record_provider_retries(&retry);
    let usage = response_body_usage_from_reqwest(result).await;
    seed.finish(StatusCode::BAD_GATEWAY, Some("fixture-failure"), usage);
    assert_eq!(ledger.summary().input_tokens, 28);
    assert_eq!(ledger.summary().output_tokens, 5);
    assert!(ledger.cache_health_summary().sessions.is_empty());
}
#[tokio::test]
async fn review16_physical_gemini_io_and_invalid_json_keep_earlier_usage() {
    for mode in ["io", "json"] {
        let prefix = sse(
            "message",
            &json!({"usageMetadata":{"promptTokenCount":17,"candidatesTokenCount":3}}),
        );
        let chunks = if mode == "io" {
            vec![
                Ok(Bytes::from(prefix)),
                Err(std::io::Error::other("fixture")),
            ]
        } else {
            vec![Ok(Bytes::from(format!("{prefix}data: {{invalid\n\n")))]
        };
        let mut source = provider_guard_stream_for_protocol(
            response(chunks, StatusCode::OK),
            false,
            ProviderProtocol::GeminiGenerateContent,
            4096,
        );
        let mut failed = false;
        while let Some(chunk) = source.next().await {
            if let Err(error) = chunk {
                let failure = error
                    .get_ref()
                    .unwrap()
                    .downcast_ref::<ProviderStreamReadFailure>()
                    .unwrap();
                assert_eq!(failure.usage.input_tokens, 17);
                assert_eq!(failure.usage.output_tokens, 3);
                failed = true;
            }
        }
        assert!(failed);
    }
}
