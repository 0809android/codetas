use super::*;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde_json::Value;
use std::process::Command;
use uuid::Uuid;
pub(crate) async fn refresh_kimi_token(refresh_token: &str) -> Result<OAuthSession, String> {
    let client_id = oauth_client_id(KIMI_CLIENT_ID_ENV, KIMI_CLIENT_ID);
    let host = env::var("KIMI_CODE_OAUTH_HOST")
        .or_else(|_| env::var("KIMI_OAUTH_HOST"))
        .unwrap_or_else(|_| KIMI_OAUTH_HOST.to_string());
    let body = form_body(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id.as_str()),
    ]);
    let response = oauth_http_client()?
        .post(format!("{}/api/oauth/token", host.trim_end_matches('/')))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .headers(kimi_common_headers())
        .body(body)
        .send()
        .await
        .map_err(|_| "Kimi token refresh failed".to_string())?;
    if !response.status().is_success() {
        return Err("Kimi token refresh failed".into());
    }
    parse_kimi_token_response(
        response
            .json()
            .await
            .map_err(|_| "Kimi token refresh failed")?,
        refresh_token,
    )
}

pub(crate) async fn login_kimi() -> Result<OAuthSession, String> {
    let client_id = oauth_client_id(KIMI_CLIENT_ID_ENV, KIMI_CLIENT_ID);
    let host = env::var("KIMI_CODE_OAUTH_HOST")
        .or_else(|_| env::var("KIMI_OAUTH_HOST"))
        .unwrap_or_else(|_| KIMI_OAUTH_HOST.to_string());
    let authorize_body = form_body(&[("client_id", client_id.as_str())]);
    let authorize = oauth_http_client()?
        .post(format!(
            "{}/api/oauth/device_authorization",
            host.trim_end_matches('/')
        ))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .headers(kimi_common_headers())
        .body(authorize_body)
        .send()
        .await
        .map_err(|_| "Kimi login could not start".to_string())?;
    if !authorize.status().is_success() {
        return Err("Kimi login could not start".into());
    }
    let payload: Value = authorize
        .json()
        .await
        .map_err(|_| "Kimi login could not start".to_string())?;
    let device_code =
        json_string(&payload, &["device_code"]).ok_or("Kimi login could not start")?;
    let verification = json_string(&payload, &["verification_uri_complete"])
        .or_else(|| json_string(&payload, &["verification_uri"]))
        .ok_or("Kimi login could not start")?;
    open_browser(&verification)?;
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(900);
    let interval = payload.get("interval").and_then(Value::as_u64).unwrap_or(5);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(expires_in.max(30));
    let mut wait = Duration::from_secs(interval.max(1));
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err("Kimi login timed out".into());
        }
        tokio::time::sleep(wait).await;
        let poll_body = form_body(&[
            ("client_id", client_id.as_str()),
            ("device_code", &device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ]);
        let response = oauth_http_client()?
            .post(format!("{}/api/oauth/token", host.trim_end_matches('/')))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .headers(kimi_common_headers())
            .body(poll_body)
            .send()
            .await
            .map_err(|_| "Kimi login failed".to_string())?;
        let payload: Value = response
            .json()
            .await
            .map_err(|_| "Kimi login failed".to_string())?;
        if payload
            .get("access_token")
            .and_then(Value::as_str)
            .is_some()
        {
            return parse_kimi_token_response(payload, "");
        }
        match payload.get("error").and_then(Value::as_str) {
            Some("authorization_pending") => {}
            Some("slow_down") => wait += Duration::from_secs(5),
            Some("expired_token") => return Err("Kimi login expired".into()),
            Some("access_denied") => return Err("Kimi login was denied".into()),
            _ => return Err("Kimi login failed".into()),
        }
    }
}

pub(crate) fn parse_kimi_token_response(
    payload: Value,
    refresh_fallback: &str,
) -> Result<OAuthSession, String> {
    let access =
        json_string(&payload, &["access_token"]).ok_or("Kimi token response is incomplete")?;
    let refresh = json_string(&payload, &["refresh_token"])
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| refresh_fallback.to_string());
    if refresh.is_empty() {
        return Err("Kimi token response is incomplete".into());
    }
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|value| *value >= 0)
        .unwrap_or(900);
    let (account_id, email) = kimi_identity_from_tokens(&access, Some(&refresh));
    Ok(OAuthSession {
        access,
        refresh,
        expires_at_ms: now_ms() + expires_in.saturating_mul(1_000) - 5 * 60 * 1_000,
        source: "oauth".into(),
        account_id,
        email,
    })
}

