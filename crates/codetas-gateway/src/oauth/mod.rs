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
        id: "opendesign",
        aliases: &["open-design"],
        display_name: "OpenDesign Cloud",
        flow: "device-code",
        native_login: true,
        cli_import: false,
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
        "opendesign" => login_opendesign().await?,
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
        "opendesign" => login_opendesign().await?,
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

const DEEPAI_REASONING_EFFORTS: &[&str] = &["low", "medium", "high"];

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
        provider
            .model_reasoning_efforts
            .retain(|model, _| keep(model));
        provider
            .model_default_reasoning_efforts
            .retain(|model, _| keep(model));
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
        // The app serves no effort list, so a reasoning model gets the levels
        // every chat endpoint accepts; without them Codex hides the effort picker.
        if model.reasoning {
            // A list the user already set is kept; only a missing one is filled.
            if !provider.model_reasoning_efforts.contains_key(&model.id) {
                let levels = DEEPAI_REASONING_EFFORTS
                    .iter()
                    .map(|effort| (*effort).to_string())
                    .collect::<Vec<_>>();
                provider
                    .model_reasoning_efforts
                    .insert(model.id.clone(), levels);
                changed = true;
            }
            if !provider
                .model_default_reasoning_efforts
                .contains_key(&model.id)
            {
                provider
                    .model_default_reasoning_efforts
                    .insert(model.id.clone(), "medium".into());
                changed = true;
            }
        } else {
            changed |= provider.model_reasoning_efforts.remove(&model.id).is_some();
            changed |= provider
                .model_default_reasoning_efforts
                .remove(&model.id)
                .is_some();
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
mod tests;
