use super::*;
use crate::catalog::public_model_id_matches;
use crate::config::model_has_image_generation_identity;

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
    let generated_catalog = build_codex_catalog(&settings);
    let selected_resolved = settings.catalog.selected_models.is_empty()
        || settings.catalog.selected_models.iter().all(|selected| {
            generated_catalog.models.iter().any(|model| {
                model.get("slug").and_then(Value::as_str).is_some_and(|published| {
                    public_model_id_matches(&settings, selected, published)
                })
            })
        });
    let catalog_synchronized = !generated_catalog.models.is_empty() && selected_resolved;
    let ready = enabled > 0 && default_ready && catalog_synchronized;
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
            "defaultProviderReady": default_ready,
            "catalogSynchronized": catalog_synchronized
        }),
    )
}

pub(crate) async fn compatibility_lab(
    State(state): State<GatewayState>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize_request(&state.settings, &headers, "compatibility:read").await {
        return response;
    }
    let settings = state.settings.read().await;
    if !settings.catalog.compatibility_lab {
        return error_response(
            StatusCode::NOT_FOUND,
            "compatibility_lab_disabled",
            "the read-only compatibility table is disabled",
        );
    }
    json_response(
        StatusCode::OK,
        serde_json::to_value(crate::conformance::compatibility_lab_report(&settings))
            .unwrap_or_else(|_| json!({"readOnly": true, "rows": []})),
    )
}

pub(crate) async fn route_dry_run(
    State(state): State<GatewayState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize_request(&state.settings, &headers, "routing:read").await {
        return response;
    }
    let model = query.get("model").map(String::as_str).unwrap_or_default();
    if model.trim().is_empty() || model.len() > 300 || model.chars().any(char::is_control) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_model", "model is required");
    }
    let is_subagent = query
        .get("isSubagent")
        .is_some_and(|value| matches!(value.as_str(), "1" | "true"));
    let settings = state.settings.read().await;
    let report = state
        .routing
        .lock()
        .await
        .dry_run(&settings, model, is_subagent);
    json_response(
        StatusCode::OK,
        serde_json::to_value(report).unwrap_or_else(|_| json!({"candidates": []})),
    )
}

