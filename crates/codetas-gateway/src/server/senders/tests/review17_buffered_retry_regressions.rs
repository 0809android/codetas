use super::super::adapt::review16_regressions::{candidate, response, seed};
use super::*;

fn usage(input: u64, output: u64) -> TokenUsage {
    TokenUsage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: input.saturating_add(output),
        ..Default::default()
    }
}
fn first() -> reqwest::Response {
    let mut response = response(
        vec![Ok(Bytes::from(
            json!({"status":"completed","output":[],
            "usage":{"input_tokens":13,"output_tokens":2,"input_tokens_details":{"cached_tokens":3},
                "output_tokens_details":{"reasoning_tokens":1}}})
            .to_string(),
        ))],
        StatusCode::OK,
    );
    response.extensions_mut().insert(ProviderRetryObservation {
        usage: usage(5, 1),
        additional_sends: 1,
        known_billing: Some(usage(13, 2)),
        ..Default::default()
    });
    response
}
fn final_value(reported: bool) -> Value {
    let mut value = json!({"status":"completed","output":[{"type":"message","role":"assistant",
            "content":[{"type":"output_text","text":"answer"}]}],
            "usage":{"input_tokens":100000,"output_tokens":7,
                "input_tokens_details":{"cache_creation_tokens":1000},
                "output_tokens_details":{"reasoning_tokens":3}}});
    if reported {
        value["usage"]["input_tokens_details"]["cached_tokens"] = json!(80000);
    }
    value
}
fn nested(response: &mut reqwest::Response) {
    response
        .extensions_mut()
        .insert(crate::observability::RequestDiagnostic {
            client_thread_id: Some("01a13ddb-8d21-7cb2-8f00-23a8a09af079".into()),
            ..Default::default()
        });
    response.extensions_mut().insert(ProviderRetryObservation {
        usage: usage(11, 2),
        additional_sends: 1,
        known_billing: Some(usage(100000, 7)),
        ..Default::default()
    });
}

#[tokio::test(start_paused = true)]
async fn review17_empty_retry_decode_limit_and_read_failure_bill_once() {
    for mode in ["trailing", "limit", "io", "invalid"] {
        let prefix = final_value(true).to_string();
        let limit = if mode == "limit" {
            prefix.len() as u64
        } else {
            8192
        };
        let chunks = match mode {
            "io" => vec![
                Ok(Bytes::from(prefix)),
                Err(std::io::Error::other("fixture")),
            ],
            "invalid" => vec![Ok(Bytes::from_static(b"not JSON"))],
            _ => vec![Ok(Bytes::from(format!("{prefix} trailing junk")))],
        };
        let mut last = response(chunks, StatusCode::OK);
        nested(&mut last);
        let mut last = Some(last);
        let result = guard_buffered_empty_completion_response(
            first(),
            limit,
            1,
            &ResponseToolMap::default(),
            || std::future::ready(Ok(last.take().expect("one physical retry"))),
        )
        .await
        .unwrap();
        assert_eq!(result.status(), StatusCode::BAD_GATEWAY);
        let billed = result
            .extensions()
            .get::<EmptyCompletionRecoveryFailure>()
            .unwrap()
            .usage
            .clone();
        let metadata = result
            .extensions()
            .get::<ProviderRetryObservation>()
            .unwrap()
            .clone();
        let expected_input = if mode == "invalid" { 29 } else { 100029 };
        let expected_output = if mode == "invalid" { 5 } else { 12 };
        assert_eq!(billed.input_tokens, expected_input, "{mode}");
        assert_eq!(billed.output_tokens, expected_output, "{mode}");
        assert_eq!(billed.total_tokens, expected_input + expected_output);
        assert_eq!(metadata.usage.total_tokens, 0);
        assert_eq!(
            metadata.known_billing.as_ref().unwrap().total_tokens,
            billed.total_tokens
        );
        assert_eq!(metadata.additional_sends, 3);
        if mode != "invalid" {
            assert_eq!(billed.cached_input_tokens, 80003);
            assert_eq!(billed.cache_creation_input_tokens, 1000);
            assert_eq!(billed.reasoning_tokens, 4);
        }
        let ledger = ObservabilityLedger::new(None);
        let mut observation = seed(&ledger, &candidate(), false);
        observation.record_provider_retries(&metadata);
        observation.finish(
            result.status(),
            Some(EMPTY_COMPLETION_RETRY_FAILED_CODE),
            billed.clone(),
        );
        assert_eq!(ledger.summary().total_tokens, billed.total_tokens);
        assert_eq!(ledger.summary().failed_requests, 1);
        assert!(ledger.cache_health_summary().sessions.is_empty());
        let value: Value = result.json().await.unwrap();
        assert_eq!(
            TokenUsage::from_json(&value).total_tokens,
            billed.total_tokens
        );
    }
}

#[tokio::test(start_paused = true)]
async fn review17_empty_retry_success_cache_is_only_final_physical_attempt() {
    for reported in [false, true] {
        let mut last = response(
            vec![Ok(Bytes::from(final_value(reported).to_string()))],
            StatusCode::OK,
        );
        nested(&mut last);
        let mut last = Some(last);
        let result = guard_buffered_empty_completion_response(
            first(),
            8192,
            1,
            &ResponseToolMap::default(),
            || std::future::ready(Ok(last.take().unwrap())),
        )
        .await
        .unwrap();
        let cache = result
            .extensions()
            .get::<crate::observability::CacheObservation>()
            .unwrap();
        assert_eq!(cache.input_tokens, 100000);
        assert_eq!(cache.cached_input_tokens, if reported { 80000 } else { 0 });
        assert_eq!(cache.cache_read_reported, reported);
        let ledger = ObservabilityLedger::new(None);
        let mut observation = seed(&ledger, &candidate(), false);
        observation.record_provider_retries(
            result
                .extensions()
                .get::<ProviderRetryObservation>()
                .unwrap(),
        );
        observation.record_reqwest_upstream_error(&result);
        let value: Value = result.json().await.unwrap();
        observation.finish(StatusCode::OK, None, TokenUsage::from_json(&value));
        assert_eq!(ledger.summary().input_tokens, 100029);
        assert_eq!(ledger.summary().total_tokens, 100041);
        let snapshot = ledger.cache_health_summary();
        assert_eq!(snapshot.sessions.len(), 1);
        let row = &snapshot.sessions[0];
        assert_eq!(row.input_tokens, 100000);
        assert_eq!(row.successful_requests, 1);
        assert_eq!(row.measured_requests, u64::from(reported));
        assert_eq!(row.unknown_cache_requests, u64::from(!reported));
        assert_eq!(row.cache_hit_percent, reported.then_some(80.0));
    }
}

#[tokio::test]
async fn review17_initial_decode_failure_keeps_prefix_without_retry_or_cache_sample() {
    let prefix = final_value(true).to_string();
    let initial = response(
        vec![Ok(Bytes::from(format!("{prefix} junk")))],
        StatusCode::OK,
    );
    let result = guard_buffered_empty_completion_response(
        initial,
        8192,
        1,
        &ResponseToolMap::default(),
        || std::future::ready(Err(request_failure("unexpected", "must not retry"))),
    )
    .await
    .unwrap();
    assert!(result
        .extensions()
        .get::<EmptyCompletionRecoveryFailure>()
        .is_none());
    let (_, billed) = super::super::stream::bounded_json_with_usage(result, 8192)
        .await
        .unwrap_err();
    assert_eq!(billed.input_tokens, 100000);
    assert_eq!(billed.output_tokens, 7);
}
