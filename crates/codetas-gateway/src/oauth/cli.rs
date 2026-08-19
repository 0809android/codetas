use super::*;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
const ANTIGRAVITY_KEYCHAIN_SERVICE: &str = "gemini";
#[cfg(target_os = "macos")]
const ANTIGRAVITY_KEYCHAIN_ACCOUNT: &str = "antigravity";

pub(crate) fn detect_kimi_cli_session(home: &Path) -> Option<OAuthSession> {
    parse_kimi_credentials(
        &fs::read_to_string(home.join(".kimi-code/credentials/kimi-code.json")).ok()?,
    )
}

pub(crate) fn detect_claude_cli_session(home: &Path) -> Option<OAuthSession> {
    if let Some(raw) = read_claude_secure_storage(home) {
        if let Some(session) = parse_claude_credentials(&raw) {
            return Some(session);
        }
    }
    None
}

pub(crate) fn detect_xai_cli_session(home: &Path) -> Option<OAuthSession> {
    parse_xai_credentials(&fs::read_to_string(home.join(".grok/auth.json")).ok()?)
}

pub(crate) fn detect_antigravity_cli_session(home: &Path) -> Option<OAuthSession> {
    parse_antigravity_credentials(&read_antigravity_secure_storage(home)?)
}

pub(crate) fn detect_muse_cli_session(home: &Path) -> Option<OAuthSession> {
    parse_muse_credentials(&fs::read_to_string(muse_auth_path(home)).ok()?)
}

pub(crate) fn muse_auth_path(home: &Path) -> PathBuf {
    home.join(".config/muse/auth.json")
}

pub(crate) fn qwen_settings_path(home: &Path) -> PathBuf {
    home.join(".qwen/settings.json")
}

pub(crate) fn detect_qwen_cli_target(home: &Path) -> Option<QwenCliTarget> {
    parse_qwen_cli_target(&fs::read_to_string(qwen_settings_path(home)).ok()?)
}

pub(crate) fn detect_zai_cli_session(home: &Path) -> Option<OAuthSession> {
    parse_named_qwen_cli_key(
        &fs::read_to_string(qwen_settings_path(home)).ok().unwrap_or_default(),
        &zai_cli_key_names(),
    )
    .or_else(|| parse_simple_api_key_file(&fs::read_to_string(home.join(".z.ai/auth.json")).ok()?))
    .or_else(|| parse_simple_api_key_file(&fs::read_to_string(home.join(".zai/auth.json")).ok()?))
    .or_else(|| parse_simple_api_key_file(&fs::read_to_string(home.join(".config/zai/auth.json")).ok()?))
}

pub(crate) fn detect_minimax_cli_session(home: &Path) -> Option<OAuthSession> {
    parse_named_qwen_cli_key(
        &fs::read_to_string(qwen_settings_path(home)).ok().unwrap_or_default(),
        &minimax_cli_key_names(),
    )
    .or_else(|| parse_simple_api_key_file(&fs::read_to_string(home.join(".minimax/auth.json")).ok()?))
    .or_else(|| {
        parse_simple_api_key_file(&fs::read_to_string(home.join(".config/minimax/auth.json")).ok()?)
    })
}

fn qwen_cli_key_names() -> Vec<&'static str> {
    vec![
        "BAILIAN_TOKEN_PLAN_API_KEY",
        "BAILIAN_CODING_PLAN_API_KEY",
        "DASHSCOPE_API_KEY",
        "QWEN_API_KEY",
    ]
}

fn zai_cli_key_names() -> Vec<&'static str> {
    vec!["ZAI_API_KEY", "ZHIPU_API_KEY", "GLM_API_KEY"]
}

fn minimax_cli_key_names() -> Vec<&'static str> {
    vec!["MINIMAX_API_KEY"]
}

pub(crate) fn read_antigravity_secure_storage(home: &Path) -> Option<String> {
    read_antigravity_keychain().or_else(|| {
        fs::read_to_string(home.join(".gemini/antigravity-cli/antigravity-oauth-token")).ok()
    })
}

