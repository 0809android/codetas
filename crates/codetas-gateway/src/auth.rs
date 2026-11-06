use crate::config::{
    CredentialSource, CredentialTransport, ProviderCredential, ProviderDefinition,
};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION},
    RequestBuilder,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::ErrorKind,
    process::Stdio,
    sync::OnceLock,
    time::{Duration, Instant},
};
use tokio::{io::AsyncReadExt, process::Command, sync::Mutex, time::timeout};
use zeroize::Zeroizing;

const KEYRING_SERVICE: &str = "jp.kinocode.codetas";
const MAX_COMMAND_SECRET_BYTES: u64 = 64 * 1024;
const MAX_CACHED_COMMAND_SECRET_BYTES: usize = 16 * 1024;
const MAX_COMMAND_CACHE_ENTRIES: usize = 64;

/// Resolve a keychain credential through the `security` CLI instead of the
/// in-process keyring crate: the native SecItem/SecKeychain call can hang in
/// `CSSM_DecryptDataFinal` on some macOS setups, while the CLI completes.
async fn keychain_secret(service: &str, account: &str) -> Result<String, String> {
    let output = timeout(
        Duration::from_secs(10),
        Command::new("security")
            .args(["find-generic-password", "-s", service, "-a", account, "-w"])
            .output(),
    )
    .await
    .map_err(|_| "OS keychain lookup timed out".to_string())?
    .map_err(|_| "OS keychain command could not be run".to_string())?;
    if !output.status.success() {
        return Err("credential is not available in the OS keychain".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Save an API key under the same keychain item `Keychain` credentials read
/// (`credential:<reference>`). The key goes to `security -i` through stdin, so it
/// never appears in a process argument list, and the stored value is read back
/// before success is reported. The settings file only ever holds `reference`.
pub fn store_keychain_credential(reference: &str, secret: &str) -> Result<(), String> {
    use std::io::Write;
    let reference = reference.trim();
    let secret = secret.trim();
    if reference.is_empty()
        || reference.len() > 128
        || !reference.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/' | b':')
        })
    {
        return Err("API key reference is invalid".into());
    }
    if secret.is_empty() || secret.len() > 4096 {
        return Err("API key is empty or too long".into());
    }
    // The key is written inside double quotes on a `security -i` line; refuse
    // characters that would need escaping rather than guess at the quoting.
    if !secret
        .bytes()
        .all(|byte| (0x21..=0x7e).contains(&byte) && !matches!(byte, b'"' | b'\\' | b'\'' | b'`'))
    {
        return Err("API key contains characters that cannot be saved safely".into());
    }
    let username = keyring_username(CredentialSource::Keychain, reference);
    let mut child = std::process::Command::new("security")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "OS keychain command could not be run".to_string())?;
    let line = format!(
        "add-generic-password -U -s \"{KEYRING_SERVICE}\" -a \"{username}\" -w \"{secret}\"\n"
    );
    child
        .stdin
        .take()
        .ok_or_else(|| "OS keychain command could not be run".to_string())?
        .write_all(line.as_bytes())
        .map_err(|_| "OS keychain command could not be run".to_string())?;
    let _ = child.wait();
    let stored = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            KEYRING_SERVICE,
            "-a",
            &username,
            "-w",
        ])
        .output()
        .map_err(|_| "OS keychain command could not be run".to_string())?;
    if !stored.status.success() || String::from_utf8_lossy(&stored.stdout).trim() != secret {
        return Err("API key could not be saved to the OS keychain".into());
    }
    Ok(())
}

struct CachedCommandSecret {
    value: Zeroizing<String>,
    expires_at: Instant,
}

static COMMAND_SECRET_CACHE: OnceLock<Mutex<HashMap<String, CachedCommandSecret>>> =
    OnceLock::new();

