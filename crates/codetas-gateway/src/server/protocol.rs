use super::*;

pub(crate) async fn anthropic_messages(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    let admission = match authorize_request(&state.settings, &headers, "messages:write").await {
        Ok(admission) => admission,
        Err(response) => return response,
    };
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let translated = match anthropic_request_to_responses(&body) {
        Ok(translated) => translated,
        Err(message) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "unsupported_anthropic_request",
                &message,
            )
        }
    };
    let response =
        responses_inner(state, headers, translated, admission.trusts_turn_metadata()).await;
    if !response.status().is_success() {
        return response;
    }
    if streaming {
        return responses_stream_to_anthropic(response, model);
    }
    let status = response.status();
    let bytes = match axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_gateway_response",
                &format!("failed to read Responses result: {error}"),
            )
        }
    };
    let value = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => value,
        Err(error) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_gateway_response",
                &format!("Responses result is not JSON: {error}"),
            )
        }
    };
    match responses_to_anthropic_response(&value, &model) {
        Ok(value) => json_response(status, value),
        Err(message) => error_response(
            StatusCode::BAD_GATEWAY,
            "invalid_gateway_response",
            &message,
        ),
    }
}

pub(crate) async fn gemini_wildcard_request(
    State(state): State<GatewayState>,
    AxumPath(operation): AxumPath<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    let operation = operation.trim_start_matches('/');
    let (model, streaming) = if let Some(model) = operation.strip_suffix(":streamGenerateContent") {
        (model, true)
    } else if let Some(model) = operation.strip_suffix(":generateContent") {
        (model, false)
    } else {
        return error_response(
            StatusCode::NOT_FOUND,
            "gemini_method_not_found",
            "Gemini endpoint supports generateContent or streamGenerateContent",
        );
    };
    let model = model.strip_prefix("models/").unwrap_or(model);
    if model.is_empty() || model.len() > 300 || model.chars().any(char::is_control) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_gemini_model",
            "Gemini model path is invalid",
        );
    }
    gemini_client_request(state, headers, body, model.to_string(), streaming).await
}

pub(crate) async fn gemini_client_request(
    state: GatewayState,
    headers: HeaderMap,
    body: Value,
    model: String,
    streaming: bool,
) -> Response<Body> {
    let admission = match authorize_request(&state.settings, &headers, "gemini:write").await {
        Ok(admission) => admission,
        Err(response) => return response,
    };
    // The external streaming surface is reframed after one bounded Responses result. This keeps
    // Gemini clients compatible even when the selected provider does not expose an SSE protocol.
    let translated = match gemini_request_to_responses(&body, &model, false) {
        Ok(translated) => translated,
        Err(message) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "unsupported_gemini_request",
                &message,
            )
        }
    };
    let response =
        responses_inner(state, headers, translated, admission.trusts_turn_metadata()).await;
    if !response.status().is_success() {
        return response;
    }
    let status = response.status();
    let bytes = match axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_gateway_response",
                &format!("failed to read Responses result: {error}"),
            )
        }
    };
    let value = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => value,
        Err(error) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_gateway_response",
                &format!("Responses result is not JSON: {error}"),
            )
        }
    };
    let value = match responses_to_gemini_response(&value) {
        Ok(value) => value,
        Err(message) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_gateway_response",
                &message,
            )
        }
    };
    if !streaming {
        return json_response(status, value);
    }
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from(format!("data: {}\n\n", value)))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "gateway_error",
                "failed to build Gemini stream response",
            )
        })
}

