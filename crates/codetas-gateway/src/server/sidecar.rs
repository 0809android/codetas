use super::*;

const MAX_MEDIA_ANALYSIS_BATCH_ITEMS: usize = 4;
const MAX_MEDIA_ANALYSIS_PAYLOAD_CHARS: usize = 20 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(crate) struct MediaPreprocessingFailureCategory(pub(crate) &'static str);

fn media_preprocessing_error(
    status: StatusCode,
    code: &'static str,
    message: &str,
) -> Response<Body> {
    let mut response = error_response(status, code, message);
    response
        .extensions_mut()
        .insert(MediaPreprocessingFailureCategory(code));
    response
}

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
    match analyze_image_with_sidecar(
        &state,
        &headers,
        image,
        prompt,
        "vision",
        explicit_model,
        body.get("max_output_tokens").and_then(Value::as_u64),
        true,
    )
    .await
    {
        Ok(result) => sidecar_result_response("vision", result),
        Err(response) => response,
    }
}

pub(crate) async fn video_analysis_sidecar(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    analyze_image_collection_sidecar(state, headers, body, "video-analysis", "frames").await
}

pub(crate) async fn document_sidecar(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    analyze_image_collection_sidecar(state, headers, body, "document", "pages").await
}

pub(crate) async fn media_sidecar_config(
    State(state): State<GatewayState>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Err(response) = authorize_request(&state.settings, &headers, "sidecars:write").await {
        return response;
    }
    let settings = state.settings.read().await;
    json_response(
        StatusCode::OK,
        json!({
            "object": "codetas.sidecar.config",
            "imageInputMode": settings.agents.image_input_mode,
            "videoInputMode": settings.agents.video_input_mode,
            "documentInputMode": settings.agents.document_input_mode,
            "auxiliaryTimeoutMs": settings.agents.auxiliary_timeout_ms,
            "videoSampleFrames": settings.agents.video_sample_frames,
            "documentMaxPages": settings.agents.document_max_pages,
            "ocrEnabled": settings.agents.ocr_enabled,
            "analysisBatchItems": MAX_MEDIA_ANALYSIS_BATCH_ITEMS,
            "analysisMaxPayloadBytes": MAX_MEDIA_ANALYSIS_PAYLOAD_CHARS,
            "configured": {
                "vision": settings.sidecars.vision_model.is_some(),
                "videoAnalysis": settings.sidecars.video_input_model.is_some()
                    || settings.sidecars.vision_model.is_some(),
                "document": settings.sidecars.document_model.is_some()
                    || settings.sidecars.vision_model.is_some(),
                "imageGeneration": settings.sidecars.image_model.is_some(),
                "videoGeneration": settings.sidecars.video_model.is_some(),
            },
        }),
    )
}

