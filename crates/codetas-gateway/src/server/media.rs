use super::*;

pub(crate) async fn search_relay(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    special_json_relay(state, headers, body, SpecialRelayKind::Search).await
}

pub(crate) async fn video_generations(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    special_json_relay(state, headers, body, SpecialRelayKind::VideoGeneration).await
}

pub(crate) async fn video_sidecar(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Response<Body> {
    let wait = body.get("wait").and_then(Value::as_bool).unwrap_or(true);
    let timeout_ms = body
        .get("timeout_ms")
        .or_else(|| body.get("timeoutMs"))
        .and_then(Value::as_u64)
        .unwrap_or(300_000)
        .clamp(5_000, 600_000);
    if let Err(response) = authorize_request(&state.settings, &headers, "sidecars:write").await {
        return response;
    }
    if let Some(object) = body.as_object_mut() {
        object.remove("wait");
        object.remove("timeout_ms");
        object.remove("timeoutMs");
    }
    let response = special_json_relay_authorized(
        state.clone(),
        headers.clone(),
        body,
        SpecialRelayKind::VideoGeneration,
    )
    .await;
    if !response.status().is_success() {
        return response;
    }
    let value = match response_json(response, 1024 * 1024).await {
        Ok(value) => value,
        Err(message) => {
            return error_response(StatusCode::BAD_GATEWAY, "invalid_video_response", &message)
        }
    };
    let value = normalize_video_status(value, "codetas-sidecar/video");
    if matches!(
        value.get("status").and_then(Value::as_str),
        Some("completed" | "failed" | "expired")
    ) {
        return json_response(StatusCode::OK, value);
    }
    let Some(request_id) = video_request_id(&value) else {
        return json_response(StatusCode::OK, value);
    };
    if !wait {
        return json_response(
            StatusCode::ACCEPTED,
            json!({
                "object": "codetas.video.job",
                "request_id": request_id,
                "status": "processing",
            }),
        );
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut delay = Duration::from_secs(5);
    loop {
        let polled = video_status_inner(&state, &headers, &request_id).await;
        match polled {
            Ok(value) => match value.get("status").and_then(Value::as_str) {
                Some("completed" | "failed" | "expired") => {
                    return json_response(StatusCode::OK, value)
                }
                _ => {}
            },
            Err(failure) if failure.kind == AttemptFailureKind::Request => return failure.response,
            Err(_) => {}
        }
        if Instant::now() >= deadline {
            return json_response(
                StatusCode::ACCEPTED,
                json!({
                    "object": "codetas.video.job",
                    "request_id": request_id,
                    "status": "processing",
                    "timed_out": true,
                }),
            );
        }
        tokio::time::sleep(delay).await;
        delay = (delay + delay / 2).min(Duration::from_secs(15));
    }
}

pub(crate) async fn video_sidecar_status(
    State(state): State<GatewayState>,
    AxumPath(request_id): AxumPath<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize_request(&state.settings, &headers, "sidecars:write").await {
        return response;
    }
    if !valid_video_request_id(&request_id) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_video_request_id",
            "video request ID must contain 1-240 ASCII letters, digits, dots, underscores, or hyphens",
        );
    }
    match video_status_inner(&state, &headers, &request_id).await {
        Ok(value) => json_response(StatusCode::OK, value),
        Err(failure) => failure.response,
    }
}

pub(crate) fn video_request_id(value: &Value) -> Option<String> {
    value
        .get("request_id")
        .or_else(|| value.get("id"))
        .and_then(Value::as_str)
        .filter(|value| valid_video_request_id(value))
        .map(str::to_string)
}

pub(crate) async fn video_status(
    State(state): State<GatewayState>,
    AxumPath(request_id): AxumPath<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize_request(&state.settings, &headers, "videos:write").await {
        return response;
    }
    if !valid_video_request_id(&request_id) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_video_request_id",
            "video request ID must contain 1-240 ASCII letters, digits, dots, underscores, or hyphens",
        );
    }
    match video_status_inner(&state, &headers, &request_id).await {
        Ok(value) => json_response(StatusCode::OK, value),
        Err(failure) => failure.response,
    }
}

