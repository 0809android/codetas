use super::*;

pub(crate) async fn response_json(response: Response<Body>, limit: usize) -> Result<Value, String> {
    let bytes = to_bytes(response.into_body(), limit)
        .await
        .map_err(|_| "gateway response exceeded the sidecar limit".to_string())?;
    serde_json::from_slice(&bytes).map_err(|_| "gateway response was not valid JSON".to_string())
}

pub(crate) async fn realtime_call_create(
    State(state): State<GatewayState>,
    request: Request,
) -> Response<Body> {
    let headers = request.headers().clone();
    if let Err(response) = authorize_request(&state.settings, &headers, "realtime:write").await {
        return response;
    }
    let inbound_path = request.uri().path().to_string();
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let body = match to_bytes(request.into_body(), 16 * 1024 * 1024).await {
        Ok(body) => body,
        Err(_) => {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "realtime call-create request exceeds 16 MiB",
            )
        }
    };
    let body_model = match realtime_body_model(&content_type, &body) {
        Ok(model) => model,
        Err(message) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_realtime_request",
                &message,
            )
        }
    };
    let (requested_model, candidates, observability_settings) = {
        let settings = state.settings.read().await;
        let requested_model = settings
            .sidecars
            .live_model
            .clone()
            .or(body_model)
            .unwrap_or_default();
        let mut routing = state.routing.lock().await;
        (
            requested_model.clone(),
            routing.candidates(&settings, &requested_model),
            settings.observability.clone(),
        )
    };
    if requested_model.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "realtime_not_configured",
            "realtime call-create requires sidecars.liveModel or a request model",
        );
    }
    let candidates = match candidates {
        Ok(candidates) => candidates
            .into_iter()
            .filter(|candidate| candidate.capabilities.realtime)
            .collect::<Vec<_>>(),
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_request", &message)
        }
    };
    if candidates.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "capability_not_supported",
            "selected route does not support realtime call creation",
        );
    }

    let started = Instant::now();
    let request_id = Uuid::new_v4().to_string();
    let mut last_failure = None;
    for (index, candidate) in candidates.iter().enumerate() {
        let attempts = (index + 1).min(usize::from(u16::MAX)) as u16;
        let has_next = index + 1 < candidates.len();
        let candidate_started = Instant::now();
        let (candidate_body, candidate_content_type) =
            match prepare_realtime_body(&body, &content_type, candidate) {
                Ok(body) => body,
                Err(message) => {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "invalid_realtime_request",
                        &message,
                    )
                }
            };
        let upstream = match send_realtime_candidate(
            &state,
            &headers,
            candidate_body,
            &candidate_content_type,
            &inbound_path,
            candidate,
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
                        state.observability.clone(),
                        observability_settings.clone(),
                        request_id.clone(),
                        false,
                        candidate_started,
                        attempts,
                        candidate,
                    );
                    if let Some(retry) = provider_retry.as_ref() {
                        observation.record_provider_retries(retry);
                    }
                    observation.as_attempt().finish(
                        failure.response.status(),
                        Some(failure.kind.category()),
                        TokenUsage::default(),
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
        let status = upstream.status();
        if !status.is_success() {
            let retryable = status == StatusCode::REQUEST_TIMEOUT
                || status == StatusCode::TOO_MANY_REQUESTS
                || status.is_server_error();
            let retry_after = validated_retry_after(upstream.headers());
            if status == StatusCode::TOO_MANY_REQUESTS {
                state.routing.lock().await.record_quota_exhausted(
                    candidate,
                    retry_after.as_ref().and_then(|value| value.1),
                );
            } else if retryable {
                state.routing.lock().await.record_failure(candidate);
            }
            let response =
                upstream_error(upstream, retry_after.as_ref().map(|value| &value.0)).await;
            if has_next && retryable {
                let mut observation = ObservationSeed::for_candidate(
                    state.observability.clone(), observability_settings.clone(), request_id.clone(),
                    false, candidate_started, attempts, candidate,
                );
                if let Some(retry) = provider_retry.as_ref() {
                    observation.record_provider_retries(retry);
                }
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
            );
            if let Some(retry) = provider_retry.as_ref() {
                observation.record_provider_retries(retry);
            }
            observation.finish(status, Some("provider_http_error"), TokenUsage::default());
            return response;
        }
        let response_content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
        let response_location = upstream.headers().get(header::LOCATION).cloned();
        let quota = quota_usage_percent(upstream.headers());
        let payload = match bounded_response_bytes(
            upstream,
            candidate
                .provider
                .limits
                .max_response_bytes
                .min(16 * 1024 * 1024),
        )
        .await
        {
            Ok(payload) => payload,
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
        let mut observation = ObservationSeed::for_candidate(
            state.observability.clone(),
            observability_settings.clone(),
            request_id.clone(),
            false,
            started,
            attempts,
            candidate,
        );
        if let Some(retry) = provider_retry.as_ref() {
            observation.record_provider_retries(retry);
        }
        observation.finish(status, None, TokenUsage::default());
        let mut response = Response::builder().status(status);
        if let Some(value) = response_content_type {
            response = response.header(header::CONTENT_TYPE, value);
        }
        if let Some(value) = response_location {
            response = response.header(header::LOCATION, value);
        }
        return response.body(Body::from(payload)).unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "gateway_error",
                "failed to build realtime response",
            )
        });
    }
    let response = last_failure.unwrap_or_else(|| {
        error_response(
            StatusCode::BAD_GATEWAY,
            "provider_unavailable",
            "no realtime provider completed the request",
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

#[derive(Clone, Copy)]
pub(crate) enum RealtimeSidebandStyle {
    Live,
    Calls,
    Query,
}

pub(crate) async fn realtime_live_sideband(
    State(state): State<GatewayState>,
    AxumPath(call_id): AxumPath<String>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response<Body> {
    realtime_sideband_upgrade(
        state,
        headers,
        websocket,
        call_id,
        RealtimeSidebandStyle::Live,
    )
    .await
}

pub(crate) async fn realtime_calls_sideband(
    State(state): State<GatewayState>,
    AxumPath(call_id): AxumPath<String>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response<Body> {
    realtime_sideband_upgrade(
        state,
        headers,
        websocket,
        call_id,
        RealtimeSidebandStyle::Calls,
    )
    .await
}

pub(crate) async fn realtime_query_sideband(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    websocket: WebSocketUpgrade,
) -> Response<Body> {
    let call_id = query.get("call_id").cloned().unwrap_or_default();
    realtime_sideband_upgrade(
        state,
        headers,
        websocket,
        call_id,
        RealtimeSidebandStyle::Query,
    )
    .await
}

pub(crate) async fn realtime_sideband_upgrade(
    state: GatewayState,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
    call_id: String,
    style: RealtimeSidebandStyle,
) -> Response<Body> {
    if !valid_realtime_call_id(&call_id) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_call_id",
            "realtime call ID must contain 1-128 ASCII letters, digits, underscores, or hyphens",
        );
    }
    if let Err(response) = authorize_request(&state.settings, &headers, "realtime:write").await {
        return response;
    }
    let candidates = {
        let settings = state.settings.read().await;
        let Some(model) = settings.sidecars.live_model.as_deref() else {
            return error_response(
                StatusCode::BAD_REQUEST,
                "realtime_not_configured",
                "realtime sideband requires sidecars.liveModel",
            );
        };
        let mut routing = state.routing.lock().await;
        match routing.candidates(&settings, model) {
            Ok(candidates) => candidates
                .into_iter()
                .filter(|candidate| candidate.capabilities.realtime)
                .collect::<Vec<_>>(),
            Err(message) => {
                return error_response(StatusCode::BAD_REQUEST, "invalid_request", &message)
            }
        }
    };
    if candidates.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "capability_not_supported",
            "selected route does not support realtime sideband WebSockets",
        );
    }
    websocket
        .max_message_size(16 * 1024 * 1024)
        .on_upgrade(move |socket| async move {
            relay_realtime_sideband(socket, state, headers, candidates, style, call_id).await;
        })
        .into_response()
}

pub(crate) fn valid_realtime_call_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

pub(crate) async fn relay_realtime_sideband(
    mut client: WebSocket,
    state: GatewayState,
    incoming_headers: HeaderMap,
    candidates: Vec<RouteCandidate>,
    style: RealtimeSidebandStyle,
    call_id: String,
) {
    let started = Instant::now();
    let request_id = Uuid::new_v4().to_string();
    let observability_settings = state.settings.read().await.observability.clone();
    let mut last_error = "realtime sideband provider is unavailable".to_string();
    let candidate_count = candidates.len();
    for (index, candidate) in candidates.into_iter().enumerate() {
        let candidate_started = Instant::now();
        let attempts = (index + 1).min(usize::from(u16::MAX)) as u16;
        let source = candidate
            .credential
            .as_ref()
            .unwrap_or(&candidate.provider.credential)
            .source;
        let mut upstream_headers = match resolve_provider_headers(
            &candidate.provider,
            candidate.credential.as_ref(),
        )
        .await
        {
            Ok(headers) => headers,
            Err(_) => {
                last_error = "realtime provider credential is unavailable".into();
                continue;
            }
        };
        if source == CredentialSource::Forward {
            match collect_forward_headers(&state.settings, &incoming_headers).await {
                Ok(headers) => {
                    for (name, value) in &headers {
                        upstream_headers.insert(name.clone(), value.clone());
                    }
                }
                Err(_) => {
                    last_error = "caller authentication cannot be forwarded to realtime".into();
                    continue;
                }
            }
        }
        for name in [
            "openai-alpha",
            "x-session-id",
            "session-id",
            "thread-id",
            "originator",
            "x-oai-attestation",
        ] {
            if let Some(value) = incoming_headers.get(name) {
                upstream_headers.insert(header::HeaderName::from_static(name), value.clone());
            }
        }
        let url = match realtime_sideband_url(&candidate, style, &call_id) {
            Ok(url) => url,
            Err(message) => {
                last_error = message;
                continue;
            }
        };
        let upstream = match connect_pinned_websocket(&url, &candidate, upstream_headers).await {
            Ok(upstream) => upstream,
            Err(message) => {
                state.routing.lock().await.record_failure(&candidate);
                let observation = ObservationSeed::for_candidate(
                    state.observability.clone(), observability_settings.clone(), request_id.clone(),
                    true,
                    if index + 1 < candidate_count { candidate_started } else { started },
                    attempts,
                    &candidate,
                );
                if index + 1 < candidate_count {
                    observation.as_attempt().finish(
                        StatusCode::BAD_GATEWAY, Some("provider_unreachable"), TokenUsage::default(),
                    );
                } else {
                    observation.finish(
                        StatusCode::BAD_GATEWAY, Some("provider_unreachable"), TokenUsage::default(),
                    );
                }
                last_error = message;
                continue;
            }
        };
        state.routing.lock().await.record_success(&candidate, None);
        relay_websocket_frames(client, upstream).await;
        ObservationSeed::for_candidate(
            state.observability.clone(), observability_settings.clone(), request_id.clone(),
            true, started, attempts, &candidate,
        ).finish(StatusCode::OK, None, TokenUsage::default());
        return;
    }
    let _ = client.send(websocket_error_message(502, &last_error)).await;
    let _ = client.close().await;
}

pub(crate) fn realtime_sideband_url(
    candidate: &RouteCandidate,
    style: RealtimeSidebandStyle,
    call_id: &str,
) -> Result<String, String> {
    let base = candidate
        .provider
        .realtime_ws_base_url
        .as_deref()
        .unwrap_or_else(|| {
            if candidate.provider.base_url.contains("/backend-api") {
                "https://api.openai.com/v1"
            } else {
                &candidate.provider.base_url
            }
        });
    let mut url =
        url::Url::parse(base).map_err(|_| "realtime WebSocket base URL is invalid".to_string())?;
    let scheme = match url.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" if candidate.provider.allow_private_network => "ws",
        _ => return Err("realtime WebSocket base URL has an unsafe scheme".into()),
    };
    url.set_scheme(scheme)
        .map_err(|_| "realtime WebSocket scheme could not be normalized")?;
    url.set_query(None);
    url.set_fragment(None);
    let mut prefix = url.path().trim_end_matches('/').to_string();
    if let Some(index) = prefix.find("/realtime/calls/") {
        prefix.truncate(index);
    } else if let Some(index) = prefix.find("/live/") {
        prefix.truncate(index);
    } else if prefix.ends_with("/realtime") {
        prefix.truncate(prefix.len() - "/realtime".len());
    }
    if prefix.ends_with("/v1") {
        prefix.truncate(prefix.len() - 3);
    }
    let root = format!("{}/v1", prefix.trim_end_matches('/'));
    match style {
        RealtimeSidebandStyle::Live => {
            url.set_path(&format!("{root}/live/{call_id}"));
        }
        RealtimeSidebandStyle::Calls => {
            url.set_path(&format!("{root}/realtime/calls/{call_id}"));
        }
        RealtimeSidebandStyle::Query => {
            url.set_path(&format!("{root}/realtime"));
            url.query_pairs_mut()
                .append_pair("intent", "quicksilver")
                .append_pair("call_id", call_id);
        }
    }
    Ok(url.to_string())
}

