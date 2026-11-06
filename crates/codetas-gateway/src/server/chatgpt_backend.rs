//! `chatgpt_base_url` endpoints.
//!
//! Codex asks `<chatgpt_base_url>/wham/usage` for the ChatGPT plan's rate
//! limits and disables the composer once the plan is exhausted, before any
//! model request reaches the gateway. Pointing `chatgpt_base_url` here lets the
//! gateway answer that one probe with an "unlimited" payload (requests are
//! routed by CODETAS, so the ChatGPT plan quota is not the gate) and relay every
//! other ChatGPT backend call (plugins, connectors, thread estimates, ...)
//! unchanged to chatgpt.com so those features keep working.
//!
//! CODETAS no longer writes that setting. Codex 0.166 refuses a workspace backend
//! that is not HTTPS, and `account/read` then fails for the whole app. These routes
//! only serve a Codex that an older build or the user still points here.
//!
//! These routes never use gateway secrets: the upstream host is fixed and only
//! the caller's own ChatGPT credentials are forwarded. On a loopback bind that is
//! why the relay skips the CODETAS admission token: Codex does not send it to this
//! base. A gateway reachable from other hosts must not relay for anyone who asks,
//! so there the relay is admitted like every other route, with the token in
//! `x-codetas-token` because `Authorization` carries the ChatGPT bearer.

use super::*;

const CHATGPT_BACKEND_HOST: &str = "chatgpt.com";
const CHATGPT_BACKEND_BASE: &str = "https://chatgpt.com/backend-api";
const MAX_BACKEND_REQUEST_BYTES: usize = 64 * 1024 * 1024;
const RELAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Response headers that are safe and useful to hand back to Codex.
const RELAYED_RESPONSE_HEADERS: &[&str] = &[
    "content-type",
    "content-disposition",
    "cache-control",
    "etag",
    "retry-after",
    "location",
];
/// Request headers beyond `FORWARD_PROVIDER_HEADERS` the backend needs.
const RELAYED_REQUEST_HEADERS: &[&str] = &[
    "content-type",
    "accept",
    "accept-language",
    "user-agent",
    "oai-device-id",
    "oai-language",
];

/// The relay does not share the gateway's client. That one stops at 300 seconds even
/// while reading a body, which would cut long-held and streamed backend calls, and it
/// skips proxies, while this host is one Codex would reach through a proxy itself.
fn relay_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(Policy::none())
        .connect_timeout(RELAY_CONNECT_TIMEOUT)
        .user_agent("CODETAS-Gateway/0.1")
}

fn relay_client() -> Option<&'static reqwest::Client> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| relay_client_builder().build().ok())
        .as_ref()
}

pub(crate) fn unlimited_usage_payload() -> Value {
    let window = |seconds: u64| {
        json!({
            "used_percent": 0,
            "limit_window_seconds": seconds,
            "reset_after_seconds": seconds,
            "reset_at": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs() + seconds)
                .unwrap_or(0),
        })
    };
    json!({
        "plan_type": "pro",
        "rate_limit": {
            "allowed": true,
            "limit_reached": false,
            "primary_window": window(18_000),
            "secondary_window": window(604_800),
        },
        "credits": null,
        "additional_rate_limits": null,
    })
}

pub(crate) async fn chatgpt_usage_stub() -> Response<Body> {
    json_response(StatusCode::OK, unlimited_usage_payload())
}

pub(crate) fn relay_requires_admission(bind_ip: IpAddr) -> bool {
    !bind_ip.is_loopback()
}

/// `rest` is already percent-decoded, so an encoded `?`, `#` or `.` looks like the
/// real character here. `%` can only be left over from a double encoding.
fn backend_path_is_safe(rest: &str) -> bool {
    !rest.is_empty()
        && !rest.contains(['\\', '%', '?', '#'])
        && !rest.chars().any(char::is_control)
        && rest
            .split('/')
            .all(|segment| segment != "." && segment != "..")
}