pub(crate) async fn video_status_inner(
    state: &GatewayState,
    headers: &HeaderMap,
    request_id: &str,
) -> Result<Value, AttemptFailure> {
    let started = Instant::now();
    let observation_request_id = Uuid::new_v4().to_string();
    let observability_settings = state.settings.read().await.observability.clone();
    let job = {
        let mut jobs = state.video_jobs.lock().await;
        let now = Instant::now();
        jobs.retain(|_, job| {
            now.duration_since(job.created_at) <= Duration::from_secs(24 * 60 * 60)
        });
        jobs.get(request_id).cloned()
    };
    let Some(job) = job else {
        return Err(AttemptFailure {
            response: error_response(
                StatusCode::NOT_FOUND,
                "video_job_not_found",
                "video job is unknown or expired; jobs are intentionally process-local",
            ),
            kind: AttemptFailureKind::Request,
        });
    };
    let candidate = job.candidate;
    let upstream = match send_video_status_candidate(
        state,
        headers,
        &candidate,
        &job.upstream_request_id,
    ).await {
        Ok(upstream) => upstream,
        Err(failure) => {
            let retry = failure.response.extensions()
                .get::<ProviderRetryObservation>().cloned();
            let mut observation = ObservationSeed::for_candidate(
                state.observability.clone(), observability_settings, observation_request_id,
                false, started, 1, &candidate,
            );
            if let Some(retry) = retry.as_ref() {
                observation.record_provider_retries(retry);
            }
            observation.finish(
                failure.response.status(), Some(failure.kind.category()), TokenUsage::default(),
            );
            return Err(failure);
        }
    };
    let provider_retry = upstream.extensions()
        .get::<ProviderRetryObservation>().cloned();
    if !upstream.status().is_success() {
        let status = upstream.status();
        let response = upstream_error(upstream, None).await;
        let kind = if status == StatusCode::REQUEST_TIMEOUT
            || status == StatusCode::TOO_MANY_REQUESTS
            || status.is_server_error()
        {
            AttemptFailureKind::Retryable
        } else {
            AttemptFailureKind::Request
        };
        let mut observation = ObservationSeed::for_candidate(
            state.observability.clone(), observability_settings, observation_request_id,
            false, started, 1, &candidate,
        );
        if let Some(retry) = provider_retry.as_ref() {
            observation.record_provider_retries(retry);
        }
        observation.record_upstream_error(&response);
        observation.finish(status, Some("provider_http_error"), TokenUsage::default());
        return Err(AttemptFailure {
            response,
            kind,
        });
    }
    let value = match bounded_json(upstream, candidate.provider.limits.max_response_bytes).await {
        Ok(value) => value,
        Err(message) => {
            let failure = request_failure("invalid_provider_response", &message);
            let status = failure.response.status();
            let mut observation = ObservationSeed::for_candidate(
                state.observability.clone(), observability_settings, observation_request_id,
                false, started, 1, &candidate,
            );
            if let Some(retry) = provider_retry.as_ref() {
                observation.record_provider_retries(retry);
            }
            observation.finish(
                status, Some("invalid_provider_response"), TokenUsage::default(),
            );
            return Err(failure);
        }
    };
    let usage = TokenUsage::from_json(&value);
    let mut value = normalize_video_status(value, &candidate.exposed_model);
    rewrite_video_request_id(&mut value, request_id);
    let mut observation = ObservationSeed::for_candidate(
        state.observability.clone(), observability_settings, observation_request_id,
        false, started, 1, &candidate,
    );
    if let Some(retry) = provider_retry.as_ref() {
        observation.record_provider_retries(retry);
    }
    observation.finish(StatusCode::OK, None, usage);
    Ok(value)
}

pub(crate) fn valid_video_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 240
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

pub(crate) fn normalize_video_status(mut value: Value, model: &str) -> Value {
    let raw = value
        .get("status")
        .or_else(|| value.get("state"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let status = match raw.as_str() {
        "done" | "completed" | "succeeded" => "completed",
        "failed" | "error" => "failed",
        "expired" | "timeout" => "expired",
        _ => "processing",
    };
    let video_url = value
        .pointer("/video/url")
        .or_else(|| value.pointer("/videos/0/url"))
        .cloned();
    if let Some(object) = value.as_object_mut() {
        object.insert("object".into(), Value::String("codetas.video.job".into()));
        object.insert("status".into(), Value::String(status.into()));
        object.insert("model".into(), Value::String(model.into()));
        if !object.contains_key("video_url") {
            if let Some(url) = video_url {
                object.insert("video_url".into(), url);
            }
        }
    }
    value
}

pub(crate) fn rewrite_video_request_id(value: &mut Value, request_id: &str) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let mut replaced = false;
    for key in ["request_id", "id"] {
        if object.contains_key(key) {
            object.insert(key.into(), Value::String(request_id.into()));
            replaced = true;
        }
    }
    if !replaced {
        object.insert("request_id".into(), Value::String(request_id.into()));
    }
}
