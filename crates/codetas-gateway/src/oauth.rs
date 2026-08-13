use crate::config::{
    CredentialSource, CredentialTransport, GatewaySettings, ProviderCredential, ProviderDefinition,
};
use crate::registry::provider_presets;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex as StdMutex, OnceLock,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

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
struct AuthStoreFile {
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

pub fn has_stored_session(provider_id: &str) -> bool {
    read_store(&auth_store_path()).ok().is_some_and(|store| {
        store
            .providers
            .contains_key(canonical_provider_id(provider_id))
    })
}

pub fn native_oauth_provider_id(provider_id: &str) -> Option<&'static str> {
    match provider_id {
        "kimi" | "kimi-code" => Some("kimi"),
        "anthropic" => Some("anthropic"),
        "xai" => Some("xai"),
        _ => None,
    }
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
    for provider_id in ["kimi", "anthropic", "xai"] {
        let activate_existing_stub = settings
            .providers
            .iter()
            .any(|provider| provider.id == provider_id && is_unwired_cli_target(provider));
        if store.providers.contains_key(provider_id) {
            settings_changed |=
                attach_stored_oauth_provider(settings, provider_id, activate_existing_stub)?;
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

pub async fn login_provider_oauth(provider_id: &str) -> Result<OAuthLoginReport, String> {
    let key = canonical_provider_id(provider_id).to_string();
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

struct AuthStoreLock {
    _process: std::sync::MutexGuard<'static, ()>,
    _file: File,
}

fn lock_auth_store(path: &Path) -> Result<AuthStoreLock, String> {
    let process = AUTH_FILE_LOCK
        .get_or_init(|| StdMutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let lock_path = auth_lock_path(path);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|_| "CODETAS auth directory could not be created")?;
    }
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&lock_path)
        .map_err(|_| "CODETAS auth store lock could not be opened".to_string())?;
    file.lock_exclusive()
        .map_err(|_| "CODETAS auth store is busy".to_string())?;
    Ok(AuthStoreLock {
        _process: process,
        _file: file,
    })
}

fn auth_lock_path(store_path: &Path) -> PathBuf {
    let name = store_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("auth.json");
    store_path.with_file_name(format!("{name}.lock"))
}

fn auth_refresh_lock() -> &'static tokio::sync::Mutex<()> {
    AUTH_REFRESH_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn read_store(path: &Path) -> Result<AuthStoreFile, String> {
    let _guard = lock_auth_store(path)?;
    load_store(path)
}

fn mutate_store(
    path: &Path,
    update: impl FnOnce(&mut AuthStoreFile) -> Result<(), String>,
) -> Result<(), String> {
    let _guard = lock_auth_store(path)?;
    let mut store = load_store(path)?;
    update(&mut store)?;
    save_store(path, &store)
}

fn stored_session(path: &Path, provider_id: &str) -> Result<Option<OAuthSession>, String> {
    Ok(read_store(path)?.providers.get(provider_id).cloned())
}

fn current_access_token(path: &Path, provider_id: &str) -> Result<Option<String>, String> {
    let _guard = lock_auth_store(path)?;
    let mut store = load_store(path)?;
    if !store.providers.contains_key(provider_id) {
        if let Some(session) = detect_local_cli_session(provider_id, &user_home()) {
            if cli_session_is_importable(&session) && !session_needs_refresh(&session) {
                let access = session.access.clone();
                store.providers.insert(provider_id.to_string(), session);
                save_store(path, &store)?;
                return Ok(Some(access));
            }
        }
    }
    Ok(store
        .providers
        .get(provider_id)
        .and_then(|session| (!session_needs_refresh(session)).then(|| session.access.clone())))
}

fn cli_session_is_importable(session: &OAuthSession) -> bool {
    !session.access.trim().is_empty()
        && (!session_needs_refresh(session) || !session.refresh.trim().is_empty())
}

fn is_unwired_cli_target(provider: &ProviderDefinition) -> bool {
    matches!(
        provider.credential.source,
        CredentialSource::None | CredentialSource::Environment
    ) && !has_non_store_credential(provider)
}

fn has_non_store_credential(provider: &ProviderDefinition) -> bool {
    match provider.credential.source {
        CredentialSource::Environment => provider
            .credential
            .reference
            .as_deref()
            .or(provider.api_key_env.as_deref())
            .is_some_and(|name| env::var_os(name).is_some_and(|value| !value.is_empty())),
        CredentialSource::Keychain => provider
            .credential
            .reference
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty()),
        CredentialSource::Command => provider.credential.command.is_some(),
        CredentialSource::Forward => true,
        CredentialSource::OAuth => provider.credential.command.is_some(),
        CredentialSource::None => false,
    }
}

fn canonical_provider_id(provider_id: &str) -> &str {
    match provider_id {
        "kimi-code" => "kimi",
        other => other,
    }
}

fn user_home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn load_store(path: &Path) -> Result<AuthStoreFile, String> {
    match fs::read(path) {
        Ok(bytes) => {
            let store: AuthStoreFile = serde_json::from_slice(&bytes)
                .map_err(|_| "CODETAS auth store could not be parsed".to_string())?;
            if store.version != AUTH_STORE_VERSION {
                return Err("CODETAS auth store version is unsupported".into());
            }
            Ok(store)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(AuthStoreFile {
            version: AUTH_STORE_VERSION,
            providers: BTreeMap::new(),
        }),
        Err(_) => Err("CODETAS auth store could not be read".into()),
    }
}

fn save_store(path: &Path, store: &AuthStoreFile) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| "CODETAS auth directory could not be created")?;
    }
    let mut file = AuthStoreFile {
        version: AUTH_STORE_VERSION,
        providers: store.providers.clone(),
    };
    file.providers.retain(|_, session| {
        !session.access.trim().is_empty() || !session.refresh.trim().is_empty()
    });
    let bytes = serde_json::to_vec_pretty(&file)
        .map_err(|_| "CODETAS auth store could not be encoded".to_string())?;
    atomic_write_private(path, &bytes)
}

fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let directory = path
        .parent()
        .ok_or_else(|| "CODETAS auth store path is invalid".to_string())?;
    let sequence = AUTH_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = directory.join(format!(
        ".auth.{}.{sequence}.{}.tmp",
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    let _ = fs::remove_file(&temp);
    {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp)
            .map_err(|_| "CODETAS auth store could not be written")?;
        if let Err(_) = file.write_all(bytes).and_then(|_| file.sync_all()) {
            let _ = fs::remove_file(&temp);
            return Err("CODETAS auth store could not be written".into());
        }
    }
    if let Err(_) = replace_auth_file(&temp, path) {
        if path.exists() {
            let _ = fs::remove_file(&temp);
        }
        return Err("CODETAS auth store could not be replaced".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| "CODETAS auth store could not be protected".to_string())?;
        if fs::metadata(path)
            .map(|meta| meta.permissions().mode() & 0o777)
            .unwrap_or(0)
            != 0o600
        {
            return Err("CODETAS auth store could not be protected".into());
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_auth_file(temporary: &Path, path: &Path) -> std::io::Result<()> {
    fs::rename(temporary, path)
}

#[cfg(windows)]
fn replace_auth_file(temporary: &Path, path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return fs::rename(temporary, path);
    }
    let backup = path.with_file_name(format!(
        ".auth-replace-{}-{}.bak",
        std::process::id(),
        AUTH_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_file(&backup);
    match fs::rename(path, &backup) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return fs::rename(temporary, path);
        }
        Err(error) => return Err(error),
    }
    match fs::rename(temporary, path) {
        Ok(()) => {
            let _ = fs::remove_file(backup);
            Ok(())
        }
        Err(error) => {
            let _ = fs::rename(&backup, path);
            Err(error)
        }
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

fn read_claude_secure_storage(home: &Path) -> Option<String> {
    let file = claude_credentials_path(home);
    let from_file = fs::read_to_string(&file).ok();
    if env::var_os("CLAUDE_CONFIG_DIR").is_some() {
        return from_file.or_else(read_claude_keychain);
    }
    read_claude_keychain().or(from_file)
}

fn claude_credentials_path(home: &Path) -> PathBuf {
    env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(".claude"))
        .join(".credentials.json")
}

fn read_claude_keychain() -> Option<String> {
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
    Some(OAuthSession {
        access,
        refresh,
        expires_at_ms: json_expiry_ms(&value, &["expires_at", "expiresAt", "expires"]),
        source: "local-cli".into(),
        account_id: json_string(&value, &["user_id", "userId", "accountId"]),
        email: json_string(&value, &["email"]),
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

fn form_body(pairs: &[(&str, &str)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in pairs {
        serializer.append_pair(name, value);
    }
    serializer.finish()
}

fn json_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(ToString::to_string)
    })
}

fn json_expiry_ms(value: &Value, keys: &[&str]) -> i64 {
    for key in keys {
        if let Some(number) = value.get(*key).and_then(Value::as_i64) {
            return normalize_expiry_ms(number);
        }
        if let Some(number) = value.get(*key).and_then(Value::as_f64) {
            return normalize_expiry_ms(number as i64);
        }
        if let Some(text) = value.get(*key).and_then(Value::as_str) {
            if let Ok(number) = text.parse::<i64>() {
                return normalize_expiry_ms(number);
            }
            if let Ok(parsed) = chrono_like_to_ms(text) {
                return parsed;
            }
        }
    }
    0
}

fn normalize_expiry_ms(value: i64) -> i64 {
    if value <= 0 {
        0
    } else if value < 100_000_000_000 {
        value.saturating_mul(1_000)
    } else {
        value
    }
}

fn chrono_like_to_ms(value: &str) -> Result<i64, ()> {
    let trimmed = value.trim();
    let (date, rest) = trimmed
        .split_once('T')
        .or_else(|| trimmed.split_once(' '))
        .ok_or(())?;
    let mut date_parts = date.split('-');
    let year = date_parts
        .next()
        .ok_or(())?
        .parse::<i32>()
        .map_err(|_| ())?;
    let month = date_parts
        .next()
        .ok_or(())?
        .parse::<u32>()
        .map_err(|_| ())?;
    let day = date_parts
        .next()
        .ok_or(())?
        .parse::<u32>()
        .map_err(|_| ())?;
    let time = rest.trim_end_matches('Z');
    let time = time.split(['+', '-']).next().unwrap_or(time);
    let mut time_parts = time.split(':');
    let hour = time_parts
        .next()
        .ok_or(())?
        .parse::<u32>()
        .map_err(|_| ())?;
    let minute = time_parts
        .next()
        .ok_or(())?
        .parse::<u32>()
        .map_err(|_| ())?;
    let second = time_parts
        .next()
        .unwrap_or("0")
        .split('.')
        .next()
        .unwrap_or("0")
        .parse::<u32>()
        .map_err(|_| ())?;
    civil_utc_to_ms(year, month, day, hour, minute, second)
}

fn civil_utc_to_ms(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Result<i64, ()> {
    if !(1..=12).contains(&month) || day == 0 || hour > 23 || minute > 59 || second > 60 {
        return Err(());
    }
    let days = days_from_civil(year, month, day)?;
    Ok(days
        .saturating_mul(86_400)
        .saturating_add(i64::from(hour) * 3_600)
        .saturating_add(i64::from(minute) * 60)
        .saturating_add(i64::from(second))
        .saturating_mul(1_000))
}

fn days_from_civil(year: i32, month: u32, day: u32) -> Result<i64, ()> {
    let dim = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max_day = if month == 2 && leap {
        29
    } else {
        dim[month as usize]
    };
    if day > max_day {
        return Err(());
    }
    let mut days = 0_i64;
    for y in 1970..year {
        days += if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
    }
    for m in 1..month {
        days += i64::from(if m == 2 && leap { 29 } else { dim[m as usize] });
    }
    Ok(days + i64::from(day) - 1)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn session_needs_refresh(session: &OAuthSession) -> bool {
    session.access.trim().is_empty()
        || session.expires_at_ms <= 0
        || session.expires_at_ms <= now_ms() + REFRESH_SKEW_MS
}

async fn refresh_session(
    provider_id: &str,
    session: &OAuthSession,
) -> Result<OAuthSession, String> {
    match provider_id {
        "kimi" => refresh_kimi_token(&session.refresh).await,
        "anthropic" => refresh_anthropic_token(&session.refresh).await,
        "xai" => refresh_xai_token(&session.refresh).await,
        _ => Err(format!("{provider_id} の自動更新に未対応です")),
    }
}

async fn refresh_kimi_token(refresh_token: &str) -> Result<OAuthSession, String> {
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

async fn login_kimi() -> Result<OAuthSession, String> {
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

fn parse_kimi_token_response(
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

fn kimi_common_headers() -> reqwest::header::HeaderMap {
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

fn kimi_device_id() -> String {
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

async fn refresh_anthropic_token(refresh_token: &str) -> Result<OAuthSession, String> {
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

async fn login_anthropic() -> Result<OAuthSession, String> {
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

fn parse_anthropic_token_response(
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

async fn refresh_xai_token(refresh_token: &str) -> Result<OAuthSession, String> {
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

async fn login_xai() -> Result<OAuthSession, String> {
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

fn parse_xai_token_response(
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

async fn discover_xai_token_endpoint() -> Result<String, String> {
    Ok(discover_xai_endpoints().await?.1)
}

async fn discover_xai_endpoints() -> Result<(String, String), String> {
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

fn validate_xai_endpoint(raw: &str) -> Result<(), String> {
    let parsed = url::Url::parse(raw).map_err(|_| "xAI OAuth discovery failed".to_string())?;
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    if parsed.scheme() == "https" && (host == "x.ai" || host.ends_with(".x.ai")) {
        Ok(())
    } else {
        Err("xAI OAuth discovery failed".into())
    }
}

async fn post_json(url: &str, body: Value) -> Result<Value, String> {
    let response = oauth_http_client()?
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|_| "OAuth token request failed".to_string())?;
    if !response.status().is_success() {
        return Err("OAuth token request failed".into());
    }
    response
        .json()
        .await
        .map_err(|_| "OAuth token request failed".to_string())
}

async fn post_form(url: &str, fields: &[(&str, &str)]) -> Result<Value, String> {
    let body = form_body(fields);
    let response = oauth_http_client()?
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
        .send()
        .await
        .map_err(|_| "OAuth token request failed".to_string())?;
    if !response.status().is_success() {
        return Err("OAuth token request failed".into());
    }
    response
        .json()
        .await
        .map_err(|_| "OAuth token request failed".to_string())
}

fn oauth_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| "OAuth HTTP client could not be created".to_string())
}

fn pkce_pair() -> (String, String) {
    let verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let digest = Sha256::digest(verifier.as_bytes());
    (verifier, URL_SAFE_NO_PAD.encode(digest))
}

async fn wait_for_oauth_code(
    port: u16,
    path: &str,
    expected_state: &str,
    authorize_url: &str,
) -> Result<String, String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|_| "OAuth callback could not listen on loopback".to_string())?;
    open_browser(authorize_url)?;
    let accept = async {
        loop {
            let (mut stream, _) = listener
                .accept()
                .await
                .map_err(|_| "OAuth callback was not received".to_string())?;
            let mut buffer = vec![0_u8; 8192];
            let read = stream
                .read(&mut buffer)
                .await
                .map_err(|_| "OAuth callback was not received".to_string())?;
            let request = String::from_utf8_lossy(&buffer[..read]);
            let request_line = request.lines().next().unwrap_or_default();
            let Some(target) = request_line.split_whitespace().nth(1) else {
                let _ = write_callback_response(&mut stream, false).await;
                continue;
            };
            let parsed = url::Url::parse(&format!("http://127.0.0.1{target}"))
                .map_err(|_| "OAuth callback was invalid".to_string())?;
            if parsed.path() != path {
                let _ = write_callback_response(&mut stream, false).await;
                continue;
            }
            let query: BTreeMap<String, String> = parsed
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
            if query.get("state").map(String::as_str) != Some(expected_state) {
                let _ = write_callback_response(&mut stream, false).await;
                return Err("OAuth callback state did not match".into());
            }
            let Some(code) = query.get("code").cloned().filter(|value| !value.is_empty()) else {
                let _ = write_callback_response(&mut stream, false).await;
                return Err("OAuth callback did not include an authorization code".into());
            };
            let _ = write_callback_response(&mut stream, true).await;
            return Ok(code);
        }
    };
    tokio::time::timeout(LOGIN_TIMEOUT, accept)
        .await
        .map_err(|_| "OAuth login timed out".to_string())?
}

async fn write_callback_response(
    stream: &mut tokio::net::TcpStream,
    success: bool,
) -> Result<(), String> {
    let body = if success {
        "<html><body>CODETAS login finished. You can close this window.</body></html>"
    } else {
        "<html><body>CODETAS login failed. Return to the app and try again.</body></html>"
    };
    let status = if success { "200 OK" } else { "400 Bad Request" };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|_| "OAuth callback response failed".to_string())
}

fn open_browser(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|_| "OAuth login URL is invalid".to_string())?;
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return Err("OAuth login URL is invalid".into());
    }
    let status = {
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("open").arg(url).status()
        }
        #[cfg(target_os = "linux")]
        {
            std::process::Command::new("xdg-open").arg(url).status()
        }
        #[cfg(target_os = "windows")]
        {
            let safe = url.replace('"', "");
            std::process::Command::new("cmd")
                .arg("/C")
                .arg(format!("start \"\" \"{safe}\""))
                .status()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            return Err("OAuth login cannot open a browser on this platform".into());
        }
    };
    status
        .map_err(|_| "OAuth login could not open a browser".to_string())
        .and_then(|code| {
            if code.success() {
                Ok(())
            } else {
                Err("OAuth login could not open a browser".into())
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