pub(crate) fn kimi_common_headers() -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    let device_id = kimi_device_id();
    let version = "0.14.0";
    let device_name = kimi_device_name();
    let device_model = kimi_device_model();
    let os_version = kimi_os_version();
    let pairs = [
        ("User-Agent", format!("KimiCLI/{version}")),
        ("X-Msh-Platform", "kimi_code_cli".into()),
        ("X-Msh-Version", version.into()),
        ("X-Msh-Device-Name", device_name),
        ("X-Msh-Device-Model", device_model),
        ("X-Msh-Os-Version", os_version),
        ("X-Msh-Device-Id", device_id),
    ];
    for (name, value) in pairs {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}

fn command_text(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn kimi_device_name() -> String {
    command_text("hostname", &[])
        .or_else(|| {
            env::var("HOSTNAME")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "codetas".into())
}

fn kimi_device_model() -> String {
    let platform = match env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    };
    let arch = match env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    let release = command_text("uname", &["-r"]).unwrap_or_else(|| env::consts::OS.into());
    format!("{platform} {release} {arch}")
}

fn kimi_os_version() -> String {
    command_text("uname", &["-v"])
        .or_else(|| command_text("uname", &["-r"]))
        .unwrap_or_else(|| env::consts::OS.into())
}

pub(crate) fn kimi_device_id() -> String {
    let path = auth_store_path().with_file_name("kimi-device-id");
    if let Ok(existing) = fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let id = Uuid::new_v4().simple().to_string();
    let _ = atomic_write_private(&path, format!("{id}\n").as_bytes());
    id
}

pub(crate) async fn refresh_anthropic_token(refresh_token: &str) -> Result<OAuthSession, String> {
    let client_id = oauth_client_id(ANTHROPIC_CLIENT_ID_ENV, ANTHROPIC_CLIENT_ID);
    let payload = post_json(
        ANTHROPIC_TOKEN_URL,
        serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": client_id,
            "refresh_token": refresh_token,
        }),
    )
    .await?;
    parse_anthropic_token_response(payload, refresh_token)
}

pub(crate) async fn login_anthropic() -> Result<OAuthSession, String> {
    let client_id = oauth_client_id(ANTHROPIC_CLIENT_ID_ENV, ANTHROPIC_CLIENT_ID);
    let (verifier, challenge) = pkce_pair();
    let state = Uuid::new_v4().to_string();
    // This client id registers `http://localhost:<port>/callback`; the literal
    // `127.0.0.1` host is rejected ("Redirect URI ... is not supported by
    // client"). The listener still binds loopback only.
    let redirect = format!("http://localhost:{ANTHROPIC_CALLBACK_PORT}/callback");
    let query = form_body(&[
        ("code", "true"),
        ("client_id", client_id.as_str()),
        ("response_type", "code"),
        ("redirect_uri", &redirect),
        ("scope", "org:create_api_key user:profile user:inference"),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
    ]);
    let url = format!("{ANTHROPIC_AUTHORIZE_URL}?{query}");
    let code = wait_for_oauth_code(ANTHROPIC_CALLBACK_PORT, "/callback", &state, &url).await?;
    let payload = post_json(
        ANTHROPIC_TOKEN_URL,
        serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": client_id,
            "code": code,
            "state": state,
            "redirect_uri": redirect,
            "code_verifier": verifier,
        }),
    )
    .await?;
    parse_anthropic_token_response(payload, "")
}

pub(crate) fn parse_anthropic_token_response(
    payload: Value,
    refresh_fallback: &str,
) -> Result<OAuthSession, String> {
    let access =
        json_string(&payload, &["access_token"]).ok_or("Claude token response is incomplete")?;
    let refresh = json_string(&payload, &["refresh_token"])
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| refresh_fallback.to_string());
    if refresh.is_empty() {
        return Err("Claude token response is incomplete".into());
    }
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|value| *value >= 0)
        .unwrap_or(3_600);
    let account = payload.get("account");
    Ok(OAuthSession {
        access,
        refresh,
        expires_at_ms: now_ms() + expires_in.saturating_mul(1_000) - 5 * 60 * 1_000,
        source: "oauth".into(),
        account_id: account.and_then(|value| json_string(value, &["uuid"])),
        email: account.and_then(|value| json_string(value, &["email_address", "email"])),
    })
}