pub async fn apply_provider_auth(
    mut request: RequestBuilder,
    provider: &ProviderDefinition,
    credential_override: Option<&ProviderCredential>,
) -> Result<RequestBuilder, String> {
    ensure_deepai_endpoint(provider)?;
    for (name, value) in &provider.headers {
        let header = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("invalid configured header name: {name}"))?;
        request = request.header(header, value);
    }
    for (name, env_key) in &provider.env_headers {
        let value = std::env::var(env_key)
            .map_err(|_| format!("environment variable {env_key} is not available to CODETAS"))?;
        let header = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("invalid configured header name: {name}"))?;
        request = request.header(header, value);
    }

    let credential = credential_override
        .cloned()
        .unwrap_or_else(|| effective_credential(provider));
    match credential.source {
        CredentialSource::None => Ok(request),
        CredentialSource::Environment => {
            let env_key = credential
                .reference
                .as_deref()
                .ok_or_else(|| "environment credential has no reference".to_string())?;
            let value = std::env::var(env_key).map_err(|_| {
                format!("environment variable {env_key} is not available to CODETAS")
            })?;
            if value.trim().is_empty() {
                return Err(format!("environment variable {env_key} is empty"));
            }
            attach_secret(request, credential, value)
        }
        CredentialSource::Keychain => {
            let reference = credential
                .reference
                .as_deref()
                .ok_or_else(|| "keychain credential has no reference".to_string())?;
            let username = keyring_username(credential.source, reference);
            let value = keychain_secret(KEYRING_SERVICE, &username).await?;
            if value.trim().is_empty() {
                return Err("OS keychain credential is empty".into());
            }
            attach_secret(request, credential, value)
        }
        CredentialSource::OAuth if credential.command.is_some() => {
            let value = cached_command_secret(&credential).await?;
            attach_secret(request, credential, value)
        }
        CredentialSource::OAuth => {
            let value = resolve_oauth_secret(provider, &credential).await?;
            attach_secret(request, credential, value)
        }
        CredentialSource::Command => {
            let value = cached_command_secret(&credential).await?;
            attach_secret(request, credential, value)
        }
        CredentialSource::Forward => Ok(request),
    }
}

pub(crate) async fn resolve_provider_headers(
    provider: &ProviderDefinition,
    credential_override: Option<&ProviderCredential>,
) -> Result<HeaderMap, String> {
    ensure_deepai_endpoint(provider)?;
    let mut headers = HeaderMap::new();
    for (name, value) in &provider.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("invalid configured header name: {name}"))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| "configured header value is invalid".to_string())?;
        headers.insert(name, value);
    }
    for (name, env_key) in &provider.env_headers {
        let value = std::env::var(env_key)
            .map_err(|_| format!("environment variable {env_key} is not available to CODETAS"))?;
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("invalid configured header name: {name}"))?;
        let value = HeaderValue::from_str(&value)
            .map_err(|_| format!("environment header {name} is invalid"))?;
        headers.insert(name, value);
    }

    let credential = credential_override
        .cloned()
        .unwrap_or_else(|| effective_credential(provider));
    let secret = match credential.source {
        CredentialSource::None | CredentialSource::Forward => None,
        CredentialSource::Environment => {
            let env_key = credential
                .reference
                .as_deref()
                .ok_or_else(|| "environment credential has no reference".to_string())?;
            let value = std::env::var(env_key).map_err(|_| {
                format!("environment variable {env_key} is not available to CODETAS")
            })?;
            if value.trim().is_empty() {
                return Err(format!("environment variable {env_key} is empty"));
            }
            Some(value)
        }
        CredentialSource::Keychain => {
            let reference = credential
                .reference
                .as_deref()
                .ok_or_else(|| "keychain credential has no reference".to_string())?;
            let username = keyring_username(credential.source, reference);
            let value = keychain_secret(KEYRING_SERVICE, &username).await?;
            if value.trim().is_empty() {
                return Err("OS keychain credential is empty".into());
            }
            Some(value)
        }
        CredentialSource::OAuth if credential.command.is_some() => {
            Some(cached_command_secret(&credential).await?)
        }
        CredentialSource::OAuth => Some(resolve_oauth_secret(provider, &credential).await?),
        CredentialSource::Command => Some(cached_command_secret(&credential).await?),
    };
    let Some(secret) = secret else {
        return Ok(headers);
    };
    let value = match credential.transport {
        CredentialTransport::Bearer => HeaderValue::from_str(&format!("Bearer {secret}")),
        CredentialTransport::XApiKey | CredentialTransport::CustomHeader => {
            HeaderValue::from_str(&secret)
        }
    }
    .map_err(|_| "provider credential contains invalid header bytes".to_string())?;
    let name = match credential.transport {
        CredentialTransport::Bearer => AUTHORIZATION,
        CredentialTransport::XApiKey => HeaderName::from_static("x-api-key"),
        CredentialTransport::CustomHeader => HeaderName::from_bytes(
            credential
                .header_name
                .as_deref()
                .ok_or_else(|| "custom credential header is missing".to_string())?
                .as_bytes(),
        )
        .map_err(|_| "custom credential header is invalid".to_string())?,
    };
    headers.insert(name, value);
    Ok(headers)
}

