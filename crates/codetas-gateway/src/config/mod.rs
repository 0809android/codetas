use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};
use url::Url;

mod credential;
mod provider;
mod types;
mod validate;

pub use credential::*;
pub use types::*;

pub const SETTINGS_VERSION: u8 = 2;


fn enabled_by_default() -> bool {
    true
}

fn default_auth_timeout_ms() -> u64 {
    5_000
}
fn default_auth_refresh_ms() -> u64 {
    300_000
}
fn default_connect_timeout_ms() -> u64 {
    15_000
}
fn default_request_timeout_ms() -> u64 {
    300_000
}
fn default_stream_idle_timeout_ms() -> u64 {
    300_000
}
fn default_request_retries() -> u8 {
    2
}
fn default_stream_retries() -> u8 {
    2
}
fn default_max_request_bytes() -> u64 {
    16 * 1024 * 1024
}
fn default_max_response_bytes() -> u64 {
    32 * 1024 * 1024
}
fn default_model_discovery_path() -> String {
    "/models".into()
}
fn default_max_models() -> usize {
    500
}
fn default_route_weight() -> u16 {
    1
}
fn default_sticky_requests() -> u32 {
    32
}
fn default_failover_threshold() -> u8 {
    3
}
fn default_gateway_host() -> String {
    "127.0.0.1".into()
}
fn default_gateway_port() -> u16 {
    42_421
}
fn default_shutdown_timeout_ms() -> u64 {
    10_000
}
fn default_log_retention_days() -> u16 {
    14
}
fn default_trash_retention_days() -> u16 {
    7
}
fn default_log_budget_bytes() -> u64 {
    256 * 1024 * 1024
}
fn default_auto_switch_threshold() -> u8 {
    90
}
fn default_agent_threads() -> u16 {
    4
}
fn default_shadow_sample_percent() -> u8 {
    10
}
fn default_shadow_timeout_ms() -> u64 {
    30_000
}
fn default_shadow_response_bytes() -> u64 {
    1024 * 1024
}
fn default_helper_source_models() -> Vec<String> {
    vec!["gpt-5.4-mini".into(), "gpt-5.6-luna".into()]
}


pub fn parse_gateway_settings_json(content: &[u8]) -> Result<(GatewaySettings, bool), String> {
    let mut raw: serde_json::Value = serde_json::from_slice(content)
        .map_err(|error| format!("settings JSON is invalid: {error}"))?;
    let version = raw
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1);
    let migrated = version == 1;
    if migrated {
        raw["version"] = serde_json::json!(SETTINGS_VERSION);
    }
    let settings: GatewaySettings = serde_json::from_value(raw)
        .map_err(|error| format!("settings cannot be decoded: {error}"))?;
    settings.validate()?;
    Ok((settings, migrated))
}

#[derive(Clone, Copy)]
enum SidecarCapability {
    WebSearch,
    Vision,
    ImageGeneration,
    VideoGeneration,
    Realtime,
}

fn validate_routing_reference(
    target: &str,
    provider_ids: &HashSet<&str>,
    route_ids: &HashSet<&str>,
) -> Result<(), String> {
    validate_single_line("routing reference", target, 300)?;
    if route_ids.contains(target) {
        return Ok(());
    }
    let (provider_id, model_id) = target
        .split_once('/')
        .ok_or_else(|| format!("routing reference must use a route or provider/model: {target}"))?;
    if !provider_ids.contains(provider_id) {
        return Err(format!(
            "routing reference uses an unknown provider: {provider_id}"
        ));
    }
    validate_model_id(model_id)
}

fn routing_reference_supports(
    settings: &GatewaySettings,
    target: &str,
    required: SidecarCapability,
) -> bool {
    if let Some(route) = settings.routes.iter().find(|route| {
        route.enabled && (route.id == target || route.alias.as_deref() == Some(target))
    }) {
        return !route.targets.is_empty()
            && route.targets.iter().all(|route_target| {
                route_target
                    .model
                    .split_once('/')
                    .and_then(|(provider_id, _)| {
                        settings
                            .providers
                            .iter()
                            .find(|provider| provider.enabled && provider.id == provider_id)
                    })
                    .is_some_and(|provider| {
                        let model = route_target.model.split_once('/').map(|(_, model)| model);
                        provider_supports(provider, model, required)
                    })
            });
    }
    target
        .split_once('/')
        .and_then(|(provider_id, _)| {
            settings
                .providers
                .iter()
                .find(|provider| provider.enabled && provider.id == provider_id)
        })
        .is_some_and(|provider| {
            let model = target.split_once('/').map(|(_, model)| model);
            provider_supports(provider, model, required)
        })
}

