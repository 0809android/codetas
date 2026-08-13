use crate::{
    auth::{apply_provider_auth, resolve_provider_headers},
    config::{ModelMetadata, ProviderDefinition, ProviderTransport},
    copilot::exchange_copilot_token,
    network::pinned_client,
};
use bytes::BytesMut;
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use thiserror::Error;
use url::Url;

const DISCOVERY_HARD_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ModelDiscoveryError {
    #[error("invalid model discovery URL: {0}")]
    InvalidUrl(String),
    #[error("failed to create discovery client: {0}")]
    Client(#[from] reqwest::Error),
    #[error("provider authentication is unavailable: {0}")]
    Authentication(String),
    #[error("model discovery request failed: {0}")]
    Request(String),
    #[error("model discovery returned HTTP {status}: {message}")]
    Status { status: u16, message: String },
    #[error("model discovery response exceeded {0} bytes")]
    TooLarge(u64),
    #[error("model discovery returned invalid JSON: {0}")]
    InvalidJson(String),
    #[error("model discovery response has no supported model list")]
    UnsupportedShape,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConnectionReport {
    pub provider_id: String,
    pub reachable: bool,
    pub authenticated: bool,
    pub status: Option<u16>,
    pub latency_ms: u128,
    pub model_count: usize,
    pub message: String,
}

pub async fn test_provider_connection(provider: &ProviderDefinition) -> ProviderConnectionReport {
    if provider.transport != ProviderTransport::Standard {
        return test_native_transport(provider).await;
    }
    let mut probe = provider.clone();
    probe.discovery.enabled = true;
    let started = Instant::now();
    let result = discover_provider_models(&probe).await;
    let latency_ms = started.elapsed().as_millis();
    match result {
        Ok(models) => ProviderConnectionReport {
            provider_id: provider.id.clone(),
            reachable: true,
            authenticated: true,
            status: Some(200),
            latency_ms,
            model_count: models.len(),
            message: "provider model endpoint is reachable".into(),
        },
        Err(ModelDiscoveryError::Status { status, .. }) => ProviderConnectionReport {
            provider_id: provider.id.clone(),
            reachable: true,
            authenticated: !matches!(status, 401 | 403),
            status: Some(status),
            latency_ms,
            model_count: 0,
            message: if matches!(status, 401 | 403) {
                "provider rejected the configured credential".into()
            } else {
                "provider is reachable but its model endpoint returned an error".into()
            },
        },
        Err(ModelDiscoveryError::Authentication(message)) => ProviderConnectionReport {
            provider_id: provider.id.clone(),
            reachable: false,
            authenticated: false,
            status: None,
            latency_ms,
            model_count: 0,
            message,
        },
        Err(error) => ProviderConnectionReport {
            provider_id: provider.id.clone(),
            reachable: false,
            authenticated: false,
            status: None,
            latency_ms,
            model_count: 0,
            message: error.to_string(),
        },
    }
}

async fn test_native_transport(provider: &ProviderDefinition) -> ProviderConnectionReport {
    let started = Instant::now();
    let result = match provider.transport {
        ProviderTransport::Kiro => probe_kiro(provider).await,
        ProviderTransport::GithubCopilot => probe_github_copilot(provider).await,
        ProviderTransport::Standard => unreachable!(),
    };
    let latency_ms = started.elapsed().as_millis();
    match result {
        Ok((status, authenticated, message)) => ProviderConnectionReport {
            provider_id: provider.id.clone(),
            reachable: true,
            authenticated,
            status: Some(status),
            latency_ms,
            model_count: provider.models.len(),
            message,
        },
        Err(ModelDiscoveryError::Authentication(message)) => ProviderConnectionReport {
            provider_id: provider.id.clone(),
            reachable: false,
            authenticated: false,
            status: None,
            latency_ms,
            model_count: provider.models.len(),
            message,
        },
        Err(error) => ProviderConnectionReport {
            provider_id: provider.id.clone(),
            reachable: false,
            authenticated: false,
            status: None,
            latency_ms,
            model_count: provider.models.len(),
            message: error.to_string(),
        },
    }
}

async fn probe_github_copilot(
    provider: &ProviderDefinition,
) -> Result<(u16, bool, String), ModelDiscoveryError> {
    let headers = resolve_provider_headers(provider, None)
        .await
        .map_err(ModelDiscoveryError::Authentication)?;
    let authorization = headers
        .get(reqwest::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            ModelDiscoveryError::Authentication(
                "GitHub Copilot requires a GitHub bearer credential".into(),
            )
        })?;
    let endpoint = "https://api.github.com/copilot_internal/v2/token";
    let client = pinned_client(endpoint, provider, "CODETAS-ConnectionProbe/0.1")
        .await
        .map_err(ModelDiscoveryError::InvalidUrl)?;
    exchange_copilot_token(&client, authorization, Duration::from_secs(15))
        .await
        .map_err(ModelDiscoveryError::Authentication)?;
    Ok((
        200,
        true,
        "GitHub credential exchanged for a short-lived Copilot session token".into(),
    ))
}

async fn probe_kiro(
    provider: &ProviderDefinition,
) -> Result<(u16, bool, String), ModelDiscoveryError> {
    let endpoint = format!("{}/", provider.base_url.trim().trim_end_matches('/'));
    let client = pinned_client(&endpoint, provider, "CODETAS-ConnectionProbe/0.1")
        .await
        .map_err(ModelDiscoveryError::InvalidUrl)?;
    let headers = resolve_provider_headers(provider, None)
        .await
        .map_err(ModelDiscoveryError::Authentication)?;
    let mut request = client
        .post(endpoint)
        .header("content-type", "application/x-amz-json-1.0")
        .header("accept", "application/vnd.amazon.eventstream")
        .header(
            "x-amz-target",
            "AmazonCodeWhispererStreamingService.GenerateAssistantResponse",
        )
        .header("x-amzn-codewhisperer-optout", "true")
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .timeout(Duration::from_secs(15));
    for (name, value) in &headers {
        request = request.header(name, value);
    }
    if headers
        .get(reqwest::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|value| value.starts_with("ksk_"))
    {
        request = request.header("tokentype", "API_KEY");
    }
    if let Some(profile_arn) = provider.kiro_profile_arn.as_deref() {
        request = request.header("x-amzn-kiro-profile-arn", profile_arn);
    }
    let response =
        request.body("{}").send().await.map_err(|_| {
            ModelDiscoveryError::Request("Kiro native endpoint was unreachable".into())
        })?;
    let status = response.status().as_u16();
    let authenticated = !matches!(status, 401 | 403);
    Ok((
        status,
        authenticated,
        if authenticated {
            "Kiro native endpoint accepted the credential probe".into()
        } else {
            "Kiro native endpoint rejected the configured credential".into()
        },
    ))
}

pub async fn discover_provider_models(
    provider: &ProviderDefinition,
) -> Result<Vec<ModelMetadata>, ModelDiscoveryError> {
    provider
        .validate()
        .map_err(ModelDiscoveryError::InvalidUrl)?;
    if !provider.discovery.enabled {
        return Ok(Vec::new());
    }

    let base = provider.base_url.trim().trim_end_matches('/');
    let mut url = Url::parse(&format!("{base}{}", provider.discovery.path))
        .map_err(|error| ModelDiscoveryError::InvalidUrl(error.to_string()))?;
    if !provider.query_params.is_empty() {
        url.query_pairs_mut().extend_pairs(
            provider
                .query_params
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        );
    }
    if let Some(version) = provider.azure_api_version.as_ref() {
        url.query_pairs_mut().append_pair("api-version", version);
    }

    let client = pinned_client(url.as_str(), provider, "CODETAS-ModelDiscovery/0.1")
        .await
        .map_err(ModelDiscoveryError::InvalidUrl)?;
    let request = apply_provider_auth(
        client.get(url).header("accept", "application/json"),
        provider,
        None,
    )
    .await
    .map_err(ModelDiscoveryError::Authentication)?;
    let response = request
        .send()
        .await
        .map_err(|error| ModelDiscoveryError::Request(error.to_string()))?;
    let status = response.status();
    let limit = provider
        .limits
        .max_response_bytes
        .min(DISCOVERY_HARD_LIMIT_BYTES);
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        return Err(ModelDiscoveryError::TooLarge(limit));
    }
    let bytes = bounded_body(response, limit).await?;
    if !status.is_success() {
        let message = String::from_utf8_lossy(&bytes)
            .chars()
            .take(2_000)
            .collect::<String>();
        return Err(ModelDiscoveryError::Status {
            status: status.as_u16(),
            message,
        });
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| ModelDiscoveryError::InvalidJson(error.to_string()))?;
    parse_models(provider, &value)
}

