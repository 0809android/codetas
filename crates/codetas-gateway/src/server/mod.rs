use crate::{
    anthropic::{
        anthropic_stream_to_chat, anthropic_subscription_oauth_headers, anthropic_to_response,
        anthropic_to_response_with_oauth, responses_to_anthropic_with_oauth,
        uses_anthropic_subscription_oauth,
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
        is_xai_chat_endpoint, is_zen_chat_endpoint, restore_anthropic_stream_tool_names,
        restore_anthropic_tool_names, sanitize_kimi_chat_tools,
        sanitize_responses_upstream_request, sanitize_xai_chat_tools, sanitize_zen_chat_tools,
        ResponsesItemIdRepair,
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
        chat_to_response, normalize_chat_reasoning_history, prepare_translated_responses_request,
        response_tool_map, responses_to_chat, sse, strip_translated_input_images,
        ChatStreamState, ResponseToolMap, ToolProgressPolicy,
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
    collections::HashMap,
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

mod adapt;
mod admission;
mod compact;
mod compatibility;
mod image;
mod media;
mod protocol;
mod realtime;
mod requests;
mod response;
mod senders;
mod shadow;
mod sidecar;
mod special;
mod stream;
mod websocket;

pub(crate) use adapt::*;
pub(crate) use admission::*;
pub(crate) use compact::*;
pub(crate) use compatibility::*;
pub(crate) use image::*;
pub(crate) use media::*;
pub(crate) use protocol::*;
pub(crate) use realtime::*;
pub(crate) use requests::*;
pub(crate) use response::*;
pub(crate) use senders::*;
pub(crate) use shadow::*;
pub(crate) use sidecar::*;
pub(crate) use special::*;
pub(crate) use stream::*;
pub(crate) use websocket::*;

pub type SharedSettings = Arc<RwLock<GatewaySettings>>;

#[derive(Clone)]
pub(crate) struct GatewayState {
    settings: SharedSettings,
    client: reqwest::Client,
    routing: Arc<Mutex<RoutingRuntime>>,
    observability: ObservabilityLedger,
    video_jobs: Arc<Mutex<HashMap<String, VideoJobRecord>>>,
    instance_id: String,
    response_state: Arc<ResponseStateStore>,
}

#[derive(Clone)]
pub(crate) struct VideoJobRecord {
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
pub(crate) enum SpecialRelayKind {
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
