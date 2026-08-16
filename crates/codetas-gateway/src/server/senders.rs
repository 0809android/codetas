use super::*;

#[derive(Clone, Copy)]
struct SentImageState {
    omitted_before_send: usize,
    sent_images: usize,
}

pub(crate) async fn send_candidate(
    state: &GatewayState,
    body: &mut Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
) -> Result<reqwest::Response, AttemptFailure> {
    let remote_compaction = request_is_remote_compaction(body);
    let protocol = candidate
        .provider
        .protocol_for_model(&candidate.upstream_model);
    let strip_unsupported_images = state.settings.read().await.agents.image_input_mode
        != crate::config::AuxiliaryInputMode::Native;
    apply_provider_request_compatibility(
        body,
        candidate,
        protocol,
        strip_unsupported_images,
    );
    if !remote_compaction {
        let image_report = normalize_translated_image_history(
            body,
            protocol,
            candidate.provider.transport,
            candidate.provider.limits.max_request_bytes,
        )
        .await
        .map_err(|message| request_failure("image_normalization_failed", &message))?;
        if image_report.omitted_images > 0 {
            crate::debug::log(&format!(
                "image history normalized: provider={} model={} omitted={} total={} kept_base64_chars={}",
                candidate.provider.id,
                candidate.upstream_model,
                image_report.omitted_images,
                image_report.inline_images,
                image_report.kept_base64_chars,
            ));
        }
    }
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let first = {
        let prepared_body: &Value = body;
        retry_provider_request(state, candidate, streaming, || {
            send_candidate_once(state, prepared_body, candidate, caller_headers)
        })
        .await?
    };
    if first.status() != StatusCode::PAYLOAD_TOO_LARGE {
        return Ok(first);
    }
    if remote_compaction {
        return Ok(first);
    }
    let sent_image_state = first
        .extensions()
        .get::<SentImageState>()
        .copied()
        .unwrap_or(SentImageState {
            omitted_before_send: 0,
            sent_images: 0,
        });
    if sent_image_state.sent_images == 0 {
        return Ok(first);
    }
    if !tighten_image_history_after_413(body, sent_image_state) {
        return Ok(first);
    }
    let _ = read_bounded(first, 64 * 1024).await;
    crate::debug::log(&format!(
        "provider returned 413; retrying once with tighter image history: provider={} model={}",
        candidate.provider.id, candidate.upstream_model,
    ));
    let prepared_body: &Value = body;
    retry_provider_request(state, candidate, streaming, || {
        send_candidate_once(state, prepared_body, candidate, caller_headers)
    })
    .await
}

fn tighten_image_history_after_413(body: &mut Value, sent: SentImageState) -> bool {
    if sent.sent_images == 0 {
        return false;
    }
    let mut omitted_for_retry = 0_usize;
    for _ in 0..=sent.omitted_before_send {
        if omit_oldest_translated_input_image(body) {
            omitted_for_retry += 1;
        } else {
            break;
        }
    }
    omitted_for_retry > sent.omitted_before_send
}

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
    let transport_retries = if streaming {
        candidate.provider.limits.stream_retries
    } else {
        candidate.provider.limits.request_retries
    };
    let mut transport_attempts = 0_u8;
    let mut rate_limit_attempts = 0_u8;
    let mut empty_attempts = 0_u8;
    loop {
        await_provider_pacing(state, candidate).await;
        match operation().await {
            Ok(response) if response.status() == StatusCode::TOO_MANY_REQUESTS => {
                if should_retry_rate_limit(&candidate.provider.limits, rate_limit_attempts)
                {
                    let delay = validated_retry_after(response.headers())
                        .map(|(_, delay)| delay.unwrap_or(Duration::ZERO))
                        .unwrap_or_else(|| retry_backoff(rate_limit_attempts));
                    let _ = read_bounded(response, 64 * 1024).await;
                    tokio::time::sleep(delay.min(Duration::from_secs(5))).await;
                    rate_limit_attempts = rate_limit_attempts.saturating_add(1);
                    continue;
                }
                return Ok(response);
            }
            Ok(response)
                if response.status() == StatusCode::REQUEST_TIMEOUT
                    || response.status().is_server_error() =>
            {
                if transport_attempts < transport_retries {
                    let delay = validated_retry_after(response.headers())
                        .map(|(_, delay)| delay.unwrap_or(Duration::ZERO))
                        .unwrap_or_else(|| retry_backoff(transport_attempts));
                    let _ = read_bounded(response, 64 * 1024).await;
                    tokio::time::sleep(delay.min(Duration::from_secs(5))).await;
                    transport_attempts = transport_attempts.saturating_add(1);
                    continue;
                }
                return Ok(response);
            }
            Ok(response)
                if empty_completion_retry_enabled(candidate, streaming, empty_attempts) =>
            {
                let (response, empty) = inspect_empty_completion(
                    response,
                    candidate.provider.limits.max_response_bytes,
                )
                .await?;
                if empty {
                    empty_attempts = empty_attempts.saturating_add(1);
                    tokio::time::sleep(retry_backoff(empty_attempts.saturating_sub(1))).await;
                    continue;
                }
                return Ok(response);
            }
            Ok(response) => return Ok(response),
            Err(failure)
                if failure.kind == AttemptFailureKind::Retryable
                    && transport_attempts < transport_retries =>
            {
                tokio::time::sleep(retry_backoff(transport_attempts)).await;
                transport_attempts = transport_attempts.saturating_add(1);
            }
            Err(failure) => return Err(failure),
        }
    }
}

