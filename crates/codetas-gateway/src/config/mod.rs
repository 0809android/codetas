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

pub(crate) use credential::*;
pub use provider::effective_model_capabilities;
pub use types::*;

pub const SETTINGS_VERSION: u8 = 2;
pub const REGISTRY_REVISION: u32 = 8;

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
fn default_429_retries() -> u8 {
    2
}
fn default_empty_completion_retries() -> u8 {
    1
}
fn default_policy_weight() -> u16 {
    100
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
fn default_memory_budget_bytes() -> u64 {
    512 * 1024 * 1024
}
fn default_max_inflight_requests() -> u32 {
    64
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
fn default_auxiliary_timeout_ms() -> u64 {
    120_000
}
fn default_video_sample_frames() -> u16 {
    8
}
fn default_document_max_pages() -> u16 {
    12
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
    import_hermes_auxiliary_aliases(&mut raw);
    let deepseek_id_repair_migrated = import_deepseek_response_id_repair_defaults(&mut raw);
    let mut settings: GatewaySettings = serde_json::from_value(raw)
        .map_err(|error| format!("settings cannot be decoded: {error}"))?;
    let registry_migrated = crate::registry::backfill_registry_input_limits(&mut settings);
    settings.validate()?;
    Ok((
        settings,
        migrated || registry_migrated || deepseek_id_repair_migrated,
    ))
}

fn import_deepseek_response_id_repair_defaults(raw: &mut serde_json::Value) -> bool {
    let Some(providers) = raw.get_mut("providers").and_then(serde_json::Value::as_array_mut)
    else {
        return false;
    };
    let mut migrated = false;
    for provider in providers {
        if provider.get("id").and_then(serde_json::Value::as_str) != Some("deepseek") {
            continue;
        }
        let Some(provider) = provider.as_object_mut() else {
            continue;
        };
        if !provider.contains_key("repairInvalidResponseItemIds") {
            provider.insert(
                "repairInvalidResponseItemIds".into(),
                serde_json::Value::Bool(true),
            );
            migrated = true;
        }
        let repair = provider
            .entry("responseItemIdRepair")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(repair) = repair.as_object_mut() {
            if !repair.contains_key("repairMissingTerminalIds") {
                repair.insert(
                    "repairMissingTerminalIds".into(),
                    serde_json::Value::Bool(true),
                );
                migrated = true;
            }
        }
    }
    migrated
}

fn import_hermes_auxiliary_aliases(raw: &mut serde_json::Value) {
    let Some(root) = raw.as_object_mut() else { return };
    let image_mode = root
        .get("agent")
        .and_then(serde_json::Value::as_object)
        .and_then(|agent| agent.get("image_input_mode"))
        .cloned();
    let agents = root
        .entry("agents")
        .or_insert_with(|| serde_json::json!({}));
    if let (Some(image_mode), Some(agents)) = (image_mode, agents.as_object_mut()) {
        agents.entry("imageInputMode").or_insert(image_mode);
    }

    let auxiliary = root
        .get("auxiliary")
        .and_then(serde_json::Value::as_object)
        .cloned();
    let Some(auxiliary) = auxiliary else { return };
    let sidecars = root
        .entry("sidecars")
        .or_insert_with(|| serde_json::json!({}));
    let Some(sidecars) = sidecars.as_object_mut() else { return };
    for (task, destination) in [
        ("vision", "visionModel"),
        ("video", "videoInputModel"),
        ("document", "documentModel"),
        ("image_generation", "imageModel"),
        ("video_generation", "videoModel"),
    ] {
        if sidecars.contains_key(destination) {
            continue;
        }
        let Some(task) = auxiliary.get(task).and_then(serde_json::Value::as_object) else {
            continue;
        };
        let provider = task
            .get("provider")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let model = task
            .get("model")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if model.is_empty() {
            continue;
        }
        let target = if model.contains('/') || provider.is_empty() || provider == "auto" {
            model.to_string()
        } else {
            format!("{provider}/{model}")
        };
        sidecars.insert(destination.into(), serde_json::Value::String(target));
    }
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
                        provider_supports(settings, provider, model, required)
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
            provider_supports(settings, provider, model, required)
        })
}

fn provider_supports(
    settings: &GatewaySettings,
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
        SidecarCapability::Vision => model
            .and_then(|model| provider.model_input_modalities.get(model))
            .map(|modalities| modalities.iter().any(|modality| modality == "image"))
            .unwrap_or(provider.capabilities.vision),
        SidecarCapability::ImageGeneration => model
            .is_some_and(|model| image_model_is_available(settings, provider, model)),
        SidecarCapability::VideoGeneration => provider.capabilities.video_generation,
        SidecarCapability::Realtime => provider.capabilities.realtime,
    }
}

