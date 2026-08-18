use super::*;

pub(crate) async fn image_generations(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    special_json_relay(state, headers, body, SpecialRelayKind::ImageGeneration).await
}

pub(crate) async fn image_sidecar(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    if let Err(response) = authorize_request(&state.settings, &headers, "sidecars:write").await {
        return response;
    }
    special_json_relay_authorized(state, headers, body, SpecialRelayKind::ImageGeneration).await
}

pub(crate) async fn image_edits(
    State(state): State<GatewayState>,
    request: Request,
) -> Response<Body> {
    let headers = request.headers().clone();
    if let Err(response) = authorize_request(
        &state.settings,
        &headers,
        SpecialRelayKind::ImageEdit.scope(),
    )
    .await
    {
        return response;
    }
    let content_type = match headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    {
        Some(value) => value.to_string(),
        None => {
            return error_response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "missing_content_type",
                "image edits require application/json or multipart/form-data",
            )
        }
    };
    let limit = {
        let settings = state.settings.read().await;
        settings
            .providers
            .iter()
            .filter(|provider| provider.enabled && provider.capabilities.image_generation)
            .map(|provider| provider.limits.max_request_bytes)
            .max()
            .unwrap_or(16 * 1024 * 1024)
            .min(64 * 1024 * 1024) as usize
    };
    let bytes = match to_bytes(request.into_body(), limit).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "image edit request exceeds the configured limit",
            )
        }
    };
    if content_type
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
    {
        let body = match serde_json::from_slice::<Value>(&bytes) {
            Ok(body) => body,
            Err(_) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "image edit JSON is invalid",
                )
            }
        };
        return special_json_relay_authorized(state, headers, body, SpecialRelayKind::ImageEdit)
            .await;
    }
    if content_type
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("multipart/form-data"))
    {
        return special_multipart_image_edit(state, headers, content_type, bytes).await;
    }
    error_response(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported_content_type",
        "image edits require application/json or multipart/form-data",
    )
}