pub(crate) fn responses_stream_to_anthropic(
    response: Response<Body>,
    model: String,
) -> Response<Body> {
    let status = response.status();
    let mut source = response.into_body().into_data_stream();
    let output = stream! {
        let mut state = ResponsesToAnthropicStream::new(model);
        let mut pending = Vec::new();
        let mut failed = false;
        while let Some(chunk) = source.next().await {
            let bytes = match chunk {
                Ok(bytes) => bytes,
                Err(_) => {
                    for event in state.push(&json!({"type": "error"})) {
                        yield Ok::<Bytes, Infallible>(Bytes::from(event));
                    }
                    failed = true;
                    break;
                }
            };
            let events = match drain_sse_values(&mut pending, &bytes) {
                Ok(events) => events,
                Err(_) => {
                    for event in state.push(&json!({"type": "error"})) {
                        yield Ok(Bytes::from(event));
                    }
                    failed = true;
                    break;
                }
            };
            for event in events {
                for event in state.push(&event) {
                    yield Ok(Bytes::from(event));
                }
                if failed {
                    break;
                }
            }
            if failed {
                break;
            }
        }
        if !failed {
            if !pending.iter().all(u8::is_ascii_whitespace) {
                for event in state.push(&json!({"type": "error"})) {
                    yield Ok(Bytes::from(event));
                }
            } else {
                for event in state.finish() {
                    yield Ok(Bytes::from(event));
                }
            }
        }
    };
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(output))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "gateway_error",
                "failed to build Anthropic stream",
            )
        })
}

pub(crate) async fn chat_completions(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    let admission = match authorize_request(&state.settings, &headers, "chat:write").await {
        Ok(admission) => admission,
        Err(response) => return response,
    };
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let translated = match chat_request_to_responses(&body) {
        Ok(translated) => translated,
        Err(message) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "unsupported_chat_request",
                &message,
            )
        }
    };
    let response =
        responses_inner(state, headers, translated, admission.trusts_turn_metadata()).await;
    if !response.status().is_success() {
        return response;
    }
    if streaming {
        return responses_stream_to_chat(response, model);
    }
    let status = response.status();
    let bytes = match axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_gateway_response",
                &format!("failed to read Responses result: {error}"),
            )
        }
    };
    let value = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => value,
        Err(error) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_gateway_response",
                &format!("Responses result is not JSON: {error}"),
            )
        }
    };
    match responses_to_chat_response(&value, &model) {
        Ok(value) => json_response(status, value),
        Err(message) => error_response(
            StatusCode::BAD_GATEWAY,
            "invalid_gateway_response",
            &message,
        ),
    }
}

pub(crate) fn responses_stream_to_chat(response: Response<Body>, model: String) -> Response<Body> {
    let status = response.status();
    let mut source = response.into_body().into_data_stream();
    let output = stream! {
        let mut state = ResponsesToChatStream::new(model);
        let mut pending = Vec::new();
        let mut failed = false;
        while let Some(chunk) = source.next().await {
            let bytes = match chunk {
                Ok(bytes) => bytes,
                Err(_) => {
                    for event in state.push(&json!({"type": "error"})) {
                        yield Ok::<Bytes, Infallible>(Bytes::from(event));
                    }
                    failed = true;
                    break;
                }
            };
            let events = match drain_sse_values(&mut pending, &bytes) {
                Ok(events) => events,
                Err(_) => {
                    for event in state.push(&json!({"type": "error"})) {
                        yield Ok(Bytes::from(event));
                    }
                    failed = true;
                    break;
                }
            };
            for event in events {
                for event in state.push(&event) {
                    yield Ok(Bytes::from(event));
                }
            }
            if failed {
                break;
            }
        }
        if !failed && !pending.iter().all(u8::is_ascii_whitespace) {
            for event in state.push(&json!({"type": "error"})) {
                yield Ok(Bytes::from(event));
            }
        } else if !failed {
            for event in state.finish() {
                yield Ok(Bytes::from(event));
            }
        }
    };
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(output))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "gateway_error",
                "failed to build Chat stream",
            )
        })
}

pub(crate) async fn health(
    State(state): State<GatewayState>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize_request(&state.settings, &headers, "health:read").await {
        return response;
    }
    let settings = state.settings.read().await;
    json_response(
        StatusCode::OK,
        json!({
            "status": "ok",
            "service": "codetas-gateway",
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "instanceId": &state.instance_id,
            "providers": settings.providers.iter().filter(|provider| provider.enabled).count()
        }),
    )
}

