use super::*;
use crate::config::ProviderLimits;

#[test]
fn retry_on_429_is_disabled_when_missing_or_defaulted() {
    let defaulted = ProviderLimits::default();
    let deserialized: ProviderLimits =
        serde_json::from_value(json!({})).expect("empty limits use safe defaults");

    assert!(!defaulted.retry_on_429);
    assert!(!deserialized.retry_on_429);
    assert!(!should_retry_rate_limit(
        &deserialized,
        CredentialSource::Environment,
        0,
    ));
}

#[test]
fn retry_on_429_requires_opt_in_and_api_key_equivalent_credentials() {
    let limits = ProviderLimits {
        retry_on_429: true,
        max_429_retries: 2,
        ..ProviderLimits::default()
    };

    assert!(should_retry_rate_limit(
        &limits,
        CredentialSource::Environment,
        0,
    ));
    assert!(should_retry_rate_limit(
        &limits,
        CredentialSource::Keychain,
        1,
    ));
    assert!(!should_retry_rate_limit(
        &limits,
        CredentialSource::OAuth,
        0,
    ));
    assert!(!should_retry_rate_limit(
        &limits,
        CredentialSource::Forward,
        0,
    ));
    assert!(!should_retry_rate_limit(
        &limits,
        CredentialSource::Environment,
        2,
    ));
}

#[test]
fn retry_after_is_not_truncated_to_the_old_five_second_cap() {
    let mut headers = HeaderMap::new();
    headers.insert(header::RETRY_AFTER, HeaderValue::from_static("30"));

    assert_eq!(rate_limit_retry_delay(&headers, 0), Duration::from_secs(30));
}

#[test]
fn same_key_wait_is_capped_without_shortening_routing_cooldown() {
    let mut headers = HeaderMap::new();
    headers.insert(header::RETRY_AFTER, HeaderValue::from_static("7200"));

    assert_eq!(
        rate_limit_retry_delay(&headers, 0),
        Duration::from_secs(10 * 60)
    );
    assert_eq!(
        validated_retry_after(&headers).and_then(|value| value.1),
        Some(Duration::from_secs(7_200))
    );
}