pub(crate) fn read_antigravity_keychain() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("security")
            .args([
                "find-generic-password",
                "-s",
                ANTIGRAVITY_KEYCHAIN_SERVICE,
                "-a",
                ANTIGRAVITY_KEYCHAIN_ACCOUNT,
                "-w",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
        if value.is_empty() {
            return None;
        }
        return Some(value);
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

pub(crate) fn read_claude_secure_storage(home: &Path) -> Option<String> {
    let file = claude_credentials_path(home);
    let from_file = fs::read_to_string(&file).ok();
    if env::var_os("CLAUDE_CONFIG_DIR").is_some() {
        return from_file.or_else(read_claude_keychain);
    }
    read_claude_keychain().or(from_file)
}

pub(crate) fn claude_credentials_path(home: &Path) -> PathBuf {
    env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(".claude"))
        .join(".credentials.json")
}

pub(crate) fn read_claude_keychain() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("security")
            .args(["find-generic-password", "-s", CLAUDE_KEYCHAIN_SERVICE, "-w"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
        if value.is_empty() {
            return None;
        }
        return Some(value);
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

pub(crate) fn parse_kimi_credentials(raw: &str) -> Option<OAuthSession> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let access = json_string(&value, &["access_token", "accessToken", "access"])?;
    let refresh =
        json_string(&value, &["refresh_token", "refreshToken", "refresh"]).unwrap_or_default();
    if access.is_empty() {
        return None;
    }
    let (jwt_account_id, jwt_email) =
        kimi_identity_from_tokens(&access, (!refresh.is_empty()).then_some(refresh.as_str()));
    Some(OAuthSession {
        access,
        refresh,
        expires_at_ms: json_expiry_ms(&value, &["expires_at", "expiresAt", "expires"]),
        source: "local-cli".into(),
        account_id: jwt_account_id
            .or_else(|| json_string(&value, &["user_id", "userId", "accountId"])),
        email: jwt_email
            .or_else(|| json_string(&value, &["email"]).map(|value| value.to_lowercase())),
    })
}

pub(crate) fn parse_claude_credentials(raw: &str) -> Option<OAuthSession> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let oauth = value.get("claudeAiOauth").unwrap_or(&value);
    let access = json_string(oauth, &["accessToken", "access_token", "access"])?;
    let refresh =
        json_string(oauth, &["refreshToken", "refresh_token", "refresh"]).unwrap_or_default();
    if access.is_empty() {
        return None;
    }
    Some(OAuthSession {
        access,
        refresh,
        expires_at_ms: json_expiry_ms(oauth, &["expiresAt", "expires_at", "expires"]),
        source: "local-cli".into(),
        account_id: json_string(oauth, &["accountId", "account_id"]),
        email: json_string(oauth, &["email"]),
    })
}

pub(crate) fn parse_xai_credentials(raw: &str) -> Option<OAuthSession> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let entry = value.as_object()?.iter().find_map(|(key, entry)| {
        key.starts_with("https://auth.x.ai::")
            .then_some(entry)
            .or_else(|| key.contains("auth.x.ai").then_some(entry))
    })?;
    let access = json_string(entry, &["key", "access_token", "access"])?;
    let refresh = json_string(entry, &["refresh_token", "refreshToken", "refresh"])?;
    if access.is_empty() || refresh.is_empty() {
        return None;
    }
    Some(OAuthSession {
        access,
        refresh,
        expires_at_ms: json_expiry_ms(entry, &["expires_at", "expiresAt", "expires"]),
        source: "local-cli".into(),
        account_id: json_string(entry, &["user_id", "userId", "accountId"]),
        email: json_string(entry, &["email"]),
    })
}