/// Return whether a concrete provider/model can be sent to an OpenAI-compatible
/// image API. Provider-wide capability is only the upper bound: the concrete
/// alias or wire model must also be declared in `image_generation_models` or
/// carry an explicit model-level image capability.
pub(crate) fn image_model_is_available(
    settings: &GatewaySettings,
    provider: &ProviderDefinition,
    model: &str,
) -> bool {
    if !provider.enabled
        || provider.transport != ProviderTransport::Standard
        || !provider.capabilities.image_generation
    {
        return false;
    }
    if !model_has_image_generation_identity(settings, provider, model) {
        return false;
    }
    let catalog_entry = |model_id: &str| {
        settings.model_catalog.iter().find(|metadata| {
            metadata.provider_id == provider.id && metadata.model_id == model_id
        })
    };
    let alias_metadata = catalog_entry(model);
    if alias_metadata.is_some_and(|metadata| {
        !metadata.enabled
            || !effective_model_capabilities(provider, Some(metadata), model).image_generation
    })
    {
        return false;
    }

    let wire_model = canonical_wire_model_id(provider, model);
    let wire_metadata = catalog_entry(&wire_model);
    if wire_metadata.is_some_and(|metadata| {
        !metadata.enabled
            || !effective_model_capabilities(provider, Some(metadata), &wire_model)
                .image_generation
    })
    {
        return false;
    }

    true
}

/// Return image-only identity independently from current provider/model
/// availability. Normal chat/catalog routing must use this predicate so an
/// unavailable image model cannot leak back into text surfaces.
pub(crate) fn model_has_image_generation_identity(
    settings: &GatewaySettings,
    provider: &ProviderDefinition,
    model: &str,
) -> bool {
    let canonical = canonical_wire_model_id(provider, model);
    let explicit_list = provider.image_generation_models.iter().any(|configured| {
        canonical_wire_model_id(provider, configured) == canonical
    });
    let explicit_metadata = settings.model_catalog.iter().any(|metadata| {
        metadata.provider_id == provider.id
            && metadata.capabilities.image_generation
            && canonical_wire_model_id(provider, &metadata.model_id) == canonical
    });
    if explicit_list || explicit_metadata {
        return true;
    }
    let provider_has_explicit_identity = !provider.image_generation_models.is_empty()
        || settings.model_catalog.iter().any(|metadata| {
            metadata.provider_id == provider.id && metadata.capabilities.image_generation
        });
    !provider_has_explicit_identity
        && [model, canonical.as_str()]
            .iter()
            .any(|candidate| legacy_image_model_name(candidate))
}

fn canonical_wire_model_id(provider: &ProviderDefinition, model: &str) -> String {
    let mut current = model.to_string();
    for _ in 0..=provider.model_wire_ids.len() {
        let next = provider.wire_model_id(&current);
        if next == current {
            break;
        }
        current = next;
    }
    current
}