async fn analyze_image_collection_sidecar(
    state: GatewayState,
    headers: HeaderMap,
    body: Value,
    kind: &str,
    field: &str,
) -> Response<Body> {
    let settings = state.settings.read().await;
    let (configured_limit, default_ocr_enabled, input_mode) = if kind == "video-analysis" {
        (
            usize::from(settings.agents.video_sample_frames),
            false,
            settings.agents.video_input_mode,
        )
    } else {
        (
            usize::from(settings.agents.document_max_pages),
            settings.agents.ocr_enabled,
            settings.agents.document_input_mode,
        )
    };
    drop(settings);
    let force_sidecar = body
        .get("force_sidecar")
        .or_else(|| body.get("forceSidecar"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if input_mode == crate::config::AuxiliaryInputMode::Native && !force_sidecar {
        return error_response(
            StatusCode::CONFLICT,
            "native_input_selected",
            &format!("{kind} input mode is native; attach the original file to the active model or explicitly force a sidecar test"),
        );
    }
    let request_limit = configured_limit.min(MAX_MEDIA_ANALYSIS_BATCH_ITEMS);
    let Some(items) = body.get(field).and_then(Value::as_array) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid_sidecar_request", &format!("{kind} sidecar requires {field}"));
    };
    if items.is_empty() || items.len() > request_limit {
        return error_response(StatusCode::BAD_REQUEST, "invalid_sidecar_request", &format!("{kind} sidecar accepts 1-{request_limit} {field} per request; split larger inputs into batches"));
    }
    let requested_prompt = body
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if requested_prompt.as_ref().is_some_and(|prompt| prompt.len() > 64 * 1024) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_sidecar_request", "media-analysis prompt exceeds 64 KiB");
    }
    let ocr_enabled = kind == "document"
        && body
            .get("ocr")
            .and_then(Value::as_bool)
            .unwrap_or(default_ocr_enabled);
    let prompt = requested_prompt.map_or_else(|| if kind == "document" {
        if ocr_enabled {
            "Read all visible text exactly where possible, then explain the document page accurately. Preserve headings, tables, code, and important values.".to_string()
        } else {
            "Explain the document page accurately. Preserve headings, tables, code, and important values.".to_string()
        }
    } else {
        "Describe this sampled video frame, including visible text, actions, scene changes, and details relevant to the user's task.".to_string()
    }, |prompt| {
        if ocr_enabled {
            format!("Read all visible text exactly where possible, then answer this request:\n{prompt}")
        } else {
            prompt
        }
    });
    let explicit_model = body.get("model").and_then(Value::as_str);
    let start_index = body
        .get("start_index")
        .or_else(|| body.get("startIndex"))
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0);
    let payload_chars = items
        .iter()
        .filter_map(Value::as_str)
        .try_fold(0usize, |total, image| total.checked_add(image.len()));
    if payload_chars.map_or(true, |total| total > MAX_MEDIA_ANALYSIS_PAYLOAD_CHARS) {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "media_batch_too_large",
            "media-analysis batch exceeds 20 MiB; resize the extracted images or send a smaller batch",
        );
    }
    let mut results = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let image = item.as_str().unwrap_or_default();
        if let Err(message) = validate_sidecar_image_url(image) {
            return error_response(StatusCode::BAD_REQUEST, "invalid_sidecar_request", &format!("{field}[{index}]: {message}"));
        }
        match analyze_image_with_sidecar(&state, &headers, image, &prompt, kind, explicit_model, None, true).await {
            Ok(result) => results.push(format!("{} {}:\n{}", if kind == "document" { "Page" } else { "Frame" }, start_index + index + 1, result)),
            Err(response) => return response,
        }
    }
    sidecar_result_response(kind, results.join("\n\n"))
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
        "video-analysis" => settings.sidecars.video_input_model.as_deref().or(settings.sidecars.vision_model.as_deref()),
        "document" => settings.sidecars.document_model.as_deref().or(settings.sidecars.vision_model.as_deref()),
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
    let response = responses_inner_without_media(state, headers, request, admission.trusts_turn_metadata()).await;
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

pub(crate) async fn analyze_image_with_sidecar(
    state: &GatewayState,
    headers: &HeaderMap,
    image: &str,
    prompt: &str,
    kind: &str,
    explicit_model: Option<&str>,
    max_output_tokens: Option<u64>,
    require_sidecar_scope: bool,
) -> Result<String, Response<Body>> {
    validate_sidecar_image_url(image)
        .map_err(|message| error_response(StatusCode::BAD_REQUEST, "invalid_sidecar_request", &message))?;
    let model = sidecar_model(state, kind, explicit_model).await?;
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
        "max_output_tokens": max_output_tokens.unwrap_or(2_048).clamp(64, 8_192),
    });
    let trust_turn_metadata = if require_sidecar_scope {
        authorize_request(&state.settings, headers, "sidecars:write")
            .await?
            .trusts_turn_metadata()
    } else {
        false
    };
    let timeout_ms = state.settings.read().await.agents.auxiliary_timeout_ms;
    let response = tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        Box::pin(responses_inner_without_media(
            state.clone(),
            headers.clone(),
            request,
            trust_turn_metadata,
        )),
    )
    .await
    .map_err(|_| {
        error_response(
            StatusCode::GATEWAY_TIMEOUT,
            "auxiliary_timeout",
            "the auxiliary media model did not respond before the configured timeout",
        )
    })?;
    if !response.status().is_success() {
        return Err(response);
    }
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .map_err(|_| error_response(StatusCode::BAD_GATEWAY, "invalid_sidecar_response", "sidecar response could not be read"))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| error_response(StatusCode::BAD_GATEWAY, "invalid_sidecar_response", "sidecar response is not valid JSON"))?;
    let result = response_output_text(&value);
    if result.trim().is_empty() {
        return Err(error_response(StatusCode::BAD_GATEWAY, "empty_sidecar_response", "sidecar completed without textual output"));
    }
    Ok(result)
}