pub(crate) async fn refresh_xai_token(refresh_token: &str) -> Result<OAuthSession, String> {
    let client_id = oauth_client_id(XAI_CLIENT_ID_ENV, XAI_CLIENT_ID);
    let token_endpoint = discover_xai_token_endpoint().await?;
    let payload = post_form(
        &token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client_id.as_str()),
            ("refresh_token", refresh_token),
        ],
    )
    .await?;
    parse_xai_token_response(payload, refresh_token)
}

pub(crate) async fn login_xai() -> Result<OAuthSession, String> {
    let client_id = oauth_client_id(XAI_CLIENT_ID_ENV, XAI_CLIENT_ID);
    let discovery = discover_xai_endpoints().await?;
    let (verifier, challenge) = pkce_pair();
    let state = Uuid::new_v4().to_string();
    let redirect = format!("http://127.0.0.1:{XAI_CALLBACK_PORT}/callback");
    let nonce = Uuid::new_v4().to_string();
    let query = form_body(&[
        ("response_type", "code"),
        ("client_id", client_id.as_str()),
        ("redirect_uri", &redirect),
        ("scope", XAI_SCOPE),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
        ("nonce", &nonce),
    ]);
    let url = format!("{}?{query}", discovery.0);
    let code = wait_for_oauth_code(XAI_CALLBACK_PORT, "/callback", &state, &url).await?;
    let payload = post_form(
        &discovery.1,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", client_id.as_str()),
            ("code", &code),
            ("redirect_uri", &redirect),
            ("code_verifier", &verifier),
        ],
    )
    .await?;
    parse_xai_token_response(payload, "")
}

pub(crate) fn parse_xai_token_response(
    payload: Value,
    refresh_fallback: &str,
) -> Result<OAuthSession, String> {
    let access =
        json_string(&payload, &["access_token"]).ok_or("xAI token response is incomplete")?;
    let refresh = json_string(&payload, &["refresh_token"])
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| refresh_fallback.to_string());
    if refresh.is_empty() {
        return Err("xAI token response is incomplete".into());
    }
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|value| *value >= 0)
        .unwrap_or(3_600);
    Ok(OAuthSession {
        access,
        refresh,
        expires_at_ms: now_ms() + expires_in.saturating_mul(1_000) - 2 * 60 * 1_000,
        source: "oauth".into(),
        account_id: None,
        email: None,
    })
}

pub(crate) async fn discover_xai_token_endpoint() -> Result<String, String> {
    Ok(discover_xai_endpoints().await?.1)
}

pub(crate) async fn discover_xai_endpoints() -> Result<(String, String), String> {
    let payload: Value = oauth_http_client()?
        .get(XAI_DISCOVERY_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|_| "xAI OAuth discovery failed".to_string())?
        .json()
        .await
        .map_err(|_| "xAI OAuth discovery failed".to_string())?;
    let authorize =
        json_string(&payload, &["authorization_endpoint"]).ok_or("xAI OAuth discovery failed")?;
    let token = json_string(&payload, &["token_endpoint"]).ok_or("xAI OAuth discovery failed")?;
    validate_xai_endpoint(&authorize)?;
    validate_xai_endpoint(&token)?;
    Ok((authorize, token))
}

pub(crate) fn validate_xai_endpoint(raw: &str) -> Result<(), String> {
    let parsed = url::Url::parse(raw).map_err(|_| "xAI OAuth discovery failed".to_string())?;
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    if parsed.scheme() == "https" && (host == "x.ai" || host.ends_with(".x.ai")) {
        Ok(())
    } else {
        Err("xAI OAuth discovery failed".into())
    }
}