fn provider_supports(
    provider: &ProviderDefinition,
    model: Option<&str>,
    required: SidecarCapability,
) -> bool {
    match required {
        SidecarCapability::WebSearch => {
            provider.capabilities.web_search
                && provider.transport == ProviderTransport::Standard
                && model.is_some_and(|model| {
                    provider.protocol_for_model(model) == ProviderProtocol::Responses
                })
        }
        SidecarCapability::Vision => provider.capabilities.vision,
        SidecarCapability::ImageGeneration => provider.capabilities.image_generation,
        SidecarCapability::VideoGeneration => provider.capabilities.video_generation,
        SidecarCapability::Realtime => provider.capabilities.realtime,
    }
}

fn is_reasoning_effort(value: &str) -> bool {
    matches!(
        value,
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
    )
}

fn validate_cors_origin(origin: &str) -> Result<(), String> {
    validate_single_line("CORS origin", origin, 2_048)?;
    if origin == "*" || origin == "null" {
        return Err("CORS origins must be explicit http or https origins".into());
    }
    let url = Url::parse(origin).map_err(|_| "CORS origin must be a valid URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(
            "CORS origin must contain only an http(s) scheme, host, and optional port".into(),
        );
    }
    Ok(())
}

pub fn validate_provider_id(value: &str) -> Result<(), String> {
    let id = value.trim();
    if id.is_empty() || id.len() > 64 {
        return Err("provider id must contain 1-64 characters".into());
    }
    let mut chars = id.chars();
    let first = chars.next().ok_or("provider id is required")?;
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err("provider id must start with a lowercase letter or number".into());
    }
    if !chars.all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '-' | '_')
    }) {
        return Err(
            "provider id may use lowercase letters, numbers, hyphens, and underscores".into(),
        );
    }
    Ok(())
}

fn validate_env_key(value: &str) -> Result<(), String> {
    let mut chars = value.chars();
    let first = chars.next().ok_or("apiKeyEnv cannot be empty")?;
    if !first.is_ascii_uppercase() && first != '_' {
        return Err("apiKeyEnv must start with an uppercase letter or underscore".into());
    }
    if !chars.all(|character| {
        character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_'
    }) {
        return Err("apiKeyEnv may use uppercase letters, numbers, and underscores".into());
    }
    Ok(())
}

fn validate_header_name(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 256
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
    {
        return Err("invalid HTTP header name".into());
    }
    Ok(())
}

fn is_sensitive_name(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase().replace('-', "").replace('_', "");
    normalized.contains("authorization")
        || normalized.contains("apikey")
        || normalized.contains("token")
        || normalized.contains("secret")
        || normalized.contains("password")
        || normalized == "key"
}

fn is_forbidden_transport_header(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    matches!(
        value.as_str(),
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "upgrade"
            | "te"
            | "trailer"
            | "proxy-authorization"
            | "proxy-authenticate"
    ) || value.starts_with("sec-websocket-")
}

fn validate_model_id(value: &str) -> Result<(), String> {
    validate_single_line("model id", value, 240)?;
    if value.trim().is_empty() {
        return Err("model id cannot be empty".into());
    }
    Ok(())
}

fn validate_single_line(field: &str, value: &str, max: usize) -> Result<(), String> {
    if value.len() > max {
        return Err(format!("{field} is too long"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{field} must be a single printable line"));
    }
    Ok(())
}

fn is_private_destination(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return true;
    };
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        return true;
    }
    host.parse::<IpAddr>().map(is_private_ip).unwrap_or(false)
}

