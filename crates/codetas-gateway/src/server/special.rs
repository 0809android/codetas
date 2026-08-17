use super::*;

pub(crate) async fn send_special_candidate(
    state: &GatewayState,
    caller_headers: &HeaderMap,
    body: &Value,
    candidate: &RouteCandidate,
    kind: SpecialRelayKind,
) -> Result<reqwest::Response, AttemptFailure> {
    retry_provider_request(state, candidate, false, || {
        send_special_candidate_once(state, caller_headers, body, candidate, kind)
    })
    .await
}

pub(crate) async fn send_special_candidate_once(
    state: &GatewayState,
    caller_headers: &HeaderMap,
    body: &Value,
    candidate: &RouteCandidate,
    kind: SpecialRelayKind,
) -> Result<reqwest::Response, AttemptFailure> {
    let serialized = serde_json::to_vec(body).map_err(|error| {
        request_failure(
            "invalid_request",
            &format!("request cannot be encoded: {error}"),
        )
    })?;
    if serialized.len() as u64 > candidate.provider.limits.max_request_bytes {
        return Err(request_failure(
            "request_too_large",
            "relay request exceeds the configured provider limit",
        ));
    }
    let endpoint = kind.endpoint(candidate);
    let dns_pinning = state.settings.read().await.security.dns_pinning;
    let client = if dns_pinning {
        pinned_client(&endpoint, &candidate.provider, "CODETAS-Gateway/0.1")
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "provider_dns_rejected",
                    &message,
                ),
                kind: AttemptFailureKind::Retryable,
            })?
    } else {
        state.client.clone()
    };
    let request = client
        .post(endpoint)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json")
        .query(&candidate.provider.query_params)
        .timeout(Duration::from_millis(
            candidate.provider.limits.request_timeout_ms,
        ));
    let request = apply_provider_auth(request, &candidate.provider, candidate.credential.as_ref())
        .await
        .map_err(|message| AttemptFailure {
            response: error_response(
                StatusCode::UNAUTHORIZED,
                "missing_provider_credential",
                &message,
            ),
            kind: AttemptFailureKind::Credential,
        })?;
    let source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let request = if source == CredentialSource::Forward {
        apply_forward_headers(request, &state.settings, caller_headers)
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?
    } else {
        request
    };
    match request.body(serialized).send().await {
        Ok(response)
            if response
                .content_length()
                .is_some_and(|length| length > candidate.provider.limits.max_response_bytes) =>
        {
            Err(AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "provider_response_too_large",
                    "provider response exceeds the configured limit",
                ),
                kind: AttemptFailureKind::Retryable,
            })
        }
        Ok(response) => Ok(response),
        Err(_) => Err(AttemptFailure {
            response: error_response(
                StatusCode::BAD_GATEWAY,
                "provider_unreachable",
                "provider request failed",
            ),
            kind: AttemptFailureKind::Retryable,
        }),
    }
}

pub(crate) async fn send_video_status_candidate(
    state: &GatewayState,
    caller_headers: &HeaderMap,
    candidate: &RouteCandidate,
    request_id: &str,
) -> Result<reqwest::Response, AttemptFailure> {
    retry_provider_request(state, candidate, false, || {
        send_video_status_candidate_once(state, caller_headers, candidate, request_id)
    })
    .await
}

