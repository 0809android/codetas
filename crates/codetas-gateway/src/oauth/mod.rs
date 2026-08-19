use super::*;
use crate::config::CredentialSource;
use crate::config::CredentialTransport;
use crate::config::GatewaySettings;
use crate::config::ProviderCredential;
use crate::registry::provider_presets;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::time::Duration;
mod cli;
mod http;
mod providers;
mod session;
mod store;
mod util;

pub(crate) use cli::*;
pub(crate) use http::*;
pub(crate) use providers::*;
pub(crate) use session::*;
pub(crate) use store::*;
pub(crate) use util::*;

const AUTH_STORE_VERSION: u8 = 1;
const REFRESH_SKEW_MS: i64 = 60_000;
// Public OAuth client IDs of the official Kimi / Claude Code / Grok CLIs. They
// are public (PKCE, no client secret) and shipped so CLI logins can be imported
// and refreshed without extra setup. CODETAS_KIMI_CLIENT_ID /
// CODETAS_ANTHROPIC_CLIENT_ID / CODETAS_XAI_CLIENT_ID override them for
// deployments that register their own OAuth app.
const KIMI_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const KIMI_CLIENT_ID_ENV: &str = "CODETAS_KIMI_CLIENT_ID";
const KIMI_OAUTH_HOST: &str = "https://auth.kimi.com";
const ANTHROPIC_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const ANTHROPIC_CLIENT_ID_ENV: &str = "CODETAS_ANTHROPIC_CLIENT_ID";
const ANTHROPIC_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const ANTHROPIC_TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";
const ANTHROPIC_CALLBACK_PORT: u16 = 54_545;
const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const XAI_CLIENT_ID_ENV: &str = "CODETAS_XAI_CLIENT_ID";
const XAI_DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
const XAI_CALLBACK_PORT: u16 = 56_121;
const XAI_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
#[cfg(target_os = "macos")]
const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

