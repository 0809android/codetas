use crate::{
    anthropic::{
        anthropic_stream_to_chat, anthropic_subscription_oauth_headers, anthropic_to_response,
        responses_to_anthropic_with_oauth, uses_anthropic_subscription_oauth,
    },
    auth::{apply_provider_auth, resolve_provider_headers},
    catalog::build_codex_catalog,
    client_anthropic::{
        anthropic_request_to_responses, responses_to_anthropic_response, ResponsesToAnthropicStream,
    },
    client_chat::{chat_request_to_responses, responses_to_chat_response, ResponsesToChatStream},
    client_gemini::{gemini_request_to_responses, responses_to_gemini_response},
    compaction::{
        compaction_item_count, encode_summary, expand_local_compactions,
        request_is_remote_compaction, response_output_text,
    },
    compat::{
        ensure_chat_function_parameters, escape_anthropic_tool_names,
        guard_repeated_function_tool_loop, is_codetas_repaired_item_id, is_kimi_chat_endpoint,
        is_xai_chat_endpoint, is_zen_chat_endpoint,
        restore_anthropic_stream_tool_names, restore_anthropic_tool_names,
        sanitize_kimi_chat_tools, sanitize_responses_upstream_request, sanitize_xai_chat_tools,
        sanitize_zen_chat_tools, ResponsesItemIdRepair,
    },
    config::{
        is_private_ip, CredentialSource, GatewaySettings, GoogleMode, ObservabilitySettings,
        ProviderProtocol, ProviderTransport,
    },
    copilot::exchange_copilot_token,
    gemini::{gemini_stream_to_chat, gemini_to_response, responses_to_gemini},
    kiro::{
        kiro_eventstream_to_response, responses_to_kiro, KiroRequestContext, KiroStreamDecoder,
    },
    network::pinned_client,
    observability::{ObservabilityLedger, ObservabilitySummary, ObservationEvent, TokenUsage},
    response_state::ResponseStateStore,
    routing::{RouteCandidate, RoutingRuntime},
    translate::{
        chat_to_response, custom_tool_names, normalize_chat_reasoning_history,
        prepare_translated_responses_request, responses_to_chat, sse,
        strip_translated_input_images, ChatStreamState,
    },
};
use async_stream::stream;
use axum::{
    body::{to_bytes, Body},
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, Path as AxumPath, Query, Request, State,
    },
    http::{header, HeaderMap, HeaderValue, Method, Response, StatusCode},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use fs2::FileExt;
use futures_util::{SinkExt, StreamExt};
use reqwest::redirect::Policy;
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeSet, HashMap},
    convert::Infallible,
    fs::{self, File, OpenOptions},
    io::Write,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use thiserror::Error;
use tokio::{
    net::{lookup_host, TcpListener, TcpStream},
    sync::{mpsc, oneshot, Mutex, RwLock},
    task::JoinHandle,
};
use tokio_tungstenite::{
    client_async_tls_with_config,
    tungstenite::{
        client::IntoClientRequest,
        protocol::{Message as TungsteniteMessage, WebSocketConfig},
    },
};
use tower_http::decompression::RequestDecompressionLayer;
use uuid::Uuid;

pub type SharedSettings = Arc<RwLock<GatewaySettings>>;

#[derive(Clone)]
struct GatewayState {
    settings: SharedSettings,
    client: reqwest::Client,
    routing: Arc<Mutex<RoutingRuntime>>,
    observability: ObservabilityLedger,
    video_jobs: Arc<Mutex<HashMap<String, VideoJobRecord>>>,
    instance_id: String,
    response_state: Arc<ResponseStateStore>,
}

#[derive(Clone)]
struct VideoJobRecord {
    candidate: RouteCandidate,
    upstream_request_id: String,
    created_at: Instant,
}

pub struct GatewayHandle {
    address: SocketAddr,
    settings: SharedSettings,
    observability: ObservabilityLedger,
    routing: Arc<Mutex<RoutingRuntime>>,
    instance_id: String,
    runtime_state_path: Option<PathBuf>,
    _runtime_lock: Option<File>,
    shutdown_sender: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), String>>,
}

impl GatewayHandle {
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn url(&self) -> String {
        let address = if self.address.ip().is_unspecified() {
            SocketAddr::new(
                if self.address.is_ipv4() {
                    IpAddr::V4(Ipv4Addr::LOCALHOST)
                } else {
                    IpAddr::V6(Ipv6Addr::LOCALHOST)
                },
                self.address.port(),
            )
        } else {
            self.address
        };
        format!("http://{address}/v1")
    }

    pub async fn set_settings(&self, settings: GatewaySettings) -> Result<(), String> {
        settings.validate()?;
        *self.settings.write().await = settings;
        *self.routing.lock().await = RoutingRuntime::default();
        Ok(())
    }

    pub async fn settings(&self) -> GatewaySettings {
        self.settings.read().await.clone()
    }

    pub fn observability_summary(&self) -> ObservabilitySummary {
        self.observability.summary()
    }

    pub async fn flush_observability(&self) {
        self.observability.flush().await;
    }

    pub async fn shutdown(mut self) {
        let timeout_ms = self.settings.read().await.runtime.shutdown_timeout_ms;
        let timeout = Duration::from_millis(timeout_ms);
        // Reserve part of the one shutdown budget for the usage checkpoint. Otherwise a
        // slow HTTP drain can consume the entire allowance and leave completed request
        // records queued in spawn_blocking with no opportunity to flush.
        let flush_reserve = (timeout / 4)
            .max(Duration::from_millis(50))
            .min(timeout / 2);
        let server_budget = timeout.saturating_sub(flush_reserve);
        let started = Instant::now();
        if let Some(sender) = self.shutdown_sender.take() {
            let _ = sender.send(());
        }
        if tokio::time::timeout(server_budget, &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
            let _ = (&mut self.task).await;
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        let _ = tokio::time::timeout(remaining, self.observability.flush()).await;
    }

    pub async fn wait_for_exit(&mut self) -> Result<(), String> {
        (&mut self.task)
            .await
            .map_err(|error| format!("gateway task failed: {error}"))?
    }
}

impl Drop for GatewayHandle {
    fn drop(&mut self) {
        if let Some(sender) = self.shutdown_sender.take() {
            let _ = sender.send(());
        }
        self.task.abort();
        if let Some(path) = self.runtime_state_path.as_deref() {
            remove_runtime_state_if_owned(path, &self.instance_id);
        }
    }
}

#[derive(Debug, Error)]
pub enum GatewayStartError {
    #[error("invalid gateway settings: {0}")]
    InvalidSettings(String),
    #[error("failed to bind local gateway: {0}")]
    Bind(#[from] std::io::Error),
    #[error("failed to create HTTP client: {0}")]
    Client(#[from] reqwest::Error),
    #[error("another CODETAS gateway owns this runtime directory: {0}")]
    AlreadyRunning(String),
    #[error("failed to publish gateway runtime state: {0}")]
    RuntimeState(String),
}

#[derive(Clone, Debug, Default)]
pub struct GatewayRuntimeOptions {
    pub observability_directory: Option<PathBuf>,
    pub runtime_state_path: Option<PathBuf>,
    pub auth_store_path: Option<PathBuf>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GatewayRuntimeState<'a> {
    version: u8,
    instance_id: &'a str,
    pid: u32,
    configured_port: u16,
    actual_port: u16,
    url: String,
    started_at_unix_ms: u64,
}

pub async fn start_gateway(
    port: u16,
    settings: GatewaySettings,
) -> Result<GatewayHandle, GatewayStartError> {
    start_gateway_with_options(port, settings, GatewayRuntimeOptions::default()).await
}

pub async fn start_gateway_with_options(
    port: u16,
    settings: GatewaySettings,
    options: GatewayRuntimeOptions,
) -> Result<GatewayHandle, GatewayStartError> {
    settings
        .validate()
        .map_err(GatewayStartError::InvalidSettings)?;
    crate::oauth::configure_auth_store_path(options.auth_store_path.clone());
    let bind_ip = if settings.runtime.host == "localhost" {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        settings.runtime.host.parse::<IpAddr>().map_err(|_| {
            GatewayStartError::InvalidSettings(
                "runtime host must be localhost or an IP address".into(),
            )
        })?
    };
    let dynamic_port_fallback = settings.runtime.dynamic_port_fallback;
    let runtime_lock = options
        .runtime_state_path
        .as_deref()
        .map(acquire_runtime_lock)
        .transpose()?;
    let shared = Arc::new(RwLock::new(settings));
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .user_agent("CODETAS-Gateway/0.1")
        .build()?;
    let observability = ObservabilityLedger::new(options.observability_directory);
    let instance_id = Uuid::new_v4().to_string();
    let routing = Arc::new(Mutex::new(RoutingRuntime::default()));
    let state = GatewayState {
        settings: Arc::clone(&shared),
        client,
        routing: Arc::clone(&routing),
        observability: observability.clone(),
        video_jobs: Arc::new(Mutex::new(HashMap::new())),
        instance_id: instance_id.clone(),
        response_state: Arc::new(ResponseStateStore::default()),
    };
    let router = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(readiness))
        .route("/v1/models", get(models))
        .route("/v1beta/models", get(gemini_models))
        .route("/v1/responses", post(responses).get(responses_websocket))
        .route("/v1/responses/compact", post(compact_response))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/messages/count_tokens", post(anthropic_count_tokens))
        .route("/v1beta/models/{*operation}", post(gemini_wildcard_request))
        .route("/v1/models/{*operation}", post(gemini_wildcard_request))
        .route("/v1/images/generations", post(image_generations))
        .route("/v1/images/edits", post(image_edits))
        .route("/v1/alpha/search", post(search_relay))
        .route("/v1/videos/generations", post(video_generations))
        .route("/v1/videos/{request_id}", get(video_status))
        .route("/v1/sidecars/web-search", post(web_search_sidecar))
        .route("/v1/sidecars/vision", post(vision_sidecar))
        .route("/v1/sidecars/image", post(image_sidecar))
        .route("/v1/sidecars/video", post(video_sidecar))
        .route("/v1/sidecars/video/{request_id}", get(video_sidecar_status))
        .route("/v1/live", post(realtime_call_create))
        .route("/v1/realtime/calls", post(realtime_call_create))
        .route("/v1/live/{call_id}", get(realtime_live_sideband))
        .route("/v1/realtime/calls/{call_id}", get(realtime_calls_sideband))
        .route("/v1/realtime", get(realtime_query_sideband))
        .with_state(state)
        // Supported inline image payloads can exceed Axum's 2 MiB default after base64 and JSON
        // framing. Keep one hard process-wide ceiling above that representation; provider limits
        // and endpoint validators still apply tighter bounds after extraction.
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        // Codex enables zstd request compression by default. Decode supported HTTP encodings
        // before JSON extraction; the body limit above still applies to decompressed bytes.
        .layer(RequestDecompressionLayer::new())
        .layer(middleware::from_fn_with_state(
            Arc::clone(&shared),
            cors_middleware,
        ));
    let listener = match TcpListener::bind(SocketAddr::new(bind_ip, port)).await {
        Ok(listener) => listener,
        Err(error)
            if error.kind() == std::io::ErrorKind::AddrInUse
                && dynamic_port_fallback
                && bind_ip.is_loopback() =>
        {
            TcpListener::bind(SocketAddr::new(bind_ip, 0)).await?
        }
        Err(error) => return Err(GatewayStartError::Bind(error)),
    };
    let address = listener.local_addr()?;
    if let Some(path) = options.runtime_state_path.as_deref() {
        publish_runtime_state(path, &instance_id, port, address)
            .map_err(GatewayStartError::RuntimeState)?;
    }
    let (shutdown_sender, shutdown_receiver) = oneshot::channel();
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = shutdown_receiver.await;
            })
            .await
            .map_err(|error| error.to_string())
    });
    Ok(GatewayHandle {
        address,
        settings: shared,
        observability,
        routing,
        instance_id,
        runtime_state_path: options.runtime_state_path,
        _runtime_lock: runtime_lock,
        shutdown_sender: Some(shutdown_sender),
        task,
    })
}

fn runtime_parent(path: &std::path::Path) -> &std::path::Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."))
}

fn acquire_runtime_lock(state_path: &std::path::Path) -> Result<File, GatewayStartError> {
    let parent = runtime_parent(state_path);
    fs::create_dir_all(parent)
        .map_err(|error| GatewayStartError::RuntimeState(error.to_string()))?;
    let lock_path = state_path.with_extension("lock");
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options
        .open(&lock_path)
        .map_err(|error| GatewayStartError::RuntimeState(error.to_string()))?;
    lock.try_lock_exclusive().map_err(|error| {
        GatewayStartError::AlreadyRunning(format!("{} ({error})", state_path.display()))
    })?;
    Ok(lock)
}

fn publish_runtime_state(
    path: &std::path::Path,
    instance_id: &str,
    configured_port: u16,
    address: SocketAddr,
) -> Result<(), String> {
    let public_address = if address.ip().is_unspecified() {
        SocketAddr::new(
            if address.is_ipv4() {
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            } else {
                IpAddr::V6(Ipv6Addr::LOCALHOST)
            },
            address.port(),
        )
    } else {
        address
    };
    let state = GatewayRuntimeState {
        version: 1,
        instance_id,
        pid: std::process::id(),
        configured_port,
        actual_port: address.port(),
        url: format!("http://{public_address}/v1"),
        started_at_unix_ms: SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0),
    };
    let content = serde_json::to_vec_pretty(&state)
        .map_err(|error| format!("cannot encode runtime state: {error}"))?;
    let parent = runtime_parent(path);
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create runtime state directory: {error}"))?;
    let nonce = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temporary = parent.join(format!(
        ".codetas-runtime-{}-{nonce}.tmp",
        std::process::id()
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("cannot create runtime state: {error}"))?;
    file.write_all(&content)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("cannot persist runtime state: {error}"))?;
    if let Err(first_error) = fs::rename(&temporary, path) {
        if path.exists() {
            fs::remove_file(path)
                .map_err(|error| format!("cannot replace runtime state: {error}"))?;
            fs::rename(&temporary, path)
                .map_err(|error| format!("cannot replace runtime state: {error}"))?;
        } else {
            let _ = fs::remove_file(&temporary);
            return Err(format!("cannot publish runtime state: {first_error}"));
        }
    }
    Ok(())
}

fn remove_runtime_state_if_owned(path: &std::path::Path, instance_id: &str) {
    let owned = fs::metadata(path)
        .ok()
        .filter(|metadata| metadata.len() <= 64 * 1024)
        .and_then(|_| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| {
            value
                .get("instanceId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .is_some_and(|value| value == instance_id);
    if owned {
        let _ = fs::remove_file(path);
    }
}

async fn cors_middleware(
    State(settings): State<SharedSettings>,
    request: Request,
    next: Next,
) -> Response<Body> {
    let origin = request.headers().get(header::ORIGIN).cloned();
    let Some(origin) = origin else {
        return next.run(request).await;
    };
    let allowed = {
        let settings = settings.read().await;
        origin.to_str().ok().is_some_and(|origin| {
            settings
                .security
                .cors_allow_origins
                .iter()
                .any(|allowed| allowed == origin)
        })
    };
    if !allowed {
        return error_response(
            StatusCode::FORBIDDEN,
            "cors_origin_denied",
            "request origin is not allowed by CODETAS",
        );
    }

    let is_preflight = request.method() == Method::OPTIONS;
    if is_preflight {
        let requested_method = request
            .headers()
            .get(header::ACCESS_CONTROL_REQUEST_METHOD)
            .and_then(|value| value.to_str().ok());
        if !matches!(requested_method, Some("GET" | "POST")) {
            return error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "cors_method_denied",
                "CORS preflight requested an unsupported method",
            );
        }
    }

    let mut response = if is_preflight {
        Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .unwrap_or_else(|_| Response::new(Body::empty()))
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    headers.insert(header::VARY, HeaderValue::from_static("Origin"));
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(
            "authorization, content-type, x-api-key, x-goog-api-key, x-codetas-token, x-codetas-client, x-codex-turn-metadata, anthropic-version, anthropic-beta, openai-alpha, x-session-id, session-id, thread-id, originator, x-oai-attestation",
        ),
    );
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("600"),
    );
    response
}

async fn anthropic_count_tokens(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    let _admission = match authorize_request(&state.settings, &headers, "tokens:count").await {
        Ok(admission) => admission,
        Err(response) => return response,
    };
    if let Err(message) = anthropic_request_to_responses(&body) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "unsupported_anthropic_request",
            &message,
        );
    }
    let bytes = serde_json::to_vec(&body)
        .map(|bytes| bytes.len())
        .unwrap_or(0);
    let estimated = ((bytes.saturating_add(3)) / 4).max(1);
    json_response(
        StatusCode::OK,
        json!({"input_tokens": estimated.min(u32::MAX as usize)}),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SpecialRelayKind {
    ImageGeneration,
    ImageEdit,
    Search,
    VideoGeneration,
}

impl SpecialRelayKind {
    fn scope(self) -> &'static str {
        match self {
            Self::ImageGeneration | Self::ImageEdit => "images:write",
            Self::Search => "search:write",
            Self::VideoGeneration => "videos:write",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::ImageGeneration => "image generation",
            Self::ImageEdit => "image edit",
            Self::Search => "search",
            Self::VideoGeneration => "video generation",
        }
    }

    fn supports(self, candidate: &RouteCandidate) -> bool {
        match self {
            Self::ImageGeneration | Self::ImageEdit => {
                candidate.provider.capabilities.image_generation
            }
            Self::Search => candidate.provider.capabilities.web_search,
            Self::VideoGeneration => candidate.provider.capabilities.video_generation,
        }
    }

    fn endpoint(self, candidate: &RouteCandidate) -> String {
        match self {
            Self::ImageGeneration => candidate.provider.image_endpoint(false),
            Self::ImageEdit => candidate.provider.image_endpoint(true),
            Self::Search => candidate.provider.search_endpoint(),
            Self::VideoGeneration => candidate.provider.video_endpoint(),
        }
    }
}

