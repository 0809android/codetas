use super::*;
#[test]
fn raw_anthropic_retry_unknown_cache_preserves_total_and_does_not_fabricate_cache_report() {
    let unknown = TokenUsage::from_json(
        &json!({"usage":{"input_tokens":1000,"cache_creation_input_tokens":500,"output_tokens":1}}),
    );
    let known = TokenUsage::from_json(
        &json!({"usage":{"input_tokens":1000,"cache_read_input_tokens":99_000,"cache_creation_input_tokens":500,"output_tokens":1}}),
    );
    let aggregate = sum_token_usage(unknown, known);
    assert_eq!(aggregate.input_tokens, 102_000);
    assert!(!aggregate.cache_read_reported);
    let mut value = json!({"type":"message","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{}});
    write_token_usage(&mut value, aggregate);
    let adapted = anthropic_to_response(&value, "model", &ResponseToolMap::default()).unwrap();
    let usage = TokenUsage::from_json(&adapted);
    assert_eq!(usage.input_tokens, 102_000);
    assert!(!usage.cache_read_reported);
}
#[test]
fn chat_raw_cache_extension_is_a_subset_of_prompt_tokens_not_extra_tokens() {
    let value = json!({"usage":{"prompt_tokens":100_000,"cache_read_input_tokens":99_000,"completion_tokens":1}});
    assert_eq!(TokenUsage::from_json(&value).input_tokens, 100_000);
    assert_eq!(
        crate::observability::CacheObservation::from_provider_json(&value).input_tokens,
        100_000
    );
}
