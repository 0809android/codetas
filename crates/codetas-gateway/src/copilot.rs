use futures_util::StreamExt;
use reqwest::{header, Client};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::OnceLock,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use url::Url;
use zeroize::Zeroizing;

const TOKEN_ENDPOINT: &str = "https://api.github.com/copilot_internal/v2/token";
const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_SESSION_CACHE_ENTRIES: usize = 32;

pub(crate) struct CopilotSession {
    pub token: Zeroizing<String>,
    pub api_base: String,
}

struct CachedCopilotSession {
    token: Zeroizing<String>,
    api_base: String,
    expires_at: Instant,
}

static SESSION_CACHE: OnceLock<Mutex<HashMap<String, CachedCopilotSession>>> = OnceLock::new();

pub(crate) async fn exchange_copilot_token(
    client: &Client,
    github_authorization: &str,
    timeout: Duration,
) -> Result<CopilotSession, String> {
    let github_token = github_authorization
        .trim()
        .strip_prefix("Bearer ")
        .or_else(|| github_authorization.trim().strip_prefix("bearer "))
        .or_else(|| github_authorization.trim().strip_prefix("token "))
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "GitHub Copilot requires a GitHub bearer token".to_string())?;
    let cache_key = credential_fingerprint(github_token);
    let cache = SESSION_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut entries = cache.lock().await;
    let now = Instant::now();
    entries.retain(|_, entry| entry.expires_at > now + Duration::from_secs(30));
    if let Some(entry) = entries.get(&cache_key) {
        return Ok(CopilotSession {
            token: Zeroizing::new(entry.token.to_string()),
            api_base: entry.api_base.clone(),
        });
    }
    // Serialize cache misses so a cold burst cannot exchange the same GitHub token many times.
    let response = client
        .get(TOKEN_ENDPOINT)
        .header(header::ACCEPT, "application/json")
        .header(header::AUTHORIZATION, format!("token {github_token}"))
        .header(header::USER_AGENT, "CODETAS/0.1")
        .header("x-github-api-version", "2022-11-28")
        .timeout(timeout.min(Duration::from_secs(15)))
        .send()
        .await
        .map_err(|_| "GitHub Copilot token exchange was unreachable".to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "GitHub Copilot token exchange failed with HTTP {}",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_TOKEN_RESPONSE_BYTES as u64)
    {
        return Err("GitHub Copilot token exchange response was too large".into());
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|_| "GitHub Copilot token exchange response failed".to_string())?;
        if bytes.len().saturating_add(chunk.len()) > MAX_TOKEN_RESPONSE_BYTES {
            return Err("GitHub Copilot token exchange response was too large".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let body: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "GitHub Copilot token exchange returned invalid JSON".to_string())?;
    let token = body
        .get("token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 16 * 1024)
        .ok_or_else(|| "GitHub Copilot token exchange returned no usable token".to_string())?
        .to_string();
    if token.bytes().any(|byte| byte.is_ascii_control()) {
        return Err("GitHub Copilot token exchange returned an invalid token".into());
    }
    let api_base = body
        .pointer("/endpoints/api")
        .and_then(Value::as_str)
        .and_then(valid_copilot_api_base)
        .unwrap_or_else(|| "https://api.githubcopilot.com".into());
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let expires_in = body
        .get("expires_at")
        .and_then(Value::as_u64)
        .map(|expires| expires.saturating_sub(now_unix))
        .or_else(|| body.get("refresh_in").and_then(Value::as_u64))
        .unwrap_or(10 * 60)
        .clamp(60, 60 * 60)
        .saturating_sub(30);
    if entries.len() >= MAX_SESSION_CACHE_ENTRIES {
        entries.clear();
    }
    entries.insert(
        cache_key,
        CachedCopilotSession {
            token: Zeroizing::new(token.clone()),
            api_base: api_base.clone(),
            expires_at: Instant::now() + Duration::from_secs(expires_in.max(30)),
        },
    );
    Ok(CopilotSession {
        token: Zeroizing::new(token),
        api_base,
    })
}

fn credential_fingerprint(value: &str) -> String {
    // In-process cache identity only: this value is never logged or persisted and does not
    // retain the original GitHub token. A cryptographic digest avoids a collision selecting
    // another credential's cached Copilot session.
    let digest = Sha256::digest(value.as_bytes());
    format!("{digest:x}")
}

pub(crate) fn valid_copilot_api_base(value: &str) -> Option<String> {
    let url = Url::parse(value.trim()).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|port| port != 443)
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "api.githubcopilot.com" && !host.ends_with(".githubcopilot.com") {
        return None;
    }
    Some(format!("https://{host}"))
}