pub(crate) async fn readiness(
    State(state): State<GatewayState>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize_request(&state.settings, &headers, "health:read").await {
        return response;
    }
    let settings = state.settings.read().await;
    let enabled = settings
        .providers
        .iter()
        .filter(|provider| provider.enabled)
        .count();
    let default_ready = settings.default_provider.as_deref().is_some_and(|default| {
        settings
            .providers
            .iter()
            .any(|provider| provider.enabled && provider.id == default)
    });
    let ready = enabled > 0 && default_ready;
    json_response(
        if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        json!({
            "ready": ready,
            "service": "codetas-gateway",
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "instanceId": &state.instance_id,
            "providers": enabled,
            "defaultProviderReady": default_ready
        }),
    )
}

pub(crate) async fn models(
    State(state): State<GatewayState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize_request(&state.settings, &headers, "models:read").await {
        return response;
    }
    if headers.contains_key("x-goog-api-key")
        || query.get("flavor").is_some_and(|flavor| flavor == "gemini")
    {
        let settings = state.settings.read().await;
        return gemini_models_response(&settings);
    }
    let settings = state.settings.read().await;
    if query.contains_key("client_version") {
        return json_response(
            StatusCode::OK,
            serde_json::to_value(build_codex_catalog(&settings))
                .unwrap_or_else(|_| json!({"models": []})),
        );
    }
    let mut data = Vec::new();
    for provider in settings
        .providers
        .iter()
        .filter(|provider| provider.enabled)
    {
        let mut models = provider.models.clone();
        if let Some(default_model) = provider.default_model.as_deref() {
            if !models.iter().any(|model| model == default_model) {
                models.insert(0, default_model.to_string());
            }
        }
        for metadata in settings
            .model_catalog
            .iter()
            .filter(|metadata| metadata.enabled && metadata.provider_id == provider.id)
        {
            if !models.iter().any(|model| model == &metadata.model_id) {
                models.push(metadata.model_id.clone());
            }
        }
        for model in models {
            data.push(json!({
                "id": format!("{}/{}", provider.id, model),
                "object": "model",
                "owned_by": provider.id
            }));
        }
    }
    for route in settings.routes.iter().filter(|route| route.enabled) {
        data.push(json!({
            "id": route.alias.as_deref().unwrap_or(&route.id),
            "object": "model",
            "owned_by": "codetas-route"
        }));
    }
    for (id, configured) in [
        (
            "codetas-sidecar/web-search",
            settings.sidecars.web_search_model.is_some(),
        ),
        (
            "codetas-sidecar/vision",
            settings.sidecars.vision_model.is_some(),
        ),
        (
            "codetas-sidecar/image",
            settings.sidecars.image_model.is_some(),
        ),
        (
            "codetas-sidecar/video",
            settings.sidecars.video_model.is_some(),
        ),
    ] {
        if configured {
            data.push(json!({
                "id": id,
                "object": "model",
                "owned_by": "codetas-sidecar"
            }));
        }
    }
    let anthropic_flavor = !query.contains_key("client_version")
        && (query
            .get("flavor")
            .is_some_and(|flavor| flavor == "anthropic")
            || headers.contains_key("anthropic-version"));
    if anthropic_flavor {
        let anthropic = data
            .into_iter()
            .filter_map(|model| {
                let id = model.get("id")?.as_str()?;
                Some(json!({
                    "id": id,
                    "type": "model",
                    "display_name": id,
                    "created_at": "1970-01-01T00:00:00Z"
                }))
            })
            .collect::<Vec<_>>();
        json_response(
            StatusCode::OK,
            json!({"data": anthropic, "has_more": false}),
        )
    } else {
        json_response(StatusCode::OK, json!({"object": "list", "data": data}))
    }
}