pub(crate) async fn memory_status(
    State(state): State<GatewayState>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize_request(&state.settings, &headers, "memory:read").await {
        return response;
    }
    let settings = state.settings.read().await;
    let estimated_request_capacity = settings
        .runtime
        .memory_budget_bytes
        .checked_div(u64::from(settings.runtime.max_inflight_requests.max(1)))
        .unwrap_or(0);
    json_response(
        StatusCode::OK,
        json!({
            "budgetBytes": settings.runtime.memory_budget_bytes,
            "maxInflightRequests": settings.runtime.max_inflight_requests,
            "estimatedPerRequestBudgetBytes": estimated_request_capacity,
            "configuredBodyLimitBytes": configured_body_limit_bytes(settings.runtime.memory_budget_bytes),
            "inflightRequests": state.memory.inflight.load(Ordering::Acquire),
            "reservedBytes": state.memory.reserved_bytes.load(Ordering::Acquire),
            "rejectedRequests": state.memory.rejected.load(Ordering::Relaxed),
            "contentStored": false
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
        for model in provider_public_model_ids(&settings, provider) {
            data.push(json!({
                "id": format!("{}/{}", provider.id, model),
                "object": "model",
                "owned_by": provider.id
            }));
        }
    }
    for route in settings
        .routes
        .iter()
        .filter(|route| route_is_public_normal_model(&settings, route))
    {
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
    apply_public_model_controls(&settings, &mut data);
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

fn provider_public_model_ids(
    settings: &GatewaySettings,
    provider: &ProviderDefinition,
) -> Vec<String> {
    let mut models = provider
        .models
        .iter()
        .filter(|model| !model_has_image_generation_identity(settings, provider, model))
        .cloned()
        .collect::<Vec<_>>();
    if let Some(default_model) = provider.default_model.as_deref() {
        if !model_has_image_generation_identity(settings, provider, default_model)
            && !models.iter().any(|model| model == default_model)
        {
            models.insert(0, default_model.to_string());
        }
    }
    for metadata in settings.model_catalog.iter().filter(|metadata| {
        metadata.enabled
            && metadata.provider_id == provider.id
            && !model_has_image_generation_identity(settings, provider, &metadata.model_id)
    }) {
        if !models.iter().any(|model| model == &metadata.model_id) {
            models.push(metadata.model_id.clone());
        }
    }
    models
}

fn route_is_public_normal_model(
    settings: &GatewaySettings,
    route: &crate::config::RouteDefinition,
) -> bool {
    route.enabled
        && route.targets.iter().any(|target| {
            let Some((provider_id, model_id)) = target.model.split_once('/') else {
                return false;
            };
            settings
                .providers
                .iter()
                .find(|provider| provider.id == provider_id)
                .is_some_and(|provider| {
                    !model_has_image_generation_identity(settings, provider, model_id)
                })
        })
}

fn apply_public_model_controls(settings: &GatewaySettings, data: &mut Vec<Value>) {
    if !settings.catalog.selected_models.is_empty() {
        data.retain(|model| {
            model
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| {
                    settings.catalog.selected_models.iter().any(|selected| {
                        public_model_id_matches(settings, selected, id)
                    })
                })
        });
    }
    if !settings.catalog.model_picker_order.is_empty() {
        data.sort_by_key(|model| {
            model
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| {
                    settings
                        .catalog
                        .model_picker_order
                        .iter()
                        .position(|configured| public_model_id_matches(settings, configured, id))
                })
                .unwrap_or(usize::MAX)
        });
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
    let models = build_codex_catalog(settings).models
        .into_iter()
        .filter_map(|model| model.get("slug").and_then(Value::as_str).map(str::to_string))
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
    Json(body): Json<Value>,
) -> Response<Body> {
    compact_response_inner(state, headers, body, CompactionRequestKind::Standalone).await
}

/// Handle a compaction trigger submitted through `/v1/responses`.
///
/// Codex remote compaction v2 uses the regular Responses streaming contract,
/// while the standalone `/v1/responses/compact` endpoint remains synchronous.
pub(crate) async fn compact_response_from_responses(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    compact_response_inner(state, headers, body, CompactionRequestKind::NativeTrigger).await
}

async fn compact_response_inner(
    state: GatewayState,
    headers: HeaderMap,
    body: Value,
    request_kind: CompactionRequestKind,
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
    let streaming = object.get("stream").and_then(Value::as_bool) == Some(true);
    if streaming && request_kind == CompactionRequestKind::Standalone {
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
        let candidate_body = compaction_body_for_candidate(&body, candidate);
        // Compaction requests carry the full conversation history by design and
        // routinely exceed the model's input budget, so no input-size gate is
        // applied here (the backend decides whether the history is compactable).
        let upstream = match candidate_compaction_mode(candidate, request_kind) {
            CompactionMode::Local => {
                match synthetic_compact_candidate(
                    &state,
                    &headers,
                    &candidate_body,
                    candidate,
                    request_kind,
                )
                .await
                {
                    Ok((value, usage)) => {
                        state.routing.lock().await.record_success(candidate, None);
                        ObservationSeed::for_candidate(
                            state.observability.clone(),
                            observability_settings.clone(),
                            request_id.clone(),
                            streaming,
                            started,
                            attempts,
                            candidate,
                        )
                        .finish(StatusCode::OK, None, usage);
                        return compaction_client_response(StatusCode::OK, value, streaming);
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
                            streaming,
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
                let mut compact_body = candidate_body;
                send_candidate(&state, &mut compact_body, candidate, Some(&headers)).await
            }
            CompactionMode::CompactEndpoint => {
                send_compact_candidate(&state, &candidate_body, candidate, Some(&headers)).await
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
                    streaming,
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
                streaming,
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
                state.routing.lock().await.record_failure(candidate);
                if has_next {
                    last_failure = Some(response);
                    continue;
                }
                ObservationSeed::for_candidate(
                    state.observability.clone(),
                    observability_settings.clone(),
                    request_id.clone(),
                    streaming,
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
        if request_kind == CompactionRequestKind::Standalone {
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
        let value = match ensure_single_compaction_output(value, &candidate.exposed_model) {
            Ok(value) => value,
            Err(message) => {
                let response = error_response(
                    StatusCode::BAD_GATEWAY,
                    "invalid_compaction_response",
                    &message,
                );
                state.routing.lock().await.record_failure(candidate);
                if has_next {
                    last_failure = Some(response);
                    continue;
                }
                ObservationSeed::for_candidate(
                    state.observability.clone(),
                    observability_settings.clone(),
                    request_id.clone(),
                    streaming,
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
            streaming,
            started,
            attempts,
            candidate,
        )
        .finish(StatusCode::OK, None, TokenUsage::from_json(&value));
        return compaction_client_response(StatusCode::OK, value, streaming);
    }
    last_failure.unwrap_or_else(|| {
        error_response(
            StatusCode::BAD_GATEWAY,
            "provider_unavailable",
            "no compact-capable provider completed the request",
        )
    })
}

fn compaction_body_for_candidate(body: &Value, candidate: &RouteCandidate) -> Value {
    let mut candidate_body = body.clone();
    candidate_body["model"] =
        Value::String(candidate.provider.wire_model_id(&candidate.upstream_model));
    candidate_body
}

fn compaction_client_response(
    status: StatusCode,
    value: Value,
    streaming: bool,
) -> Response<Body> {
    if !streaming {
        return json_response(status, value);
    }
    let mut value = value;
    if let Some(object) = value.as_object_mut() {
        object.insert("object".into(), Value::String("response".into()));
    }
    let mut encoded = String::new();
    for event in websocket_json_response_events(&value) {
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message");
        encoded.push_str(&sse(event_type, &event));
    }
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from(encoded))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "gateway_error",
                "failed to build compaction stream response",
            )
        })
}

/// Reassemble a compaction response into the synchronous JSON `response`
/// object. Synchronous JSON is passed through; streaming responses are
/// reassembled from added items and deltas, because some backends omit done
/// events or emit `response.completed` with an empty/partial `output` (and may
/// omit Content-Type).
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
    if !pending.iter().all(u8::is_ascii_whitespace) {
        return Err("compaction stream ended with an incomplete SSE frame".into());
    }
    compaction_value_from_sse_events(values)
}

fn compaction_value_from_sse_events(values: Vec<Value>) -> Result<Value, String> {
    let mut completed: Option<Value> = None;
    let mut snapshot = ResponsesSnapshotAccumulator::default();
    for mut value in values {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match kind.as_str() {
            "response.completed" => {
                snapshot.observe(&value);
                snapshot.repair_compaction_terminal_event(&mut value);
                if snapshot.is_tainted() {
                    return Err("compaction stream contained an unproven or non-contiguous output lifecycle".into());
                }
                let response = value
                    .get("response")
                    .ok_or("compaction completed event is missing its response object")?;
                require_completed_compaction_source(response)?;
                completed = Some(value);
            }
            "response.failed" | "response.incomplete" => {
                let message = value
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| {
                        if kind == "response.incomplete" {
                            "compaction was incomplete upstream"
                        } else {
                            "compaction failed upstream"
                        }
                    });
                return Err(format!("compaction terminal failure: {message}"));
            }
            _ => snapshot.observe(&value),
        }
    }
    let completed = completed
        .ok_or("compaction stream ended before a completed event".to_string())?;
    let response = completed
        .get("response")
        .cloned()
        .ok_or("compaction response is missing its response object".to_string())?;
    require_completed_compaction_source(&response)?;
    Ok(response)
}

#[cfg(test)]
mod public_model_control_tests {
    use super::*;
    use crate::config::ProviderDefinition;

    fn settings_with_openai() -> GatewaySettings {
        GatewaySettings {
            providers: vec![ProviderDefinition {
                id: "openai".into(),
                name: "OpenAI".into(),
                enabled: true,
                models: vec!["gpt-test".into(), "gpt-second".into()],
                ..ProviderDefinition::default()
            }],
            ..GatewaySettings::default()
        }
    }

    #[test]
    fn standard_model_list_excludes_image_only_models_from_all_sources() {
        let provider = ProviderDefinition {
            id: "openai".into(),
            name: "OpenAI".into(),
            default_model: Some("gpt-image-2".into()),
            models: vec!["gpt-test".into(), "gpt-image-2".into()],
            image_generation_models: vec!["imagegen-2".into(), "gpt-image-2".into()],
            ..ProviderDefinition::default()
        };
        let settings = GatewaySettings {
            providers: vec![provider.clone()],
            model_catalog: vec![crate::config::ModelMetadata {
                provider_id: "openai".into(),
                model_id: "imagegen-2".into(),
                enabled: true,
                ..crate::config::ModelMetadata::default()
            }],
            ..GatewaySettings::default()
        };

        assert_eq!(
            provider_public_model_ids(&settings, &provider),
            vec!["gpt-test"]
        );
    }

    #[test]
    fn standard_model_list_excludes_all_image_routes_but_keeps_mixed_routes() {
        let provider = ProviderDefinition {
            id: "openai-api".into(),
            name: "OpenAI API".into(),
            models: vec!["gpt-5.5".into()],
            image_generation_models: vec!["gpt-image-2".into()],
            capabilities: crate::config::ProviderCapabilities {
                image_generation: true,
                ..crate::config::ProviderCapabilities::default()
            },
            ..ProviderDefinition::default()
        };
        let image_route = crate::config::RouteDefinition {
            id: "image-route".into(),
            name: "Image route".into(),
            targets: vec![crate::config::RouteTarget {
                model: "openai-api/gpt-image-2".into(),
                weight: 1,
            }],
            enabled: true,
            ..crate::config::RouteDefinition::default()
        };
        let mixed_route = crate::config::RouteDefinition {
            id: "mixed-route".into(),
            name: "Mixed route".into(),
            targets: vec![
                crate::config::RouteTarget {
                    model: "openai-api/gpt-image-2".into(),
                    weight: 1,
                },
                crate::config::RouteTarget {
                    model: "openai-api/gpt-5.5".into(),
                    weight: 1,
                },
            ],
            enabled: true,
            ..crate::config::RouteDefinition::default()
        };
        let settings = GatewaySettings {
            providers: vec![provider],
            ..GatewaySettings::default()
        };

        assert!(!route_is_public_normal_model(&settings, &image_route));
        assert!(route_is_public_normal_model(&settings, &mixed_route));
    }

    #[test]
    fn standard_model_list_accepts_native_and_qualified_openai_allowlist_ids() {
        for selected in ["gpt-test", "openai/gpt-test"] {
            let mut settings = settings_with_openai();
            settings.catalog.selected_models = vec![selected.into()];
            let mut models = vec![
                json!({"id": "openai/gpt-test"}),
                json!({"id": "openai/gpt-second"}),
                json!({"id": "other/model"}),
            ];

            apply_public_model_controls(&settings, &mut models);

            assert_eq!(models, vec![json!({"id": "openai/gpt-test"})]);
        }
    }

    #[test]
    fn standard_model_list_orders_openai_aliases_without_aliasing_unknown_ids() {
        let mut settings = settings_with_openai();
        settings.catalog.model_picker_order =
            vec!["gpt-second".into(), "openai/gpt-test".into()];
        let mut models = vec![
            json!({"id": "openai/gpt-test"}),
            json!({"id": "openai/gpt-second"}),
            json!({"id": "openai/not-configured"}),
        ];

        apply_public_model_controls(&settings, &mut models);

        assert_eq!(models[0]["id"], "openai/gpt-second");
        assert_eq!(models[1]["id"], "openai/gpt-test");
        assert_eq!(models[2]["id"], "openai/not-configured");

        settings.catalog.selected_models = vec!["not-configured".into()];
        apply_public_model_controls(&settings, &mut models);
        assert!(models.is_empty());
    }
}

#[cfg(test)]
mod compaction_response_tests {
    use super::*;

    fn compaction_candidate() -> RouteCandidate {
        let mut provider = ProviderDefinition::default();
        provider.id = "openai-api".into();
        provider.base_url = "https://api.openai.com/v1".into();
        provider.protocol = ProviderProtocol::Responses;
        let capabilities = provider.capabilities.clone();
        RouteCandidate {
            provider,
            upstream_model: "gpt-wire".into(),
            exposed_model: "gpt-alias".into(),
            credential: None,
            account_id: None,
            target_key: "openai-api/gpt-alias".into(),
            route_id: None,
            failure_threshold: 0,
            quota_threshold_percent: 0,
            input_price_per_million: None,
            output_price_per_million: None,
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            capabilities,
        }
    }

    fn compacted_value() -> Value {
        json!({
            "id": "resp_compact_test",
            "object": "response.compaction",
            "model": "gpt-test",
            "output": [{
                "id": "cmpctitem_test",
                "type": "compaction",
                "encrypted_content": "codetas1:test"
            }]
        })
    }

    fn compact_v1_value() -> Value {
        json!({
            "output": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "summary"}]
            }]
        })
    }

    #[test]
    fn native_compaction_candidate_body_preserves_trigger_input_tools_and_streaming_fields() {
        let body = json!({
            "model": "gpt-alias",
            "stream": true,
            "store": true,
            "tools": [{"type": "function", "name": "lookup"}],
            "input": [
                {"type": "message", "role": "user", "content": "hello"},
                {"type": "compaction_trigger", "id": "trigger_1"}
            ]
        });

        let routed = compaction_body_for_candidate(&body, &compaction_candidate());

        assert_eq!(routed["model"], "gpt-wire");
        for field in ["stream", "store", "tools", "input"] {
            assert_eq!(routed[field], body[field], "{field} changed during routing");
        }
    }

    #[tokio::test]
    async fn responses_compaction_stream_ends_with_completed_event() {
        let response = compaction_client_response(StatusCode::OK, compacted_value(), true);

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );
        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("compaction SSE body");
        let body = std::str::from_utf8(&body).expect("UTF-8 compaction SSE");

        let added = body
            .find("event: response.output_item.added")
            .expect("compaction item added event");
        let done = body
            .find("event: response.output_item.done")
            .expect("compaction item done event");
        let completed = body
            .find("event: response.completed")
            .expect("compaction completed event");
        assert!(added < done && done < completed);
        assert!(body.contains("\"type\":\"compaction\""));
        assert!(body.contains("\"object\":\"response\""));
    }

    #[tokio::test]
    async fn standalone_compaction_response_stays_json() {
        let response = compaction_client_response(StatusCode::OK, compact_v1_value(), false);

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("compaction JSON body");
        let value: Value = serde_json::from_slice(&body).expect("valid compaction JSON");

        assert!(value.get("object").is_none());
        assert_eq!(value["output"][0]["type"], "message");
        assert!(!serde_json::to_string(&value).unwrap().contains("codetas1:"));
    }

    #[test]
    fn compaction_sse_recovers_done_text_only_from_a_completed_terminal() {
        let value = compaction_value_from_sse_events(vec![
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"id": "msg_summary", "type": "message", "status": "in_progress",
                    "role": "assistant", "content": []}
            }),
            json!({
                "type": "response.output_text.done",
                "output_index": 0,
                "item_id": "msg_summary",
                "content_index": 0,
                "text": "summary from done"
            }),
            json!({
                "type": "response.completed",
                "response": {"id": "resp_summary", "status": "completed", "output": []}
            }),
        ])
        .expect("completed compaction stream");

        assert_eq!(response_output_text(&value), "summary from done");
        assert!(ensure_single_compaction_output(value, "gpt-test").is_ok());
    }

    #[test]
    fn compaction_sse_rejects_delta_only_items_and_completed_index_gaps() {
        for events in [
            vec![
                json!({
                    "type": "response.function_call_arguments.delta",
                    "output_index": 0,
                    "item_id": "call_unproven",
                    "delta": "{}"
                }),
                json!({
                    "type": "response.completed",
                    "response": {"id": "resp_unproven", "status": "completed", "output": []}
                }),
            ],
            vec![
                json!({
                    "type": "response.output_item.done",
                    "output_index": 1,
                    "item": {"id": "msg_gap", "type": "message", "status": "completed",
                        "role": "assistant", "content": []}
                }),
                json!({
                    "type": "response.completed",
                    "response": {"id": "resp_gap", "status": "completed", "output": []}
                }),
            ],
        ] {
            assert!(compaction_value_from_sse_events(events).is_err());
        }
    }

    #[test]
    fn compaction_sse_rejects_failed_and_incomplete_terminals() {
        for terminal in [
            json!({
                "type": "response.failed",
                "response": {"id": "resp_failed", "status": "failed",
                    "error": {"message": "upstream exploded"}, "output": []}
            }),
            json!({
                "type": "response.incomplete",
                "response": {"id": "resp_partial", "status": "incomplete",
                    "incomplete_details": {"reason": "max_output_tokens"},
                    "output": [{"type": "message", "content": [{"type": "output_text", "text": "partial"}]}]}
            }),
        ] {
            assert!(compaction_value_from_sse_events(vec![terminal]).is_err());
        }
    }
}