async fn image_generations(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    special_json_relay(state, headers, body, SpecialRelayKind::ImageGeneration).await
}

async fn image_sidecar(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    if let Err(response) = authorize_request(&state.settings, &headers, "sidecars:write").await {
        return response;
    }
    special_json_relay_authorized(state, headers, body, SpecialRelayKind::ImageGeneration).await
}

async fn image_edits(State(state): State<GatewayState>, request: Request) -> Response<Body> {
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

async fn special_multipart_image_edit(
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
        let mut routing = state.routing.lock().await;
        (
            requested_model.clone(),
            routing.candidates(&settings, &requested_model),
            settings.observability.clone(),
        )
    };
    let candidates = match candidates {
        Ok(candidates) => candidates
            .into_iter()
            .filter(|candidate| candidate.provider.capabilities.image_generation)
            .collect::<Vec<_>>(),
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_request", &message)
        }
    };
    if candidates.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "capability_not_supported",
            "selected route does not support image editing",
        );
    }

    let started = Instant::now();
    let request_id = Uuid::new_v4().to_string();
    let mut last_failure = None;
    for (index, candidate) in candidates.iter().enumerate() {
        let attempts = (index + 1).min(usize::from(u16::MAX)) as u16;
        let has_next = index + 1 < candidates.len();
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
                if failure.kind != AttemptFailureKind::Request {
                    state.routing.lock().await.record_failure(candidate);
                }
                if has_next && failure.kind != AttemptFailureKind::Request {
                    last_failure = Some(failure.response);
                    continue;
                }
                let status = failure.response.status();
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
                    status,
                    Some(failure.kind.category()),
                    TokenUsage::default(),
                );
                return failure.response;
            }
        };
        if !upstream.status().is_success() {
            let status = upstream.status();
            let transient = status == StatusCode::REQUEST_TIMEOUT
                || status == StatusCode::TOO_MANY_REQUESTS
                || status.is_server_error();
            let retry_after = validated_retry_after(upstream.headers());
            let response =
                upstream_error(upstream, retry_after.as_ref().map(|value| &value.0)).await;
            if status == StatusCode::TOO_MANY_REQUESTS {
                state
                    .routing
                    .lock()
                    .await
                    .record_quota_exhausted(candidate, retry_after.and_then(|value| value.1));
            } else if transient {
                state.routing.lock().await.record_failure(candidate);
            }
            if transient && has_next {
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
                .finish(
                    StatusCode::BAD_GATEWAY,
                    Some("invalid_provider_response"),
                    TokenUsage::default(),
                );
                return response;
            }
        };
        state.routing.lock().await.record_success(candidate, quota);
        let usage = TokenUsage::from_json(&value);
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

