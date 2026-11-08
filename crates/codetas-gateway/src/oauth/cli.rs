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

#[cfg(target_os = "macos")]
const MUSE_KEYCHAIN_SERVICE: &str = "ai.meta.dev.credentials";
#[cfg(target_os = "macos")]
const MUSE_KEYCHAIN_ACCOUNT: &str = "meta";

#[cfg(target_os = "macos")]
const VIBE_KEYCHAIN_SERVICE: &str = "ai.mistral.vibe";
const VIBE_API_KEY_NAME: &str = "MISTRAL_API_KEY";
/// Long enough to answer a Keychain prompt, short enough not to wedge a caller
/// that nobody is there to answer for.
#[cfg(target_os = "macos")]
const KEYCHAIN_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs a command that may wait on the user and returns its trimmed output, or
/// nothing if it fails or is still waiting at the deadline.
#[cfg(any(target_os = "macos", test))]
fn output_within(mut command: std::process::Command, timeout: Duration) -> Option<String> {
    use std::io::Read as _;
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    // Drained while it runs: a command that fills the pipe would otherwise never exit.
    let mut stdout = child.stdout.take()?;
    let (sender, output) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = sender.send(stdout.read_to_end(&mut bytes).map(|_| bytes));
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let bytes = output.recv_timeout(remaining).ok()?.ok()?;
    let value = String::from_utf8(bytes).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(target_os = "macos")]
fn read_keychain_item(service: &str, account: Option<&str>) -> Option<String> {
    // Temp-HOME tests must not pick up the developer's own logins.
    if cfg!(test) {
        return None;
    }
    let mut command = std::process::Command::new("/usr/bin/security");
    command.args(["find-generic-password", "-s", service]);
    if let Some(account) = account {
        command.args(["-a", account]);
    }
    command.arg("-w");
    output_within(command, KEYCHAIN_READ_TIMEOUT)
}

/// Mistral Vibe keeps one API key: in `~/.vibe/.env`, or in the OS keyring.
/// Browser sign-in also ends in an API key, so both setup paths import alike.
pub(crate) fn detect_vibe_cli_session(home: &Path) -> Option<OAuthSession> {
    // The order Vibe itself resolves them in.
    detect_vibe_env_session(home).or_else(detect_vibe_keychain_session)
}

pub(crate) fn detect_vibe_env_session(home: &Path) -> Option<OAuthSession> {
    fs::read_to_string(home.join(".vibe/.env"))
        .ok()
        .and_then(|raw| parse_vibe_dotenv_key(&raw))
        .map(|access| static_cli_api_key_session(&access))
}

pub(crate) fn detect_vibe_keychain_session() -> Option<OAuthSession> {
    read_vibe_keychain().map(|access| static_cli_api_key_session(&access))
}

/// Reads only the Mistral key; the file may also hold other providers' keys. Vibe's
/// server loads this file with python-dotenv, where a later assignment replaces an
/// earlier one, even with an empty value. A line it cannot parse assigns nothing.
pub(crate) fn parse_vibe_dotenv_key(raw: &str) -> Option<String> {
    raw.lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line
                .strip_prefix("export ")
                .map(str::trim_start)
                .unwrap_or(line);
            let (name, value) = line.split_once('=')?;
            if name.trim() != VIBE_API_KEY_NAME {
                return None;
            }
            dotenv_value(value.trim())
        })
        .next_back()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Vibe's dotenv rules: a quoted value ends at its closing quote and may only be
/// followed by a comment. An unquoted one ends at a whitespace-`#` comment.
fn dotenv_value(raw: &str) -> Option<&str> {
    let quote = raw.chars().next().filter(|c| matches!(c, '"' | '\''));
    if let Some(quote) = quote {
        let (value, after) = raw[1..].split_once(quote)?;
        let after = after.trim_start();
        return (after.is_empty() || after.starts_with('#')).then_some(value);
    }
    let end = raw
        .char_indices()
        .find(|(index, c)| {
            *c == '#'
                && raw[..*index]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace)
        })
        .map_or(raw.len(), |(index, _)| index);
    Some(raw[..end].trim_end())
}