fn legacy_image_model_name(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    [
        "gpt-image",
        "imagegen",
        "image-generation",
        "imagen",
        "dall-e",
        "flux",
        "stable-diffusion",
    ]
    .iter()
    .any(|term| model.contains(term))
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

fn validate_managed_client_id(value: &str) -> Result<(), String> {
    const MAX_MANAGED_CLIENT_ID_BYTES: usize = 80;
    if value.is_empty() || value.len() > MAX_MANAGED_CLIENT_ID_BYTES {
        return Err("managed client id must contain between 1 and 80 bytes".into());
    }
    if !value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
        || !value
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric())
    {
        return Err("managed client id contains unsupported characters".into());
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
    fn managed_client_ids_accept_all_owned_artifact_keys() {
        for client in [
            "claudeCode",
            "claudeDesktop",
            "claudeDesktopInference",
            "opencode",
            "grok",
            "pi",
            "hermes",
        ] {
            assert!(validate_managed_client_id(client).is_ok(), "{client}");
            let mut settings = GatewaySettings::default();
            settings
                .integrations
                .managed_clients
                .insert(client.into(), ManagedClientSettings::default());
            assert!(settings.validate().is_ok(), "{client}");
        }
    }

    #[test]
    fn managed_client_ids_reject_controls_paths_and_oversized_values() {
        for client in ["", "claude\nCode", "../claudeCode", "claude/Code"] {
            assert!(validate_managed_client_id(client).is_err(), "{client:?}");
        }
        assert!(validate_managed_client_id(&"a".repeat(81)).is_err());
    }

    #[test]
    fn omitted_advanced_capabilities_default_to_false_for_custom_providers() {
        let capabilities: ProviderCapabilities =
            serde_json::from_value(serde_json::json!({"tools": true}))
                .expect("capabilities");
        assert!(!capabilities.structured_output);
        assert!(!capabilities.custom_tools);
        assert!(!capabilities.tool_search);
        assert!(!capabilities.mcp_namespaces);
    }

    #[test]
    fn tool_subcapabilities_require_the_base_tools_capability() {
        let configurations: [fn(&mut ProviderCapabilities); 4] = [
            |capabilities: &mut ProviderCapabilities| capabilities.parallel_tools = true,
            |capabilities: &mut ProviderCapabilities| capabilities.custom_tools = true,
            |capabilities: &mut ProviderCapabilities| capabilities.tool_search = true,
            |capabilities: &mut ProviderCapabilities| capabilities.mcp_namespaces = true,
        ];
        for configure in configurations {
            let mut provider = provider("https://models.example/v1");
            provider.capabilities.tools = false;
            configure(&mut provider.capabilities);
            assert!(provider.validate().is_err());
        }
    }

    #[test]
    fn model_tool_subcapabilities_require_the_base_tools_capability() {
        let mut settings = GatewaySettings::default();
        let provider = provider("https://models.example/v1");
        let provider_id = provider.id.clone();
        settings.providers = vec![provider];
        settings.default_provider = Some(provider_id.clone());
        let mut metadata = ModelMetadata {
            provider_id,
            model_id: "fixture-model".into(),
            ..ModelMetadata::default()
        };
        metadata.capabilities.tools = false;
        metadata.capabilities.custom_tools = true;
        settings.model_catalog = vec![metadata];

        assert!(settings.validate().is_err());
    }

    #[test]
    fn effective_model_capabilities_keep_explicit_metadata_and_provider_exceptions() {
        let mut provider = provider("https://models.example/v1");
        provider.capabilities.structured_output = true;
        provider.capabilities.service_tier = false;
        provider.no_structured_output_models = vec!["legacy".into()];
        provider.service_tier_models = vec!["tiered".into()];
        let mut metadata = ModelMetadata {
            provider_id: provider.id.clone(),
            model_id: "explicit".into(),
            ..ModelMetadata::default()
        };
        metadata.capabilities.structured_output = false;

        assert!(!effective_model_capabilities(&provider, Some(&metadata), "explicit")
            .structured_output);
        metadata.capabilities.structured_output = true;
        assert!(!effective_model_capabilities(&provider, Some(&metadata), "legacy")
            .structured_output);
        assert!(effective_model_capabilities(&provider, Some(&metadata), "tiered")
            .service_tier);
    }

    fn image_alias_provider() -> ProviderDefinition {
        let mut provider = ProviderDefinition {
            id: "openai".into(),
            name: "OpenAI".into(),
            base_url: "https://api.openai.com/v1".into(),
            capabilities: ProviderCapabilities {
                image_generation: true,
                ..ProviderCapabilities::default()
            },
            image_generation_models: vec!["imagegen-2".into(), "gpt-image-2".into()],
            ..ProviderDefinition::default()
        };
        provider
            .model_wire_ids
            .insert("imagegen-2".into(), "gpt-image-2".into());
        provider
    }

    fn image_metadata(model_id: &str, enabled: bool, image_generation: bool) -> ModelMetadata {
        ModelMetadata {
            provider_id: "openai".into(),
            model_id: model_id.into(),
            enabled,
            capabilities: ProviderCapabilities {
                image_generation,
                ..ProviderCapabilities::default()
            },
            ..ModelMetadata::default()
        }
    }

    #[test]
    fn image_alias_is_rejected_when_canonical_catalog_model_is_disabled() {
        let provider = image_alias_provider();
        let settings = GatewaySettings {
            model_catalog: vec![image_metadata("gpt-image-2", false, true)],
            ..GatewaySettings::default()
        };

        assert!(!image_model_is_available(
            &settings,
            &provider,
            "imagegen-2"
        ));
        assert!(model_has_image_generation_identity(
            &settings,
            &provider,
            "imagegen-2"
        ));
    }

    #[test]
    fn image_alias_is_rejected_when_canonical_capability_is_false() {
        let provider = image_alias_provider();
        let settings = GatewaySettings {
            model_catalog: vec![image_metadata("gpt-image-2", true, false)],
            ..GatewaySettings::default()
        };

        assert!(!image_model_is_available(
            &settings,
            &provider,
            "imagegen-2"
        ));
    }

    #[test]
    fn image_alias_is_allowed_when_canonical_catalog_model_is_enabled() {
        let provider = image_alias_provider();
        let settings = GatewaySettings {
            model_catalog: vec![image_metadata("gpt-image-2", true, true)],
            ..GatewaySettings::default()
        };

        assert!(image_model_is_available(
            &settings,
            &provider,
            "imagegen-2"
        ));
    }

    #[test]
    fn explicitly_disabled_image_alias_is_rejected() {
        let provider = image_alias_provider();
        let settings = GatewaySettings {
            model_catalog: vec![
                image_metadata("imagegen-2", false, true),
                image_metadata("gpt-image-2", true, true),
            ],
            ..GatewaySettings::default()
        };

        assert!(!image_model_is_available(
            &settings,
            &provider,
            "imagegen-2"
        ));
    }

    #[test]
    fn image_alias_with_explicitly_false_capability_is_rejected() {
        let provider = image_alias_provider();
        let settings = GatewaySettings {
            model_catalog: vec![
                image_metadata("imagegen-2", true, false),
                image_metadata("gpt-image-2", true, true),
            ],
            ..GatewaySettings::default()
        };

        assert!(!image_model_is_available(
            &settings,
            &provider,
            "imagegen-2"
        ));
    }

    #[test]
    fn provider_wide_image_capability_does_not_promote_chat_models() {
        let mut provider = image_alias_provider();
        provider.models.push("gpt-5.5".into());

        assert!(!image_model_is_available(
            &GatewaySettings::default(),
            &provider,
            "gpt-5.5"
        ));
    }

    #[test]
    fn image_identity_is_independent_from_provider_availability() {
        let mut provider = image_alias_provider();
        provider.capabilities.image_generation = false;

        assert!(model_has_image_generation_identity(
            &GatewaySettings::default(),
            &provider,
            "imagegen-2"
        ));
        assert!(!image_model_is_available(
            &GatewaySettings::default(),
            &provider,
            "imagegen-2"
        ));
    }

    #[test]
    fn disabled_model_metadata_still_defines_image_identity() {
        let mut provider = image_alias_provider();
        provider.image_generation_models.clear();
        provider.model_wire_ids.clear();
        let settings = GatewaySettings {
            model_catalog: vec![image_metadata("render-v3", false, true)],
            ..GatewaySettings::default()
        };

        assert!(model_has_image_generation_identity(
            &settings,
            &provider,
            "render-v3"
        ));
        assert!(!image_model_is_available(&settings, &provider, "render-v3"));
    }

    #[test]
    fn legacy_image_identity_uses_name_policy_only_without_explicit_contract() {
        let mut provider = image_alias_provider();
        provider.image_generation_models.clear();
        provider.model_wire_ids.clear();

        assert!(model_has_image_generation_identity(
            &GatewaySettings::default(),
            &provider,
            "legacy-imagegen-v1"
        ));
        assert!(!model_has_image_generation_identity(
            &GatewaySettings::default(),
            &provider,
            "gpt-5.5"
        ));
    }

    #[test]
    fn arbitrary_explicit_image_model_and_wire_alias_are_available() {
        let mut provider = image_alias_provider();
        provider.image_generation_models = vec!["studio-art".into()];
        provider
            .model_wire_ids
            .insert("studio-art".into(), "art-v2".into());

        assert!(image_model_is_available(
            &GatewaySettings::default(),
            &provider,
            "art-v2"
        ));
        assert!(image_model_is_available(
            &GatewaySettings::default(),
            &provider,
            "studio-art"
        ));
    }

    #[test]
    fn explicit_model_level_image_capability_is_sufficient() {
        let mut provider = image_alias_provider();
        provider.image_generation_models.clear();
        provider.model_wire_ids.clear();
        let settings = GatewaySettings {
            model_catalog: vec![image_metadata("render-v3", true, true)],
            ..GatewaySettings::default()
        };

        assert!(image_model_is_available(&settings, &provider, "render-v3"));
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
        assert!(settings.catalog.selected_models.is_empty());
        assert!(settings.catalog.model_picker_order.is_empty());
        assert!(!settings.catalog.compatibility_lab);
        assert_eq!(settings.runtime.memory_budget_bytes, default_memory_budget_bytes());
        assert_eq!(settings.runtime.max_inflight_requests, default_max_inflight_requests());
    }

    #[test]
    fn compatibility_lab_is_opt_in_and_preserves_explicit_values() {
        let (missing, _) = parse_gateway_settings_json(
            br#"{"version":2,"defaultProvider":null,"providers":[],"catalog":{}}"#,
        )
        .expect("settings without compatibility lab flag");
        assert!(!missing.catalog.compatibility_lab);

        for expected in [false, true] {
            let content = format!(
                "{{\"version\":2,\"defaultProvider\":null,\"providers\":[],\"catalog\":{{\"compatibilityLab\":{expected}}}}}"
            );
            let (settings, _) = parse_gateway_settings_json(content.as_bytes())
                .expect("settings with explicit compatibility lab flag");
            assert_eq!(settings.catalog.compatibility_lab, expected);
        }
    }

    #[test]
    fn deepseek_id_repair_migration_opts_in_missing_fields_but_preserves_false() {
        let deepseek = crate::registry::provider_presets()
            .into_iter()
            .find(|preset| preset.id == "deepseek")
            .unwrap()
            .instantiate(None)
            .unwrap();
        let mut missing = serde_json::to_value(GatewaySettings {
            providers: vec![deepseek.clone()],
            ..GatewaySettings::default()
        })
        .unwrap();
        let provider = missing["providers"][0].as_object_mut().unwrap();
        provider.remove("repairInvalidResponseItemIds");
        provider
            .get_mut("responseItemIdRepair")
            .and_then(serde_json::Value::as_object_mut)
            .unwrap()
            .remove("repairMissingTerminalIds");

        let (settings, migrated) =
            parse_gateway_settings_json(&serde_json::to_vec(&missing).unwrap()).unwrap();
        assert!(migrated);
        assert!(settings.providers[0].repair_invalid_response_item_ids);
        assert!(settings.providers[0]
            .response_item_id_repair
            .repair_missing_terminal_ids);

        let mut opted_out = serde_json::to_value(GatewaySettings {
            providers: vec![deepseek],
            ..GatewaySettings::default()
        })
        .unwrap();
        opted_out["providers"][0]["repairInvalidResponseItemIds"] = serde_json::json!(false);
        opted_out["providers"][0]["responseItemIdRepair"]["repairMissingTerminalIds"] =
            serde_json::json!(false);
        let (settings, _) =
            parse_gateway_settings_json(&serde_json::to_vec(&opted_out).unwrap()).unwrap();
        assert!(!settings.providers[0].repair_invalid_response_item_ids);
        assert!(!settings.providers[0]
            .response_item_id_repair
            .repair_missing_terminal_ids);
    }

    #[test]
    fn local_compaction_settings_default_to_v2_and_accept_v1_rollback() {
        let defaulted = parse_gateway_settings_json(
            br#"{"version":2,"providers":[],"defaultProvider":null}"#,
        )
        .unwrap()
        .0;
        assert!(defaulted.local_compaction.generate_v2());
        assert_eq!(defaulted.local_compaction.tail_token_limit(), 20_000);

        let rolled_back = parse_gateway_settings_json(
            br#"{"version":2,"providers":[],"defaultProvider":null,"localCompaction":{"envelope":"v1","tailTokenLimit":12000}}"#,
        )
        .unwrap()
        .0;
        assert!(!rolled_back.local_compaction.generate_v2());
        assert_eq!(rolled_back.local_compaction.tail_token_limit(), 12_000);
    }

    fn migrates_missing_registry_input_limits_in_v2_settings() {
        let mut provider = crate::registry::provider_presets()
            .into_iter()
            .find(|preset| preset.id == "openai")
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.model_max_input_tokens.clear();
        let content = serde_json::to_vec(&GatewaySettings {
            providers: vec![provider],
            ..GatewaySettings::default()
        })
        .unwrap();

        let (settings, migrated) = parse_gateway_settings_json(&content).unwrap();

        assert!(migrated);
        assert_eq!(
            settings.providers[0]
                .model_max_input_tokens
                .get("gpt-5.6-sol")
                .copied(),
            Some(272_000)
        );
    }

    #[test]
    fn rejects_future_registry_revisions() {
        let mut settings = GatewaySettings::default();
        settings.registry_revision = REGISTRY_REVISION + 1;
        assert!(settings.validate().is_err());
    }
}
