use crate::config::{is_private_ip, UpdateSettings, SETTINGS_VERSION};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use futures_util::StreamExt;
use reqwest::redirect::Policy;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, net::SocketAddr, time::Duration};
use tokio::net::lookup_host;
use url::Url;

const MAX_MANIFEST_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignedEnvelope {
    payload: String,
    signature: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedUpdateManifest {
    pub version: String,
    pub channel: String,
    pub download_url: String,
    pub sha256: String,
    pub artifact_size_bytes: u64,
    pub settings_schema_version: u8,
    pub published_at: String,
    pub notes_url: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheck {
    pub current_version: String,
    pub update_available: bool,
    pub manifest: SignedUpdateManifest,
}

pub async fn check_signed_update(
    settings: &UpdateSettings,
    current_version: &str,
) -> Result<UpdateCheck, String> {
    let manifest_url = settings
        .manifest_url
        .as_deref()
        .ok_or("update manifest URL is not configured")?;
    let public_key = settings
        .public_key_base64
        .as_deref()
        .ok_or("update public key is not configured")?;
    let bytes = fetch_pinned_https(manifest_url).await?;
    let envelope: SignedEnvelope = serde_json::from_slice(&bytes)
        .map_err(|_| "update manifest envelope is invalid JSON".to_string())?;
    let payload = STANDARD
        .decode(envelope.payload.as_bytes())
        .map_err(|_| "update manifest payload is not valid base64".to_string())?;
    if payload.len() > MAX_MANIFEST_BYTES {
        return Err("update manifest payload exceeds the size limit".into());
    }
    let public_key = STANDARD
        .decode(public_key.as_bytes())
        .map_err(|_| "update public key is not valid base64".to_string())?;
    let public_key: [u8; 32] = public_key
        .try_into()
        .map_err(|_| "update public key must contain 32 bytes".to_string())?;
    let signature = STANDARD
        .decode(envelope.signature.as_bytes())
        .map_err(|_| "update signature is not valid base64".to_string())?;
    let signature = Signature::from_slice(&signature)
        .map_err(|_| "update signature has an invalid length".to_string())?;
    VerifyingKey::from_bytes(&public_key)
        .map_err(|_| "update public key is invalid".to_string())?
        .verify(&payload, &signature)
        .map_err(|_| "update manifest signature verification failed".to_string())?;

    let manifest: SignedUpdateManifest = serde_json::from_slice(&payload)
        .map_err(|_| "signed update payload is invalid JSON".to_string())?;
    validate_manifest(&manifest, settings)?;
    let current = Version::parse(current_version)
        .map_err(|_| "current CODETAS version is not valid semver".to_string())?;
    let available = Version::parse(&manifest.version)
        .map_err(|_| "update version is not valid semver".to_string())?
        > current;
    Ok(UpdateCheck {
        current_version: current_version.to_string(),
        update_available: available,
        manifest,
    })
}

fn validate_manifest(
    manifest: &SignedUpdateManifest,
    settings: &UpdateSettings,
) -> Result<(), String> {
    if manifest.channel != settings.channel.as_str() {
        return Err("signed update channel does not match the configured channel".into());
    }
    if manifest.settings_schema_version != SETTINGS_VERSION {
        return Err(format!(
            "update requires unsupported settings schema version {}",
            manifest.settings_schema_version
        ));
    }
    validate_https_url(&manifest.download_url, "update download")?;
    if let Some(notes) = manifest.notes_url.as_deref() {
        validate_https_url(notes, "update notes")?;
    }
    if manifest.sha256.len() != 64 || !manifest.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("update SHA-256 must be 64 hexadecimal characters".into());
    }
    if manifest.artifact_size_bytes == 0 || manifest.artifact_size_bytes > 1024 * 1024 * 1024 {
        return Err("update artifact size must be between 1 byte and 1 GiB".into());
    }
    if manifest.published_at.len() > 80 || manifest.published_at.chars().any(char::is_control) {
        return Err("update publishedAt is invalid".into());
    }
    Ok(())
}

fn validate_https_url(value: &str, label: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|_| format!("{label} URL is invalid"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || url.username() != ""
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(format!("{label} URL must be credential-free HTTPS"));
    }
    Ok(url)
}

async fn fetch_pinned_https(value: &str) -> Result<Vec<u8>, String> {
    let url = validate_https_url(value, "update manifest")?;
    let host = url.host_str().ok_or("update manifest URL has no host")?;
    let port = url.port_or_known_default().unwrap_or(443);
    let resolved = lookup_host((host, port))
        .await
        .map_err(|_| "update manifest host could not be resolved".to_string())?;
    let mut seen = HashSet::new();
    let addresses = resolved
        .filter(|address| seen.insert(address.ip()))
        .collect::<Vec<SocketAddr>>();
    if addresses.is_empty() || addresses.iter().any(|address| is_private_ip(address.ip())) {
        return Err("update manifest host resolved to a private or local address".into());
    }
    let client = reqwest::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(Policy::none())
        .resolve_to_addrs(host, &addresses)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .user_agent("CODETAS-Updater/0.1")
        .build()
        .map_err(|_| "update HTTP client could not be created".to_string())?;
    let response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|_| "update manifest request failed".to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "update manifest returned HTTP {}",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MANIFEST_BYTES as u64)
    {
        return Err("update manifest exceeds the size limit".into());
    }
    let mut stream = response.bytes_stream();
    let mut output = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "update manifest response failed".to_string())?;
        if output.len().saturating_add(chunk.len()) > MAX_MANIFEST_BYTES {
            return Err("update manifest exceeds the size limit".into());
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}