/// Starts Google's authorization-code + PKCE login for Antigravity. The token
/// endpoint always requires the installed-app client secret, even with PKCE,
/// so unlike the Kimi / Anthropic / xAI flows this provider needs a secret.
pub(crate) async fn login_antigravity() -> Result<OAuthSession, String> {
    let (client_id, client_secret) = antigravity_client_credentials()?;
    let (verifier, challenge) = pkce_pair();
    let state = Uuid::new_v4().to_string();
    // Google's installed-app clients accept any loopback port. `localhost` is
    // not registered for this client; `127.0.0.1` is.
    let redirect = format!("http://127.0.0.1:{ANTIGRAVITY_CALLBACK_PORT}/oauth2callback");
    let query = form_body(&[
        ("client_id", client_id.as_str()),
        ("response_type", "code"),
        ("redirect_uri", &redirect),
        ("scope", ANTIGRAVITY_SCOPE),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
        // Ask for a refresh token even when this Google account already
        // granted the same scopes to another app.
        ("access_type", "offline"),
        ("prompt", "consent"),
    ]);
    let url = format!("{ANTIGRAVITY_AUTHORIZE_URL}?{query}");
    let code =
        wait_for_oauth_code(ANTIGRAVITY_CALLBACK_PORT, "/oauth2callback", &state, &url).await?;
    let payload = post_google_form(
        ANTIGRAVITY_TOKEN_URL,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("code", &code),
            ("redirect_uri", &redirect),
            ("code_verifier", &verifier),
        ],
    )
    .await?;
    let mut session = parse_antigravity_token_response(payload, "")?;
    enrich_antigravity_identity(&mut session).await;
    Ok(session)
}

pub(crate) async fn refresh_antigravity_token(refresh_token: &str) -> Result<OAuthSession, String> {
    let (client_id, client_secret) = antigravity_client_credentials()?;
    let payload = post_google_form(
        ANTIGRAVITY_TOKEN_URL,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("refresh_token", refresh_token),
        ],
    )
    .await?;
    parse_antigravity_token_response(payload, refresh_token)
}

/// Google reports authorization errors as JSON on the token endpoint. Surface
/// `error` / `error_description` (never the request body) so a revoked or
/// mismatched client produces an actionable message instead of "request failed".
async fn post_google_form(url: &str, fields: &[(&str, &str)]) -> Result<Value, String> {
    let response = oauth_http_client()?
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form_body(fields))
        .send()
        .await
        .map_err(|_| "Google のOAuthトークン要求を送信できませんでした".to_string())?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let payload: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(payload);
    }
    let code = json_string(&payload, &["error"]).unwrap_or_else(|| status.to_string());
    let detail = json_string(&payload, &["error_description"]).unwrap_or_default();
    let detail = detail.chars().take(240).collect::<String>();
    Err(if detail.is_empty() {
        format!("Google のOAuthトークン要求に失敗しました ({code})")
    } else {
        format!("Google のOAuthトークン要求に失敗しました ({code}): {detail}")
    })
}

pub(crate) fn parse_antigravity_token_response(
    payload: Value,
    refresh_fallback: &str,
) -> Result<OAuthSession, String> {
    let access =
        json_string(&payload, &["access_token"]).ok_or("Google token response is incomplete")?;
    let refresh = json_string(&payload, &["refresh_token"])
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| refresh_fallback.to_string());
    if refresh.is_empty() {
        return Err(
            "Google が refresh token を返しませんでした。ブラウザで再ログインしてください".into(),
        );
    }
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|value| *value >= 0)
        .unwrap_or(3_600);
    let (account_id, email) = payload
        .get("id_token")
        .and_then(Value::as_str)
        .map(antigravity_identity_from_id_token)
        .unwrap_or((None, None));
    Ok(OAuthSession {
        access,
        refresh,
        expires_at_ms: now_ms() + expires_in.saturating_mul(1_000) - 2 * 60 * 1_000,
        source: "oauth".into(),
        account_id,
        email,
    })
}

/// Reads `sub` / `email` from the id_token Google returned over TLS. This is
/// used only as a label and store-key identity; it is not an authorization
/// decision, so the signature does not need to be verified.
fn antigravity_identity_from_id_token(id_token: &str) -> (Option<String>, Option<String>) {
    let Some(encoded) = id_token.split('.').nth(1) else {
        return (None, None);
    };
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(encoded) else {
        return (None, None);
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return (None, None);
    };
    (
        json_string(&value, &["sub"]),
        json_string(&value, &["email"]).map(|email| email.to_lowercase()),
    )
}