pub(crate) async fn connect_pinned_websocket(
    endpoint: &str,
    candidate: &RouteCandidate,
    headers: HeaderMap,
) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>, String>
{
    let url =
        url::Url::parse(endpoint).map_err(|_| "realtime WebSocket URL is invalid".to_string())?;
    let host = url
        .host_str()
        .ok_or_else(|| "realtime WebSocket URL has no host".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "realtime WebSocket URL has no port".to_string())?;
    let timeout_duration = Duration::from_millis(candidate.provider.limits.connect_timeout_ms);
    let resolved = tokio::time::timeout(timeout_duration, lookup_host((host, port)))
        .await
        .map_err(|_| "realtime WebSocket DNS lookup timed out".to_string())?
        .map_err(|_| "realtime WebSocket host could not be resolved".to_string())?;
    let mut addresses = Vec::new();
    for address in resolved.take(16) {
        if !addresses.contains(&address) {
            addresses.push(address);
        }
    }
    if addresses.is_empty() {
        return Err("realtime WebSocket host resolved to no addresses".into());
    }
    if !candidate.provider.allow_private_network
        && addresses.iter().any(|address| is_private_ip(address.ip()))
    {
        return Err("realtime WebSocket host resolved to a private address".into());
    }
    let deadline = tokio::time::Instant::now() + timeout_duration;
    let mut stream = None;
    for address in addresses {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        if let Ok(Ok(connected)) =
            tokio::time::timeout(remaining, TcpStream::connect(address)).await
        {
            let _ = connected.set_nodelay(true);
            stream = Some(connected);
            break;
        }
    }
    let stream = stream.ok_or_else(|| "realtime WebSocket host is unreachable".to_string())?;
    let mut request = endpoint
        .into_client_request()
        .map_err(|_| "realtime WebSocket request is invalid".to_string())?;
    for (name, value) in &headers {
        request.headers_mut().insert(name.clone(), value.clone());
    }
    let config = WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .write_buffer_size(16 * 1024)
        .max_write_buffer_size(2 * 1024 * 1024)
        .max_message_size(Some(16 * 1024 * 1024))
        .max_frame_size(Some(16 * 1024 * 1024));
    let (websocket, _) = tokio::time::timeout(
        timeout_duration,
        client_async_tls_with_config(request, stream, Some(config), None),
    )
    .await
    .map_err(|_| "realtime WebSocket handshake timed out".to_string())?
    .map_err(|_| "realtime WebSocket handshake failed".to_string())?;
    Ok(websocket)
}