pub(crate) async fn special_multipart_image_edit(
    state: GatewayState,
    headers: HeaderMap,
    content_type: String,
    body: Bytes,
) -> Response<Body> {
    let boundary = match multipart_boundary(&content_type) {
        Ok(boundary) => boundary,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_multipart", &message)
        }
    };
    let (model_start, model_end, body_model) = match multipart_model_field(&body, &boundary) {
        Ok(model) => model,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_multipart", &message)
        }
    };
    let (requested_model, candidates, observability_settings) = {
        let settings = state.settings.read().await;
        let requested_model = settings.sidecars.image_model.clone().unwrap_or(body_model);
        let session_scope = crate::response_state::session_key_from_headers(&headers);
        let mut routing = state.routing.lock().await;
        (
            requested_model.clone(),
            routing.candidates_for_image_generation(
                &settings,
                settings.sidecars.image_model.as_deref(),
                Some(&requested_model),
                session_scope.as_deref(),
            ),
            settings.observability.clone(),
        )
    };
    let candidates = match candidates {
        Ok(candidates) => candidates
            .into_iter()
            .filter(|candidate| candidate.capabilities.image_generation)
            .collect::<Vec<_>>(),
        Err(message) => {
            if is_cooldown_rejection(&message) {
                return cooldown_response_for_message(&message);
            }
            return error_response(
                StatusCode::BAD_REQUEST,
                "image_generation_not_configured",
                &message,
            )
        }
    };
    if candidates.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "image_generation_not_configured",
            "no configured and available image-generation provider/model can serve image editing",
        );
    }

    let started = Instant::now();
    let request_id = Uuid::new_v4().to_string();
    let mut last_failure = None;
    for (index, candidate) in candidates.iter().enumerate() {
        let attempts = (index + 1).min(usize::from(u16::MAX)) as u16;
        let has_next = index + 1 < candidates.len();
        let candidate_started = Instant::now();
        let wire_model = candidate.provider.wire_model_id(&candidate.upstream_model);
        let candidate_body = replace_multipart_model(&body, model_start, model_end, &wire_model);
        let upstream = match send_special_raw_candidate(
            &state,
            &headers,
            candidate_body,
            &content_type,
            candidate,
            SpecialRelayKind::ImageEdit,
        )
        .await
        {
            Ok(upstream) => upstream,
            Err(failure) => {
                let provider_retry = failure.response.extensions()
                    .get::<ProviderRetryObservation>().cloned();
                if failure.kind != AttemptFailureKind::Request {
                    state.routing.lock().await.record_failure(candidate);
                }
                if has_next && failure.kind != AttemptFailureKind::Request {
                    let mut observation = ObservationSeed::for_candidate(
                        state.observability.clone(), observability_settings.clone(), request_id.clone(),
                        false, candidate_started, attempts, candidate,
                    ).with_upstream_image_details(
                        &wire_model, &SpecialRelayKind::ImageEdit.endpoint(candidate),
                    );
                    if let Some(retry) = provider_retry.as_ref() {
                        observation.record_provider_retries(retry);
                    }
                    observation.as_attempt().finish(
                        failure.response.status(), Some(failure.kind.category()), TokenUsage::default(),
                    );
                    last_failure = Some(failure.response);
                    continue;
                }
                let status = failure.response.status();
                let mut observation = ObservationSeed::for_candidate(
                    state.observability.clone(),
                    observability_settings.clone(),
                    request_id.clone(),
                    false,
                    started,
                    attempts,
                    candidate,
                ).with_upstream_image_details(
                    &wire_model,
                    &SpecialRelayKind::ImageEdit.endpoint(candidate),
                );
                if let Some(retry) = provider_retry.as_ref() {
                    observation.record_provider_retries(retry);
                }
                observation.finish(
                    status,
                    Some(failure.kind.category()),
                    TokenUsage::default(),
                );
                return failure.response;
            }
        };
        let provider_retry = upstream.extensions()
            .get::<ProviderRetryObservation>().cloned();
        if !upstream.status().is_success() {
            let status = upstream.status();
            let transient = status == StatusCode::REQUEST_TIMEOUT
                || status == StatusCode::TOO_MANY_REQUESTS
                || status.is_server_error();
            let retry_after = validated_retry_after(upstream.headers());
            if status == StatusCode::TOO_MANY_REQUESTS {
                state.routing.lock().await.record_quota_exhausted(
                    candidate,
                    retry_after.as_ref().and_then(|value| value.1),
                );
            } else if transient {
                state.routing.lock().await.record_failure(candidate);
            }
            let response =
                upstream_error(upstream, retry_after.as_ref().map(|value| &value.0)).await;
            if transient && has_next {
                let mut observation = ObservationSeed::for_candidate(
                    state.observability.clone(), observability_settings.clone(), request_id.clone(),
                    false, candidate_started, attempts, candidate,
                ).with_upstream_image_details(
                    &wire_model, &SpecialRelayKind::ImageEdit.endpoint(candidate),
                );
                if let Some(retry) = provider_retry.as_ref() {
                    observation.record_provider_retries(retry);
                }
                observation.record_upstream_error(&response);
                observation.as_attempt().finish(status, Some("provider_http_error"), TokenUsage::default());
                last_failure = Some(response);
                continue;
            }
            let mut observation = ObservationSeed::for_candidate(
                state.observability.clone(),
                observability_settings.clone(),
                request_id.clone(),
                false,
                started,
                attempts,
                candidate,
            ).with_upstream_image_details(
                &wire_model,
                &SpecialRelayKind::ImageEdit.endpoint(candidate),
            );
            if let Some(retry) = provider_retry.as_ref() {
                observation.record_provider_retries(retry);
            }
            observation.record_upstream_error(&response);
            observation.finish(status, Some("provider_http_error"), TokenUsage::default());
            return response;
        }
        let quota = quota_usage_percent(upstream.headers());
        let value = match bounded_json(upstream, candidate.provider.limits.max_response_bytes).await
        {
            Ok(value) => value,
            Err(message) => {
                state.routing.lock().await.record_failure(candidate);
                let response = error_response(
                    StatusCode::BAD_GATEWAY,
                    "invalid_provider_response",
                    &message,
                );
                if has_next {
                    let mut observation = ObservationSeed::for_candidate(
                        state.observability.clone(), observability_settings.clone(), request_id.clone(),
                        false, candidate_started, attempts, candidate,
                    ).with_upstream_image_details(
                        &wire_model, &SpecialRelayKind::ImageEdit.endpoint(candidate),
                    );
                    if let Some(retry) = provider_retry.as_ref() {
                        observation.record_provider_retries(retry);
                    }
                    observation.as_attempt().finish(
                        StatusCode::BAD_GATEWAY, Some("invalid_provider_response"), TokenUsage::default(),
                    );
                    last_failure = Some(response);
                    continue;
                }
                let mut observation = ObservationSeed::for_candidate(
                    state.observability.clone(),
                    observability_settings.clone(),
                    request_id.clone(),
                    false,
                    started,
                    attempts,
                    candidate,
                ).with_upstream_image_details(
                    &wire_model,
                    &SpecialRelayKind::ImageEdit.endpoint(candidate),
                );
                if let Some(retry) = provider_retry.as_ref() {
                    observation.record_provider_retries(retry);
                }
                observation.finish(
                    StatusCode::BAD_GATEWAY,
                    Some("invalid_provider_response"),
                    TokenUsage::default(),
                );
                return response;
            }
        };
        state.routing.lock().await.record_success(candidate, quota);
        let usage = TokenUsage::from_json(&value);
        let mut observation = ObservationSeed::for_candidate(
            state.observability.clone(),
            observability_settings.clone(),
            request_id.clone(),
            false,
            started,
            attempts,
            candidate,
        ).with_upstream_image_details(
            &wire_model,
            &SpecialRelayKind::ImageEdit.endpoint(candidate),
        );
        if let Some(retry) = provider_retry.as_ref() {
            observation.record_provider_retries(retry);
        }
        observation.finish(StatusCode::OK, None, usage);
        return json_response(StatusCode::OK, value);
    }
    let response = last_failure.unwrap_or_else(|| {
        error_response(
            StatusCode::BAD_GATEWAY,
            "provider_unavailable",
            "no image-edit provider completed the request",
        )
    });
    ObservationSeed::without_candidate(
        state.observability.clone(),
        observability_settings,
        request_id,
        &requested_model,
        false,
        started,
    )
    .finish(
        response.status(),
        Some("provider_unavailable"),
        TokenUsage::default(),
    );
    response
}

