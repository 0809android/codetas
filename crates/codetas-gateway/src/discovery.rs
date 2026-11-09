use crate::{
    auth::{apply_provider_auth, resolve_provider_headers},
    config::{
        GoogleMode, ModelMetadata, ProviderCapabilities, ProviderDefinition, ProviderTransport,
    },
    copilot::exchange_copilot_token,
    network::pinned_client,
    oauth::{
        fetch_antigravity_cli_models, probe_antigravity_cloud_project, AntigravityProjectError,
    },
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
    if provider.id == "google-antigravity" && provider.google_mode == GoogleMode::CloudCodeAssist {
        return test_antigravity(provider).await;
    }
    // The DeepAI Code transport serves no model list, but it exposes a status
    // endpoint that distinguishes "not running", "running", and "signed out".
    if provider.id == "deepai" {
        return test_deepai(provider).await;
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

async fn test_deepai(provider: &ProviderDefinition) -> ProviderConnectionReport {
    let started = Instant::now();
    let result = async {
        let url = format!(
            "{}/model-status",
            provider.base_url.trim().trim_end_matches('/')
        );
        let client = pinned_client(&url, provider, "CODETAS-ConnectionProbe/0.1")
            .await
            .map_err(|error| error.to_string())?;
        let response = apply_provider_auth(client.get(&url), provider, None)
            .await?
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        // The status probe cannot carry a chat identity, so a valid credential
        // answers 400 with this transport error type; an invalid one answers 403.
        let body = if status == 400 {
            response.text().await.unwrap_or_default()
        } else {
            String::new()
        };
        Ok::<(u16, bool), String>((status, body.contains("deepai_connection_error")))
    }
    .await;
    let latency_ms = started.elapsed().as_millis();
    match result {
        Ok((status, connection_error)) => {
            let (reachable, authenticated, normalized, message) =
                classify_deepai_probe(status, connection_error);
            ProviderConnectionReport {
                provider_id: provider.id.clone(),
                reachable,
                authenticated,
                status: normalized,
                latency_ms,
                model_count: provider.models.len(),
                message: message.into(),
            }
        }
        Err(message) => ProviderConnectionReport {
            provider_id: provider.id.clone(),
            reachable: false,
            authenticated: false,
            status: None,
            latency_ms,
            model_count: 0,
            message,
        },
    }
}

/// The DeepAI status probe answers 400 with a transport error even when the
/// capability was accepted, so that specific answer counts as authenticated.
fn classify_deepai_probe(
    status: u16,
    connection_error: bool,
) -> (bool, bool, Option<u16>, &'static str) {
    if status == 200 || (status == 400 && connection_error) {
        return (
            true,
            true,
            None,
            "the running DeepAI Code model transport is reachable",
        );
    }
    if matches!(status, 401 | 403) {
        return (
            true,
            false,
            Some(status),
            "DeepAI Code is running but the model transport rejected the credential",
        );
    }
    (
        true,
        false,
        Some(status),
        "DeepAI Code answered but the model transport returned an error",
    )
}

async fn test_antigravity(provider: &ProviderDefinition) -> ProviderConnectionReport {
    let started = Instant::now();
    let result = async {
        let headers = resolve_provider_headers(provider, None)
            .await
            .map_err(|message| AntigravityProjectError::Authentication {
                status: None,
                message,
            })?;
        let access_token = headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| AntigravityProjectError::Authentication {
                status: None,
                message: "Antigravity requires an imported OAuth bearer credential".into(),
            })?;
        probe_antigravity_cloud_project(access_token).await
    }
    .await;
    let latency_ms = started.elapsed().as_millis();
    match result {
        Ok(_) => ProviderConnectionReport {
            provider_id: provider.id.clone(),
            reachable: true,
            authenticated: true,
            status: Some(200),
            latency_ms,
            model_count: provider.models.len(),
            message: "Antigravity Cloud Code Assist session is reachable".into(),
        },
        Err(AntigravityProjectError::Authentication { status, message }) => {
            ProviderConnectionReport {
                provider_id: provider.id.clone(),
                reachable: status.is_some(),
                authenticated: false,
                status,
                latency_ms,
                model_count: provider.models.len(),
                message,
            }
        }
        Err(AntigravityProjectError::Status { status, message }) => ProviderConnectionReport {
            provider_id: provider.id.clone(),
            reachable: true,
            authenticated: true,
            status: Some(status),
            latency_ms,
            model_count: provider.models.len(),
            message,
        },
        Err(AntigravityProjectError::InvalidResponse { status, message }) => {
            ProviderConnectionReport {
                provider_id: provider.id.clone(),
                reachable: true,
                authenticated: true,
                status: Some(status),
                latency_ms,
                model_count: provider.models.len(),
                message,
            }
        }
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
    let result = fetch_provider_models(provider).await;
    if is_mistral_catalog(provider) && mistral_catalog_is_out_of_reach(&result) {
        let mut fallback = provider.clone();
        fallback.discovery.path = "/models".into();
        return fetch_provider_models(&fallback).await;
    }
    result
}

fn is_mistral_catalog(provider: &ProviderDefinition) -> bool {
    provider.id == "mistral" && provider.discovery.path == "/model-catalog"
}

/// The catalog is not a published API and serves the Vibe surface, so a key may not
/// be allowed to read it, or it may go away. A 429 or 5xx says neither, and falling
/// back then would swap the plan's models for every model on the account until the
/// next refresh. A rejected key would fail on `/models` as well.
fn mistral_catalog_is_out_of_reach(
    result: &Result<Vec<ModelMetadata>, ModelDiscoveryError>,
) -> bool {
    matches!(
        result,
        Err(ModelDiscoveryError::Status {
            status: 403 | 404,
            ..
        })
    )
}

async fn fetch_provider_models(
    provider: &ProviderDefinition,
) -> Result<Vec<ModelMetadata>, ModelDiscoveryError> {
    provider
        .validate()
        .map_err(ModelDiscoveryError::InvalidUrl)?;
    if provider.id == "google-antigravity" {
        return discover_antigravity_models(provider).await;
    }
    if !provider.discovery.enabled {
        return Ok(Vec::new());
    }

    let base = provider.base_url.trim().trim_end_matches('/');
    // Anthropic serves model discovery under its versioned prefix. A provider
    // saved without `/v1` would otherwise probe `<host>/models` and read the
    // edge's empty 404 as "reachable but errored".
    let base = if provider.protocol == crate::config::ProviderProtocol::AnthropicMessages {
        crate::config::versioned_anthropic_base(base)
    } else {
        base.to_string()
    };
    let base = base.as_str();
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
    // The catalog is per surface, and a discovery path cannot carry a query.
    if is_mistral_catalog(provider) {
        url.query_pairs_mut().append_pair("surface", "vibe-code");
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
    // Do not surface upstream bodies: they may contain credential details.
    if !status.is_success() {
        return Err(ModelDiscoveryError::Status {
            status: status.as_u16(),
            message: if matches!(status.as_u16(), 401 | 403) {
                "the provider rejected the request".into()
            } else {
                "the provider returned an error".into()
            },
        });
    }
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

/// Provider-level image support is only an upper bound.  A discovered model is
/// image-capable only when the provider explicitly identifies that model (or
/// its wire alias) in `image_generation_models`.
fn discovered_model_capabilities(
    provider: &ProviderDefinition,
    model_id: &str,
) -> ProviderCapabilities {
    let mut capabilities = provider.capabilities.clone();
    let wire_model = provider.wire_model_id(model_id);
    capabilities.image_generation = provider.capabilities.image_generation
        && provider
            .image_generation_models
            .iter()
            .any(|configured| provider.wire_model_id(configured) == wire_model);
    capabilities
}

async fn discover_antigravity_models(
    provider: &ProviderDefinition,
) -> Result<Vec<ModelMetadata>, ModelDiscoveryError> {
    let rows = fetch_antigravity_cli_models()
        .await
        .map_err(ModelDiscoveryError::Request)?;
    Ok(antigravity_models_from_cli_rows(provider, &rows))
}

fn antigravity_models_from_cli_rows(
    provider: &ProviderDefinition,
    rows: &[(String, Option<String>)],
) -> Vec<ModelMetadata> {
    let mut models = BTreeMap::new();
    for (model_id, display_name) in rows {
        models
            .entry(model_id.clone())
            .or_insert_with(|| ModelMetadata {
                provider_id: provider.id.clone(),
                model_id: model_id.clone(),
                display_name: display_name.clone(),
                enabled: true,
                context_window: crate::registry::resolve_model_context_window(
                    &provider.model_context_windows,
                    model_id,
                ),
                max_input_tokens: None,
                max_output_tokens: None,
                input_modalities: provider
                    .model_input_modalities
                    .get(model_id)
                    .cloned()
                    .unwrap_or_else(|| {
                        let mut items = vec!["text".into()];
                        if provider.capabilities.vision {
                            items.push("image".into());
                        }
                        items
                    }),
                reasoning_efforts: provider
                    .model_reasoning_efforts
                    .get(model_id)
                    .cloned()
                    .unwrap_or_else(|| {
                        if matches!(model_id.as_str(), "claude-opus-5-5" | "claude-sonnet-5-5") {
                            vec!["low".into(), "medium".into(), "high".into()]
                        } else {
                            Vec::new()
                        }
                    }),
                default_reasoning_effort: provider
                    .model_default_reasoning_efforts
                    .get(model_id)
                    .cloned()
                    .or_else(|| {
                        matches!(model_id.as_str(), "claude-opus-5-5" | "claude-sonnet-5-5")
                            .then(|| "medium".into())
                    }),
                capabilities: discovered_model_capabilities(provider, model_id),
                input_price_per_million: None,
                output_price_per_million: None,
                instructions_template: None,
            });
    }
    models.into_values().collect()
}

/// Online discovery uses a fixed official origin: forwarded credentials must
/// never be sent to an editable provider URL or followed through redirects.
pub async fn discover_codex_online_models(
    provider: &ProviderDefinition,
    access_token: &str,
    account_id: &str,
    client_version: &str,
) -> Result<Vec<ModelMetadata>, ModelDiscoveryError> {
    let mut url = Url::parse("https://chatgpt.com/backend-api/codex/models").unwrap();
    url.query_pairs_mut()
        .append_pair("client_version", client_version);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()?;
    fetch_codex_model_list(
        &client,
        url,
        provider,
        access_token,
        account_id,
        client_version,
    )
    .await
}

async fn fetch_codex_model_list(
    client: &reqwest::Client,
    url: Url,
    provider: &ProviderDefinition,
    access_token: &str,
    account_id: &str,
    client_version: &str,
) -> Result<Vec<ModelMetadata>, ModelDiscoveryError> {
    let response = client
        .get(url)
        .bearer_auth(access_token)
        .header("ChatGPT-Account-ID", account_id)
        .header("originator", "codex_cli_rs")
        .header("User-Agent", format!("codex_cli_rs/{client_version}"))
        .send()
        .await
        .map_err(|_| {
            ModelDiscoveryError::Request(
                "Codexのオンラインモデル取得に失敗しました。接続を確認してください".into(),
            )
        })?;
    let status = response.status();
    if !status.is_success() {
        // Do not surface upstream bodies: they may contain credential details.
        return Err(ModelDiscoveryError::Status {
            status: status.as_u16(),
            message: match status.as_u16() {
                401 => "Codexの認証が切れています。Codexで再ログインしてください",
                403 => "Codexがモデル一覧へのアクセスを拒否しました。アカウントの利用権限を確認してください",
                429 => "Codexの取得制限に達しました。時間をおいて再実行してください",
                _ => "Codexのオンラインモデル取得に失敗しました",
            }.into(),
        });
    }
    let bytes = bounded_body(response, DISCOVERY_HARD_LIMIT_BYTES).await?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        ModelDiscoveryError::InvalidJson("Codexのモデル一覧が不正なJSONです".into())
    })?;
    parse_codex_model_list(provider, &value)
}

fn parse_codex_model_list(
    provider: &ProviderDefinition,
    value: &Value,
) -> Result<Vec<ModelMetadata>, ModelDiscoveryError> {
    let rows = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or(ModelDiscoveryError::UnsupportedShape)?;
    let normalized = rows
        .iter()
        .filter(|row| row.get("visibility").and_then(Value::as_str) == Some("list"))
        .map(|row| {
            serde_json::json!({
                "id": row.get("slug"),
                "display_name": row.get("display_name"),
                "context_window": row.get("context_window"),
                "input_modalities": row.get("input_modalities"),
            })
        })
        .collect::<Vec<_>>();
    let mut models = parse_models(provider, &Value::Array(normalized))?;
    for model in &mut models {
        if let Some(row) = rows
            .iter()
            .find(|row| row.get("slug").and_then(Value::as_str) == Some(model.model_id.as_str()))
        {
            model.reasoning_efforts = row
                .get("supported_reasoning_levels")
                .and_then(Value::as_array)
                .map(|levels| {
                    levels
                        .iter()
                        .filter_map(|level| level.get("effort").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            model.default_reasoning_effort = row
                .get("default_reasoning_level")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
    }
    if models.is_empty() {
        return Err(ModelDiscoveryError::Request(
            "Codexの応答に選択可能なモデルがありません。既存の一覧は保持しました".into(),
        ));
    }
    Ok(models)
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
        if provider.id == "mistral" && !mistral_row_can_run_agents(object) {
            continue;
        }
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
            // Elsewhere `label` may hold a tag such as "free" or "beta".
            .or_else(|| {
                (provider.id == "mistral")
                    .then(|| object.get("label"))
                    .flatten()
            })
            .and_then(Value::as_str)
            .map(str::to_string);
        let context_window = number_at(
            object,
            &[
                "context_window",
                "contextWindow",
                "context_length",
                "max_context_length",
                "max_model_len",
                "inputTokenLimit",
            ],
        )
        // OpenDesign nests its limits: `context_limit` is the model's window. Its
        // `context_budget` is the size OpenDesign's own app compacts to, not a cap:
        // a 390k-token request was accepted by a model whose budget is 232k.
        .or_else(|| {
            row.pointer("/metadata/context_limit")
                .and_then(Value::as_u64)
        })
        .or_else(|| {
            crate::registry::resolve_model_context_window(&provider.model_context_windows, model_id)
        });
        let max_output_tokens = number_at(
            object,
            &["max_output_tokens", "maxOutputTokens", "outputTokenLimit"],
        );
        let input_modalities = object
            .get("input_modalities")
            .or_else(|| object.get("inputModalities"))
            .or_else(|| row.pointer("/architecture/input_modalities"))
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
                // A row that says it has no vision outranks the provider default.
                if provider.capabilities.vision
                    && row.pointer("/capabilities/vision").and_then(Value::as_bool) != Some(false)
                {
                    items.push("image".into());
                }
                if provider.capabilities.audio {
                    items.push("audio".into());
                }
                items
            });
        let max_input_tokens = opendesign_max_input_tokens(provider, row, context_window);
        let reasoning_efforts = if provider.id == "mistral" {
            mistral_catalog_efforts(row)
        } else {
            Vec::new()
        };
        let default_reasoning_effort = mistral_default_effort(&reasoning_efforts);
        models
            .entry(model_id.to_string())
            .or_insert_with(|| ModelMetadata {
                provider_id: provider.id.clone(),
                model_id: model_id.to_string(),
                display_name,
                enabled: true,
                context_window,
                max_input_tokens,
                max_output_tokens,
                input_modalities,
                reasoning_efforts,
                default_reasoning_effort,
                capabilities: discovered_model_capabilities(provider, model_id),
                input_price_per_million: None,
                output_price_per_million: None,
                instructions_template: None,
            });
    }
    Ok(models.into_values().collect())
}

/// The levels Mistral's catalog lists for a model, so one it adds later reasons
/// without a registry change, as long as CODETAS knows the level. The catalog is not
/// a published API, and one value the validator rejects would fail the whole refresh.
/// `none` is left out for the reason in `apply_mistral`.
fn mistral_catalog_efforts(row: &Value) -> Vec<String> {
    let mut efforts = Vec::new();
    for effort in row
        .pointer("/parameters/reasoning")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let effort = effort.trim().to_ascii_lowercase();
        if effort != "none"
            && crate::config::is_reasoning_effort(&effort)
            && !efforts.contains(&effort)
        {
            efforts.push(effort);
        }
    }
    efforts
}

/// `high`, like the models in `apply_mistral`. Without a default a request with no
/// effort would use the first level, which is the lowest, and answers would get
/// worse without any sign of why.
fn mistral_default_effort(efforts: &[String]) -> Option<String> {
    efforts
        .iter()
        .find(|effort| *effort == "high")
        .or_else(|| efforts.last())
        .cloned()
}

/// Mistral's list mixes in embedding, OCR, moderation and audio models, and
/// flags each row. A coding agent needs chat completions with tool calls.
fn mistral_row_can_run_agents(row: &serde_json::Map<String, Value>) -> bool {
    let lacks = |capability: &str| {
        row.get("capabilities")
            .and_then(|capabilities| capabilities.get(capability))
            .and_then(Value::as_bool)
            == Some(false)
    };
    !lacks("completion_chat") && !lacks("function_calling")
}

/// Where compaction should start for an OpenDesign model. Its `context_budget` is
/// what OpenDesign's own app compacts to (228k-328k), but the part of a request
/// that compaction cannot shrink already reaches about 250k tokens with 400+ tools,
/// so a trigger below that compacts after every tool call. The budget is therefore
/// raised to 400k, as for a model with no stated limit, and never past the window.
fn opendesign_max_input_tokens(
    provider: &ProviderDefinition,
    row: &Value,
    context_window: Option<u64>,
) -> Option<u64> {
    const FLOOR: u64 = 400_000;
    if provider.id != "opendesign" {
        return None;
    }
    let window = context_window?;
    let budget = row
        .pointer("/metadata/context_budget")
        .and_then(Value::as_u64)?;
    Some(budget.max(FLOOR).min(window))
}

fn number_at(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderProtocol;
    use crate::oauth::parse_antigravity_cli_models;

    #[test]
    fn opendesign_limits_are_read_from_their_nested_fields() {
        let rows = serde_json::json!({"data": [
            {"id": "deepseek-v4.1-flash", "architecture": {"input_modalities": ["text", "image"]},
             "metadata": {"context_limit": 1048000, "context_budget": 328000, "output_budget": 128000}},
            {"id": "deepseek-v4-pro", "architecture": {"input_modalities": ["text"]},
             "metadata": {"context_limit": 1048000, "context_budget": 328000}}
        ]});
        let provider = crate::provider_presets()
            .into_iter()
            .find(|preset| preset.id == "opendesign")
            .unwrap()
            .instantiate(None)
            .unwrap();
        let models = parse_models(&provider, &rows).unwrap();
        let by_id = |id: &str| models.iter().find(|model| model.model_id == id).unwrap();
        let flash = by_id("deepseek-v4.1-flash");
        let pro = by_id("deepseek-v4-pro");
        assert_eq!(flash.context_window, Some(1_048_000));
        assert!(flash.input_modalities.contains(&"image".to_string()));
        assert!(!pro.input_modalities.contains(&"image".to_string()));
        // 328k budgets are raised to 400k; a window under that stays the cap.
        assert_eq!(flash.max_input_tokens, Some(400_000));
        let small = parse_models(
            &provider,
            &serde_json::json!({"data": [{"id": "kimi-k2.7-code",
                "metadata": {"context_limit": 262000, "context_budget": 262000}}]}),
        )
        .unwrap();
        assert_eq!(small[0].max_input_tokens, Some(262_000));
    }

    #[test]
    fn deepai_status_probe_treats_the_identity_error_as_authenticated() {
        // A valid credential answers 400 with this transport error type.
        assert_eq!(
            classify_deepai_probe(400, true),
            (
                true,
                true,
                None,
                "the running DeepAI Code model transport is reachable"
            )
        );
        assert_eq!(
            classify_deepai_probe(200, false),
            (
                true,
                true,
                None,
                "the running DeepAI Code model transport is reachable"
            )
        );
        // An invalid credential answers 403; a bare 400 stays an error.
        assert_eq!(
            classify_deepai_probe(403, false),
            (
                true,
                false,
                Some(403),
                "DeepAI Code is running but the model transport rejected the credential"
            )
        );
        assert_eq!(
            classify_deepai_probe(400, false).2,
            Some(400),
            "a 400 without the transport error type is not success"
        );
        assert!(!classify_deepai_probe(500, false).1);
    }

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

    #[test]
    fn mistral_discovery_keeps_only_agent_capable_models_with_their_vision() {
        let provider = crate::provider_presets()
            .into_iter()
            .find(|preset| preset.id == "mistral")
            .unwrap()
            .instantiate(None)
            .unwrap();
        let models = parse_models(
            &provider,
            &serde_json::json!({"data": [
                {"id": "mistral-medium-latest", "max_context_length": 262144,
                    "capabilities": {"completion_chat": true, "function_calling": true, "vision": true}},
                {"id": "text-only-model",
                    "capabilities": {"completion_chat": true, "function_calling": true, "vision": false}},
                {"id": "mistral-embed",
                    "capabilities": {"completion_chat": false, "function_calling": false}},
                {"id": "chat-without-tools",
                    "capabilities": {"completion_chat": true, "function_calling": false}}
            ]}),
        )
        .unwrap();
        let ids: Vec<_> = models.iter().map(|model| model.model_id.as_str()).collect();
        assert_eq!(ids, ["mistral-medium-latest", "text-only-model"]);
        assert_eq!(models[0].context_window, Some(262_144));
        assert!(models[0].input_modalities.contains(&"image".to_string()));
        assert!(!models[1].input_modalities.contains(&"image".to_string()));
    }

    #[test]
    fn mistral_catalog_row_brings_its_label_and_reasoning_levels() {
        let provider = crate::provider_presets()
            .into_iter()
            .find(|preset| preset.id == "mistral")
            .unwrap()
            .instantiate(None)
            .unwrap();
        assert!(is_mistral_catalog(&provider));
        let models = parse_models(
            &provider,
            &serde_json::json!({"models": [{
                "id": "mistral-next", "label": "Mistral Next", "max_context_length": 1000000,
                "parameters": {"reasoning": ["none", "low", "high"]},
                "capabilities": {"completion_chat": true, "function_calling": true, "vision": true}
            }]}),
        )
        .unwrap();
        assert_eq!(models[0].display_name.as_deref(), Some("Mistral Next"));
        assert_eq!(models[0].reasoning_efforts, ["low", "high"]);
        assert_eq!(models[0].default_reasoning_effort.as_deref(), Some("high"));
        assert_eq!(models[0].context_window, Some(1_000_000));
    }

    #[test]
    fn mistral_catalog_efforts_keep_only_what_settings_accept() {
        let efforts = mistral_catalog_efforts(&serde_json::json!({
            "parameters": {"reasoning": ["None", "LOW", "low", " High ", "extreme", 3, "max"]}
        }));
        assert_eq!(efforts, ["low", "high", "max"]);
        // The filter is the validator's own check, so what it keeps cannot fail the refresh.
        assert!(efforts
            .iter()
            .all(|effort| crate::config::is_reasoning_effort(effort)));
    }

    #[test]
    fn mistral_default_effort_is_high_or_else_the_top_level() {
        let levels = |efforts: &[&str]| efforts.iter().map(|e| e.to_string()).collect::<Vec<_>>();
        assert_eq!(
            mistral_default_effort(&levels(&["low", "high", "max"])).as_deref(),
            Some("high")
        );
        assert_eq!(
            mistral_default_effort(&levels(&["low", "medium"])).as_deref(),
            Some("medium")
        );
        assert_eq!(mistral_default_effort(&[]), None);
    }

    #[test]
    fn only_an_unreachable_mistral_catalog_falls_back_to_the_full_list() {
        let failed = |status| {
            Err(ModelDiscoveryError::Status {
                status,
                message: String::new(),
            })
        };
        assert!(mistral_catalog_is_out_of_reach(&failed(403)));
        assert!(mistral_catalog_is_out_of_reach(&failed(404)));
        for status in [401, 429, 500, 503] {
            assert!(!mistral_catalog_is_out_of_reach(&failed(status)));
        }
        assert!(!mistral_catalog_is_out_of_reach(&Ok(Vec::new())));
    }

    #[test]
    fn label_names_a_model_only_for_mistral() {
        let provider = ProviderDefinition {
            id: "other".into(),
            name: "Other".into(),
            base_url: "https://api.example.com/v1".into(),
            protocol: ProviderProtocol::ChatCompletions,
            ..ProviderDefinition::default()
        };
        let models = parse_models(
            &provider,
            &serde_json::json!({"data": [{"id": "model-a", "label": "free"}]}),
        )
        .unwrap();
        assert_eq!(models[0].display_name, None);
    }

    #[test]
    fn discovered_xai_models_inherit_context_when_the_api_omits_it() {
        let mut provider = ProviderDefinition {
            id: "xai".into(),
            name: "xAI".into(),
            base_url: "https://api.x.ai/v1".into(),
            protocol: ProviderProtocol::ChatCompletions,
            ..ProviderDefinition::default()
        };
        provider
            .model_context_windows
            .insert("grok-4.6".into(), 500_000);
        let models = parse_models(
            &provider,
            &serde_json::json!({
                "data": [
                    {"id": "grok-4.6"},
                    {"id": "grok-4.7"},
                    {"id": "grok-4.8", "context_length": 512_000},
                    {"id": "grok-imagine-image"}
                ]
            }),
        )
        .expect("xAI list should parse");
        let window = |id: &str| {
            models
                .iter()
                .find(|model| model.model_id == id)
                .and_then(|model| model.context_window)
        };
        assert_eq!(window("grok-4.6"), Some(500_000));
        assert_eq!(window("grok-4.7"), Some(500_000));
        assert_eq!(window("grok-4.8"), Some(512_000));
        assert_eq!(window("grok-imagine-image"), None);
    }

    #[test]
    fn provider_image_capability_is_scoped_to_explicit_image_models() {
        let mut provider = ProviderDefinition {
            id: "xai".into(),
            name: "xAI".into(),
            base_url: "https://models.example/v1".into(),
            protocol: ProviderProtocol::Responses,
            ..ProviderDefinition::default()
        };
        provider.capabilities.image_generation = true;
        provider.image_generation_models = vec!["image-model".into()];

        let models = parse_models(
            &provider,
            &serde_json::json!({
                "data": [{"id": "chat-model"}, {"id": "image-model"}]
            }),
        )
        .expect("model list should parse");

        assert!(!models[0].capabilities.image_generation);
        assert!(models[1].capabilities.image_generation);
    }

    #[tokio::test]
    async fn codex_online_status_errors_are_not_cached_or_leaked() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for status in [401, 403, 429, 500, 302] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = Url::parse(&format!(
                "http://{}/models?client_version=0.159.1",
                listener.local_addr().unwrap()
            ))
            .unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 8192];
                let count = socket.read(&mut request).await.unwrap();
                let headers = String::from_utf8_lossy(&request[..count]).to_ascii_lowercase();
                assert!(headers.contains("authorization: bearer test-token"));
                assert!(headers.contains("chatgpt-account-id: test-account"));
                assert!(headers.contains("client_version=0.159.1"));
                socket.write_all(format!("HTTP/1.1 {status} Error\r\nContent-Length: 12\r\nLocation: http://127.0.0.1:1/secret\r\nConnection: close\r\n\r\nsecret-token").as_bytes()).await.unwrap();
            });
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            let error = fetch_codex_model_list(
                &client,
                url,
                &ProviderDefinition::default(),
                "test-token",
                "test-account",
                "0.159.1",
            )
            .await
            .unwrap_err();
            assert!(
                matches!(error, ModelDiscoveryError::Status { status: actual, .. } if actual == status)
            );
            assert!(!error.to_string().contains("secret-token"));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_failed_discovery_keeps_the_status_but_not_the_response_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        for status in [401, 403, 404, 500] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 8192];
                let _ = socket.read(&mut request).await.unwrap();
                socket.write_all(format!("HTTP/1.1 {status} Error\r\nContent-Length: 12\r\nConnection: close\r\n\r\nsecret-token").as_bytes()).await.unwrap();
            });
            let mut provider = ProviderDefinition {
                id: "custom".into(),
                name: "Custom".into(),
                base_url,
                protocol: ProviderProtocol::ChatCompletions,
                allow_private_network: true,
                ..ProviderDefinition::default()
            };
            provider.discovery.enabled = true;
            let error = discover_provider_models(&provider).await.unwrap_err();
            assert!(
                matches!(error, ModelDiscoveryError::Status { status: actual, .. } if actual == status)
            );
            assert!(!error.to_string().contains("secret-token"));
            server.await.unwrap();
        }
    }

    #[test]
    fn codex_online_list_imports_visible_models_and_efforts() {
        let provider = ProviderDefinition {
            id: "openai".into(),
            ..ProviderDefinition::default()
        };
        let value = serde_json::json!({"models": [
            {"slug": "gpt-6.1-sol", "visibility": "list", "display_name": "GPT-6.1-Sol",
             "context_window": 272000, "input_modalities": ["text", "image"],
             "supported_reasoning_levels": [{"effort": "low"}, {"effort": "high"}],
             "default_reasoning_level": "low"},
            {"slug": "hidden-model", "visibility": "hide"},
            {"slug": "", "visibility": "list"}
        ]});
        let models = parse_codex_model_list(&provider, &value).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].provider_id, "openai");
        assert_eq!(models[0].model_id, "gpt-6.1-sol");
        assert_eq!(models[0].context_window, Some(272000));
        assert_eq!(models[0].input_modalities, ["text", "image"]);
        assert_eq!(models[0].reasoning_efforts, ["low", "high"]);
        assert_eq!(models[0].default_reasoning_effort.as_deref(), Some("low"));
        for invalid in [serde_json::json!({}), serde_json::json!({"models": []})] {
            assert!(parse_codex_model_list(&provider, &invalid).is_err());
        }
    }

    #[test]
    fn antigravity_claude_55_variants_have_separate_effort_metadata() {
        let mut output = String::new();
        for family in ["opus", "sonnet"] {
            for effort in ["low", "medium", "high"] {
                output.push_str(&format!(
                    "claude-{family}-5-5-{effort}\tClaude {family} 5.5 ({effort})\n"
                ));
            }
        }
        output.push_str("claude-opus-4-6-thinking\tClaude Opus 4.6 (Thinking)\n");
        output.push_str("gpt-oss-120b-medium\tGPT-OSS 120B (Medium)\n");
        let rows = parse_antigravity_cli_models(&output);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[2].0, "claude-opus-4-6-thinking");
        assert_eq!(rows[3].0, "gpt-oss-120b-medium");
        // Existing saved providers may not yet have registry effort metadata.
        let models = antigravity_models_from_cli_rows(&ProviderDefinition::default(), &rows);
        for family in ["opus", "sonnet"] {
            let model = models
                .iter()
                .find(|m| m.model_id == format!("claude-{family}-5-5"))
                .unwrap();
            assert_eq!(model.reasoning_efforts, ["low", "medium", "high"]);
            assert_eq!(model.default_reasoning_effort.as_deref(), Some("medium"));
            assert_eq!(
                model.display_name.as_deref(),
                Some(format!("Claude {family} 5.5").as_str())
            );
        }
    }

    #[test]
    fn antigravity_cli_models_collapse_effort_variants() {
        let rows = parse_antigravity_cli_models(
            "Fetching available models...\n\
gemini-3.6-flash-high\tGemini 3.6 Flash (High)\n\
gemini-3.6-flash-medium\tGemini 3.6 Flash (Medium)\n\
gemini-3.6-flash-low\tGemini 3.6 Flash (Low)\n\
claude-sonnet-4-6\tClaude Sonnet 4.6 (Thinking)\n",
        );
        assert_eq!(
            rows.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            ["gemini-3.6-flash", "claude-sonnet-4-6"]
        );

        let preserved =
            parse_antigravity_cli_models("gpt-oss-120b-medium\tGPT-OSS 120B (Medium)\n");
        assert_eq!(preserved[0].0, "gpt-oss-120b-medium");
        let mut provider = ProviderDefinition {
            id: "google-antigravity".into(),
            name: "Google Antigravity".into(),
            base_url: "https://daily-cloudcode-pa.googleapis.com".into(),
            protocol: ProviderProtocol::GeminiGenerateContent,
            ..ProviderDefinition::default()
        };
        // Image input is on by default; start from a text-only provider.
        provider.capabilities.vision = false;
        provider
            .model_context_windows
            .insert("gemini-3.6-flash".into(), 1_048_576);
        provider.model_reasoning_efforts.insert(
            "gemini-3.6-flash".into(),
            vec!["low".into(), "medium".into(), "high".into()],
        );
        let models = antigravity_models_from_cli_rows(&provider, &rows);
        let flash = models
            .iter()
            .find(|model| model.model_id == "gemini-3.6-flash")
            .unwrap();
        assert_eq!(flash.context_window, Some(1_048_576));
        assert_eq!(flash.reasoning_efforts, ["low", "medium", "high"]);
        assert_eq!(flash.input_modalities, ["text"]);

        provider.capabilities.vision = true;
        let vision = antigravity_models_from_cli_rows(&provider, &rows)
            .into_iter()
            .find(|model| model.model_id == "gemini-3.6-flash")
            .unwrap();
        assert_eq!(vision.input_modalities, ["text", "image"]);
    }
}