pub(crate) async fn relay_websocket_frames(
    client: WebSocket,
    upstream: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
) {
    let (mut client_sender, mut client_receiver) = client.split();
    let (mut upstream_sender, mut upstream_receiver) = upstream.split();
    let idle = tokio::time::sleep(Duration::from_secs(60 * 60));
    tokio::pin!(idle);
    loop {
        tokio::select! {
            _ = &mut idle => break,
            incoming = client_receiver.next() => {
                let Some(Ok(message)) = incoming else { break };
                idle.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(60 * 60));
                let forwarded = match message {
                    Message::Text(value) => TungsteniteMessage::Text(value.to_string().into()),
                    Message::Binary(value) => TungsteniteMessage::Binary(value.to_vec().into()),
                    Message::Ping(value) => TungsteniteMessage::Ping(value.to_vec().into()),
                    Message::Pong(value) => TungsteniteMessage::Pong(value.to_vec().into()),
                    Message::Close(_) => {
                        let _ = upstream_sender.send(TungsteniteMessage::Close(None)).await;
                        break;
                    }
                };
                if upstream_sender.send(forwarded).await.is_err() { break; }
            }
            incoming = upstream_receiver.next() => {
                let Some(Ok(message)) = incoming else { break };
                idle.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(60 * 60));
                let forwarded = match message {
                    TungsteniteMessage::Text(value) => Message::Text(value.to_string().into()),
                    TungsteniteMessage::Binary(value) => Message::Binary(value.to_vec().into()),
                    TungsteniteMessage::Ping(value) => Message::Ping(value.to_vec().into()),
                    TungsteniteMessage::Pong(value) => Message::Pong(value.to_vec().into()),
                    TungsteniteMessage::Close(_) => {
                        let _ = client_sender.send(Message::Close(None)).await;
                        break;
                    }
                    TungsteniteMessage::Frame(_) => continue,
                };
                if client_sender.send(forwarded).await.is_err() { break; }
            }
        }
    }
    let _ = upstream_sender.send(TungsteniteMessage::Close(None)).await;
    let _ = client_sender.send(Message::Close(None)).await;
}