/// The URL parser drops tabs and newlines and resolves dot segments, so what a
/// path check saw is not always what gets sent. Judge the URL that will be sent.
fn backend_url(rest: &str, query: Option<&str>) -> Option<url::Url> {
    if !backend_path_is_safe(rest) {
        return None;
    }
    let mut url = format!("{CHATGPT_BACKEND_BASE}/{rest}");
    if let Some(query) = query {
        url.push('?');
        url.push_str(query);
    }
    let url = url::Url::parse(&url).ok()?;
    (url.scheme() == "https"
        && url.host_str() == Some(CHATGPT_BACKEND_HOST)
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path().starts_with("/backend-api/"))
    .then_some(url)
}

pub(crate) async fn chatgpt_backend_passthrough(
    State(state): State<GatewayState>,
    AxumPath(rest): AxumPath<String>,
    request: Request,
    requires_admission: bool,
) -> Response<Body> {
    if requires_admission {
        if let Err(response) =
            authorize_request(&state.settings, request.headers(), "responses:write").await
        {
            return response;
        }
    }
    let (parts, body) = request.into_parts();
    let Some(url) = backend_url(&rest, parts.uri.query()) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_backend_path",
            "the ChatGPT backend path is not allowed",
        );
    };
    let Some(client) = relay_client() else {
        return error_response(
            StatusCode::BAD_GATEWAY,
            "chatgpt_backend_unreachable",
            "the ChatGPT backend request failed",
        );
    };
    relay_to_backend(&state.settings, client, url, parts, body).await
}

