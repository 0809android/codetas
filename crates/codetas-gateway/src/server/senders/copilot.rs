use super::*;

pub(crate) async fn send_github_copilot_candidate(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
    wire_model: &str,
) -> Result<reqwest::Response, AttemptFailure> {
    validate_candidate_image_references(body, candidate)?;
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
    let image_report = normalize_translated_image_history(
        &mut upstream_body,
        ProviderProtocol::ChatCompletions,
        candidate.provider.transport,
        candidate.provider.limits.max_request_bytes,
    )
    .await
    .map_err(image_history_failure)?;
    if image_report.omitted_images > 0 {
        crate::debug::log(&format!(
            "copilot wire image history normalized: provider={} model={} omitted={} total={}",
            candidate.provider.id,
            candidate.upstream_model,
            image_report.omitted_images,
            image_report.inline_images,
        ));
    }
    let serialized = crate::translate::serialize_preserving_image_history_async(
        &mut upstream_body,
        candidate.provider.limits.max_request_bytes,
    )
    .await
    .map_err(image_history_failure)?;

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
    let binding_request = state
        .client
        .get(token_endpoint)
        .headers(credential_headers.clone())
        .build()
        .map_err(|_| {
            request_failure(
                "invalid_request",
                "cannot build Copilot authentication binding",
            )
        })?;
    let auth_binding = diagnostics::resolved_auth_binding(&binding_request);
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
    match diagnostics::send_with_wire_diagnostic(
        request,
        serialized,
        &upstream_body,
        caller_headers,
    )
    .await
    {
        Ok(response)
            if response
                .content_length()
                .is_some_and(|length| length > candidate.provider.limits.max_response_bytes) =>
        {
            Err(reject_oversized_provider_response(
                response,
                candidate.provider.limits.max_response_bytes,
                false,
            )
            .await)
        }
        Ok(mut response) => {
            if let Some(diagnostic) = response
                .extensions_mut()
                .get_mut::<crate::observability::RequestDiagnostic>()
            {
                diagnostic.auth_binding = Some(auth_binding);
            }
            response.extensions_mut().insert(SentImageState {
                sent_images: crate::translate::count_rendered_sent_images(&upstream_body),
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
        ("claude-opus-5-5" | "claude-sonnet-5-5", Some("low")) => format!("{model}-low"),
        ("claude-opus-5-5" | "claude-sonnet-5-5", Some("high" | "xhigh" | "max" | "ultra")) => {
            format!("{model}-high")
        }
        ("claude-opus-5-5" | "claude-sonnet-5-5", _) => format!("{model}-medium"),
        ("gemini-3.8-flash" | "gemini-3.7-flash", _) => format!("{model}-tiered"),
        // Gemini 4 Argon uses the same effort-suffixed ids that `agy` sends. They are
        // only served to a client that identifies itself like agy (see
        // `antigravity_user_agent`); under a bare `antigravity` user agent every one of
        // them answered HTTP 404 "Requested entity was not found".
        ("gemini-4-argon" | "gemini-3.6-flash" | "gemini-3.5-flash", Some("low")) => {
            format!("{model}-low")
        }
        (
            "gemini-4-argon" | "gemini-3.6-flash" | "gemini-3.5-flash",
            Some("high" | "xhigh" | "max" | "ultra"),
        ) => format!("{model}-high"),
        ("gemini-4-argon" | "gemini-3.6-flash" | "gemini-3.5-flash", _) => {
            format!("{model}-medium")
        }
        ("gemini-3.1-pro", Some("low")) => "gemini-3.1-pro-low".into(),
        ("gemini-3.1-pro", _) => "gemini-pro-agent".into(),
        _ => candidate.provider.wire_model_id(model),
    }
}

pub(super) fn antigravity_project_oauth_reference<'a>(
    provider: &'a ProviderDefinition,
    credential_override: Option<&'a ProviderCredential>,
) -> Option<&'a str> {
    if provider.google_mode != GoogleMode::CloudCodeAssist || provider.project.is_some() {
        return None;
    }
    let credential = credential_override.unwrap_or(&provider.credential);
    if credential.source != CredentialSource::OAuth {
        return None;
    }
    let reference = credential.reference.as_deref()?;
    // Only the provider check uses the base id. Token lookup must retain the
    // selected account's full key, never fall back to the default CLI account.
    (crate::oauth::session_key_provider(reference) == "google-antigravity").then_some(reference)
}