pub(crate) fn realtime_body_model(
    content_type: &str,
    body: &[u8],
) -> Result<Option<String>, String> {
    let media_type = content_type.split(';').next().unwrap_or_default().trim();
    if media_type.eq_ignore_ascii_case("application/json") {
        let value: Value =
            serde_json::from_slice(body).map_err(|_| "realtime JSON body is invalid")?;
        return Ok(value
            .get("model")
            .or_else(|| value.pointer("/session/model"))
            .and_then(Value::as_str)
            .map(str::to_string));
    }
    if media_type.eq_ignore_ascii_case("multipart/form-data") {
        let boundary = multipart_boundary(content_type)?;
        if let Some((_, _, model)) = multipart_text_field(body, &boundary, "model", 240)? {
            return Ok(Some(model));
        }
        if let Some((_, _, session)) =
            multipart_text_field(body, &boundary, "session", 2 * 1024 * 1024)?
        {
            let value: Value = serde_json::from_str(&session)
                .map_err(|_| "multipart session field must contain JSON")?;
            return Ok(value
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string));
        }
    }
    Ok(None)
}

pub(crate) fn prepare_realtime_body(
    body: &[u8],
    content_type: &str,
    candidate: &RouteCandidate,
) -> Result<(Vec<u8>, String), String> {
    let backend_shape = candidate.provider.base_url.contains("/backend-api");
    let wire_model = candidate.provider.wire_model_id(&candidate.upstream_model);
    let media_type = content_type.split(';').next().unwrap_or_default().trim();
    if media_type.eq_ignore_ascii_case("application/json") {
        let mut payload: Value =
            serde_json::from_slice(body).map_err(|_| "realtime JSON body is invalid")?;
        rewrite_realtime_json_model(&mut payload, &wire_model);
        return serde_json::to_vec(&payload)
            .map(|body| (body, "application/json".into()))
            .map_err(|_| "realtime request body could not be encoded".into());
    }
    if !media_type.eq_ignore_ascii_case("multipart/form-data") {
        return Ok((body.to_vec(), content_type.to_string()));
    }
    let boundary = multipart_boundary(content_type)?;
    if !backend_shape {
        if let Some((start, end, _)) = multipart_text_field(body, &boundary, "model", 240)? {
            return Ok((
                replace_multipart_model(body, start, end, &wire_model),
                content_type.to_string(),
            ));
        }
        if let Some((start, end, session)) =
            multipart_text_field(body, &boundary, "session", 2 * 1024 * 1024)?
        {
            let mut session: Value = serde_json::from_str(&session)
                .map_err(|_| "multipart session field must contain JSON")?;
            rewrite_realtime_json_model(&mut session, &wire_model);
            let session = serde_json::to_string(&session)
                .map_err(|_| "multipart session field could not be encoded")?;
            return Ok((
                replace_multipart_model(body, start, end, &session),
                content_type.to_string(),
            ));
        }
        return Ok((body.to_vec(), content_type.to_string()));
    }
    let (_, _, sdp) = multipart_text_field(body, &boundary, "sdp", 8 * 1024 * 1024)?
        .ok_or("ChatGPT realtime backend requires multipart field sdp")?;
    let mut session = multipart_text_field(body, &boundary, "session", 2 * 1024 * 1024)?
        .map(|(_, _, session)| {
            serde_json::from_str::<Value>(&session)
                .map_err(|_| "multipart session field must contain JSON".to_string())
        })
        .transpose()?;
    if let Some(session) = session.as_mut() {
        rewrite_realtime_json_model(session, &wire_model);
    }
    let payload = match session {
        Some(session) => json!({"sdp": sdp, "session": session}),
        None => json!({"sdp": sdp}),
    };
    serde_json::to_vec(&payload)
        .map(|body| (body, "application/json".into()))
        .map_err(|_| "realtime backend body could not be encoded".into())
}

