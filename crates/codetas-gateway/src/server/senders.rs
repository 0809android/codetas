use super::*;

pub(crate) async fn send_candidate(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
) -> Result<reqwest::Response, AttemptFailure> {
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    retry_provider_request(candidate, streaming, || {
        send_candidate_once(state, body, candidate, caller_headers)
    })
    .await
}

pub(crate) async fn retry_provider_request<F, Fut>(
    candidate: &RouteCandidate,
    streaming: bool,
    mut operation: F,
) -> Result<reqwest::Response, AttemptFailure>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, AttemptFailure>>,
{
    let retries = if streaming {
        candidate.provider.limits.stream_retries
    } else {
        candidate.provider.limits.request_retries
    };
    for attempt in 0..=retries {
        match operation().await {
            Ok(response) => {
                let retryable = response.status() == StatusCode::REQUEST_TIMEOUT
                    || response.status() == StatusCode::TOO_MANY_REQUESTS
                    || response.status().is_server_error();
                if retryable && attempt < retries {
                    let delay = validated_retry_after(response.headers())
                        .map(|(_, delay)| delay.unwrap_or(Duration::ZERO))
                        .unwrap_or_else(|| retry_backoff(attempt));
                    let _ = read_bounded(response, 64 * 1024).await;
                    tokio::time::sleep(delay.min(Duration::from_secs(5))).await;
                    continue;
                }
                return Ok(response);
            }
            Err(failure) if failure.kind == AttemptFailureKind::Retryable && attempt < retries => {
                tokio::time::sleep(retry_backoff(attempt)).await;
            }
            Err(failure) => return Err(failure),
        }
    }
    unreachable!("bounded provider retry loop always returns")
}

pub(crate) fn retry_backoff(attempt: u8) -> Duration {
    Duration::from_millis(250_u64.saturating_mul(1_u64 << attempt.min(4)))
}