fn multipart_boundary(content_type: &str) -> Result<String, String> {
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

fn multipart_model_field(body: &[u8], boundary: &str) -> Result<(usize, usize, String), String> {
    multipart_text_field(body, boundary, "model", 240)?
        .ok_or_else(|| "multipart image edit requires a model field".into())
}

fn multipart_text_field(
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

fn multipart_part_name(headers: &str) -> Option<String> {
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

fn find_bytes(haystack: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    if needle.is_empty() || start > haystack.len() {
        return None;
    }
    haystack[start..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| start + offset)
}

fn replace_multipart_model(body: &[u8], start: usize, end: usize, model: &str) -> Vec<u8> {
    let mut output = Vec::with_capacity(body.len() - (end - start) + model.len());
    output.extend_from_slice(&body[..start]);
    output.extend_from_slice(model.as_bytes());
    output.extend_from_slice(&body[end..]);
    output
}

async fn search_relay(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    special_json_relay(state, headers, body, SpecialRelayKind::Search).await
}

async fn video_generations(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    special_json_relay(state, headers, body, SpecialRelayKind::VideoGeneration).await
}

async fn video_sidecar(
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

async fn video_sidecar_status(
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

fn video_request_id(value: &Value) -> Option<String> {
    value
        .get("request_id")
        .or_else(|| value.get("id"))
        .and_then(Value::as_str)
        .filter(|value| valid_video_request_id(value))
        .map(str::to_string)
}

async fn video_status(
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

async fn video_status_inner(
    state: &GatewayState,
    headers: &HeaderMap,
    request_id: &str,
) -> Result<Value, AttemptFailure> {
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
    let upstream =
        send_video_status_candidate(state, headers, &candidate, &job.upstream_request_id).await?;
    if !upstream.status().is_success() {
        let status = upstream.status();
        return Err(AttemptFailure {
            response: upstream_error(upstream, None).await,
            kind: if status == StatusCode::REQUEST_TIMEOUT
                || status == StatusCode::TOO_MANY_REQUESTS
                || status.is_server_error()
            {
                AttemptFailureKind::Retryable
            } else {
                AttemptFailureKind::Request
            },
        });
    }
    let value = bounded_json(upstream, candidate.provider.limits.max_response_bytes)
        .await
        .map_err(|message| request_failure("invalid_provider_response", &message))?;
    let mut value = normalize_video_status(value, &candidate.exposed_model);
    rewrite_video_request_id(&mut value, request_id);
    Ok(value)
}

fn valid_video_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 240
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn normalize_video_status(mut value: Value, model: &str) -> Value {
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

fn rewrite_video_request_id(value: &mut Value, request_id: &str) {
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

async fn response_json(response: Response<Body>, limit: usize) -> Result<Value, String> {
    let bytes = to_bytes(response.into_body(), limit)
        .await
        .map_err(|_| "gateway response exceeded the sidecar limit".to_string())?;
    serde_json::from_slice(&bytes).map_err(|_| "gateway response was not valid JSON".to_string())
}

async fn realtime_call_create(
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
            .filter(|candidate| candidate.provider.capabilities.realtime)
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
                if failure.kind != AttemptFailureKind::Request {
                    state.routing.lock().await.record_failure(candidate);
                }
                if has_next && failure.kind != AttemptFailureKind::Request {
                    last_failure = Some(failure.response);
                    continue;
                }
                let status = failure.response.status();
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
                    status,
                    Some(failure.kind.category()),
                    TokenUsage::default(),
                );
                return failure.response;
            }
        };
        let status = upstream.status();
        if !status.is_success() {
            let retryable = status == StatusCode::REQUEST_TIMEOUT
                || status == StatusCode::TOO_MANY_REQUESTS
                || status.is_server_error();
            let retry_after = validated_retry_after(upstream.headers());
            let response =
                upstream_error(upstream, retry_after.as_ref().map(|value| &value.0)).await;
            if status == StatusCode::TOO_MANY_REQUESTS {
                state
                    .routing
                    .lock()
                    .await
                    .record_quota_exhausted(candidate, retry_after.and_then(|value| value.1));
            } else if retryable {
                state.routing.lock().await.record_failure(candidate);
            }
            if has_next && retryable {
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
                .finish(
                    StatusCode::BAD_GATEWAY,
                    Some("invalid_provider_response"),
                    TokenUsage::default(),
                );
                return response;
            }
        };
        state.routing.lock().await.record_success(candidate, quota);
        ObservationSeed::for_candidate(
            state.observability.clone(),
            observability_settings.clone(),
            request_id.clone(),
            false,
            started,
            attempts,
            candidate,
        )
        .finish(status, None, TokenUsage::default());
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
enum RealtimeSidebandStyle {
    Live,
    Calls,
    Query,
}

async fn realtime_live_sideband(
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

async fn realtime_calls_sideband(
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

async fn realtime_query_sideband(
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

async fn realtime_sideband_upgrade(
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
                .filter(|candidate| candidate.provider.capabilities.realtime)
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

fn valid_realtime_call_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

async fn relay_realtime_sideband(
    mut client: WebSocket,
    state: GatewayState,
    incoming_headers: HeaderMap,
    candidates: Vec<RouteCandidate>,
    style: RealtimeSidebandStyle,
    call_id: String,
) {
    let mut last_error = "realtime sideband provider is unavailable".to_string();
    for candidate in candidates {
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
                last_error = message;
                continue;
            }
        };
        state.routing.lock().await.record_success(&candidate, None);
        relay_websocket_frames(client, upstream).await;
        return;
    }
    let _ = client.send(websocket_error_message(502, &last_error)).await;
    let _ = client.close().await;
}

fn realtime_sideband_url(
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

async fn connect_pinned_websocket(
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

async fn relay_websocket_frames(
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

fn realtime_body_model(content_type: &str, body: &[u8]) -> Result<Option<String>, String> {
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

fn prepare_realtime_body(
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

fn rewrite_realtime_json_model(value: &mut Value, model: &str) {
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

async fn special_json_relay(
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

async fn special_json_relay_authorized(
    state: GatewayState,
    headers: HeaderMap,
    mut body: Value,
    kind: SpecialRelayKind,
) -> Response<Body> {
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
        (
            requested_model.clone(),
            routing.candidates(&settings, &requested_model),
            settings.observability.clone(),
        )
    };
    if requested_model.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "relay_not_configured",
            &format!("{} relay has no configured model", kind.label()),
        );
    }
    let candidates = match candidates {
        Ok(candidates) => candidates
            .into_iter()
            .filter(|candidate| kind.supports(candidate))
            .collect::<Vec<_>>(),
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_request", &message)
        }
    };
    if candidates.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "capability_not_supported",
            &format!("selected route does not support {}", kind.label()),
        );
    }

    let started = Instant::now();
    let request_id = Uuid::new_v4().to_string();
    let mut last_failure = None;
    for (index, candidate) in candidates.iter().enumerate() {
        body["model"] = Value::String(candidate.provider.wire_model_id(&candidate.upstream_model));
        let attempts = (index + 1).min(usize::from(u16::MAX)) as u16;
        let has_next = index + 1 < candidates.len();
        let upstream = match send_special_candidate(&state, &headers, &body, candidate, kind).await
        {
            Ok(upstream) => upstream,
            Err(failure) => {
                if failure.kind != AttemptFailureKind::Request {
                    state.routing.lock().await.record_failure(candidate);
                }
                if has_next && failure.kind != AttemptFailureKind::Request {
                    last_failure = Some(failure.response);
                    continue;
                }
                return failure.response;
            }
        };
        if !upstream.status().is_success() {
            let status = upstream.status();
            let transient = status == StatusCode::REQUEST_TIMEOUT
                || status == StatusCode::TOO_MANY_REQUESTS
                || status.is_server_error();
            let retry_after = validated_retry_after(upstream.headers());
            let response =
                upstream_error(upstream, retry_after.as_ref().map(|value| &value.0)).await;
            if status == StatusCode::TOO_MANY_REQUESTS {
                state
                    .routing
                    .lock()
                    .await
                    .record_quota_exhausted(candidate, retry_after.and_then(|value| value.1));
            } else if transient {
                state.routing.lock().await.record_failure(candidate);
            }
            if transient && has_next {
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
        let quota = quota_usage_percent(upstream.headers());
        let mut value =
            match bounded_json(upstream, candidate.provider.limits.max_response_bytes).await {
                Ok(value) => value,
                Err(message) => {
                    if has_next {
                        state.routing.lock().await.record_failure(candidate);
                        last_failure = Some(error_response(
                            StatusCode::BAD_GATEWAY,
                            "invalid_provider_response",
                            &message,
                        ));
                        continue;
                    }
                    return error_response(
                        StatusCode::BAD_GATEWAY,
                        "invalid_provider_response",
                        &message,
                    );
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
        ObservationSeed::for_candidate(
            state.observability.clone(),
            observability_settings,
            request_id,
            false,
            started,
            attempts,
            candidate,
        )
        .finish(StatusCode::OK, None, usage);
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

async fn web_search_sidecar(
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

async fn vision_sidecar(
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

async fn sidecar_model(
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

async fn run_text_sidecar(
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

fn validate_sidecar_image_url(value: &str) -> Result<(), String> {
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

async fn anthropic_messages(
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

async fn gemini_wildcard_request(
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

async fn gemini_client_request(
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

fn responses_stream_to_anthropic(response: Response<Body>, model: String) -> Response<Body> {
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

async fn chat_completions(
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

fn responses_stream_to_chat(response: Response<Body>, model: String) -> Response<Body> {
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

async fn health(State(state): State<GatewayState>, headers: HeaderMap) -> Response<Body> {
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

async fn readiness(State(state): State<GatewayState>, headers: HeaderMap) -> Response<Body> {
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

async fn models(
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

async fn gemini_models(State(state): State<GatewayState>, headers: HeaderMap) -> Response<Body> {
    if let Err(response) = authorize_request(&state.settings, &headers, "models:read").await {
        return response;
    }
    let settings = state.settings.read().await;
    gemini_models_response(&settings)
}

fn gemini_models_response(settings: &GatewaySettings) -> Response<Body> {
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

fn authoritative_model_context(settings: &GatewaySettings, route: &str) -> Option<u64> {
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

fn authoritative_model_output(settings: &GatewaySettings, route: &str) -> Option<u64> {
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

async fn compact_response(
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
        if let Err(message) = enforce_candidate_input_budget(&body, candidate, 0) {
            if has_next {
                last_failure = Some(error_response(
                    StatusCode::BAD_REQUEST,
                    "context_limit_exceeded",
                    &message,
                ));
                continue;
            }
            return error_response(StatusCode::BAD_REQUEST, "context_limit_exceeded", &message);
        }
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
                        if failure.kind != AttemptFailureKind::Request {
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
                send_candidate(&state, &body, candidate, Some(&headers)).await
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
            let response =
                upstream_error(upstream, retry_after.as_ref().map(|value| &value.0)).await;
            if status == StatusCode::TOO_MANY_REQUESTS {
                state
                    .routing
                    .lock()
                    .await
                    .record_quota_exhausted(candidate, retry_after.and_then(|value| value.1));
            } else if retryable {
                state.routing.lock().await.record_failure(candidate);
            }
            if has_next && retryable {
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
        let value = match bounded_json(upstream, limit).await {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompactionMode {
    Local,
    Responses,
    CompactEndpoint,
}

fn candidate_compaction_mode(candidate: &RouteCandidate) -> CompactionMode {
    let credential_source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let is_openai = matches!(
        candidate.provider.id.as_str(),
        "openai" | "openai-api" | "openai-apikey"
    );
    let is_responses = candidate.provider.transport == ProviderTransport::Standard
        && candidate
            .provider
            .protocol_for_model(&candidate.upstream_model)
            == ProviderProtocol::Responses
        && candidate.provider.azure_deployment.is_none();
    if !is_openai || !is_responses {
        return CompactionMode::Local;
    }
    if credential_source == CredentialSource::Forward {
        // Codex subscription traffic performs remote compaction by sending the
        // compaction trigger to the normal ChatGPT Codex Responses endpoint.
        CompactionMode::Responses
    } else {
        // Public OpenAI API credentials use the dedicated compact endpoint.
        CompactionMode::CompactEndpoint
    }
}

async fn responses_websocket(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response<Body> {
    let admission = match authorize_request(&state.settings, &headers, "responses:write").await {
        Ok(admission) => admission,
        Err(response) => return response,
    };
    websocket
        .max_message_size(16 * 1024 * 1024)
        .on_upgrade(move |socket| async move {
            responses_websocket_session(socket, state, headers, admission.trusts_turn_metadata())
                .await;
        })
        .into_response()
}

async fn responses_websocket_session(
    socket: WebSocket,
    state: GatewayState,
    headers: HeaderMap,
    trust_turn_metadata: bool,
) {
    let (mut socket_sender, mut socket_receiver) = socket.split();
    let (turn_sender, mut turn_receiver) = mpsc::channel::<WebSocketTurnEvent>(128);
    let mut local_contexts = HashMap::<String, Value>::new();
    let mut current_turn_id = 0_u64;
    let mut active_turn = None::<(u64, JoinHandle<()>)>;
    let idle = tokio::time::sleep(Duration::from_secs(60 * 60));
    tokio::pin!(idle);
    loop {
        tokio::select! {
            _ = &mut idle => break,
            incoming = socket_receiver.next() => {
                let message = match incoming {
                    Some(Ok(message)) => message,
                    _ => break,
                };
                idle.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(60 * 60));
                let text = match message {
                    Message::Text(text) => text,
                    Message::Ping(payload) => {
                        if socket_sender.send(Message::Pong(payload)).await.is_err() { break; }
                        continue;
                    }
                    Message::Pong(_) => continue,
                    Message::Close(_) => break,
                    Message::Binary(_) => {
                        if socket_sender.send(websocket_error_message(400, "binary messages are not supported")).await.is_err() { break; }
                        continue;
                    }
                };
                let mut event = match serde_json::from_str::<Value>(&text) {
                    Ok(event) => event,
                    Err(_) => {
                        if socket_sender.send(websocket_error_message(400, "WebSocket event is not valid JSON")).await.is_err() { break; }
                        continue;
                    }
                };
                match event.get("type").and_then(Value::as_str) {
                    Some("response.processed") => continue,
                    Some("response.create") => {}
                    _ => continue,
                }
                if let Some((_, task)) = active_turn.take() {
                    task.abort();
                }
                current_turn_id = current_turn_id.wrapping_add(1).max(1);
                let turn_id = current_turn_id;
                let generate = event.get("generate").and_then(Value::as_bool).unwrap_or(true);
                if let Some(object) = event.as_object_mut() {
                    object.remove("type");
                    object.remove("generate");
                    object.remove("background");
                    object.remove("stream");
                }
                if let Some(previous_id) = event
                    .get("previous_response_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                {
                    if let Some(previous) = local_contexts.get(&previous_id) {
                        event = merge_websocket_context(previous, &event);
                        if let Some(object) = event.as_object_mut() {
                            object.remove("previous_response_id");
                        }
                    }
                }
                if !generate {
                    let id = format!("resp_{}", Uuid::new_v4().simple());
                    retain_websocket_context(&mut local_contexts, id.clone(), event.clone());
                    let mut created = websocket_response_object(&id, &event, "in_progress", Vec::new());
                    let completed = websocket_response_object(&id, &event, "completed", Vec::new());
                    created["output"] = Value::Array(Vec::new());
                    for value in [
                        json!({"type": "response.created", "sequence_number": 0, "response": created}),
                        json!({"type": "response.completed", "sequence_number": 1, "response": completed}),
                    ] {
                        if socket_sender.send(Message::Text(value.to_string().into())).await.is_err() { return; }
                    }
                    continue;
                }
                let request_context = event.clone();
                event["stream"] = Value::Bool(true);
                let task_sender = turn_sender.clone();
                let task_state = state.clone();
                let task_headers = headers.clone();
                let task = tokio::spawn(async move {
                    run_websocket_turn(
                        task_sender,
                        turn_id,
                        task_state,
                        task_headers,
                        event,
                        request_context,
                        trust_turn_metadata,
                    ).await;
                });
                active_turn = Some((turn_id, task));
            }
            output = turn_receiver.recv() => {
                let Some(output) = output else { break; };
                idle.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(60 * 60));
                let turn_id = output.turn_id();
                if turn_id != current_turn_id { continue; }
                match output {
                    WebSocketTurnEvent::Frame { value, .. } => {
                        if socket_sender.send(Message::Text(value.to_string().into())).await.is_err() { break; }
                    }
                    WebSocketTurnEvent::Error { status, message, .. } => {
                        if socket_sender.send(websocket_error_message(status, &message)).await.is_err() { break; }
                    }
                    WebSocketTurnEvent::Context { id, context, .. } => {
                        retain_websocket_context(&mut local_contexts, id, context);
                    }
                    WebSocketTurnEvent::Finished { .. } => {
                        if active_turn.as_ref().is_some_and(|(id, _)| *id == turn_id) {
                            active_turn.take();
                        }
                    }
                }
            }
        }
    }
    if let Some((_, task)) = active_turn {
        task.abort();
    }
}

enum WebSocketTurnEvent {
    Frame {
        turn_id: u64,
        value: Value,
    },
    Error {
        turn_id: u64,
        status: u16,
        message: String,
    },
    Context {
        turn_id: u64,
        id: String,
        context: Value,
    },
    Finished {
        turn_id: u64,
    },
}

impl WebSocketTurnEvent {
    fn turn_id(&self) -> u64 {
        match self {
            Self::Frame { turn_id, .. }
            | Self::Error { turn_id, .. }
            | Self::Context { turn_id, .. }
            | Self::Finished { turn_id } => *turn_id,
        }
    }
}

async fn run_websocket_turn(
    sender: mpsc::Sender<WebSocketTurnEvent>,
    turn_id: u64,
    state: GatewayState,
    headers: HeaderMap,
    event: Value,
    request_context: Value,
    trust_turn_metadata: bool,
) {
    let response = responses_inner(state, headers, event, trust_turn_metadata).await;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let message = to_bytes(response.into_body(), 64 * 1024)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| {
                value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "CODETAS could not create the response".into());
        let _ = sender
            .send(WebSocketTurnEvent::Error {
                turn_id,
                status,
                message,
            })
            .await;
        let _ = sender.send(WebSocketTurnEvent::Finished { turn_id }).await;
        return;
    }
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if content_type.contains("application/json") {
        match to_bytes(response.into_body(), 64 * 1024 * 1024)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        {
            Some(value) => {
                for frame in websocket_json_response_events(&value) {
                    if frame.get("type").and_then(Value::as_str) == Some("response.completed") {
                        if let Some(response) = frame.get("response") {
                            if let Some(id) = response.get("id").and_then(Value::as_str) {
                                let _ = sender
                                    .send(WebSocketTurnEvent::Context {
                                        turn_id,
                                        id: id.to_string(),
                                        context: context_after_response(&request_context, response),
                                    })
                                    .await;
                            }
                        }
                    }
                    if sender
                        .send(WebSocketTurnEvent::Frame {
                            turn_id,
                            value: frame,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            None => {
                let _ = sender
                    .send(WebSocketTurnEvent::Error {
                        turn_id,
                        status: 502,
                        message: "Responses JSON was invalid or too large".into(),
                    })
                    .await;
            }
        }
        let _ = sender.send(WebSocketTurnEvent::Finished { turn_id }).await;
        return;
    }

    let mut source = response.into_body().into_data_stream();
    let mut pending = Vec::new();
    let mut terminal_seen = false;
    while let Some(chunk) = source.next().await {
        let bytes = match chunk {
            Ok(bytes) => bytes,
            Err(_) => {
                let _ = sender
                    .send(WebSocketTurnEvent::Error {
                        turn_id,
                        status: 502,
                        message: "Responses stream failed".into(),
                    })
                    .await;
                let _ = sender.send(WebSocketTurnEvent::Finished { turn_id }).await;
                return;
            }
        };
        let values = match drain_sse_values(&mut pending, &bytes) {
            Ok(values) => values,
            Err(_) => {
                let _ = sender
                    .send(WebSocketTurnEvent::Error {
                        turn_id,
                        status: 502,
                        message: "Responses stream was invalid".into(),
                    })
                    .await;
                let _ = sender.send(WebSocketTurnEvent::Finished { turn_id }).await;
                return;
            }
        };
        for value in values {
            let kind = value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if kind == "response.completed" {
                if let Some(response) = value.get("response") {
                    if let Some(id) = response.get("id").and_then(Value::as_str) {
                        if sender
                            .send(WebSocketTurnEvent::Context {
                                turn_id,
                                id: id.to_string(),
                                context: context_after_response(&request_context, response),
                            })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
            terminal_seen = matches!(
                kind,
                "response.completed" | "response.failed" | "response.incomplete"
            );
            if sender
                .send(WebSocketTurnEvent::Frame { turn_id, value })
                .await
                .is_err()
            {
                return;
            }
            if terminal_seen {
                break;
            }
        }
        if terminal_seen {
            break;
        }
    }
    if !terminal_seen {
        let message = if pending.iter().all(u8::is_ascii_whitespace) {
            "Responses stream ended before a terminal event"
        } else {
            "Responses stream ended mid-frame"
        };
        let _ = sender
            .send(WebSocketTurnEvent::Error {
                turn_id,
                status: 502,
                message: message.into(),
            })
            .await;
    }
    let _ = sender.send(WebSocketTurnEvent::Finished { turn_id }).await;
}

fn websocket_json_response_events(response: &Value) -> Vec<Value> {
    let mut in_progress = response.clone();
    in_progress["status"] = Value::String("in_progress".into());
    in_progress["output"] = Value::Array(Vec::new());
    let mut events = vec![json!({
        "type": "response.created",
        "sequence_number": 0,
        "response": in_progress,
    })];
    for (index, item) in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        events.push(json!({
            "type": "response.output_item.added",
            "sequence_number": events.len(),
            "output_index": index,
            "item": item,
        }));
        if item.get("type").and_then(Value::as_str) == Some("message") {
            for (content_index, part) in item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    events.push(json!({
                        "type": "response.output_text.delta",
                        "sequence_number": events.len(),
                        "item_id": item.get("id").cloned().unwrap_or(Value::Null),
                        "output_index": index,
                        "content_index": content_index,
                        "delta": text,
                    }));
                    events.push(json!({
                        "type": "response.output_text.done",
                        "sequence_number": events.len(),
                        "item_id": item.get("id").cloned().unwrap_or(Value::Null),
                        "output_index": index,
                        "content_index": content_index,
                        "text": text,
                    }));
                }
            }
        }
        events.push(json!({
            "type": "response.output_item.done",
            "sequence_number": events.len(),
            "output_index": index,
            "item": item,
        }));
    }
    let status = match response.get("status").and_then(Value::as_str) {
        Some("failed") => "failed",
        Some("incomplete") => "incomplete",
        _ => "completed",
    };
    let mut terminal = response.clone();
    terminal["status"] = Value::String(status.into());
    events.push(json!({
        "type": format!("response.{status}"),
        "sequence_number": events.len(),
        "response": terminal,
    }));
    events
}

fn websocket_error_message(status: u16, message: &str) -> Message {
    Message::Text(
        json!({"type": "error", "status": status, "error": {"type": "invalid_request_error", "message": message}})
            .to_string()
            .into(),
    )
}

fn merge_websocket_context(previous: &Value, current: &Value) -> Value {
    let mut merged = previous.clone();
    let previous_input = websocket_input_items(merged.get("input"));
    if let (Some(target), Some(source)) = (merged.as_object_mut(), current.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    let mut input = previous_input;
    input.extend(websocket_input_items(current.get("input")));
    merged["input"] = Value::Array(input);
    merged
}

fn context_after_response(request: &Value, response: &Value) -> Value {
    let mut context = request.clone();
    let mut input = websocket_input_items(context.get("input"));
    let mut output = response
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for item in &mut output {
        let repairable = matches!(
            item.get("type").and_then(Value::as_str),
            Some("message" | "reasoning")
        );
        let synthetic = item
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(is_codetas_repaired_item_id);
        if repairable && synthetic {
            if let Some(object) = item.as_object_mut() {
                object.remove("id");
            }
        }
    }
    input.extend(output);
    context["input"] = Value::Array(input);
    context
}

fn websocket_input_items(input: Option<&Value>) -> Vec<Value> {
    match input {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::String(text)) => vec![json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": text}]
        })],
        _ => Vec::new(),
    }
}

fn retain_websocket_context(contexts: &mut HashMap<String, Value>, id: String, context: Value) {
    if serde_json::to_vec(&context).is_ok_and(|bytes| bytes.len() <= 16 * 1024 * 1024) {
        if contexts.len() >= 8 {
            contexts.clear();
        }
        contexts.insert(id, context);
    }
}

fn websocket_response_object(id: &str, request: &Value, status: &str, output: Vec<Value>) -> Value {
    json!({
        "id": id,
        "object": "response",
        "created_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0),
        "status": status,
        "model": request.get("model").cloned().unwrap_or(Value::Null),
        "output": output,
        "usage": Value::Null,
        "error": Value::Null,
        "incomplete_details": Value::Null
    })
}

async fn responses(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    let admission = match authorize_request(&state.settings, &headers, "responses:write").await {
        Ok(admission) => admission,
        Err(response) => return response,
    };
    responses_inner(state, headers, body, admission.trusts_turn_metadata()).await
}

async fn responses_inner(
    state: GatewayState,
    headers: HeaderMap,
    mut body: Value,
    trust_turn_metadata: bool,
) -> Response<Body> {
    let started = Instant::now();
    let request_id = Uuid::new_v4().to_string();
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let claims_subagent = is_subagent_request(&headers);
    let claims_subagent = is_subagent_request(&headers);
    let requested_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let (candidates, observability_settings, effort_cap) = {
        let settings = state.settings.read().await;
        // Turn metadata is only a routing authority after the caller has
        // passed admission authentication. Without it, the header is advisory
        // and cannot select a different cost/model policy.
        let is_subagent = claims_subagent && trust_turn_metadata;
        let observability_settings = settings.observability.clone();
        let effort_cap = if is_subagent {
            settings
                .agents
                .subagent_effort_cap
                .clone()
                .or_else(|| settings.agents.effort_cap.clone())
        } else {
            settings.agents.effort_cap.clone()
        };
        let desktop_target = claude_desktop_target(&headers, &settings, &requested_model);
        let effective_model = desktop_target.as_deref().unwrap_or(&requested_model);
        let mut routing = state.routing.lock().await;
        let mut candidates =
            routing.candidates_for_request(&settings, effective_model, is_subagent);
        if desktop_target.is_some() {
            if let Ok(candidates) = candidates.as_mut() {
                for candidate in candidates {
                    candidate.exposed_model = requested_model.clone();
                }
            }
        }
        (candidates, observability_settings, effort_cap)
    };
    if request_is_remote_compaction(&body) {
        if let Some(object) = body.as_object_mut() {
            object.insert("stream".into(), Value::Bool(false));
        }
        return compact_response(State(state), headers, Json(body)).await;
    }
    // Replay the locally cached continuation history for `previous_response_id`
    // before routing. The ChatGPT Codex backend rejects that field (see
    // `sanitize_responses_upstream_request`), so without this expansion the
    // upstream would only ever see the delta input the client appends each
    // turn — losing all earlier context and making a plan-mode model re-propose
    // `update_plan` forever. Compaction turns are excluded above.
    let had_previous_response = body
        .get("previous_response_id")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());
    let expanded_previous = state
        .response_state
        .expand_previous_response_input(&mut body);
    // A delta-only request whose own continuation could not be expanded must not
    // be recorded: storing it would replay a truncated conversation (opencodex
    // applies the same guard). Requests without `previous_response_id` are always
    // eligible — they carry the full input themselves.
    let record_eligible = !had_previous_response || expanded_previous;
    if let Some(cap) = effort_cap.as_deref() {
        cap_reasoning_effort(&mut body, cap);
    }
    let candidates = match candidates {
        Ok(candidates) => candidates,
        Err(message) => {
            ObservationSeed::without_candidate(
                state.observability.clone(),
                observability_settings,
                request_id,
                &requested_model,
                streaming,
                started,
            )
            .finish(
                StatusCode::BAD_REQUEST,
                Some("routing_rejected"),
                TokenUsage::default(),
            );
            return error_response(StatusCode::BAD_REQUEST, "invalid_request", &message);
        }
    };
    let mut last_failure = None;
    for (index, candidate) in candidates.iter().enumerate() {
        let attempts = (index + 1).min(usize::from(u16::MAX)) as u16;
        let has_next = index + 1 < candidates.len();
        let mut candidate_body = body.clone();
        if let Err(message) =
            apply_candidate_model_policy(&mut candidate_body, candidate, claims_subagent)
        {
            if has_next {
                last_failure = Some(error_response(
                    StatusCode::BAD_REQUEST,
                    "model_limit_exceeded",
                    &message,
                ));
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
                StatusCode::BAD_REQUEST,
                Some("model_limit_exceeded"),
                TokenUsage::default(),
            );
            return error_response(StatusCode::BAD_REQUEST, "model_limit_exceeded", &message);
        }
        let upstream = match send_candidate(&state, &candidate_body, candidate, Some(&headers))
            .await
        {
            Ok(upstream) => upstream,
            Err(failure) => {
                if matches!(
                    failure.kind,
                    AttemptFailureKind::Credential | AttemptFailureKind::Retryable
                ) {
                    state.routing.lock().await.record_failure(candidate);
                }
                let can_retry = match failure.kind {
                    AttemptFailureKind::Credential => candidates[index + 1..].iter().any(|next| {
                        next.target_key == candidate.target_key && next.account_id.is_some()
                    }),
                    AttemptFailureKind::Retryable => has_next,
                    AttemptFailureKind::Request => false,
                };
                if can_retry {
                    last_failure = Some(failure.response);
                    continue;
                }
                let status = failure.response.status();
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
                    status,
                    Some(failure.kind.category()),
                    TokenUsage::default(),
                );
                return failure.response;
            }
        };
        if !upstream.status().is_success() {
            let status = upstream.status();
            let account_retry = matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
                && candidates[index + 1..].iter().any(|next| {
                    next.target_key == candidate.target_key && next.account_id.is_some()
                });
            let transient = status == StatusCode::REQUEST_TIMEOUT
                || status == StatusCode::TOO_MANY_REQUESTS
                || status.is_server_error();
            let retry_after = validated_retry_after(upstream.headers());
            let response =
                upstream_error(upstream, retry_after.as_ref().map(|value| &value.0)).await;
            if status == StatusCode::TOO_MANY_REQUESTS {
                state
                    .routing
                    .lock()
                    .await
                    .record_quota_exhausted(candidate, retry_after.and_then(|value| value.1));
            } else if account_retry || transient {
                state.routing.lock().await.record_failure(candidate);
            }
            if has_next && (account_retry || transient) {
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

        let quota = if candidate.quota_threshold_percent == 0 {
            None
        } else {
            quota_usage_percent(upstream.headers())
        };
        state.routing.lock().await.record_success(candidate, quota);
        schedule_shadow_calls(
            state.clone(),
            body.clone(),
            request_id.clone(),
            requested_model.clone(),
            candidate.clone(),
        );
        let observation = ObservationSeed::for_candidate(
            state.observability.clone(),
            observability_settings.clone(),
            request_id.clone(),
            streaming,
            started,
            attempts,
            candidate,
        );
        let custom_tools = custom_tool_names(&candidate_body);
        return adapt_successful_response(
            upstream,
            candidate,
            observation,
            custom_tools,
            &state.response_state,
            &body,
            record_eligible,
        )
        .await;
    }
    let response = last_failure.unwrap_or_else(|| {
        error_response(
            StatusCode::BAD_GATEWAY,
            "provider_unavailable",
            "no provider route completed the request",
        )
    });
    ObservationSeed::without_candidate(
        state.observability.clone(),
        observability_settings,
        request_id,
        &requested_model,
        streaming,
        started,
    )
    .finish(
        response.status(),
        Some("provider_unavailable"),
        TokenUsage::default(),
    );
    response
}

fn claude_desktop_target(
    headers: &HeaderMap,
    settings: &GatewaySettings,
    requested_model: &str,
) -> Option<String> {
    let client = headers
        .get("x-codetas-client")
        .and_then(|value| value.to_str().ok())?;
    if client != "claude-desktop" || !settings.integrations.claude_desktop {
        return None;
    }
    settings
        .integrations
        .claude_desktop_aliases
        .get(requested_model)
        .cloned()
}

fn is_subagent_request(headers: &HeaderMap) -> bool {
    let Some(raw) = headers
        .get("x-codex-turn-metadata")
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 8 * 1024)
    else {
        return false;
    };
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|metadata| {
            metadata
                .get("thread_source")
                .or_else(|| metadata.pointer("/turn/thread_source"))
                .and_then(Value::as_str)
                .map(|source| source == "subagent")
        })
        .unwrap_or(false)
}

fn cap_reasoning_effort(body: &mut Value, cap: &str) {
    let requested = body
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(requested) = requested else { return };
    let Some(cap_rank) = reasoning_rank(cap) else {
        return;
    };
    if reasoning_rank(&requested).is_some_and(|rank| rank > cap_rank) {
        if let Some(reasoning) = body.get_mut("reasoning").and_then(Value::as_object_mut) {
            reasoning.insert("effort".into(), Value::String(cap.to_string()));
        }
    }
}

fn reasoning_rank(value: &str) -> Option<u8> {
    match value {
        "none" => Some(0),
        "minimal" => Some(1),
        "low" => Some(2),
        "medium" => Some(3),
        "high" => Some(4),
        "xhigh" => Some(5),
        "max" => Some(6),
        "ultra" => Some(7),
        _ => None,
    }
}

fn apply_candidate_model_policy(
    body: &mut Value,
    candidate: &RouteCandidate,
    is_subagent: bool,
) -> Result<(), String> {
    let requested_output = body.get("max_output_tokens").and_then(Value::as_u64);
    let effective_output = match (requested_output, candidate.max_output_tokens) {
        (Some(requested), Some(limit)) => {
            let value = requested.min(limit);
            body["max_output_tokens"] = json!(value);
            value
        }
        (None, Some(limit)) => {
            body["max_output_tokens"] = json!(limit);
            limit
        }
        (Some(requested), None) => requested,
        (None, None) => 0,
    };
    // Subagent turns inherit the parent conversation history through the
    // collaboration transport. When that history exceeds the subagent model's
    // context budget, shrink the input instead of failing the turn — the
    // parent's own budget stays untouched.
    if is_subagent && shrink_subagent_input(body, candidate, effective_output)? {
        eprintln!(
            "CODETAS Gateway: shrunk subagent input to fit {}/{} budget",
            candidate.provider.id, candidate.upstream_model
        );
    } else {
        enforce_candidate_input_budget(body, candidate, effective_output)?;
    }

    let requested_effort = body
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mapped = if candidate.reasoning_efforts.is_empty() {
        candidate.default_reasoning_effort.clone()
    } else if let Some(requested) = requested_effort.as_deref() {
        if candidate
            .reasoning_efforts
            .iter()
            .any(|effort| effort == requested)
        {
            Some(requested.to_string())
        } else {
            let requested_rank = reasoning_rank(requested).unwrap_or(u8::MAX);
            candidate
                .reasoning_efforts
                .iter()
                .filter_map(|effort| reasoning_rank(effort).map(|rank| (rank, effort)))
                .filter(|(rank, _)| *rank <= requested_rank)
                .max_by_key(|(rank, _)| *rank)
                .or_else(|| {
                    candidate
                        .reasoning_efforts
                        .iter()
                        .filter_map(|effort| reasoning_rank(effort).map(|rank| (rank, effort)))
                        .min_by_key(|(rank, _)| *rank)
                })
                .map(|(_, effort)| effort.clone())
        }
    } else {
        candidate
            .default_reasoning_effort
            .clone()
            .or_else(|| candidate.reasoning_efforts.first().cloned())
    };
    if let Some(mapped) = mapped {
        if !body.get("reasoning").is_some_and(Value::is_object) {
            body["reasoning"] = json!({});
        }
        body["reasoning"]["effort"] = Value::String(mapped);
    }
    Ok(())
}

fn enforce_candidate_input_budget(
    body: &Value,
    candidate: &RouteCandidate,
    reserved_output_tokens: u64,
) -> Result<(), String> {
    let context_input = candidate
        .context_window
        .map(|context| context.saturating_sub(reserved_output_tokens));
    let limit = match (candidate.max_input_tokens, context_input) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(limit), None) | (None, Some(limit)) => Some(limit),
        (None, None) => None,
    };
    let Some(limit) = limit else { return Ok(()) };
    // Responses-lite requests carry their tool definitions as `additional_tools`
    // items inside `input` (Codex Desktop's responses-lite shape). Those tool
    // definitions are not conversation history and must not be counted against
    // the input token budget — they are constant overhead already accounted for
    // by the model's own tool budget. Exclude them before measuring.
    let input_bytes = body
        .get("input")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    item.get("type").and_then(Value::as_str) != Some("additional_tools")
                })
                .collect::<Vec<_>>()
        })
        .map(|filtered| serde_json::to_vec(&filtered))
        .transpose()
        .map_err(|_| "request input cannot be encoded for limit enforcement".to_string())?
        .map(|bytes| bytes.len() as u64)
        .unwrap_or(0);
    // The budget is expressed in tokens, but the request only exposes the input
    // as serialized bytes. Comparing raw byte length against a token limit
    // rejects valid requests at roughly a quarter of the real context window, so
    // approximate the token count with a conservative bytes-per-token ratio.
    let estimated_input_tokens = input_bytes.saturating_add(APPROX_INPUT_BYTES_PER_TOKEN - 1)
        / APPROX_INPUT_BYTES_PER_TOKEN;
    if estimated_input_tokens > limit {
        return Err(format!(
            "request input exceeds the conservative {}-token budget for {}/{}",
            limit, candidate.provider.id, candidate.upstream_model
        ));
    }
    Ok(())
}

/// Compute the input token estimate for the given input items (same
/// approximation as `enforce_candidate_input_budget`).
fn estimate_input_items_tokens(items: &[Value]) -> u64 {
    let bytes = items
        .iter()
        .filter(|item| {
            item.get("type").and_then(Value::as_str) != Some("additional_tools")
        })
        .collect::<Vec<_>>();
    let input_bytes = serde_json::to_vec(&bytes).map(|v| v.len() as u64).unwrap_or(0);
    input_bytes.saturating_add(APPROX_INPUT_BYTES_PER_TOKEN - 1) / APPROX_INPUT_BYTES_PER_TOKEN
}

/// Subagent turns inherit the parent conversation history through the
/// collaboration transport. When that history exceeds the subagent model's
/// context budget, drop redundant intermediate items (reasoning traces and
/// older custom tool outputs) to fit, instead of failing the turn. Returns
/// `true` when the input was modified, `false` when it already fits or cannot
/// be shrunk further.
fn shrink_subagent_input(
    body: &mut Value,
    candidate: &RouteCandidate,
    reserved_output_tokens: u64,
) -> Result<bool, String> {
    let context_input = candidate
        .context_window
        .map(|context| context.saturating_sub(reserved_output_tokens));
    let limit = match (candidate.max_input_tokens, context_input) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(limit), None) | (None, Some(limit)) => Some(limit),
        (None, None) => None,
    };
    let Some(limit) = limit else { return Ok(false) };
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return Ok(false);
    };
    if estimate_input_items_tokens(items) <= limit {
        return Ok(false);
    }
    // Drop in order of least value for a subagent continuation: reasoning
    // traces first, then older custom tool outputs. Keep messages, function
    // calls, and their outputs so the turn stays coherent.
    let mut changed = false;
    let mut removable = items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            matches!(
                item.get("type").and_then(Value::as_str),
                Some("reasoning" | "custom_tool_call_output")
            )
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    // Prefer dropping reasoning first: they are the largest and least
    // semantically essential for a continuation. Collect removable indices,
    // build a to-drop set in that order, then rebuild the array once with
    // `retain` so indices never go stale.
    removable.sort_by_key(|index| {
        items[*index].get("type").and_then(Value::as_str) != Some("reasoning")
    });
    let mut drop = std::collections::HashSet::new();
    while estimate_input_items_tokens(items) > limit {
        let Some(index) = removable.pop() else {
            break;
        };
        drop.insert(index);
        changed = true;
    }
    if changed {
        let retained = items
            .iter()
            .enumerate()
            .filter(|(index, _)| !drop.contains(index))
            .map(|(_, item)| item.clone())
            .collect::<Vec<_>>();
        *items = retained;
    }
    Ok(changed)
}

fn quota_usage_percent(headers: &HeaderMap) -> Option<u8> {
    [
        (
            "x-ratelimit-limit-requests",
            "x-ratelimit-remaining-requests",
        ),
        ("x-ratelimit-limit-tokens", "x-ratelimit-remaining-tokens"),
        ("ratelimit-limit", "ratelimit-remaining"),
    ]
    .into_iter()
    .filter_map(|(limit_name, remaining_name)| {
        let limit = headers
            .get(limit_name)?
            .to_str()
            .ok()?
            .parse::<f64>()
            .ok()?;
        let remaining = headers
            .get(remaining_name)?
            .to_str()
            .ok()?
            .parse::<f64>()
            .ok()?;
        if !limit.is_finite() || !remaining.is_finite() || limit <= 0.0 {
            return None;
        }
        let used = ((limit - remaining).max(0.0) / limit * 100.0).ceil();
        Some(used.clamp(0.0, 100.0) as u8)
    })
    .max()
}

#[derive(Clone, Copy)]
enum Admission {
    Anonymous,
    LocalMaster,
    External,
}

impl Admission {
    fn trusts_turn_metadata(self) -> bool {
        matches!(self, Self::LocalMaster)
    }
}

async fn authorize_request(
    settings: &SharedSettings,
    headers: &HeaderMap,
    required_scope: &str,
) -> Result<Admission, Response<Body>> {
    let security = settings.read().await.security.clone();
    let external_keys = security
        .external_access_keys
        .iter()
        .filter(|key| key.enabled)
        .collect::<Vec<_>>();
    if !security.require_local_token && external_keys.is_empty() {
        return Ok(Admission::Anonymous);
    }

    let provided = provided_access_tokens(headers);
    if security.require_local_token {
        if let Ok(expected) = std::env::var("CODETAS_GATEWAY_TOKEN") {
            if !expected.is_empty()
                && provided
                    .iter()
                    .any(|token| constant_time_equal(expected.as_bytes(), token.as_bytes()))
            {
                return Ok(Admission::LocalMaster);
            }
        } else if external_keys.is_empty() {
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "local_token_unavailable",
                "CODETAS_GATEWAY_TOKEN is required by the gateway configuration",
            ));
        }
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    for key in external_keys {
        let Ok(expected) = std::env::var(&key.env_var) else {
            continue;
        };
        if expected.is_empty()
            || !provided
                .iter()
                .any(|token| constant_time_equal(expected.as_bytes(), token.as_bytes()))
        {
            continue;
        }
        if key.expires_at_unix.is_some_and(|expires| expires <= now) {
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "access_key_expired",
                "the matching CODETAS access key has expired",
            ));
        }
        if key
            .scopes
            .iter()
            .any(|scope| scope == "gateway:*" || scope == required_scope)
        {
            return Ok(Admission::External);
        }
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "access_scope_denied",
            "the CODETAS access key does not grant this operation",
        ));
    }
    Err(error_response(
        StatusCode::UNAUTHORIZED,
        "invalid_gateway_token",
        "a valid CODETAS gateway token is required",
    ))
}

fn provided_access_tokens(headers: &HeaderMap) -> Vec<&str> {
    let mut tokens = Vec::new();
    for name in ["x-codetas-token", "x-api-key", "x-goog-api-key"] {
        if let Some(token) = headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .filter(|token| !token.is_empty())
        {
            tokens.push(token);
        }
    }
    if let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty())
    {
        tokens.push(token);
    }
    tokens
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

const APPROX_INPUT_BYTES_PER_TOKEN: u64 = 4;

const FORWARD_PROVIDER_HEADERS: &[&str] = &[
    "authorization",
    "chatgpt-account-id",
    "openai-beta",
    "originator",
    "session_id",
    "session-id",
    "thread-id",
    "x-client-request-id",
    "x-codex-beta-features",
    "x-codex-installation-id",
    "x-codex-parent-thread-id",
    "x-codex-turn-metadata",
    "x-codex-turn-state",
    "x-codex-window-id",
    "x-oai-attestation",
    "x-openai-subagent",
    "x-responsesapi-include-timing-metrics",
];

async fn apply_forward_headers(
    mut request: reqwest::RequestBuilder,
    settings: &SharedSettings,
    incoming: &HeaderMap,
) -> Result<reqwest::RequestBuilder, String> {
    let headers = collect_forward_headers(settings, incoming).await?;
    for (name, value) in &headers {
        request = request.header(name.clone(), value.clone());
    }
    Ok(request)
}

async fn collect_forward_headers(
    settings: &SharedSettings,
    incoming: &HeaderMap,
) -> Result<HeaderMap, String> {
    let security = settings.read().await.security.clone();
    let mut admission_secrets = Vec::new();
    if security.require_local_token {
        if let Ok(value) = std::env::var("CODETAS_GATEWAY_TOKEN") {
            if !value.is_empty() {
                admission_secrets.push(value);
            }
        }
    }
    for key in security
        .external_access_keys
        .iter()
        .filter(|key| key.enabled)
    {
        if let Ok(value) = std::env::var(&key.env_var) {
            if !value.is_empty() {
                admission_secrets.push(value);
            }
        }
    }

    let mut forwarded_authorization = false;
    let mut forwarded = HeaderMap::new();
    for name in FORWARD_PROVIDER_HEADERS {
        let Some(value) = incoming.get(*name) else {
            continue;
        };
        if *name == "authorization" {
            let presented = value
                .to_str()
                .ok()
                .and_then(|value| value.strip_prefix("Bearer "));
            if presented.is_some_and(|presented| {
                admission_secrets
                    .iter()
                    .any(|secret| constant_time_equal(secret.as_bytes(), presented.as_bytes()))
            }) {
                continue;
            }
            forwarded_authorization = true;
        }
        let parsed = header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| "internal forward header policy is invalid".to_string())?;
        forwarded.insert(parsed, value.clone());
    }
    if !forwarded_authorization {
        return Err(
            "forward authentication requires a caller Authorization header distinct from the CODETAS admission token; use x-codetas-token for gateway admission"
                .into(),
        );
    }
    Ok(forwarded)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AttemptFailureKind {
    Request,
    Credential,
    Retryable,
}

impl AttemptFailureKind {
    fn category(self) -> &'static str {
        match self {
            Self::Request => "request_rejected",
            Self::Credential => "credential_unavailable",
            Self::Retryable => "provider_unreachable",
        }
    }
}

#[derive(Clone)]
struct ObservationSeed {
    ledger: ObservabilityLedger,
    settings: ObservabilitySettings,
    request_id: String,
    provider_id: Option<String>,
    upstream_model: Option<String>,
    exposed_model: String,
    route_id: Option<String>,
    account_id: Option<String>,
    streaming: bool,
    started: Instant,
    attempts: u16,
    input_price_per_million: Option<f64>,
    output_price_per_million: Option<f64>,
}

impl ObservationSeed {
    fn without_candidate(
        ledger: ObservabilityLedger,
        settings: ObservabilitySettings,
        request_id: String,
        exposed_model: &str,
        streaming: bool,
        started: Instant,
    ) -> Self {
        Self {
            ledger,
            settings,
            request_id,
            provider_id: None,
            upstream_model: None,
            exposed_model: bounded_metadata(exposed_model),
            route_id: None,
            account_id: None,
            streaming,
            started,
            attempts: 0,
            input_price_per_million: None,
            output_price_per_million: None,
        }
    }

    fn for_candidate(
        ledger: ObservabilityLedger,
        settings: ObservabilitySettings,
        request_id: String,
        streaming: bool,
        started: Instant,
        attempts: u16,
        candidate: &RouteCandidate,
    ) -> Self {
        Self {
            ledger,
            settings,
            request_id,
            provider_id: Some(candidate.provider.id.clone()),
            upstream_model: Some(bounded_metadata(&candidate.upstream_model)),
            exposed_model: bounded_metadata(&candidate.exposed_model),
            route_id: candidate.route_id.clone(),
            account_id: candidate.account_id.clone(),
            streaming,
            started,
            attempts,
            input_price_per_million: candidate.input_price_per_million,
            output_price_per_million: candidate.output_price_per_million,
        }
    }

    fn finish(self, status: StatusCode, failure_category: Option<&str>, usage: TokenUsage) {
        let estimated_cost_usd =
            if self.input_price_per_million.is_some() || self.output_price_per_million.is_some() {
                Some(
                    usage.input_tokens as f64 * self.input_price_per_million.unwrap_or(0.0)
                        / 1_000_000.0
                        + usage.output_tokens as f64 * self.output_price_per_million.unwrap_or(0.0)
                            / 1_000_000.0,
                )
            } else {
                None
            };
        self.ledger.record(
            ObservationEvent {
                ledger_sequence: 0,
                timestamp_ms: ObservationEvent::now_ms(),
                request_id: self.request_id,
                provider_id: self.provider_id,
                upstream_model: self.upstream_model,
                exposed_model: self.exposed_model,
                route_id: self.route_id,
                account_id: self.account_id,
                status_code: status.as_u16(),
                outcome: if status.is_success() && failure_category.is_none() {
                    "success".into()
                } else {
                    "failure".into()
                },
                failure_category: failure_category.map(str::to_string),
                latency_ms: self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                attempts: self.attempts,
                streaming: self.streaming,
                usage,
                estimated_cost_usd,
                shadow: false,
                shadow_rule_id: None,
                parent_request_id: None,
            },
            self.settings,
        );
    }
}

fn bounded_metadata(value: &str) -> String {
    value.chars().take(240).collect()
}

fn schedule_shadow_calls(
    state: GatewayState,
    body: Value,
    parent_request_id: String,
    requested_model: String,
    primary: RouteCandidate,
) {
    tokio::spawn(async move {
        let (rules, observation_settings) = {
            let settings = state.settings.read().await;
            (
                settings
                    .shadows
                    .iter()
                    .filter(|rule| {
                        rule.enabled
                            && (rule.source_model == requested_model
                                || rule.source_model == primary.target_key)
                            && shadow_sampled(&parent_request_id, &rule.id, rule.sample_percent)
                    })
                    .cloned()
                    .collect::<Vec<_>>(),
                settings.observability.clone(),
            )
        };
        let mut launched = 0_usize;
        for rule in rules {
            for target in &rule.targets {
                if launched >= 8 {
                    return;
                }
                let candidate = {
                    let settings = state.settings.read().await;
                    let mut routing = state.routing.lock().await;
                    routing
                        .candidates(&settings, target)
                        .ok()
                        .and_then(|candidates| candidates.into_iter().next())
                };
                let Some(mut candidate) = candidate else {
                    continue;
                };
                if candidate.target_key == primary.target_key
                    && candidate.account_id == primary.account_id
                {
                    continue;
                }
                launched += 1;
                candidate.provider.limits.request_timeout_ms = candidate
                    .provider
                    .limits
                    .request_timeout_ms
                    .min(rule.timeout_ms);
                candidate.provider.limits.max_response_bytes = candidate
                    .provider
                    .limits
                    .max_response_bytes
                    .min(rule.max_response_bytes);
                let mut shadow_body = body.clone();
                shadow_body["stream"] = Value::Bool(false);
                let started = Instant::now();
                let request_id = Uuid::new_v4().to_string();
                let (status, failure, usage) =
                    match apply_candidate_model_policy(&mut shadow_body, &candidate, false) {
                        Err(_) => (
                            StatusCode::BAD_REQUEST,
                            Some("shadow_model_limit"),
                            TokenUsage::default(),
                        ),
                        Ok(()) => {
                            match send_candidate(&state, &shadow_body, &candidate, None).await {
                                Err(failure) => (
                                    failure.response.status(),
                                    Some(failure.kind.category()),
                                    TokenUsage::default(),
                                ),
                                Ok(response) if !response.status().is_success() => {
                                    let status = response.status();
                                    let _ = read_bounded(response, rule.max_response_bytes).await;
                                    (status, Some("provider_http_error"), TokenUsage::default())
                                }
                                Ok(response) => match candidate_response_value(
                                    response,
                                    &candidate,
                                    rule.max_response_bytes,
                                )
                                .await
                                {
                                    Ok(value) => {
                                        (StatusCode::OK, None, TokenUsage::from_json(&value))
                                    }
                                    Err(_) => (
                                        StatusCode::BAD_GATEWAY,
                                        Some("invalid_provider_response"),
                                        TokenUsage::default(),
                                    ),
                                },
                            }
                        }
                    };
                let estimated_cost_usd = if candidate.input_price_per_million.is_some()
                    || candidate.output_price_per_million.is_some()
                {
                    Some(
                        usage.input_tokens as f64
                            * candidate.input_price_per_million.unwrap_or(0.0)
                            / 1_000_000.0
                            + usage.output_tokens as f64
                                * candidate.output_price_per_million.unwrap_or(0.0)
                                / 1_000_000.0,
                    )
                } else {
                    None
                };
                state.observability.record(
                    ObservationEvent {
                        ledger_sequence: 0,
                        timestamp_ms: ObservationEvent::now_ms(),
                        request_id,
                        provider_id: Some(candidate.provider.id.clone()),
                        upstream_model: Some(bounded_metadata(&candidate.upstream_model)),
                        exposed_model: bounded_metadata(target),
                        route_id: candidate.route_id.clone(),
                        account_id: candidate.account_id.clone(),
                        status_code: status.as_u16(),
                        outcome: if status.is_success() && failure.is_none() {
                            "success".into()
                        } else {
                            "failure".into()
                        },
                        failure_category: failure.map(str::to_string),
                        latency_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                        attempts: 1,
                        streaming: false,
                        usage,
                        estimated_cost_usd,
                        shadow: true,
                        shadow_rule_id: Some(rule.id.clone()),
                        parent_request_id: Some(parent_request_id.clone()),
                    },
                    observation_settings.clone(),
                );
            }
        }
    });
}

fn shadow_sampled(parent_request_id: &str, rule_id: &str, sample_percent: u8) -> bool {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in parent_request_id.bytes().chain(rule_id.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash % 100 < u64::from(sample_percent)
}

fn apply_provider_request_compatibility(
    body: &mut Value,
    candidate: &RouteCandidate,
    protocol: ProviderProtocol,
) {
    if let Some(tool_name) = guard_repeated_function_tool_loop(body) {
        eprintln!(
            "CODETAS Gateway: blocked repeated successful function-tool loop for {}",
            bounded_metadata(&tool_name)
        );
    }
    let provider = &candidate.provider;
    let model = &candidate.upstream_model;
    if protocol == ProviderProtocol::Responses
        && candidate.provider.transport == ProviderTransport::Standard
    {
        expand_local_compactions(body);
        sanitize_responses_upstream_request(body, provider, model);
    } else {
        prepare_translated_responses_request(
            body,
            candidate.provider.transport != ProviderTransport::Kiro,
        );
    }
    if protocol == ProviderProtocol::ChatCompletions {
        normalize_chat_reasoning_history(
            body,
            model_matches_any(model, &provider.preserve_reasoning_content_models),
        );
        if !model_supports_vision(provider, model) {
            strip_translated_input_images(body);
        }
    }
    let Some(object) = body.as_object_mut() else {
        return;
    };

    if model_matches_any(model, &provider.no_reasoning_models) {
        object.remove("reasoning");
    } else if let Some(effort) = object
        .get("reasoning")
        .and_then(Value::as_object)
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str)
        .map(str::to_string)
    {
        let mapped = map_value_ignore_case(&provider.model_reasoning_effort_map, model)
            .and_then(|mapping| mapping.get(&effort).cloned())
            .or_else(|| provider.reasoning_effort_map.get(&effort).cloned());
        if let Some(mapped) = mapped {
            if let Some(reasoning) = object.get_mut("reasoning").and_then(Value::as_object_mut) {
                reasoning.insert("effort".into(), Value::String(mapped));
            }
        }
    }
    if model_matches_any(model, &provider.no_temperature_models) {
        object.remove("temperature");
    }
    if model_matches_any(model, &provider.no_top_p_models) {
        object.remove("top_p");
    }
    if model_matches_any(model, &provider.no_penalty_models) {
        object.remove("presence_penalty");
        object.remove("frequency_penalty");
    }
    if let Some(parallel) = provider.parallel_tool_calls {
        object.insert("parallel_tool_calls".into(), Value::Bool(parallel));
    }
    if protocol == ProviderProtocol::ChatCompletions && !provider.prompt_cache_key {
        object.remove("prompt_cache_key");
    }
}

fn apply_provider_wire_compatibility(
    body: &mut Value,
    request: &Value,
    candidate: &RouteCandidate,
    protocol: ProviderProtocol,
) -> Result<(), String> {
    if protocol == ProviderProtocol::GeminiGenerateContent {
        if let Some(effort) = request
            .pointer("/reasoning/effort")
            .and_then(Value::as_str)
            .and_then(google_thinking_level)
        {
            let Some(object) = body.as_object_mut() else {
                return Ok(());
            };
            let generation = object
                .entry("generationConfig")
                .or_insert_with(|| json!({}));
            if !generation.is_object() {
                *generation = json!({});
            }
            if let Some(generation) = generation.as_object_mut() {
                generation.insert("thinkingConfig".into(), json!({"thinkingLevel": effort}));
            }
        }
        return Ok(());
    }
    if protocol == ProviderProtocol::AnthropicMessages {
        if candidate.provider.escape_builtin_tool_names {
            escape_anthropic_tool_names(body)?;
        }
        return Ok(());
    }
    if protocol == ProviderProtocol::Responses {
        if let Some(mode) = candidate
            .provider
            .model_reasoning_modes
            .get(&candidate.upstream_model)
        {
            let Some(object) = body.as_object_mut() else {
                return Ok(());
            };
            let reasoning = object.entry("reasoning").or_insert_with(|| json!({}));
            if !reasoning.is_object() {
                *reasoning = json!({});
            }
            if let Some(reasoning) = reasoning.as_object_mut() {
                reasoning.insert("mode".into(), Value::String(mode.clone()));
            }
        }
        return Ok(());
    }
    if protocol != ProviderProtocol::ChatCompletions {
        return Ok(());
    }
    let provider = &candidate.provider;
    let model = &candidate.upstream_model;
    let Some(object) = body.as_object_mut() else {
        return Ok(());
    };
    let effort = request.pointer("/reasoning/effort").and_then(Value::as_str);
    if model_matches_any(model, &provider.reasoning_split_models) {
        object.insert("reasoning_split".into(), Value::Bool(true));
    }
    if let Some(effort) = effort {
        if model_matches_any(model, &provider.thinking_budget_models) {
            if let Some(rank) = reasoning_rank(effort) {
                let max_tokens = object
                    .get("max_tokens")
                    .and_then(Value::as_u64)
                    .or(candidate.max_output_tokens)
                    .unwrap_or(32_768);
                let percent = match rank {
                    0 | 1 => 0,
                    2 => 20,
                    3 => 50,
                    4 => 75,
                    5 => 90,
                    _ => 100,
                };
                if rank == 1 {
                    object.insert("thinking_budget".into(), json!(0));
                } else if percent > 0 {
                    object.insert(
                        "thinking_budget".into(),
                        json!((max_tokens.saturating_mul(percent) / 100).max(1)),
                    );
                }
            }
        } else if model_matches_any(model, &provider.thinking_toggle_models) || is_minimax_m3(model)
        {
            let toggle = if is_minimax_m3(model) {
                match effort {
                    "enabled" | "disabled" | "adaptive" => Some(effort),
                    value if reasoning_rank(value).is_some_and(|rank| rank <= 2) => {
                        Some("disabled")
                    }
                    value if reasoning_rank(value).is_some() => Some("adaptive"),
                    _ => None,
                }
            } else {
                match effort {
                    "enabled" | "disabled" | "adaptive" => Some(effort),
                    value if reasoning_rank(value).is_some_and(|rank| rank <= 2) => {
                        Some("disabled")
                    }
                    value if reasoning_rank(value).is_some() => Some("enabled"),
                    _ => None,
                }
            };
            if let Some(toggle) = toggle {
                object.insert("thinking".into(), json!({"type": toggle}));
            }
        } else if map_value_ignore_case(&provider.model_reasoning_efforts, model)
            .is_some_and(|efforts| !efforts.is_empty())
            || map_value_ignore_case(&provider.model_reasoning_effort_map, model).is_some()
        {
            object.insert("reasoning_effort".into(), Value::String(effort.to_string()));
        }
    }
    if model_matches_any(model, &provider.auto_tool_choice_only_models) {
        if let Some(choice) = object.get_mut("tool_choice") {
            if choice.as_str() != Some("none") {
                *choice = Value::String("auto".into());
            }
        }
    }
    ensure_chat_function_parameters(object);
    if is_zen_chat_endpoint(&provider.base_url) {
        sanitize_zen_chat_tools(object);
    } else if is_kimi_chat_endpoint(&provider.base_url) {
        sanitize_kimi_chat_tools(object);
    } else if is_xai_chat_endpoint(&provider.base_url) {
        sanitize_xai_chat_tools(object);
    }
    Ok(())
}

fn model_supports_vision(provider: &crate::config::ProviderDefinition, model: &str) -> bool {
    map_value_ignore_case(&provider.model_input_modalities, model)
        .map(|modalities| modalities.iter().any(|value| value == "image"))
        .unwrap_or(provider.capabilities.vision)
}

fn model_matches_any(model: &str, configured: &[String]) -> bool {
    let model_folded = model.to_ascii_lowercase();
    configured.iter().any(|candidate| {
        let candidate_folded = candidate.to_ascii_lowercase();
        model_folded == candidate_folded
            || model_folded
                .strip_prefix(&candidate_folded)
                .is_some_and(|suffix| suffix.starts_with(':'))
            || candidate_folded
                .strip_prefix(&model_folded)
                .is_some_and(|suffix| suffix.starts_with(':'))
    })
}

fn map_value_ignore_case<'a, V>(
    map: &'a std::collections::BTreeMap<String, V>,
    key: &str,
) -> Option<&'a V> {
    map.get(key).or_else(|| {
        let folded = key.to_ascii_lowercase();
        map.iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(&folded))
            .map(|(_, value)| value)
    })
}

fn is_minimax_m3(model: &str) -> bool {
    model.eq_ignore_ascii_case("minimax-m3") || model.eq_ignore_ascii_case("MiniMax-M3")
}

struct AttemptFailure {
    response: Response<Body>,
    kind: AttemptFailureKind,
}

async fn send_candidate(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
) -> Result<reqwest::Response, AttemptFailure> {
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    retry_provider_request(candidate, streaming, || {
        send_candidate_once(state, body, candidate, caller_headers)
    })
    .await
}

async fn retry_provider_request<F, Fut>(
    candidate: &RouteCandidate,
    streaming: bool,
    mut operation: F,
) -> Result<reqwest::Response, AttemptFailure>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, AttemptFailure>>,
{
    let retries = if streaming {
        candidate.provider.limits.stream_retries
    } else {
        candidate.provider.limits.request_retries
    };
    for attempt in 0..=retries {
        match operation().await {
            Ok(response) => {
                let retryable = response.status() == StatusCode::REQUEST_TIMEOUT
                    || response.status() == StatusCode::TOO_MANY_REQUESTS
                    || response.status().is_server_error();
                if retryable && attempt < retries {
                    let delay = validated_retry_after(response.headers())
                        .map(|(_, delay)| delay.unwrap_or(Duration::ZERO))
                        .unwrap_or_else(|| retry_backoff(attempt));
                    let _ = read_bounded(response, 64 * 1024).await;
                    tokio::time::sleep(delay.min(Duration::from_secs(5))).await;
                    continue;
                }
                return Ok(response);
            }
            Err(failure) if failure.kind == AttemptFailureKind::Retryable && attempt < retries => {
                tokio::time::sleep(retry_backoff(attempt)).await;
            }
            Err(failure) => return Err(failure),
        }
    }
    unreachable!("bounded provider retry loop always returns")
}

fn retry_backoff(attempt: u8) -> Duration {
    Duration::from_millis(250_u64.saturating_mul(1_u64 << attempt.min(4)))
}

async fn send_candidate_once(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
) -> Result<reqwest::Response, AttemptFailure> {
    let protocol = candidate
        .provider
        .protocol_for_model(&candidate.upstream_model);
    let mut compatible_body = body.clone();
    apply_provider_request_compatibility(&mut compatible_body, candidate, protocol);
    let wire_model = wire_model_for_request(candidate, &compatible_body);
    if candidate.provider.transport == ProviderTransport::Kiro {
        return send_kiro_candidate(
            state,
            &compatible_body,
            candidate,
            caller_headers,
            &wire_model,
        )
        .await;
    }
    if candidate.provider.transport == ProviderTransport::GithubCopilot {
        return send_github_copilot_candidate(
            state,
            &compatible_body,
            candidate,
            caller_headers,
            &wire_model,
        )
        .await;
    }
    let mut upstream_body = match protocol {
        ProviderProtocol::Responses => compatible_body.clone(),
        ProviderProtocol::ChatCompletions => match responses_to_chat(&compatible_body, &wire_model)
        {
            Ok(value) => value,
            Err(message) => return Err(request_failure("unsupported_request", &message)),
        },
        ProviderProtocol::AnthropicMessages => {
            match responses_to_anthropic_with_oauth(
                &compatible_body,
                &wire_model,
                uses_anthropic_subscription_oauth(&candidate.provider),
            ) {
                Ok(value) => value,
                Err(message) => return Err(request_failure("unsupported_request", &message)),
            }
        }
        ProviderProtocol::GeminiGenerateContent => {
            match responses_to_gemini(&compatible_body, &wire_model) {
                Ok(value) => value,
                Err(message) => return Err(request_failure("unsupported_request", &message)),
            }
        }
    };
    if let Err(message) =
        apply_provider_wire_compatibility(&mut upstream_body, &compatible_body, candidate, protocol)
    {
        return Err(request_failure("unsupported_request", &message));
    }
    if protocol == ProviderProtocol::GeminiGenerateContent
        && candidate.provider.google_mode == GoogleMode::CloudCodeAssist
    {
        upstream_body =
            cloud_code_assist_envelope(upstream_body, &compatible_body, candidate, &wire_model)?;
    }
    if protocol != ProviderProtocol::GeminiGenerateContent {
        upstream_body["model"] = Value::String(wire_model.clone());
    }

    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let serialized = serde_json::to_vec(&upstream_body).map_err(|error| {
        request_failure(
            "invalid_request",
            &format!("request cannot be encoded: {error}"),
        )
    })?;
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
        request.header(
            header::USER_AGENT,
            "CODETAS/0.1 (Cloud-Code-Assist-compatible)",
        )
    } else {
        request
    };

    match request.body(serialized).send().await {
        Ok(response) => {
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

async fn send_kiro_candidate(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
    wire_model: &str,
) -> Result<reqwest::Response, AttemptFailure> {
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
    let (payload, context) = responses_to_kiro(body, wire_model, profile_arn)
        .map_err(|message| request_failure("unsupported_request", &message))?;
    let serialized = serde_json::to_vec(&payload).map_err(|error| {
        request_failure(
            "invalid_request",
            &format!("Kiro request cannot be encoded: {error}"),
        )
    })?;
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

async fn send_github_copilot_candidate(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
    wire_model: &str,
) -> Result<reqwest::Response, AttemptFailure> {
    let mut upstream_body = responses_to_chat(body, wire_model)
        .map_err(|message| request_failure("unsupported_request", &message))?;
    apply_provider_wire_compatibility(
        &mut upstream_body,
        body,
        candidate,
        ProviderProtocol::ChatCompletions,
    )
    .map_err(|message| request_failure("unsupported_request", &message))?;
    upstream_body["model"] = Value::String(wire_model.to_string());
    let serialized = serde_json::to_vec(&upstream_body).map_err(|error| {
        request_failure(
            "invalid_request",
            &format!("GitHub Copilot request cannot be encoded: {error}"),
        )
    })?;
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
        Ok(response) => Ok(response),
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

fn google_thinking_level(effort: &str) -> Option<&'static str> {
    match effort {
        "minimal" => Some("minimal"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" | "xhigh" | "max" | "ultra" => Some("high"),
        _ => None,
    }
}

fn wire_model_for_request(candidate: &RouteCandidate, request: &Value) -> String {
    let model = candidate.upstream_model.as_str();
    if candidate.provider.google_mode != GoogleMode::CloudCodeAssist {
        return candidate.provider.wire_model_id(model);
    }
    let effort = request.pointer("/reasoning/effort").and_then(Value::as_str);
    match (model, effort) {
        ("gemini-3.6-flash", Some("low")) => "gemini-3.6-flash-low".into(),
        ("gemini-3.6-flash", Some("high" | "xhigh" | "max" | "ultra")) => {
            "gemini-3.6-flash-high".into()
        }
        ("gemini-3.6-flash", _) => "gemini-3.6-flash-medium".into(),
        ("gemini-3.1-pro", Some("low")) => "gemini-3.1-pro-low".into(),
        ("gemini-3.1-pro", _) => "gemini-pro-agent".into(),
        _ => candidate.provider.wire_model_id(model),
    }
}

fn cloud_code_assist_envelope(
    mut request: Value,
    source_request: &Value,
    candidate: &RouteCandidate,
    wire_model: &str,
) -> Result<Value, AttemptFailure> {
    let project = candidate.provider.project.as_deref().ok_or_else(|| {
        request_failure(
            "provider_configuration_error",
            "Cloud Code Assist requires provider.project from the external OAuth broker",
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

async fn send_compact_candidate(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
) -> Result<reqwest::Response, AttemptFailure> {
    retry_provider_request(candidate, false, || {
        send_compact_candidate_once(state, body, candidate, caller_headers)
    })
    .await
}

async fn send_compact_candidate_once(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
) -> Result<reqwest::Response, AttemptFailure> {
    let mut body = body.clone();
    body["model"] = Value::String(candidate.provider.wire_model_id(&candidate.upstream_model));
    expand_local_compactions(&mut body);
    sanitize_responses_upstream_request(&mut body, &candidate.provider, &candidate.upstream_model);
    let serialized = serde_json::to_vec(&body).map_err(|error| {
        request_failure(
            "invalid_request",
            &format!("request cannot be encoded: {error}"),
        )
    })?;
    if serialized.len() as u64 > candidate.provider.limits.max_request_bytes {
        return Err(request_failure(
            "request_too_large",
            "compaction request exceeds the configured provider limit",
        ));
    }
    let endpoint = candidate.provider.compact_endpoint();
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
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json")
        .query(&candidate.provider.query_params)
        .timeout(Duration::from_millis(
            candidate.provider.limits.request_timeout_ms,
        ));
    let request = apply_provider_auth(request, &candidate.provider, candidate.credential.as_ref())
        .await
        .map_err(|message| AttemptFailure {
            response: error_response(
                StatusCode::UNAUTHORIZED,
                "missing_provider_credential",
                &message,
            ),
            kind: AttemptFailureKind::Credential,
        })?;
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
                    "provider response exceeds the configured limit",
                ),
                kind: AttemptFailureKind::Retryable,
            })
        }
        Ok(response) => Ok(response),
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

async fn synthetic_compact_candidate(
    state: &GatewayState,
    caller_headers: &HeaderMap,
    body: &Value,
    candidate: &RouteCandidate,
) -> Result<(Value, TokenUsage), AttemptFailure> {
    let mut request = body.clone();
    let Some(object) = request.as_object_mut() else {
        return Err(request_failure(
            "invalid_compaction_request",
            "compact request must be an object",
        ));
    };
    let input = object
        .get_mut("input")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            request_failure(
                "invalid_compaction_request",
                "compact request input must be an array",
            )
        })?;
    // `compaction_trigger` is a control marker accepted by the Responses
    // compaction endpoint. It marks where compaction should occur, but it is
    // not conversation content and cannot be represented in Chat Completions.
    // Remove it only on this dedicated compaction path; the normal Responses
    // adapter remains strict about unknown input item types.
    input.retain(|item| item.get("type").and_then(Value::as_str) != Some("compaction_trigger"));
    input.push(json!({
        "type": "message",
        "role": "developer",
        "content": [{
            "type": "input_text",
            "text": "Create a faithful compact summary of the conversation state. Preserve user requirements, decisions, constraints, file paths, completed work, unresolved work, and tool results needed to continue. Do not add commentary; output only the summary."
        }]
    }));
    object.insert("stream".into(), Value::Bool(false));
    object.insert("store".into(), Value::Bool(false));
    object.insert("tools".into(), Value::Array(Vec::new()));
    object.remove("tool_choice");
    object.remove("previous_response_id");
    object.insert(
        "max_output_tokens".into(),
        json!(candidate
            .max_output_tokens
            .unwrap_or(4_096)
            .clamp(256, 8_192)),
    );
    enforce_candidate_input_budget(&request, candidate, 0)
        .map_err(|message| request_failure("context_limit_exceeded", &message))?;

    let upstream = send_candidate(state, &request, candidate, Some(caller_headers)).await?;
    if !upstream.status().is_success() {
        let status = upstream.status();
        let response = upstream_error(upstream, None).await;
        return Err(AttemptFailure {
            response,
            kind: if status == StatusCode::REQUEST_TIMEOUT
                || status == StatusCode::TOO_MANY_REQUESTS
                || status.is_server_error()
            {
                AttemptFailureKind::Retryable
            } else {
                AttemptFailureKind::Request
            },
        });
    }
    let upstream_value = candidate_response_value(
        upstream,
        candidate,
        candidate.provider.limits.max_response_bytes,
    )
    .await
    .map_err(|message| AttemptFailure {
        response: error_response(
            StatusCode::BAD_GATEWAY,
            "invalid_provider_response",
            &message,
        ),
        kind: AttemptFailureKind::Retryable,
    })?;
    let adapted = match candidate
        .provider
        .protocol_for_model(&candidate.upstream_model)
    {
        ProviderProtocol::Responses => Ok(upstream_value),
        ProviderProtocol::ChatCompletions => {
            chat_to_response(&upstream_value, &candidate.exposed_model, &BTreeSet::new())
        }
        ProviderProtocol::AnthropicMessages => {
            anthropic_to_response(&upstream_value, &candidate.exposed_model, &BTreeSet::new())
        }
        ProviderProtocol::GeminiGenerateContent => {
            gemini_to_response(&upstream_value, &candidate.exposed_model, &BTreeSet::new())
        }
    }
    .map_err(|message| AttemptFailure {
        response: error_response(
            StatusCode::BAD_GATEWAY,
            "invalid_provider_response",
            &message,
        ),
        kind: AttemptFailureKind::Retryable,
    })?;
    let summary = response_output_text(&adapted);
    let encrypted = encode_summary(&summary).map_err(|message| AttemptFailure {
        response: error_response(
            StatusCode::BAD_GATEWAY,
            "invalid_compaction_response",
            &message,
        ),
        kind: AttemptFailureKind::Retryable,
    })?;
    let usage = TokenUsage::from_json(&adapted);
    let compacted = json!({
        "id": format!("cmpct_{}", Uuid::new_v4().simple()),
        "object": "response.compaction",
        "created_at": ObservationEvent::now_ms() / 1_000,
        "model": candidate.exposed_model,
        "output": [{
            "id": format!("cmpctitem_{}", Uuid::new_v4().simple()),
            "type": "compaction",
            "encrypted_content": encrypted,
            "created_by": "codetas"
        }],
        "usage": adapted.get("usage").cloned().unwrap_or_else(|| json!({
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "total_tokens": usage.total_tokens
        }))
    });
    Ok((compacted, usage))
}

fn ensure_single_compaction_output(value: Value, model: &str) -> Result<Value, String> {
    if compaction_item_count(&value) == 1 {
        return Ok(value);
    }
    let summary = response_output_text(&value);
    let encrypted = encode_summary(&summary)?;
    let mut wrapped = value;
    let object = wrapped
        .as_object_mut()
        .ok_or_else(|| "compaction response must be an object".to_string())?;
    object.insert("object".into(), json!("response.compaction"));
    object.insert("model".into(), json!(model));
    object.insert(
        "output".into(),
        json!([{
            "id": format!("cmpctitem_{}", Uuid::new_v4().simple()),
            "type": "compaction",
            "encrypted_content": encrypted,
            "created_by": "codetas"
        }]),
    );
    Ok(wrapped)
}

async fn send_special_candidate(
    state: &GatewayState,
    caller_headers: &HeaderMap,
    body: &Value,
    candidate: &RouteCandidate,
    kind: SpecialRelayKind,
) -> Result<reqwest::Response, AttemptFailure> {
    retry_provider_request(candidate, false, || {
        send_special_candidate_once(state, caller_headers, body, candidate, kind)
    })
    .await
}

async fn send_special_candidate_once(
    state: &GatewayState,
    caller_headers: &HeaderMap,
    body: &Value,
    candidate: &RouteCandidate,
    kind: SpecialRelayKind,
) -> Result<reqwest::Response, AttemptFailure> {
    let serialized = serde_json::to_vec(body).map_err(|error| {
        request_failure(
            "invalid_request",
            &format!("request cannot be encoded: {error}"),
        )
    })?;
    if serialized.len() as u64 > candidate.provider.limits.max_request_bytes {
        return Err(request_failure(
            "request_too_large",
            "relay request exceeds the configured provider limit",
        ));
    }
    let endpoint = kind.endpoint(candidate);
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
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json")
        .query(&candidate.provider.query_params)
        .timeout(Duration::from_millis(
            candidate.provider.limits.request_timeout_ms,
        ));
    let request = apply_provider_auth(request, &candidate.provider, candidate.credential.as_ref())
        .await
        .map_err(|message| AttemptFailure {
            response: error_response(
                StatusCode::UNAUTHORIZED,
                "missing_provider_credential",
                &message,
            ),
            kind: AttemptFailureKind::Credential,
        })?;
    let source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let request = if source == CredentialSource::Forward {
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
                    "provider response exceeds the configured limit",
                ),
                kind: AttemptFailureKind::Retryable,
            })
        }
        Ok(response) => Ok(response),
        Err(_) => Err(AttemptFailure {
            response: error_response(
                StatusCode::BAD_GATEWAY,
                "provider_unreachable",
                "provider request failed",
            ),
            kind: AttemptFailureKind::Retryable,
        }),
    }
}

async fn send_video_status_candidate(
    state: &GatewayState,
    caller_headers: &HeaderMap,
    candidate: &RouteCandidate,
    request_id: &str,
) -> Result<reqwest::Response, AttemptFailure> {
    retry_provider_request(candidate, false, || {
        send_video_status_candidate_once(state, caller_headers, candidate, request_id)
    })
    .await
}

async fn send_video_status_candidate_once(
    state: &GatewayState,
    caller_headers: &HeaderMap,
    candidate: &RouteCandidate,
    request_id: &str,
) -> Result<reqwest::Response, AttemptFailure> {
    let root = candidate
        .provider
        .video_endpoint()
        .strip_suffix("/generations")
        .unwrap_or(candidate.provider.base_url.trim().trim_end_matches('/'))
        .to_string();
    let endpoint = format!("{root}/{}", request_id);
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
        .get(endpoint)
        .header(header::ACCEPT, "application/json")
        .query(&candidate.provider.query_params)
        .timeout(Duration::from_millis(
            candidate.provider.limits.request_timeout_ms,
        ));
    let request = apply_provider_auth(request, &candidate.provider, candidate.credential.as_ref())
        .await
        .map_err(|message| AttemptFailure {
            response: error_response(
                StatusCode::UNAUTHORIZED,
                "missing_provider_credential",
                &message,
            ),
            kind: AttemptFailureKind::Credential,
        })?;
    let source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let request = if source == CredentialSource::Forward {
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
    request.send().await.map_err(|_| AttemptFailure {
        response: error_response(
            StatusCode::BAD_GATEWAY,
            "provider_unreachable",
            "video status provider was unreachable",
        ),
        kind: AttemptFailureKind::Retryable,
    })
}

async fn send_special_raw_candidate(
    state: &GatewayState,
    caller_headers: &HeaderMap,
    body: Vec<u8>,
    content_type: &str,
    candidate: &RouteCandidate,
    kind: SpecialRelayKind,
) -> Result<reqwest::Response, AttemptFailure> {
    retry_provider_request(candidate, false, || {
        send_special_raw_candidate_once(state, caller_headers, &body, content_type, candidate, kind)
    })
    .await
}

async fn send_special_raw_candidate_once(
    state: &GatewayState,
    caller_headers: &HeaderMap,
    body: &[u8],
    content_type: &str,
    candidate: &RouteCandidate,
    kind: SpecialRelayKind,
) -> Result<reqwest::Response, AttemptFailure> {
    if body.len() as u64 > candidate.provider.limits.max_request_bytes {
        return Err(request_failure(
            "request_too_large",
            "multipart relay request exceeds the configured provider limit",
        ));
    }
    let endpoint = kind.endpoint(candidate);
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
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT, "application/json")
        .query(&candidate.provider.query_params)
        .timeout(Duration::from_millis(
            candidate.provider.limits.request_timeout_ms,
        ));
    let request = apply_provider_auth(request, &candidate.provider, candidate.credential.as_ref())
        .await
        .map_err(|message| AttemptFailure {
            response: error_response(
                StatusCode::UNAUTHORIZED,
                "missing_provider_credential",
                &message,
            ),
            kind: AttemptFailureKind::Credential,
        })?;
    let source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let request = if source == CredentialSource::Forward {
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
    match request.body(body.to_vec()).send().await {
        Ok(response)
            if response
                .content_length()
                .is_some_and(|length| length > candidate.provider.limits.max_response_bytes) =>
        {
            Err(AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "provider_response_too_large",
                    "provider response exceeds the configured limit",
                ),
                kind: AttemptFailureKind::Retryable,
            })
        }
        Ok(response) => Ok(response),
        Err(_) => Err(AttemptFailure {
            response: error_response(
                StatusCode::BAD_GATEWAY,
                "provider_unreachable",
                "provider request failed",
            ),
            kind: AttemptFailureKind::Retryable,
        }),
    }
}

async fn send_realtime_candidate(
    state: &GatewayState,
    caller_headers: &HeaderMap,
    body: Vec<u8>,
    content_type: &str,
    inbound_path: &str,
    candidate: &RouteCandidate,
) -> Result<reqwest::Response, AttemptFailure> {
    retry_provider_request(candidate, false, || {
        send_realtime_candidate_once(
            state,
            caller_headers,
            &body,
            content_type,
            inbound_path,
            candidate,
        )
    })
    .await
}

async fn send_realtime_candidate_once(
    state: &GatewayState,
    caller_headers: &HeaderMap,
    body: &[u8],
    content_type: &str,
    inbound_path: &str,
    candidate: &RouteCandidate,
) -> Result<reqwest::Response, AttemptFailure> {
    if body.len() > 16 * 1024 * 1024
        || body.len() as u64 > candidate.provider.limits.max_request_bytes
    {
        return Err(request_failure(
            "request_too_large",
            "realtime request exceeds the configured provider limit",
        ));
    }
    let credential_source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let endpoint = realtime_endpoint(candidate, credential_source, inbound_path);
    let dns_pinning = state.settings.read().await.security.dns_pinning;
    let client = if dns_pinning {
        pinned_client(&endpoint, &candidate.provider, "CODETAS-Realtime/0.1")
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
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT, "application/sdp, application/json")
        .query(&candidate.provider.query_params)
        .timeout(Duration::from_millis(
            candidate.provider.limits.request_timeout_ms.min(120_000),
        ));
    let request = apply_provider_auth(request, &candidate.provider, candidate.credential.as_ref())
        .await
        .map_err(|message| AttemptFailure {
            response: error_response(
                StatusCode::UNAUTHORIZED,
                "missing_provider_credential",
                &message,
            ),
            kind: AttemptFailureKind::Credential,
        })?;
    let mut request = if credential_source == CredentialSource::Forward {
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
    for name in [
        "openai-alpha",
        "x-session-id",
        "session-id",
        "thread-id",
        "originator",
        "x-oai-attestation",
    ] {
        if let Some(value) = caller_headers.get(name) {
            request = request.header(name, value);
        }
    }
    match request.body(body.to_vec()).send().await {
        Ok(response)
            if response.content_length().is_some_and(|length| {
                length
                    > candidate
                        .provider
                        .limits
                        .max_response_bytes
                        .min(16 * 1024 * 1024)
            }) =>
        {
            Err(AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "provider_response_too_large",
                    "realtime response exceeds the configured limit",
                ),
                kind: AttemptFailureKind::Retryable,
            })
        }
        Ok(response) => Ok(response),
        Err(_) => Err(AttemptFailure {
            response: error_response(
                StatusCode::BAD_GATEWAY,
                "provider_unreachable",
                "realtime provider request failed",
            ),
            kind: AttemptFailureKind::Retryable,
        }),
    }
}

fn realtime_endpoint(
    candidate: &RouteCandidate,
    credential_source: CredentialSource,
    inbound_path: &str,
) -> String {
    let base = candidate.provider.base_url.trim().trim_end_matches('/');
    if base.contains("/backend-api") {
        return format!("{base}/realtime/calls?intent=quicksilver&architecture=avas");
    }
    if credential_source == CredentialSource::Forward && inbound_path == "/v1/live" {
        return format!("{base}/live");
    }
    let endpoint = if base.ends_with("/realtime/calls") {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{base}/realtime/calls")
    } else {
        format!("{base}/v1/realtime/calls")
    };
    format!("{endpoint}?intent=quicksilver&architecture=avas")
}

async fn bounded_response_bytes(
    response: reqwest::Response,
    limit: u64,
) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "provider response stream failed".to_string())?;
        if output.len() as u64 + chunk.len() as u64 > limit {
            return Err("provider response exceeded the configured limit".into());
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

fn request_failure(code: &str, message: &str) -> AttemptFailure {
    AttemptFailure {
        response: error_response(StatusCode::BAD_REQUEST, code, message),
        kind: AttemptFailureKind::Request,
    }
}

async fn adapt_successful_response(
    upstream: reqwest::Response,
    candidate: &RouteCandidate,
    observation: ObservationSeed,
    custom_tools: BTreeSet<String>,
    response_state: &Arc<ResponseStateStore>,
    request_body: &Value,
    record_eligible: bool,
) -> Response<Body> {
    if candidate.provider.transport == ProviderTransport::Kiro {
        return kiro_response(upstream, candidate, observation).await;
    }
    // Some Responses-compatible upstreams omit or rewrite the SSE content type even
    // after accepting `stream: true`. Trust the already-validated request intent as
    // well as the response header so an SSE body is not parsed as ordinary JSON.
    let streaming = candidate.provider.capabilities.streaming
        && (observation.streaming
            || upstream
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.contains("text/event-stream")));
    let limit = candidate.provider.limits.max_response_bytes;
    let idle_timeout = Duration::from_millis(candidate.provider.limits.stream_idle_timeout_ms);
    // Only the ChatGPT forward / stateless Responses path strips `previous_response_id`
    // (see `sanitize_responses_upstream_request`), so only there the local continuation
    // cache must record with `force` (Codex sends `store: false` on every request).
    // Requests whose own continuation could not be expanded are excluded as well.
    let force_record = candidate.provider.credential.source == CredentialSource::Forward
        || candidate.provider.stateless_responses;
    let should_record = force_record && record_eligible;
    let state_for_stream = Arc::clone(response_state);
    let body_for_stream = request_body.clone();
    match candidate
        .provider
        .protocol_for_model(&candidate.upstream_model)
    {
        ProviderProtocol::Responses if streaming => {
            if let Some(repair) =
                ResponsesItemIdRepair::new(&candidate.provider.response_item_id_repair)
            {
                repairing_responses_stream(
                    upstream,
                    limit,
                    idle_timeout,
                    observation,
                    repair,
                    state_for_stream,
                    body_for_stream,
                    should_record,
                )
            } else {
                passthrough_stream(
                    upstream,
                    limit,
                    idle_timeout,
                    observation,
                    state_for_stream,
                    body_for_stream,
                    should_record,
                )
            }
        }
        ProviderProtocol::Responses => {
            responses_json_response(
                upstream,
                limit,
                observation,
                ResponsesItemIdRepair::new(&candidate.provider.response_item_id_repair),
                Arc::clone(response_state),
                request_body.clone(),
                should_record,
            )
            .await
        }
        ProviderProtocol::ChatCompletions if streaming => translated_stream_response(
            upstream,
            candidate.exposed_model.clone(),
            StreamAdapter::Chat,
            limit,
            idle_timeout,
            observation,
            false,
            custom_tools,
        ),
        ProviderProtocol::ChatCompletions => {
            chat_json_response(
                upstream,
                &candidate.exposed_model,
                limit,
                observation,
                custom_tools,
            )
            .await
        }
        ProviderProtocol::AnthropicMessages if streaming => translated_stream_response(
            upstream,
            candidate.exposed_model.clone(),
            StreamAdapter::Anthropic { input_tokens: 0 },
            limit,
            idle_timeout,
            observation,
            candidate.provider.escape_builtin_tool_names,
            custom_tools,
        ),
        ProviderProtocol::AnthropicMessages => {
            adapted_json_response(
                upstream,
                &candidate.exposed_model,
                anthropic_to_response,
                limit,
                observation,
                candidate.provider.escape_builtin_tool_names,
                custom_tools,
            )
            .await
        }
        ProviderProtocol::GeminiGenerateContent if streaming => translated_stream_response(
            upstream,
            candidate.exposed_model.clone(),
            StreamAdapter::Gemini,
            limit,
            idle_timeout,
            observation,
            false,
            custom_tools,
        ),
        ProviderProtocol::GeminiGenerateContent => {
            adapted_json_response(
                upstream,
                &candidate.exposed_model,
                gemini_to_response,
                limit,
                observation,
                false,
                custom_tools,
            )
            .await
        }
    }
}

async fn candidate_response_value(
    upstream: reqwest::Response,
    candidate: &RouteCandidate,
    limit: u64,
) -> Result<Value, String> {
    if candidate.provider.transport != ProviderTransport::Kiro {
        return bounded_json(upstream, limit).await;
    }
    let context = upstream
        .extensions()
        .get::<KiroRequestContext>()
        .cloned()
        .ok_or_else(|| "Kiro response lost its request context".to_string())?;
    let bytes = read_bounded(upstream, limit).await?;
    kiro_eventstream_to_response(&bytes, &context, &candidate.exposed_model)
}

async fn kiro_response(
    upstream: reqwest::Response,
    candidate: &RouteCandidate,
    observation: ObservationSeed,
) -> Response<Body> {
    let status = upstream.status();
    let context = upstream.extensions().get::<KiroRequestContext>().cloned();
    let Some(context) = context else {
        observation.finish(
            StatusCode::BAD_GATEWAY,
            Some("invalid_provider_response"),
            TokenUsage::default(),
        );
        return error_response(
            StatusCode::BAD_GATEWAY,
            "invalid_provider_response",
            "Kiro response lost its request context",
        );
    };
    if context.streaming && context.completion_tool.is_none() {
        return kiro_incremental_response(upstream, candidate, observation, context);
    }
    let bytes = match read_bounded(upstream, candidate.provider.limits.max_response_bytes).await {
        Ok(bytes) => bytes,
        Err(message) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("invalid_provider_response"),
                TokenUsage::default(),
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            );
        }
    };
    let value = match kiro_eventstream_to_response(&bytes, &context, &candidate.exposed_model) {
        Ok(value) => value,
        Err(message) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("response_translation_error"),
                TokenUsage::default(),
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            );
        }
    };
    observation.finish(status, None, TokenUsage::from_json(&value));
    if !context.streaming {
        return json_response(status, value);
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
                "failed to build Kiro stream response",
            )
        })
}

fn kiro_incremental_response(
    upstream: reqwest::Response,
    candidate: &RouteCandidate,
    observation: ObservationSeed,
    context: KiroRequestContext,
) -> Response<Body> {
    let status = upstream.status();
    let limit = candidate.provider.limits.max_response_bytes;
    let idle_timeout = Duration::from_millis(candidate.provider.limits.stream_idle_timeout_ms);
    let mut source = upstream.bytes_stream();
    let mut decoder = KiroStreamDecoder::new(context, candidate.exposed_model.clone());
    let output = stream! {
        let mut completion = StreamObservation::new(observation);
        let mut usage = TokenUsage::default();
        let mut received = 0_u64;
        let mut failure = None;
        let created = decoder.created_event();
        let created_type = created.get("type").and_then(Value::as_str).unwrap_or("response.created");
        yield Ok::<Bytes, std::io::Error>(Bytes::from(sse(created_type, &created)));
        loop {
            let chunk = match tokio::time::timeout(idle_timeout, source.next()).await {
                Ok(Some(Ok(chunk))) => chunk,
                Ok(Some(Err(_))) => {
                    failure = Some("upstream_stream_error");
                    yield Err(std::io::Error::other("Kiro provider stream failed"));
                    break;
                }
                Ok(None) => break,
                Err(_) => {
                    failure = Some("stream_idle_timeout");
                    yield Err(std::io::Error::other("Kiro provider stream exceeded the idle timeout"));
                    break;
                }
            };
            received = received.saturating_add(chunk.len() as u64);
            if received > limit {
                failure = Some("provider_response_too_large");
                yield Err(std::io::Error::other("Kiro provider stream exceeded the response limit"));
                break;
            }
            match decoder.push(&chunk) {
                Ok(events) => {
                    for event in events {
                        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("message");
                        yield Ok(Bytes::from(sse(event_type, &event)));
                    }
                }
                Err(_) => {
                    failure = Some("response_translation_error");
                    yield Err(std::io::Error::other("Kiro provider stream was invalid"));
                    break;
                }
            }
        }
        if failure.is_none() {
            match decoder.finish() {
                Ok((events, response)) => {
                    usage = TokenUsage::from_json(&response);
                    for event in events {
                        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("message");
                        yield Ok(Bytes::from(sse(event_type, &event)));
                    }
                }
                Err(_) => {
                    failure = Some("response_translation_error");
                    yield Err(std::io::Error::other("Kiro provider stream ended before a valid response"));
                }
            }
        }
        completion.finish(
            if failure.is_some() { StatusCode::BAD_GATEWAY } else { status },
            failure,
            usage,
        );
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
                "failed to build incremental Kiro stream response",
            )
        })
}

fn repairing_responses_stream(
    upstream: reqwest::Response,
    limit: u64,
    idle_timeout: Duration,
    observation: ObservationSeed,
    mut repair: ResponsesItemIdRepair,
    response_state: Arc<ResponseStateStore>,
    request_body: Value,
    force_record: bool,
) -> Response<Body> {
    let status = upstream.status();
    let mut source = upstream.bytes_stream();
    let output = stream! {
        let mut completion = StreamObservation::new(observation);
        let mut usage = TokenUsage::default();
        let mut pending = Vec::new();
        let mut received = 0_u64;
        let mut failure = None;
        let mut terminal_response = None;
        let mut collected_output = Vec::new();
        loop {
            let item = match tokio::time::timeout(idle_timeout, source.next()).await {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(_) => {
                    failure = Some("stream_idle_timeout");
                    yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                        "provider stream exceeded the idle timeout",
                    ));
                    break;
                }
            };
            match item {
                Ok(bytes) => {
                    received = received.saturating_add(bytes.len() as u64);
                    if received > limit {
                        failure = Some("response_too_large");
                        yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                            "provider response exceeded the configured limit",
                        ));
                        break;
                    }
                    let values = match drain_sse_values(&mut pending, &bytes) {
                        Ok(values) => values,
                        Err(error) => {
                            failure = Some("invalid_provider_stream");
                            yield Err::<Bytes, std::io::Error>(std::io::Error::other(error));
                            break;
                        }
                    };
                    for mut value in values {
                        repair.repair_event(&mut value);
                        usage.merge_max(TokenUsage::from_json(&value));
                        match value.get("type").and_then(Value::as_str) {
                            Some("response.output_item.done") => {
                                if let Some(item) = value.get("item").cloned() {
                                    collected_output.push(item);
                                }
                            }
                            Some(
                                "response.completed" | "response.failed" | "response.incomplete",
                            ) => {
                                if terminal_response.is_none() {
                                    let mut response = value.get("response").cloned();
                                    if let Some(Value::Object(object)) = response.as_mut() {
                                        if object
                                            .get("output")
                                            .and_then(Value::as_array)
                                            .is_none_or(Vec::is_empty)
                                        {
                                            object.insert(
                                                "output".into(),
                                                Value::Array(std::mem::take(&mut collected_output)),
                                            );
                                        }
                                    }
                                    terminal_response = response;
                                }
                            }
                            _ => {}
                        }
                        let event = value
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("message");
                        yield Ok(Bytes::from(sse(event, &value)));
                    }
                }
                Err(error) => {
                    failure = Some("upstream_stream_error");
                    yield Err(std::io::Error::other(error.to_string()));
                    break;
                }
            }
        }
        if let Some(response) = terminal_response {
            response_state.remember(&request_body, &response, force_record);
        }
        if failure.is_none() && !pending.iter().all(u8::is_ascii_whitespace) {
            failure = Some("invalid_provider_stream");
            yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                "provider stream ended with an incomplete SSE frame",
            ));
        }
        completion.finish(
            if failure.is_some() { StatusCode::BAD_GATEWAY } else { status },
            failure,
            usage,
        );
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
                "failed to build response",
            )
        })
}