fn read_vibe_keychain() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        read_keychain_item(VIBE_KEYCHAIN_SERVICE, Some(VIBE_API_KEY_NAME))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

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
    parse_muse_secure_value(&read_muse_secure_storage(home)?, home)
}

pub(crate) fn parse_muse_secure_value(raw: &str, home: &Path) -> Option<OAuthSession> {
    if let Some(session) = parse_muse_credentials(raw) {
        return Some(session);
    }
    // Keychain may hold a bare token (LLM|... / dca:...). Enrich with
    // the account email from auth.json when available.
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with('{') {
        return None;
    }
    let email = fs::read_to_string(muse_auth_path(home))
        .ok()
        .and_then(|file_raw| serde_json::from_str::<Value>(&file_raw).ok())
        .and_then(|value| {
            value
                .pointer("/providers/meta/user_email")
                .or_else(|| value.pointer("/providers/meta/email"))
                .and_then(|entry| entry.as_str())
                .map(|entry| entry.to_lowercase())
        });
    Some(OAuthSession {
        access: trimmed.to_string(),
        refresh: String::new(),
        // Muse CLI does not persist a refresh token; request-time
        // refresh re-reads the secure storage.
        expires_at_ms: i64::MAX / 2,
        source: "local-cli".into(),
        account_id: None,
        email,
    })
}

pub(crate) fn read_muse_secure_storage(home: &Path) -> Option<String> {
    read_muse_keychain().or_else(|| fs::read_to_string(muse_auth_path(home)).ok())
}

pub(crate) fn read_muse_keychain() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        read_keychain_item(MUSE_KEYCHAIN_SERVICE, Some(MUSE_KEYCHAIN_ACCOUNT))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
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
        &fs::read_to_string(qwen_settings_path(home))
            .ok()
            .unwrap_or_default(),
        &zai_cli_key_names(),
    )
    .or_else(|| parse_simple_api_key_file(&fs::read_to_string(home.join(".z.ai/auth.json")).ok()?))
    .or_else(|| parse_simple_api_key_file(&fs::read_to_string(home.join(".zai/auth.json")).ok()?))
    .or_else(|| {
        parse_simple_api_key_file(&fs::read_to_string(home.join(".config/zai/auth.json")).ok()?)
    })
}

pub(crate) fn detect_minimax_cli_session(home: &Path) -> Option<OAuthSession> {
    parse_named_qwen_cli_key(
        &fs::read_to_string(qwen_settings_path(home))
            .ok()
            .unwrap_or_default(),
        &minimax_cli_key_names(),
    )
    .or_else(|| {
        parse_simple_api_key_file(&fs::read_to_string(home.join(".minimax/auth.json")).ok()?)
    })
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
        read_keychain_item(
            ANTIGRAVITY_KEYCHAIN_SERVICE,
            Some(ANTIGRAVITY_KEYCHAIN_ACCOUNT),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

pub(crate) fn read_claude_secure_storage(home: &Path) -> Option<String> {
    let file = claude_credentials_path(home);
    let from_file = fs::read_to_string(&file).ok();
    if claude_config_dir_var().is_some() {
        return from_file.or_else(read_claude_keychain);
    }
    read_claude_keychain().or(from_file)
}

fn claude_config_dir_var() -> Option<std::ffi::OsString> {
    // Temp-HOME tests must not pick up the developer's own login.
    env::var_os("CLAUDE_CONFIG_DIR").filter(|_| !cfg!(test))
}

pub(crate) fn claude_credentials_path(home: &Path) -> PathBuf {
    claude_config_dir_var()
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(".claude"))
        .join(".credentials.json")
}

pub(crate) fn read_claude_keychain() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        read_keychain_item(CLAUDE_KEYCHAIN_SERVICE, None)
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
        // `agy` records the signed-in account separately from the token.
        email: json_string(&value, &["id_token_email"]).map(|email| email.to_lowercase()),
    })
}

