use crate::{
    anthropic::{
        anthropic_stream_to_chat, anthropic_subscription_oauth_headers, anthropic_to_response,
        anthropic_to_response_with_oauth, responses_to_anthropic_with_oauth,
        AnthropicStreamState,
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
    gemini::{
        gemini_finish_disposition, gemini_stream_to_chat, gemini_to_response,
        responses_to_gemini, GeminiFinishDisposition,
    },
    kiro::{
        kiro_eventstream_to_response, omit_oldest_kiro_wire_image, responses_to_kiro,
        KiroRequestContext, KiroStreamDecoder,
    },
    network::pinned_client,
    observability::{ObservabilityLedger, ObservabilitySummary, ObservationEvent, TokenUsage},
    oauth::{resolve_antigravity_cloud_project, resolve_oauth_access_token},
    response_state::ResponseStateStore,
    routing::{RouteCandidate, RouteDryRunReport, RoutingRuntime},
    translate::{
        chat_to_response, count_translated_input_images, normalize_chat_reasoning_history,
        normalize_responses_tool_result_adjacency, normalize_translated_image_history,
        omit_oldest_translated_input_image, prepare_translated_responses_request,
        response_tool_map, responses_to_chat_with_options, sse, strip_translated_input_images,
        strip_translated_input_images_for_compaction, ChatStreamState, ResponseToolMap,
        ToolProgressPolicy,
    },
};
use async_stream::stream;
use axum::{
    body::{to_bytes, Body},
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path as AxumPath, Query, Request, State,
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
    collections::{HashMap, HashSet},
    convert::Infallible,
    fs::{self, File, OpenOptions},
    io::Write,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    pin::Pin,
    sync::{atomic::{AtomicU32, AtomicU64, Ordering}, Arc},
    task::{Context, Poll},
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
const MAX_PROVIDER_PACING_KEYS: usize = 4_096;

#[derive(Default)]
pub(crate) struct ProviderPacingState {
    next_slots: HashMap<String, Instant>,
    reservations: u64,
}

#[derive(Clone)]
pub(crate) struct GatewayState {
    settings: SharedSettings,
    client: reqwest::Client,
    routing: Arc<Mutex<RoutingRuntime>>,
    observability: ObservabilityLedger,
    video_jobs: Arc<Mutex<HashMap<String, VideoJobRecord>>>,
    instance_id: String,
    response_state: Arc<ResponseStateStore>,
    memory: Arc<MemoryAdmission>,
    pacing: Arc<Mutex<ProviderPacingState>>,
}

pub(crate) struct MemoryAdmission {
    settings: SharedSettings,
    inflight: AtomicU32,
    reserved_bytes: AtomicU64,
    rejected: AtomicU64,
}

pub(crate) struct MemoryReservation {
    memory: Arc<MemoryAdmission>,
    bytes: u64,
}

struct AdmissionGuardedBody {
    inner: Body,
    reservation: Option<MemoryReservation>,
}

impl http_body::Body for AdmissionGuardedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match http_body::Body::poll_frame(Pin::new(&mut this.inner), cx) {
            Poll::Ready(None) => {
                this.reservation.take();
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(error))) => {
                this.reservation.take();
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(Some(Ok(frame))) => {
                if http_body::Body::is_end_stream(&this.inner) {
                    this.reservation.take();
                }
                Poll::Ready(Some(Ok(frame)))
            }
            other => other,
        }
    }

    fn is_end_stream(&self) -> bool {
        http_body::Body::is_end_stream(&self.inner)
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::Body::size_hint(&self.inner)
    }
}

