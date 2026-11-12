use super::*;
#[tokio::test]
async fn malformed_gemini_json_retains_reported_usage_on_translation_failure() {
    let ledger = ObservabilityLedger::new(None);
    let mut seed = ObservationSeed::without_candidate(
        ledger.clone(),
        ObservabilitySettings::default(),
        "malformed-gemini".into(),
        "test/gemini",
        false,
        Instant::now(),
    );
    seed.provider_id = Some("test".into());
    seed.upstream_model = Some("gemini".into());
    seed.send_count = 1;
    let value = json!({"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"MALFORMED_FUNCTION_CALL"}],"usageMetadata":{"promptTokenCount":100_000,"candidatesTokenCount":1,"cachedContentTokenCount":0}});
    let raw = Response::builder()
        .status(200)
        .body(reqwest::Body::from(value.to_string()))
        .unwrap();
    let response = adapted_json_response(
        reqwest::Response::from(raw),
        "model",
        gemini_to_response,
        1_000_000,
        seed,
        false,
        ResponseToolMap::default(),
        Arc::new(ResponseStateStore::new(None)),
        json!({"input":[]}),
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(ledger.summary().failed_requests, 1);
    assert_eq!(ledger.summary().input_tokens, 100_000);
    assert!(ledger.cache_health_summary().sessions.is_empty());
}
#[test]
fn nullable_cache_detail_does_not_hide_valid_chat_extension() {
    for cached in [0, 99_000] {
        let raw = json!({"prompt_tokens":100_000,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":null},"cache_read_input_tokens":cached});
        let usage = crate::translate::chat_usage_to_responses(Some(&raw));
        assert_eq!(usage["input_tokens"], 100_000);
        assert_eq!(usage["input_tokens_details"]["cached_tokens"], cached);
    }
}