pub(crate) async fn send_video_status_candidate_once(
    state: &GatewayState,
    caller_headers: &HeaderMap,
    candidate: &RouteCandidate,
    request_id: &str,
) -> Result<reqwest::Response, AttemptFailure> {
    let root = candidate
        .provider
        .video_endpoint()
        .strip_suffix("/generations")
        .unwrap_or(candidate.provider.base_url.trim().trim_end_matches('/'))
        .to_string();
    let endpoint = format!("{root}/{}", request_id);
    let dns_pinning = state.settings.read().await.security.dns_pinning;
    let client = if dns_pinning {
        pinned_client(&endpoint, &candidate.provider, "CODETAS-Gateway/0.1")
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "provider_dns_rejected",
                    &message,
                ),
                kind: AttemptFailureKind::Retryable,
            })?
    } else {
        state.client.clone()
    };
    let request = client
        .get(endpoint)
        .header(header::ACCEPT, "application/json")
        .query(&candidate.provider.query_params)
        .timeout(Duration::from_millis(
            candidate.provider.limits.request_timeout_ms,
        ));
    let request = apply_provider_auth(request, &candidate.provider, candidate.credential.as_ref())
        .await
        .map_err(|message| AttemptFailure {
            response: error_response(
                StatusCode::UNAUTHORIZED,
                "missing_provider_credential",
                &message,
            ),
            kind: AttemptFailureKind::Credential,
        })?;
    let source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let request = if source == CredentialSource::Forward {
        apply_forward_headers(request, &state.settings, caller_headers)
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?
    } else {
        request
    };
    request.send().await.map_err(|_| AttemptFailure {
        response: error_response(
            StatusCode::BAD_GATEWAY,
            "provider_unreachable",
            "video status provider was unreachable",
        ),
        kind: AttemptFailureKind::Retryable,
    })
}

pub(crate) async fn send_special_raw_candidate(
    state: &GatewayState,
    caller_headers: &HeaderMap,
    body: Vec<u8>,
    content_type: &str,
    candidate: &RouteCandidate,
    kind: SpecialRelayKind,
) -> Result<reqwest::Response, AttemptFailure> {
    retry_provider_request(state, candidate, false, || {
        send_special_raw_candidate_once(state, caller_headers, &body, content_type, candidate, kind)
    })
    .await
}

pub(crate) async fn send_special_raw_candidate_once(
    state: &GatewayState,
    caller_headers: &HeaderMap,
    body: &[u8],
    content_type: &str,
    candidate: &RouteCandidate,
    kind: SpecialRelayKind,
) -> Result<reqwest::Response, AttemptFailure> {
    if body.len() as u64 > candidate.provider.limits.max_request_bytes {
        return Err(request_failure(
            "request_too_large",
            "multipart relay request exceeds the configured provider limit",
        ));
    }
    let endpoint = kind.endpoint(candidate);
    let dns_pinning = state.settings.read().await.security.dns_pinning;
    let client = if dns_pinning {
        pinned_client(&endpoint, &candidate.provider, "CODETAS-Gateway/0.1")
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "provider_dns_rejected",
                    &message,
                ),
                kind: AttemptFailureKind::Retryable,
            })?
    } else {
        state.client.clone()
    };
    let request = client
        .post(endpoint)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT, "application/json")
        .query(&candidate.provider.query_params)
        .timeout(Duration::from_millis(
            candidate.provider.limits.request_timeout_ms,
        ));
    let request = apply_provider_auth(request, &candidate.provider, candidate.credential.as_ref())
        .await
        .map_err(|message| AttemptFailure {
            response: error_response(
                StatusCode::UNAUTHORIZED,
                "missing_provider_credential",
                &message,
            ),
            kind: AttemptFailureKind::Credential,
        })?;
    let source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let request = if source == CredentialSource::Forward {
        apply_forward_headers(request, &state.settings, caller_headers)
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?
    } else {
        request
    };
    match request.body(body.to_vec()).send().await {
        Ok(response)
            if response
                .content_length()
                .is_some_and(|length| length > candidate.provider.limits.max_response_bytes) =>
        {
            Err(AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "provider_response_too_large",
                    "provider response exceeds the configured limit",
                ),
                kind: AttemptFailureKind::Retryable,
            })
        }
        Ok(response) => Ok(response),
        Err(_) => Err(AttemptFailure {
            response: error_response(
                StatusCode::BAD_GATEWAY,
                "provider_unreachable",
                "provider request failed",
            ),
            kind: AttemptFailureKind::Retryable,
        }),
    }
}

pub(crate) async fn send_realtime_candidate(
    state: &GatewayState,
    caller_headers: &HeaderMap,
    body: Vec<u8>,
    content_type: &str,
    inbound_path: &str,
    candidate: &RouteCandidate,
) -> Result<reqwest::Response, AttemptFailure> {
    retry_provider_request(state, candidate, false, || {
        send_realtime_candidate_once(
            state,
            caller_headers,
            &body,
            content_type,
            inbound_path,
            candidate,
        )
    })
    .await
}