impl Drop for MemoryReservation {
    fn drop(&mut self) {
        self.memory.inflight.fetch_sub(1, Ordering::AcqRel);
        self.memory.reserved_bytes.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

impl MemoryReservation {
    fn grow(&mut self, bytes: u64, budget: u64) -> bool {
        if bytes == 0 {
            return true;
        }
        let reserved = self.memory.reserved_bytes.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |current| current.checked_add(bytes).filter(|next| *next <= budget),
        );
        if reserved.is_ok() {
            self.bytes = self.bytes.saturating_add(bytes);
            true
        } else {
            false
        }
    }
}

pub(crate) async fn reserve_websocket_turn_memory(
    memory: &Arc<MemoryAdmission>,
    body_bytes: u64,
) -> Result<MemoryReservation, &'static str> {
    let (budget, max_inflight) = {
        let settings = memory.settings.read().await;
        (settings.runtime.memory_budget_bytes, settings.runtime.max_inflight_requests)
    };
    let inflight = memory.inflight.fetch_add(1, Ordering::AcqRel).saturating_add(1);
    if inflight > max_inflight {
        memory.inflight.fetch_sub(1, Ordering::AcqRel);
        memory.rejected.fetch_add(1, Ordering::Relaxed);
        return Err("gateway inflight request limit reached");
    }
    let mut reservation = MemoryReservation {
        memory: Arc::clone(memory),
        bytes: 0,
    };
    let requested = 1024_u64
        .saturating_mul(1024)
        .saturating_add(body_bytes.saturating_mul(3));
    if !reservation.grow(requested, budget) {
        memory.rejected.fetch_add(1, Ordering::Relaxed);
        return Err("gateway request memory budget reached");
    }
    Ok(reservation)
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

    pub async fn route_dry_run(
        &self,
        model: &str,
        is_subagent: bool,
    ) -> Result<RouteDryRunReport, String> {
        let require_local_token = self.settings.read().await.security.require_local_token;
        let client = reqwest::Client::new();
        let mut request = client
            .get(format!("{}/routes/dry-run", self.url()))
            .query(&[("model", model), ("isSubagent", if is_subagent { "true" } else { "false" })]);
        if require_local_token {
            let token = std::env::var("CODETAS_GATEWAY_TOKEN")
                .map_err(|_| "CODETAS_GATEWAY_TOKEN is required for route dry-run".to_string())?;
            request = request.header("x-codetas-token", token);
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("live gateway route dry-run is unavailable: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "live gateway route dry-run failed with status {}",
                response.status()
            ));
        }
        response
            .json::<RouteDryRunReport>()
            .await
            .map_err(|error| format!("live gateway route dry-run returned invalid JSON: {error}"))
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
    let response_state_path = options
        .auth_store_path
        .as_ref()
        .or(options.runtime_state_path.as_ref())
        .map(|path| path.with_file_name("response-state.json"));
    let instance_id = Uuid::new_v4().to_string();
    let routing = Arc::new(Mutex::new(RoutingRuntime::default()));
    let memory = Arc::new(MemoryAdmission {
        settings: Arc::clone(&shared), inflight: AtomicU32::new(0),
        reserved_bytes: AtomicU64::new(0), rejected: AtomicU64::new(0),
    });
    let pacing = Arc::new(Mutex::new(ProviderPacingState::default()));
    let state = GatewayState {
        settings: Arc::clone(&shared),
        client,
        routing: Arc::clone(&routing),
        observability: observability.clone(),
        video_jobs: Arc::new(Mutex::new(HashMap::new())),
        instance_id: instance_id.clone(),
        response_state: Arc::new(ResponseStateStore::new(response_state_path)),
        memory: Arc::clone(&memory),
        pacing,
    };
    let router = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(readiness))
        .route("/v1/compatibility", get(compatibility_lab))
        .route("/v1/routes/dry-run", get(route_dry_run))
        .route("/v1/management/memory", get(memory_status))
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
        .route("/v1/sidecars/config", get(media_sidecar_config))
        .route("/v1/sidecars/video-analysis", post(video_analysis_sidecar))
        .route("/v1/sidecars/document", post(document_sidecar))
        .route("/v1/sidecars/image", post(image_sidecar))
        .route("/v1/sidecars/video", post(video_sidecar))
        .route("/v1/sidecars/video/{request_id}", get(video_sidecar_status))
        .route("/v1/live", post(realtime_call_create))
        .route("/v1/realtime/calls", post(realtime_call_create))
        .route("/v1/live/{call_id}", get(realtime_live_sideband))
        .route("/v1/realtime/calls/{call_id}", get(realtime_calls_sideband))
        .route("/v1/realtime", get(realtime_query_sideband))
        .with_state(state)
        // Admission is inside decompression, so it observes and accounts the
        // actual decoded/chunked body rather than trusting Content-Length. It is
        // also the sole body limit, allowing field-scoped runtime updates to take
        // effect without restarting the embedded gateway.
        .layer(middleware::from_fn_with_state(
            Arc::clone(&memory),
            memory_admission_middleware,
        ))
        // Codex enables zstd request compression by default. This outer layer
        // decodes before the admission middleware collects the bounded body.
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