pub(crate) async fn send_candidate_once(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
) -> Result<reqwest::Response, AttemptFailure> {
    let protocol = candidate
        .provider
        .protocol_for_model(&candidate.upstream_model);
    let mut compatible_body = body.clone();
    let strip_unsupported_images = state.settings.read().await.agents.image_input_mode
        != crate::config::AuxiliaryInputMode::Native;
    apply_provider_request_compatibility(
        &mut compatible_body,
        candidate,
        protocol,
        strip_unsupported_images,
    );
    let wire_model = wire_model_for_request(candidate, &compatible_body);
    if candidate.provider.transport == ProviderTransport::Kiro {
        return send_kiro_candidate(
            state,
            &compatible_body,
            candidate,
            caller_headers,
            &wire_model,
        )
        .await;
    }
    if candidate.provider.transport == ProviderTransport::GithubCopilot {
        return send_github_copilot_candidate(
            state,
            &compatible_body,
            candidate,
            caller_headers,
            &wire_model,
        )
        .await;
    }
    let mut upstream_body = match protocol {
        ProviderProtocol::Responses => compatible_body.clone(),
        ProviderProtocol::ChatCompletions => match responses_to_chat(&compatible_body, &wire_model)
        {
            Ok(value) => value,
            Err(message) => return Err(request_failure("unsupported_request", &message)),
        },
        ProviderProtocol::AnthropicMessages => {
            match responses_to_anthropic_with_oauth(
                &compatible_body,
                &wire_model,
                uses_anthropic_subscription_oauth(&candidate.provider),
            ) {
                Ok(value) => value,
                Err(message) => return Err(request_failure("unsupported_request", &message)),
            }
        }
        ProviderProtocol::GeminiGenerateContent => {
            match responses_to_gemini(&compatible_body, &wire_model) {
                Ok(value) => value,
                Err(message) => return Err(request_failure("unsupported_request", &message)),
            }
        }
    };
    if let Err(message) =
        apply_provider_wire_compatibility(&mut upstream_body, &compatible_body, candidate, protocol)
    {
        return Err(request_failure("unsupported_request", &message));
    }
    if protocol == ProviderProtocol::GeminiGenerateContent
        && candidate.provider.google_mode == GoogleMode::CloudCodeAssist
    {
        upstream_body =
            cloud_code_assist_envelope(upstream_body, &compatible_body, candidate, &wire_model)?;
    }
    if protocol != ProviderProtocol::GeminiGenerateContent {
        upstream_body["model"] = Value::String(wire_model.clone());
    }

    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let serialized = serde_json::to_vec(&upstream_body).map_err(|error| {
        request_failure(
            "invalid_request",
            &format!("request cannot be encoded: {error}"),
        )
    })?;
    if serialized.len() as u64 > candidate.provider.limits.max_request_bytes {
        return Err(request_failure(
            "request_too_large",
            "translated request exceeds the configured provider limit",
        ));
    }

    let mut query_params = candidate.provider.query_params.clone();
    if let Some(version) = candidate.provider.azure_api_version.as_ref() {
        query_params.insert("api-version".into(), version.clone());
    }
    if protocol == ProviderProtocol::GeminiGenerateContent && streaming {
        query_params.insert("alt".into(), "sse".into());
    }

    let endpoint = candidate
        .provider
        .endpoint_for_model_streaming(&wire_model, streaming);
    let dns_pinning = state.settings.read().await.security.dns_pinning;
    let client = if dns_pinning {
        let client = pinned_client(&endpoint, &candidate.provider, "CODETAS-Gateway/0.1")
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "provider_dns_rejected",
                    &message,
                ),
                kind: AttemptFailureKind::Retryable,
            })?;
        client
    } else {
        state.client.clone()
    };
    let mut request = client
        .post(endpoint.clone())
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::ACCEPT,
            if streaming {
                "text/event-stream"
            } else {
                "application/json"
            },
        )
        .query(&query_params)
        .timeout(Duration::from_millis(
            candidate.provider.limits.request_timeout_ms,
        ));
    if protocol == ProviderProtocol::AnthropicMessages {
        request = request.header("anthropic-version", "2023-06-01");
        if uses_anthropic_subscription_oauth(&candidate.provider) {
            for (name, value) in anthropic_subscription_oauth_headers() {
                request = request.header(name, value);
            }
        }
    }
    let request = match apply_provider_auth(
        request,
        &candidate.provider,
        candidate.credential.as_ref(),
    )
    .await
    {
        Ok(request) => request,
        Err(message) => {
            return Err(AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_provider_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })
        }
    };
    let credential_source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let request = if credential_source == CredentialSource::Forward {
        let Some(caller_headers) = caller_headers else {
            return Err(AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    "the selected provider requires caller authentication headers",
                ),
                kind: AttemptFailureKind::Credential,
            });
        };
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
    let request = if candidate.provider.google_mode == GoogleMode::CloudCodeAssist {
        request.header(
            header::USER_AGENT,
            "CODETAS/0.1 (Cloud-Code-Assist-compatible)",
        )
    } else {
        request
    };

    let messages = upstream_body
        .get("messages")
        .and_then(serde_json::Value::as_array);
    let reasoning_count = messages.map(|m| {
        m.iter()
            .filter(|msg| msg.get("reasoning_content").is_some())
            .count()
    });
    let assistant_missing_reasoning = messages.map(|m| {
        m.iter()
            .filter(|msg| {
                msg.get("role").and_then(serde_json::Value::as_str) == Some("assistant")
                    && msg.get("reasoning_content").is_none()
            })
            .count()
    });
    let assistant_with_reasoning = messages.map(|m| {
        m.iter()
            .filter(|msg| {
                msg.get("role").and_then(serde_json::Value::as_str) == Some("assistant")
                    && msg.get("reasoning_content").is_some()
            })
            .count()
    });
    crate::debug::log(&format!(
        "send_candidate_once: POST {} (stream={} store={} input_items={} messages={} reasoning={} asst_no_reasoning={} asst_reasoning={} model={})",
        endpoint,
        upstream_body
            .get("stream")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        upstream_body
            .get("store")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
        upstream_body
            .get("input")
            .and_then(serde_json::Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0),
        messages.map(|m| m.len()).unwrap_or(0),
        reasoning_count.unwrap_or(0),
        assistant_missing_reasoning.unwrap_or(0),
        assistant_with_reasoning.unwrap_or(0),
        wire_model
    ));
    match request.body(serialized).send().await {
        Ok(response) => {
            crate::debug::log(&format!(
                "send_candidate_once: -> {}",
                response.status()
            ));
            if response
                .content_length()
                .is_some_and(|length| length > candidate.provider.limits.max_response_bytes)
            {
                Err(AttemptFailure {
                    response: error_response(
                        StatusCode::BAD_GATEWAY,
                        "provider_response_too_large",
                        "provider response exceeds the configured limit",
                    ),
                    kind: AttemptFailureKind::Retryable,
                })
            } else {
                Ok(response)
            }
        }
        Err(error) => Err(AttemptFailure {
            response: error_response(
                StatusCode::BAD_GATEWAY,
                "provider_unreachable",
                &format!("provider request failed: {error}"),
            ),
            kind: AttemptFailureKind::Retryable,
        }),
    }
}