pub(crate) async fn send_realtime_candidate_once(
    state: &GatewayState,
    caller_headers: &HeaderMap,
    body: &[u8],
    content_type: &str,
    inbound_path: &str,
    candidate: &RouteCandidate,
) -> Result<reqwest::Response, AttemptFailure> {
    if body.len() > 16 * 1024 * 1024
        || body.len() as u64 > candidate.provider.limits.max_request_bytes
    {
        return Err(request_failure(
            "request_too_large",
            "realtime request exceeds the configured provider limit",
        ));
    }
    let credential_source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let endpoint = realtime_endpoint(candidate, credential_source, inbound_path);
    let dns_pinning = state.settings.read().await.security.dns_pinning;
    let client = if dns_pinning {
        pinned_client(&endpoint, &candidate.provider, "CODETAS-Realtime/0.1")
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "provider_dns_rejected",
                    &message,
                ),
                kind: AttemptFailureKind::Retryable,
            })?
    } else {
        state.client.clone()
    };
    let request = client
        .post(endpoint)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT, "application/sdp, application/json")
        .query(&candidate.provider.query_params)
        .timeout(Duration::from_millis(
            candidate.provider.limits.request_timeout_ms.min(120_000),
        ));
    let request = apply_provider_auth(request, &candidate.provider, candidate.credential.as_ref())
        .await
        .map_err(|message| AttemptFailure {
            response: error_response(
                StatusCode::UNAUTHORIZED,
                "missing_provider_credential",
                &message,
            ),
            kind: AttemptFailureKind::Credential,
        })?;
    let mut request = if credential_source == CredentialSource::Forward {
        apply_forward_headers(request, &state.settings, caller_headers)
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?
    } else {
        request
    };
    for name in [
        "openai-alpha",
        "x-session-id",
        "session-id",
        "thread-id",
        "originator",
        "x-oai-attestation",
    ] {
        if let Some(value) = caller_headers.get(name) {
            request = request.header(name, value);
        }
    }
    match request.body(body.to_vec()).send().await {
        Ok(response)
            if response.content_length().is_some_and(|length| {
                length
                    > candidate
                        .provider
                        .limits
                        .max_response_bytes
                        .min(16 * 1024 * 1024)
            }) =>
        {
            Err(AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "provider_response_too_large",
                    "realtime response exceeds the configured limit",
                ),
                kind: AttemptFailureKind::Retryable,
            })
        }
        Ok(response) => Ok(response),
        Err(_) => Err(AttemptFailure {
            response: error_response(
                StatusCode::BAD_GATEWAY,
                "provider_unreachable",
                "realtime provider request failed",
            ),
            kind: AttemptFailureKind::Retryable,
        }),
    }
}

pub(crate) fn realtime_endpoint(
    candidate: &RouteCandidate,
    credential_source: CredentialSource,
    inbound_path: &str,
) -> String {
    let base = candidate.provider.base_url.trim().trim_end_matches('/');
    if base.contains("/backend-api") {
        return format!("{base}/realtime/calls?intent=quicksilver&architecture=avas");
    }
    if credential_source == CredentialSource::Forward && inbound_path == "/v1/live" {
        return format!("{base}/live");
    }
    let endpoint = if base.ends_with("/realtime/calls") {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{base}/realtime/calls")
    } else {
        format!("{base}/v1/realtime/calls")
    };
    format!("{endpoint}?intent=quicksilver&architecture=avas")
}

pub(crate) async fn bounded_response_bytes(
    mut response: reqwest::Response,
    limit: u64,
) -> Result<Vec<u8>, String> {
    let _routing_attempt_lease = take_routing_attempt_lease(&mut response);
    let mut output = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "provider response stream failed".to_string())?;
        if output.len() as u64 + chunk.len() as u64 > limit {
            return Err("provider response exceeded the configured limit".into());
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

pub(crate) fn request_failure(code: &str, message: &str) -> AttemptFailure {
    AttemptFailure {
        response: error_response(StatusCode::BAD_REQUEST, code, message),
        kind: AttemptFailureKind::Request,
    }
}