async fn memory_admission_middleware(
    State(memory): State<Arc<MemoryAdmission>>,
    request: Request,
    next: Next,
) -> Response<Body> {
    if memory_admission_exempt_path(request.uri().path()) {
        return next.run(request).await;
    }
    let (budget, max_inflight) = {
        let settings = memory.settings.read().await;
        (settings.runtime.memory_budget_bytes, settings.runtime.max_inflight_requests)
    };
    let inflight = memory.inflight.fetch_add(1, Ordering::AcqRel).saturating_add(1);
    if inflight > max_inflight {
        memory.inflight.fetch_sub(1, Ordering::AcqRel);
        memory.rejected.fetch_add(1, Ordering::Relaxed);
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "gateway_capacity", "gateway inflight request limit reached");
    }
    let mut reservation = MemoryReservation {
        memory: Arc::clone(&memory),
        bytes: 0,
    };
    if !reservation.grow(1024 * 1024, budget) {
        memory.rejected.fetch_add(1, Ordering::Relaxed);
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "gateway_memory_budget", "gateway request memory budget reached");
    }
    let body_limit = configured_body_limit_bytes(budget);
    let request = match collect_admitted_request_body(
        &memory,
        &mut reservation,
        request,
        budget,
        body_limit,
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return response,
    };
    let response = next.run(request).await;
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        Body::new(AdmissionGuardedBody {
            inner: body,
            reservation: Some(reservation),
        }),
    )
}

fn configured_body_limit_bytes(memory_budget_bytes: u64) -> u64 {
    memory_budget_bytes
        .saturating_div(2)
        .clamp(1024 * 1024, 128 * 1024 * 1024)
}

async fn collect_admitted_request_body(
    memory: &Arc<MemoryAdmission>,
    reservation: &mut MemoryReservation,
    request: Request,
    budget: u64,
    body_limit: u64,
) -> Result<Request, Response<Body>> {
    let (mut parts, body) = request.into_parts();
    let mut stream = body.into_data_stream();
    let mut chunks = Vec::new();
    let mut body_bytes = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                memory.rejected.fetch_add(1, Ordering::Relaxed);
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_body",
                    "request body could not be decoded",
                ));
            }
        };
        body_bytes = body_bytes.saturating_add(chunk.len() as u64);
        if body_bytes > body_limit {
            memory.rejected.fetch_add(1, Ordering::Relaxed);
            return Err(error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "decoded request body exceeds the configured limit",
            ));
        }
        if !reservation.grow((chunk.len() as u64).saturating_mul(3), budget) {
            memory.rejected.fetch_add(1, Ordering::Relaxed);
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "gateway_memory_budget",
                "gateway request memory budget reached",
            ));
        }
        chunks.push(chunk);
    }
    let mut decoded = Vec::with_capacity(body_bytes as usize);
    for chunk in chunks {
        decoded.extend_from_slice(&chunk);
    }
    parts.headers.remove(header::TRANSFER_ENCODING);
    if let Ok(length) = HeaderValue::from_str(&body_bytes.to_string()) {
        parts.headers.insert(header::CONTENT_LENGTH, length);
    }
    Ok(Request::from_parts(parts, Body::from(decoded)))
}