pub(crate) async fn gemini_models(
    State(state): State<GatewayState>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize_request(&state.settings, &headers, "models:read").await {
        return response;
    }
    let settings = state.settings.read().await;
    gemini_models_response(&settings)
}

pub(crate) fn gemini_models_response(settings: &GatewaySettings) -> Response<Body> {
    let mut names = Vec::new();
    for provider in settings
        .providers
        .iter()
        .filter(|provider| provider.enabled)
    {
        let mut models = provider.models.clone();
        if let Some(default) = provider.default_model.as_ref() {
            if !models.contains(default) {
                models.insert(0, default.clone());
            }
        }
        for model in models {
            names.push(format!("{}/{}", provider.id, model));
        }
    }
    names.extend(
        settings
            .routes
            .iter()
            .filter(|route| route.enabled)
            .map(|route| route.alias.clone().unwrap_or_else(|| route.id.clone())),
    );
    names.sort();
    names.dedup();
    let models = names
        .into_iter()
        .map(|name| {
            json!({
                "name": format!("models/{name}"),
                "baseModelId": name,
                "version": "codetas-v1",
                "displayName": format!("CODETAS · {name}"),
                "description": "Routed by CODETAS",
                "inputTokenLimit": authoritative_model_context(&settings, &name).unwrap_or(0),
                "outputTokenLimit": authoritative_model_output(&settings, &name).unwrap_or(0),
                "supportedGenerationMethods": ["generateContent", "streamGenerateContent"]
            })
        })
        .collect::<Vec<_>>();
    json_response(StatusCode::OK, json!({"models": models}))
}

pub(crate) fn authoritative_model_context(settings: &GatewaySettings, route: &str) -> Option<u64> {
    let (provider, model) = route.split_once('/')?;
    settings
        .model_catalog
        .iter()
        .find(|metadata| {
            metadata.enabled && metadata.provider_id == provider && metadata.model_id == model
        })
        .and_then(|metadata| metadata.context_window.or(metadata.max_input_tokens))
        .or_else(|| {
            settings
                .providers
                .iter()
                .find(|item| item.enabled && item.id == provider)
                .and_then(|item| item.model_context_windows.get(model).copied())
        })
}

pub(crate) fn authoritative_model_output(settings: &GatewaySettings, route: &str) -> Option<u64> {
    let (provider, model) = route.split_once('/')?;
    settings
        .model_catalog
        .iter()
        .find(|metadata| {
            metadata.enabled && metadata.provider_id == provider && metadata.model_id == model
        })
        .and_then(|metadata| metadata.max_output_tokens)
        .or_else(|| {
            settings
                .providers
                .iter()
                .find(|item| item.enabled && item.id == provider)
                .and_then(|item| item.model_max_output_tokens.get(model).copied())
        })
}