pub(crate) async fn prepare_candidate_media_input(
    state: &GatewayState,
    headers: &HeaderMap,
    body: &mut Value,
    candidate: &RouteCandidate,
) -> Result<(), Response<Body>> {
    let settings = state.settings.read().await;
    let mode = settings.agents.image_input_mode;
    let vision_configured = settings.sidecars.vision_model.is_some();
    drop(settings);
    if mode == crate::config::AuxiliaryInputMode::Native {
        return Ok(());
    }
    let supports_vision = model_supports_vision(&candidate.provider, &candidate.upstream_model);
    if mode == crate::config::AuxiliaryInputMode::Auto && supports_vision {
        return Ok(());
    }

    let mut images = Vec::new();
    collect_input_images(body, &mut images);
    images.sort();
    images.dedup();
    if images.is_empty() {
        return Ok(());
    }
    if !vision_configured {
        return Err(media_preprocessing_error(
            StatusCode::BAD_REQUEST,
            "vision_sidecar_required",
            "the selected model cannot accept image input; configure a Vision model in CODETAS or use a vision-capable main model",
        ));
    }

    let user_context = latest_user_text(body);
    let prompt = if user_context.is_empty() {
        "Describe everything visible in this image in thorough detail. Include text, code, UI, charts, objects, layout, colors, and details needed to continue the user's task.".to_string()
    } else {
        format!(
            "Describe everything visible in this image in thorough detail, prioritizing information needed for the user's request. Include text, code, UI, charts, objects, layout, and important values.\n\nUser request:\n{}",
            user_context.chars().take(8_192).collect::<String>()
        )
    };
    let mut descriptions = HashMap::new();
    for image in &images {
        let description = analyze_image_with_sidecar(
            state,
            headers,
            image,
            &prompt,
            "vision",
            None,
            None,
            false,
        )
        .await?;
        descriptions.insert(image.clone(), description);
    }
    replace_input_images_with_text(body, &descriptions);
    Ok(())
}

fn collect_input_images(value: &Value, images: &mut Vec<String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_input_images(item, images);
            }
        }
        Value::Object(object) => {
            if matches!(object.get("type").and_then(Value::as_str), Some("input_image" | "image_url")) {
                let image = object.get("image_url");
                let url = image
                    .and_then(Value::as_str)
                    .or_else(|| image.and_then(Value::as_object).and_then(|image| image.get("url")).and_then(Value::as_str));
                if let Some(url) = url.filter(|url| !url.is_empty()) {
                    images.push(url.to_string());
                }
                return;
            }
            for child in object.values() {
                collect_input_images(child, images);
            }
        }
        _ => {}
    }
}

fn replace_input_images_with_text(value: &mut Value, descriptions: &HashMap<String, String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                replace_input_images_with_text(item, descriptions);
            }
        }
        Value::Object(object) => {
            if matches!(object.get("type").and_then(Value::as_str), Some("input_image" | "image_url")) {
                let image = object.get("image_url");
                let url = image
                    .and_then(Value::as_str)
                    .or_else(|| image.and_then(Value::as_object).and_then(|image| image.get("url")).and_then(Value::as_str));
                let description = url
                    .and_then(|url| descriptions.get(url))
                    .cloned()
                    .unwrap_or_else(|| "Image analysis was unavailable.".into());
                object.clear();
                object.insert("type".into(), Value::String("input_text".into()));
                object.insert(
                    "text".into(),
                    Value::String(format!("[The user attached an image. Vision analysis:\n{description}]")),
                );
                return;
            }
            for child in object.values_mut() {
                replace_input_images_with_text(child, descriptions);
            }
        }
        _ => {}
    }
}

fn latest_user_text(body: &Value) -> String {
    let Some(items) = body.get("input").and_then(Value::as_array) else {
        return String::new();
    };
    for item in items.iter().rev() {
        if item.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let mut text = Vec::new();
        collect_text_parts(item.get("content").unwrap_or(&Value::Null), &mut text);
        return text.join("\n");
    }
    String::new()
}

fn collect_text_parts(value: &Value, text: &mut Vec<String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_text_parts(item, text);
            }
        }
        Value::Object(object) => {
            if matches!(object.get("type").and_then(Value::as_str), Some("input_text" | "text")) {
                if let Some(value) = object.get("text").and_then(Value::as_str).filter(|value| !value.is_empty()) {
                    text.push(value.to_string());
                }
                return;
            }
            for child in object.values() {
                collect_text_parts(child, text);
            }
        }
        Value::String(value) if !value.is_empty() => text.push(value.clone()),
        _ => {}
    }
}

fn sidecar_result_response(kind: &str, result: String) -> Response<Body> {
    json_response(
        StatusCode::OK,
        json!({
            "object": "codetas.sidecar.result",
            "kind": kind,
            "result": result,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_preprocessing_error_preserves_the_observability_category() {
        let response = media_preprocessing_error(
            StatusCode::BAD_REQUEST,
            "vision_sidecar_required",
            "vision model required",
        );
        assert_eq!(
            response
                .extensions()
                .get::<MediaPreprocessingFailureCategory>()
                .map(|category| category.0),
            Some("vision_sidecar_required")
        );
    }
}
