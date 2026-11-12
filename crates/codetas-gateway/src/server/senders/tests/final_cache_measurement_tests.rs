use super::*;
#[tokio::test]
async fn cache_health_buffer_keeps_raw_final_measurement_after_billing_rewrite() {
    let response = axum::http::Response::builder()
        .status(200)
        .body(reqwest::Body::from(
            json!({"choices":[{"message":{"content":"ok"}}]}).to_string(),
        ))
        .unwrap();
    let mut buffered = buffer_provider_response(reqwest::Response::from(response), 1_000_000)
        .await
        .unwrap();
    assert!(!buffered.cache_observation.cache_read_reported);
    let mut rewritten = buffered.value.take().unwrap();
    write_token_usage(
        &mut rewritten,
        TokenUsage {
            input_tokens: 100_000,
            cache_read_reported: true,
            ..Default::default()
        },
    );
    buffered.bytes = Bytes::from(rewritten.to_string());
    let again = buffer_provider_response(buffered.into_response().unwrap(), 1_000_000)
        .await
        .unwrap();
    assert!(!again.cache_observation.cache_read_reported);
    assert_eq!(again.cache_observation.input_tokens, 0);
    assert_eq!(
        TokenUsage::from_json(again.value.as_ref().unwrap()).input_tokens,
        100_000
    );
}
#[test]
fn cache_health_sse_final_measurement_keeps_unknown_and_explicit_zero_distinct() {
    for (protocol, missing, zero) in [
        (
            ProviderProtocol::ChatCompletions,
            json!({"usage":{"prompt_tokens":100_000}}),
            json!({"usage":{"prompt_tokens":100_000,"prompt_tokens_details":{"cached_tokens":0}}}),
        ),
        (
            ProviderProtocol::Responses,
            json!({"response":{"usage":{"input_tokens":100_000}}}),
            json!({"response":{"usage":{"input_tokens":100_000,"input_tokens_details":{"cached_tokens":0}}}}),
        ),
        (
            ProviderProtocol::GeminiGenerateContent,
            json!({"usageMetadata":{"promptTokenCount":100_000}}),
            json!({"usageMetadata":{"promptTokenCount":100_000,"cachedContentTokenCount":0}}),
        ),
    ] {
        let mut state = FinalCacheObservation::default();
        state.update(protocol, &missing);
        assert_eq!(state.value.input_tokens, 100_000);
        assert!(!state.value.cache_read_reported);
        state.update(protocol, &zero);
        assert!(state.value.cache_read_reported);
        assert_eq!(state.value.cached_input_tokens, 0);
        state = FinalCacheObservation::default();
        state.update(protocol, &json!({"choices":[{"delta":{"content":"ok"}}]}));
        assert_eq!(state.value.input_tokens, 0);
        assert!(!state.value.cache_read_reported);
    }
}
#[test]
fn cache_health_anthropic_missing_cache_does_not_become_reported_zero() {
    let mut state = FinalCacheObservation::default();
    state.update(ProviderProtocol::AnthropicMessages,&json!({"type":"message_start","message":{"id":"m","role":"assistant","content":[],"usage":{"input_tokens":100_000,"output_tokens":0}}}));
    state.update(ProviderProtocol::AnthropicMessages,&json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}));
    assert_eq!(state.value.input_tokens, 100_000);
    assert!(!state.value.cache_read_reported);
}
#[test]
fn cache_health_anthropic_sparse_delta_preserves_real_cache_sample() {
    let mut state = FinalCacheObservation::default();
    state.update(ProviderProtocol::AnthropicMessages,&json!({"type":"message_start","message":{"id":"m","role":"assistant","content":[],"usage":{"input_tokens":1000,"cache_read_input_tokens":99_000,"cache_creation_input_tokens":0,"output_tokens":0}}}));
    state.update(ProviderProtocol::AnthropicMessages,&json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}));
    assert_eq!(state.value.input_tokens, 100_000);
    assert_eq!(state.value.cached_input_tokens, 99_000);
    assert!(state.value.cache_read_reported);
}