pub(crate) async fn send_kiro_candidate(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
    wire_model: &str,
) -> Result<reqwest::Response, AttemptFailure> {
    let credential_source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let mut credential_headers =
        resolve_provider_headers(&candidate.provider, candidate.credential.as_ref())
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_provider_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?;
    if credential_source == CredentialSource::Forward {
        let Some(caller_headers) = caller_headers else {
            return Err(AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    "the selected Kiro provider requires caller authentication headers",
                ),
                kind: AttemptFailureKind::Credential,
            });
        };
        let forwarded = collect_forward_headers(&state.settings, caller_headers)
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?;
        let authorization = forwarded
            .get(header::AUTHORIZATION)
            .cloned()
            .ok_or_else(|| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    "the selected Kiro provider requires a caller Authorization header",
                ),
                kind: AttemptFailureKind::Credential,
            })?;
        credential_headers.insert(header::AUTHORIZATION, authorization);
    }
    let kiro_api_key = credential_headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|value| value.starts_with("ksk_"));
    let profile_arn = (!kiro_api_key)
        .then(|| candidate.provider.kiro_profile_arn.as_deref())
        .flatten();
    let (payload, context) = responses_to_kiro(body, wire_model, profile_arn)
        .map_err(|message| request_failure("unsupported_request", &message))?;
    let serialized = serde_json::to_vec(&payload).map_err(|error| {
        request_failure(
            "invalid_request",
            &format!("Kiro request cannot be encoded: {error}"),
        )
    })?;
    if serialized.len() as u64 > candidate.provider.limits.max_request_bytes {
        return Err(request_failure(
            "request_too_large",
            "translated Kiro request exceeds the configured provider limit",
        ));
    }

    let endpoint = format!(
        "{}/",
        candidate.provider.base_url.trim().trim_end_matches('/')
    );
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
        .header(header::CONTENT_TYPE, "application/x-amz-json-1.0")
        .header(header::ACCEPT, "application/vnd.amazon.eventstream")
        .header(
            "x-amz-target",
            "AmazonCodeWhispererStreamingService.GenerateAssistantResponse",
        )
        .header("x-amzn-codewhisperer-optout", "true")
        .header("amz-sdk-request", "attempt=1; max=1")
        .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
        .header(
            header::USER_AGENT,
            "CODETAS-Gateway/0.1 (Kiro-compatible transport)",
        )
        .query(&candidate.provider.query_params)
        .timeout(Duration::from_millis(
            candidate.provider.limits.request_timeout_ms,
        ));
    let mut request = request;
    for (name, value) in &credential_headers {
        request = request.header(name, value);
    }
    if kiro_api_key {
        request = request.header("tokentype", "API_KEY");
    }
    if let Some(profile_arn) = profile_arn {
        request = request
            .header("x-amzn-kiro-profile-arn", profile_arn)
            .header("x-amzn-kiro-agent-mode", "vibe");
    }
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
                    "Kiro response exceeds the configured limit",
                ),
                kind: AttemptFailureKind::Retryable,
            })
        }
        Ok(mut response) => {
            response.extensions_mut().insert(context);
            Ok(response)
        }
        Err(error) => Err(AttemptFailure {
            response: error_response(
                StatusCode::BAD_GATEWAY,
                "provider_unreachable",
                &format!("Kiro request failed: {error}"),
            ),
            kind: AttemptFailureKind::Retryable,
        }),
    }
}