pub(crate) fn parse_antigravity_credentials(raw: &str) -> Option<OAuthSession> {
    let raw = raw.trim();
    let decoded;
    let json = if raw.starts_with('{') {
        raw
    } else {
        let (_, payload) = raw.split_once(':')?;
        decoded = STANDARD.decode(payload.trim()).ok()?;
        std::str::from_utf8(&decoded).ok()?.trim()
    };
    let value: Value = serde_json::from_str(json).ok()?;
    let token = value.get("token").unwrap_or(&value);
    let access = json_string(token, &["access_token", "accessToken", "access"])?;
    let refresh =
        json_string(token, &["refresh_token", "refreshToken", "refresh"]).unwrap_or_default();
    if access.is_empty() {
        return None;
    }
    Some(OAuthSession {
        access,
        refresh,
        expires_at_ms: json_expiry_ms(token, &["expiry", "expires_at", "expiresAt", "expires"]),
        source: "local-cli".into(),
        account_id: None,
        email: None,
    })
}

pub(crate) fn parse_muse_credentials(raw: &str) -> Option<OAuthSession> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let entry = value
        .pointer("/providers/meta")
        .or_else(|| value.get("meta"))
        .unwrap_or(&value);
    let api_key = json_string(entry, &["api_key", "apiKey"]).filter(|value| !value.is_empty());
    let access_token =
        json_string(entry, &["access_token", "accessToken", "access"]).filter(|value| !value.is_empty());
    let access = api_key.or(access_token)?;
    Some(OAuthSession {
        access,
        refresh: String::new(),
        // Muse CLI does not persist a refresh token. API keys and the current
        // account token stay valid until the user runs `muse login` again;
        // request-time refresh re-reads ~/.config/muse/auth.json.
        expires_at_ms: i64::MAX / 2,
        source: "local-cli".into(),
        account_id: json_string(entry, &["user_id", "userId", "accountId"]),
        email: json_string(entry, &["user_email", "email"]).map(|value| value.to_lowercase()),
    })
}

#[derive(Clone, Debug)]
pub(crate) struct QwenCliTarget {
    pub provider_id: &'static str,
    pub base_url: Option<String>,
    pub session: OAuthSession,
}

pub(crate) fn parse_qwen_cli_target(raw: &str) -> Option<QwenCliTarget> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let selected = qwen_selected_model_id(&value);
    let items = value
        .get("modelProviders")
        .map(flatten_qwen_model_providers)
        .unwrap_or_default();
    let item = selected
        .and_then(|model_id| {
            items.iter().copied().find(|item| {
                json_string(item, &["id"]).as_deref() == Some(model_id) && qwen_item_is_alibaba(item)
            })
        })
        .or_else(|| items.iter().copied().find(|item| qwen_item_is_alibaba(item)));
    let (access, base_url) = if let Some(item) = item {
        (qwen_item_secret(&value, item)?, qwen_item_base_url(&value, item))
    } else {
        let access = qwen_cli_key_names().into_iter().find_map(|name| {
            value
                .get("env")
                .and_then(Value::as_object)
                .and_then(|env| env.get(name))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })?;
        let base_url = value
            .pointer("/model/baseUrl")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        (access, base_url)
    };
    Some(QwenCliTarget {
        provider_id: alibaba_provider_id_for_url(base_url.as_deref()),
        base_url: alibaba_base_url_override(base_url.as_deref()),
        session: static_cli_api_key_session(&access),
    })
}

pub(crate) fn parse_qwen_cli_key(raw: &str, names: &[&str]) -> Option<OAuthSession> {
    parse_named_qwen_cli_key(raw, names)
}

pub(crate) fn parse_named_qwen_cli_key(raw: &str, names: &[&str]) -> Option<OAuthSession> {
    if raw.trim().is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(raw).ok()?;
    let env = value.get("env").and_then(Value::as_object);
    for name in names {
        if let Some(access) = env
            .and_then(|env| env.get(*name))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(static_cli_api_key_session(access));
        }
    }
    for item in flatten_qwen_model_providers(value.get("modelProviders")?) {
        let env_key = json_string(item, &["envKey", "env_key"]).unwrap_or_default();
        if !names.iter().any(|name| *name == env_key) {
            continue;
        }
        if let Some(access) = qwen_item_secret(&value, item) {
            return Some(static_cli_api_key_session(&access));
        }
    }
    None
}