async fn await_provider_pacing(state: &GatewayState, candidate: &RouteCandidate) {
    let interval = Duration::from_millis(candidate.provider.limits.request_pacing_ms);
    if interval.is_zero() {
        return;
    }
    let now = Instant::now();
    let scheduled = {
        let mut pacing = state.pacing.lock().await;
        reserve_provider_start(&mut pacing, &candidate.provider.id, interval, now)
    };
    if scheduled > now {
        tokio::time::sleep_until(tokio::time::Instant::from_std(scheduled)).await;
    }
}

pub(crate) fn should_retry_rate_limit(
    limits: &crate::config::ProviderLimits,
    attempts: u8,
) -> bool {
    limits.retry_on_429 && attempts < limits.max_429_retries
}

pub(crate) fn empty_completion_retry_enabled(
    candidate: &RouteCandidate,
    streaming: bool,
    attempts: u8,
) -> bool {
    !streaming
        && (model_matches_any(
            &candidate.upstream_model,
            &candidate.provider.empty_completion_retry_models,
        ) || model_matches_any(
            &candidate.upstream_model,
            &candidate.provider.terminal_continuation_guard_models,
        ))
        && attempts < candidate.provider.limits.empty_completion_retries
}

pub(crate) fn reserve_provider_start(
    pacing: &mut ProviderPacingState,
    key: &str,
    interval: Duration,
    now: Instant,
) -> Instant {
    if interval.is_zero() {
        return now;
    }
    pacing.reservations = pacing.reservations.saturating_add(1);
    if pacing.reservations % 128 == 0 || pacing.next_slots.len() >= MAX_PROVIDER_PACING_KEYS {
        pacing.next_slots.retain(|_, next| *next > now);
    }
    if !pacing.next_slots.contains_key(key)
        && pacing.next_slots.len() >= MAX_PROVIDER_PACING_KEYS
    {
        if let Some(oldest) = pacing.next_slots.iter()
            .min_by_key(|(_, next)| **next).map(|(key, _)| key.clone())
        {
            pacing.next_slots.remove(&oldest);
        }
    }
    let scheduled = pacing.next_slots.get(key).copied().unwrap_or(now).max(now);
    pacing.next_slots.insert(key.to_string(), scheduled + interval);
    scheduled
}

async fn inspect_empty_completion(
    response: reqwest::Response,
    limit: u64,
) -> Result<(reqwest::Response, bool), AttemptFailure> {
    if !response.status().is_success() {
        return Ok((response, false));
    }
    let status = response.status();
    let version = response.version();
    let headers = response.headers().clone();
    let bytes = read_bounded(response, limit)
        .await
        .map_err(|message| request_failure("invalid_provider_response", &message))?;
    let empty = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .is_some_and(|value| completion_is_empty(&value));
    let mut builder = axum::http::Response::builder()
        .status(status)
        .version(version);
    if let Some(target) = builder.headers_mut() {
        *target = headers;
    }
    let response = builder
        .body(reqwest::Body::from(bytes))
        .map(reqwest::Response::from)
        .map_err(|_| request_failure("gateway_error", "failed to rebuild provider response"))?;
    Ok((response, empty))
}

