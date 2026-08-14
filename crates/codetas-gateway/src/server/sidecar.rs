use super::*;

pub(crate) async fn web_search_sidecar(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    let query = body
        .get("query")
        .or_else(|| body.get("input"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if query.is_empty() || query.len() > 64 * 1024 {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_sidecar_request",
            "web-search sidecar requires a query of at most 64 KiB",
        );
    }
    let explicit_model = body.get("model").and_then(Value::as_str);
    let model = match sidecar_model(&state, "web-search", explicit_model).await {
        Ok(model) => model,
        Err(response) => return response,
    };
    let request = json!({
        "model": model,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": query}],
        }],
        "tools": [{"type": "web_search_preview"}],
        "tool_choice": "auto",
        "stream": false,
    });
    run_text_sidecar(state, headers, request, "web-search").await
}

pub(crate) async fn vision_sidecar(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    let prompt = body
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("Describe this image precisely, including visible text and details relevant to the user's task.");
    if prompt.len() > 64 * 1024 {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_sidecar_request",
            "vision sidecar prompt exceeds 64 KiB",
        );
    }
    let image = body
        .get("image_url")
        .or_else(|| body.get("imageUrl"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Err(message) = validate_sidecar_image_url(image) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_sidecar_request", &message);
    }
    let explicit_model = body.get("model").and_then(Value::as_str);
    let model = match sidecar_model(&state, "vision", explicit_model).await {
        Ok(model) => model,
        Err(response) => return response,
    };
    let request = json!({
        "model": model,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [
                {"type": "input_text", "text": prompt},
                {"type": "input_image", "image_url": image},
            ],
        }],
        "stream": false,
        "max_output_tokens": body.get("max_output_tokens").and_then(Value::as_u64).unwrap_or(2_048).clamp(64, 8_192),
    });
    run_text_sidecar(state, headers, request, "vision").await
}

pub(crate) async fn sidecar_model(
    state: &GatewayState,
    kind: &str,
    explicit: Option<&str>,
) -> Result<String, Response<Body>> {
    if let Some(model) = explicit.map(str::trim).filter(|value| !value.is_empty()) {
        return Ok(model.to_string());
    }
    let settings = state.settings.read().await;
    let configured = match kind {
        "web-search" => settings.sidecars.web_search_model.as_deref(),
        "vision" => settings.sidecars.vision_model.as_deref(),
        _ => None,
    };
    configured
        .map(|_| format!("codetas-sidecar/{kind}"))
        .ok_or_else(|| {
            error_response(
                StatusCode::BAD_REQUEST,
                "sidecar_not_configured",
                &format!("{kind} sidecar has no configured model"),
            )
        })
}

pub(crate) async fn run_text_sidecar(
    state: GatewayState,
    headers: HeaderMap,
    request: Value,
    kind: &str,
) -> Response<Body> {
    let admission = match authorize_request(&state.settings, &headers, "sidecars:write").await {
        Ok(admission) => admission,
        Err(response) => return response,
    };
    let response = responses_inner(state, headers, request, admission.trusts_turn_metadata()).await;
    if !response.status().is_success() {
        return response;
    }
    let status = response.status();
    let bytes = match axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_sidecar_response",
                "sidecar response could not be read",
            )
        }
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_sidecar_response",
                "sidecar response is not valid JSON",
            )
        }
    };
    let result = response_output_text(&value);
    if result.trim().is_empty() {
        return error_response(
            StatusCode::BAD_GATEWAY,
            "empty_sidecar_response",
            "sidecar completed without textual output",
        );
    }
    json_response(
        status,
        json!({
            "object": "codetas.sidecar.result",
            "kind": kind,
            "result": result,
            "response_id": value.get("id").cloned().unwrap_or(Value::Null),
            "model": value.get("model").cloned().unwrap_or(Value::Null),
            "usage": value.get("usage").cloned().unwrap_or(Value::Null),
        }),
    )
}

pub(crate) fn validate_sidecar_image_url(value: &str) -> Result<(), String> {
    if let Some(rest) = value.strip_prefix("data:image/") {
        let Some((subtype, data)) = rest.split_once(";base64,") else {
            return Err("vision sidecar inline image is malformed or too large".into());
        };
        if subtype.is_empty()
            || subtype.len() > 64
            || !subtype
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-'))
            || data.is_empty()
            || data.len() > 24 * 1024 * 1024
            || data.bytes().any(|byte| byte.is_ascii_whitespace())
            || STANDARD.decode(data.as_bytes()).is_err()
        {
            return Err("vision sidecar inline image is malformed or too large".into());
        }
        return Ok(());
    }
    let url = url::Url::parse(value)
        .map_err(|_| "vision sidecar requires an HTTPS or inline image URL".to_string())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("vision sidecar requires a credential-free HTTPS image URL".into());
    }
    Ok(())
}