pub(crate) fn parse_muse_credentials(raw: &str) -> Option<OAuthSession> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let entry = value
        .pointer("/providers/meta")
        .or_else(|| value.get("meta"))
        .unwrap_or(&value);
    let api_key = json_string(entry, &["api_key", "apiKey"]).filter(|value| !value.is_empty());
    let access_token = json_string(entry, &["access_token", "accessToken", "access"])
        .filter(|value| !value.is_empty());
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
                json_string(item, &["id"]).as_deref() == Some(model_id)
                    && qwen_item_is_alibaba(item)
            })
        })
        .or_else(|| {
            items
                .iter()
                .copied()
                .find(|item| qwen_item_is_alibaba(item))
        });
    let (access, base_url) = if let Some(item) = item {
        (
            qwen_item_secret(&value, item)?,
            qwen_item_base_url(&value, item),
        )
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
    let url = qwen_item_base_url_raw(item)
        .unwrap_or_default()
        .to_ascii_lowercase();
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
        "alibaba"
            if url
                .to_ascii_lowercase()
                .contains("coding.dashscope.aliyuncs.com") =>
        {
            Some(url.to_string())
        }
        "qwen"
            if url.to_ascii_lowercase().contains("dashscope.aliyuncs.com")
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

/// One model as DeepAI Code's own Pi runtime declares it. DeepAI's local
/// transport serves `/chat/completions` or `/responses`, so the wire kind is
/// carried per model rather than once for the provider.
#[derive(Clone, Debug)]
pub(crate) struct DeepAiModel {
    pub(crate) id: String,
    pub(crate) context_window: Option<u64>,
    pub(crate) max_output: Option<u64>,
    pub(crate) modalities: Vec<String>,
    pub(crate) responses_api: bool,
    /// The app declares `reasoning: true`; the model then takes an effort.
    pub(crate) reasoning: bool,
}

/// The loopback transport a running DeepAI Code app exposes. Its port and
/// bearer capability are chosen when the app starts and written into the Pi
/// config, so both are read together instead of being assumed to be stable.
#[derive(Clone, Debug)]
pub(crate) struct DeepAiRuntime {
    pub(crate) session: OAuthSession,
    pub(crate) base_url: Option<String>,
    pub(crate) models: Vec<DeepAiModel>,
}

/// DeepAI Code stores its runtime state beside the user's home in
/// `.deepai-code`. The Pi instance id in the path is fixed to `deepai`; a
/// renamed instance is not found, which keeps detection from guessing.
fn deepai_code_home(home: &Path) -> PathBuf {
    home.join(".deepai-code")
}

pub(crate) fn deepai_models_path(home: &Path) -> PathBuf {
    deepai_code_home(home).join("userdata/deepai/pi/deepai/models.json")
}

pub(crate) fn detect_deepai_runtime(home: &Path) -> Option<DeepAiRuntime> {
    parse_deepai_models_json(&fs::read_to_string(deepai_models_path(home)).ok()?)
}

pub(crate) fn parse_deepai_models_json(raw: &str) -> Option<DeepAiRuntime> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let provider = value.get("providers")?.get("deepai")?;
    let api_key = provider.get("apiKey").and_then(Value::as_str)?.trim();
    if api_key.is_empty() {
        return None;
    }
    let base_url = provider
        .get("baseUrl")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| is_loopback_base_url(value))
        .map(str::to_string);
    // A URL outside loopback (or a malformed one) is refused instead of trusted:
    // DeepAI Code only ever binds loopback, and a prompt must not be sent
    // wherever else the file asks.
    if provider.get("baseUrl").and_then(Value::as_str).is_some() && base_url.is_none() {
        return None;
    }
    // The file belongs to another app, so nothing in it is trusted: values that
    // would fail `ProviderDefinition::validate` are dropped here rather than
    // being allowed to fail the whole settings file.
    let models = provider
        .get("models")
        .and_then(Value::as_array)
        .map(|entries| {
            let mut seen = std::collections::HashSet::new();
            entries
                .iter()
                .filter_map(parse_deepai_model)
                .filter(|model| seen.insert(model.id.clone()))
                .take(250)
                .collect()
        })
        .unwrap_or_default();
    Some(DeepAiRuntime {
        session: static_cli_api_key_session(api_key),
        base_url,
        models,
    })
}