pub(crate) fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(value) => {
            value.is_private()
                || value.is_loopback()
                || value.is_link_local()
                || value.is_multicast()
                || value.is_documentation()
                || value.is_broadcast()
                || value.is_unspecified()
                || value.octets()[0] == 0
                || matches!(value.octets(), [100, second, _, _] if (64..=127).contains(&second))
                || matches!(value.octets(), [198, second, _, _] if matches!(second, 18 | 19))
                || value.octets()[0] >= 240
                || value == Ipv4Addr::new(169, 254, 169, 254)
        }
        IpAddr::V6(value) => {
            value.is_loopback()
                || value.is_unspecified()
                || value.is_unique_local()
                || value.is_unicast_link_local()
                || value.is_multicast()
                || matches!(value.segments(), [0x2001, 0x0db8, _, _, _, _, _, _])
                || value
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| is_private_ip(IpAddr::V4(mapped)))
                || value == Ipv6Addr::LOCALHOST
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn provider(base_url: &str) -> ProviderDefinition {
        ProviderDefinition {
            id: "test-provider".into(),
            name: "Test provider".into(),
            base_url: base_url.into(),
            protocol: ProviderProtocol::ChatCompletions,
            api_key_env: Some("TEST_PROVIDER_API_KEY".into()),
            default_model: Some("test-model".into()),
            models: vec!["test-model".into()],
            enabled: true,
            allow_private_network: false,
            ..ProviderDefinition::default()
        }
    }

    #[test]
    fn creates_protocol_endpoint() {
        let provider = provider("https://models.example/v1");
        assert_eq!(
            provider.endpoint(),
            "https://models.example/v1/chat/completions"
        );
        assert!(provider.validate().is_ok());
    }

    #[test]
    fn private_destinations_require_opt_in() {
        let mut provider = provider("http://127.0.0.1:11434/v1");
        assert!(provider.validate().is_err());
        provider.allow_private_network = true;
        assert!(provider.validate().is_ok());
    }

    #[test]
    fn rejects_credentials_in_url() {
        let provider = provider("https://user:secret@models.example/v1");
        assert!(provider.validate().is_err());
    }

    #[test]
    fn rejects_persisted_secret_headers() {
        let mut provider = provider("https://models.example/v1");
        provider
            .headers
            .insert("Authorization".into(), "Bearer secret".into());
        assert!(provider.validate().is_err());
        provider.headers.clear();
        provider
            .env_headers
            .insert("Authorization".into(), "TEST_PROVIDER_API_KEY".into());
        assert!(provider.validate().is_ok());
    }

    #[test]
    fn validates_virtual_routes() {
        let provider = provider("https://models.example/v1");
        let settings = GatewaySettings {
            default_provider: Some(provider.id.clone()),
            providers: vec![provider],
            routes: vec![RouteDefinition {
                id: "reliable".into(),
                name: "Reliable route".into(),
                strategy: RouteStrategy::Failover,
                targets: vec![RouteTarget {
                    model: "test-provider/test-model".into(),
                    weight: 1,
                }],
                sticky_requests: 32,
                failure_threshold: 3,
                enabled: true,
                ..RouteDefinition::default()
            }],
            ..GatewaySettings::default()
        };
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn creates_native_adapter_endpoints() {
        let mut provider = provider("https://api.anthropic.com/v1");
        provider.protocol = ProviderProtocol::AnthropicMessages;
        assert_eq!(
            provider.endpoint_for_model("claude-model"),
            "https://api.anthropic.com/v1/messages"
        );

        provider.base_url = "https://generativelanguage.googleapis.com/v1beta".into();
        provider.protocol = ProviderProtocol::GeminiGenerateContent;
        assert_eq!(
            provider.endpoint_for_model("gemini-model"),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-model:generateContent"
        );
    }

    #[test]
    fn migrates_v1_settings_through_v2_defaults() {
        let (settings, migrated) =
            parse_gateway_settings_json(br#"{"version":1,"defaultProvider":null,"providers":[]}"#)
                .unwrap();
        assert!(migrated);
        assert_eq!(settings.version, SETTINGS_VERSION);
        assert!(settings.shadows.is_empty());
        assert!(settings.security.external_access_keys.is_empty());
        assert_eq!(settings.updates.channel, UpdateChannel::Stable);
    }
}

