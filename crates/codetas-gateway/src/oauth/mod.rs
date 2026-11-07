use super::*;
use crate::config::AccountPoolSettings;
use crate::config::CredentialSource;
use crate::config::CredentialTransport;
use crate::config::GatewaySettings;
use crate::config::ProviderCredential;
use crate::registry::provider_presets;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;
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
// Google Antigravity uses Google OAuth and its token endpoint requires a
// client secret even with PKCE. Antigravity's own client id/secret are embedded
// in the `agy` binary and Google's terms do not permit another app to reuse
// them; the public Gemini CLI client is likewise restricted to Gemini CLI.
// In-app login therefore runs only with a client the operator is authorized to
// use, supplied through these variables. Without them, CODETAS adds accounts by
// importing the account currently signed in to `agy`.
const ANTIGRAVITY_CLIENT_ID_ENV: &str = "CODETAS_ANTIGRAVITY_CLIENT_ID";
const ANTIGRAVITY_CLIENT_SECRET_ENV: &str = "CODETAS_ANTIGRAVITY_CLIENT_SECRET";
const ANTIGRAVITY_AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const ANTIGRAVITY_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const ANTIGRAVITY_USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo";
const ANTIGRAVITY_CALLBACK_PORT: u16 = 53_118;
const ANTIGRAVITY_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform \
https://www.googleapis.com/auth/userinfo.email \
https://www.googleapis.com/auth/userinfo.profile \
https://www.googleapis.com/auth/experimentsandconfigs";
#[cfg(target_os = "macos")]
const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

