use super::*;

pub(crate) async fn retry_provider_request<F, Fut>(
    state: &GatewayState,
    candidate: &RouteCandidate,
    streaming: bool,
    mut operation: F,
) -> Result<reqwest::Response, AttemptFailure>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, AttemptFailure>>,
{
    state
        .routing
        .lock()
        .await
        .begin_attempt(candidate)
        .map_err(|message| AttemptFailure {
            response: error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "routing_capacity",
                &message,
            ),
            kind: AttemptFailureKind::Request,
        })?;
    let lease = RoutingAttemptLease::new(Arc::new(RoutingAttemptLeaseInner {
        routing: Arc::clone(&state.routing),
        candidate: candidate.clone(),
    }));
    let transport_retries = if streaming {
        candidate.provider.limits.stream_retries
    } else {
        candidate.provider.limits.request_retries
    };
    let mut transport_attempts = 0_u8;
    let mut rate_limit_attempts = 0_u8;
    let mut sends = 0_u16;
    let mut retry_observation = ProviderRetryObservation::default();
    let result = async {
        loop {
            if let Err(mut failure) = await_provider_pacing(state, candidate).await {
                attach_retry_observation_to_failure(&mut failure, sends, retry_observation);
                return Err(failure);
            }
            sends = sends.saturating_add(1);
            match operation().await {
                Ok(mut response) if response.status() == StatusCode::TOO_MANY_REQUESTS => {
                    let credential_source = candidate
                        .credential
                        .as_ref()
                        .unwrap_or(&candidate.provider.credential)
                        .source;
                    if should_retry_rate_limit(
                        &candidate.provider.limits,
                        credential_source,
                        rate_limit_attempts,
                    ) {
                        let delay = rate_limit_retry_delay(response.headers(), rate_limit_attempts);
                        merge_retry_response_usage(&mut retry_observation, response).await;
                        retry_observation.recovery_kinds.push("rate-limit".into());
                        tokio::time::sleep(delay).await;
                        rate_limit_attempts = rate_limit_attempts.saturating_add(1);
                        continue;
                    }
                    attach_retry_observation(&mut response, sends, retry_observation);
                    return Ok(response);
                }
                Ok(mut response)
                    if response.status() == StatusCode::REQUEST_TIMEOUT
                        || response.status().is_server_error() =>
                {
                    if transport_attempts < transport_retries {
                        let recovery = if response.status() == StatusCode::REQUEST_TIMEOUT {
                            "request-timeout"
                        } else {
                            "provider-5xx"
                        };
                        let delay = validated_retry_after(response.headers())
                            .map(|(_, delay)| delay.unwrap_or(Duration::ZERO))
                            .unwrap_or_else(|| retry_backoff(transport_attempts));
                        merge_retry_response_usage(&mut retry_observation, response).await;
                        retry_observation.recovery_kinds.push(recovery.into());
                        tokio::time::sleep(delay.min(Duration::from_secs(5))).await;
                        transport_attempts = transport_attempts.saturating_add(1);
                        continue;
                    }
                    attach_retry_observation(&mut response, sends, retry_observation);
                    return Ok(response);
                }
                Ok(mut response) => {
                    attach_retry_observation(&mut response, sends, retry_observation);
                    return Ok(response);
                }
                Err(failure)
                    if failure.kind == AttemptFailureKind::Retryable
                        && transport_attempts < transport_retries =>
                {
                    let usage = response_body_usage(failure.response).await;
                    retry_observation.usage = sum_token_usage(retry_observation.usage, usage);
                    retry_observation.recovery_kinds.push("transport".into());
                    tokio::time::sleep(retry_backoff(transport_attempts)).await;
                    transport_attempts = transport_attempts.saturating_add(1);
                }
                Err(mut failure) => {
                    attach_retry_observation_to_failure(&mut failure, sends, retry_observation);
                    return Err(failure);
                }
            }
        }
    }
    .await;
    match result {
        Ok(mut response) => {
            response.extensions_mut().insert(lease);
            Ok(response)
        }
        Err(mut failure) => {
            failure.response.extensions_mut().insert(lease);
            Err(failure)
        }
    }
}