pub(crate) fn rewrite_realtime_json_model(value: &mut Value, model: &str) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if object.contains_key("model") {
        object.insert("model".into(), Value::String(model.into()));
    }
    if let Some(session) = object.get_mut("session").and_then(Value::as_object_mut) {
        session.insert("model".into(), Value::String(model.into()));
    }
}

pub(crate) async fn special_json_relay(
    state: GatewayState,
    headers: HeaderMap,
    body: Value,
    kind: SpecialRelayKind,
) -> Response<Body> {
    if let Err(response) = authorize_request(&state.settings, &headers, kind.scope()).await {
        return response;
    }
    special_json_relay_authorized(state, headers, body, kind).await
}

pub(crate) async fn special_json_relay_authorized(
    state: GatewayState,
    headers: HeaderMap,
    mut body: Value,
    kind: SpecialRelayKind,
) -> Response<Body> {
    let started = Instant::now();
    let request_id = Uuid::new_v4().to_string();
    if !body.is_object() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "relay request must be a JSON object",
        );
    }
    let requested_body_model = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string);
    let (requested_model, candidates, observability_settings) = {
        let settings = state.settings.read().await;
        let configured = match kind {
            SpecialRelayKind::ImageGeneration | SpecialRelayKind::ImageEdit => {
                settings.sidecars.image_model.clone()
            }
            SpecialRelayKind::Search => settings.sidecars.web_search_model.clone(),
            SpecialRelayKind::VideoGeneration => settings.sidecars.video_model.clone(),
        };
        let requested_model = configured.or(requested_body_model).unwrap_or_default();
        let mut routing = state.routing.lock().await;
        let candidates = match kind {
            SpecialRelayKind::ImageGeneration | SpecialRelayKind::ImageEdit => routing
                .candidates_for_image_generation(
                    &settings,
                    settings.sidecars.image_model.as_deref(),
                    body.get("model").and_then(Value::as_str),
                ),
            _ => routing.candidates(&settings, &requested_model),
        };
        (
            requested_model.clone(),
            candidates,
            settings.observability.clone(),
        )
    };
    if requested_model.is_empty() {
        let code = if matches!(
            kind,
            SpecialRelayKind::ImageGeneration | SpecialRelayKind::ImageEdit
        ) {
            "image_generation_not_configured"
        } else {
            "relay_not_configured"
        };
        let response = error_response(
            StatusCode::BAD_REQUEST,
            code,
            &format!("{} relay has no configured model", kind.label()),
        );
        ObservationSeed::without_candidate(
            state.observability.clone(),
            observability_settings,
            request_id,
            &requested_model,
            false,
            started,
        )
        .finish(response.status(), Some(code), TokenUsage::default());
        return response;
    }
    let candidates = match candidates {
        Ok(candidates) => candidates
            .into_iter()
            .filter(|candidate| kind.supports(candidate))
            .collect::<Vec<_>>(),
        Err(message) => {
            let image_kind = matches!(
                kind,
                SpecialRelayKind::ImageGeneration | SpecialRelayKind::ImageEdit
            );
            let code = if image_kind {
                "image_generation_not_configured"
            } else {
                "invalid_request"
            };
            let response = error_response(StatusCode::BAD_REQUEST, code, &message);
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
                Some(if image_kind {
                    "image_generation_not_configured"
                } else {
                    "special_route_invalid"
                }),
                TokenUsage::default(),
            );
            return response;
        }
    };
    if candidates.is_empty() {
        let image_kind = matches!(
            kind,
            SpecialRelayKind::ImageGeneration | SpecialRelayKind::ImageEdit
        );
        let code = if image_kind {
            "image_generation_not_configured"
        } else {
            "capability_not_supported"
        };
        let message = if image_kind {
            "no configured and available image-generation provider/model can serve this request"
                .to_string()
        } else {
            format!("selected route does not support {}", kind.label())
        };
        let response = error_response(
            StatusCode::BAD_REQUEST,
            code,
            &message,
        );
        ObservationSeed::without_candidate(
            state.observability.clone(),
            observability_settings,
            request_id,
            &requested_model,
            false,
            started,
        )
        .finish(response.status(), Some(code), TokenUsage::default());
        return response;
    }

    let mut last_failure = None;
    for (index, candidate) in candidates.iter().enumerate() {
        body["model"] = Value::String(candidate.provider.wire_model_id(&candidate.upstream_model));
        let attempts = (index + 1).min(usize::from(u16::MAX)) as u16;
        let has_next = index + 1 < candidates.len();
        let candidate_started = Instant::now();
        let upstream = match send_special_candidate(&state, &headers, &body, candidate, kind).await
        {
            Ok(upstream) => upstream,
            Err(failure) => {
                let provider_retry = failure.response.extensions()
                    .get::<ProviderRetryObservation>().cloned();
                if failure.kind != AttemptFailureKind::Request {
                    state.routing.lock().await.record_failure(candidate);
                }
                if has_next && failure.kind != AttemptFailureKind::Request {
                    let mut observation = special_observation_seed(
                        &state,
                        observability_settings.clone(),
                        request_id.clone(),
                        candidate_started,
                        attempts,
                        candidate,
                        kind,
                    );
                    if let Some(retry) = provider_retry.as_ref() {
                        observation.record_provider_retries(retry);
                    }
                    observation.as_attempt().finish(
                        failure.response.status(),
                        Some(failure.kind.category()),
                        TokenUsage::default(),
                    );
                    last_failure = Some(failure.response);
                    continue;
                }
                let mut observation = special_observation_seed(
                    &state,
                    observability_settings.clone(),
                    request_id.clone(),
                    started,
                    attempts,
                    candidate,
                    kind,
                );
                if let Some(retry) = provider_retry.as_ref() {
                    observation.record_provider_retries(retry);
                }
                observation.finish(
                    failure.response.status(),
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
                let mut observation = special_observation_seed(
                    &state,
                    observability_settings.clone(),
                    request_id.clone(),
                    candidate_started,
                    attempts,
                    candidate,
                    kind,
                );
                if let Some(retry) = provider_retry.as_ref() {
                    observation.record_provider_retries(retry);
                }
                observation.as_attempt().finish(
                    status,
                    Some("provider_http_error"),
                    TokenUsage::default(),
                );
                last_failure = Some(response);
                continue;
            }
            let mut observation = special_observation_seed(
                &state,
                observability_settings.clone(),
                request_id.clone(),
                started,
                attempts,
                candidate,
                kind,
            );
            if let Some(retry) = provider_retry.as_ref() {
                observation.record_provider_retries(retry);
            }
            observation.finish(
                status,
                Some("provider_http_error"),
                TokenUsage::default(),
            );
            return response;
        }
        let quota = quota_usage_percent(upstream.headers());
        let mut value =
            match bounded_json(upstream, candidate.provider.limits.max_response_bytes).await {
                Ok(value) => value,
                Err(message) => {
                    state.routing.lock().await.record_failure(candidate);
                    let response = error_response(
                        StatusCode::BAD_GATEWAY,
                        "invalid_provider_response",
                        &message,
                    );
                    if has_next {
                        let mut observation = special_observation_seed(
                            &state,
                            observability_settings.clone(),
                            request_id.clone(),
                            candidate_started,
                            attempts,
                            candidate,
                            kind,
                        );
                        if let Some(retry) = provider_retry.as_ref() {
                            observation.record_provider_retries(retry);
                        }
                        observation.as_attempt().finish(
                            StatusCode::BAD_GATEWAY,
                            Some("invalid_provider_response"),
                            TokenUsage::default(),
                        );
                        last_failure = Some(response);
                        continue;
                    }
                    let mut observation = special_observation_seed(
                        &state,
                        observability_settings.clone(),
                        request_id.clone(),
                        started,
                        attempts,
                        candidate,
                        kind,
                    );
                    if let Some(retry) = provider_retry.as_ref() {
                        observation.record_provider_retries(retry);
                    }
                    return finish_invalid_special_provider_response(observation, response);
                }
            };
        if kind == SpecialRelayKind::VideoGeneration {
            if let Some(upstream_request_id) = video_request_id(&value) {
                let mut jobs = state.video_jobs.lock().await;
                let now = Instant::now();
                jobs.retain(|_, job| {
                    now.duration_since(job.created_at) <= Duration::from_secs(24 * 60 * 60)
                });
                if jobs.len() >= 256 {
                    if let Some(oldest) = jobs
                        .iter()
                        .min_by_key(|(_, job)| job.created_at)
                        .map(|(id, _)| id.clone())
                    {
                        jobs.remove(&oldest);
                    }
                }
                let request_id = format!("video_{}", Uuid::new_v4().simple());
                jobs.insert(
                    request_id.clone(),
                    VideoJobRecord {
                        candidate: candidate.clone(),
                        upstream_request_id,
                        created_at: now,
                    },
                );
                rewrite_video_request_id(&mut value, &request_id);
            }
        }
        state.routing.lock().await.record_success(candidate, quota);
        let usage = TokenUsage::from_json(&value);
        let mut observation = special_observation_seed(
            &state,
            observability_settings,
            request_id,
            started,
            attempts,
            candidate,
            kind,
        );
        if let Some(retry) = provider_retry.as_ref() {
            observation.record_provider_retries(retry);
        }
        observation.finish(StatusCode::OK, None, usage);
        return json_response(StatusCode::OK, value);
    }
    last_failure.unwrap_or_else(|| {
        error_response(
            StatusCode::BAD_GATEWAY,
            "provider_unavailable",
            "no relay provider completed the request",
        )
    })
}