fn passthrough_stream(
    upstream: reqwest::Response,
    limit: u64,
    idle_timeout: Duration,
    observation: ObservationSeed,
    response_state: Arc<ResponseStateStore>,
    request_body: Value,
    force_record: bool,
) -> Response<Body> {
    let status = upstream.status();
    let mut source = upstream.bytes_stream();
    let output = stream! {
        let mut completion = StreamObservation::new(observation);
        let mut usage = TokenUsage::default();
        let mut pending = Vec::new();
        let mut received = 0_u64;
        let mut failure = None;
        let mut terminal_response = None;
        let mut collected_output = Vec::new();
        loop {
            let item = match tokio::time::timeout(idle_timeout, source.next()).await {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(_) => {
                    failure = Some("stream_idle_timeout");
                    yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                        "provider stream exceeded the idle timeout",
                    ));
                    break;
                }
            };
            match item {
                Ok(bytes) => {
                    received = received.saturating_add(bytes.len() as u64);
                    if received > limit {
                        failure = Some("response_too_large");
                        yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                            "provider response exceeded the configured limit",
                        ));
                        break;
                    }
                    let terminal = inspect_sse_usage(
                        &mut pending,
                        &bytes,
                        &mut usage,
                        &mut terminal_response,
                        &mut collected_output,
                    );
                    // Codex closes the downstream body as soon as it receives a terminal
                    // Responses event. Record success before yielding that chunk so Drop does
                    // not misclassify a completed turn as a cancelled 502.
                    if terminal {
                        completion.finish(status, None, usage.clone());
                    }
                    yield Ok(bytes);
                    if terminal {
                        break;
                    }
                }
                Err(error) => {
                    failure = Some("upstream_stream_error");
                    yield Err(std::io::Error::other(error.to_string()));
                    break;
                }
            }
        }
        if let Some(response) = terminal_response {
            response_state.remember(&request_body, &response, force_record);
        }
        completion.finish(
            if failure.is_some() { StatusCode::BAD_GATEWAY } else { status },
            failure,
            usage,
        );
    };
    Response::builder()
        .status(status)
        // This helper is only used for a validated streaming Responses request.
        // Normalize the downstream type even when the upstream mislabeled its SSE body.
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(output))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "gateway_error",
                "failed to build response",
            )
        })
}