pub(crate) async fn compact_response(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Response<Body> {
    if let Err(response) = authorize_request(&state.settings, &headers, "responses:write").await {
        return response;
    }
    let Some(object) = body.as_object() else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_compaction_request",
            "compact request must be a JSON object",
        );
    };
    let requested_model = object
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if requested_model.is_empty() || !object.get("input").is_some_and(Value::is_array) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_compaction_request",
            "compact request requires model and an input item array",
        );
    }
    if object.get("stream").and_then(Value::as_bool) == Some(true) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_compaction_request",
            "standalone compaction does not support streaming",
        );
    }

    let started = Instant::now();
    let request_id = Uuid::new_v4().to_string();
    let (candidates, observability_settings) = {
        let settings = state.settings.read().await;
        let mut routing = state.routing.lock().await;
        (
            routing.candidates(&settings, &requested_model),
            settings.observability.clone(),
        )
    };
    let candidates = match candidates {
        Ok(candidates) => candidates,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_request", &message)
        }
    };
    let mut last_failure = None;
    for (index, candidate) in candidates.iter().enumerate() {
        let attempts = (index + 1).min(usize::from(u16::MAX)) as u16;
        let has_next = index + 1 < candidates.len();
        body["model"] = Value::String(candidate.provider.wire_model_id(&candidate.upstream_model));
        // Compaction requests carry the full conversation history by design and
        // routinely exceed the model's input budget, so no input-size gate is
        // applied here (the backend decides whether the history is compactable).
        let upstream = match candidate_compaction_mode(candidate) {
            CompactionMode::Local => {
                match synthetic_compact_candidate(&state, &headers, &body, candidate).await {
                    Ok((value, usage)) => {
                        state.routing.lock().await.record_success(candidate, None);
                        ObservationSeed::for_candidate(
                            state.observability.clone(),
                            observability_settings.clone(),
                            request_id.clone(),
                            false,
                            started,
                            attempts,
                            candidate,
                        )
                        .finish(StatusCode::OK, None, usage);
                        return json_response(StatusCode::OK, value);
                    }
                    Err(failure) => {
                        if matches!(
                            failure.kind,
                            AttemptFailureKind::Credential | AttemptFailureKind::Retryable
                        ) {
                            state.routing.lock().await.record_failure(candidate);
                        }
                        if has_next && failure.kind != AttemptFailureKind::Request {
                            last_failure = Some(failure.response);
                            continue;
                        }
                        ObservationSeed::for_candidate(
                            state.observability.clone(),
                            observability_settings.clone(),
                            request_id.clone(),
                            false,
                            started,
                            attempts,
                            candidate,
                        )
                        .finish(
                            failure.response.status(),
                            Some(failure.kind.category()),
                            TokenUsage::default(),
                        );
                        return failure.response;
                    }
                }
            }
            CompactionMode::Responses => {
                let mut compact_body = body.clone();
                // The ChatGPT backend requires `store:false` + `stream:true` on the
                // Responses endpoint; the client's non-streaming compaction envelope
                // is forwarded with `stream` forced on and the SSE response is
                // reassembled into a synchronous JSON compaction result below.
                if let Some(object) = compact_body.as_object_mut() {
                    object.insert("stream".into(), Value::Bool(true));
                    object.insert("store".into(), Value::Bool(false));
                }
                send_candidate(&state, &mut compact_body, candidate, Some(&headers)).await
            }
            CompactionMode::CompactEndpoint => {
                send_compact_candidate(&state, &body, candidate, Some(&headers)).await
            }
        };
        let upstream = match upstream {
            Ok(upstream) => upstream,
            Err(failure) => {
                if matches!(
                    failure.kind,
                    AttemptFailureKind::Credential | AttemptFailureKind::Retryable
                ) {
                    state.routing.lock().await.record_failure(candidate);
                }
                if has_next && failure.kind != AttemptFailureKind::Request {
                    last_failure = Some(failure.response);
                    continue;
                }
                ObservationSeed::for_candidate(
                    state.observability.clone(),
                    observability_settings.clone(),
                    request_id.clone(),
                    false,
                    started,
                    attempts,
                    candidate,
                )
                .finish(
                    failure.response.status(),
                    Some(failure.kind.category()),
                    TokenUsage::default(),
                );
                return failure.response;
            }
        };
        if !upstream.status().is_success() {
            let status = upstream.status();
            let retryable = status == StatusCode::REQUEST_TIMEOUT
                || status == StatusCode::TOO_MANY_REQUESTS
                || status.is_server_error();
            let retry_after = validated_retry_after(upstream.headers());
            let classified = upstream_responses_error_classified(
                upstream,
                retry_after.as_ref().map(|value| &value.0),
            )
            .await;
            let context_window_exceeded = classified.context_window_exceeded;
            let response = classified.response;
            if status == StatusCode::TOO_MANY_REQUESTS {
                state
                    .routing
                    .lock()
                    .await
                    .record_quota_exhausted(candidate, retry_after.and_then(|value| value.1));
            } else if retryable {
                state.routing.lock().await.record_failure(candidate);
            }
            if has_next && (retryable || context_window_exceeded) {
                last_failure = Some(response);
                continue;
            }
            ObservationSeed::for_candidate(
                state.observability.clone(),
                observability_settings.clone(),
                request_id.clone(),
                false,
                started,
                attempts,
                candidate,
            )
            .finish(status, Some("provider_http_error"), TokenUsage::default());
            return response;
        }
        let limit = candidate.provider.limits.max_response_bytes;
        // The ChatGPT backend omits Content-Type on streaming responses, so try
        // the synchronous JSON form first and fall back to SSE reassembly.
        let value = sse_to_compaction_value(upstream, limit).await;
        let value = match value {
            Ok(value) => value,
            Err(message) => {
                let response = error_response(
                    StatusCode::BAD_GATEWAY,
                    "invalid_provider_response",
                    &message,
                );
                ObservationSeed::for_candidate(
                    state.observability.clone(),
                    observability_settings.clone(),
                    request_id.clone(),
                    false,
                    started,
                    attempts,
                    candidate,
                )
                .finish(
                    StatusCode::BAD_GATEWAY,
                    Some("invalid_provider_response"),
                    TokenUsage::default(),
                );
                return response;
            }
        };
        let value = match ensure_single_compaction_output(value, &candidate.exposed_model) {
            Ok(value) => value,
            Err(message) => {
                let response = error_response(
                    StatusCode::BAD_GATEWAY,
                    "invalid_compaction_response",
                    &message,
                );
                ObservationSeed::for_candidate(
                    state.observability.clone(),
                    observability_settings.clone(),
                    request_id.clone(),
                    false,
                    started,
                    attempts,
                    candidate,
                )
                .finish(
                    StatusCode::BAD_GATEWAY,
                    Some("invalid_compaction_response"),
                    TokenUsage::default(),
                );
                return response;
            }
        };
        state.routing.lock().await.record_success(candidate, None);
        ObservationSeed::for_candidate(
            state.observability.clone(),
            observability_settings.clone(),
            request_id.clone(),
            false,
            started,
            attempts,
            candidate,
        )
        .finish(StatusCode::OK, None, TokenUsage::from_json(&value));
        return json_response(StatusCode::OK, value);
    }
    last_failure.unwrap_or_else(|| {
        error_response(
            StatusCode::BAD_GATEWAY,
            "provider_unavailable",
            "no compact-capable provider completed the request",
        )
    })
}