pub(crate) fn completion_is_empty(value: &Value) -> bool {
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return false;
    }
    if let Some(output) = value.get("output").and_then(Value::as_array) {
        return output.is_empty();
    }
    if let Some(choices) = value.get("choices").and_then(Value::as_array) {
        return choices.is_empty()
            || choices.iter().all(|choice| {
                let message = choice.get("message").or_else(|| choice.get("delta"));
                message.is_none_or(|message| {
                    message
                        .get("content")
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
                        && message
                            .get("tool_calls")
                            .and_then(Value::as_array)
                            .is_none_or(Vec::is_empty)
                })
            });
    }
    if let Some(content) = value.get("content").and_then(Value::as_array) {
        return content.is_empty();
    }
    if let Some(candidates) = value.get("candidates").and_then(Value::as_array) {
        return candidates.is_empty();
    }
    false
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
    let remote_compaction = request_is_remote_compaction(body);
    let source_image_count = count_translated_input_images(body);
    let protocol = candidate
        .provider
        .protocol_for_model(&candidate.upstream_model);
    let wire_model = wire_model_for_request(candidate, body);
    if candidate.provider.transport == ProviderTransport::Kiro {
        return send_kiro_candidate(
            state,
            body,
            candidate,
            caller_headers,
            &wire_model,
        )
        .await;
    }
    if candidate.provider.transport == ProviderTransport::GithubCopilot {
        return send_github_copilot_candidate(
            state,
            body,
            candidate,
            caller_headers,
            &wire_model,
        )
        .await;
    }
    let mut upstream_body = match protocol {
        ProviderProtocol::Responses => body.clone(),
        ProviderProtocol::ChatCompletions => match responses_to_chat_with_options(
            &chat_compatible_request(body, candidate.capabilities.provider_metadata),
            &wire_model,
            model_requires_reasoning_placeholder(
                &candidate.provider,
                &candidate.upstream_model,
            ),
        ) {
            Ok(value) => value,
            Err(message) => return Err(request_failure("unsupported_request", &message)),
        },
        ProviderProtocol::AnthropicMessages => {
            match responses_to_anthropic_with_oauth(
                body,
                &wire_model,
                uses_anthropic_subscription_oauth(&candidate.provider),
            ) {
                Ok(value) => value,
                Err(message) => return Err(request_failure("unsupported_request", &message)),
            }
        }
        ProviderProtocol::GeminiGenerateContent => {
            match responses_to_gemini(body, &wire_model) {
                Ok(value) => value,
                Err(message) => return Err(request_failure("unsupported_request", &message)),
            }
        }
    };
    if !remote_compaction {
        if let Err(message) =
            apply_provider_wire_compatibility(&mut upstream_body, body, candidate, protocol)
        {
            return Err(request_failure("unsupported_request", &message));
        }
    }
    let antigravity_project = if protocol == ProviderProtocol::GeminiGenerateContent
        && candidate.provider.google_mode == GoogleMode::CloudCodeAssist
        && candidate.provider.project.is_none()
        && candidate
            .credential
            .as_ref()
            .unwrap_or(&candidate.provider.credential)
            .source
            == CredentialSource::OAuth
        && candidate
            .credential
            .as_ref()
            .unwrap_or(&candidate.provider.credential)
            .reference
            .as_deref()
            == Some("google-antigravity")
    {
        let token = resolve_oauth_access_token("google-antigravity")
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_provider_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?;
        Some(
            resolve_antigravity_cloud_project(&token)
                .await
                .map_err(|message| AttemptFailure {
                    response: error_response(
                        StatusCode::BAD_GATEWAY,
                        "provider_configuration_error",
                        &message,
                    ),
                    kind: AttemptFailureKind::Retryable,
                })?,
        )
    } else {
        None
    };
    if protocol == ProviderProtocol::GeminiGenerateContent
        && candidate.provider.google_mode == GoogleMode::CloudCodeAssist
    {
        upstream_body = cloud_code_assist_envelope(
            upstream_body,
            body,
            candidate,
            &wire_model,
            antigravity_project.as_deref(),
        )?;
    }
    if protocol != ProviderProtocol::GeminiGenerateContent {
        upstream_body["model"] = Value::String(wire_model.clone());
    }
    let translation_image_omissions =
        source_image_count.saturating_sub(count_translated_input_images(&upstream_body));

    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut serialized = serde_json::to_vec(&upstream_body).map_err(|error| {
        request_failure(
            "invalid_request",
            &format!("request cannot be encoded: {error}"),
        )
    })?;
    let mut wire_image_omissions = 0_usize;
    while !remote_compaction
        && serialized.len() as u64 > candidate.provider.limits.max_request_bytes
        && omit_oldest_translated_input_image(&mut upstream_body)
    {
        wire_image_omissions += 1;
        serialized = serde_json::to_vec(&upstream_body).map_err(|error| {
            request_failure(
                "invalid_request",
                &format!("request cannot be encoded after image normalization: {error}"),
            )
        })?;
    }
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
        request.header(header::USER_AGENT, "antigravity")
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
        Ok(mut response) => {
            response.extensions_mut().insert(SentImageState {
                omitted_before_send: translation_image_omissions
                    .saturating_add(wire_image_omissions),
                sent_images: count_translated_input_images(&upstream_body),
            });
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

fn chat_compatible_request(body: &Value, preserve_provider_metadata: bool) -> Value {
    let mut body = body.clone();
    if preserve_provider_metadata {
        return body;
    }
    if let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) {
        for item in items.iter_mut().filter_map(Value::as_object_mut) {
            item.remove("provider_metadata");
        }
    }
    body
}