async fn responses_json_response(
    upstream: reqwest::Response,
    limit: u64,
    observation: ObservationSeed,
    mut repair: Option<ResponsesItemIdRepair>,
    response_state: Arc<ResponseStateStore>,
    request_body: Value,
    force_record: bool,
) -> Response<Body> {
    let status = upstream.status();
    let value = match bounded_json(upstream, limit).await {
        Ok(value) => value,
        Err(message) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("invalid_provider_response"),
                TokenUsage::default(),
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            );
        }
    };
    let mut value = value;
    if let Some(repair) = repair.as_mut() {
        repair.repair_response(&mut value);
    }
    response_state.remember(&request_body, &value, force_record);
    observation.finish(status, None, TokenUsage::from_json(&value));
    json_response(status, value)
}

async fn chat_json_response(
    upstream: reqwest::Response,
    exposed_model: &str,
    limit: u64,
    observation: ObservationSeed,
    custom_tools: BTreeSet<String>,
) -> Response<Body> {
    let value = match bounded_json(upstream, limit).await {
        Ok(value) => value,
        Err(message) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("invalid_provider_response"),
                TokenUsage::default(),
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            );
        }
    };
    match chat_to_response(&value, exposed_model, &custom_tools) {
        Ok(value) => {
            observation.finish(StatusCode::OK, None, TokenUsage::from_json(&value));
            json_response(StatusCode::OK, value)
        }
        Err(message) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("response_translation_error"),
                TokenUsage::default(),
            );
            error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            )
        }
    }
}

