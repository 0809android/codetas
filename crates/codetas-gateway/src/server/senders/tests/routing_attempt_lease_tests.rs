use super::*;
use crate::config::{ProviderCapabilities, ProviderDefinition};

fn candidate(epoch: u64) -> RouteCandidate {
    RouteCandidate {
        provider: ProviderDefinition::default(),
        upstream_model: "model".into(),
        exposed_model: "model".into(),
        credential: None,
        account_id: None,
        target_key: "provider/model".into(),
        route_id: None,
        failure_threshold: 3,
        input_price_per_million: None,
        output_price_per_million: None,
        context_window: None,
        max_input_tokens: None,
        max_output_tokens: None,
        reasoning_efforts: Vec::new(),
        default_reasoning_effort: None,
        capabilities: ProviderCapabilities::default(),
        routing_epoch: epoch,
        routing_generation: 0,
        session_scope: None,
    }
}

fn response_with_body(body: reqwest::Body) -> reqwest::Response {
    axum::http::Response::builder()
        .status(StatusCode::OK)
        .body(body)
        .map(reqwest::Response::from)
        .expect("provider response")
}

async fn leased_response(
    body: reqwest::Body,
) -> (
    reqwest::Response,
    Arc<Mutex<RoutingRuntime>>,
    RouteCandidate,
) {
    let routing = Arc::new(Mutex::new(RoutingRuntime::default()));
    let epoch = routing.lock().await.epoch();
    let candidate = candidate(epoch);
    routing
        .lock()
        .await
        .begin_attempt(&candidate)
        .expect("attempt lease");
    let lease = RoutingAttemptLease::new(Arc::new(RoutingAttemptLeaseInner {
        routing: Arc::clone(&routing),
        candidate: candidate.clone(),
    }));
    let mut response = response_with_body(body);
    response.extensions_mut().insert(lease);
    (response, routing, candidate)
}

async fn assert_attempt_released(routing: &Arc<Mutex<RoutingRuntime>>, candidate: &RouteCandidate) {
    for _ in 0..16 {
        if routing.lock().await.active_attempts(candidate) == 0 {
            return;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(routing.lock().await.active_attempts(candidate), 0);
}

#[tokio::test]
async fn buffered_response_and_bounded_reader_hold_attempt_until_body_eof() {
    let (response, routing, candidate) = leased_response(reqwest::Body::from("complete")).await;
    let buffered = buffer_provider_response(response, 1024)
        .await
        .expect("buffered provider response");
    assert_eq!(routing.lock().await.active_attempts(&candidate), 1);

    let rebuilt = buffered.into_response().expect("rebuilt response");
    assert_eq!(routing.lock().await.active_attempts(&candidate), 1);
    assert_eq!(
        read_bounded(rebuilt, 1024).await.expect("body"),
        b"complete".to_vec()
    );
    assert_attempt_released(&routing, &candidate).await;
}

#[tokio::test]
async fn cancelling_guarded_body_releases_attempt_lease() {
    let pending = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();
    let (response, routing, candidate) = leased_response(reqwest::Body::wrap_stream(pending)).await;
    let guarded = provider_guard_stream(response, false, false);
    assert_eq!(routing.lock().await.active_attempts(&candidate), 1);
    drop(guarded);
    assert_attempt_released(&routing, &candidate).await;
}