fn memory_admission_exempt_path(path: &str) -> bool {
    matches!(path, "/healthz" | "/readyz")
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
            Self::ImageGeneration | Self::ImageEdit => candidate.capabilities.image_generation,
            Self::Search => candidate.capabilities.web_search,
            Self::VideoGeneration => candidate.capabilities.video_generation,
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

#[cfg(test)]
mod memory_admission_tests {
    use super::*;
    use base64::Engine as _;
    use http_body_util::{BodyExt, StreamBody};
    use tower::ServiceExt;

    fn memory() -> Arc<MemoryAdmission> {
        Arc::new(MemoryAdmission {
            settings: Arc::new(RwLock::new(GatewaySettings::default())),
            inflight: AtomicU32::new(0),
            reserved_bytes: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        })
    }

    fn begin_test_reservation(
        memory: &Arc<MemoryAdmission>,
        budget: u64,
    ) -> MemoryReservation {
        memory.inflight.fetch_add(1, Ordering::AcqRel);
        let mut reservation = MemoryReservation {
            memory: Arc::clone(memory),
            bytes: 0,
        };
        assert!(reservation.grow(1024 * 1024, budget));
        reservation
    }

    #[test]
    fn liveness_and_readiness_bypass_inference_memory_admission() {
        assert!(memory_admission_exempt_path("/healthz"));
        assert!(memory_admission_exempt_path("/readyz"));
        assert!(!memory_admission_exempt_path("/v1/responses"));
        assert!(!memory_admission_exempt_path("/v1/management/memory"));
        assert!(!memory_admission_exempt_path("/healthz/extra"));
    }

    #[tokio::test]
    async fn decoded_body_size_overrides_a_misleading_compressed_content_length() {
        let memory = memory();
        let budget = 4 * 1024 * 1024;
        let mut reservation = begin_test_reservation(&memory, budget);
        let request = Request::builder()
            .uri("/v1/responses")
            .header(header::CONTENT_LENGTH, "32")
            .body(Body::from(vec![b'x'; 2 * 1024 * 1024]))
            .expect("request");

        let result = collect_admitted_request_body(
            &memory,
            &mut reservation,
            request,
            budget,
            3 * 1024 * 1024,
        )
        .await;

        assert!(result.is_err());
        assert_eq!(memory.rejected.load(Ordering::Acquire), 1);
        drop(reservation);
        assert_eq!(memory.inflight.load(Ordering::Acquire), 0);
        assert_eq!(memory.reserved_bytes.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn decompression_layer_runs_before_decoded_memory_admission() {
        let mut settings = GatewaySettings::default();
        settings.runtime.memory_budget_bytes = 4 * 1024 * 1024;
        let memory = Arc::new(MemoryAdmission {
            settings: Arc::new(RwLock::new(settings)),
            inflight: AtomicU32::new(0),
            reserved_bytes: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        });
        let app = Router::new()
            .route("/v1/responses", post(|| async { StatusCode::OK }))
            .layer(middleware::from_fn_with_state(
                Arc::clone(&memory),
                memory_admission_middleware,
            ))
            .layer(RequestDecompressionLayer::new());
        // A roughly 2 KiB gzip frame that expands to 2 MiB. With a 4 MiB
        // budget, decoded accounting reserves 1 MiB baseline plus 6 MiB body
        // headroom and must reject it. Compressed-size accounting would pass.
        let compressed = STANDARD.decode(concat!(
            "H4sIAAAAAAAC/+3BMQEAAADCoNqLbwwfoAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAC+Bv9PKHkAACAA"
        ))
        .expect("gzip fixture");
        let compressed_len = compressed.len();
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/responses")
                    .header(header::CONTENT_ENCODING, "gzip")
                    .header(header::CONTENT_LENGTH, compressed_len.to_string())
                    .body(Body::from(compressed))
                    .expect("compressed request"),
            )
            .await
            .expect("router response");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(memory.rejected.load(Ordering::Acquire), 1);
        assert_eq!(memory.inflight.load(Ordering::Acquire), 0);
        assert_eq!(memory.reserved_bytes.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn streaming_response_holds_admission_until_the_body_is_dropped() {
        let memory = memory();
        let app = Router::new()
            .route(
                "/v1/responses",
                post(|| async {
                    Response::new(Body::from_stream(futures_util::stream::pending::<
                        Result<Bytes, Infallible>,
                    >()))
                }),
            )
            .layer(middleware::from_fn_with_state(
                Arc::clone(&memory),
                memory_admission_middleware,
            ));

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/responses")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("streaming response");

        assert_eq!(memory.inflight.load(Ordering::Acquire), 1);
        assert_eq!(memory.reserved_bytes.load(Ordering::Acquire), 1024 * 1024);
        drop(response);
        assert_eq!(memory.inflight.load(Ordering::Acquire), 0);
        assert_eq!(memory.reserved_bytes.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn websocket_turn_reservation_counts_and_releases_like_http_inference() {
        let memory = memory();
        let reservation = reserve_websocket_turn_memory(&memory, 128)
            .await
            .expect("websocket turn admission");
        assert_eq!(memory.inflight.load(Ordering::Acquire), 1);
        assert_eq!(
            memory.reserved_bytes.load(Ordering::Acquire),
            1024 * 1024 + 128 * 3
        );
        drop(reservation);
        assert_eq!(memory.inflight.load(Ordering::Acquire), 0);
        assert_eq!(memory.reserved_bytes.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn guarded_body_preserves_trailers_and_releases_on_stream_end() {
        let memory = memory();
        let reservation = begin_test_reservation(&memory, 4 * 1024 * 1024);
        let mut trailers = HeaderMap::new();
        trailers.insert("x-fixture-trailer", HeaderValue::from_static("present"));
        let frames = futures_util::stream::iter(vec![Ok::<_, Infallible>(
            http_body::Frame::trailers(trailers),
        )]);
        let inner = Body::new(StreamBody::new(frames));
        let mut body = Body::new(AdmissionGuardedBody {
            inner,
            reservation: Some(reservation),
        });

        let frame = body
            .frame()
            .await
            .expect("trailer frame")
            .expect("valid frame");
        assert_eq!(
            frame
                .trailers_ref()
                .and_then(|headers| headers.get("x-fixture-trailer"))
                .and_then(|value| value.to_str().ok()),
            Some("present")
        );
        assert!(body.frame().await.is_none());
        assert_eq!(memory.inflight.load(Ordering::Acquire), 0);
        assert_eq!(memory.reserved_bytes.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn guarded_body_preserves_non_streaming_data_and_releases_at_end() {
        let memory = memory();
        let reservation = begin_test_reservation(&memory, 4 * 1024 * 1024);
        let mut body = Body::new(AdmissionGuardedBody {
            inner: Body::from("fixture response"),
            reservation: Some(reservation),
        });

        let frame = body
            .frame()
            .await
            .expect("data frame")
            .expect("valid frame");
        assert_eq!(
            frame.data_ref().map(|bytes| bytes.as_ref()),
            Some(b"fixture response".as_slice())
        );
        assert!(body.frame().await.is_none());
        assert_eq!(memory.inflight.load(Ordering::Acquire), 0);
        assert_eq!(memory.reserved_bytes.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn guarded_body_preserves_errors_and_releases_immediately() {
        let memory = memory();
        let reservation = begin_test_reservation(&memory, 4 * 1024 * 1024);
        let errors = futures_util::stream::iter(vec![Err::<Bytes, _>(std::io::Error::other(
            "fixture stream failure",
        ))]);
        let mut body = Body::new(AdmissionGuardedBody {
            inner: Body::from_stream(errors),
            reservation: Some(reservation),
        });

        let error = body
            .frame()
            .await
            .expect("error frame")
            .expect_err("stream error must survive the guard");
        assert!(error.to_string().contains("fixture stream failure"));
        assert_eq!(memory.inflight.load(Ordering::Acquire), 0);
        assert_eq!(memory.reserved_bytes.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn parallel_chunked_bodies_share_the_atomic_decoded_budget() {
        let memory = memory();
        let budget = 5 * 1024 * 1024;
        let mut first_reservation = begin_test_reservation(&memory, budget);
        let mut second_reservation = begin_test_reservation(&memory, budget);
        let chunked_request = || {
            let chunks = futures_util::stream::iter(vec![Ok::<Bytes, Infallible>(
                Bytes::from(vec![b'x'; 1024 * 1024]),
            )]);
            Request::builder()
                .uri("/v1/responses")
                .body(Body::from_stream(chunks))
                .expect("chunked request")
        };

        let (first, second) = tokio::join!(
            collect_admitted_request_body(
                &memory,
                &mut first_reservation,
                chunked_request(),
                budget,
                2 * 1024 * 1024,
            ),
            collect_admitted_request_body(
                &memory,
                &mut second_reservation,
                chunked_request(),
                budget,
                2 * 1024 * 1024,
            )
        );

        assert_ne!(first.is_ok(), second.is_ok());
        assert_eq!(memory.rejected.load(Ordering::Acquire), 1);
        drop(first);
        drop(second);
        drop(first_reservation);
        drop(second_reservation);
        assert_eq!(memory.inflight.load(Ordering::Acquire), 0);
        assert_eq!(memory.reserved_bytes.load(Ordering::Acquire), 0);
    }
}