async fn adapted_json_response(
    upstream: reqwest::Response,
    exposed_model: &str,
    adapter: fn(&Value, &str, &BTreeSet<String>) -> Result<Value, String>,
    limit: u64,
    observation: ObservationSeed,
    restore_escaped_anthropic_names: bool,
    custom_tools: BTreeSet<String>,
) -> Response<Body> {
    let mut value = match bounded_json(upstream, limit).await {
        Ok(value) => value,
        Err(message) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("invalid_provider_response"),
                TokenUsage::default(),
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            );
        }
    };
    if restore_escaped_anthropic_names {
        restore_anthropic_tool_names(&mut value);
    }
    match adapter(&value, exposed_model, &custom_tools) {
        Ok(value) => {
            observation.finish(StatusCode::OK, None, TokenUsage::from_json(&value));
            json_response(StatusCode::OK, value)
        }
        Err(message) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("response_translation_error"),
                TokenUsage::default(),
            );
            error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            )
        }
    }
}

enum StreamAdapter {
    Chat,
    Anthropic { input_tokens: u64 },
    Gemini,
}

fn translated_stream_response(
    upstream: reqwest::Response,
    exposed_model: String,
    mut adapter: StreamAdapter,
    limit: u64,
    idle_timeout: Duration,
    observation: ObservationSeed,
    restore_escaped_anthropic_names: bool,
    custom_tools: BTreeSet<String>,
) -> Response<Body> {
    let mut upstream_stream = upstream.bytes_stream();
    let output = stream! {
        let mut completion = StreamObservation::new(observation);
        let mut usage = TokenUsage::default();
        let (mut state, initial) = ChatStreamState::new(exposed_model, custom_tools);
        for event in initial {
            yield Ok::<Bytes, Infallible>(Bytes::from(event));
        }
        let mut pending = Vec::new();
        let mut failure = None;
        let mut received = 0_u64;
        'upstream: loop {
            let chunk = match tokio::time::timeout(idle_timeout, upstream_stream.next()).await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(_) => {
                    yield Ok(Bytes::from(state.fail("provider stream exceeded the idle timeout")));
                    failure = Some("stream_idle_timeout");
                    break;
                }
            };
            match chunk {
                Ok(bytes) => {
                    received = received.saturating_add(bytes.len() as u64);
                    if received > limit {
                        yield Ok(Bytes::from(state.fail("provider stream exceeded the configured limit")));
                        failure = Some("response_too_large");
                        break;
                    }
                    let values = match drain_sse_values(&mut pending, &bytes) {
                        Ok(values) => values,
                        Err(error) => {
                            yield Ok(Bytes::from(state.fail(&format!(
                                "provider returned an invalid SSE frame: {error}"
                            ))));
                            failure = Some("invalid_provider_stream");
                            break 'upstream;
                        }
                    };
                    for mut value in values {
                        if restore_escaped_anthropic_names {
                            restore_anthropic_stream_tool_names(&mut value);
                        }
                        usage.merge_max(TokenUsage::from_json(&value));
                        let converted = match &mut adapter {
                            StreamAdapter::Chat => {
                                if let Some(error) = value.get("error") {
                                    Err(error.get("message").and_then(Value::as_str).unwrap_or("provider reported a streaming error").to_string())
                                } else {
                                    Ok(Some(value))
                                }
                            }
                            StreamAdapter::Anthropic { input_tokens } => {
                                anthropic_stream_to_chat(&value, input_tokens)
                            }
                            StreamAdapter::Gemini => gemini_stream_to_chat(&value),
                        };
                        match converted {
                            Ok(Some(chunk)) => {
                                usage.merge_max(TokenUsage::from_json(&chunk));
                                let events = state.push_chat_chunk(&chunk);
                                let actionable_function_call =
                                    state.has_actionable_function_call();
                                for event in events {
                                    if actionable_function_call
                                        && translated_event_is_function_call_delta(&event)
                                    {
                                        completion.succeed_if_cancelled(usage.clone());
                                    }
                                    yield Ok(Bytes::from(event));
                                }
                            }
                            Ok(None) => {}
                            Err(message) => {
                                yield Ok(Bytes::from(state.fail(&message)));
                                failure = Some("provider_stream_error");
                                break 'upstream;
                            }
                        }
                    }
                }
                Err(error) => {
                    yield Ok(Bytes::from(state.fail(&format!("upstream stream failed: {error}"))));
                    failure = Some("upstream_stream_error");
                    break;
                }
            }
        }
        if failure.is_none() && !pending.iter().all(u8::is_ascii_whitespace) {
            yield Ok(Bytes::from(state.fail("provider stream ended with an incomplete SSE frame")));
            failure = Some("invalid_provider_stream");
        } else if failure.is_none() {
            for event in state.finish() {
                yield Ok(Bytes::from(event));
            }
        }
        completion.finish(
            if failure.is_some() { StatusCode::BAD_GATEWAY } else { StatusCode::OK },
            failure,
            usage,
        );
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(output))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "gateway_error",
                "failed to build stream",
            )
        })
}