async fn command_secret(credential: &ProviderCredential) -> Result<String, String> {
    let command = credential
        .command
        .as_ref()
        .ok_or_else(|| "command credential has no command settings".to_string())?;
    let mut process = Command::new(&command.program);
    process
        .args(&command.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(cwd) = command.cwd.as_deref() {
        process.current_dir(cwd);
    }
    let mut child = process.spawn().map_err(|error| match error.kind() {
        ErrorKind::NotFound => "credential command was not found".to_string(),
        _ => "credential command could not be started".to_string(),
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "credential command stdout is unavailable".to_string())?;
    let timeout_duration = Duration::from_millis(command.timeout_ms);
    let result = timeout(timeout_duration, async move {
        let mut output = Vec::new();
        let mut bounded = stdout.take(MAX_COMMAND_SECRET_BYTES + 1);
        bounded
            .read_to_end(&mut output)
            .await
            .map_err(|_| "credential command output could not be read".to_string())?;
        let status = child
            .wait()
            .await
            .map_err(|_| "credential command did not exit cleanly".to_string())?;
        Ok::<_, String>((status, output))
    })
    .await
    .map_err(|_| "credential command timed out".to_string())??;
    if !result.0.success() {
        return Err("credential command returned a failure status".into());
    }
    if result.1.len() as u64 > MAX_COMMAND_SECRET_BYTES {
        return Err("credential command output exceeded 64 KiB".into());
    }
    let value = String::from_utf8(result.1)
        .map_err(|_| "credential command output is not UTF-8".to_string())?;
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err("credential command returned an empty secret".into());
    }
    Ok(value)
}

async fn cached_command_secret(credential: &ProviderCredential) -> Result<String, String> {
    let command = credential
        .command
        .as_ref()
        .ok_or_else(|| "command credential has no command settings".to_string())?;
    let key = command_cache_key(credential);
    let cache = COMMAND_SECRET_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut entries = cache.lock().await;
    let now = Instant::now();
    entries.retain(|_, entry| entry.expires_at > now);
    if let Some(entry) = entries.get(&key) {
        return Ok(entry.value.to_string());
    }
    // Keep the gate until this credential is resolved. This avoids a burst of concurrent
    // gh/gcloud processes (and their interactive lock files) on the first request after TTL.
    let value = command_secret(credential).await?;
    if value.len() <= MAX_CACHED_COMMAND_SECRET_BYTES && command.refresh_interval_ms >= 1_000 {
        if entries.len() >= MAX_COMMAND_CACHE_ENTRIES {
            entries.clear();
        }
        entries.insert(
            key,
            CachedCommandSecret {
                value: Zeroizing::new(value.clone()),
                expires_at: Instant::now()
                    + Duration::from_millis(command.refresh_interval_ms.min(24 * 60 * 60 * 1_000)),
            },
        );
    }
    Ok(value)
}

fn command_cache_key(credential: &ProviderCredential) -> String {
    let Some(command) = credential.command.as_ref() else {
        return String::new();
    };
    let mut digest = Sha256::new();
    let source = match credential.source {
        CredentialSource::None => "none",
        CredentialSource::Environment => "environment",
        CredentialSource::Keychain => "keychain",
        CredentialSource::OAuth => "oauth",
        CredentialSource::Command => "command",
        CredentialSource::Forward => "forward",
    };
    for value in [
        source,
        credential.reference.as_deref().unwrap_or_default(),
        command.program.as_str(),
        command.cwd.as_deref().unwrap_or_default(),
    ] {
        digest.update(value.as_bytes());
        digest.update([0]);
    }
    digest.update(command.refresh_interval_ms.to_le_bytes());
    for argument in &command.args {
        digest.update([0]);
        digest.update(argument.as_bytes());
    }
    let digest = digest.finalize();
    format!("{digest:x}")
}

fn keyring_username(source: CredentialSource, reference: &str) -> String {
    match source {
        CredentialSource::OAuth => format!("oauth:{reference}"),
        _ => format!("credential:{reference}"),
    }
}

fn effective_credential(provider: &ProviderDefinition) -> ProviderCredential {
    if provider.credential.source != CredentialSource::None {
        return provider.credential.clone();
    }
    match provider.api_key_env.as_deref() {
        Some(env_key) => ProviderCredential {
            source: CredentialSource::Environment,
            reference: Some(env_key.to_string()),
            transport: CredentialTransport::Bearer,
            header_name: None,
            command: None,
        },
        None => ProviderCredential::default(),
    }
}

/// A connection's URL can change after a login was attached to it, so this is
/// checked per request, whatever the login's reference says.
fn login_may_reach_connection(reference: &str, provider: &ProviderDefinition) -> bool {
    crate::oauth::session_key_provider(reference) != "mistral"
        || crate::compat::is_mistral_chat_endpoint(&provider.base_url)
}

/// DeepAI Code's capability belongs to the port it was read from. Refusing any
/// other endpoint means a rotated port never receives a newer bearer, whatever
/// credential source the connection was switched to.
fn ensure_deepai_endpoint(provider: &ProviderDefinition) -> Result<(), String> {
    if provider.id != "deepai" {
        return Ok(());
    }
    if deepai_endpoint_matches(
        &provider.base_url,
        crate::oauth::stored_deepai_endpoint().as_deref(),
    ) {
        return Ok(());
    }
    Err("The DeepAI Code endpoint changed. Re-import the connection in CODETAS.".into())
}

fn deepai_endpoint_matches(provider_base_url: &str, recorded: Option<&str>) -> bool {
    let Some(recorded) = recorded else {
        return false;
    };
    provider_base_url.trim().trim_end_matches('/') == recorded
}

async fn resolve_oauth_secret(
    provider: &ProviderDefinition,
    credential: &ProviderCredential,
) -> Result<String, String> {
    let reference = credential
        .reference
        .as_deref()
        .ok_or_else(|| "OAuth credential has no provider reference".to_string())?;
    if !login_may_reach_connection(reference, provider) {
        return Err(
            "The Mistral Vibe login is only sent to api.mistral.ai. Set this connection's own key."
                .into(),
        );
    }
    // Account-scoped references (`provider#account`) resolve through the auth
    // store; CLI detection only applies to the base provider key.
    let base = crate::oauth::session_key_provider(reference);
    if crate::oauth::provider_supports_native_oauth(base) {
        return crate::oauth::resolve_oauth_access_token(reference).await;
    }
    // This reads the stored session first: reading a CLI login can start a Keychain
    // lookup, which is limited and must not happen on every request.
    if let Some(access) = crate::oauth::resolve_oauth_access_token_if_logged_in(reference).await? {
        return Ok(access);
    }
    let username = keyring_username(CredentialSource::OAuth, reference);
    let value = keychain_secret(KEYRING_SERVICE, &username).await?;
    if value.trim().is_empty() {
        return Err("OAuth login is empty. Sign in from CODETAS.".into());
    }
    Ok(value)
}

fn attach_secret(
    request: RequestBuilder,
    credential: ProviderCredential,
    value: String,
) -> Result<RequestBuilder, String> {
    match credential.transport {
        CredentialTransport::Bearer => Ok(request.bearer_auth(value)),
        CredentialTransport::XApiKey => Ok(request.header("x-api-key", value)),
        CredentialTransport::CustomHeader => {
            let name = credential
                .header_name
                .as_deref()
                .ok_or_else(|| "custom credential header is missing".to_string())?;
            let header = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| "custom credential header is invalid".to_string())?;
            Ok(request.header(header, value))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(id: &str, base_url: &str) -> ProviderDefinition {
        ProviderDefinition {
            id: id.into(),
            base_url: base_url.into(),
            credential: crate::oauth::oauth_session_credential(id),
            ..ProviderDefinition::default()
        }
    }

    #[test]
    fn only_the_vibe_login_is_tied_to_a_host() {
        let official = connection("mistral", "https://api.mistral.ai/v1");
        let proxy = connection("mistral", "https://proxy.example/v1");
        assert!(login_may_reach_connection("mistral", &official));
        assert!(!login_may_reach_connection("mistral", &proxy));
        assert!(!login_may_reach_connection("mistral#work", &proxy));
        assert!(login_may_reach_connection("kimi", &proxy));
    }

    #[test]
    fn the_deepai_guard_requires_the_recorded_endpoint() {
        let endpoint = "http://127.0.0.1:60011";
        assert!(deepai_endpoint_matches(endpoint, Some(endpoint)));
        assert!(deepai_endpoint_matches(
            "http://127.0.0.1:60011/",
            Some(endpoint)
        ));
        assert!(!deepai_endpoint_matches(
            "http://127.0.0.1:60012",
            Some(endpoint)
        ));
        // No recorded endpoint is a fail-closed state, not a pass.
        assert!(!deepai_endpoint_matches(endpoint, None));
    }

    // These fail before the auth store is touched, so no real login is involved.
    #[tokio::test]
    async fn a_proxy_connection_never_gets_the_vibe_login_in_either_request_path() {
        let proxy = connection("mistral", "https://proxy.example/v1");
        assert!(resolve_provider_headers(&proxy, None).await.is_err());
        let request = reqwest::Client::new().get("https://proxy.example/v1/models");
        assert!(apply_provider_auth(request, &proxy, None).await.is_err());
    }
}