/// Reassemble a compaction response into the synchronous JSON `response`
/// object. Synchronous JSON is passed through; streaming responses are
/// reassembled from `response.output_item.done` events, because the backend
/// emits `response.completed` with an empty `output` (and omits Content-Type).
async fn sse_to_compaction_value(
    upstream: reqwest::Response,
    limit: u64,
) -> Result<Value, String> {
    let bytes = super::response::read_bounded(upstream, limit).await?;
    if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
        return Ok(value);
    }
    let mut pending: Vec<u8> = Vec::new();
    let values = super::stream::drain_sse_values(&mut pending, &bytes)?;
    let mut completed_items: std::collections::BTreeMap<usize, Value> = Default::default();
    let mut completed: Option<Value> = None;
    for mut value in values {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match kind.as_str() {
            "response.output_item.done" => {
                if let (Some(index), Some(item)) = (
                    value.get("output_index").and_then(Value::as_u64),
                    value.get("item").cloned(),
                ) {
                    completed_items.insert(index as usize, item);
                }
            }
            "response.completed" => completed = Some(value),
            "response.failed" => {
                let message = value
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("compaction failed upstream");
                return Err(format!("compaction failed upstream: {message}"));
            }
            _ => {}
        }
    }
    let mut completed = completed
        .ok_or("compaction stream ended before a completed event".to_string())?;
    if let Some(object) = completed.get_mut("response").and_then(Value::as_object_mut) {
        let items: Vec<Value> = completed_items.values().cloned().collect();
        object.insert("output".into(), Value::Array(items));
    }
    completed
        .get("response")
        .cloned()
        .ok_or("compaction response is missing its response object".to_string())
}
