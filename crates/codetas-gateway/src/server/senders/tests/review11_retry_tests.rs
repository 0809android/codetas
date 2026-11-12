use super::*;

fn usage(input: u64) -> TokenUsage {
    TokenUsage {
        input_tokens: input,
        output_tokens: 3,
        total_tokens: input + 3,
        cached_input_tokens: input / 2,
        cache_creation_input_tokens: 2,
        reasoning_tokens: 1,
        cache_read_reported: true,
    }
}
fn seed(ledger: &ObservabilityLedger) -> ObservationSeed {
    ObservationSeed::without_candidate(
        ledger.clone(),
        ObservabilitySettings::default(),
        "review11".into(),
        "fixture/model",
        false,
        Instant::now(),
    )
}

#[tokio::test]
async fn failed_empty_retry_accounts_http_and_empty_attempts_exactly_once() {
    let response = empty_completion_retry_failed_response_with_observation(
        usage(200),
        2,
        Some(ProviderRetryObservation {
            usage: usage(100),
            additional_sends: 1,
            ..Default::default()
        }),
    )
    .unwrap();
    let marker = response
        .extensions()
        .get::<EmptyCompletionRecoveryFailure>()
        .unwrap()
        .clone();
    let retry = response
        .extensions()
        .get::<ProviderRetryObservation>()
        .unwrap()
        .clone();
    assert_eq!(marker.usage.input_tokens, 300);
    assert_eq!(retry.additional_sends, 3);
    let body: Value =
        serde_json::from_slice(&read_bounded(response, 65536).await.unwrap()).unwrap();
    assert_eq!(TokenUsage::from_json(&body).input_tokens, 300);
    // Both the dedicated request failure consumer and generic compact
    // consumers must charge the same total, without a duplicate HTTP retry.
    for dedicated in [false, true] {
        let ledger = ObservabilityLedger::new(None);
        let mut observation = seed(&ledger);
        let mut metadata = retry.clone();
        if dedicated {
            metadata.usage = TokenUsage::default();
        }
        observation.record_provider_retries(&metadata);
        observation.finish(StatusCode::BAD_GATEWAY, Some("empty"), marker.usage.clone());
        let summary = ledger.summary();
        assert_eq!(summary.input_tokens, 300);
        assert_eq!(summary.output_tokens, 6);
        assert_eq!(summary.cached_input_tokens, 150);
        assert_eq!(summary.reasoning_tokens, 2);
    }
}

#[tokio::test]
async fn buffer_read_failure_retains_known_retries_and_wire_diagnostic() {
    for oversized in [false, true] {
        let mut response = if oversized {
            reqwest::Response::from(
                Response::builder()
                    .body(reqwest::Body::from("oversized"))
                    .unwrap(),
            )
        } else {
            let stream = futures_util::stream::iter(vec![Err::<Bytes, _>(std::io::Error::other(
                "fixture read failure",
            ))]);
            reqwest::Response::from(
                Response::builder()
                    .body(reqwest::Body::wrap_stream(stream))
                    .unwrap(),
            )
        };
        response.extensions_mut().insert(ProviderRetryObservation {
            usage: usage(123),
            additional_sends: 2,
            ..Default::default()
        });
        response
            .extensions_mut()
            .insert(crate::observability::RequestDiagnostic {
                wire_bytes: 456,
                ..Default::default()
            });
        let failure = match buffer_provider_response(response, 4).await {
            Err(failure) => failure,
            Ok(_) => panic!("must fail"),
        };
        let retry = failure
            .response
            .extensions()
            .get::<ProviderRetryObservation>()
            .unwrap();
        assert_eq!(retry.usage.input_tokens, 123);
        assert_eq!(retry.additional_sends, 2);
        assert_eq!(
            failure
                .response
                .extensions()
                .get::<crate::observability::RequestDiagnostic>()
                .unwrap()
                .wire_bytes,
            456
        );
    }
}

