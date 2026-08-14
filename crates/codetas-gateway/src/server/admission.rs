use super::*;

#[derive(Clone, Copy)]
pub(crate) enum Admission {
    Anonymous,
    LocalMaster,
    External,
}

impl Admission {
    pub(crate) fn trusts_turn_metadata(self) -> bool {
        matches!(self, Self::LocalMaster)
    }
}

pub(crate) async fn authorize_request(
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

pub(crate) fn provided_access_tokens(headers: &HeaderMap) -> Vec<&str> {
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

pub(crate) fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

pub(crate) const APPROX_INPUT_BYTES_PER_TOKEN: u64 = 4;

pub(crate) const FORWARD_PROVIDER_HEADERS: &[&str] = &[
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

pub(crate) async fn apply_forward_headers(
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

pub(crate) async fn collect_forward_headers(
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