fn translated_event_is_function_call_delta(event: &str) -> bool {
    event
        .lines()
        .any(|line| line.trim() == "event: response.function_call_arguments.delta")
}

struct StreamObservation {
    seed: Option<ObservationSeed>,
    cancelled_outcome: Option<(StatusCode, TokenUsage)>,
}

impl StreamObservation {
    fn new(seed: ObservationSeed) -> Self {
        Self {
            seed: Some(seed),
            cancelled_outcome: None,
        }
    }

    fn succeed_if_cancelled(&mut self, usage: TokenUsage) {
        self.cancelled_outcome = Some((StatusCode::OK, usage));
    }

    fn finish(&mut self, status: StatusCode, failure_category: Option<&str>, usage: TokenUsage) {
        if let Some(seed) = self.seed.take() {
            seed.finish(status, failure_category, usage);
        }
    }
}

impl Drop for StreamObservation {
    fn drop(&mut self) {
        if let Some(seed) = self.seed.take() {
            if let Some((status, usage)) = self.cancelled_outcome.take() {
                seed.finish(status, None, usage);
            } else {
                seed.finish(
                    StatusCode::BAD_GATEWAY,
                    Some("stream_cancelled"),
                    TokenUsage::default(),
                );
            }
        }
    }
}

fn inspect_sse_usage(
    pending: &mut Vec<u8>,
    bytes: &[u8],
    usage: &mut TokenUsage,
    terminal_response: &mut Option<Value>,
    collected_output: &mut Vec<Value>,
) -> bool {
    match drain_sse_values(pending, bytes) {
        Ok(values) => {
            let mut terminal = false;
            for value in values {
                usage.merge_max(TokenUsage::from_json(&value));
                match value.get("type").and_then(Value::as_str) {
                    Some("response.output_item.done") => {
                        if let Some(item) = value.get("item").cloned() {
                            collected_output.push(item);
                        }
                    }
                    Some("response.completed" | "response.failed" | "response.incomplete") => {
                        terminal = true;
                        if terminal_response.is_none() {
                            let mut response = value.get("response").cloned();
                            if let Some(Value::Object(object)) = response.as_mut() {
                                if object
                                    .get("output")
                                    .and_then(Value::as_array)
                                    .is_none_or(Vec::is_empty)
                                {
                                    object.insert(
                                        "output".into(),
                                        Value::Array(std::mem::take(collected_output)),
                                    );
                                }
                            }
                            *terminal_response = response;
                        }
                    }
                    _ => {}
                }
            }
            terminal
        }
        Err(_) => {
            pending.clear();
            false
        }
    }
}