async fn bounded_body(
    response: reqwest::Response,
    limit: u64,
) -> Result<BytesMut, ModelDiscoveryError> {
    let mut output = BytesMut::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| ModelDiscoveryError::Request(error.to_string()))?;
        if output.len() as u64 + chunk.len() as u64 > limit {
            return Err(ModelDiscoveryError::TooLarge(limit));
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

fn parse_models(
    provider: &ProviderDefinition,
    value: &Value,
) -> Result<Vec<ModelMetadata>, ModelDiscoveryError> {
    let rows = value
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| value.get("models").and_then(Value::as_array))
        .or_else(|| value.as_array())
        .ok_or(ModelDiscoveryError::UnsupportedShape)?;
    let mut models = BTreeMap::new();
    for row in rows.iter().take(provider.discovery.max_models) {
        let Some(object) = row.as_object() else {
            continue;
        };
        let Some(raw_id) = object
            .get("id")
            .or_else(|| object.get("name"))
            .or_else(|| object.get("model"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let model_id = raw_id.strip_prefix("models/").unwrap_or(raw_id).trim();
        if model_id.is_empty() || model_id.len() > 240 || model_id.chars().any(char::is_control) {
            continue;
        }
        let display_name = object
            .get("display_name")
            .or_else(|| object.get("displayName"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let context_window = number_at(
            object,
            &["context_window", "contextWindow", "inputTokenLimit"],
        );
        let max_output_tokens = number_at(
            object,
            &["max_output_tokens", "maxOutputTokens", "outputTokenLimit"],
        );
        let input_modalities = object
            .get("input_modalities")
            .or_else(|| object.get("inputModalities"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|item| item.to_ascii_lowercase())
                    .filter(|item| matches!(item.as_str(), "text" | "image" | "audio" | "video"))
                    .collect::<Vec<_>>()
            })
            .filter(|items| !items.is_empty())
            .unwrap_or_else(|| {
                let mut items = vec!["text".into()];
                if provider.capabilities.vision {
                    items.push("image".into());
                }
                if provider.capabilities.audio {
                    items.push("audio".into());
                }
                items
            });
        models
            .entry(model_id.to_string())
            .or_insert_with(|| ModelMetadata {
                provider_id: provider.id.clone(),
                model_id: model_id.to_string(),
                display_name,
                enabled: true,
                context_window,
                max_input_tokens: None,
                max_output_tokens,
                input_modalities,
                reasoning_efforts: Vec::new(),
                default_reasoning_effort: None,
                capabilities: provider.capabilities.clone(),
                input_price_per_million: None,
                output_price_per_million: None,
            });
    }
    Ok(models.into_values().collect())
}

fn number_at(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderProtocol;

    #[test]
    fn parses_openai_and_google_model_lists() {
        let mut provider = ProviderDefinition {
            id: "test".into(),
            name: "Test".into(),
            base_url: "https://models.example/v1".into(),
            protocol: ProviderProtocol::Responses,
            ..ProviderDefinition::default()
        };
        provider.discovery.enabled = true;
        let openai = parse_models(
            &provider,
            &serde_json::json!({
                "data": [{"id": "model-a"}]
            }),
        )
        .expect("OpenAI list should parse");
        assert_eq!(openai[0].model_id, "model-a");

        let google = parse_models(&provider, &serde_json::json!({
            "models": [{"name": "models/gemini-a", "displayName": "Gemini A", "inputTokenLimit": 1000}]
        })).expect("Google list should parse");
        assert_eq!(google[0].model_id, "gemini-a");
        assert_eq!(google[0].context_window, Some(1000));
    }
}