fn special_observation_seed(
    state: &GatewayState,
    settings: ObservabilitySettings,
    request_id: String,
    started: Instant,
    attempts: u16,
    candidate: &RouteCandidate,
    kind: SpecialRelayKind,
) -> ObservationSeed {
    let seed = ObservationSeed::for_candidate(
        state.observability.clone(),
        settings,
        request_id,
        false,
        started,
        attempts,
        candidate,
    );
    if matches!(
        kind,
        SpecialRelayKind::ImageGeneration | SpecialRelayKind::ImageEdit
    ) {
        let wire_model = candidate.provider.wire_model_id(&candidate.upstream_model);
        seed.with_upstream_image_details(&wire_model, &kind.endpoint(candidate))
    } else {
        seed
    }
}

fn finish_invalid_special_provider_response(
    observation: ObservationSeed,
    response: Response<Body>,
) -> Response<Body> {
    observation.finish(
        StatusCode::BAD_GATEWAY,
        Some("invalid_provider_response"),
        TokenUsage::default(),
    );
    response
}

#[cfg(test)]
mod special_relay_observability_tests {
    use super::*;
    use crate::config::{ProviderCapabilities, ProviderDefinition};

    fn image_candidate() -> RouteCandidate {
        let mut provider = ProviderDefinition {
            id: "openai-api".into(),
            name: "OpenAI API".into(),
            base_url: "https://api.openai.com/v1/private-token".into(),
            capabilities: ProviderCapabilities {
                image_generation: true,
                ..ProviderCapabilities::default()
            },
            routing_generation: 0,
            ..ProviderDefinition::default()
        };
        provider
            .model_wire_ids
            .insert("imagegen-2".into(), "gpt-image-2".into());
        RouteCandidate {
            provider,
            upstream_model: "imagegen-2".into(),
            exposed_model: "imagegen-2".into(),
            credential: None,
            account_id: None,
            target_key: "openai-api/imagegen-2".into(),
            route_id: None,
            failure_threshold: 3,
            quota_threshold_percent: 90,
            input_price_per_million: None,
            output_price_per_million: None,
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            capabilities: ProviderCapabilities {
                image_generation: true,
                ..ProviderCapabilities::default()
            },
            routing_epoch: 0,
            routing_generation: 0,
        }
    }

    #[test]
    fn invalid_image_json_records_failure_for_generation_and_edit_resources() {
        for kind in [
            SpecialRelayKind::ImageGeneration,
            SpecialRelayKind::ImageEdit,
        ] {
            let ledger = ObservabilityLedger::new(None);
            let mut settings = ObservabilitySettings::default();
            settings.request_log = true;
            let candidate = image_candidate();
            let wire_model = candidate.provider.wire_model_id(&candidate.upstream_model);
            let observation = ObservationSeed::for_candidate(
                ledger.clone(),
                settings,
                if kind == SpecialRelayKind::ImageGeneration {
                    "request-generation".into()
                } else {
                    "request-edit".into()
                },
                false,
                Instant::now(),
                1,
                &candidate,
            )
            .with_upstream_image_details(&wire_model, &kind.endpoint(&candidate));
            let response = error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                "provider response is invalid JSON",
            );

            let response = finish_invalid_special_provider_response(observation, response);
            let summary = ledger.summary();

            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            assert_eq!(summary.total_requests, 1);
            assert_eq!(summary.failed_requests, 1);
        }
    }
}