async fn enrich_antigravity_identity(session: &mut OAuthSession) {
    if session.account_id.is_some() && session.email.is_some() {
        return;
    }
    let Ok(client) = oauth_http_client() else {
        return;
    };
    let Ok(response) = client
        .get(ANTIGRAVITY_USERINFO_URL)
        .bearer_auth(&session.access)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
    else {
        return;
    };
    if !response.status().is_success() {
        return;
    }
    let Ok(value) = response.json::<Value>().await else {
        return;
    };
    if session.account_id.is_none() {
        session.account_id = json_string(&value, &["id", "sub"]);
    }
    if session.email.is_none() {
        session.email = json_string(&value, &["email"]).map(|email| email.to_lowercase());
    }
}

#[cfg(test)]
mod antigravity_token_tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn id_token(payload: &str) -> String {
        let encoded = URL_SAFE_NO_PAD.encode(payload.as_bytes());
        format!("header.{encoded}.signature")
    }

    #[test]
    fn parses_google_login_response_and_id_token_identity() {
        let payload = serde_json::json!({
            "access_token": "atok",
            "refresh_token": "rtok",
            "expires_in": 3600,
            "id_token": id_token(r#"{"sub":"12345","email":"User@Example.com"}"#),
        });
        let session = parse_antigravity_token_response(payload, "").unwrap();
        assert_eq!(session.access, "atok");
        assert_eq!(session.refresh, "rtok");
        assert_eq!(session.account_id.as_deref(), Some("12345"));
        assert_eq!(session.email.as_deref(), Some("user@example.com"));
        assert_eq!(session.source, "oauth");
        assert!(!session_needs_refresh(&session));
    }

    #[test]
    fn refresh_response_keeps_the_previous_refresh_token() {
        let payload = serde_json::json!({ "access_token": "new", "expires_in": 3600 });
        let session = parse_antigravity_token_response(payload, "old-refresh").unwrap();
        assert_eq!(session.access, "new");
        assert_eq!(session.refresh, "old-refresh");
        assert!(session.account_id.is_none());
    }

    #[test]
    fn login_without_a_refresh_token_is_rejected() {
        let payload = serde_json::json!({ "access_token": "atok", "expires_in": 3600 });
        assert!(parse_antigravity_token_response(payload, "").is_err());
    }

    #[test]
    fn malformed_id_token_does_not_fail_the_login() {
        let payload = serde_json::json!({
            "access_token": "atok",
            "refresh_token": "rtok",
            "expires_in": 3600,
            "id_token": "not-a-jwt",
        });
        let session = parse_antigravity_token_response(payload, "").unwrap();
        assert!(session.account_id.is_none());
        assert!(session.email.is_none());
    }
}

#[cfg(test)]
mod antigravity_live_tests {
    use super::*;

    fn live_cache_path() -> PathBuf {
        std::env::temp_dir().join("codetas-antigravity-live-session.json")
    }

    /// Reuses the last browser login for follow-up diagnostics so each probe
    /// does not need another consent screen. The file is a temporary secret and
    /// must be deleted after the manual verification.
    async fn live_session() -> OAuthSession {
        if let Ok(raw) = fs::read_to_string(live_cache_path()) {
            if let Ok(session) = serde_json::from_str::<OAuthSession>(&raw) {
                if !session_needs_refresh(&session) {
                    return session;
                }
                if let Ok(fresh) = refresh_session("google-antigravity", &session).await {
                    let _ = atomic_write_private(
                        &live_cache_path(),
                        &serde_json::to_vec(&fresh).unwrap_or_default(),
                    );
                    return fresh;
                }
            }
        }
        let session = login_antigravity().await.expect("Google login failed");
        let _ = atomic_write_private(
            &live_cache_path(),
            &serde_json::to_vec(&session).unwrap_or_default(),
        );
        session
    }