pub(crate) async fn send_github_copilot_candidate(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
    wire_model: &str,
) -> Result<reqwest::Response, AttemptFailure> {
    let mut upstream_body = responses_to_chat(body, wire_model)
        .map_err(|message| request_failure("unsupported_request", &message))?;
    apply_provider_wire_compatibility(
        &mut upstream_body,
        body,
        candidate,
        ProviderProtocol::ChatCompletions,
    )
    .map_err(|message| request_failure("unsupported_request", &message))?;
    upstream_body["model"] = Value::String(wire_model.to_string());
    let serialized = serde_json::to_vec(&upstream_body).map_err(|error| {
        request_failure(
            "invalid_request",
            &format!("GitHub Copilot request cannot be encoded: {error}"),
        )
    })?;
    if serialized.len() as u64 > candidate.provider.limits.max_request_bytes {
        return Err(request_failure(
            "request_too_large",
            "translated GitHub Copilot request exceeds the configured provider limit",
        ));
    }

    let credential_source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let mut credential_headers =
        resolve_provider_headers(&candidate.provider, candidate.credential.as_ref())
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_provider_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?;
    if credential_source == CredentialSource::Forward {
        let Some(caller_headers) = caller_headers else {
            return Err(AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    "the selected GitHub Copilot provider requires caller authentication headers",
                ),
                kind: AttemptFailureKind::Credential,
            });
        };
        let forwarded = collect_forward_headers(&state.settings, caller_headers)
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?;
        let authorization = forwarded
            .get(header::AUTHORIZATION)
            .cloned()
            .ok_or_else(|| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    "the selected GitHub Copilot provider requires a caller Authorization header",
                ),
                kind: AttemptFailureKind::Credential,
            })?;
        credential_headers.insert(header::AUTHORIZATION, authorization);
    }
    let github_authorization = credential_headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AttemptFailure {
            response: error_response(
                StatusCode::UNAUTHORIZED,
                "missing_provider_credential",
                "GitHub Copilot requires a GitHub token via a bearer credential",
            ),
            kind: AttemptFailureKind::Credential,
        })?;
    let token_endpoint = "https://api.github.com/copilot_internal/v2/token";
    let dns_pinning = state.settings.read().await.security.dns_pinning;
    let exchange_client = if dns_pinning {
        pinned_client(token_endpoint, &candidate.provider, "CODETAS/0.1")
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
    let session = exchange_copilot_token(
        &exchange_client,
        github_authorization,
        Duration::from_millis(candidate.provider.limits.request_timeout_ms),
    )
    .await
    .map_err(|message| AttemptFailure {
        response: error_response(
            StatusCode::UNAUTHORIZED,
            "copilot_token_exchange_failed",
            &message,
        ),
        kind: AttemptFailureKind::Credential,
    })?;
    let endpoint = format!(
        "{}/chat/completions",
        session.api_base.trim_end_matches('/')
    );
    let client = if dns_pinning {
        pinned_client(&endpoint, &candidate.provider, "CODETAS/0.1")
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
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut request = client
        .post(endpoint)
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::ACCEPT,
            if streaming {
                "text/event-stream"
            } else {
                "application/json"
            },
        )
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", session.token.as_str()),
        )
        .header("editor-version", "CODETAS/0.1")
        .header("editor-plugin-version", "CODETAS/0.1")
        .header("copilot-integration-id", "vscode-chat")
        .header("openai-intent", "conversation-panel")
        .header(header::USER_AGENT, "CODETAS/0.1")
        .query(&candidate.provider.query_params)
        .timeout(Duration::from_millis(
            candidate.provider.limits.request_timeout_ms,
        ));
    for (name, value) in &credential_headers {
        if name != header::AUTHORIZATION {
            request = request.header(name, value);
        }
    }
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
                    "GitHub Copilot response exceeds the configured limit",
                ),
                kind: AttemptFailureKind::Retryable,
            })
        }
        Ok(response) => Ok(response),
        Err(error) => Err(AttemptFailure {
            response: error_response(
                StatusCode::BAD_GATEWAY,
                "provider_unreachable",
                &format!("GitHub Copilot request failed: {error}"),
            ),
            kind: AttemptFailureKind::Retryable,
        }),
    }
}

pub(crate) fn google_thinking_level(effort: &str) -> Option<&'static str> {
    match effort {
        "minimal" => Some("minimal"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" | "xhigh" | "max" | "ultra" => Some("high"),
        _ => None,
    }
}

pub(crate) fn wire_model_for_request(candidate: &RouteCandidate, request: &Value) -> String {
    let model = candidate.upstream_model.as_str();
    if candidate.provider.google_mode != GoogleMode::CloudCodeAssist {
        return candidate.provider.wire_model_id(model);
    }
    let effort = request.pointer("/reasoning/effort").and_then(Value::as_str);
    match (model, effort) {
        ("gemini-3.6-flash", Some("low")) => "gemini-3.6-flash-low".into(),
        ("gemini-3.6-flash", Some("high" | "xhigh" | "max" | "ultra")) => {
            "gemini-3.6-flash-high".into()
        }
        ("gemini-3.6-flash", _) => "gemini-3.6-flash-medium".into(),
        ("gemini-3.1-pro", Some("low")) => "gemini-3.1-pro-low".into(),
        ("gemini-3.1-pro", _) => "gemini-pro-agent".into(),
        _ => candidate.provider.wire_model_id(model),
    }
}

pub(crate) fn cloud_code_assist_envelope(
    mut request: Value,
    source_request: &Value,
    candidate: &RouteCandidate,
    wire_model: &str,
) -> Result<Value, AttemptFailure> {
    let project = candidate.provider.project.as_deref().ok_or_else(|| {
        request_failure(
            "provider_configuration_error",
            "Cloud Code Assist requires provider.project from the external OAuth broker",
        )
    })?;
    let session_source = source_request
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .unwrap_or_else(|| candidate.exposed_model.as_str());
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in session_source.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    if let Some(object) = request.as_object_mut() {
        object.insert("sessionId".into(), Value::String(format!("-{hash}")));
    }
    Ok(json!({
        "model": wire_model,
        "userAgent": "antigravity",
        "requestType": "agent",
        "project": project,
        "requestId": format!("agent-{}", Uuid::new_v4()),
        "request": request,
    }))
}