pub(crate) async fn send_kiro_candidate(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
    wire_model: &str,
) -> Result<reqwest::Response, AttemptFailure> {
    let source_image_count = count_translated_input_images(body);
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
    let (mut payload, context) = responses_to_kiro(body, wire_model, profile_arn)
        .map_err(|message| request_failure("unsupported_request", &message))?;
    let translation_image_omissions =
        source_image_count.saturating_sub(count_translated_input_images(&payload));
    let mut serialized = serde_json::to_vec(&payload).map_err(|error| {
        request_failure(
            "invalid_request",
            &format!("Kiro request cannot be encoded: {error}"),
        )
    })?;
    let mut wire_image_omissions = 0_usize;
    while serialized.len() as u64 > candidate.provider.limits.max_request_bytes
        && omit_oldest_kiro_wire_image(&mut payload)
    {
        wire_image_omissions += 1;
        serialized = serde_json::to_vec(&payload).map_err(|error| {
            request_failure(
                "invalid_request",
                &format!("Kiro request cannot be encoded after image normalization: {error}"),
            )
        })?;
    }
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
            response.extensions_mut().insert(SentImageState {
                omitted_before_send: translation_image_omissions
                    .saturating_add(wire_image_omissions),
                sent_images: count_translated_input_images(&payload),
            });
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
    let source_image_count = count_translated_input_images(body);
    let mut upstream_body = responses_to_chat_with_options(
        body,
        wire_model,
        model_requires_reasoning_placeholder(&candidate.provider, &candidate.upstream_model),
    )
        .map_err(|message| request_failure("unsupported_request", &message))?;
    apply_provider_wire_compatibility(
        &mut upstream_body,
        body,
        candidate,
        ProviderProtocol::ChatCompletions,
    )
    .map_err(|message| request_failure("unsupported_request", &message))?;
    upstream_body["model"] = Value::String(wire_model.to_string());
    let translation_image_omissions =
        source_image_count.saturating_sub(count_translated_input_images(&upstream_body));
    let mut serialized = serde_json::to_vec(&upstream_body).map_err(|error| {
        request_failure(
            "invalid_request",
            &format!("GitHub Copilot request cannot be encoded: {error}"),
        )
    })?;
    let mut wire_image_omissions = 0_usize;
    while serialized.len() as u64 > candidate.provider.limits.max_request_bytes
        && omit_oldest_translated_input_image(&mut upstream_body)
    {
        wire_image_omissions += 1;
        serialized = serde_json::to_vec(&upstream_body).map_err(|error| {
            request_failure(
                "invalid_request",
                &format!("GitHub Copilot request cannot be encoded after image normalization: {error}"),
            )
        })?;
    }
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
        Ok(mut response) => {
            response.extensions_mut().insert(SentImageState {
                omitted_before_send: translation_image_omissions
                    .saturating_add(wire_image_omissions),
                sent_images: count_translated_input_images(&upstream_body),
            });
            Ok(response)
        }
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
        (
            "gemini-3.7-flash" | "gemini-3.6-flash" | "gemini-3.5-flash",
            Some("low"),
        ) => format!("{model}-low"),
        (
            "gemini-3.7-flash" | "gemini-3.6-flash" | "gemini-3.5-flash",
            Some("high" | "xhigh" | "max" | "ultra"),
        ) => format!("{model}-high"),
        ("gemini-3.7-flash" | "gemini-3.6-flash" | "gemini-3.5-flash", _) => {
            format!("{model}-medium")
        }
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
    project_override: Option<&str>,
) -> Result<Value, AttemptFailure> {
    let project = candidate
        .provider
        .project
        .as_deref()
        .or(project_override)
        .ok_or_else(|| {
            request_failure(
                "provider_configuration_error",
                "Cloud Code Assist requires provider.project or an Antigravity OAuth session",
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

#[cfg(test)]
mod image_retry_tests {
    use super::*;

    #[test]
    fn retry_tightening_removes_prior_wire_omissions_plus_one_image() {
        let image = |suffix: &str| {
            json!({
                "type": "input_image",
                "image_url": format!("data:image/png;base64,AA{suffix}=")
            })
        };
        let mut body = json!({"input": [image("A"), image("B"), image("C")]});
        assert!(tighten_image_history_after_413(
            &mut body,
            SentImageState {
                omitted_before_send: 1,
                sent_images: 2,
            },
        ));
        assert_eq!(count_translated_input_images(&body), 1);
    }

    #[test]
    fn empty_completion_detection_has_positive_and_negative_cases() {
        assert!(completion_is_empty(&json!({"choices": []})));
        assert!(completion_is_empty(&json!({"output": []})));
        assert!(!completion_is_empty(&json!({"choices": [{"message": {"content": "ok"}}]})));
        assert!(!completion_is_empty(&json!({"error": {"message": "failed"}})));
    }

    #[test]
    fn chat_metadata_is_forwarded_only_when_provider_opts_in() {
        let body = json!({"input": [{"type": "reasoning", "provider_metadata": {"vendor": {"signature": "opaque"}}}]});
        assert!(chat_compatible_request(&body, false).pointer("/input/0/provider_metadata").is_none());
        assert!(chat_compatible_request(&body, true).pointer("/input/0/provider_metadata").is_some());
    }
}

#[cfg(test)]
mod provider_pacing_tests {
    use super::*;

    #[test]
    fn concurrent_reservations_for_one_provider_are_spaced() {
        let mut pacing = ProviderPacingState::default();
        let now = Instant::now();
        let interval = Duration::from_millis(250);
        let first = reserve_provider_start(&mut pacing, "provider", interval, now);
        let second = reserve_provider_start(&mut pacing, "provider", interval, now);
        assert_eq!(first, now);
        assert!(second.duration_since(first) >= interval);
    }

    #[test]
    fn disabled_pacing_and_different_providers_do_not_share_a_queue() {
        let mut pacing = ProviderPacingState::default();
        let now = Instant::now();
        assert_eq!(reserve_provider_start(&mut pacing, "off", Duration::ZERO, now), now);
        assert!(pacing.next_slots.is_empty());
        assert_eq!(reserve_provider_start(&mut pacing, "one", Duration::from_secs(1), now), now);
        assert_eq!(reserve_provider_start(&mut pacing, "two", Duration::from_secs(1), now), now);
    }

    #[test]
    fn pacing_state_is_bounded_and_periodically_drops_expired_keys() {
        let mut pacing = ProviderPacingState::default();
        let now = Instant::now();
        for index in 0..(MAX_PROVIDER_PACING_KEYS + 32) {
            reserve_provider_start(
                &mut pacing,
                &format!("provider-{index}"),
                Duration::from_secs(60),
                now,
            );
        }
        assert!(pacing.next_slots.len() <= MAX_PROVIDER_PACING_KEYS);

        pacing.reservations = 127;
        reserve_provider_start(
            &mut pacing,
            "fresh",
            Duration::from_millis(1),
            now + Duration::from_secs(61),
        );
        assert_eq!(pacing.next_slots.len(), 1);
        assert!(pacing.next_slots.contains_key("fresh"));
    }
}