pub(crate) fn parse_simple_api_key_file(raw: &str) -> Option<OAuthSession> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let access = json_string(&value, &["api_key", "apiKey"]).filter(|value| !value.is_empty())?;
    Some(static_cli_api_key_session(&access))
}

fn qwen_selected_model_id(value: &Value) -> Option<&str> {
    value
        .pointer("/model/name")
        .or_else(|| value.pointer("/model/id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn qwen_item_is_alibaba(item: &Value) -> bool {
    let env_key = json_string(item, &["envKey", "env_key"]).unwrap_or_default();
    if qwen_cli_key_names().iter().any(|name| *name == env_key) {
        return true;
    }
    let url = qwen_item_base_url_raw(item).unwrap_or_default().to_ascii_lowercase();
    url.contains("dashscope") || url.contains("token-plan") || url.contains("maas.aliyuncs.com")
}

fn qwen_item_base_url(value: &Value, item: &Value) -> Option<String> {
    qwen_item_base_url_raw(item).or_else(|| {
        value
            .pointer("/model/baseUrl")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn qwen_item_base_url_raw(item: &Value) -> Option<String> {
    json_string(item, &["baseUrl", "base_url"])
}

fn qwen_item_secret(value: &Value, item: &Value) -> Option<String> {
    let env = value.get("env").and_then(Value::as_object);
    if let Some(env_key) = json_string(item, &["envKey", "env_key"]) {
        if let Some(access) = env
            .and_then(|env| env.get(&env_key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(access.to_string());
        }
    }
    json_string(item, &["apiKey", "api_key"]).filter(|value| !value.is_empty())
}

pub(crate) fn alibaba_provider_id_for_url(base_url: Option<&str>) -> &'static str {
    let url = base_url.unwrap_or("").to_ascii_lowercase();
    if url.contains("token-plan.cn-beijing") {
        "alibaba-token-plan"
    } else if url.contains("token-plan.") || url.contains("maas.aliyuncs.com") {
        "alibaba-token-plan-intl"
    } else if url.contains("coding-intl.dashscope") {
        "alibaba"
    } else if url.contains("coding.dashscope.aliyuncs.com") {
        "alibaba"
    } else if url.contains("dashscope-intl") {
        "qwen"
    } else if url.contains("dashscope.aliyuncs.com") {
        "qwen"
    } else {
        "alibaba-token-plan-intl"
    }
}

pub(crate) fn alibaba_base_url_override(base_url: Option<&str>) -> Option<String> {
    let url = base_url?;
    match alibaba_provider_id_for_url(Some(url)) {
        "alibaba" if url.to_ascii_lowercase().contains("coding.dashscope.aliyuncs.com") => {
            Some(url.to_string())
        }
        "qwen" if url.to_ascii_lowercase().contains("dashscope.aliyuncs.com")
            && !url.to_ascii_lowercase().contains("dashscope-intl") =>
        {
            Some(url.to_string())
        }
        _ => None,
    }
}

fn flatten_qwen_model_providers(value: &Value) -> Vec<&Value> {
    match value {
        Value::Array(items) => items.iter().collect(),
        Value::Object(map) => map
            .values()
            .flat_map(|entry| match entry {
                Value::Array(items) => items.iter().collect::<Vec<_>>(),
                Value::Object(_) => vec![entry],
                _ => Vec::new(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn static_cli_api_key_session(access: &str) -> OAuthSession {
    OAuthSession {
        access: access.to_string(),
        refresh: String::new(),
        expires_at_ms: i64::MAX / 2,
        source: "local-cli".into(),
        account_id: None,
        email: None,
    }
}

pub(crate) fn is_valid_google_project_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    (6..=30).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_lowercase)
        && bytes.last().is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}