fn oauth_client_id(env_var: &str, default: &str) -> String {
    env::var(env_var)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

static AUTH_STORE_PATH: OnceLock<PathBuf> = OnceLock::new();
static AUTH_FILE_LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
static AUTH_REFRESH_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
static AUTH_WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthSession {
    pub access: String,
    pub refresh: String,
    pub expires_at_ms: i64,
    #[serde(default)]
    pub source: String,
    pub account_id: Option<String>,
    pub email: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuthStoreFile {
    version: u8,
    #[serde(default)]
    providers: BTreeMap<String, OAuthSession>,
}

#[derive(Clone, Debug)]
pub struct OAuthLoginReport {
    pub provider_id: String,
    pub imported: bool,
    pub source: String,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthProviderDescriptor {
    pub id: &'static str,
    pub aliases: &'static [&'static str],
    pub display_name: &'static str,
    pub flow: &'static str,
    pub native_login: bool,
    pub cli_import: bool,
}

const OAUTH_PROVIDER_REGISTRY: &[OAuthProviderDescriptor] = &[
    OAuthProviderDescriptor { id: "kimi", aliases: &["kimi-code"], display_name: "Kimi", flow: "device-code", native_login: true, cli_import: true },
    OAuthProviderDescriptor { id: "anthropic", aliases: &[], display_name: "Anthropic", flow: "authorization-code-pkce", native_login: true, cli_import: true },
    OAuthProviderDescriptor { id: "xai", aliases: &[], display_name: "xAI", flow: "authorization-code-pkce", native_login: true, cli_import: true },
    OAuthProviderDescriptor { id: "google-antigravity", aliases: &[], display_name: "Google Antigravity", flow: "cli-import", native_login: false, cli_import: true },
    OAuthProviderDescriptor { id: "meta", aliases: &["meta-ai", "muse"], display_name: "Meta Muse", flow: "cli-import", native_login: false, cli_import: true },
    OAuthProviderDescriptor { id: "alibaba-token-plan-intl", aliases: &["qwen-cloud"], display_name: "Qwen Token Plan", flow: "cli-import", native_login: false, cli_import: true },
    OAuthProviderDescriptor { id: "alibaba-token-plan", aliases: &[], display_name: "Qwen Token Plan (Beijing)", flow: "cli-import", native_login: false, cli_import: true },
    OAuthProviderDescriptor { id: "alibaba", aliases: &[], display_name: "Qwen Coding Plan", flow: "cli-import", native_login: false, cli_import: true },
    OAuthProviderDescriptor { id: "qwen", aliases: &[], display_name: "Qwen International", flow: "cli-import", native_login: false, cli_import: true },
    OAuthProviderDescriptor { id: "zai", aliases: &["zhipu-bigmodel"], display_name: "Z.AI GLM", flow: "cli-import", native_login: false, cli_import: true },
    OAuthProviderDescriptor { id: "minimax", aliases: &["minimax-cn"], display_name: "MiniMax", flow: "cli-import", native_login: false, cli_import: true },
];

pub fn oauth_provider_registry() -> &'static [OAuthProviderDescriptor] {
    OAUTH_PROVIDER_REGISTRY
}

pub fn configure_auth_store_path(path: Option<PathBuf>) {
    if AUTH_STORE_PATH.get().is_some() {
        return;
    }
    let _ = AUTH_STORE_PATH.set(path.unwrap_or_else(default_auth_store_path));
}

pub fn auth_store_path() -> PathBuf {
    AUTH_STORE_PATH
        .get()
        .cloned()
        .unwrap_or_else(default_auth_store_path)
}

pub fn auth_store_is_configured() -> bool {
    AUTH_STORE_PATH.get().is_some()
}

pub fn default_auth_store_path() -> PathBuf {
    if let Ok(explicit) = env::var("CODETAS_AUTH_PATH") {
        let path = PathBuf::from(explicit);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codetas")
        .join("auth.json")
}

pub fn user_home_dir() -> PathBuf {
    user_home()
}

pub fn has_stored_session(provider_id: &str) -> bool {
    read_store(&auth_store_path()).ok().is_some_and(|store| {
        store
            .providers
            .contains_key(canonical_provider_id(provider_id))
    })
}

pub fn native_oauth_provider_id(provider_id: &str) -> Option<&'static str> {
    OAUTH_PROVIDER_REGISTRY.iter().find(|descriptor| {
        descriptor.native_login
            && (descriptor.id == provider_id || descriptor.aliases.contains(&provider_id))
    }).map(|descriptor| descriptor.id)
}

pub fn provider_supports_native_oauth(provider_id: &str) -> bool {
    native_oauth_provider_id(provider_id).is_some()
}

pub fn oauth_session_credential(provider_id: &str) -> ProviderCredential {
    ProviderCredential {
        source: CredentialSource::OAuth,
        reference: Some(provider_id.to_string()),
        transport: CredentialTransport::Bearer,
        header_name: None,
        command: None,
    }
}

pub fn adopt_local_cli_sessions(settings: &mut GatewaySettings) -> Result<bool, String> {
    adopt_local_cli_sessions_from(settings, &auth_store_path(), &user_home())
}

pub fn adopt_local_cli_sessions_from(
    settings: &mut GatewaySettings,
    store_path: &Path,
    home: &Path,
) -> Result<bool, String> {
    let mut settings_changed = false;
    let _guard = lock_auth_store(store_path)?;
    let mut store = load_store(store_path)?;
    let mut store_changed = false;
    for provider_id in OAUTH_PROVIDER_REGISTRY.iter().filter(|item| item.cli_import).map(|item| item.id) {
        let activate_existing_stub = settings
            .providers
            .iter()
            .any(|provider| provider.id == provider_id && is_unwired_cli_target(provider));
        if store.providers.contains_key(provider_id) && !is_qwen_cli_provider(provider_id) && provider_id != "meta" && provider_id != "zai" && provider_id != "minimax" {
            settings_changed |=
                attach_stored_oauth_provider(settings, provider_id, activate_existing_stub)?;
            continue;
        }
        if is_qwen_cli_provider(provider_id) {
            let Some(target) = detect_qwen_cli_target(home) else {
                continue;
            };
            if target.provider_id != provider_id || !cli_session_is_importable(&target.session) {
                continue;
            }
            store.providers.insert(provider_id.to_string(), target.session);
            store_changed = true;
            settings_changed |= attach_stored_oauth_provider(settings, provider_id, true)?;
            if let Some(base_url) = target.base_url.as_deref() {
                settings_changed |= apply_cli_base_url(settings, provider_id, base_url);
            }
            continue;
        }
        if let Some(session) = detect_local_cli_session(provider_id, home) {
            if !cli_session_is_importable(&session) {
                continue;
            }
            store.providers.insert(provider_id.to_string(), session);
            store_changed = true;
            settings_changed |= attach_stored_oauth_provider(settings, provider_id, true)?;
        }
    }
    if store_changed {
        save_store(store_path, &store)?;
    }
    Ok(settings_changed)
}

pub fn detect_local_cli_session(provider_id: &str, home: &Path) -> Option<OAuthSession> {
    match provider_id {
        "kimi" | "kimi-code" => detect_kimi_cli_session(home),
        "anthropic" => detect_claude_cli_session(home),
        "xai" => detect_xai_cli_session(home),
        "google-antigravity" => detect_antigravity_cli_session(home),
        "meta" | "meta-ai" | "muse" => detect_muse_cli_session(home),
        "alibaba-token-plan-intl" | "qwen-cloud" | "alibaba-token-plan" | "alibaba" | "qwen" => {
            detect_qwen_cli_target(home)
                .filter(|target| target.provider_id == canonical_provider_id(provider_id))
                .map(|target| target.session)
        }
        "zai" | "zhipu-bigmodel" => detect_zai_cli_session(home),
        "minimax" | "minimax-cn" => detect_minimax_cli_session(home),
        _ => None,
    }
}

pub async fn resolve_oauth_access_token(provider_id: &str) -> Result<String, String> {
    let store_path = auth_store_path();
    let key = canonical_provider_id(provider_id).to_string();
    if let Some(access) = current_access_token(&store_path, &key)? {
        return Ok(access);
    }
    let _refresh = auth_refresh_lock().lock().await;
    if let Some(access) = current_access_token(&store_path, &key)? {
        return Ok(access);
    }
    let session = stored_session(&store_path, &key)?.ok_or_else(|| {
        format!("{key} のログインが見つかりません。CODETASでログインしてください")
    })?;
    if session.refresh.trim().is_empty() {
        return Err(format!(
            "{key} のログイン期限が切れています。もう一度ログインしてください"
        ));
    }
    let refreshed = refresh_session(&key, &session).await?;
    persist_session(&key, refreshed.clone())?;
    Ok(refreshed.access)
}

pub(crate) fn oauth_account_id(provider_id: &str) -> Option<String> {
    let key = canonical_provider_id(provider_id);
    let path = auth_store_path();
    let _ = current_access_token(&path, key);
    stored_session(&path, key).ok().flatten().and_then(|session| session.account_id)
}

/// Force one OAuth refresh after an upstream authentication rejection. This is
/// intentionally separate from `resolve_oauth_access_token`, which returns a
/// still-valid cached token and therefore cannot recover from server-side
/// revocation or an entitlement transition.
pub async fn force_refresh_oauth_access_token(provider_id: &str) -> Result<String, String> {
    let key = canonical_provider_id(provider_id).to_string();
    if native_oauth_provider_id(&key).is_none() && !is_static_cli_oauth_provider(&key) {
        return Err(format!("{key} はネイティブOAuth更新に未対応です"));
    }
    let _refresh = auth_refresh_lock().lock().await;
    let store_path = auth_store_path();
    let session = if is_static_cli_oauth_provider(&key) {
        detect_local_cli_session(&key, &user_home())
            .ok_or_else(|| format!("{key} のログインが見つかりません"))?
    } else if let Some(session) = stored_session(&store_path, &key)? {
        session
    } else {
        detect_local_cli_session(&key, &user_home())
            .ok_or_else(|| format!("{key} のログインが見つかりません"))?
    };
    if session.refresh.trim().is_empty() && !is_static_cli_oauth_provider(&key) {
        return Err(format!("{key} のrefresh tokenがありません"));
    }
    let refreshed = refresh_session(&key, &session).await?;
    let access = refreshed.access.clone();
    persist_session(&key, refreshed)?;
    Ok(access)
}

pub async fn login_provider_oauth(provider_id: &str) -> Result<OAuthLoginReport, String> {
    let key = canonical_provider_id(provider_id).to_string();
    if key == "google-antigravity" {
        let _refresh = auth_refresh_lock().lock().await;
        let session = refresh_antigravity_cli_session().await?;
        persist_session(&key, session)?;
        return Ok(imported_cli_report(key));
    }
    if key == "meta" {
        let _refresh = auth_refresh_lock().lock().await;
        let session = refresh_muse_cli_session().await?;
        persist_session(&key, session)?;
        return Ok(imported_cli_report(key));
    }
    if is_static_cli_oauth_provider(&key) && key != "meta" {
        let _refresh = auth_refresh_lock().lock().await;
        let session = refresh_static_cli_session(&key).await?;
        persist_session(&key, session)?;
        return Ok(imported_cli_report(key));
    }
    if native_oauth_provider_id(&key).is_none() {
        return Err(format!("{key} はアプリ内OAuthに未対応です"));
    }
    let store_path = auth_store_path();
    if store_session_blocks_cli_import(stored_session(&store_path, &key)?.as_ref()) {
        return refresh_or_relogin_store_session(&store_path, &key).await;
    }
    if let Some(session) = detect_local_cli_session(&key, &user_home()) {
        if !session_needs_refresh(&session) && !session.access.is_empty() {
            persist_session(&key, session)?;
            return Ok(imported_cli_report(key));
        }
        if !session.refresh.trim().is_empty() {
            let _refresh = auth_refresh_lock().lock().await;
            if stored_session(&store_path, &key)?.is_some() {
                return refresh_or_relogin_store_session(&store_path, &key).await;
            }
            if let Ok(fresh) = refresh_session(&key, &session).await {
                persist_session(&key, fresh)?;
                return Ok(imported_cli_report(key));
            }
        }
    }
    complete_browser_login(&key).await
}

async fn refresh_or_relogin_store_session(
    store_path: &Path,
    key: &str,
) -> Result<OAuthLoginReport, String> {
    {
        let _refresh = auth_refresh_lock().lock().await;
        let stored = stored_session(store_path, key)?.ok_or_else(|| {
            format!("{key} のログインが見つかりません。CODETASでログインしてください")
        })?;
        if !session_needs_refresh(&stored) {
            return Ok(OAuthLoginReport {
                provider_id: key.to_string(),
                imported: stored.source == "local-cli",
                source: stored.source,
                message: "既存のCODETASログインを使います。".into(),
            });
        }
        if !stored.refresh.trim().is_empty() {
            if let Ok(fresh) = refresh_session(key, &stored).await {
                persist_session(key, fresh)?;
                return Ok(OAuthLoginReport {
                    provider_id: key.to_string(),
                    imported: false,
                    source: "oauth".into(),
                    message: "既存のCODETASログインを更新しました。".into(),
                });
            }
        }
    }
    // The stored session could not be refreshed (expired or revoked server-side).
    // Prefer a working local CLI login before forcing a browser login.
    if let Some(session) = detect_local_cli_session(key, &user_home()) {
        if cli_session_is_importable(&session) {
            persist_session(key, session)?;
            return Ok(imported_cli_report(key.to_string()));
        }
    }
    complete_browser_login(key).await
}

async fn complete_browser_login(key: &str) -> Result<OAuthLoginReport, String> {
    let session = match key {
        "kimi" => login_kimi().await?,
        "anthropic" => login_anthropic().await?,
        "xai" => login_xai().await?,
        _ => return Err(format!("{key} はアプリ内OAuthに未対応です")),
    };
    persist_session(key, session)?;
    Ok(OAuthLoginReport {
        provider_id: key.to_string(),
        imported: false,
        source: "oauth".into(),
        message: "ログインしました。以降はCODETASが自動更新します。".into(),
    })
}

fn imported_cli_report(key: String) -> OAuthLoginReport {
    OAuthLoginReport {
        provider_id: key,
        imported: true,
        source: "local-cli".into(),
        message: "既存のCLIログインを取り込みました。以降はCODETASが自動更新します。".into(),
    }
}

fn store_session_blocks_cli_import(stored: Option<&OAuthSession>) -> bool {
    stored.is_some()
}

fn persist_session(provider_id: &str, session: OAuthSession) -> Result<(), String> {
    mutate_store(&auth_store_path(), |store| {
        store.providers.insert(provider_id.to_string(), session);
        Ok(())
    })
}

fn attach_stored_oauth_provider(
    settings: &mut GatewaySettings,
    provider_id: &str,
    enable: bool,
) -> Result<bool, String> {
    if let Some(provider) = settings
        .providers
        .iter_mut()
        .find(|provider| provider.id == provider_id)
    {
        if has_non_store_credential(provider) {
            return Ok(false);
        }
        let already_wired = provider.credential.source == CredentialSource::OAuth
            && provider.credential.reference.as_deref() == Some(provider_id)
            && provider.credential.command.is_none();
        if already_wired && (!enable || provider.enabled) {
            return Ok(false);
        }
        provider.api_key_env = None;
        provider.credential = oauth_session_credential(provider_id);
        if enable {
            provider.enabled = true;
        }
        return Ok(true);
    }
    if !enable {
        return Ok(false);
    }
    let mut provider = provider_presets()
        .into_iter()
        .find(|preset| preset.id == provider_id)
        .ok_or_else(|| format!("presetがありません: {provider_id}"))?
        .instantiate(None)?;
    provider.api_key_env = None;
    provider.credential = oauth_session_credential(provider_id);
    provider.enabled = true;
    settings.providers.push(provider);
    Ok(true)
}

fn is_static_cli_oauth_provider(provider_id: &str) -> bool {
    matches!(
        provider_id,
        "meta"
            | "alibaba-token-plan-intl"
            | "alibaba-token-plan"
            | "alibaba"
            | "qwen"
            | "zai"
            | "minimax"
    )
}

fn is_qwen_cli_provider(provider_id: &str) -> bool {
    matches!(
        provider_id,
        "alibaba-token-plan-intl" | "alibaba-token-plan" | "alibaba" | "qwen"
    )
}

fn apply_cli_base_url(settings: &mut GatewaySettings, provider_id: &str, base_url: &str) -> bool {
    let Some(provider) = settings
        .providers
        .iter_mut()
        .find(|provider| provider.id == provider_id)
    else {
        return false;
    };
    if provider.base_url == base_url {
        return false;
    }
    provider.base_url = base_url.to_string();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    use tempfile::tempdir;

    #[test]
    fn parses_antigravity_keyring_payload() {
        let raw = r#"{"auth_method":"consumer","token":{"access_token":"atok","token_type":"Bearer","refresh_token":"rtok","expiry":"2026-08-16T11:05:30+09:00"}}"#;
        let stored = format!("antigravity-oauth:{}", STANDARD.encode(raw));
        let session = parse_antigravity_credentials(&stored).unwrap();
        assert_eq!(session.access, "atok");
        assert_eq!(session.refresh, "rtok");
        assert_eq!(session.expires_at_ms, 1_786_845_930_000);
    }

    #[test]
    fn parses_rfc3339_offsets_as_utc() {
        assert_eq!(chrono_like_to_ms("1970-01-01T09:00:00+09:00"), Ok(0));
        assert_eq!(chrono_like_to_ms("1970-01-01T00:00:00Z"), Ok(0));
    }

    #[test]
    fn parses_kimi_seconds_expiry() {
        let session = parse_kimi_credentials(
            r#"{"access_token":"atok","refresh_token":"rtok","expires_at":1786151767}"#,
        )
        .unwrap();
        assert_eq!(session.access, "atok");
        assert_eq!(session.refresh, "rtok");
        assert_eq!(session.expires_at_ms, 1_786_151_767_000);
        assert_eq!(session.source, "local-cli");
    }

    #[test]
    fn parses_claude_millis_expiry() {
        let session = parse_claude_credentials(
            r#"{"claudeAiOauth":{"accessToken":"atok","refreshToken":"rtok","expiresAt":1786151767000}}"#,
        )
        .unwrap();
        assert_eq!(session.expires_at_ms, 1_786_151_767_000);
    }

    #[test]
    fn parses_grok_rfc3339_expiry() {
        let session = parse_xai_credentials(
            r#"{"https://auth.x.ai::abc":{"key":"atok","refresh_token":"rtok","expires_at":"2026-08-13T01:02:03Z","user_id":"u1","email":"a@b.c"}}"#,
        )
        .unwrap();
        assert_eq!(session.account_id.as_deref(), Some("u1"));
        assert_eq!(session.email.as_deref(), Some("a@b.c"));
        assert!(session.expires_at_ms > 1_700_000_000_000);
    }

    #[test]
    fn parses_muse_cli_auth_preferring_api_key() {
        let session = parse_muse_credentials(
            r#"{"schema_version":1,"providers":{"meta":{"access_token":"dca:atok","obtained_via":"device_code","mechanism":"oauth","api_key":"LLM|key","api_base_url":"https://api.meta.ai/v1","user_email":"User@Example.com"}}}"#,
        )
        .unwrap();
        assert_eq!(session.access, "LLM|key");
        assert!(session.refresh.is_empty());
        assert_eq!(session.email.as_deref(), Some("user@example.com"));
        assert_eq!(session.source, "local-cli");
        assert!(cli_session_is_importable(&session));
    }

    #[test]
    fn parses_muse_cli_account_token_when_api_key_is_absent() {
        let session = parse_muse_credentials(
            r#"{"providers":{"meta":{"access_token":"dca:atok","user_email":"a@b.c"}}}"#,
        )
        .unwrap();
        assert_eq!(session.access, "dca:atok");
        assert!(cli_session_is_importable(&session));
    }

    #[test]
    fn adopt_imports_muse_and_wires_oauth_without_settings_secret() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        fs::create_dir_all(home.join(".config/muse")).unwrap();
        fs::write(
            home.join(".config/muse/auth.json"),
            r#"{"schema_version":1,"providers":{"meta":{"api_key":"LLM|imported","access_token":"dca:atok","user_email":"a@b.c"}}}"#,
        )
        .unwrap();
        let store = directory.path().join("auth.json");
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
        let provider = settings
            .providers
            .iter()
            .find(|provider| provider.id == "meta")
            .unwrap();
        assert!(provider.enabled);
        assert_eq!(provider.credential.source, CredentialSource::OAuth);
        assert_eq!(provider.credential.reference.as_deref(), Some("meta"));
        let settings_json = serde_json::to_string(&settings).unwrap();
        assert!(!settings_json.contains("LLM|imported"));
        assert!(!settings_json.contains("dca:atok"));
        let stored = load_store(&store).unwrap();
        assert_eq!(stored.providers["meta"].access, "LLM|imported");
    }

    #[test]
    fn parses_qwen_settings_env_key() {
        let session = parse_qwen_cli_key(
            r#"{"env":{"BAILIAN_TOKEN_PLAN_API_KEY":"sk-sp-qwen"},"modelProviders":{"openai":[{"id":"qwen3.8-max","envKey":"BAILIAN_TOKEN_PLAN_API_KEY"}]}}"#,
            &["BAILIAN_TOKEN_PLAN_API_KEY", "DASHSCOPE_API_KEY"],
        )
        .unwrap();
        assert_eq!(session.access, "sk-sp-qwen");
        assert!(cli_session_is_importable(&session));
    }

    #[test]
    fn qwen_token_plan_intl_url_selects_the_intl_preset() {
        let target = parse_qwen_cli_target(
            r#"{"env":{"BAILIAN_TOKEN_PLAN_API_KEY":"sk-sp-qwen"},"model":{"name":"qwen3.8-max","baseUrl":"https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1"},"modelProviders":{"openai":[{"id":"qwen3.8-max","envKey":"BAILIAN_TOKEN_PLAN_API_KEY","baseUrl":"https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1"}]}}"#,
        )
        .unwrap();
        assert_eq!(target.provider_id, "alibaba-token-plan-intl");
        assert_eq!(target.session.access, "sk-sp-qwen");
        assert_eq!(target.base_url, None);
    }

    #[test]
    fn qwen_beijing_coding_plan_overrides_the_intl_coding_preset_url() {
        let target = parse_qwen_cli_target(
            r#"{"env":{"BAILIAN_CODING_PLAN_API_KEY":"sk-sp-cn"},"modelProviders":{"openai":[{"id":"qwen3-coder-plus","envKey":"BAILIAN_CODING_PLAN_API_KEY","baseUrl":"https://coding.dashscope.aliyuncs.com/v1"}]}}"#,
        )
        .unwrap();
        assert_eq!(target.provider_id, "alibaba");
        assert_eq!(
            target.base_url.as_deref(),
            Some("https://coding.dashscope.aliyuncs.com/v1")
        );
    }

    #[test]
    fn qwen_settings_do_not_impersonate_zai_without_zai_key() {
        assert!(parse_qwen_cli_key(
            r#"{"env":{"BAILIAN_TOKEN_PLAN_API_KEY":"sk-sp-qwen"},"modelProviders":{"openai":[{"id":"glm-5.2","envKey":"BAILIAN_TOKEN_PLAN_API_KEY"}]}}"#,
            &["ZAI_API_KEY", "ZHIPU_API_KEY", "GLM_API_KEY"],
        )
        .is_none());
    }

    #[test]
    fn adopt_imports_qwen_settings_into_alibaba_token_plan() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        fs::create_dir_all(home.join(".qwen")).unwrap();
        fs::write(
            home.join(".qwen/settings.json"),
            r#"{"env":{"BAILIAN_TOKEN_PLAN_API_KEY":"sk-sp-imported"},"modelProviders":{"openai":[{"id":"qwen3.8-max","envKey":"BAILIAN_TOKEN_PLAN_API_KEY"}]}}"#,
        )
        .unwrap();
        let store = directory.path().join("auth.json");
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
        let provider = settings
            .providers
            .iter()
            .find(|provider| provider.id == "alibaba-token-plan-intl")
            .unwrap();
        assert!(provider.enabled);
        assert_eq!(provider.credential.source, CredentialSource::OAuth);
        assert_eq!(
            provider.credential.reference.as_deref(),
            Some("alibaba-token-plan-intl")
        );
        let settings_json = serde_json::to_string(&settings).unwrap();
        assert!(!settings_json.contains("sk-sp-imported"));
        assert_eq!(
            load_store(&store).unwrap().providers["alibaba-token-plan-intl"].access,
            "sk-sp-imported"
        );
        assert!(!settings.providers.iter().any(|provider| provider.id == "zai"));
        assert!(!settings.providers.iter().any(|provider| provider.id == "minimax"));
    }

    #[test]
    fn adopt_imports_dedicated_zai_and_minimax_cli_files() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        fs::create_dir_all(home.join(".z.ai")).unwrap();
        fs::create_dir_all(home.join(".minimax")).unwrap();
        fs::write(home.join(".z.ai/auth.json"), r#"{"api_key":"zai-imported"}"#).unwrap();
        fs::write(
            home.join(".minimax/auth.json"),
            r#"{"api_key":"minimax-imported"}"#,
        )
        .unwrap();
        let store = directory.path().join("auth.json");
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
        assert_eq!(load_store(&store).unwrap().providers["zai"].access, "zai-imported");
        assert_eq!(
            load_store(&store).unwrap().providers["minimax"].access,
            "minimax-imported"
        );
    }

    #[test]
    fn auth_store_roundtrip_keeps_sessions_private_to_path() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("auth.json");
        let mut store = AuthStoreFile {
            version: AUTH_STORE_VERSION,
            providers: BTreeMap::new(),
        };
        store.providers.insert(
            "kimi".into(),
            OAuthSession {
                access: "atok".into(),
                refresh: "rtok".into(),
                expires_at_ms: 1_786_151_767_000,
                source: "local-cli".into(),
                account_id: None,
                email: None,
            },
        );
        save_store(&path, &store).unwrap();
        store.providers.insert(
            "anthropic".into(),
            OAuthSession {
                access: "atok2".into(),
                refresh: "rtok2".into(),
                expires_at_ms: 1_786_151_767_000,
                source: "oauth".into(),
                account_id: None,
                email: None,
            },
        );
        save_store(&path, &store).unwrap();
        let loaded = load_store(&path).unwrap();
        assert_eq!(loaded.providers["kimi"].refresh, "rtok");
        assert_eq!(loaded.providers["anthropic"].refresh, "rtok2");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn adopt_imports_kimi_and_wires_oauth_without_settings_secret() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        fs::create_dir_all(home.join(".kimi-code/credentials")).unwrap();
        fs::write(
            home.join(".kimi-code/credentials/kimi-code.json"),
            r#"{"access_token":"atok","refresh_token":"rtok","expires_at":1786151767}"#,
        )
        .unwrap();
        let store = directory.path().join("auth.json");
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
        let provider = settings
            .providers
            .iter()
            .find(|provider| provider.id == "kimi")
            .unwrap();
        assert!(provider.enabled);
        assert_eq!(provider.credential.source, CredentialSource::OAuth);
        assert_eq!(provider.credential.reference.as_deref(), Some("kimi"));
        let settings_json = serde_json::to_string(&settings).unwrap();
        assert!(!settings_json.contains("atok"));
        assert!(!settings_json.contains("rtok"));
        let stored = load_store(&store).unwrap();
        assert_eq!(stored.providers["kimi"].access, "atok");
    }

    #[test]
    fn adopt_does_not_replace_environment_api_key() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        fs::create_dir_all(home.join(".kimi-code/credentials")).unwrap();
        fs::write(
            home.join(".kimi-code/credentials/kimi-code.json"),
            r#"{"access_token":"atok","refresh_token":"rtok","expires_at":1786151767}"#,
        )
        .unwrap();
        env::set_var("CODETAS_TEST_KIMI_KEY", "user-key");
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "kimi")
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.credential = ProviderCredential {
            source: CredentialSource::Environment,
            reference: Some("CODETAS_TEST_KIMI_KEY".into()),
            ..ProviderCredential::default()
        };
        settings.providers.push(provider);
        adopt_local_cli_sessions_from(&mut settings, &directory.path().join("auth.json"), &home)
            .unwrap();
        env::remove_var("CODETAS_TEST_KIMI_KEY");
        assert_eq!(
            settings.providers[0].credential.source,
            CredentialSource::Environment
        );
    }

    #[test]
    fn adopt_enables_disabled_env_stub_when_cli_login_exists() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        fs::create_dir_all(home.join(".kimi-code/credentials")).unwrap();
        fs::write(
            home.join(".kimi-code/credentials/kimi-code.json"),
            r#"{"access_token":"atok","refresh_token":"rtok","expires_at":1786151767}"#,
        )
        .unwrap();
        let store = directory.path().join("auth.json");
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "kimi")
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.enabled = false;
        provider.credential = ProviderCredential {
            source: CredentialSource::Environment,
            reference: Some("KIMI_API_KEY".into()),
            ..ProviderCredential::default()
        };
        settings.providers.push(provider);
        assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
        let kimi = settings
            .providers
            .iter()
            .find(|provider| provider.id == "kimi")
            .unwrap();
        assert!(kimi.enabled);
        assert_eq!(kimi.credential.source, CredentialSource::OAuth);
        assert_eq!(kimi.credential.reference.as_deref(), Some("kimi"));
    }

    #[test]
    fn adopt_does_not_resurrect_removed_provider() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        fs::create_dir_all(home.join(".kimi-code/credentials")).unwrap();
        fs::write(
            home.join(".kimi-code/credentials/kimi-code.json"),
            r#"{"access_token":"atok","refresh_token":"rtok","expires_at":1786151767}"#,
        )
        .unwrap();
        let store = directory.path().join("auth.json");
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
        settings.providers.clear();
        assert!(!adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
        assert!(settings.providers.is_empty());
        assert!(load_store(&store).unwrap().providers.contains_key("kimi"));
    }

    #[test]
    fn store_session_is_kept_when_still_valid() {
        let stored = OAuthSession {
            access: "store-access".into(),
            refresh: "store-refresh".into(),
            expires_at_ms: now_ms() + 3_600_000,
            source: "oauth".into(),
            account_id: None,
            email: None,
        };
        assert!(!session_needs_refresh(&stored));
        let cli = OAuthSession {
            access: "cli-access".into(),
            refresh: "cli-refresh".into(),
            expires_at_ms: now_ms() + 3_600_000,
            source: "local-cli".into(),
            account_id: None,
            email: None,
        };
        assert!(cli_session_is_importable(&cli));
        assert!(!session_needs_refresh(&stored) || stored.refresh.is_empty());
    }

    #[test]
    fn store_session_blocks_cli_import_when_present() {
        let stored = OAuthSession {
            access: "store-access".into(),
            refresh: "store-refresh".into(),
            expires_at_ms: 1,
            source: "oauth".into(),
            account_id: None,
            email: None,
        };
        assert!(store_session_blocks_cli_import(Some(&stored)));
        assert!(!store_session_blocks_cli_import(None));
    }

    #[test]
    fn expired_cli_access_without_refresh_is_not_imported() {
        let session = OAuthSession {
            access: "stale".into(),
            refresh: String::new(),
            expires_at_ms: 1,
            source: "local-cli".into(),
            account_id: None,
            email: None,
        };
        assert!(session_needs_refresh(&session));
        assert!(!cli_session_is_importable(&session));
    }

    #[test]
    fn expired_session_without_access_needs_refresh() {
        let session = OAuthSession {
            access: String::new(),
            refresh: "rtok".into(),
            expires_at_ms: 1,
            source: "oauth".into(),
            account_id: None,
            email: None,
        };
        assert!(session_needs_refresh(&session));
    }
}