pub(crate) fn multipart_boundary(content_type: &str) -> Result<String, String> {
    let boundary = content_type
        .split(';')
        .skip(1)
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(name, value)| {
            name.trim()
                .eq_ignore_ascii_case("boundary")
                .then_some(value.trim())
        })
        .ok_or("multipart content type requires a boundary")?;
    let boundary = boundary
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(boundary);
    if boundary.is_empty()
        || boundary.len() > 200
        || !boundary.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'\''
                        | b'('
                        | b')'
                        | b'+'
                        | b'_'
                        | b','
                        | b'-'
                        | b'.'
                        | b'/'
                        | b':'
                        | b'='
                        | b'?'
                )
        })
    {
        return Err("multipart boundary is invalid".into());
    }
    Ok(boundary.to_string())
}

pub(crate) fn multipart_model_field(
    body: &[u8],
    boundary: &str,
) -> Result<(usize, usize, String), String> {
    multipart_text_field(body, boundary, "model", 240)?
        .ok_or_else(|| "multipart image edit requires a model field".into())
}

pub(crate) fn multipart_text_field(
    body: &[u8],
    boundary: &str,
    field_name: &str,
    max_bytes: usize,
) -> Result<Option<(usize, usize, String)>, String> {
    let delimiter = format!("--{boundary}").into_bytes();
    let next_delimiter = format!("\r\n--{boundary}").into_bytes();
    let mut cursor = 0;
    let mut found = None;
    while let Some(delimiter_start) = find_bytes(body, &delimiter, cursor) {
        let after = delimiter_start.saturating_add(delimiter.len());
        if body.get(after..after.saturating_add(2)) == Some(&b"--"[..]) {
            break;
        }
        if body.get(after..after.saturating_add(2)) != Some(&b"\r\n"[..]) {
            return Err("multipart delimiter is malformed".into());
        }
        let header_start = after + 2;
        let header_end = find_bytes(body, b"\r\n\r\n", header_start)
            .ok_or("multipart part headers are incomplete")?;
        let headers = std::str::from_utf8(&body[header_start..header_end])
            .map_err(|_| "multipart part headers are not UTF-8")?;
        let data_start = header_end + 4;
        let data_end = find_bytes(body, &next_delimiter, data_start)
            .ok_or("multipart part is not terminated")?;
        if multipart_part_name(headers).as_deref() == Some(field_name) {
            if found.is_some() {
                return Err(format!(
                    "multipart request contains duplicate {field_name} fields"
                ));
            }
            let value = std::str::from_utf8(&body[data_start..data_end])
                .map_err(|_| format!("multipart {field_name} field is not UTF-8"))?
                .trim();
            if value.is_empty() || value.len() > max_bytes {
                return Err(format!("multipart {field_name} field is invalid"));
            }
            found = Some((data_start, data_end, value.to_string()));
        }
        cursor = data_end + 2;
    }
    Ok(found)
}

pub(crate) fn multipart_part_name(headers: &str) -> Option<String> {
    let value = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("content-disposition")
            .then_some(value.trim())
    })?;
    value.split(';').skip(1).find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("name") {
            return None;
        }
        Some(
            value
                .trim()
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(value.trim())
                .to_string(),
        )
    })
}

pub(crate) fn find_bytes(haystack: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    if needle.is_empty() || start > haystack.len() {
        return None;
    }
    haystack[start..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| start + offset)
}

pub(crate) fn replace_multipart_model(
    body: &[u8],
    start: usize,
    end: usize,
    model: &str,
) -> Vec<u8> {
    let mut output = Vec::with_capacity(body.len() - (end - start) + model.len());
    output.extend_from_slice(&body[..start]);
    output.extend_from_slice(model.as_bytes());
    output.extend_from_slice(&body[end..]);
    output
}