fn attach_retry_observation(
    response: &mut reqwest::Response,
    sends: u16,
    mut observation: ProviderRetryObservation,
) {
    observation.additional_sends = sends.saturating_sub(1);
    if retry_observation_has_evidence(&observation) {
        response.extensions_mut().insert(observation);
    }
}

fn attach_retry_observation_to_failure(
    failure: &mut AttemptFailure,
    sends: u16,
    mut observation: ProviderRetryObservation,
) {
    observation.additional_sends = sends.saturating_sub(1);
    if retry_observation_has_evidence(&observation) {
        failure.response.extensions_mut().insert(observation);
    }
}

fn retry_observation_has_evidence(observation: &ProviderRetryObservation) -> bool {
    observation.additional_sends > 0
        || !observation.recovery_kinds.is_empty()
        || observation.known_billing.is_some()
        || observation.cache_observation.is_some()
        || observation.terminal_failure
        || observation.usage.input_tokens > 0
        || observation.usage.output_tokens > 0
        || observation.usage.total_tokens > 0
        || observation.usage.cached_input_tokens > 0
        || observation.usage.cache_creation_input_tokens > 0
        || observation.usage.reasoning_tokens > 0
        || observation.usage.cache_read_reported
}

pub(crate) fn merge_provider_retry_observation(
    target: &mut Option<ProviderRetryObservation>,
    next: Option<ProviderRetryObservation>,
) {
    let Some(next) = next else {
        return;
    };
    if let Some(current) = target.as_mut() {
        current.additional_sends = current
            .additional_sends
            .saturating_add(next.additional_sends);
        current.recovery_kinds.extend(next.recovery_kinds);
        if let Some(billing) = next.known_billing {
            current.known_billing = Some(
                current
                    .known_billing
                    .take()
                    .map(|previous| sum_token_usage(previous, billing.clone()))
                    .unwrap_or(billing),
            );
        }
        current.terminal_failure |= next.terminal_failure;
        current.usage = sum_token_usage(current.usage.clone(), next.usage);
        if next.cache_observation.is_some() {
            current.cache_observation = next.cache_observation;
        }
        if next.request_diagnostic.is_some() {
            current.request_diagnostic = next.request_diagnostic;
        }
    } else {
        *target = Some(next);
    }
}

async fn merge_retry_response_usage(
    observation: &mut ProviderRetryObservation,
    response: reqwest::Response,
) {
    let usage = response_body_usage_from_reqwest(response).await;
    observation.usage = sum_token_usage(observation.usage.clone(), usage);
}

pub(super) fn retry_after_seconds_ceil(delay: Duration) -> u64 {
    delay
        .as_secs()
        .saturating_add(u64::from(delay.subsec_nanos() > 0))
        .max(1)
}

pub(crate) fn should_retry_rate_limit(
    limits: &crate::config::ProviderLimits,
    credential_source: CredentialSource,
    attempts: u8,
) -> bool {
    limits.retry_on_429
        && matches!(
            credential_source,
            CredentialSource::Environment | CredentialSource::Keychain | CredentialSource::Command
        )
        && attempts < limits.max_429_retries
}

pub(crate) fn rate_limit_retry_delay(headers: &HeaderMap, attempts: u8) -> Duration {
    validated_retry_after(headers)
        .map(|(_, delay)| delay.unwrap_or(Duration::ZERO))
        .unwrap_or_else(|| retry_backoff(attempts))
        .min(Duration::from_secs(10 * 60))
}

pub(crate) fn empty_completion_retry_enabled(
    candidate: &RouteCandidate,
    _streaming: bool,
    attempts: u8,
) -> bool {
    (model_matches_any(
        &candidate.upstream_model,
        &candidate.provider.empty_completion_retry_models,
    ) || model_matches_any(
        &candidate.upstream_model,
        &candidate.provider.terminal_continuation_guard_models,
    )) && attempts < candidate.provider.limits.empty_completion_retries
}

pub(crate) fn retry_backoff(attempt: u8) -> Duration {
    Duration::from_millis(250_u64.saturating_mul(1_u64 << attempt.min(4)))
}