async fn relay_to_backend(
    settings: &SharedSettings,
    client: &reqwest::Client,
    url: url::Url,
    parts: axum::http::request::Parts,
    body: Body,
) -> Response<Body> {
    let forwarded = match collect_forward_headers(settings, &parts.headers).await {
        Ok(forwarded) => forwarded,
        Err(_) => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "missing_chatgpt_login",
                "the ChatGPT backend relay requires the caller's own Authorization header",
            )
        }
    };
    let body = match to_bytes(body, MAX_BACKEND_REQUEST_BYTES).await {
        Ok(body) => body,
        Err(_) => {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "backend_request_too_large",
                "the ChatGPT backend request body is too large",
            )
        }
    };
    let mut upstream = client.request(parts.method.clone(), url);
    for (name, value) in &forwarded {
        upstream = upstream.header(name.clone(), value.clone());
    }
    for name in RELAYED_REQUEST_HEADERS {
        if let Some(value) = parts.headers.get(*name) {
            upstream = upstream.header(*name, value.clone());
        }
    }
    if !body.is_empty() {
        upstream = upstream.body(body);
    }
    let response = match upstream.send().await {
        Ok(response) => response,
        Err(_) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "chatgpt_backend_unreachable",
                "the ChatGPT backend request failed",
            )
        }
    };
    let mut builder = Response::builder().status(response.status());
    for name in RELAYED_RESPONSE_HEADERS {
        if let Some(value) = response.headers().get(*name) {
            builder = builder.header(*name, value.clone());
        }
    }
    builder
        .body(Body::from_stream(response.bytes_stream()))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::BAD_GATEWAY,
                "chatgpt_backend_response_invalid",
                "the ChatGPT backend response could not be relayed",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_stub_reports_not_limited_with_both_windows() {
        let payload = unlimited_usage_payload();
        assert_eq!(payload["rate_limit"]["allowed"], true);
        assert_eq!(payload["rate_limit"]["limit_reached"], false);
        assert_eq!(payload["rate_limit"]["primary_window"]["used_percent"], 0);
        assert_eq!(payload["rate_limit"]["secondary_window"]["used_percent"], 0);
    }

    #[test]
    fn backend_urls_stay_under_the_base_on_the_fixed_host() {
        let url = backend_url("plugins/list", Some("a=b")).unwrap();
        assert_eq!(
            url.as_str(),
            "https://chatgpt.com/backend-api/plugins/list?a=b"
        );
        assert!(backend_url("wham/usage/thread-estimates/query", None).is_some());
        // These are the decoded forms the extractor hands over.
        for rest in [
            "",
            "../codex",
            "a/./b",
            "a/../b",
            ".\t./codex",
            ".\n./codex",
            "a\u{0}b",
            "x?y=1",
            "x#y",
            "a\\b",
            "%2e%2e/codex",
            // Trailing spaces are trimmed, leaving a dot segment only the parser sees.
            ".. ",
        ] {
            assert!(backend_url(rest, None).is_none(), "{rest:?}");
        }
    }

    #[test]
    fn only_a_loopback_bind_skips_admission() {
        assert!(!relay_requires_admission(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(!relay_requires_admission("::1".parse().unwrap()));
        assert!(relay_requires_admission(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert!(relay_requires_admission("192.168.1.20".parse().unwrap()));
    }

    #[test]
    fn the_relay_client_has_no_total_timeout_uses_proxies_and_follows_no_redirects() {
        let relay = format!("{:?}", relay_client_builder().build().unwrap());
        assert!(!relay.contains("TotalTimeout"), "{relay}");
        assert!(relay.contains("proxies"), "{relay}");
        assert!(relay.contains("redirect_policy"), "{relay}");
        // The gateway's client settings, which show these markers mean what they say.
        let gateway = format!(
            "{:?}",
            reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(300))
                .build()
                .unwrap()
        );
        assert!(gateway.contains("TotalTimeout"), "{gateway}");
        assert!(!gateway.contains("proxies"), "{gateway}");
    }

    async fn local_backend(router: Router) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (address, task)
    }

    fn test_client() -> reqwest::Client {
        relay_client_builder().no_proxy().build().unwrap()
    }

    #[tokio::test]
    async fn the_relay_forwards_allowed_headers_and_streams_the_response() {
        let (captured_sender, mut captured) = tokio::sync::mpsc::unbounded_channel();
        let (release, held) = oneshot::channel::<()>();
        let held = Arc::new(StdMutex::new(Some(held)));
        let router = Router::new().fallback(move |request: Request| {
            let captured_sender = captured_sender.clone();
            let held = Arc::clone(&held);
            async move {
                let (parts, body) = request.into_parts();
                let body = to_bytes(body, 1024).await.unwrap();
                captured_sender.send((parts, body)).unwrap();
                let held = held.lock().unwrap().take().unwrap();
                let chunks = stream! {
                    yield Ok::<Bytes, Infallible>(Bytes::from_static(b"first"));
                    let _ = held.await;
                    yield Ok(Bytes::from_static(b"second"));
                };
                Response::builder()
                    .status(201)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::RETRY_AFTER, "7")
                    .header(header::SET_COOKIE, "session=upstream")
                    .body(Body::from_stream(chunks))
                    .unwrap()
            }
        });
        let (address, task) = local_backend(router).await;

        let (parts, body) = Request::builder()
            .method("POST")
            .uri("/backend-api/plugins/list?a=b")
            .header(header::AUTHORIZATION, "Bearer chatgpt-login")
            .header("chatgpt-account-id", "account")
            .header("oai-device-id", "device")
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-codetas-token", "relay-secret")
            .header("x-api-key", "relay-secret")
            .header(header::COOKIE, "session=caller")
            .body(Body::from("{\"x\":1}"))
            .unwrap()
            .into_parts();
        let url = url::Url::parse(&format!("http://{address}/backend-api/plugins/list?a=b"));
        let state = state_with_external_key();
        let response =
            relay_to_backend(&state.settings, &test_client(), url.unwrap(), parts, body).await;

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        assert_eq!(response.headers()[header::RETRY_AFTER], "7");
        assert!(response.headers().get(header::SET_COOKIE).is_none());

        let (upstream, upstream_body) = captured.recv().await.unwrap();
        assert_eq!(upstream.method, Method::POST);
        assert_eq!(
            upstream.uri.path_and_query().unwrap().as_str(),
            "/backend-api/plugins/list?a=b"
        );
        assert_eq!(upstream_body, "{\"x\":1}");
        for (name, value) in [
            ("authorization", "Bearer chatgpt-login"),
            ("chatgpt-account-id", "account"),
            ("oai-device-id", "device"),
            ("content-type", "application/json"),
        ] {
            assert_eq!(upstream.headers[name], value, "{name}");
        }
        for name in ["x-codetas-token", "x-api-key", "cookie"] {
            assert!(upstream.headers.get(name).is_none(), "{name}");
        }

        // The first chunk arrives while the backend is still holding the second back.
        let mut chunks = response.into_body().into_data_stream();
        assert_eq!(chunks.next().await.unwrap().unwrap(), "first");
        release.send(()).unwrap();
        assert_eq!(chunks.next().await.unwrap().unwrap(), "second");
        assert!(chunks.next().await.is_none());
        task.abort();
    }

    #[tokio::test]
    async fn the_relay_hands_a_redirect_back_instead_of_following_it() {
        let sends = Arc::new(AtomicU32::new(0));
        let router = Router::new().fallback({
            let sends = Arc::clone(&sends);
            move || {
                let sends = Arc::clone(&sends);
                async move {
                    sends.fetch_add(1, Ordering::SeqCst);
                    Response::builder()
                        .status(302)
                        .header(header::LOCATION, "/backend-api/elsewhere")
                        .body(Body::empty())
                        .unwrap()
                }
            }
        });
        let (address, task) = local_backend(router).await;
        let (parts, body) = Request::builder()
            .uri("/backend-api/plugins")
            .header(header::AUTHORIZATION, "Bearer chatgpt-login")
            .body(Body::empty())
            .unwrap()
            .into_parts();
        let url = url::Url::parse(&format!("http://{address}/backend-api/plugins")).unwrap();
        let state = state_with_external_key();
        let response = relay_to_backend(&state.settings, &test_client(), url, parts, body).await;
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            response.headers()[header::LOCATION],
            "/backend-api/elsewhere"
        );
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        task.abort();
    }

    fn state_with_external_key() -> GatewayState {
        std::env::set_var("CODETAS_TEST_RELAY_KEY", "relay-secret");
        let mut settings = GatewaySettings::default();
        settings.security.external_access_keys = vec![crate::config::ExternalAccessKey {
            id: "relay".into(),
            label: "relay".into(),
            env_var: "CODETAS_TEST_RELAY_KEY".into(),
            scopes: vec!["responses:write".into()],
            enabled: true,
            expires_at_unix: None,
        }];
        let settings = Arc::new(RwLock::new(settings));
        GatewayState {
            settings: settings.clone(),
            client: reqwest::Client::new(),
            routing: Arc::new(Mutex::new(RoutingRuntime::default())),
            observability: ObservabilityLedger::new(None),
            video_jobs: Arc::new(Mutex::new(HashMap::new())),
            instance_id: "relay-test".into(),
            response_state: Arc::new(ResponseStateStore::new(None)),
            memory: Arc::new(MemoryAdmission {
                settings,
                inflight: AtomicU32::new(0),
                reserved_bytes: AtomicU64::new(0),
                rejected: AtomicU64::new(0),
            }),
            pacing: Arc::new(ProviderPacing::default()),
        }
    }

    /// Runs the real route. Every path used here is refused before any upstream
    /// call, so no test reaches the network.
    async fn relay_status(
        requires_admission: bool,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> StatusCode {
        use tower::ServiceExt as _;
        let router = Router::new()
            .route(
                "/backend-api/{*rest}",
                any(
                    move |state: State<GatewayState>, rest: AxumPath<String>, request: Request| {
                        chatgpt_backend_passthrough(state, rest, request, requires_admission)
                    },
                ),
            )
            .with_state(state_with_external_key());
        let mut request = Request::builder().uri(uri);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        router
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn the_relay_refuses_paths_as_they_arrive_on_the_wire() {
        for uri in [
            "/backend-api/.%09./codex/responses",
            "/backend-api/.%0A./codex/responses",
            "/backend-api/x%3Fa=b",
            "/backend-api/x%23y",
            "/backend-api/..%2Fcodex",
            "/backend-api/%2e%2e/codex",
            "/backend-api/%252e%252e/codex",
            "/backend-api/a%5Cb",
            "/backend-api/..%20",
            "/backend-api/../codex",
        ] {
            assert_eq!(
                relay_status(false, uri, &[]).await,
                StatusCode::BAD_REQUEST,
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn a_remote_relay_needs_admission_and_a_loopback_one_takes_only_the_bearer() {
        let chatgpt_bearer = ("authorization", "Bearer chatgpt-login");
        // Admission comes first, so the refused path shows which stage answered.
        let refused_path = "/backend-api/a%5Cb";
        assert_eq!(
            relay_status(true, refused_path, &[chatgpt_bearer]).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            relay_status(
                true,
                refused_path,
                &[chatgpt_bearer, ("x-codetas-token", "relay-secret")]
            )
            .await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            relay_status(false, refused_path, &[chatgpt_bearer]).await,
            StatusCode::BAD_REQUEST
        );
    }
}
