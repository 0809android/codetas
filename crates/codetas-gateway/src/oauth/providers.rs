use super::*;
use serde_json::Value;
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
    Ok(OAuthSession {
        access,
        refresh,
        expires_at_ms: now_ms() + expires_in.saturating_mul(1_000) - 5 * 60 * 1_000,
        source: "oauth".into(),
        account_id: None,
        email: None,
    })
}

pub(crate) fn kimi_common_headers() -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    let device_id = kimi_device_id();
    let pairs = [
        ("User-Agent", "KimiCLI/0.14.0"),
        ("X-Msh-Platform", "kimi_code_cli"),
        ("X-Msh-Version", "0.14.0"),
        ("X-Msh-Device-Name", "codetas"),
        ("X-Msh-Device-Model", env::consts::OS),
        ("X-Msh-Os-Version", env::consts::OS),
        ("X-Msh-Device-Id", device_id.as_str()),
    ];
    for (name, value) in pairs {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    headers
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
    let redirect = format!("http://127.0.0.1:{ANTHROPIC_CALLBACK_PORT}/callback");
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