fn oauth_client_id(env_var: &str, default: &str) -> String {
    env::var(env_var)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

pub(crate) fn antigravity_native_login_configured() -> bool {
    let set = |name: &str| {
        env::var(name)
            .ok()
            .is_some_and(|value| !value.trim().is_empty())
    };
    (set(ANTIGRAVITY_CLIENT_ID_ENV) && set(ANTIGRAVITY_CLIENT_SECRET_ENV))
        || antigravity_client_from_private_file().is_some()
}

/// The client secret is never committed. Operators place the pair in a private
/// file next to `auth.json` (mode 0600) instead of sharing it in the repo.
pub(crate) fn antigravity_client_config_path() -> PathBuf {
    auth_store_path().with_file_name("antigravity-oauth-client.json")
}

fn antigravity_client_from_private_file() -> Option<(String, String)> {
    let raw = fs::read_to_string(antigravity_client_config_path()).ok()?;
    parse_antigravity_client_file(&raw)
}

pub(crate) fn parse_antigravity_client_file(raw: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let id = json_string(&value, &["clientId", "client_id"])?;
    let secret = json_string(&value, &["clientSecret", "client_secret"])?;
    Some((id, secret))
}

/// Google's token endpoint binds the client secret to the client id, so both
/// variables must be set together.
fn antigravity_client_credentials() -> Result<(String, String), String> {
    let id = env::var(ANTIGRAVITY_CLIENT_ID_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    let secret = env::var(ANTIGRAVITY_CLIENT_SECRET_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    match (id, secret) {
        (Some(id), Some(secret)) => Ok((id, secret)),
        _ => antigravity_client_from_private_file().ok_or_else(|| {
            format!(
                "Antigravity のアプリ内ログインには、利用が許可された Google OAuth クライアントが必要です。{ANTIGRAVITY_CLIENT_ID_ENV} と {ANTIGRAVITY_CLIENT_SECRET_ENV} を設定するか、{} に clientId / clientSecret を置いてください。agy でログインして取り込むこともできます",
                antigravity_client_config_path().display()
            )
        }),
    }
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
    OAuthProviderDescriptor {
        id: "kimi",
        aliases: &["kimi-code"],
        display_name: "Kimi",
        flow: "device-code",
        native_login: true,
        cli_import: true,
    },
    OAuthProviderDescriptor {
        id: "anthropic",
        aliases: &[],
        display_name: "Anthropic",
        flow: "authorization-code-pkce",
        native_login: true,
        cli_import: true,
    },
    OAuthProviderDescriptor {
        id: "xai",
        aliases: &[],
        display_name: "xAI",
        flow: "authorization-code-pkce",
        native_login: true,
        cli_import: true,
    },
    OAuthProviderDescriptor {
        id: "google-antigravity",
        aliases: &[],
        display_name: "Google Antigravity",
        flow: "authorization-code-pkce",
        native_login: true,
        cli_import: true,
    },
    OAuthProviderDescriptor {
        id: "meta",
        aliases: &["meta-ai", "muse"],
        display_name: "Meta Muse",
        flow: "cli-import",
        native_login: false,
        cli_import: true,
    },
    OAuthProviderDescriptor {
        id: "alibaba-token-plan-intl",
        aliases: &["qwen-cloud"],
        display_name: "Qwen Token Plan",
        flow: "cli-import",
        native_login: false,
        cli_import: true,
    },
    OAuthProviderDescriptor {
        id: "alibaba-token-plan",
        aliases: &[],
        display_name: "Qwen Token Plan (Beijing)",
        flow: "cli-import",
        native_login: false,
        cli_import: true,
    },
    OAuthProviderDescriptor {
        id: "alibaba",
        aliases: &[],
        display_name: "Qwen Coding Plan",
        flow: "cli-import",
        native_login: false,
        cli_import: true,
    },
    OAuthProviderDescriptor {
        id: "qwen",
        aliases: &[],
        display_name: "Qwen International",
        flow: "cli-import",
        native_login: false,
        cli_import: true,
    },
    OAuthProviderDescriptor {
        id: "zai",
        aliases: &["zhipu-bigmodel"],
        display_name: "Z.AI GLM",
        flow: "cli-import",
        native_login: false,
        cli_import: true,
    },
    OAuthProviderDescriptor {
        id: "minimax",
        aliases: &["minimax-cn"],
        display_name: "MiniMax",
        flow: "cli-import",
        native_login: false,
        cli_import: true,
    },
    OAuthProviderDescriptor {
        id: "mistral",
        aliases: &[],
        display_name: "Mistral Vibe",
        flow: "cli-import",
        native_login: false,
        cli_import: true,
    },
];

pub fn oauth_provider_registry() -> &'static [OAuthProviderDescriptor] {
    OAUTH_PROVIDER_REGISTRY
}

impl OAuthProviderDescriptor {
    /// Reading this provider's CLI login can put up a Keychain prompt.
    pub fn cli_login_can_prompt(&self) -> bool {
        cli_login_can_prompt(self.id)
    }
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

/// Multiple OAuth accounts for one provider live in the same store under
/// `provider#account` keys. Routing and credential resolution must keep the
/// full key for lookup, but use the base provider id for login / refresh.
pub fn session_key_provider(key: &str) -> &str {
    key.split_once('#').map(|(base, _)| base).unwrap_or(key)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthStoredAccount {
    pub store_key: String,
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub expires_at_ms: i64,
    pub source: String,
}

impl OAuthStoredAccount {
    fn from_session(store_key: &str, session: &OAuthSession) -> Self {
        Self {
            store_key: store_key.to_string(),
            account_id: session.account_id.clone(),
            email: session.email.clone(),
            expires_at_ms: session.expires_at_ms,
            source: session.source.clone(),
        }
    }
}

/// Every stored OAuth session for a provider: the legacy single-session key
/// (`provider`) and any account-scoped keys (`provider#account`). Tokens are
/// never returned; only identity and expiry are exposed.
pub fn list_stored_oauth_accounts(provider_id: &str) -> Result<Vec<OAuthStoredAccount>, String> {
    let base = canonical_provider_id(provider_id);
    let store = read_store(&auth_store_path())?;
    let prefix = format!("{base}#");
    let mut accounts = store
        .providers
        .iter()
        .filter(|(key, _)| key.as_str() == base || key.starts_with(&prefix))
        .map(|(key, session)| OAuthStoredAccount::from_session(key, session))
        .collect::<Vec<_>>();
    accounts.sort_by_key(|account| account.store_key != base);
    Ok(accounts)
}

/// Sign in a *new* account even when this provider already has a stored
/// session. Re-signing into an existing account reuses its store key, so it
/// updates that account instead of creating a duplicate.
pub async fn login_provider_oauth_new_account(
    provider_id: &str,
) -> Result<OAuthStoredAccount, String> {
    let base = canonical_provider_id(provider_id).to_string();
    let Some(native) = native_oauth_provider_id(&base) else {
        return Err(format!("{base} はアプリ内OAuthに未対応です"));
    };
    let session = match native {
        "kimi" => login_kimi().await?,
        "anthropic" => login_anthropic().await?,
        "xai" => login_xai().await?,
        "google-antigravity" => {
            if antigravity_native_login_configured() {
                login_antigravity().await?
            } else {
                // No permitted OAuth client is configured, so add the account
                // that `agy` is signed into right now. The user switches
                // accounts in `agy` and presses Add again.
                current_antigravity_cli_session().await?
            }
        }
        _ => return Err(format!("{native} はアプリ内OAuthに未対応です")),
    };
    let existing = list_stored_oauth_accounts(&base)?
        .into_iter()
        .find(|account| same_oauth_identity(account, &session));
    let store_key = existing
        .map(|account| account.store_key)
        .unwrap_or_else(|| format!("{base}#{}", oauth_account_slug(&session)));
    persist_session(&store_key, session)?;
    let stored = stored_session(&auth_store_path(), &store_key)?
        .ok_or_else(|| "CODETAS auth store could not be read after login".to_string())?;
    Ok(OAuthStoredAccount::from_session(&store_key, &stored))
}

pub fn remove_stored_oauth_account(store_key: &str) -> Result<(), String> {
    let base = session_key_provider(store_key);
    if store_key.trim().is_empty() || base.trim().is_empty() {
        return Err("account store key is empty".into());
    }
    // A reference saved under an alias names the canonical provider's session.
    let store_key = canonical_provider_id(store_key);
    mutate_store(&auth_store_path(), |store| {
        store.providers.remove(store_key);
        Ok(())
    })
}

/// Two credential references name the same stored login: an alias such as
/// `kimi-code` shares the canonical provider's session.
pub fn same_oauth_login(left: &str, right: &str) -> bool {
    canonical_provider_id(left) == canonical_provider_id(right)
}

fn same_oauth_identity(account: &OAuthStoredAccount, session: &OAuthSession) -> bool {
    // An older record may hold only an email while the new session carries an
    // account id; then the email decides. When both sides have an id, the id
    // alone decides.
    let present = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let (account_id, session_id) = (present(&account.account_id), present(&session.account_id));
    // Two different ids are two different accounts, whatever their emails say:
    // matching on the email would overwrite the other account's tokens.
    if let (Some(left), Some(right)) = (&account_id, &session_id) {
        return left == right;
    }
    matches!(
        (present(&account.email), present(&session.email)),
        (Some(left), Some(right)) if left.eq_ignore_ascii_case(&right)
    )
}

fn oauth_account_slug(session: &OAuthSession) -> String {
    let identity = session
        .account_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or(session
            .email
            .as_deref()
            .filter(|value| !value.trim().is_empty()));
    let Some(identity) = identity else {
        return format!("account-{}", now_ms());
    };
    let readable = identity
        .chars()
        .map(|character| {
            if character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '_')
            {
                character
            } else if character.is_ascii_uppercase() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(32)
        .collect::<String>();
    // Truncation alone can collide for two long identities sharing a prefix,
    // and a store-key collision would overwrite another account's tokens.
    // Append a digest of the full identity so keys stay unique.
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(identity.as_bytes());
    let suffix = format!("{digest:x}");
    if readable.is_empty() {
        format!("account-{}", &suffix[..12])
    } else {
        format!("{readable}-{}", &suffix[..8])
    }
}

pub fn native_oauth_provider_id(provider_id: &str) -> Option<&'static str> {
    OAUTH_PROVIDER_REGISTRY
        .iter()
        .find(|descriptor| {
            descriptor.native_login
                && (descriptor.id == provider_id || descriptor.aliases.contains(&provider_id))
        })
        .map(|descriptor| descriptor.id)
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

fn cli_import_is_dismissed(pool: &AccountPoolSettings, provider_id: &str) -> bool {
    let provider_id = canonical_provider_id(provider_id);
    pool.dismissed_cli_imports
        .iter()
        .any(|id| canonical_provider_id(id) == provider_id)
}

/// Remember a removed login so startup adoption does not bring it back. Adoption
/// only knows canonical ids, and a connection can be saved under an alias such as
/// `kimi-code`, so the canonical id is what gets recorded. A token lookup does not
/// see this list: a request already in flight for the removed login can still
/// import it again.
pub fn dismiss_cli_import(pool: &mut AccountPoolSettings, provider_id: &str) {
    if !cli_import_is_dismissed(pool, provider_id) {
        pool.dismissed_cli_imports
            .push(canonical_provider_id(provider_id).to_string());
    }
}

/// Registering a provider again undoes `dismiss_cli_import`.
pub fn restore_cli_import(pool: &mut AccountPoolSettings, provider_id: &str) {
    let provider_id = canonical_provider_id(provider_id);
    pool.dismissed_cli_imports
        .retain(|id| canonical_provider_id(id) != provider_id);
}

/// Whether this connection may take a CLI login at all. A Vibe key belongs to a
/// Mistral plan, so a `mistral` connection pointing anywhere else must not carry it
/// to that host.
pub fn provider_may_take_cli_login(provider: &ProviderDefinition) -> bool {
    provider.id != "mistral" || crate::compat::is_mistral_chat_endpoint(&provider.base_url)
}

/// Registering turns a connection on, so a connection that cannot take a CLI login
/// needs a credential of its own first.
pub fn registration_may_enable_provider(provider: &ProviderDefinition) -> bool {
    provider_may_take_cli_login(provider) || has_non_store_credential(provider)
}

/// Adoption would only keep a Vibe key next to the key this connection pays with,
/// where it shows up as an account it does not use. Other providers still import
/// here: their imported session is what a later OAuth switch starts from.
fn adoption_leaves_connection_alone(provider: &ProviderDefinition) -> bool {
    !provider_may_take_cli_login(provider)
        || (provider.id == "mistral" && has_non_store_credential(provider))
}

pub fn adopt_local_cli_sessions_from(
    settings: &mut GatewaySettings,
    store_path: &Path,
    home: &Path,
) -> Result<bool, String> {
    let mut settings_changed = false;
    let adoptable: Vec<&'static str> = OAUTH_PROVIDER_REGISTRY
        .iter()
        .filter(|item| item.cli_import)
        .map(|item| item.id)
        .filter(|provider_id| {
            // A provider with pool accounts is explicitly managed from the
            // Accounts section. Importing whatever the local CLI is signed into
            // would add a surprise third account and, worse, could point the
            // provider credential at it. A login the user removed stays removed
            // until they register it again.
            let pool = &settings.account_pool;
            !pool.accounts.iter().any(|a| a.provider_id == *provider_id)
                && !cli_import_is_dismissed(pool, provider_id)
                && !settings
                    .providers
                    .iter()
                    .any(|p| p.id == *provider_id && adoption_leaves_connection_alone(p))
        })
        .collect();
    let keeps_store_session = |provider_id: &str| {
        !is_qwen_cli_provider(provider_id) && !matches!(provider_id, "meta" | "zai" | "minimax")
    };
    // Reading a CLI login can wait on a Keychain prompt. Do it before taking the
    // store lock, which every OAuth provider's token lookup needs. One convergence
    // adopts twice, and the limit keeps that to one prompt.
    let stored = read_store(store_path)?;
    let mut detected: BTreeMap<&str, OAuthSession> = adoptable
        .iter()
        .copied()
        .filter(|id| !is_qwen_cli_provider(id))
        .filter(|id| !keeps_store_session(id) || !stored.providers.contains_key(*id))
        .filter_map(|id| detect_local_cli_session_limited(id, home).map(|session| (id, session)))
        .collect();
    let _guard = lock_auth_store(store_path)?;
    let mut store = load_store(store_path)?;
    let mut store_changed = false;
    let mut replaced_keys = Vec::new();
    for provider_id in adoptable {
        let activate_existing_stub = settings
            .providers
            .iter()
            .any(|provider| provider.id == provider_id && is_unwired_cli_target(provider));
        // Importing a login turns on a new connection or an unwired stub. A connection
        // already wired to this login and switched off was switched off on purpose.
        let enable_on_import = activate_existing_stub
            || settings
                .providers
                .iter()
                .all(|provider| provider.id != provider_id);
        if store.providers.contains_key(provider_id) && keeps_store_session(provider_id) {
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
            if store
                .providers
                .insert(provider_id.to_string(), target.session.clone())
                .is_some_and(|previous| previous.access != target.session.access)
            {
                replaced_keys.push(provider_id);
            }
            store_changed = true;
            settings_changed |=
                attach_stored_oauth_provider(settings, provider_id, enable_on_import)?;
            if let Some(base_url) = target.base_url.as_deref() {
                settings_changed |= apply_cli_base_url(settings, provider_id, base_url);
            }
            continue;
        }
        if let Some(session) = detected.remove(provider_id) {
            if !cli_session_is_importable(&session) {
                continue;
            }
            let access = session.access.clone();
            if store
                .providers
                .insert(provider_id.to_string(), session)
                .is_some_and(|previous| previous.access != access)
            {
                replaced_keys.push(provider_id);
            }
            store_changed = true;
            settings_changed |=
                attach_stored_oauth_provider(settings, provider_id, enable_on_import)?;
        }
    }
    if store_changed {
        save_store(store_path, &store)?;
    }
    // A request already sent with the old key can still come back rejected.
    for provider_id in replaced_keys {
        note_cli_key_changed(store_path, provider_id);
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
        "mistral" => detect_vibe_cli_session(home),
        _ => None,
    }
}

/// The logins that can be in the Keychain, so that reading one can put up a prompt.
fn cli_login_can_prompt(provider_id: &str) -> bool {
    matches!(
        canonical_provider_id(provider_id),
        "anthropic" | "google-antigravity" | "meta" | "mistral"
    )
}

const CLI_LOGIN_READ_INTERVAL: Duration = Duration::from_secs(60);
/// Long enough for the Keychain read a caller waits for, which is cut off at 30 s.
const CLI_LOGIN_READ_WAIT: Duration = Duration::from_secs(35);

type CliLoginKey = (String, PathBuf);

/// One read in progress, and its answer for whoever waits on it.
#[derive(Default)]
struct RunningCliLoginRead {
    answer: StdMutex<Option<Option<OAuthSession>>>,
    finished: std::sync::Condvar,
}

impl RunningCliLoginRead {
    fn wait(&self, limit: Duration) -> Option<OAuthSession> {
        let answer = self
            .answer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (answer, _) = self
            .finished
            .wait_timeout_while(answer, limit, |answer| answer.is_none())
            .unwrap_or_else(|error| error.into_inner());
        answer.clone().flatten()
    }

    fn finish(&self, session: Option<OAuthSession>) {
        *self
            .answer
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(session);
        self.finished.notify_all();
    }
}

struct CliLoginReadState {
    last: Instant,
    running: Option<std::sync::Arc<RunningCliLoginRead>>,
}

fn cli_login_reads() -> std::sync::MutexGuard<'static, HashMap<CliLoginKey, CliLoginReadState>> {
    static READS: OnceLock<StdMutex<HashMap<CliLoginKey, CliLoginReadState>>> = OnceLock::new();
    READS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// One read per interval. A caller that arrives during the read waits for its answer,
/// up to `wait`, so a request is not told there is no login while one is being read.
fn limited_cli_login_read(
    key: CliLoginKey,
    wait: Duration,
    read: impl FnOnce() -> Option<OAuthSession>,
) -> Option<OAuthSession> {
    let running = std::sync::Arc::new(RunningCliLoginRead::default());
    {
        let mut reads = cli_login_reads();
        let now = Instant::now();
        if let Some(state) = reads.get(&key) {
            let age = now.duration_since(state.last);
            match &state.running {
                // A read older than the wait ended without an answer: nothing to wait on.
                Some(other) if age < wait => {
                    let other = other.clone();
                    drop(reads);
                    return other.wait(wait);
                }
                _ if age < CLI_LOGIN_READ_INTERVAL => return None,
                _ => {}
            }
        }
        reads.insert(
            key.clone(),
            CliLoginReadState {
                last: now,
                running: Some(running.clone()),
            },
        );
    }
    let session = read();
    running.finish(session.clone());
    // The interval starts again here, so a prompt left unanswered does not use it up.
    cli_login_reads().insert(
        key,
        CliLoginReadState {
            last: Instant::now(),
            running: None,
        },
    );
    session
}

/// Takes this interval's read for a read that happens elsewhere. False if it is gone.
fn take_cli_login_read(key: CliLoginKey) -> bool {
    let mut reads = cli_login_reads();
    let now = Instant::now();
    if reads
        .get(&key)
        .is_some_and(|state| now.duration_since(state.last) < CLI_LOGIN_READ_INTERVAL)
    {
        return false;
    }
    reads.insert(
        key,
        CliLoginReadState {
            last: now,
            running: None,
        },
    );
    true
}

/// `detect_local_cli_session` for reads nobody asked for: token lookups, a static
/// key's 401/403 recovery and adoption. A login that can prompt reads the Keychain at
/// most once a minute, whatever the last read found, and no answer means "not now".
/// A person asking for a re-import uses `detect_local_cli_session` directly.
///
/// A plain file is cheap and a limit would only delay a rotated key, so it is never
/// limited: that covers every other login and Vibe's `.env`, which comes before its
/// Keychain item. The other three read the Keychain first and use their file only if
/// it has nothing, so the file cannot answer on its own and the whole read is limited.
/// An imported Antigravity login's rejection is limited in
/// `gate_agy_refresh_after_rejection`. Refreshing its expired token is not limited,
/// since the request has no other token: it runs under Antigravity's own lock.
pub(crate) fn detect_local_cli_session_limited(
    provider_id: &str,
    home: &Path,
) -> Option<OAuthSession> {
    // Only macOS has a Keychain to wait on. Tests exercise the limit everywhere.
    if !cfg!(any(target_os = "macos", test)) || !cli_login_can_prompt(provider_id) {
        return detect_local_cli_session(provider_id, home);
    }
    let provider = canonical_provider_id(provider_id);
    if provider == "mistral" {
        return detect_vibe_limited(home, detect_vibe_keychain_session);
    }
    limited_cli_login_read(
        (provider.to_string(), home.to_path_buf()),
        CLI_LOGIN_READ_WAIT,
        || detect_local_cli_session(provider_id, home),
    )
}

fn detect_vibe_limited(
    home: &Path,
    keychain: impl FnOnce() -> Option<OAuthSession>,
) -> Option<OAuthSession> {
    detect_vibe_env_session(home).or_else(|| {
        limited_cli_login_read(
            ("mistral".to_string(), home.to_path_buf()),
            CLI_LOGIN_READ_WAIT,
            keychain,
        )
    })
}

type CliKeyChanges = HashMap<CliLoginKey, Instant>;

fn cli_key_changes() -> std::sync::MutexGuard<'static, CliKeyChanges> {
    static CHANGES: OnceLock<StdMutex<CliKeyChanges>> = OnceLock::new();
    CHANGES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// The stored key for this login was just replaced with a different one from the CLI.
/// A request sent before that can still come back rejected, with the old key.
pub(crate) fn note_cli_key_changed(store_path: &Path, key: &str) {
    cli_key_changes().insert((key.to_string(), store_path.to_path_buf()), Instant::now());
}

/// The request does not know which key it sent, so within the interval a rejection is
/// taken to be of the old key: it retries once with the stored one and reads nothing.
fn cli_key_changed_recently(store_path: &Path, key: &str) -> bool {
    cli_key_changes()
        .get(&(key.to_string(), store_path.to_path_buf()))
        .is_some_and(|changed| changed.elapsed() < CLI_LOGIN_READ_INTERVAL)
}

/// A CLI login read from async code. It can wait on a Keychain prompt, and the
/// gateway service runs on a single thread.
pub(crate) async fn read_cli_login(provider_id: &str, home: &Path) -> Option<OAuthSession> {
    let (provider_id, home) = (provider_id.to_string(), home.to_path_buf());
    tokio::task::spawn_blocking(move || detect_local_cli_session(&provider_id, &home))
        .await
        .ok()
        .flatten()
}

async fn read_cli_login_limited(provider_id: &str, home: &Path) -> Option<OAuthSession> {
    let (provider_id, home) = (provider_id.to_string(), home.to_path_buf());
    tokio::task::spawn_blocking(move || detect_local_cli_session_limited(&provider_id, &home))
        .await
        .ok()
        .flatten()
}

/// One token lookup. A valid stored session costs one read of the store, since this
/// runs on every request. The CLI login is read only by the providers whose CLI key
/// wins, or to fill a base key with no session at all, and never for an account key.
async fn lookup_access_token(
    store_path: &Path,
    home: &Path,
    key: &str,
) -> Result<Option<String>, String> {
    if !cli_login_replaces_stored_session(key) {
        if let Some(access) = current_access_token(store_path, key, None)? {
            return Ok(Some(access));
        }
        if key != session_key_provider(key) || stored_session(store_path, key)?.is_some() {
            return Ok(None);
        }
    }
    let cli_login = read_cli_login_limited(key, home).await;
    current_access_token(store_path, key, cli_login)
}

pub async fn resolve_oauth_access_token(provider_id: &str) -> Result<String, String> {
    let store_path = auth_store_path();
    let key = canonical_provider_id(provider_id).to_string();
    if let Some(access) = lookup_access_token(&store_path, &user_home(), &key).await? {
        return Ok(access);
    }
    let _refresh = auth_refresh_lock_for(&key).lock().await;
    // Another request may have refreshed this while it waited for the lock.
    if let Some(access) = current_access_token(&store_path, &key, None)? {
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

/// Like `resolve_oauth_access_token`, for a provider that may have no login at all,
/// which is `None`.
pub async fn resolve_oauth_access_token_if_logged_in(
    provider_id: &str,
) -> Result<Option<String>, String> {
    let store_path = auth_store_path();
    let key = canonical_provider_id(provider_id);
    if let Some(access) = lookup_access_token(&store_path, &user_home(), key).await? {
        return Ok(Some(access));
    }
    if stored_session(&store_path, key)?.is_none() {
        return Ok(None);
    }
    resolve_oauth_access_token(provider_id).await.map(Some)
}

pub(crate) fn oauth_account_id(provider_id: &str) -> Option<String> {
    let key = canonical_provider_id(provider_id);
    let path = auth_store_path();
    // For the Kimi identity this fills in. Routing calls this from sync code, so it
    // must not read a CLI login.
    let _ = current_access_token(&path, key, None);
    stored_session(&path, key)
        .ok()
        .flatten()
        .and_then(|session| session.account_id)
}

/// The login a request used, which is what recovery from a 401/403 has to refresh:
/// a pool account overrides the connection's own credential.
pub(crate) fn request_login_key<'a>(
    provider: &'a ProviderDefinition,
    credential_override: Option<&'a ProviderCredential>,
) -> &'a str {
    credential_override
        .unwrap_or(&provider.credential)
        .reference
        .as_deref()
        .unwrap_or(&provider.id)
}

/// Force one OAuth refresh after an upstream authentication rejection. This is
/// intentionally separate from `resolve_oauth_access_token`, which returns a
/// still-valid cached token and therefore cannot recover from server-side
/// revocation or an entitlement transition. `provider_id` is the key the request
/// used; see `request_login_key`.
pub async fn force_refresh_oauth_access_token(provider_id: &str) -> Result<String, String> {
    force_refresh_access_token_at(&auth_store_path(), &user_home(), provider_id).await
}

async fn force_refresh_access_token_at(
    store_path: &Path,
    home: &Path,
    provider_id: &str,
) -> Result<String, String> {
    let key = canonical_provider_id(provider_id).to_string();
    let base = session_key_provider(&key).to_string();
    if is_static_cli_oauth_provider(&base) {
        return reread_cli_login_after_rejection(store_path, home, &key).await;
    }
    if native_oauth_provider_id(&base).is_none() {
        return Err(format!("{key} はネイティブOAuth更新に未対応です"));
    }
    let _refresh = auth_refresh_lock_for(&key).lock().await;
    // Never a CLI login in place of a missing session: that would save whichever
    // account the CLI is signed into as this key.
    let session = stored_session(store_path, &key)?
        .ok_or_else(|| format!("{key} のログインが見つかりません"))?;
    if session.refresh.trim().is_empty() {
        return Err(format!("{key} のrefresh tokenがありません"));
    }
    let through_agy =
        session_key_provider(&key) == "google-antigravity" && session.source != "oauth";
    if through_agy {
        if let Some(outcome) = gate_agy_refresh_after_rejection(store_path, home, &key, &session) {
            return outcome;
        }
    }
    let refreshed = refresh_session(&key, &session).await?;
    let access = refreshed.access.clone();
    mutate_store(store_path, |store| {
        store.providers.insert(key.clone(), refreshed);
        Ok(())
    })?;
    if through_agy && access != session.access {
        note_cli_key_changed(store_path, &key);
    }
    Ok(access)
}

/// Refreshing an imported `agy` login reads the Keychain and runs `agy models`, so
/// after a rejection, which repeats per request on a 403 about something else, it
/// goes by the same limit as reading any other login. `None` means go ahead.
fn gate_agy_refresh_after_rejection(
    store_path: &Path,
    home: &Path,
    key: &str,
    session: &OAuthSession,
) -> Option<Result<String, String>> {
    if cli_key_changed_recently(store_path, key) {
        return Some(Ok(session.access.clone()));
    }
    let agy = (session_key_provider(key).to_string(), home.to_path_buf());
    if take_cli_login_read(agy) {
        return None;
    }
    Some(Err(format!(
        "{key} の agy ログインを読み直してから1分経っていません"
    )))
}

/// A static key has nothing to refresh, only the CLI's current login to compare
/// with. The same key means the rejection was about something else, so a retry
/// cannot help, and failing here keeps a 403 from retrying per request.
async fn reread_cli_login_after_rejection(
    store_path: &Path,
    home: &Path,
    key: &str,
) -> Result<String, String> {
    if key != session_key_provider(key) {
        return Err(format!("{key} はCLIログインから更新できません"));
    }
    if let Some(access) = key_replaced_meanwhile(store_path, key)? {
        return Ok(access);
    }
    let Some(session) = read_cli_login_limited(key, home)
        .await
        .filter(cli_session_is_importable)
    else {
        // A concurrent rejection may have stored a new key while this one waited
        // on the read, or used up the read window doing so.
        if let Some(access) = key_replaced_meanwhile(store_path, key)? {
            return Ok(access);
        }
        return Err(format!(
            "{key} の CLI ログインが見つからないか、読み直してから1分経っていません"
        ));
    };
    if stored_session(store_path, key)?.is_some_and(|stored| stored.access == session.access) {
        // Equal to the store can mean another request just stored this key, in
        // which case this request was sent with the old one and is worth a retry.
        if let Some(access) = key_replaced_meanwhile(store_path, key)? {
            return Ok(access);
        }
        return Err(format!("{key} の CLI ログインは拒否されたキーと同じです"));
    }
    let access = session.access.clone();
    mutate_store(store_path, |store| {
        store.providers.insert(key.to_string(), session);
        Ok(())
    })?;
    note_cli_key_changed(store_path, key);
    Ok(access)
}

/// The stored key, when it was replaced within the interval: a rejection then is taken
/// to be of the key before it.
fn key_replaced_meanwhile(store_path: &Path, key: &str) -> Result<Option<String>, String> {
    if !cli_key_changed_recently(store_path, key) {
        return Ok(None);
    }
    Ok(stored_session(store_path, key)?.map(|stored| stored.access))
}

pub async fn login_provider_oauth(provider_id: &str) -> Result<OAuthLoginReport, String> {
    let key = canonical_provider_id(provider_id).to_string();
    if key == "google-antigravity" {
        // Prefer an existing CODETAS session (native or imported), then a
        // logged-in `agy`, and only then start the in-app Google login. This
        // keeps the existing flow working for users who already use the CLI.
        let store_path = auth_store_path();
        let stored = stored_session(&store_path, &key)?;
        if let Some(stored) = stored.as_ref() {
            let _refresh = auth_refresh_lock_for(&key).lock().await;
            if !session_needs_refresh(stored) {
                return Ok(existing_session_report(&key, stored));
            }
            if !stored.refresh.trim().is_empty() {
                if let Ok(fresh) = refresh_session(&key, stored).await {
                    let report = existing_session_report(&key, &fresh);
                    persist_session(&key, fresh)?;
                    return Ok(report);
                }
            }
        }
        if let Some(session) = read_cli_login(&key, &user_home()).await {
            // Only import a session that is usable right now. A stale CLI
            // session would otherwise hide a failed `agy models` refresh and
            // fail later at request time.
            if !session_needs_refresh(&session) && cli_session_is_importable(&session) {
                if let Some(stored) = stored.as_ref() {
                    // Do not let a different `agy` login silently replace the
                    // account this provider is already using. That account is
                    // added through "Add account" instead.
                    if antigravity_identity_conflict(stored, &session) {
                        return Err(antigravity_other_account_message(stored, &session));
                    }
                }
                let _refresh = auth_refresh_lock_for(&key).lock().await;
                persist_session(&key, session)?;
                return Ok(imported_cli_report(key));
            }
        }
        if !antigravity_native_login_configured() {
            return Err(
                "agy の有効なログインが見つかりません。agy で再ログインするか、利用が許可された Google OAuth クライアントを CODETAS_ANTIGRAVITY_CLIENT_ID / CODETAS_ANTIGRAVITY_CLIENT_SECRET に設定してください"
                    .into(),
            );
        }
        return complete_browser_login(&key).await;
    }
    if key == "meta" {
        let session = refresh_muse_cli_session().await?;
        let _refresh = auth_refresh_lock().lock().await;
        persist_session(&key, session)?;
        return Ok(imported_cli_report(key));
    }
    if is_static_cli_oauth_provider(&key) && key != "meta" {
        let session = refresh_static_cli_session(&key).await?;
        let _refresh = auth_refresh_lock().lock().await;
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
    if let Some(session) = read_cli_login(&key, &user_home()).await {
        if !session_needs_refresh(&session) && !session.access.is_empty() {
            persist_session(&key, session)?;
            return Ok(imported_cli_report(key));
        }
        if !session.refresh.trim().is_empty() {
            let refresh = auth_refresh_lock().lock().await;
            if stored_session(&store_path, &key)?.is_some() {
                // It takes the same lock, which is not reentrant.
                drop(refresh);
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

/// Registering a connection reads its CLI login once and keeps it, so the checks
/// that follow and the first request have no reason to read it again.
pub async fn import_local_cli_session(provider_id: &str) -> bool {
    import_local_cli_session_at(&auth_store_path(), &user_home(), provider_id).await
}

async fn import_local_cli_session_at(store_path: &Path, home: &Path, provider_id: &str) -> bool {
    let key = canonical_provider_id(provider_id);
    if stored_session(store_path, key).is_ok_and(|session| session.is_some()) {
        return true;
    }
    let Some(session) = read_cli_login(key, home)
        .await
        .filter(cli_session_is_importable)
    else {
        return false;
    };
    mutate_store(store_path, |store| {
        store.providers.entry(key.to_string()).or_insert(session);
        Ok(())
    })
    .is_ok()
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
    if let Some(session) = read_cli_login(key, &user_home()).await {
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
        "google-antigravity" => login_antigravity().await?,
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

fn existing_session_report(key: &str, session: &OAuthSession) -> OAuthLoginReport {
    OAuthLoginReport {
        provider_id: key.to_string(),
        imported: session.source == "local-cli",
        source: session.source.clone(),
        message: if session.source == "local-cli" {
            "既存のCLIログインを使います。".into()
        } else {
            "既存のCODETASログインを使います。".into()
        },
    }
}

fn antigravity_identity_conflict(stored: &OAuthSession, current: &OAuthSession) -> bool {
    matches!(
        (session_email(stored), session_email(current)),
        (Some(expected), Some(actual)) if !expected.eq_ignore_ascii_case(actual)
    )
}

fn session_email(session: &OAuthSession) -> Option<&str> {
    session
        .email
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn antigravity_other_account_message(stored: &OAuthSession, current: &OAuthSession) -> String {
    let expected = stored.email.as_deref().unwrap_or("既存のアカウント");
    let actual = current.email.as_deref().unwrap_or("別のアカウント");
    format!(
        "agy は {actual} にログインしています。CODETAS の {expected} を更新するには、agy で {expected} に再ログインしてください。{actual} を別アカウントとして追加するには「アカウント追加」を使ってください。"
    )
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
    let store_path = auth_store_path();
    let mut replaced = false;
    mutate_store(&store_path, |store| {
        let access = session.access.clone();
        replaced = store
            .providers
            .insert(provider_id.to_string(), session)
            .is_some_and(|previous| previous.access != access);
        Ok(())
    })?;
    // A request already sent with the old key can still come back rejected.
    if replaced {
        note_cli_key_changed(&store_path, provider_id);
    }
    Ok(())
}

fn attach_stored_oauth_provider(
    settings: &mut GatewaySettings,
    provider_id: &str,
    enable: bool,
) -> Result<bool, String> {
    // Once accounts are managed in the pool, the provider credential points at
    // the selected main account. Never rewrite it back to the legacy
    // single-session key on startup adoption.
    let pool_managed = settings
        .account_pool
        .accounts
        .iter()
        .any(|account| account.provider_id == provider_id);
    if let Some(provider) = settings
        .providers
        .iter_mut()
        .find(|provider| provider.id == provider_id)
    {
        if pool_managed {
            if enable && !provider.enabled {
                provider.enabled = true;
                return Ok(true);
            }
            return Ok(false);
        }
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
            | "mistral"
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

/// DeepAI Code hands its Pi runtime a loopback model transport whose port and
/// bearer capability change every time the app starts. Importing the running
/// app therefore has to read both from its own config and keep the `deepai`
/// connection pointed at them, instead of treating the endpoint as static.
///
/// This is the only writer of the `deepai` credential: the token and the
/// endpoint are always taken from one read together, so a fresh bearer can
/// never be paired with a stale port. A connection the user removed stays
/// removed until it is registered again. Nothing is written when DeepAI Code
/// has never written its runtime config.
pub fn apply_deepai_runtime(settings: &mut GatewaySettings, home: &Path) -> Result<bool, String> {
    apply_deepai_runtime_at(settings, &auth_store_path(), home)
}

/// Whether DeepAI Code has written a usable runtime config. The local-CLI scan
/// uses this so a presence check returns nothing derived from the capability.
pub fn deepai_runtime_present(home: &Path) -> bool {
    detect_deepai_runtime(home).is_some_and(|runtime| runtime.base_url.is_some())
}

/// The endpoint the stored DeepAI session was read from. A request refuses to
/// send the capability anywhere else, so a rotated port cannot receive it.
pub fn stored_deepai_endpoint() -> Option<String> {
    stored_session(&auth_store_path(), "deepai")
        .ok()
        .flatten()
        .and_then(|session| session.account_id)
}

/// The endpoint the running DeepAI Code app currently serves. Comparing this
/// with [`stored_deepai_endpoint`] tells a caller whether the app restarted on a
/// new port, without touching the credential store.
pub fn deepai_runtime_endpoint(home: &Path) -> Option<String> {
    detect_deepai_runtime(home)?.base_url
}

fn apply_deepai_runtime_at(
    settings: &mut GatewaySettings,
    store_path: &Path,
    home: &Path,
) -> Result<bool, String> {
    let Some(runtime) = detect_deepai_runtime(home) else {
        return Ok(false);
    };
    apply_deepai_runtime_from(settings, store_path, &runtime)
}

fn apply_deepai_runtime_from(
    settings: &mut GatewaySettings,
    store_path: &Path,
    runtime: &DeepAiRuntime,
) -> Result<bool, String> {
    if !cli_session_is_importable(&runtime.session) {
        return Ok(false);
    }
    // Same guards as adoption: a removed connection stays removed, and a
    // pool-managed one is left to the Accounts section.
    if cli_import_is_dismissed(&settings.account_pool, "deepai")
        || settings
            .account_pool
            .accounts
            .iter()
            .any(|account| account.provider_id == "deepai")
    {
        return Ok(false);
    }
    let Some(base_url) = runtime
        .base_url
        .as_deref()
        .map(str::trim)
        .map(|value| value.trim_end_matches('/'))
        .filter(|value| !value.is_empty())
    else {
        return Ok(false);
    };
    // Stage the settings change and validate it before the store is touched, so
    // a rejected value cannot leave a new bearer behind with an old endpoint.
    let mut staged = settings.clone();
    let settings_changed = upsert_deepai_provider(&mut staged, base_url, &runtime.models);
    let provider = staged
        .providers
        .iter()
        .find(|provider| provider.id == "deepai")
        .ok_or("deepai 接続がありません")?;
    provider
        .clone()
        .validate()
        .map_err(|error| format!("DeepAI Code の設定が不正です: {error}"))?;
    let mut session = runtime.session.clone();
    // Record which endpoint this capability belongs to. A request refuses to
    // send it to any other endpoint, so a rotation cannot pair the two wrong.
    session.account_id = Some(base_url.to_string());
    let session_changed = write_deepai_session(store_path, &session)?;
    if session_changed {
        // A request already sent with the old token can still come back rejected.
        note_cli_key_changed(store_path, "deepai");
    }
    *settings = staged;
    Ok(settings_changed || session_changed)
}

fn write_deepai_session(store_path: &Path, session: &OAuthSession) -> Result<bool, String> {
    let _guard = lock_auth_store(store_path)?;
    let mut store = load_store(store_path)?;
    let replaced = store.providers.insert("deepai".into(), session.clone());
    if replaced.as_ref().is_none_or(|previous| {
        previous.access != session.access || previous.account_id != session.account_id
    }) {
        save_store(store_path, &store)?;
        return Ok(true);
    }
    Ok(false)
}

fn upsert_deepai_provider(
    settings: &mut GatewaySettings,
    base_url: &str,
    models: &[DeepAiModel],
) -> bool {
    let mut changed = false;
    if !settings
        .providers
        .iter()
        .any(|provider| provider.id == "deepai")
    {
        let Some(mut provider) = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "deepai")
            .and_then(|preset| preset.instantiate(None).ok())
        else {
            return false;
        };
        provider.credential = oauth_session_credential("deepai");
        provider.enabled = true;
        settings.providers.push(provider);
        changed = true;
    }
    let Some(provider) = settings
        .providers
        .iter_mut()
        .find(|provider| provider.id == "deepai")
    else {
        return changed;
    };
    if provider.api_key_env.take().is_some() {
        changed = true;
    }
    if provider.credential.source != CredentialSource::OAuth
        || provider.credential.reference.as_deref() != Some("deepai")
        || provider.credential.command.is_some()
    {
        provider.credential = oauth_session_credential("deepai");
        changed = true;
    }
    if provider.base_url != base_url {
        provider.base_url = base_url.to_string();
        changed = true;
    }
    changed | apply_deepai_models(provider, models)
}

fn apply_deepai_models(
    provider: &mut crate::config::ProviderDefinition,
    models: &[DeepAiModel],
) -> bool {
    let mut changed = false;
    let ids = models
        .iter()
        .map(|model| model.id.clone())
        .collect::<Vec<_>>();
    if provider.models != ids {
        // Drop metadata for models that left the list, then keep the user's
        // choice if it is still offered. An empty list clears them, because the
        // app said it has no models and stale entries would only fail upstream.
        let keep = |model: &String| ids.iter().any(|id| id == model);
        provider
            .model_context_windows
            .retain(|model, _| keep(model));
        provider
            .model_max_output_tokens
            .retain(|model, _| keep(model));
        provider
            .model_input_modalities
            .retain(|model, _| keep(model));
        provider.model_protocols.retain(|model, _| keep(model));
        if provider
            .default_model
            .as_deref()
            .is_none_or(|current| !ids.iter().any(|id| id == current))
        {
            provider.default_model = ids.first().cloned();
        }
        provider.models = ids;
        changed = true;
    }
    for model in models {
        // A value that is missing this time must clear the previous one, or a
        // shrunken context window would leave a stale max output behind and fail
        // `validate` for every provider.
        match model.context_window {
            Some(context_window) => {
                if provider.model_context_windows.get(&model.id) != Some(&context_window) {
                    provider
                        .model_context_windows
                        .insert(model.id.clone(), context_window);
                    changed = true;
                }
            }
            None if provider.model_context_windows.remove(&model.id).is_some() => {
                changed = true;
            }
            None => {}
        }
        match model.max_output {
            Some(max_output) => {
                if provider.model_max_output_tokens.get(&model.id) != Some(&max_output) {
                    provider
                        .model_max_output_tokens
                        .insert(model.id.clone(), max_output);
                    changed = true;
                }
            }
            None if provider.model_max_output_tokens.remove(&model.id).is_some() => {
                changed = true;
            }
            None => {}
        }
        if provider.model_input_modalities.get(&model.id) != Some(&model.modalities) {
            provider
                .model_input_modalities
                .insert(model.id.clone(), model.modalities.clone());
            changed = true;
        }
        let responses = model
            .responses_api
            .then_some(crate::config::ProviderProtocol::Responses);
        match (responses, provider.model_protocols.get(&model.id).copied()) {
            (Some(protocol), current) if current != Some(protocol) => {
                provider.model_protocols.insert(model.id.clone(), protocol);
                changed = true;
            }
            (None, Some(_)) => {
                provider.model_protocols.remove(&model.id);
                changed = true;
            }
            _ => {}
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    use tempfile::tempdir;

    #[test]
    fn account_scoped_session_keys_resolve_to_the_base_provider() {
        assert_eq!(session_key_provider("anthropic"), "anthropic");
        assert_eq!(session_key_provider("anthropic#0f9a"), "anthropic");
        assert_eq!(session_key_provider("anthropic#"), "anthropic");
    }

    #[test]
    fn relogin_matches_a_stored_account_by_any_shared_identifier() {
        let stored = |account_id: Option<&str>, email: Option<&str>| OAuthStoredAccount {
            store_key: "anthropic#x".into(),
            account_id: account_id.map(str::to_string),
            email: email.map(str::to_string),
            expires_at_ms: 0,
            source: "oauth".into(),
        };
        let session = |account_id: Option<&str>, email: Option<&str>| OAuthSession {
            access: "a".into(),
            refresh: "r".into(),
            expires_at_ms: 0,
            source: "oauth".into(),
            account_id: account_id.map(str::to_string),
            email: email.map(str::to_string),
        };
        assert!(same_oauth_identity(
            &stored(None, Some("User@Example.com")),
            &session(Some("acct-1"), Some("user@example.com"))
        ));
        assert!(same_oauth_identity(
            &stored(Some("acct-1"), None),
            &session(Some("acct-1"), Some("other@example.com"))
        ));
        // Different ids with the same email are different accounts.
        assert!(!same_oauth_identity(
            &stored(Some("acct-1"), Some("a@example.com")),
            &session(Some("acct-2"), Some("a@example.com"))
        ));
        assert!(!same_oauth_identity(
            &stored(Some("acct-1"), Some("a@example.com")),
            &session(Some("acct-2"), Some("b@example.com"))
        ));
        assert!(!same_oauth_identity(
            &stored(None, None),
            &session(None, None)
        ));
    }

    #[test]
    fn account_slugs_stay_unique_for_long_shared_prefixes() {
        let session = |email: String| OAuthSession {
            access: "access".into(),
            refresh: "refresh".into(),
            expires_at_ms: 0,
            source: "oauth".into(),
            account_id: None,
            email: Some(email),
        };
        let prefix = "a".repeat(60);
        let first = session(format!("{prefix}1@example.com"));
        let second = session(format!("{prefix}2@example.com"));
        let first_slug = oauth_account_slug(&first);
        let second_slug = oauth_account_slug(&second);
        assert_ne!(first_slug, second_slug);
        assert!(first_slug.len() <= 48);
        // Same identity must map to the same store key so re-login updates
        // instead of duplicating.
        assert_eq!(first_slug, oauth_account_slug(&first));
    }

    #[test]
    fn parses_antigravity_keyring_payload() {
        let raw = r#"{"auth_method":"consumer","token":{"access_token":"atok","token_type":"Bearer","refresh_token":"rtok","expiry":"2026-08-16T11:05:30+09:00"}}"#;
        let stored = format!("antigravity-oauth:{}", STANDARD.encode(raw));
        let session = parse_antigravity_credentials(&stored).unwrap();
        assert_eq!(session.access, "atok");
        assert_eq!(session.refresh, "rtok");
        assert_eq!(session.expires_at_ms, 1_786_845_930_000);
        assert!(session.email.is_none());
    }

    #[test]
    fn parses_antigravity_signed_in_email() {
        let raw = r#"{"auth_method":"consumer","id_token_email":"User@Example.com","token":{"access_token":"atok","refresh_token":"rtok","expiry":"2026-08-16T11:05:30+09:00"}}"#;
        let session = parse_antigravity_credentials(raw).unwrap();
        assert_eq!(session.email.as_deref(), Some("user@example.com"));
    }

    #[test]
    fn antigravity_supports_native_login_and_cli_import() {
        let descriptor = OAUTH_PROVIDER_REGISTRY
            .iter()
            .find(|item| item.id == "google-antigravity")
            .unwrap();
        assert!(descriptor.native_login);
        assert!(descriptor.cli_import);
        assert_eq!(descriptor.flow, "authorization-code-pkce");
        assert!(provider_supports_native_oauth("google-antigravity"));
    }

    #[test]
    fn antigravity_login_never_replaces_a_different_agy_account() {
        let session = |email: Option<&str>| OAuthSession {
            access: "access".into(),
            refresh: "refresh".into(),
            expires_at_ms: now_ms() + 3_600_000,
            source: "local-cli".into(),
            account_id: None,
            email: email.map(str::to_string),
        };
        assert!(antigravity_identity_conflict(
            &session(Some("one@example.com")),
            &session(Some("two@example.com"))
        ));
        assert!(!antigravity_identity_conflict(
            &session(Some("One@Example.com")),
            &session(Some("one@example.com"))
        ));
        // Without an email on both sides the identity cannot be compared; keep
        // the legacy import behaviour instead of blocking a valid refresh.
        assert!(!antigravity_identity_conflict(
            &session(None),
            &session(Some("two@example.com"))
        ));
        let message = antigravity_other_account_message(
            &session(Some("one@example.com")),
            &session(Some("two@example.com")),
        );
        assert!(message.contains("one@example.com"), "{message}");
        assert!(message.contains("two@example.com"), "{message}");
    }

    #[test]
    fn antigravity_client_file_requires_both_values() {
        let parsed = parse_antigravity_client_file(
            r#"{"clientId":"id.apps.googleusercontent.com","clientSecret":"GOCSPX-test"}"#,
        )
        .unwrap();
        assert_eq!(parsed.0, "id.apps.googleusercontent.com");
        assert_eq!(parsed.1, "GOCSPX-test");
        // Snake case is accepted for hand-written files.
        assert!(
            parse_antigravity_client_file(r#"{"client_id":"id","client_secret":"secret"}"#)
                .is_some()
        );
        // A half-configured file must not enable native login.
        assert!(parse_antigravity_client_file(r#"{"clientId":"id"}"#).is_none());
        assert!(parse_antigravity_client_file("not json").is_none());
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
        assert!(!settings
            .providers
            .iter()
            .any(|provider| provider.id == "zai"));
        assert!(!settings
            .providers
            .iter()
            .any(|provider| provider.id == "minimax"));
    }

    #[test]
    fn adopt_imports_dedicated_zai_and_minimax_cli_files() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        fs::create_dir_all(home.join(".z.ai")).unwrap();
        fs::create_dir_all(home.join(".minimax")).unwrap();
        fs::write(
            home.join(".z.ai/auth.json"),
            r#"{"api_key":"zai-imported"}"#,
        )
        .unwrap();
        fs::write(
            home.join(".minimax/auth.json"),
            r#"{"api_key":"minimax-imported"}"#,
        )
        .unwrap();
        let store = directory.path().join("auth.json");
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
        assert_eq!(
            load_store(&store).unwrap().providers["zai"].access,
            "zai-imported"
        );
        assert_eq!(
            load_store(&store).unwrap().providers["minimax"].access,
            "minimax-imported"
        );
    }

    #[test]
    fn adopt_imports_only_the_mistral_key_from_the_vibe_env_file() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        fs::create_dir_all(home.join(".vibe")).unwrap();
        fs::write(
            home.join(".vibe/.env"),
            "# MISTRAL_API_KEY=commented-out\nOPENROUTER_API_KEY=other-provider\nexport MISTRAL_API_KEY='vibe-imported'\n",
        )
        .unwrap();
        let store = directory.path().join("auth.json");
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
        let session = load_store(&store).unwrap().providers["mistral"].clone();
        assert_eq!(session.access, "vibe-imported");
        assert_eq!(session.source, "local-cli");
        assert!(cli_session_is_importable(&session));
    }

    #[test]
    fn adopt_does_not_bring_back_a_dismissed_login() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        fs::create_dir_all(home.join(".vibe")).unwrap();
        fs::write(home.join(".vibe/.env"), "MISTRAL_API_KEY=vibe-imported\n").unwrap();
        let store = directory.path().join("auth.json");
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        settings
            .account_pool
            .dismissed_cli_imports
            .push("mistral".into());
        adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap();
        assert!(settings.providers.iter().all(|p| p.id != "mistral"));
        assert!(!load_store(&store)
            .unwrap()
            .providers
            .contains_key("mistral"));

        settings.account_pool.dismissed_cli_imports.clear();
        assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
        assert!(settings
            .providers
            .iter()
            .any(|p| p.id == "mistral" && p.enabled));
    }

    fn key_session(access: &str) -> OAuthSession {
        OAuthSession {
            access: access.into(),
            refresh: String::new(),
            expires_at_ms: i64::MAX / 2,
            source: "local-cli".into(),
            account_id: None,
            email: None,
        }
    }

    fn store_session(path: &Path, key: &str, session: OAuthSession) {
        mutate_store(path, |store| {
            store.providers.insert(key.into(), session);
            Ok(())
        })
        .unwrap();
    }

    fn stored_access(path: &Path, key: &str) -> Option<String> {
        stored_session(path, key)
            .unwrap()
            .map(|session| session.access)
    }

    fn write_vibe_key(home: &Path, key: &str) {
        fs::create_dir_all(home.join(".vibe")).unwrap();
        fs::write(home.join(".vibe/.env"), format!("MISTRAL_API_KEY={key}\n")).unwrap();
    }

    fn write_muse_key(home: &Path, key: &str) {
        fs::create_dir_all(home.join(".config/muse")).unwrap();
        fs::write(
            home.join(".config/muse/auth.json"),
            format!(r#"{{"providers":{{"meta":{{"api_key":"{key}"}}}}}}"#),
        )
        .unwrap();
    }

    fn write_kimi_login(home: &Path) {
        fs::create_dir_all(home.join(".kimi-code/credentials")).unwrap();
        fs::write(
            home.join(".kimi-code/credentials/kimi-code.json"),
            r#"{"access_token":"cli-access","refresh_token":"cli-refresh","expires_at":32503680000}"#,
        )
        .unwrap();
    }

    fn mistral_connection(
        base_url: Option<&str>,
        credential: ProviderCredential,
    ) -> ProviderDefinition {
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "mistral")
            .unwrap()
            .instantiate(base_url)
            .unwrap();
        provider.credential = credential;
        provider
    }

    fn unset_env_credential() -> ProviderCredential {
        ProviderCredential {
            source: CredentialSource::Environment,
            reference: Some("CODETAS_TEST_UNSET_MISTRAL_KEY".into()),
            ..ProviderCredential::default()
        }
    }

    #[tokio::test]
    async fn a_token_lookup_with_a_stored_session_does_not_read_the_vibe_login() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        store_session(&store, "mistral", key_session("stored-key"));
        write_vibe_key(&home, "vibe-key");
        assert_eq!(
            lookup_access_token(&store, &home, "mistral").await.unwrap(),
            Some("stored-key".into())
        );
        // The Keychain allowance is untouched, so nothing read the login.
        fs::remove_file(home.join(".vibe/.env")).unwrap();
        let mut keychain_read = false;
        detect_vibe_limited(&home, || {
            keychain_read = true;
            None
        });
        assert!(keychain_read);
    }

    #[tokio::test]
    async fn a_token_lookup_fills_a_missing_session_and_keeps_it() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        write_vibe_key(&home, "vibe-key");
        assert_eq!(
            lookup_access_token(&store, &home, "mistral").await.unwrap(),
            Some("vibe-key".into())
        );
        assert_eq!(
            stored_access(&store, "mistral").as_deref(),
            Some("vibe-key")
        );
        write_vibe_key(&home, "newer-key");
        assert_eq!(
            lookup_access_token(&store, &home, "mistral").await.unwrap(),
            Some("vibe-key".into())
        );
    }

    #[tokio::test]
    async fn a_missing_keychain_first_login_is_not_read_again_for_a_minute() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        assert_eq!(
            lookup_access_token(&store, &home, "meta").await.unwrap(),
            None
        );
        // Found by the next read, so only skipping that read leaves it unseen.
        write_muse_key(&home, "LLM|first");
        assert_eq!(
            lookup_access_token(&store, &home, "meta").await.unwrap(),
            None
        );
        assert_eq!(stored_access(&store, "meta"), None);
    }

    #[tokio::test]
    async fn a_vibe_env_key_is_found_as_soon_as_it_is_written() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        assert_eq!(
            lookup_access_token(&store, &home, "mistral").await.unwrap(),
            None
        );
        write_vibe_key(&home, "vibe-key");
        assert_eq!(
            lookup_access_token(&store, &home, "mistral").await.unwrap(),
            Some("vibe-key".into())
        );
    }

    #[test]
    fn only_the_keychain_step_of_the_vibe_login_is_limited() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let keychain = || {
            reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            None
        };
        assert!(detect_vibe_limited(&home, keychain).is_none());
        assert!(detect_vibe_limited(&home, keychain).is_none());
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);
        // The file answers every time, and a rotated key in it shows up at once.
        for key in ["first", "second"] {
            write_vibe_key(&home, key);
            let session = detect_vibe_limited(&home, keychain).unwrap();
            assert_eq!(session.access, key);
        }
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Waits until a caller is parked on the read in progress.
    fn wait_for_a_waiter(key: &CliLoginKey) {
        let started = Instant::now();
        while cli_login_reads()
            .get(key)
            .and_then(|state| state.running.as_ref().map(std::sync::Arc::strong_count))
            != Some(3)
        {
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_request_during_a_read_waits_for_its_answer() {
        let directory = tempdir().unwrap();
        let key = ("meta".to_string(), directory.path().to_path_buf());
        let (started, read_started) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let reader = {
            let key = key.clone();
            std::thread::spawn(move || {
                limited_cli_login_read(key, Duration::from_secs(10), move || {
                    started.send(()).unwrap();
                    released.recv().unwrap();
                    Some(key_session("from-the-read"))
                })
            })
        };
        read_started.recv().unwrap();
        let waiter = {
            let key = key.clone();
            std::thread::spawn(move || {
                limited_cli_login_read(key, Duration::from_secs(10), || {
                    panic!("a second read while one is in progress")
                })
            })
        };
        wait_for_a_waiter(&key);
        release.send(()).unwrap();
        assert_eq!(reader.join().unwrap().unwrap().access, "from-the-read");
        assert_eq!(waiter.join().unwrap().unwrap().access, "from-the-read");
    }

    #[test]
    fn the_wait_for_a_read_in_progress_is_bounded() {
        let directory = tempdir().unwrap();
        let key = ("meta".to_string(), directory.path().to_path_buf());
        let (started, read_started) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let reader = {
            let key = key.clone();
            std::thread::spawn(move || {
                limited_cli_login_read(key, Duration::from_secs(10), move || {
                    started.send(()).unwrap();
                    released.recv().unwrap();
                    None
                })
            })
        };
        read_started.recv().unwrap();
        let waited = Instant::now();
        assert!(
            limited_cli_login_read(key, Duration::from_millis(50), || panic!("a second read"))
                .is_none()
        );
        assert!(waited.elapsed() < Duration::from_secs(5));
        release.send(()).unwrap();
        assert!(reader.join().unwrap().is_none());
    }

    #[tokio::test]
    async fn a_login_that_is_only_a_file_is_not_limited() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        assert_eq!(
            lookup_access_token(&store, &home, "zai").await.unwrap(),
            None
        );
        fs::create_dir_all(home.join(".z.ai")).unwrap();
        fs::write(home.join(".z.ai/auth.json"), r#"{"api_key":"zai-key"}"#).unwrap();
        assert_eq!(
            lookup_access_token(&store, &home, "zai").await.unwrap(),
            Some("zai-key".into())
        );
    }

    #[tokio::test]
    async fn a_meta_login_replaces_the_stored_key_at_most_once_a_minute() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        store_session(&store, "meta", key_session("stored-key"));
        write_muse_key(&home, "LLM|first");
        assert_eq!(
            lookup_access_token(&store, &home, "meta").await.unwrap(),
            Some("LLM|first".into())
        );
        write_muse_key(&home, "LLM|second");
        assert_eq!(
            lookup_access_token(&store, &home, "meta").await.unwrap(),
            Some("LLM|first".into())
        );
    }

    #[test]
    fn a_cli_login_read_before_the_lock_yields_to_what_the_store_holds_now() {
        let directory = tempdir().unwrap();
        let store = directory.path().join("auth.json");
        // Filled while the read waited on a prompt.
        store_session(&store, "mistral", key_session("stored-meanwhile"));
        assert_eq!(
            current_access_token(&store, "mistral", Some(key_session("cli-key"))).unwrap(),
            Some("stored-meanwhile".into())
        );
        // Still empty after the wait.
        assert_eq!(
            current_access_token(&store, "kimi", Some(key_session("cli-key"))).unwrap(),
            Some("cli-key".into())
        );
        // A provider whose CLI key wins takes it either way.
        store_session(&store, "meta", key_session("stored-key"));
        assert_eq!(
            current_access_token(&store, "meta", Some(key_session("cli-key"))).unwrap(),
            Some("cli-key".into())
        );
        // An account key never takes a CLI login.
        store_session(&store, "kimi#b", key_session("account-key"));
        assert_eq!(
            current_access_token(&store, "kimi#b", Some(key_session("cli-key"))).unwrap(),
            Some("account-key".into())
        );
    }

    #[tokio::test]
    async fn recovery_fails_without_a_retry_when_the_cli_key_is_unchanged() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        store_session(&store, "mistral", key_session("same-key"));
        write_vibe_key(&home, "same-key");
        let error = force_refresh_access_token_at(&store, &home, "mistral")
            .await
            .unwrap_err();
        assert!(error.contains("同じ"), "{error}");
        // Reading the file costs nothing, so a key rotated since is picked up.
        write_vibe_key(&home, "new-key");
        assert_eq!(
            force_refresh_access_token_at(&store, &home, "mistral")
                .await
                .unwrap(),
            "new-key"
        );
    }

    #[tokio::test]
    async fn a_repeated_rejection_does_not_read_a_keychain_first_login_again() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        store_session(&store, "meta", key_session("LLM|same"));
        write_muse_key(&home, "LLM|same");
        let error = force_refresh_access_token_at(&store, &home, "meta")
            .await
            .unwrap_err();
        assert!(error.contains("同じ"), "{error}");
        // Found by a second read, so only skipping that read leaves it unseen.
        write_muse_key(&home, "LLM|new");
        assert!(force_refresh_access_token_at(&store, &home, "meta")
            .await
            .is_err());
        assert_eq!(stored_access(&store, "meta").as_deref(), Some("LLM|same"));
    }

    fn write_zai_key(home: &Path, key: &str) {
        fs::create_dir_all(home.join(".z.ai")).unwrap();
        fs::write(
            home.join(".z.ai/auth.json"),
            format!(r#"{{"api_key":"{key}"}}"#),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn a_second_rejection_right_after_a_recovery_takes_the_new_key_without_reading() {
        for provider in ["mistral", "zai"] {
            let directory = tempdir().unwrap();
            let home = directory.path().join("home");
            let store = directory.path().join("auth.json");
            let write_key = |key: &str| match provider {
                "mistral" => write_vibe_key(&home, key),
                _ => write_zai_key(&home, key),
            };
            store_session(&store, provider, key_session("revoked"));
            write_key("rotated");
            assert_eq!(
                force_refresh_access_token_at(&store, &home, provider)
                    .await
                    .unwrap(),
                "rotated"
            );
            // Sent before the recovery, so it comes back rejected with the old key.
            write_key("not-read");
            assert_eq!(
                force_refresh_access_token_at(&store, &home, provider)
                    .await
                    .unwrap(),
                "rotated",
                "{provider}"
            );
            assert_eq!(stored_access(&store, provider).as_deref(), Some("rotated"));
        }
    }

    #[tokio::test]
    async fn a_rejection_right_after_startup_adoption_replaced_the_key_retries_with_it() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        store_session(&store, "zai", key_session("revoked"));
        write_zai_key(&home, "rotated");
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap();
        assert_eq!(stored_access(&store, "zai").as_deref(), Some("rotated"));
        // Sent with the old key before the adoption, so it comes back rejected.
        write_zai_key(&home, "not-read");
        assert_eq!(
            force_refresh_access_token_at(&store, &home, "zai")
                .await
                .unwrap(),
            "rotated"
        );
    }

    #[tokio::test]
    async fn a_rejection_after_a_lookup_took_the_new_key_retries_with_it() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        store_session(&store, "zai", key_session("revoked"));
        write_zai_key(&home, "rotated");
        assert_eq!(
            lookup_access_token(&store, &home, "zai").await.unwrap(),
            Some("rotated".into())
        );
        assert_eq!(
            force_refresh_access_token_at(&store, &home, "zai")
                .await
                .unwrap(),
            "rotated"
        );
    }

    #[tokio::test]
    async fn a_rejection_once_the_interval_has_passed_gets_no_retry_for_the_same_key() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        store_session(&store, "zai", key_session("current"));
        write_zai_key(&home, "current");
        cli_key_changes().insert(
            ("zai".to_string(), store.clone()),
            Instant::now() - CLI_LOGIN_READ_INTERVAL,
        );
        let error = force_refresh_access_token_at(&store, &home, "zai")
            .await
            .unwrap_err();
        assert!(error.contains("同じ"), "{error}");
    }

    fn imported_agy_session() -> OAuthSession {
        OAuthSession {
            refresh: "agy-refresh".into(),
            email: Some("user@example.com".into()),
            ..key_session("agy-access")
        }
    }

    #[test]
    fn a_rejected_agy_login_goes_through_the_cli_once_a_minute() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        let session = imported_agy_session();
        let gate =
            || gate_agy_refresh_after_rejection(&store, &home, "google-antigravity", &session);
        assert!(gate().is_none());
        assert!(gate().unwrap().unwrap_err().contains("1分"));
        // An account key shares `agy`'s single login, so it shares the limit too.
        assert!(
            gate_agy_refresh_after_rejection(&store, &home, "google-antigravity#b", &session)
                .unwrap()
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_repeated_rejection_of_an_agy_login_fails_before_reaching_the_cli() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        store_session(&store, "google-antigravity", imported_agy_session());
        assert!(take_cli_login_read((
            "google-antigravity".to_string(),
            home.clone()
        )));
        let error = force_refresh_access_token_at(&store, &home, "google-antigravity")
            .await
            .unwrap_err();
        assert!(error.contains("1分"), "{error}");
    }

    #[tokio::test]
    async fn a_second_rejection_of_an_agy_login_takes_the_token_just_refreshed() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        store_session(&store, "google-antigravity", imported_agy_session());
        note_cli_key_changed(&store, "google-antigravity");
        assert_eq!(
            force_refresh_access_token_at(&store, &home, "google-antigravity")
                .await
                .unwrap(),
            "agy-access"
        );
    }

    #[tokio::test]
    async fn an_agy_refresh_in_progress_does_not_hold_up_other_providers() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        store_session(&store, "anthropic", key_session("anthropic-access"));
        store_session(&store, "google-antigravity", imported_agy_session());
        let _agy = auth_refresh_lock_for("google-antigravity").lock().await;
        // No refresh token, so reaching the session fails before any network call.
        let other = tokio::time::timeout(
            Duration::from_secs(5),
            force_refresh_access_token_at(&store, &home, "anthropic"),
        )
        .await
        .expect("another provider's recovery waited for agy");
        assert!(other.unwrap_err().contains("refresh token"));
        // Another Antigravity recovery does wait, so `agy models` runs one at a time.
        assert!(tokio::time::timeout(
            Duration::from_millis(100),
            force_refresh_access_token_at(&store, &home, "google-antigravity"),
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn recovery_stores_a_new_cli_key_for_the_retry() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        store_session(&store, "mistral", key_session("rejected-key"));
        write_vibe_key(&home, "new-key");
        assert_eq!(
            force_refresh_access_token_at(&store, &home, "mistral")
                .await
                .unwrap(),
            "new-key"
        );
        assert_eq!(stored_access(&store, "mistral").as_deref(), Some("new-key"));
    }

    #[tokio::test]
    async fn recovery_uses_the_session_of_the_key_the_request_used() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        let mut base = key_session("base-access");
        base.refresh = "base-refresh".into();
        store_session(&store, "kimi", base);
        // No refresh token, so reaching it fails before any network call.
        store_session(&store, "kimi#b", key_session("account-access"));
        let error = force_refresh_access_token_at(&store, &home, "kimi#b")
            .await
            .unwrap_err();
        assert!(error.contains("kimi#b"), "{error}");
        assert_eq!(
            stored_access(&store, "kimi").as_deref(),
            Some("base-access")
        );
    }

    #[tokio::test]
    async fn recovery_with_no_session_does_not_import_the_cli_login() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        write_kimi_login(&home);
        store_session(&store, "kimi#b", key_session("account-access"));
        let error = force_refresh_access_token_at(&store, &home, "kimi")
            .await
            .unwrap_err();
        assert!(error.contains("見つかりません"), "{error}");
        assert_eq!(stored_access(&store, "kimi"), None);
    }

    #[tokio::test]
    async fn recovery_of_a_static_account_key_does_not_take_the_cli_login() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        write_vibe_key(&home, "vibe-key");
        store_session(&store, "mistral#work", key_session("work-key"));
        let error = force_refresh_access_token_at(&store, &home, "mistral#work")
            .await
            .unwrap_err();
        assert!(error.contains("CLIログインから更新できません"), "{error}");
        assert_eq!(
            stored_access(&store, "mistral#work").as_deref(),
            Some("work-key")
        );
    }

    #[test]
    fn recovery_targets_the_login_the_request_used() {
        let provider = ProviderDefinition {
            id: "anthropic".into(),
            credential: oauth_session_credential("anthropic"),
            ..ProviderDefinition::default()
        };
        let account = oauth_session_credential("anthropic#b");
        assert_eq!(request_login_key(&provider, Some(&account)), "anthropic#b");
        assert_eq!(request_login_key(&provider, None), "anthropic");
        let unnamed = ProviderCredential::default();
        assert_eq!(request_login_key(&provider, Some(&unnamed)), "anthropic");
    }

    #[test]
    fn only_a_mistral_connection_elsewhere_refuses_a_cli_login() {
        let official = mistral_connection(None, ProviderCredential::default());
        assert!(provider_may_take_cli_login(&official));
        assert!(registration_may_enable_provider(&official));
        let proxy = mistral_connection(
            Some("https://proxy.example/v1"),
            ProviderCredential::default(),
        );
        assert!(!provider_may_take_cli_login(&proxy));
        assert!(!registration_may_enable_provider(&proxy));
        let keyed_proxy = mistral_connection(
            Some("https://proxy.example/v1"),
            ProviderCredential {
                source: CredentialSource::Keychain,
                reference: Some("api-key/mistral".into()),
                ..ProviderCredential::default()
            },
        );
        assert!(!provider_may_take_cli_login(&keyed_proxy));
        assert!(registration_may_enable_provider(&keyed_proxy));
        let other = ProviderDefinition {
            id: "kimi".into(),
            base_url: "https://proxy.example/v1".into(),
            ..ProviderDefinition::default()
        };
        assert!(provider_may_take_cli_login(&other));
    }

    #[test]
    fn adopt_never_attaches_the_vibe_key_to_a_proxy_connection() {
        for (credential, enabled, has_session) in [
            (ProviderCredential::default(), true, false),
            (ProviderCredential::default(), false, false),
            (unset_env_credential(), true, false),
            (unset_env_credential(), false, false),
            (ProviderCredential::default(), false, true),
            (unset_env_credential(), true, true),
        ] {
            let directory = tempdir().unwrap();
            let home = directory.path().join("home");
            let store = directory.path().join("auth.json");
            write_vibe_key(&home, "vibe-key");
            if has_session {
                store_session(&store, "mistral", key_session("stored-key"));
            }
            let mut provider =
                mistral_connection(Some("https://proxy.example/v1"), credential.clone());
            provider.enabled = enabled;
            let mut settings = GatewaySettings::default();
            settings.providers.clear();
            settings.providers.push(provider);
            assert!(!adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
            let provider = &settings.providers[0];
            assert_eq!(provider.enabled, enabled);
            assert_eq!(provider.credential.source, credential.source);
            assert_eq!(provider.credential.reference, credential.reference);
            assert_eq!(
                stored_access(&store, "mistral"),
                has_session.then(|| "stored-key".to_string())
            );
        }
    }

    #[test]
    fn adopt_still_wires_a_mistral_connection_on_the_official_host() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        write_vibe_key(&home, "vibe-key");
        let mut provider = mistral_connection(None, unset_env_credential());
        provider.enabled = false;
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        settings.providers.push(provider);
        assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
        assert!(settings.providers[0].enabled);
        assert_eq!(
            settings.providers[0].credential.source,
            CredentialSource::OAuth
        );
        assert_eq!(
            stored_access(&store, "mistral").as_deref(),
            Some("vibe-key")
        );
    }

    #[test]
    fn adopt_keeps_no_vibe_key_for_a_connection_with_its_own_key() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        write_vibe_key(&home, "vibe-key");
        let keychain = ProviderCredential {
            source: CredentialSource::Keychain,
            reference: Some("api-key/mistral".into()),
            ..ProviderCredential::default()
        };
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        settings
            .providers
            .push(mistral_connection(None, keychain.clone()));
        assert!(!adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
        assert_eq!(settings.providers[0].credential.source, keychain.source);
        assert_eq!(stored_access(&store, "mistral"), None);
    }

    #[test]
    fn adopting_twice_reads_a_missing_keychain_first_login_once() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        assert!(!adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
        // Found by a second read, so only skipping that read leaves it unseen.
        write_muse_key(&home, "LLM|first");
        assert!(!adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
        assert_eq!(stored_access(&store, "meta"), None);
    }

    /// One login each, and the connection's id, for a family adoption treats its own way.
    fn write_cli_login(home: &Path, provider: &str) {
        match provider {
            "kimi" => write_kimi_login(home),
            "meta" => write_muse_key(home, "LLM|key"),
            "zai" => write_zai_key(home, "zai-key"),
            "minimax" => {
                fs::create_dir_all(home.join(".minimax")).unwrap();
                fs::write(home.join(".minimax/auth.json"), r#"{"api_key":"mm-key"}"#).unwrap();
            }
            "alibaba-token-plan-intl" => {
                fs::create_dir_all(home.join(".qwen")).unwrap();
                fs::write(
                    home.join(".qwen/settings.json"),
                    r#"{"env":{"DASHSCOPE_API_KEY":"qwen-key"}}"#,
                )
                .unwrap();
            }
            "mistral" => write_vibe_key(home, "vibe-key"),
            other => panic!("no login for {other}"),
        }
    }

    fn write_deepai_models(home: &Path) {
        let directory = home.join(".deepai-code/userdata/deepai/pi/deepai");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("models.json"),
            r#"{"providers":{"deepai":{"baseUrl":"http://127.0.0.1:60011","apiKey":"deepai-key","models":[{"id":"kimi-k3","name":"Kimi K3","input":["text","image"],"contextWindow":1048576,"maxTokens":131072}]}}}"#,
        )
        .unwrap();
    }

    #[test]
    fn deepai_runtime_endpoint_reports_the_port_the_app_is_serving() {
        let home = tempdir().unwrap();
        assert_eq!(deepai_runtime_endpoint(home.path()), None);
        write_deepai_models(home.path());
        assert_eq!(
            deepai_runtime_endpoint(home.path()).as_deref(),
            Some("http://127.0.0.1:60011")
        );
    }

    #[test]
    fn deepai_runtime_endpoint_follows_a_restart_and_ignores_a_remote_url() {
        let home = tempdir().unwrap();
        write_deepai_models(home.path());
        let path = home
            .path()
            .join(".deepai-code/userdata/deepai/pi/deepai/models.json");
        // A restart rewrites the same file on a new port.
        fs::write(
            &path,
            r#"{"providers":{"deepai":{"baseUrl":"http://127.0.0.1:52003","apiKey":"deepai-key","models":[{"id":"kimi-k3"}]}}}"#,
        )
        .unwrap();
        assert_eq!(
            deepai_runtime_endpoint(home.path()).as_deref(),
            Some("http://127.0.0.1:52003")
        );
        // A URL outside loopback is refused rather than followed.
        fs::write(
            &path,
            r#"{"providers":{"deepai":{"baseUrl":"https://example.invalid","apiKey":"deepai-key","models":[{"id":"kimi-k3"}]}}}"#,
        )
        .unwrap();
        assert_eq!(deepai_runtime_endpoint(home.path()), None);
    }

    const ADOPTION_FAMILIES: [&str; 6] = [
        "kimi",
        "meta",
        "zai",
        "minimax",
        "alibaba-token-plan-intl",
        "mistral",
    ];

    fn preset_connection(id: &str) -> ProviderDefinition {
        provider_presets()
            .into_iter()
            .find(|preset| preset.id == id)
            .unwrap_or_else(|| panic!("no preset for {id}"))
            .instantiate(None)
            .unwrap()
    }

    #[test]
    fn adoption_leaves_a_connection_the_user_switched_off_switched_off() {
        for provider in ADOPTION_FAMILIES {
            let directory = tempdir().unwrap();
            let home = directory.path().join("home");
            let store = directory.path().join("auth.json");
            write_cli_login(&home, provider);
            let mut connection = preset_connection(provider);
            connection.credential = oauth_session_credential(provider);
            connection.enabled = false;
            let mut settings = GatewaySettings::default();
            settings.providers = vec![connection];
            assert!(
                !adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap(),
                "{provider}"
            );
            assert!(!settings.providers[0].enabled, "{provider}");
            // The login itself is still kept, ready for when it is switched back on.
            assert!(stored_access(&store, provider).is_some(), "{provider}");
        }
    }

    #[test]
    fn a_first_import_still_turns_on_a_new_connection_and_an_unwired_stub() {
        for provider in ADOPTION_FAMILIES {
            let directory = tempdir().unwrap();
            let home = directory.path().join("home");
            write_cli_login(&home, provider);
            let store = directory.path().join("new.json");
            let mut settings = GatewaySettings::default();
            settings.providers.clear();
            assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
            let created = settings.providers.iter().find(|p| p.id == provider);
            assert!(created.is_some_and(|p| p.enabled), "{provider}");

            // A read of a Keychain-first login was used up above, so use another home.
            let directory = tempdir().unwrap();
            let home = directory.path().join("home");
            write_cli_login(&home, provider);
            let store = directory.path().join("stub.json");
            let mut stub = preset_connection(provider);
            stub.enabled = false;
            stub.credential = unset_env_credential();
            let mut settings = GatewaySettings::default();
            settings.providers = vec![stub];
            assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
            assert!(settings.providers[0].enabled, "{provider}");
            assert_eq!(
                settings.providers[0].credential.source,
                CredentialSource::OAuth,
                "{provider}"
            );
        }
    }

    #[test]
    fn deepai_runtime_points_the_connection_at_the_running_app() {
        let directory = tempdir().unwrap();
        let store = directory.path().join("auth.json");
        let runtime = DeepAiRuntime {
            session: OAuthSession {
                access: "deepai-key".into(),
                refresh: String::new(),
                expires_at_ms: i64::MAX / 2,
                source: "local-cli".into(),
                account_id: None,
                email: None,
            },
            base_url: Some("http://127.0.0.1:61234".into()),
            models: vec![
                DeepAiModel {
                    id: "kimi-k3".into(),
                    context_window: Some(1_048_576),
                    max_output: Some(131_072),
                    modalities: vec!["text".into(), "image".into()],
                    responses_api: false,
                },
                DeepAiModel {
                    id: "gpt-6-astra".into(),
                    context_window: Some(1_048_576),
                    max_output: Some(128_000),
                    modalities: vec!["text".into()],
                    responses_api: true,
                },
            ],
        };
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        assert!(apply_deepai_runtime_from(&mut settings, &store, &runtime).unwrap());
        settings.validate().unwrap();
        let provider = settings
            .providers
            .iter()
            .find(|provider| provider.id == "deepai")
            .unwrap();
        assert!(provider.enabled);
        assert!(provider.allow_private_network);
        assert_eq!(provider.base_url, "http://127.0.0.1:61234");
        assert_eq!(provider.models, vec!["kimi-k3", "gpt-6-astra"]);
        assert_eq!(provider.default_model.as_deref(), Some("kimi-k3"));
        assert_eq!(
            provider.model_context_windows.get("kimi-k3"),
            Some(&1_048_576)
        );
        assert_eq!(
            provider.model_max_output_tokens.get("kimi-k3"),
            Some(&131_072)
        );
        assert_eq!(
            provider
                .model_input_modalities
                .get("kimi-k3")
                .map(|values| values.iter().map(String::as_str).collect::<Vec<_>>()),
            Some(vec!["text", "image"])
        );
        assert_eq!(
            provider.model_protocols.get("gpt-6-astra"),
            Some(&crate::config::ProviderProtocol::Responses)
        );
        assert!(!provider.model_protocols.contains_key("kimi-k3"));
        assert_eq!(provider.credential.source, CredentialSource::OAuth);
        assert_eq!(provider.credential.reference.as_deref(), Some("deepai"));
        assert_eq!(
            stored_access(&store, "deepai").as_deref(),
            Some("deepai-key")
        );
        assert!(!apply_deepai_runtime_from(&mut settings, &store, &runtime).unwrap());
    }

    fn deepai_runtime_for(base_url: &str, models: Vec<DeepAiModel>) -> DeepAiRuntime {
        DeepAiRuntime {
            session: OAuthSession {
                access: "deepai-key".into(),
                refresh: String::new(),
                expires_at_ms: i64::MAX / 2,
                source: "local-cli".into(),
                account_id: None,
                email: None,
            },
            base_url: Some(base_url.into()),
            models,
        }
    }

    fn deepai_model(id: &str) -> DeepAiModel {
        DeepAiModel {
            id: id.into(),
            context_window: Some(1_000),
            max_output: Some(100),
            modalities: vec!["text".into()],
            responses_api: false,
        }
    }

    #[test]
    fn deepai_runtime_respects_a_dismissed_import() {
        let directory = tempdir().unwrap();
        let store = directory.path().join("auth.json");
        let runtime = deepai_runtime_for("http://127.0.0.1:60011", vec![deepai_model("kimi-k3")]);
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        dismiss_cli_import(&mut settings.account_pool, "deepai");
        assert!(!apply_deepai_runtime_from(&mut settings, &store, &runtime).unwrap());
        assert!(settings
            .providers
            .iter()
            .all(|provider| provider.id != "deepai"));
        assert!(stored_access(&store, "deepai").is_none());
        // Registering again clears the dismissal, so an explicit import works.
        restore_cli_import(&mut settings.account_pool, "deepai");
        assert!(apply_deepai_runtime_from(&mut settings, &store, &runtime).unwrap());
        assert!(settings
            .providers
            .iter()
            .any(|provider| provider.id == "deepai"));
    }

    #[test]
    fn deepai_runtime_drops_metadata_for_models_that_left() {
        let directory = tempdir().unwrap();
        let store = directory.path().join("auth.json");
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        apply_deepai_runtime_from(
            &mut settings,
            &store,
            &deepai_runtime_for(
                "http://127.0.0.1:60011",
                vec![deepai_model("a"), deepai_model("b")],
            ),
        )
        .unwrap();
        {
            let provider = settings
                .providers
                .iter_mut()
                .find(|provider| provider.id == "deepai")
                .unwrap();
            provider.default_model = Some("b".into());
        }
        assert!(apply_deepai_runtime_from(
            &mut settings,
            &store,
            &deepai_runtime_for(
                "http://127.0.0.1:60012",
                vec![deepai_model("b"), deepai_model("c")],
            ),
        )
        .unwrap());
        let provider = settings
            .providers
            .iter()
            .find(|provider| provider.id == "deepai")
            .unwrap();
        assert_eq!(provider.base_url, "http://127.0.0.1:60012");
        assert_eq!(provider.models, vec!["b", "c"]);
        // A chosen default that is still offered is kept.
        assert_eq!(provider.default_model.as_deref(), Some("b"));
        assert!(!provider.model_context_windows.contains_key("a"));
        assert!(provider.model_context_windows.contains_key("c"));
        settings.validate().unwrap();
    }

    #[test]
    fn deepai_runtime_clears_stale_limits_when_the_context_can_shrink() {
        let directory = tempdir().unwrap();
        let store = directory.path().join("auth.json");
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        let mut first = deepai_model("m");
        first.context_window = Some(1_000);
        first.max_output = Some(500);
        apply_deepai_runtime_from(
            &mut settings,
            &store,
            &deepai_runtime_for("http://127.0.0.1:60011", vec![first]),
        )
        .unwrap();
        settings.validate().unwrap();

        // The context shrinks and the file no longer offers a usable max output.
        let mut second = deepai_model("m");
        second.context_window = Some(400);
        second.max_output = None;
        apply_deepai_runtime_from(
            &mut settings,
            &store,
            &deepai_runtime_for("http://127.0.0.1:60011", vec![second]),
        )
        .unwrap();
        let provider = settings
            .providers
            .iter()
            .find(|provider| provider.id == "deepai")
            .unwrap();
        assert_eq!(provider.model_context_windows.get("m"), Some(&400));
        assert!(!provider.model_max_output_tokens.contains_key("m"));
        settings.validate().unwrap();

        // The context disappears entirely; the max output must not outlive it.
        let mut third = deepai_model("m");
        third.context_window = None;
        third.max_output = Some(2_000);
        apply_deepai_runtime_from(
            &mut settings,
            &store,
            &deepai_runtime_for("http://127.0.0.1:60011", vec![third]),
        )
        .unwrap();
        let provider = settings
            .providers
            .iter()
            .find(|provider| provider.id == "deepai")
            .unwrap();
        assert!(!provider.model_context_windows.contains_key("m"));
        assert_eq!(provider.model_max_output_tokens.get("m"), Some(&2_000));
        settings.validate().unwrap();
    }

    #[test]
    fn the_stored_deepai_session_records_its_endpoint() {
        let directory = tempdir().unwrap();
        let store = directory.path().join("auth.json");
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        apply_deepai_runtime_from(
            &mut settings,
            &store,
            &deepai_runtime_for("http://127.0.0.1:60001", vec![deepai_model("m")]),
        )
        .unwrap();
        assert_eq!(
            stored_session(&store, "deepai")
                .unwrap()
                .unwrap()
                .account_id
                .as_deref(),
            Some("http://127.0.0.1:60001")
        );
        apply_deepai_runtime_from(
            &mut settings,
            &store,
            &deepai_runtime_for("http://127.0.0.1:60002", vec![deepai_model("m")]),
        )
        .unwrap();
        assert_eq!(
            stored_session(&store, "deepai")
                .unwrap()
                .unwrap()
                .account_id
                .as_deref(),
            Some("http://127.0.0.1:60002")
        );
    }

    #[test]
    fn deepai_runtime_reads_the_runtime_file_from_the_home() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        write_deepai_models(&home);
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        assert!(apply_deepai_runtime_at(&mut settings, &store, &home).unwrap());
        let provider = settings
            .providers
            .iter()
            .find(|provider| provider.id == "deepai")
            .unwrap();
        assert_eq!(provider.models, vec!["kimi-k3"]);
        assert_eq!(provider.base_url, "http://127.0.0.1:60011");
        // A home without a runtime config leaves everything untouched.
        let empty = directory.path().join("empty-home");
        assert!(!apply_deepai_runtime_at(&mut settings, &store, &empty).unwrap());
    }

    #[test]
    fn a_runtime_without_a_loopback_endpoint_is_not_present() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let runtime = home.join(".deepai-code/userdata/deepai/pi/deepai");
        fs::create_dir_all(&runtime).unwrap();
        let write = |body: &str| fs::write(runtime.join("models.json"), body).unwrap();
        // A base URL that is present but outside loopback is refused at parse time.
        write(r#"{"providers":{"deepai":{"apiKey":"k","baseUrl":"http://example.com:60011"}}}"#);
        assert!(!deepai_runtime_present(&home));
        // A missing base URL parses as a runtime but is not usable.
        write(r#"{"providers":{"deepai":{"apiKey":"k","models":[{"id":"m"}]}}}"#);
        assert!(detect_deepai_runtime(&home).is_some());
        assert!(!deepai_runtime_present(&home));
    }

    #[test]
    fn deepai_runtime_clears_models_when_the_app_reports_none() {
        let directory = tempdir().unwrap();
        let store = directory.path().join("auth.json");
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        apply_deepai_runtime_from(
            &mut settings,
            &store,
            &deepai_runtime_for("http://127.0.0.1:60011", vec![deepai_model("m")]),
        )
        .unwrap();
        assert!(apply_deepai_runtime_from(
            &mut settings,
            &store,
            &deepai_runtime_for("http://127.0.0.1:60011", Vec::new()),
        )
        .unwrap());
        let provider = settings
            .providers
            .iter()
            .find(|provider| provider.id == "deepai")
            .unwrap();
        assert!(provider.models.is_empty());
        assert!(provider.model_context_windows.is_empty());
        assert!(provider.model_max_output_tokens.is_empty());
        assert!(provider.model_input_modalities.is_empty());
        assert!(provider.model_protocols.is_empty());
        assert_eq!(provider.default_model, None);
        settings.validate().unwrap();
    }

    #[test]
    fn adopting_twice_reads_a_meta_login_once() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        write_muse_key(&home, "LLM|first");
        let mut settings = GatewaySettings::default();
        settings.providers.clear();
        assert!(adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap());
        write_muse_key(&home, "LLM|second");
        adopt_local_cli_sessions_from(&mut settings, &store, &home).unwrap();
        assert_eq!(stored_access(&store, "meta").as_deref(), Some("LLM|first"));
    }

    #[tokio::test]
    async fn registering_keeps_the_login_it_read_whatever_the_read_limit_says() {
        let directory = tempdir().unwrap();
        let home = directory.path().join("home");
        let store = directory.path().join("auth.json");
        // A recent automatic read does not hold back a person asking for this.
        assert!(detect_local_cli_session_limited("meta", &home).is_none());
        assert!(!import_local_cli_session_at(&store, &home, "meta").await);
        write_muse_key(&home, "LLM|first");
        assert!(detect_local_cli_session_limited("meta", &home).is_none());
        assert!(import_local_cli_session_at(&store, &home, "meta").await);
        assert_eq!(stored_access(&store, "meta").as_deref(), Some("LLM|first"));
        // Kept, so the first request has nothing to read.
        fs::remove_file(home.join(".config/muse/auth.json")).unwrap();
        assert!(import_local_cli_session_at(&store, &home, "meta").await);
    }

    #[test]
    fn dismissing_under_an_alias_covers_the_canonical_provider() {
        let mut pool = AccountPoolSettings::default();
        dismiss_cli_import(&mut pool, "kimi-code");
        assert_eq!(pool.dismissed_cli_imports, ["kimi"]);
        assert!(cli_import_is_dismissed(&pool, "kimi"));
        // Recording it twice under either name keeps one entry.
        dismiss_cli_import(&mut pool, "kimi");
        assert_eq!(pool.dismissed_cli_imports.len(), 1);
        // An entry saved under the alias by an earlier build still counts.
        let mut saved = AccountPoolSettings::default();
        saved.dismissed_cli_imports.push("minimax-cn".into());
        assert!(cli_import_is_dismissed(&saved, "minimax"));
        restore_cli_import(&mut saved, "minimax");
        assert!(saved.dismissed_cli_imports.is_empty());
    }

    #[test]
    fn vibe_env_file_ignores_blank_and_missing_keys() {
        assert_eq!(parse_vibe_dotenv_key("MISTRAL_API_KEY=\n"), None);
        assert_eq!(parse_vibe_dotenv_key("MISTRAL_API_KEY=''\n"), None);
        assert_eq!(parse_vibe_dotenv_key("OTHER=value\n"), None);
        assert_eq!(
            parse_vibe_dotenv_key("MISTRAL_API_KEY=\"quoted\"\n").as_deref(),
            Some("quoted")
        );
        for line in [
            "MISTRAL_API_KEY=plain # note",
            "MISTRAL_API_KEY='plain' # note",
            "MISTRAL_API_KEY=\"plain\"",
        ] {
            assert_eq!(parse_vibe_dotenv_key(line).as_deref(), Some("plain"));
        }
        assert_eq!(
            parse_vibe_dotenv_key("MISTRAL_API_KEY=has#hash").as_deref(),
            Some("has#hash")
        );
        // Vibe drops these lines, so importing them would differ from what it uses.
        assert_eq!(parse_vibe_dotenv_key("MISTRAL_API_KEY='open"), None);
        assert_eq!(parse_vibe_dotenv_key("MISTRAL_API_KEY='a' b"), None);
    }

    #[test]
    fn vibe_env_file_uses_the_last_assignment_like_python_dotenv() {
        let parsed = |raw: &str| parse_vibe_dotenv_key(raw);
        assert_eq!(
            parsed("MISTRAL_API_KEY=old\nOTHER=x\nMISTRAL_API_KEY=new\n").as_deref(),
            Some("new")
        );
        // An empty assignment still replaces the earlier one.
        assert_eq!(parsed("MISTRAL_API_KEY=old\nMISTRAL_API_KEY=\n"), None);
        // A line it cannot parse assigns nothing.
        assert_eq!(
            parsed("MISTRAL_API_KEY=good\nMISTRAL_API_KEY='open\n").as_deref(),
            Some("good")
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
