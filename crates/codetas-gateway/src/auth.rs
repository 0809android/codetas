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
            .args([
                "find-generic-password",
                "-s",
                service,
                "-a",
                account,
                "-w",
            ])
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
            let value = resolve_oauth_secret(&credential).await?;
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
        CredentialSource::OAuth => Some(resolve_oauth_secret(&credential).await?),
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

async fn resolve_oauth_secret(credential: &ProviderCredential) -> Result<String, String> {
    let reference = credential
        .reference
        .as_deref()
        .ok_or_else(|| "OAuth credential has no provider reference".to_string())?;
    if crate::oauth::provider_supports_native_oauth(reference)
        || crate::oauth::detect_local_cli_session(reference, &crate::oauth::user_home_dir()).is_some()
        || crate::oauth::has_stored_session(reference)
    {
        return crate::oauth::resolve_oauth_access_token(reference).await;
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