fn drain_sse_values(pending: &mut Vec<u8>, bytes: &[u8]) -> Result<Vec<Value>, String> {
    pending.extend_from_slice(bytes);
    let mut values = Vec::new();
    while let Some((boundary, separator_len)) = sse_frame_boundary(pending) {
        let frame = pending.drain(..boundary).collect::<Vec<_>>();
        pending.drain(..separator_len);
        let frame =
            std::str::from_utf8(&frame).map_err(|_| "SSE frame is not valid UTF-8".to_string())?;
        let normalized = frame.replace("\r\n", "\n");
        let data = normalized
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        values.push(
            serde_json::from_str::<Value>(&data)
                .map_err(|error| format!("invalid JSON data: {error}"))?,
        );
    }
    Ok(values)
}

fn sse_frame_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let lf = bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2));
    let crlf = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4));
    match (lf, crlf) {
        (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
        (Some(boundary), None) | (None, Some(boundary)) => Some(boundary),
        (None, None) => None,
    }
}

fn validated_retry_after(headers: &HeaderMap) -> Option<(HeaderValue, Option<Duration>)> {
    const MAX_COOLDOWN: Duration = Duration::from_secs(10 * 60);
    let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
    if value.is_empty() || value.len() > 128 {
        return None;
    }
    let header_value = HeaderValue::from_str(value).ok()?;
    if value == "0" {
        return Some((header_value, None));
    }
    if value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        let seconds = value.parse::<f64>().ok()?;
        if !seconds.is_finite() || seconds <= 0.0 {
            return None;
        }
        let millis = (seconds * 1_000.0).ceil().clamp(1.0, 600_000.0) as u64;
        return Some((
            header_value,
            Some(Duration::from_millis(millis).min(MAX_COOLDOWN)),
        ));
    }
    let retry_at = httpdate::parse_http_date(value).ok()?;
    let delay = retry_at.duration_since(SystemTime::now()).ok()?;
    if delay.is_zero() {
        return None;
    }
    Some((header_value, Some(delay.min(MAX_COOLDOWN))))
}

async fn upstream_error(
    upstream: reqwest::Response,
    retry_after: Option<&HeaderValue>,
) -> Response<Body> {
    let status = upstream.status();
    // Drain a bounded body, but never reflect provider text. Some upstreams
    // echo prompts, headers, or credentials in diagnostic responses.
    let body = read_bounded(upstream, 64 * 1024).await.unwrap_or_default();
    if status.as_u16() == 400 {
        record_upstream_error(status, &body);
    }
    let mut response = error_response(
        status,
        "provider_error",
        &format!("provider request failed with HTTP {}", status.as_u16()),
    );
    if let Some(retry_after) = retry_after {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, retry_after.clone());
    }
    response
}

/// Temporary opt-in diagnostic: appends bounded upstream 400 bodies to a local
/// file when CODETAS_LOG_UPSTREAM_ERRORS is set. Never affects responses.
fn record_upstream_error(status: reqwest::StatusCode, body: &[u8]) {
    if std::env::var_os("CODETAS_LOG_UPSTREAM_ERRORS").is_none() {
        return;
    }
    let Ok(text) = std::str::from_utf8(body) else {
        return;
    };
    let mut text: String = text.chars().take(2048).collect();
    if text.trim().is_empty() {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    text.push('\n');
    let entry = format!("[{now}] HTTP {} {text}", status.as_u16());
    let path = std::env::temp_dir().join("codetas-upstream-errors.log");
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        let _ = file.write_all(entry.as_bytes());
    }
}

async fn bounded_json(upstream: reqwest::Response, limit: u64) -> Result<Value, String> {
    let bytes = read_bounded(upstream, limit).await?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("provider returned invalid JSON: {error}"))
}

async fn read_bounded(upstream: reqwest::Response, limit: u64) -> Result<Vec<u8>, String> {
    let mut stream = upstream.bytes_stream();
    let mut output = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("provider response failed: {error}"))?;
        if output.len() as u64 + chunk.len() as u64 > limit {
            return Err("provider response exceeded the configured limit".into());
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

fn json_response(status: StatusCode, value: Value) -> Response<Body> {
    let body = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn error_response(status: StatusCode, code: &str, message: &str) -> Response<Body> {
    json_response(
        status,
        json!({
            "error": {
                "type": "codetas_gateway_error",
                "code": code,
                "message": message
            }
        }),
    )
}

#[cfg(test)]
mod subagent_shrink_tests {
    use super::*;
    use crate::config::ProviderDefinition;

    fn candidate(context: u64) -> RouteCandidate {
        RouteCandidate {
            provider: ProviderDefinition::default(),
            upstream_model: "kimi/k3".into(),
            exposed_model: "kimi/k3".into(),
            credential: None,
            account_id: None,
            target_key: "test".into(),
            route_id: None,
            failure_threshold: 0,
            quota_threshold_percent: 0,
            input_price_per_million: None,
            output_price_per_million: None,
            context_window: Some(context),
            max_input_tokens: None,
            max_output_tokens: None,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
        }
    }

    fn input_with(count: usize) -> Value {
        let mut items = Vec::new();
        for i in 0..count {
            items.push(json!({"type": "reasoning", "id": format!("rs_{i}"), "summary": "x".repeat(50)}));
            items.push(json!({"type": "custom_tool_call_output", "call_id": format!("c_{i}"), "output": "y".repeat(50)}));
        }
        json!({"input": items})
    }

    #[test]
    fn shrinks_when_over_budget() {
        // 大きな input を小さい context に
        let mut body = input_with(20);
        let cand = candidate(300);
        let changed = shrink_subagent_input(&mut body, &cand, 0).unwrap();
        assert!(changed);
        assert!(estimate_input_items_tokens(body["input"].as_array().unwrap()) <= 300);
    }

    #[test]
    fn keeps_within_budget_untouched() {
        let mut body = input_with(2);
        let cand = candidate(200_000);
        let changed = shrink_subagent_input(&mut body, &cand, 0).unwrap();
        assert!(!changed);
        assert_eq!(body["input"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn drops_reasoning_before_tool_outputs() {
        let mut body = input_with(10);
        let cand = candidate(4_000);
        // 半分程度まで縮小される
        shrink_subagent_input(&mut body, &cand, 0).unwrap();
        let items = body["input"].as_array().unwrap();
        let reasoning_left = items.iter().filter(|i| i.get("type").and_then(Value::as_str) == Some("reasoning")).count();
        let outputs_left = items.iter().filter(|i| i.get("type").and_then(Value::as_str) == Some("custom_tool_call_output")).count();
        // reasoning を優先的に落とす
        assert!(reasoning_left <= outputs_left);
    }
}
