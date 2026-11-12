use super::*;

pub(crate) async fn send_kiro_candidate(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
    wire_model: &str,
) -> Result<reqwest::Response, AttemptFailure> {
    validate_candidate_image_references(body, candidate)?;
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
        .then_some(candidate.provider.kiro_profile_arn.as_deref())
        .flatten();
    // Kiro rejects a message with more than 20 images, and Codex never compacts
    // on image counts. `send_candidate` prunes before this hop, but compact and
    // retry paths call this function directly, so prune here as well.
    let pruned_body;
    let body = if count_translated_input_images(body) > crate::kiro::MAX_KIRO_IMAGES {
        let mut pruned = body.clone();
        let dropped = crate::translate::prune_oldest_images_to_count(
            &mut pruned,
            crate::kiro::MAX_KIRO_IMAGES,
        );
        if dropped > 0 {
            crate::debug::log(&format!(
                "kiro image count pruned: provider={} model={} dropped={} limit={}",
                candidate.provider.id,
                candidate.upstream_model,
                dropped,
                crate::kiro::MAX_KIRO_IMAGES,
            ));
        }
        pruned_body = pruned;
        &pruned_body
    } else {
        body
    };
    let (mut payload, context) =
        crate::kiro::responses_to_kiro_checked(body, wire_model, profile_arn).map_err(|error| {
            match error {
                crate::kiro::KiroRequestError::ImageBudget => {
                    image_history_compaction_required(&error.to_string())
                }
                crate::kiro::KiroRequestError::Unsupported(message) => {
                    request_failure("unsupported_request", &message)
                }
            }
        })?;
    {
        let protocol = candidate
            .provider
            .protocol_for_model(&candidate.upstream_model);
        let image_report = normalize_translated_image_history(
            &mut payload,
            protocol,
            candidate.provider.transport,
            candidate.provider.limits.max_request_bytes,
        )
        .await
        .map_err(image_history_failure)?;
        scrub_kiro_omitted_images(&mut payload);
        if image_report.omitted_images > 0 {
            crate::debug::log(&format!(
                "kiro wire image history normalized: provider={} model={} omitted={} total={} kept_base64_chars={}",
                candidate.provider.id,
                candidate.upstream_model,
                image_report.omitted_images,
                image_report.inline_images,
                image_report.kept_base64_chars,
            ));
        }
    }
    let serialized = crate::translate::serialize_preserving_image_history_async(
        &mut payload,
        candidate.provider.limits.max_request_bytes,
    )
    .await
    .map_err(image_history_failure)?;

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
    match diagnostics::send_with_wire_diagnostic(request, serialized, &payload, caller_headers)
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
                true,
            )
            .await)
        }
        Ok(mut response) => {
            response.extensions_mut().insert(SentImageState {
                sent_images: crate::translate::count_rendered_sent_images(&payload),
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
