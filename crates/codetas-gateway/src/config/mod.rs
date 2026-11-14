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
pub(crate) use provider::model_rejects_images;
pub use types::*;

pub const SETTINGS_VERSION: u8 = 2;
pub const REGISTRY_REVISION: u32 = 29;

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
    let mut unmigrated = settings.clone();
    let registry_migrated = crate::registry::backfill_registry_input_limits(&mut settings);
    let catalog_sanitized = settings.catalog.sanitize_model_lists();
    if let Err(error) = settings.validate() {
        // A failed load takes down every provider, so a backfill must not be able to cause
        // one. The settings stay at their old revision, and the next load tries again.
        let unmigrated_sanitized = unmigrated.catalog.sanitize_model_lists();
        if unmigrated.validate().is_err() {
            return Err(error);
        }
        // The desktop app loads settings often, so say this once per process.
        static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            crate::debug::log_always(&format!(
                "kept settings at registry revision {}: the backfill would make them invalid: {error}",
                unmigrated.registry_revision
            ));
        }
        return Ok((
            unmigrated,
            migrated || deepseek_id_repair_migrated || unmigrated_sanitized,
        ));
    }
    Ok((
        settings,
        migrated || registry_migrated || deepseek_id_repair_migrated || catalog_sanitized,
    ))
}

fn import_deepseek_response_id_repair_defaults(raw: &mut serde_json::Value) -> bool {
    let Some(providers) = raw
        .get_mut("providers")
        .and_then(serde_json::Value::as_array_mut)
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
    let Some(root) = raw.as_object_mut() else {
        return;
    };
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
    let Some(sidecars) = sidecars.as_object_mut() else {
        return;
    };
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
        SidecarCapability::ImageGeneration => {
            model.is_some_and(|model| image_model_is_available(settings, provider, model))
        }
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
        settings
            .model_catalog
            .iter()
            .find(|metadata| metadata.provider_id == provider.id && metadata.model_id == model_id)
    };
    let alias_metadata = catalog_entry(model);
    if alias_metadata.is_some_and(|metadata| {
        !metadata.enabled
            || !effective_model_capabilities(provider, Some(metadata), model).image_generation
    }) {
        return false;
    }

    let wire_model = canonical_wire_model_id(provider, model);
    let wire_metadata = catalog_entry(&wire_model);
    if wire_metadata.is_some_and(|metadata| {
        !metadata.enabled
            || !effective_model_capabilities(provider, Some(metadata), &wire_model).image_generation
    }) {
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
    let explicit_list = provider
        .image_generation_models
        .iter()
        .any(|configured| canonical_wire_model_id(provider, configured) == canonical);
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

pub(crate) fn is_reasoning_effort(value: &str) -> bool {
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

/// Validates an id and names the offending entry in the error, so a bad id in a
/// hand-edited settings file is easy to find ("route id \"a.b\": ...").
pub(crate) fn validate_labeled_id(kind: &str, value: &str) -> Result<(), String> {
    validate_provider_id(value).map_err(|error| format!("{kind} id {value:?}: {error}"))
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
    let normalized = value.to_ascii_lowercase().replace(['-', '_'], "");
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
            let segments = value.segments();
            value.is_loopback()
                || value.is_unspecified()
                || value.is_unique_local()
                || value.is_unicast_link_local()
                || value.is_multicast()
                // Documentation 2001:db8::/32.
                || matches!(segments, [0x2001, 0x0db8, ..])
                // Benchmarking 2001:2::/48.
                || matches!(segments, [0x2001, 0x0002, 0x0000, ..])
                // Teredo 2001::/32 tunnels to an endpoint this check cannot see.
                || matches!(segments, [0x2001, 0x0000, ..])
                // Local-use NAT64 64:ff9b:1::/48 translates into a local network.
                || matches!(segments, [0x0064, 0xff9b, 0x0001, ..])
                // Discard-only 100::/64.
                || matches!(segments, [0x0100, 0x0000, 0x0000, 0x0000, ..])
                // Deprecated site-local fec0::/10.
                || (segments[0] & 0xffc0) == 0xfec0
                || value
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| is_private_ip(IpAddr::V4(mapped)))
                || ipv4_embedded_in_ipv6(value)
                    .is_some_and(|embedded| is_private_ip(IpAddr::V4(embedded)))
                || value == Ipv6Addr::LOCALHOST
        }
    }
}

/// The IPv4 address an IPv6 address reaches through translation or tunneling:
/// well-known NAT64 (64:ff9b::/96), 6to4 (2002::/16), and the deprecated
/// IPv4-compatible form (::/96). DNS64 hands out NAT64 addresses for public IPv4
/// hosts on IPv6-only networks, so these are judged by the IPv4 they carry, not
/// rejected outright.
fn ipv4_embedded_in_ipv6(value: Ipv6Addr) -> Option<Ipv4Addr> {
    let ipv4 = |high: u16, low: u16| {
        let [a, b] = high.to_be_bytes();
        let [c, d] = low.to_be_bytes();
        Ipv4Addr::new(a, b, c, d)
    };
    match value.segments() {
        [0x0064, 0xff9b, 0, 0, 0, 0, high, low] => Some(ipv4(high, low)),
        [0x2002, high, low, ..] => Some(ipv4(high, low)),
        [0, 0, 0, 0, 0, 0, high, low] => Some(ipv4(high, low)),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