    /// Manual end-to-end check of the in-app Google login. Ignored by default
    /// because it opens a browser and needs a real Google account:
    ///
    /// ```text
    /// CODETAS_ANTIGRAVITY_CLIENT_ID=... CODETAS_ANTIGRAVITY_CLIENT_SECRET=... \
    ///   cargo test -p codetas-gateway live_antigravity_login_works -- --ignored --nocapture
    /// ```
    ///
    /// It asserts that the browser login yields a refresh token and that
    /// `loadCodeAssist` and `fetchAvailableModels` accept the resulting access
    /// token. Nothing is written to the auth store.
    #[tokio::test]
    #[ignore = "opens a browser and needs a real Google account"]
    async fn live_antigravity_login_works() {
        let session = live_session().await;
        println!("account_id={:?}", session.account_id);
        println!("email={:?}", session.email);
        println!("refresh_token_present={}", !session.refresh.is_empty());
        assert!(!session.refresh.trim().is_empty());

        let project = probe_antigravity_cloud_project(&session.access)
            .await
            .expect("loadCodeAssist was rejected for this access token");
        println!("project={project}");

        let response = oauth_http_client()
            .expect("client")
            .post("https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels")
            .bearer_auth(&session.access)
            .header(reqwest::header::USER_AGENT, antigravity_user_agent())
            .json(&serde_json::json!({ "project": project }))
            .send()
            .await
            .expect("fetchAvailableModels request");
        let status = response.status();
        assert!(status.is_success(), "fetchAvailableModels HTTP {status}");
        let value: Value = response.json().await.expect("fetchAvailableModels json");
        let models = value
            .get("models")
            .and_then(Value::as_object)
            .map(|models| models.len())
            .unwrap_or(0);
        println!("model_count={models}");
        assert!(models > 0, "no models for this account");
    }

    /// Prints the raw Code Assist onboarding responses for a real session so
    /// the project resolution can be implemented against the actual shape.
    #[tokio::test]
    #[ignore = "needs a real Google account"]
    async fn live_antigravity_load_code_assist_shapes() {
        let session = live_session().await;
        let client = oauth_http_client().expect("client");
        for metadata in [
            serde_json::json!({ "ideType": "ANTIGRAVITY" }),
            serde_json::json!({
                "ideType": "ANTIGRAVITY",
                "pluginType": "GEMINI",
                "platform": "MACOS"
            }),
        ] {
            let response = client
                .post("https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist")
                .bearer_auth(&session.access)
                .header(reqwest::header::USER_AGENT, antigravity_user_agent())
                .json(&serde_json::json!({ "metadata": metadata }))
                .send()
                .await
                .expect("loadCodeAssist request");
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            println!("--- metadata={metadata}\nstatus={status}\nbody={body}");
        }
    }
}

const OPENDESIGN_CONTROL_URL: &str = "https://amr-api.open-design.ai";
/// A login the user has not approved yet. Kept short: the approval page is open
/// while this waits.
const OPENDESIGN_LOGIN_FALLBACK_SECS: u64 = 600;

/// Sign in through OpenDesign's device authorization, the same one `vela login`
/// uses. The runtime key (`sk-amr-`) pays for model calls and is stored as the
/// session's `access`; the control key (`vela_ctrl_`) is the only credential that
/// can read plan usage and is stored as `refresh`. Neither expires on its own, so
/// the session never asks to be refreshed.
pub(crate) async fn login_opendesign() -> Result<OAuthSession, String> {
    let client = oauth_http_client()?;
    let started = client
        .post(format!(
            "{OPENDESIGN_CONTROL_URL}/api/v1/cli/device-authorizations"
        ))
        .json(&serde_json::json!({"cliVersion":"0.1.4","host":"CODETAS","profile":"prod"}))
        .send()
        .await
        .map_err(|_| "OpenDesign のログインを開始できません".to_string())?;
    if !started.status().is_success() {
        return Err("OpenDesign のログインを開始できません".into());
    }
    let payload: Value = started
        .json()
        .await
        .map_err(|_| "OpenDesign のログインを開始できません".to_string())?;
    let device_id =
        json_string(&payload, &["deviceId"]).ok_or("OpenDesign のログインを開始できません")?;
    let secret =
        json_string(&payload, &["deviceSecret"]).ok_or("OpenDesign のログインを開始できません")?;
    let activation =
        json_string(&payload, &["activationUrl"]).ok_or("OpenDesign のログインを開始できません")?;
    if !opendesign_activation_url_is_official(&activation) {
        return Err("OpenDesign の承認ページが想定外のため中止しました".into());
    }
    open_browser(&activation)?;
    let interval = payload
        .get("pollIntervalSeconds")
        .and_then(Value::as_u64)
        .unwrap_or(2)
        .clamp(1, 10);
    // The server also sends `expiresAt`; the wait is capped well inside it.
    let expires_in = OPENDESIGN_LOGIN_FALLBACK_SECS;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(expires_in);
    let device = url::form_urlencoded::byte_serialize(device_id.as_bytes()).collect::<String>();
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err("OpenDesign のログインがタイムアウトしました".into());
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let response = client
            .post(format!(
                "{OPENDESIGN_CONTROL_URL}/api/v1/cli/device-authorizations/{device}/token"
            ))
            .json(&serde_json::json!({"deviceSecret": secret}))
            .send()
            .await
            .map_err(|_| "OpenDesign のログインに失敗しました".to_string())?;
        let status = response.status();
        if status.as_u16() == 404 {
            return Err("OpenDesign のログインの有効期限が切れました".into());
        }
        if !status.is_success() {
            return Err(format!(
                "OpenDesign のログインに失敗しました (HTTP {status})"
            ));
        }
        let data: Value = response
            .json()
            .await
            .map_err(|_| "OpenDesign のログインに失敗しました".to_string())?;
        match data.get("status").and_then(Value::as_str) {
            Some("pending") => continue,
            Some("expired") => return Err("OpenDesign のログインの有効期限が切れました".into()),
            Some("canceled" | "cancelled" | "denied" | "rejected") => {
                return Err("OpenDesign のログインが拒否されました".into())
            }
            _ => {}
        }
        return parse_opendesign_profile(&data);
    }
}