fn parse_deepai_model(value: &Value) -> Option<DeepAiModel> {
    let id = value.get("id").and_then(Value::as_str)?.trim();
    if id.is_empty() || id.len() > 240 || id.contains('/') || id.chars().any(char::is_control) {
        return None;
    }
    let modalities = value
        .get("input")
        .and_then(Value::as_array)
        .map(|values| {
            let mut seen: Vec<String> = Vec::new();
            for modality in values.iter().filter_map(Value::as_str) {
                if matches!(modality, "text" | "image" | "audio" | "video")
                    && !seen.iter().any(|existing| existing == modality)
                {
                    seen.push(modality.to_string());
                }
            }
            seen
        })
        .filter(|values| !values.is_empty())
        .unwrap_or_else(|| vec!["text".into()]);
    let context_window = deepai_limit(value.get("contextWindow"));
    // `modelMaxOutputTokens` must not exceed the context window, so a larger
    // value is dropped instead of failing validation.
    let max_output = deepai_limit(value.get("maxTokens"))
        .filter(|max| context_window.is_none_or(|context| *max <= context));
    Some(DeepAiModel {
        id: id.to_string(),
        context_window,
        max_output,
        modalities,
        responses_api: value.get("api").and_then(Value::as_str) == Some("openai-responses"),
        reasoning: value
            .get("reasoning")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn deepai_limit(value: Option<&Value>) -> Option<u64> {
    value
        .and_then(Value::as_u64)
        .filter(|limit| (1..=100_000_000).contains(limit))
}

/// DeepAI Code binds `127.0.0.1` on a random port. Only that shape is accepted;
/// a credential-free plaintext URL to any other host would leak the prompt and
/// the capability.
fn is_loopback_base_url(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https")
        && url.port().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && matches!(
            url.host_str(),
            Some("127.0.0.1") | Some("localhost") | Some("::1") | Some("[::1]")
        )
}

pub(crate) fn is_valid_google_project_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    (6..=30).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_lowercase)
        && bytes
            .last()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

#[cfg(test)]
mod deepai_tests {
    use super::*;

    fn models_json() -> &'static str {
        r#"{
          "providers": {
            "deepai": {
              "baseUrl": "http://127.0.0.1:60011",
              "api": "openai-completions",
              "apiKey": "capability-token",
              "models": [
                { "id": "kimi-k3", "name": "Kimi K3", "input": ["text", "image"], "contextWindow": 1048576, "maxTokens": 131072 },
                { "id": "gpt-6-astra", "name": "GPT-6 Astra", "input": ["text", "image"], "contextWindow": 1048576, "maxTokens": 128000, "api": "openai-responses" }
              ]
            }
          }
        }"#
    }

    #[test]
    fn parses_the_running_app_transport_and_its_models() {
        let runtime = parse_deepai_models_json(models_json()).unwrap();
        assert_eq!(runtime.session.access, "capability-token");
        assert_eq!(runtime.base_url.as_deref(), Some("http://127.0.0.1:60011"));
        assert_eq!(
            runtime
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec!["kimi-k3", "gpt-6-astra"]
        );
        assert_eq!(runtime.models[0].context_window, Some(1_048_576));
        assert_eq!(runtime.models[0].max_output, Some(131_072));
        assert_eq!(runtime.models[0].modalities, vec!["text", "image"]);
        assert!(!runtime.models[0].responses_api);
        assert!(runtime.models[1].responses_api);
    }

    #[test]
    fn a_missing_provider_or_token_is_not_a_runtime() {
        assert!(parse_deepai_models_json("{\"providers\":{}}").is_none());
        assert!(parse_deepai_models_json(
            "{\"providers\":{\"deepai\":{\"baseUrl\":\"http://127.0.0.1:1\",\"apiKey\":\"  \"}}}"
        )
        .is_none());
    }

    #[test]
    fn a_model_without_input_defaults_to_text() {
        let runtime = parse_deepai_models_json(
            r#"{"providers":{"deepai":{"apiKey":"k","models":[{"id":"m","name":"M"}]}}}"#,
        )
        .unwrap();
        assert_eq!(runtime.base_url, None);
        assert_eq!(runtime.models[0].modalities, vec!["text"]);
    }

    #[test]
    fn values_that_would_fail_validation_are_dropped() {
        let runtime = parse_deepai_models_json(
            r#"{"providers":{"deepai":{"apiKey":"k","models":[
                {"id":"good","input":["image","image","file","text"],"contextWindow":1000,"maxTokens":2000},
                {"id":"zero","contextWindow":0,"maxTokens":0},
                {"id":"huge","contextWindow":200000000},
                {"id":""},
                {"id":"ctrl\u0007"},
                {"id":"a/b"},
                {"id":"good"}
            ]}}}"#,
        )
        .unwrap();
        assert_eq!(
            runtime
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec!["good", "zero", "huge"]
        );
        // Duplicate ids are collapsed to the first occurrence.
        // Unknown and duplicate modalities are removed, order preserved.
        assert_eq!(runtime.models[0].modalities, vec!["image", "text"]);
        // maxTokens above contextWindow is dropped.
        assert_eq!(runtime.models[0].context_window, Some(1000));
        assert_eq!(runtime.models[0].max_output, None);
        // Zero and out-of-range limits are dropped.
        assert_eq!(runtime.models[1].context_window, None);
        assert_eq!(runtime.models[1].max_output, None);
        assert_eq!(runtime.models[2].context_window, None);
    }

    #[test]
    fn at_most_250_models_are_kept() {
        let models = (0..300)
            .map(|index| format!(r#"{{"id":"m{index}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let raw = format!(r#"{{"providers":{{"deepai":{{"apiKey":"k","models":[{models}]}}}}}}"#);
        assert_eq!(parse_deepai_models_json(&raw).unwrap().models.len(), 250);
    }

    #[test]
    fn a_base_url_outside_loopback_is_refused() {
        for base_url in [
            "http://example.com:60011",
            "http://10.0.0.5:60011",
            "http://127.0.0.1",
            "http://127.0.0.1:60011/?x=1",
            "http://user@127.0.0.1:60011",
        ] {
            let raw =
                format!(r#"{{"providers":{{"deepai":{{"apiKey":"k","baseUrl":"{base_url}"}}}}}}"#);
            assert!(
                parse_deepai_models_json(&raw).is_none(),
                "{base_url} must be refused"
            );
        }
        for base_url in [
            "http://127.0.0.1:60011",
            "http://localhost:60011",
            "http://[::1]:60011",
            "https://127.0.0.1:60011",
        ] {
            let raw =
                format!(r#"{{"providers":{{"deepai":{{"apiKey":"k","baseUrl":"{base_url}"}}}}}}"#);
            assert!(
                parse_deepai_models_json(&raw).is_some(),
                "{base_url} must be accepted"
            );
        }
    }
}

#[cfg(all(test, unix))]
mod timed_read_tests {
    use super::*;

    fn shell(script: &str) -> std::process::Command {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn a_read_still_waiting_at_the_deadline_is_given_up_on() {
        let started = std::time::Instant::now();
        assert_eq!(
            output_within(shell("sleep 30"), Duration::from_millis(100)),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_read_larger_than_a_pipe_buffer_comes_back_whole() {
        let output = output_within(
            shell("head -c 300000 /dev/zero | tr '\\0' x"),
            Duration::from_secs(10),
        );
        assert_eq!(output.map(|value| value.len()), Some(300_000));
    }

    #[test]
    fn a_read_returns_trimmed_output_only_on_success() {
        let timeout = Duration::from_secs(10);
        assert_eq!(
            output_within(shell("printf ' key \\n'"), timeout).as_deref(),
            Some("key")
        );
        assert_eq!(output_within(shell("printf key; exit 1"), timeout), None);
        assert_eq!(output_within(shell("printf ' '"), timeout), None);
    }
}