#[test]
fn sparse_cache_updates_preserve_known_fields_only_within_one_attempt() {
    for (protocol, first, next, zero) in [
        (
            ProviderProtocol::ChatCompletions,
            json!({"usage":{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":90}}}),
            json!({"usage":{"prompt_tokens":110,"completion_tokens":1}}),
            json!({"usage":{"prompt_tokens_details":{"cached_tokens":0}}}),
        ),
        (
            ProviderProtocol::GeminiGenerateContent,
            json!({"response":{"usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":90}}}),
            json!({"response":{"usageMetadata":{"promptTokenCount":110,"candidatesTokenCount":1}}}),
            json!({"response":{"usageMetadata":{"cachedContentTokenCount":0}}}),
        ),
    ] {
        let mut tracker = FinalCacheObservation::default();
        tracker.update(protocol, &first);
        tracker.update(protocol, &next);
        assert_eq!(tracker.value.input_tokens, 110);
        assert_eq!(tracker.value.cached_input_tokens, 90);
        assert!(tracker.value.cache_read_reported);
        tracker.update(protocol, &zero);
        assert_eq!(tracker.value.cached_input_tokens, 0);
        assert!(tracker.value.cache_read_reported);
        let mut another = FinalCacheObservation::default();
        another.update(protocol, &next);
        assert!(!another.value.cache_read_reported);
    }
}

#[test]
fn repaired_compaction_usage_roundtrips_every_counter_without_double_input() {
    let combined = sum_token_usage(usage(100), usage(200));
    let value = json!({"usage": token_usage_json(&combined)});
    let decoded = TokenUsage::from_json(&value);
    assert_eq!(decoded.input_tokens, 300);
    assert_eq!(decoded.output_tokens, 6);
    assert_eq!(decoded.cached_input_tokens, 150);
    assert_eq!(decoded.cache_creation_input_tokens, 4);
    assert_eq!(decoded.reasoning_tokens, 2);
    assert_eq!(decoded.total_tokens, 306);
    assert!(decoded.cache_read_reported);
}

#[tokio::test]
async fn oauth_retirement_releases_lease_and_preserves_rejected_usage() {
    let mut provider = crate::config::ProviderDefinition::default();
    provider.id = "fixture".into();
    provider.enabled = true;
    provider.models = vec!["model".into()];
    provider.credential.source = CredentialSource::None;
    let mut settings = GatewaySettings::default();
    settings.providers = vec![provider];
    let routing = Arc::new(Mutex::new(RoutingRuntime::default()));
    let candidate = routing
        .lock()
        .await
        .candidates_for_request(&settings, "fixture/model", false, None)
        .unwrap()
        .remove(0);
    routing.lock().await.begin_attempt(&candidate).unwrap();
    let lease = RoutingAttemptLease::new(Arc::new(RoutingAttemptLeaseInner {
        routing: routing.clone(),
        candidate: candidate.clone(),
    }));
    let weak = Arc::downgrade(&lease._inner);
    let mut response = reqwest::Response::from(
        Response::builder()
            .status(401)
            .body(reqwest::Body::from(
                json!({"usage":{"input_tokens":20,"output_tokens":3}}).to_string(),
            ))
            .unwrap(),
    );
    response.extensions_mut().insert(lease);
    response.extensions_mut().insert(ProviderRetryObservation {
        usage: usage(100),
        additional_sends: 2,
        ..Default::default()
    });
    response
        .extensions_mut()
        .insert(crate::observability::RequestDiagnostic {
            wire_bytes: 999,
            ..Default::default()
        });
    let (retry, _, diagnostic) = retire_oauth_response(response).await;
    assert!(
        weak.upgrade().is_none(),
        "first attempt must be retired before resend"
    );
    routing.lock().await.begin_attempt(&candidate).unwrap();
    routing.lock().await.end_attempt(&candidate);
    assert_eq!(retry.usage.input_tokens, 120);
    assert_eq!(retry.additional_sends, 3);
    assert_eq!(retry.recovery_kinds, ["oauth-refresh"]);
    assert_eq!(diagnostic.unwrap().wire_bytes, 999);
}