pub(crate) fn opendesign_activation_url_is_official(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("open-design.ai")
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && url.path().starts_with("/cloud/cli/activate")
    })
}

/// The approved device authorization carries both keys and the user's identity.
pub(crate) fn parse_opendesign_profile(data: &Value) -> Result<OAuthSession, String> {
    let runtime = json_string(data, &["runtimeKey"])
        .filter(|key| key.starts_with("sk-amr"))
        .ok_or("OpenDesign の応答にAPIキーがありません")?;
    let control = json_string(data, &["controlKey"])
        .filter(|key| key.starts_with("vela_ctrl_"))
        .ok_or("OpenDesign の応答に使用量用のキーがありません")?;
    let user = data.get("user").unwrap_or(&Value::Null);
    let account_id = json_string(user, &["id", "appUserId"]);
    let email = json_string(user, &["email"]);
    if account_id.is_none() && email.is_none() {
        return Err("OpenDesign の応答にアカウント情報がありません".into());
    }
    Ok(OAuthSession {
        access: runtime,
        refresh: control,
        // Keys stay valid until revoked on the API keys page.
        expires_at_ms: now_ms() + 10 * 365 * 24 * 3_600 * 1_000,
        source: "oauth".into(),
        account_id,
        email,
    })
}

#[cfg(test)]
mod opendesign_tests {
    use super::*;

    #[test]
    fn activation_page_must_be_the_official_one() {
        assert!(opendesign_activation_url_is_official(
            "https://open-design.ai/cloud/cli/activate?deviceId=d&userCode=ABC123"
        ));
        for bad in [
            "http://open-design.ai/cloud/cli/activate",
            "https://open-design.ai.evil.test/cloud/cli/activate",
            "https://user@open-design.ai/cloud/cli/activate",
            "https://open-design.ai/other",
        ] {
            assert!(!opendesign_activation_url_is_official(bad), "{bad}");
        }
    }

    #[test]
    fn approved_profile_keeps_both_keys_and_identity() {
        let session = parse_opendesign_profile(&serde_json::json!({
            "runtimeKey": "sk-amr-abc",
            "controlKey": "vela_ctrl_abc",
            "user": {"id": "u1", "email": "a@example.com"}
        }))
        .unwrap();
        assert_eq!(session.access, "sk-amr-abc");
        assert_eq!(session.refresh, "vela_ctrl_abc");
        assert_eq!(session.account_id.as_deref(), Some("u1"));
        assert!(!session_needs_refresh(&session));
        assert!(parse_opendesign_profile(
            &serde_json::json!({"runtimeKey":"x","controlKey":"vela_ctrl_a","user":{"id":"u"}})
        )
        .is_err());
        assert!(parse_opendesign_profile(
            &serde_json::json!({"runtimeKey":"sk-amr-a","controlKey":"vela_ctrl_a"})
        )
        .is_err());
    }
}
