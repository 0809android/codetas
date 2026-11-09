use crate::config::{
    CredentialSource, CredentialTransport, GatewaySettings, ModelDiscoverySettings,
    ProviderCapabilities, ProviderCredential, ProviderDefinition, ProviderProtocol,
    REGISTRY_REVISION,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

mod vendors_a;
mod vendors_b;

const ANTIGRAVITY_MODEL_MIGRATION_REVISION: u32 = 1;
const STRICT_RESPONSES_TOOL_ADJACENCY_REVISION: u32 = 2;
const CONFORMANCE_POLICY_REVISION: u32 = 3;
const SAFE_CAPABILITY_DEFAULTS_REVISION: u32 = 4;
const IMAGE_MODEL_ISOLATION_REVISION: u32 = 6;
const MODEL_CAPABILITY_ISOLATION_REVISION: u32 = 7;
const GPT6_ASTRA_MODEL_REVISION: u32 = 10;
const OPENAI_REQUEST_BUDGET_REVISION: u32 = 11;
const DEEPSEEK_V41_FLASH_MODEL_REVISION: u32 = 12;
const DEFAULT_VISION_CAPABILITY_REVISION: u32 = 13;
const SEPTEMBER_MODEL_REFRESH_REVISION: u32 = 14;
const RETIRED_ANTHROPIC_MYTHOS_REVISION: u32 = 15;
const ANTHROPIC_FABLE_OPUS_CONTEXT_REVISION: u32 = 16;
// Revision 17 shipped without adding the model to `catalog.selected_models`,
// so Codex never listed it. The migration only fills gaps, so rerun it at 18.
const ANTHROPIC_SONNET_55_REVISION: u32 = 18;
// `gemini-4-argon` is the newest Antigravity Gemini family. Backfill the
// picker/publication entry once; never retune the user's default model.
// Revisions 20-23 mapped it to wire ids that Antigravity answers with HTTP 404,
// so the block reruns at 24 and drops those aliases (the wire id is chosen in
// `wire_model_for_request`).
const ANTIGRAVITY_GEMINI4_MODEL_REVISION: u32 = 24;
// Antigravity Gemini occasionally ends a turn with no text and no tool call
// (about three output tokens). Without a retry list the gateway reported that
// as `empty_response` and Codex stopped the turn with "Incomplete response
// returned". Existing settings never had the list, so backfill it once.
const ANTIGRAVITY_EMPTY_RETRY_REVISION: u32 = 25;
// OpenCode's `deepseek-v4.1-flash` reached existing settings through discovery,
// so it never got the wire effort map that `deepseek-v4-flash` has. A map cannot
// tell a missing entry from one the user removed, so backfill it once. Its
// reasoning preservation comes from `PRESERVE_REASONING_ONCE_REVISION`.
const OPENCODE_DEEPSEEK_V41_REASONING_REVISION: u32 = 26;
// The `mistral` preset predates the Vibe plan support. A saved connection keeps
// `prompt_cache_key = false`, which leaves every request uncached, and has no
// reasoning levels for the plan's models. Backfill them once.
const MISTRAL_VIBE_PLAN_REVISION: u32 = 27;
// Filling `preserve_reasoning_content_models` on every load brought back an entry
// the user had removed. Settings from before this revision get the fill one last
// time. After that, `inherit_settings_with_reported_window` covers a model when it
// first appears.
const PRESERVE_REASONING_ONCE_REVISION: u32 = 28;
// Nothing documents this 200k as a real Antigravity limit, and its 180k trigger is
// what 340 tools already take on Claude, so the model compacted after every tool
// call. The row is gone, so the catalog's hosted default applies. Only a saved value
// equal to the old default is removed, because any other number is the user's.
const ANTIGRAVITY_SONNET_46_WINDOW_REVISION: u32 = 28;
const ANTIGRAVITY_SONNET_46: &str = "claude-sonnet-4-6";
const ANTIGRAVITY_SONNET_46_OLD_WINDOW: u64 = 200_000;
// `gpt-6.1-sol` is the current Codex default. Existing settings stop at
// `gpt-6-sol`, and the September block does not touch `selected_models`, so a
// new revision has to add the model and its picker selection together.
const GPT61_SOL_MODEL_REVISION: u32 = 19;
const OPENAI_MAX_REQUEST_BYTES: u64 = 64 * 1024 * 1024;
// The Anthropic Messages API rejects requests over 32 MB. The preset used the
// generic 16 MiB default, which made screenshot-heavy threads hit the local
// image-history budget long before Anthropic would. Decimal MB keeps the
// preset inside the provider's stated limit.
const ANTHROPIC_REQUEST_BUDGET_REVISION: u32 = 21;
const ANTHROPIC_MAX_REQUEST_BYTES: u64 = 32_000_000;

/// Anthropic model IDs whose context window the registry must state exactly,
/// because their names do not follow the `family-major.minor` convention that
/// `resolve_model_context_window` uses to inherit a sibling's window.
///
/// The registry added these in the September refresh without a window, so both
/// fell back to the 128k default and produced an
/// `auto_compact_token_limit` of 115,200 — low enough that Codex compacted on
/// every turn. Anthropic reports `max_input_tokens: 1000000` for each.
const ANTHROPIC_EXPLICIT_CONTEXT_MODELS: &[(&str, u64)] = &[
    ("claude-fable-5-1", 1_000_000),
    ("claude-opus-5-5", 1_000_000),
];

/// Model IDs the registry once shipped but that the upstream API never served.
///
/// `claude-mythos-5-1` was added in the September refresh, but Anthropic
/// answers `POST /v1/messages` with `not_found_error` and omits it from
/// `GET /v1/models` under every beta header, so selecting it could only ever
/// fail. Retire it by the same model-id convention as `retired_model_ids`.
const RETIRED_ANTHROPIC_MODELS: &[&str] = &["claude-mythos-5-1"];

fn backfill_non_empty_strings(target: &mut Vec<String>, defaults: &[String]) -> bool {
    if target.is_empty() && !defaults.is_empty() {
        target.extend(defaults.iter().cloned());
        true
    } else {
        false
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub base_url: &'static str,
    pub protocol: ProviderProtocol,
    pub api_key_env: Option<&'static str>,
    pub credential_source: CredentialSource,
    pub credential_transport: CredentialTransport,
    pub allow_private_network: bool,
    pub discovery: bool,
    pub requires_custom_url: bool,
    pub capabilities: ProviderCapabilities,
}

impl ProviderPreset {
    pub fn instantiate(&self, base_url: Option<&str>) -> Result<ProviderDefinition, String> {
        let selected_url = base_url
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(self.base_url);
        if self.requires_custom_url
            && base_url
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
        {
            return Err(format!("{} requires a custom base URL", self.name));
        }
        let credential = match (self.credential_source, self.api_key_env) {
            (CredentialSource::Environment, Some(env_key)) => ProviderCredential {
                source: self.credential_source,
                reference: Some(env_key.into()),
                transport: self.credential_transport,
                header_name: match self.id {
                    "google" => Some("x-goog-api-key".into()),
                    _ => None,
                },
                command: None,
            },
            (CredentialSource::Forward, _) => ProviderCredential {
                source: CredentialSource::Forward,
                ..ProviderCredential::default()
            },
            _ => ProviderCredential::default(),
        };
        let mut provider = ProviderDefinition {
            id: self.id.into(),
            name: self.name.into(),
            base_url: selected_url.into(),
            protocol: self.protocol,
            allow_private_network: self.allow_private_network,
            credential,
            capabilities: self.capabilities.clone(),
            discovery: ModelDiscoverySettings {
                enabled: self.discovery,
                ..ModelDiscoverySettings::default()
            },
            ..ProviderDefinition::default()
        };
        if matches!(self.id, "anthropic" | "anthropic-apikey") {
            provider
                .headers
                .insert("anthropic-version".into(), "2023-06-01".into());
        }
        if self.id == "azure-openai" {
            provider.credential.transport = CredentialTransport::CustomHeader;
            provider.credential.header_name = Some("api-key".into());
            provider.discovery.path = "/openai/models".into();
        }
        if self.id == "umans" {
            provider.escape_builtin_tool_names = true;
        }
        apply_registry_defaults(&mut provider);
        provider.validate()?;
        Ok(provider)
    }
}

const FULL_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const DEEPSEEK_EFFORTS: &[&str] = &["high", "xhigh", "max"];

fn apply_registry_defaults(provider: &mut ProviderDefinition) {
    match provider.id.as_str() {
        "openai" => vendors_a::apply_openai(provider),
        "openai-api" | "openai-apikey" => vendors_a::apply_openai_api(provider),
        "anthropic" | "anthropic-apikey" => vendors_a::apply_anthropic(provider),
        "google" => vendors_a::apply_google(provider),
        "google-vertex" => vendors_a::apply_google_vertex(provider),
        "google-antigravity" => vendors_a::apply_google_antigravity(provider),
        "kiro" => vendors_a::apply_kiro(provider),
        "github-copilot" => vendors_a::apply_github_copilot(provider),
        "xai" => vendors_a::apply_xai(provider),
        "meta" | "meta-ai" | "muse" => vendors_a::apply_meta(provider),
        "kimi" | "kimi-code" => vendors_b::apply_kimi(provider),
        "moonshot" => vendors_b::apply_moonshot(provider),
        "deepseek" => vendors_b::apply_deepseek(provider),
        "opencode-go" => vendors_b::apply_opencode_go(provider),
        "opencode-zen" => vendors_b::apply_opencode_zen(provider),
        "nvidia" => vendors_b::apply_nvidia(provider),
        "neuralwatt" => vendors_b::apply_neuralwatt(provider),
        "zai" => vendors_b::apply_zai(provider),
        "zhipu-bigmodel" => vendors_b::apply_zhipu_bigmodel(provider),
        "minimax" | "minimax-cn" => vendors_b::apply_minimax(provider),
        "mistral" => vendors_b::apply_mistral(provider),
        "alibaba-token-plan" => vendors_b::apply_alibaba_token_plan(provider),
        "alibaba-token-plan-intl" => vendors_b::apply_alibaba_token_plan_intl(provider),
        "tencent-coding-plan" => vendors_b::apply_tencent_coding_plan(provider),
        "volcengine" => vendors_b::apply_volcengine(provider),
        "volcengine-coding-plan" | "volcengine-agent-plan" => {
            vendors_b::apply_volcengine_coding_plan(provider)
        }
        "orcarouter" => vendors_b::apply_orcarouter(provider),
        "bizrouter" => vendors_b::apply_bizrouter(provider),
        "ollama-cloud" => vendors_b::apply_ollama_cloud(provider),
        "umans" => vendors_b::apply_umans(provider),
        "opencode-free" => vendors_b::apply_opencode_free(provider),
        _ => {}
    }
    insert_published_limits(provider);
    if provider.protocol == ProviderProtocol::AnthropicMessages {
        provider.anthropic_eof_tolerance_models = vec!["*".into()];
    }
    if matches!(
        provider.id.as_str(),
        "openai" | "openai-api" | "openai-apikey"
    ) {
        provider.capabilities.service_tier = true;
        provider.service_tier_models = provider.models.clone();
    }
    if matches!(
        provider.id.as_str(),
        "deepseek" | "minimax" | "minimax-cn" | "zhipu-bigmodel"
    ) {
        provider.no_structured_output_models = provider.models.clone();
    }
}

pub(crate) fn backfill_registry_input_limits(settings: &mut GatewaySettings) -> bool {
    let mut changed = false;
    let migrate_antigravity_models =
        settings.registry_revision < ANTIGRAVITY_MODEL_MIGRATION_REVISION;
    let migrate_deepseek_v41_flash = settings.registry_revision < DEEPSEEK_V41_FLASH_MODEL_REVISION;
    let migrate_default_vision = settings.registry_revision < DEFAULT_VISION_CAPABILITY_REVISION;
    let sidecar_image_targets = models_sidecars_send_images_to(settings);
    let mut catalog_models = BTreeMap::<String, HashSet<String>>::new();
    let registry_capabilities = provider_presets()
        .into_iter()
        .filter_map(|preset| {
            let id = preset.id.to_string();
            preset
                .instantiate(
                    preset
                        .requires_custom_url
                        .then_some("https://fixture.invalid/v1"),
                )
                .ok()
                .map(|provider| (id, provider.capabilities))
        })
        .collect::<BTreeMap<_, _>>();
    for model in settings.model_catalog.iter().filter(|model| model.enabled) {
        catalog_models
            .entry(model.provider_id.clone())
            .or_default()
            .insert(model.model_id.clone());
    }
    for provider in &mut settings.providers {
        let mut defaults = ProviderDefinition {
            id: provider.id.clone(),
            protocol: provider.protocol,
            ..ProviderDefinition::default()
        };
        apply_registry_defaults(&mut defaults);
        if settings.registry_revision < MODEL_CAPABILITY_ISOLATION_REVISION
            && provider.id == "xai"
            && provider.capabilities.image_generation
            && provider.image_generation_models.is_empty()
        {
            // xAI historically exposed image support only as a provider-wide
            // capability. Preserve the built-in image sidecar for existing
            // settings before model-level capability isolation removes the
            // stale image identity from normal discovered models.
            provider.image_generation_models = defaults.image_generation_models.clone();
            changed = true;
        }
        if settings.registry_revision < IMAGE_MODEL_ISOLATION_REVISION
            && matches!(
                provider.id.as_str(),
                "openai" | "openai-api" | "openai-apikey"
            )
        {
            // A serialized boolean cannot distinguish an omitted legacy value
            // from an explicit user opt-out. Preserve `false` fail-closed while
            // still backfilling the model aliases needed when the capability is
            // enabled. New preset instances retain the registry's `true` default.
            for (alias, wire_model) in &defaults.model_wire_ids {
                if alias != "imagegen-2" {
                    continue;
                }
                if !provider.model_wire_ids.contains_key(alias) {
                    provider
                        .model_wire_ids
                        .insert(alias.clone(), wire_model.clone());
                    changed = true;
                }
            }
            if provider
                .model_wire_ids
                .get("gpt-image-2")
                .is_some_and(|wire| wire == "gpt-image-2")
            {
                provider.model_wire_ids.remove("gpt-image-2");
                changed = true;
            }
            for model in ["imagegen-2", "gpt-image-2"] {
                if !provider
                    .image_generation_models
                    .iter()
                    .any(|configured| configured == model)
                {
                    provider.image_generation_models.push(model.into());
                    changed = true;
                }
            }
            let previous_model_count = provider.models.len();
            provider
                .models
                .retain(|model| !matches!(model.as_str(), "imagegen-2" | "gpt-image-2"));
            if provider.models.len() != previous_model_count {
                changed = true;
            }
            for model in ["imagegen-2", "gpt-image-2"] {
                changed |= provider.model_context_windows.remove(model).is_some();
                changed |= provider.model_max_input_tokens.remove(model).is_some();
                changed |= provider.model_max_output_tokens.remove(model).is_some();
                changed |= provider.model_input_modalities.remove(model).is_some();
                changed |= provider.model_reasoning_efforts.remove(model).is_some();
                changed |= provider
                    .model_default_reasoning_efforts
                    .remove(model)
                    .is_some();
            }
        }
        if settings.registry_revision < SAFE_CAPABILITY_DEFAULTS_REVISION {
            if let Some(registry) = registry_capabilities.get(&provider.id) {
                if registry.structured_output && !provider.capabilities.structured_output {
                    provider.capabilities.structured_output = true;
                    changed = true;
                }
                if provider.capabilities.tools {
                    changed |= enable_capability(
                        &mut provider.capabilities.custom_tools,
                        registry.custom_tools,
                    );
                    changed |= enable_capability(
                        &mut provider.capabilities.tool_search,
                        registry.tool_search,
                    );
                    changed |= enable_capability(
                        &mut provider.capabilities.mcp_namespaces,
                        registry.mcp_namespaces,
                    );
                }
            }
            if !provider.capabilities.tools {
                changed |= disable_capability(&mut provider.capabilities.parallel_tools);
                changed |= disable_capability(&mut provider.capabilities.custom_tools);
                changed |= disable_capability(&mut provider.capabilities.tool_search);
                changed |= disable_capability(&mut provider.capabilities.mcp_namespaces);
            }
        }
        if settings.registry_revision < STRICT_RESPONSES_TOOL_ADJACENCY_REVISION
            && defaults.requires_adjacent_responses_tool_results
            && !provider.requires_adjacent_responses_tool_results
        {
            provider.requires_adjacent_responses_tool_results = true;
            changed = true;
        }
        if settings.registry_revision < CONFORMANCE_POLICY_REVISION {
            if defaults.capabilities.service_tier && !provider.capabilities.service_tier {
                provider.capabilities.service_tier = true;
                changed = true;
            }
            changed |= backfill_non_empty_strings(
                &mut provider.service_tier_models,
                &defaults.service_tier_models,
            );
            changed |= backfill_non_empty_strings(
                &mut provider.no_structured_output_models,
                &defaults.no_structured_output_models,
            );
            changed |= backfill_non_empty_strings(
                &mut provider.anthropic_eof_tolerance_models,
                &defaults.anthropic_eof_tolerance_models,
            );
            changed |= backfill_non_empty_strings(
                &mut provider.terminal_continuation_guard_models,
                &defaults.terminal_continuation_guard_models,
            );
            changed |= backfill_non_empty_strings(
                &mut provider.empty_completion_retry_models,
                &defaults.empty_completion_retry_models,
            );
        }
        if settings.registry_revision < STRICT_RESPONSES_TOOL_ADJACENCY_REVISION
            && provider.requires_reasoning_placeholder_models.is_none()
        {
            if let Some(models) = &defaults.requires_reasoning_placeholder_models {
                provider.requires_reasoning_placeholder_models = Some(models.clone());
                changed = true;
            }
        }
        if provider.id == "google-antigravity"
            && provider.models.is_empty()
            && !defaults.models.is_empty()
        {
            provider.models = defaults.models.clone();
            changed = true;
        }
        if settings.registry_revision < ANTIGRAVITY_GEMINI4_MODEL_REVISION
            && provider.id == "google-antigravity"
        {
            let model = "gemini-4-argon";
            if !provider.models.iter().any(|configured| configured == model) {
                provider.models.insert(0, model.into());
                changed = true;
            }
            if !provider.model_context_windows.contains_key(model) {
                if let Some(window) = defaults
                    .model_context_windows
                    .get(model)
                    .filter(|window| window_fits_saved_limits(provider, model, **window))
                {
                    provider.model_context_windows.insert(model.into(), *window);
                    changed = true;
                }
            }
            if !provider.model_reasoning_efforts.contains_key(model) {
                if let Some(efforts) = defaults
                    .model_reasoning_efforts
                    .get(model)
                    .filter(|efforts| efforts_fit_saved_default(provider, model, efforts))
                {
                    provider
                        .model_reasoning_efforts
                        .insert(model.into(), efforts.clone());
                    changed = true;
                }
            }
            if !provider.model_default_reasoning_efforts.contains_key(model) {
                if let Some(effort) = defaults
                    .model_default_reasoning_efforts
                    .get(model)
                    .filter(|effort| default_effort_fits_saved_efforts(provider, model, effort))
                {
                    provider
                        .model_default_reasoning_efforts
                        .insert(model.into(), effort.clone());
                    changed = true;
                }
            }
            // Revisions 20-22 pointed the aliases at `-tiered` / `-medium` ids that
            // Antigravity does not serve; the model is addressed by its own id.
            let before = provider.model_wire_ids.len();
            provider.model_wire_ids.retain(|alias, wire| {
                !(alias.starts_with("gemini-4-argon") && wire.starts_with("gemini-4-argon-"))
            });
            changed |= provider.model_wire_ids.len() != before;
            let published = format!("{}/{}", provider.id, model);
            if provider.enabled
                && !settings
                    .catalog
                    .selected_models
                    .iter()
                    .any(|id| id == &published)
            {
                settings.catalog.selected_models.push(published);
                changed = true;
            }
        }
        if settings.registry_revision < ANTIGRAVITY_EMPTY_RETRY_REVISION
            && provider.id == "google-antigravity"
        {
            changed |= backfill_non_empty_strings(
                &mut provider.empty_completion_retry_models,
                &defaults.empty_completion_retry_models,
            );
        }
        if migrate_antigravity_models && provider.id == "google-antigravity" {
            for model in ["gemini-3.7-flash", "gemini-3.5-flash"] {
                if !provider.models.iter().any(|configured| configured == model) {
                    let default_index = defaults
                        .models
                        .iter()
                        .position(|candidate| candidate == model)
                        .unwrap_or(provider.models.len());
                    provider
                        .models
                        .insert(default_index.min(provider.models.len()), model.into());
                    changed = true;
                }
                if !provider.model_context_windows.contains_key(model) {
                    if let Some(context) = defaults
                        .model_context_windows
                        .get(model)
                        .filter(|context| window_fits_saved_limits(provider, model, **context))
                    {
                        provider
                            .model_context_windows
                            .insert(model.into(), *context);
                        changed = true;
                    }
                }
                if !provider.model_reasoning_efforts.contains_key(model) {
                    if let Some(efforts) = defaults
                        .model_reasoning_efforts
                        .get(model)
                        .filter(|efforts| efforts_fit_saved_default(provider, model, efforts))
                    {
                        provider
                            .model_reasoning_efforts
                            .insert(model.into(), efforts.clone());
                        changed = true;
                    }
                }
                if !provider.model_default_reasoning_efforts.contains_key(model) {
                    if let Some(effort) = defaults
                        .model_default_reasoning_efforts
                        .get(model)
                        .filter(|effort| default_effort_fits_saved_efforts(provider, model, effort))
                    {
                        provider
                            .model_default_reasoning_efforts
                            .insert(model.into(), effort.clone());
                        changed = true;
                    }
                }
            }
        }
        if migrate_deepseek_v41_flash && provider.id == "deepseek" {
            // DeepSeek's current V4.1 Flash API model is exposed as
            // `deepseek-flash`; keep the old `deepseek-v4-flash` alias for
            // existing routes and configurations.
            let model = "deepseek-flash";
            if !provider.models.iter().any(|configured| configured == model) {
                let insert_at = provider
                    .models
                    .iter()
                    .position(|configured| configured == "deepseek-v4-flash")
                    .unwrap_or(provider.models.len());
                provider.models.insert(insert_at, model.into());
                changed = true;
            }
            if !provider
                .no_structured_output_models
                .iter()
                .any(|configured| configured == model)
            {
                provider.no_structured_output_models.push(model.into());
                changed = true;
            }
            if provider
                .model_protocols
                .insert(model.into(), ProviderProtocol::Responses)
                != Some(ProviderProtocol::Responses)
            {
                changed = true;
            }
            if settings.catalog.selected_models.iter().any(|selected| {
                selected == "deepseek/deepseek-v4-flash" || selected == "deepseek-v4-flash"
            }) && !settings
                .catalog
                .selected_models
                .iter()
                .any(|selected| selected == "deepseek/deepseek-flash")
            {
                settings
                    .catalog
                    .selected_models
                    .push("deepseek/deepseek-flash".into());
                changed = true;
            }
        }
        if migrate_default_vision && !provider.capabilities.vision {
            provider.capabilities.vision = true;
            changed = true;
        }
        if settings.registry_revision < ANTHROPIC_FABLE_OPUS_CONTEXT_REVISION
            && matches!(provider.id.as_str(), "anthropic" | "anthropic-apikey")
        {
            // Fill a missing window only. An existing value is left untouched
            // because a serialized number cannot say whether it came from the
            // old 128k default or from the user's own choice: a user who set
            // 64_000 or 128_000 deliberately would otherwise be silently
            // raised to 1,000,000. Recovering an intended smaller value after
            // the fact is not possible, so never overwrite.
            for (model, window) in ANTHROPIC_EXPLICIT_CONTEXT_MODELS {
                if !provider.models.iter().any(|configured| configured == model) {
                    continue;
                }
                if !provider.model_context_windows.contains_key(*model)
                    && window_fits_saved_limits(provider, model, *window)
                {
                    provider
                        .model_context_windows
                        .insert((*model).into(), *window);
                    changed = true;
                }
            }
        }
        if settings.registry_revision < ANTIGRAVITY_SONNET_46_WINDOW_REVISION
            && provider.id == "google-antigravity"
            && provider.model_context_windows.get(ANTIGRAVITY_SONNET_46)
                == Some(&ANTIGRAVITY_SONNET_46_OLD_WINDOW)
        {
            provider.model_context_windows.remove(ANTIGRAVITY_SONNET_46);
            changed = true;
        }
        if settings.registry_revision < MISTRAL_VIBE_PLAN_REVISION
            && provider.id == "mistral"
            && crate::compat::is_mistral_chat_endpoint(&provider.base_url)
        {
            // The per-model rows arrive through the fills below, which know how to
            // keep them consistent with what the connection already saved, because
            // these models are configured from here on.
            provider.prompt_cache_key = true;
            // Any path other than the old default is the user's own.
            if provider.discovery.path == ModelDiscoverySettings::default().path {
                provider.discovery.path = defaults.discovery.path.clone();
            }
            provider.capabilities.reasoning = true;
            for model in &defaults.models {
                if !provider.models.contains(model) {
                    provider.models.push(model.clone());
                }
            }
            if provider.default_model.is_none() {
                provider.default_model = defaults.default_model.clone();
            }
            changed = true;
        }
        if settings.registry_revision < OPENCODE_DEEPSEEK_V41_REASONING_REVISION
            && matches!(
                provider.id.as_str(),
                "opencode-go" | "opencode-zen" | "opencode-free"
            )
        {
            for (model, mapping) in &defaults.model_reasoning_effort_map {
                if model.starts_with("deepseek-v4.1")
                    && !provider.model_reasoning_effort_map.contains_key(model)
                {
                    provider
                        .model_reasoning_effort_map
                        .insert(model.clone(), mapping.clone());
                    changed = true;
                }
            }
        }
        // Migrate only the old preset budget once; retain non-default limits
        // and allow users to explicitly restore a smaller cap after migration.
        if settings.registry_revision < OPENAI_REQUEST_BUDGET_REVISION
            && matches!(
                provider.id.as_str(),
                "openai" | "openai-api" | "openai-apikey"
            )
            && provider.limits.max_request_bytes == 16 * 1024 * 1024
        {
            provider.limits.max_request_bytes = OPENAI_MAX_REQUEST_BYTES;
            changed = true;
        }
        // Same one-shot rule as the OpenAI budget: only the old preset value
        // is raised; a limit the user chose is kept.
        if settings.registry_revision < ANTHROPIC_REQUEST_BUDGET_REVISION
            && matches!(provider.id.as_str(), "anthropic" | "anthropic-apikey")
            && provider.limits.max_request_bytes == 16 * 1024 * 1024
        {
            provider.limits.max_request_bytes = ANTHROPIC_MAX_REQUEST_BYTES;
            changed = true;
        }
        if settings.registry_revision < GPT6_ASTRA_MODEL_REVISION
            && matches!(
                provider.id.as_str(),
                "openai" | "openai-api" | "openai-apikey"
            )
        {
            let model = "gpt-6-astra";
            if !provider.models.iter().any(|configured| configured == model) {
                provider.models.insert(0, model.into());
                changed = true;
            }
            if !provider
                .service_tier_models
                .iter()
                .any(|configured| configured == model)
            {
                provider.service_tier_models.insert(0, model.into());
                changed = true;
            }
            if !provider.model_context_windows.contains_key(model) {
                if let Some(value) = defaults
                    .model_context_windows
                    .get(model)
                    .filter(|value| window_fits_saved_limits(provider, model, **value))
                {
                    provider.model_context_windows.insert(model.into(), *value);
                    changed = true;
                }
            }
            if !provider.model_max_input_tokens.contains_key(model) {
                if let Some(value) = defaults
                    .model_max_input_tokens
                    .get(model)
                    .filter(|value| limit_fits_saved_window(provider, model, **value))
                {
                    provider.model_max_input_tokens.insert(model.into(), *value);
                    changed = true;
                }
            }
            if !provider.model_max_output_tokens.contains_key(model) {
                if let Some(value) = defaults
                    .model_max_output_tokens
                    .get(model)
                    .filter(|value| limit_fits_saved_window(provider, model, **value))
                {
                    provider
                        .model_max_output_tokens
                        .insert(model.into(), *value);
                    changed = true;
                }
            }
            if !provider.model_input_modalities.contains_key(model) {
                if let Some(value) = defaults.model_input_modalities.get(model) {
                    provider
                        .model_input_modalities
                        .insert(model.into(), value.clone());
                    changed = true;
                }
            }
            if !provider.model_reasoning_efforts.contains_key(model) {
                if let Some(value) = defaults
                    .model_reasoning_efforts
                    .get(model)
                    .filter(|value| efforts_fit_saved_default(provider, model, value))
                {
                    provider
                        .model_reasoning_efforts
                        .insert(model.into(), value.clone());
                    changed = true;
                }
            }
        }
        if settings.registry_revision < SEPTEMBER_MODEL_REFRESH_REVISION {
            let additions: &[&str] = match provider.id.as_str() {
                "openai" | "openai-api" | "openai-apikey" => &["gpt-6-sol", "gpt-6-luna"],
                "anthropic" | "anthropic-apikey" => &["claude-fable-5-1", "claude-opus-5-5"],
                "xai" => &["grok-4.7"],
                _ => &[],
            };
            for &model in additions {
                if !provider.models.iter().any(|existing| existing == model) {
                    provider.models.push(model.into());
                    changed = true;
                }
                if defaults
                    .service_tier_models
                    .iter()
                    .any(|existing| existing == model)
                    && !provider
                        .service_tier_models
                        .iter()
                        .any(|existing| existing == model)
                {
                    provider.service_tier_models.push(model.into());
                    changed = true;
                }
                if !provider.model_max_output_tokens.contains_key(model) {
                    if let Some(limit) = defaults
                        .model_max_output_tokens
                        .get(model)
                        .filter(|limit| limit_fits_saved_window(provider, model, **limit))
                    {
                        provider
                            .model_max_output_tokens
                            .insert(model.into(), *limit);
                        changed = true;
                    }
                }
            }
        }
        if settings.registry_revision < ANTHROPIC_SONNET_55_REVISION
            && matches!(provider.id.as_str(), "anthropic" | "anthropic-apikey")
        {
            // Add the new model and fill only missing per-model metadata, so
            // user-edited windows or efforts survive the migration.
            let model = "claude-sonnet-5-5";
            // Move only the previous preset default; a user-picked default stays.
            if provider.default_model.as_deref() == Some("claude-sonnet-5") {
                provider.default_model = Some(model.into());
                changed = true;
            }
            if !provider.models.iter().any(|existing| existing == model) {
                provider.models.push(model.into());
                changed = true;
            }
            if !provider.model_context_windows.contains_key(model) {
                if let Some(value) = defaults
                    .model_context_windows
                    .get(model)
                    .filter(|value| window_fits_saved_limits(provider, model, **value))
                {
                    provider.model_context_windows.insert(model.into(), *value);
                    changed = true;
                }
            }
            if !provider.model_reasoning_efforts.contains_key(model) {
                if let Some(value) = defaults
                    .model_reasoning_efforts
                    .get(model)
                    .filter(|value| efforts_fit_saved_default(provider, model, value))
                {
                    provider
                        .model_reasoning_efforts
                        .insert(model.into(), value.clone());
                    changed = true;
                }
            }
            // Surface it in Codex only when this provider's Claude models are
            // already selected; never opt an unused provider in.
            let prefix = format!("{}/", provider.id);
            let qualified = format!("{prefix}{model}");
            let selected = &mut settings.catalog.selected_models;
            if selected.iter().any(|entry| entry.starts_with(&prefix))
                && !selected.iter().any(|entry| entry == &qualified)
            {
                selected.push(qualified);
                changed = true;
            }
        }
        if settings.registry_revision < GPT61_SOL_MODEL_REVISION
            && matches!(
                provider.id.as_str(),
                "openai" | "openai-api" | "openai-apikey"
            )
        {
            // Fill only missing per-model metadata so a user-edited window or
            // effort list survives the migration. `gpt-6-sol` and `gpt-6-luna`
            // shipped in the September refresh without a picker entry; this
            // revision adds the current default model next to them.
            let model = "gpt-6.1-sol";
            let published = if provider.id == "openai" {
                model.to_string()
            } else {
                format!("{}/{}", provider.id, model)
            };
            // Surface it in Codex only when this provider already publishes
            // models; never opt an unused provider in. `selected_models`
            // entries for native OpenAI are bare slugs.
            // An empty list means "publish everything"; pushing an entry
            // would turn it into a one-model allowlist, so leave it empty.
            let provider_is_published = !settings.catalog.selected_models.is_empty()
                && settings.catalog.selected_models.iter().any(|entry| {
                    entry == model
                        || entry == &published
                        || provider.models.iter().any(|configured| {
                            entry == configured
                                || entry == &format!("{}/{}", provider.id, configured)
                        })
                });
            if !provider.models.iter().any(|existing| existing == model) {
                // Keep the family order by following `gpt-6-sol` when present.
                let insert_at = provider
                    .models
                    .iter()
                    .position(|configured| configured == "gpt-6-sol")
                    .map(|index| index + 1)
                    .unwrap_or(provider.models.len());
                provider
                    .models
                    .insert(insert_at.min(provider.models.len()), model.into());
                changed = true;
            }
            if !provider.model_context_windows.contains_key(model) {
                if let Some(value) = defaults
                    .model_context_windows
                    .get(model)
                    .filter(|value| window_fits_saved_limits(provider, model, **value))
                {
                    provider.model_context_windows.insert(model.into(), *value);
                    changed = true;
                }
            }
            if !provider.model_max_input_tokens.contains_key(model) {
                if let Some(value) = defaults
                    .model_max_input_tokens
                    .get(model)
                    .filter(|value| limit_fits_saved_window(provider, model, **value))
                {
                    provider.model_max_input_tokens.insert(model.into(), *value);
                    changed = true;
                }
            }
            if !provider.model_max_output_tokens.contains_key(model) {
                if let Some(value) = defaults
                    .model_max_output_tokens
                    .get(model)
                    .filter(|value| limit_fits_saved_window(provider, model, **value))
                {
                    provider
                        .model_max_output_tokens
                        .insert(model.into(), *value);
                    changed = true;
                }
            }
            if !provider.model_input_modalities.contains_key(model) {
                if let Some(value) = defaults.model_input_modalities.get(model) {
                    provider
                        .model_input_modalities
                        .insert(model.into(), value.clone());
                    changed = true;
                }
            }
            if !provider.model_reasoning_efforts.contains_key(model) {
                if let Some(value) = defaults
                    .model_reasoning_efforts
                    .get(model)
                    .filter(|value| efforts_fit_saved_default(provider, model, value))
                {
                    provider
                        .model_reasoning_efforts
                        .insert(model.into(), value.clone());
                    changed = true;
                }
            }
            if !provider.model_default_reasoning_efforts.contains_key(model) {
                if let Some(value) = defaults
                    .model_default_reasoning_efforts
                    .get(model)
                    .filter(|value| default_effort_fits_saved_efforts(provider, model, value))
                {
                    provider
                        .model_default_reasoning_efforts
                        .insert(model.into(), value.clone());
                    changed = true;
                }
            }
            if provider
                .service_tier_models
                .iter()
                .any(|configured| configured == "gpt-6-sol")
                && !provider
                    .service_tier_models
                    .iter()
                    .any(|configured| configured == model)
            {
                provider.service_tier_models.push(model.into());
                changed = true;
            }
            let selected = &mut settings.catalog.selected_models;
            if provider_is_published && !selected.iter().any(|entry| entry == &published) {
                selected.push(published);
                changed = true;
            }
        }
        if settings.registry_revision < RETIRED_ANTHROPIC_MYTHOS_REVISION
            && matches!(provider.id.as_str(), "anthropic" | "anthropic-apikey")
        {
            // Drop the retired ID everywhere it can be selected or described.
            // Leave the default model alone unless it *is* the retired ID,
            // which can only ever be an upstream 404.
            let retired = RETIRED_ANTHROPIC_MODELS;
            let previous_len = provider.models.len();
            provider
                .models
                .retain(|model| !retired.contains(&model.as_str()));
            changed |= provider.models.len() != previous_len;
            if provider
                .default_model
                .as_deref()
                .is_some_and(|model| retired.contains(&model))
            {
                provider.default_model = None;
                changed = true;
            }
            for model in retired {
                changed |= provider.model_context_windows.remove(*model).is_some();
                changed |= provider.model_max_input_tokens.remove(*model).is_some();
                changed |= provider.model_max_output_tokens.remove(*model).is_some();
                changed |= provider.model_input_modalities.remove(*model).is_some();
                changed |= provider.model_reasoning_efforts.remove(*model).is_some();
                changed |= provider
                    .model_default_reasoning_efforts
                    .remove(*model)
                    .is_some();
                changed |= provider.model_reasoning_modes.remove(*model).is_some();
                changed |= provider.model_protocols.remove(*model).is_some();
                changed |= provider.model_wire_ids.remove(*model).is_some();
                changed |= provider.model_reasoning_effort_map.remove(*model).is_some();
                for list in [
                    &mut provider.no_reasoning_models,
                    &mut provider.no_temperature_models,
                    &mut provider.no_top_p_models,
                    &mut provider.no_penalty_models,
                    &mut provider.no_structured_output_models,
                    &mut provider.service_tier_models,
                    &mut provider.anthropic_eof_tolerance_models,
                    &mut provider.terminal_continuation_guard_models,
                    &mut provider.empty_completion_retry_models,
                    &mut provider.auto_tool_choice_only_models,
                    &mut provider.preserve_reasoning_content_models,
                    &mut provider.reasoning_split_models,
                    &mut provider.thinking_toggle_models,
                    &mut provider.thinking_budget_models,
                ] {
                    let previous_len = list.len();
                    list.retain(|entry| !retired.contains(&entry.as_str()));
                    changed |= list.len() != previous_len;
                }
            }
        }
        let configured_models = provider
            .models
            .iter()
            .cloned()
            .chain(provider.default_model.iter().cloned())
            .chain(
                catalog_models
                    .get(&provider.id)
                    .into_iter()
                    .flat_map(|models| models.iter().cloned()),
            )
            .collect::<HashSet<_>>();
        for (model, limit) in &defaults.model_max_input_tokens {
            if !configured_models.contains(model) {
                continue;
            }
            if !provider.model_context_windows.contains_key(model) {
                if let Some(context) = defaults
                    .model_context_windows
                    .get(model)
                    .filter(|context| window_fits_saved_limits(provider, model, **context))
                {
                    provider
                        .model_context_windows
                        .insert(model.clone(), *context);
                    changed = true;
                }
            }
            if !provider.model_max_input_tokens.contains_key(model) {
                let input_limit = provider
                    .model_context_windows
                    .get(model)
                    .map(|context| (*limit).min(*context))
                    .unwrap_or(*limit);
                provider
                    .model_max_input_tokens
                    .insert(model.clone(), input_limit);
                changed = true;
            }
        }
        // Context windows are independent of max-input contracts. Discovered
        // models such as grok-4.6 used to keep the 128k catalog fallback until
        // a matching max-input row existed.
        for (model, context) in &defaults.model_context_windows {
            if !configured_models.contains(model)
                || provider.model_context_windows.contains_key(model)
                || !window_fits_saved_limits(provider, model, *context)
            {
                continue;
            }
            provider
                .model_context_windows
                .insert(model.clone(), *context);
            changed = true;
        }
        let inherited = configured_models
            .iter()
            .filter(|model| !provider.model_context_windows.contains_key(model.as_str()))
            .filter_map(|model| {
                resolve_model_context_window(&provider.model_context_windows, model)
                    .filter(|window| window_fits_saved_limits(provider, model, *window))
                    .map(|window| (model.clone(), window))
            })
            .collect::<Vec<_>>();
        for (model, window) in inherited {
            provider.model_context_windows.insert(model, window);
            changed = true;
        }
        for (model, efforts) in &defaults.model_reasoning_efforts {
            if !configured_models.contains(model)
                || provider.model_reasoning_efforts.contains_key(model)
                || !efforts_fit_saved_default(provider, model, efforts)
            {
                continue;
            }
            provider
                .model_reasoning_efforts
                .insert(model.clone(), efforts.clone());
            changed = true;
        }
        for (model, effort) in &defaults.model_default_reasoning_efforts {
            if !configured_models.contains(model)
                || provider.model_default_reasoning_efforts.contains_key(model)
                || !default_effort_fits_saved_efforts(provider, model, effort)
            {
                continue;
            }
            provider
                .model_default_reasoning_efforts
                .insert(model.clone(), effort.clone());
            changed = true;
        }
        for (model, modalities) in &defaults.model_input_modalities {
            if !configured_models.contains(model)
                || provider.model_input_modalities.contains_key(model)
                || (!modalities.iter().any(|modality| modality == "image")
                    && sidecar_image_targets.contains(&format!("{}/{}", provider.id, model)))
            {
                continue;
            }
            provider
                .model_input_modalities
                .insert(model.clone(), modalities.clone());
            changed = true;
        }
        if settings.registry_revision < PRESERVE_REASONING_ONCE_REVISION {
            for model in &defaults.preserve_reasoning_content_models {
                if !configured_models.contains(model)
                    || provider
                        .preserve_reasoning_content_models
                        .iter()
                        .any(|existing| existing == model)
                {
                    continue;
                }
                provider
                    .preserve_reasoning_content_models
                    .push(model.clone());
                changed = true;
            }
        }
    }
    if settings.registry_revision < ANTIGRAVITY_SONNET_46_WINDOW_REVISION {
        // Discovery copies the window into the catalog row, which outranks the
        // provider's own map.
        for metadata in &mut settings.model_catalog {
            if metadata.provider_id == "google-antigravity"
                && metadata.model_id == ANTIGRAVITY_SONNET_46
                && metadata.context_window == Some(ANTIGRAVITY_SONNET_46_OLD_WINDOW)
            {
                metadata.context_window = None;
                changed = true;
            }
        }
    }
    if migrate_default_vision {
        for metadata in &mut settings.model_catalog {
            if !metadata.capabilities.vision {
                metadata.capabilities.vision = true;
                changed = true;
            }
            if !metadata
                .input_modalities
                .iter()
                .any(|modality| modality == "image")
            {
                metadata.input_modalities.push("image".into());
                changed = true;
            }
        }
    }
    if settings.registry_revision < MODEL_CAPABILITY_ISOLATION_REVISION {
        // Before model capability isolation, discovery copied the provider-wide
        // image flag into every catalog row. Only models explicitly declared as
        // image models may retain that identity during migration. Restrict the
        // repair to models already configured on the provider so user-defined
        // metadata-only overrides remain intact.
        let provider_image_contracts = settings
            .providers
            .iter()
            .map(|provider| {
                (
                    provider.id.clone(),
                    (
                        provider.capabilities.image_generation,
                        provider.models.clone(),
                        provider.default_model.clone(),
                        provider.image_generation_models.clone(),
                        provider.model_wire_ids.clone(),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        for metadata in &mut settings.model_catalog {
            let Some((provider_image_generation, models, default_model, image_models, wire_ids)) =
                provider_image_contracts.get(&metadata.provider_id)
            else {
                continue;
            };
            let is_configured_model = model_matches_registry(&metadata.model_id, models)
                || default_model.as_deref() == Some(metadata.model_id.as_str());
            let wire_model = wire_ids
                .get(&metadata.model_id)
                .unwrap_or(&metadata.model_id);
            let explicit_image_model = image_models
                .iter()
                .any(|configured| wire_ids.get(configured).unwrap_or(configured) == wire_model);
            if !*provider_image_generation || (!is_configured_model && !explicit_image_model) {
                continue;
            }
            if !explicit_image_model {
                changed |= disable_capability(&mut metadata.capabilities.image_generation);
            }
        }
    }
    if settings.registry_revision < SAFE_CAPABILITY_DEFAULTS_REVISION {
        let provider_defaults = settings
            .providers
            .iter()
            .map(|provider| {
                (
                    provider.id.clone(),
                    (
                        provider.capabilities.clone(),
                        provider.no_structured_output_models.clone(),
                        provider.service_tier_models.clone(),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        for metadata in &mut settings.model_catalog {
            let Some((provider, no_structured, service_tier)) =
                provider_defaults.get(&metadata.provider_id)
            else {
                continue;
            };
            let structured_output = provider.structured_output
                && !model_matches_registry(&metadata.model_id, no_structured);
            let service_tier =
                provider.service_tier || model_matches_registry(&metadata.model_id, service_tier);
            changed |= set_capability(
                &mut metadata.capabilities.structured_output,
                structured_output,
            );
            changed |= set_capability(&mut metadata.capabilities.service_tier, service_tier);
            changed |= set_capability(
                &mut metadata.capabilities.custom_tools,
                provider.tools && provider.custom_tools,
            );
            changed |= set_capability(
                &mut metadata.capabilities.tool_search,
                provider.tools && provider.tool_search,
            );
            changed |= set_capability(
                &mut metadata.capabilities.mcp_namespaces,
                provider.tools && provider.mcp_namespaces,
            );
            changed |= set_capability(
                &mut metadata.capabilities.provider_metadata,
                provider.provider_metadata,
            );
            if !metadata.capabilities.tools {
                changed |= disable_capability(&mut metadata.capabilities.parallel_tools);
                changed |= disable_capability(&mut metadata.capabilities.custom_tools);
                changed |= disable_capability(&mut metadata.capabilities.tool_search);
                changed |= disable_capability(&mut metadata.capabilities.mcp_namespaces);
            }
        }
    }
    if settings.registry_revision < RETIRED_ANTHROPIC_MYTHOS_REVISION {
        // The retired ID can also linger in the catalog, in a route target, or
        // in the user's picker selection. Remove it from all three so nothing
        // offers a model that can only return an upstream 404.
        for entry in RETIRED_ANTHROPIC_MODELS {
            let route_suffix = format!("/{entry}");
            let previous_len = settings.model_catalog.len();
            settings.model_catalog.retain(|metadata| {
                !(metadata.provider_id == "anthropic"
                    && (metadata.model_id == *entry || metadata.model_id.ends_with(&route_suffix)))
            });
            changed |= settings.model_catalog.len() != previous_len;

            let previous_len = settings.catalog.selected_models.len();
            settings
                .catalog
                .selected_models
                .retain(|route| !route.ends_with(&route_suffix));
            changed |= settings.catalog.selected_models.len() != previous_len;

            let previous_len = settings.catalog.model_picker_order.len();
            settings
                .catalog
                .model_picker_order
                .retain(|route| !route.ends_with(&route_suffix));
            changed |= settings.catalog.model_picker_order.len() != previous_len;

            for route in &mut settings.routes {
                for target in &mut route.targets {
                    // Route targets are `provider/model` route names.
                    if target.model.ends_with(&route_suffix) {
                        let replacement = settings
                            .providers
                            .iter()
                            .find(|provider| provider.id == "anthropic")
                            .and_then(|provider| provider.default_model.clone())
                            .or_else(|| provider_default_model("anthropic"));
                        let Some(replacement) = replacement else {
                            continue;
                        };
                        target.model = format!("anthropic/{replacement}");
                        changed = true;
                    }
                }
            }
        }
    }
    if settings.registry_revision < REGISTRY_REVISION {
        settings.registry_revision = REGISTRY_REVISION;
        changed = true;
    }
    changed
}

/// `provider/model` targets that a saved sidecar sends images to, directly or through a
/// route. Validation rejects a text-only row on any of them, and settings that fail
/// validation cannot be loaded, so the model's real limit shows up as a sidecar error.
fn models_sidecars_send_images_to(settings: &GatewaySettings) -> HashSet<String> {
    [
        settings.sidecars.vision_model.as_deref(),
        settings.sidecars.video_input_model.as_deref(),
        settings.sidecars.document_model.as_deref(),
    ]
    .into_iter()
    .flatten()
    .flat_map(|target| {
        settings
            .routes
            .iter()
            .find(|route| {
                route.enabled && (route.id == target || route.alias.as_deref() == Some(target))
            })
            .map_or_else(
                || vec![target.to_string()],
                |route| {
                    route
                        .targets
                        .iter()
                        .map(|item| item.model.clone())
                        .collect()
                },
            )
    })
    .collect()
}

fn provider_default_model(provider_id: &str) -> Option<String> {
    provider_presets()
        .into_iter()
        .find(|preset| preset.id == provider_id)
        .and_then(|preset| preset.instantiate(None).ok())
        .and_then(|provider| provider.default_model)
}

fn enable_capability(current: &mut bool, desired: bool) -> bool {
    if desired && !*current {
        *current = true;
        true
    } else {
        false
    }
}

fn disable_capability(current: &mut bool) -> bool {
    if *current {
        *current = false;
        true
    } else {
        false
    }
}

fn set_capability(current: &mut bool, desired: bool) -> bool {
    if *current == desired {
        false
    } else {
        *current = desired;
        true
    }
}

fn model_matches_registry(model: &str, configured: &[String]) -> bool {
    let model_folded = model.to_ascii_lowercase();
    configured.iter().any(|candidate| {
        let candidate_folded = candidate.to_ascii_lowercase();
        candidate_folded == "*"
            || model_folded == candidate_folded
            || model_folded
                .strip_prefix(&candidate_folded)
                .is_some_and(|suffix| suffix.starts_with(':'))
            || candidate_folded
                .strip_prefix(&model_folded)
                .is_some_and(|suffix| suffix.starts_with(':'))
    })
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

/// Exact window, or the window of the closest older sibling. `grok-4.7` therefore
/// inherits `grok-4.6` (500k) instead of falling back to the catalog's default,
/// and `deepseek-v4.1-flash` inherits `deepseek-v4-flash`.
pub(crate) fn resolve_model_context_window(
    windows: &BTreeMap<String, u64>,
    model_id: &str,
) -> Option<u64> {
    if let Some(window) = windows.get(model_id) {
        return Some(*window);
    }
    let sibling = closest_older_sibling(windows.keys().map(String::as_str), model_id)?;
    windows.get(sibling).copied()
}

/// Validation rejects a max input or output above the window, and settings that
/// fail validation cannot be loaded at all, so a window filled in automatically must
/// not contradict a limit the user saved. The catalog's default covers the gap.
fn window_fits_saved_limits(provider: &ProviderDefinition, model: &str, window: u64) -> bool {
    [
        &provider.model_max_input_tokens,
        &provider.model_max_output_tokens,
    ]
    .iter()
    .all(|limits| limits.get(model).is_none_or(|limit| *limit <= window))
}

/// The reverse of `window_fits_saved_limits`, for a limit filled in under a window
/// the user saved.
fn limit_fits_saved_window(provider: &ProviderDefinition, model: &str, limit: u64) -> bool {
    provider
        .model_context_windows
        .get(model)
        .is_none_or(|window| limit <= *window)
}

/// Validation requires a default effort to be in the model's own list, so an effort
/// list filled in automatically must contain the default the user saved.
fn efforts_fit_saved_default(
    provider: &ProviderDefinition,
    model: &str,
    efforts: &[String],
) -> bool {
    provider
        .model_default_reasoning_efforts
        .get(model)
        .is_none_or(|default| efforts.is_empty() || efforts.contains(default))
}

/// The reverse of `efforts_fit_saved_default`, for a default filled in under a list
/// the user saved.
fn default_effort_fits_saved_efforts(
    provider: &ProviderDefinition,
    model: &str,
    default: &str,
) -> bool {
    provider
        .model_reasoning_efforts
        .get(model)
        .is_none_or(|efforts| efforts.is_empty() || efforts.iter().any(|effort| effort == default))
}

/// Gives a newly discovered model the settings of its closest older sibling.
///
/// A provider's `/models` usually lists only ids, so a new version would otherwise
/// start with none of the limits and wire quirks its family needs. Call this once,
/// when the model first appears: a value the user later changes or removes must
/// stay that way, and existing values are never replaced.
///
/// Everything comes from one sibling. Choosing per setting would mix versions:
/// `glm-5.4` would take the thinking toggle of `glm-5.1` even though `glm-5.2` and
/// `glm-5.3` dropped it.
pub fn inherit_settings_from_older_sibling(provider: &mut ProviderDefinition, model: &str) {
    inherit_settings_with_reported_window(provider, model, None);
}

/// `inherit_settings_from_older_sibling` for a model whose `/models` row states a
/// window. That window outranks the provider's own map in the catalog and in routing,
/// so limits taken from a larger sibling must fit it too.
pub fn inherit_settings_with_reported_window(
    provider: &mut ProviderDefinition,
    model: &str,
    reported_window: Option<u64>,
) {
    fn copy_entry<V: Clone>(map: &mut BTreeMap<String, V>, sibling: &str, model: &str) {
        if !map.contains_key(model) {
            if let Some(value) = map.get(sibling).cloned() {
                map.insert(model.to_string(), value);
            }
        }
    }
    fn copy_membership(list: &mut Vec<String>, sibling: &str, model: &str) {
        if list.iter().any(|entry| entry == sibling) && !list.iter().any(|entry| entry == model) {
            list.push(model.to_string());
        }
    }

    // The load-time backfill stops after one revision so a removal sticks. DeepSeek
    // and Kimi can reject a history without this, even with no sibling to copy from.
    let mut defaults = ProviderDefinition {
        id: provider.id.clone(),
        protocol: provider.protocol,
        ..ProviderDefinition::default()
    };
    apply_registry_defaults(&mut defaults);
    if defaults
        .preserve_reasoning_content_models
        .iter()
        .any(|entry| entry == model)
        && !provider
            .preserve_reasoning_content_models
            .iter()
            .any(|entry| entry == model)
    {
        provider
            .preserve_reasoning_content_models
            .push(model.to_string());
    }

    let known_models = provider
        .models
        .iter()
        .chain(provider.model_protocols.keys())
        .chain(provider.model_input_modalities.keys())
        .chain(provider.model_reasoning_effort_map.keys())
        .chain(provider.model_context_windows.keys())
        .chain(provider.model_max_input_tokens.keys())
        .chain(provider.model_max_output_tokens.keys())
        .chain(provider.model_reasoning_efforts.keys())
        .chain(provider.model_default_reasoning_efforts.keys())
        .map(String::as_str)
        .filter(|known| *known != model);
    let Some(sibling) = closest_older_sibling(known_models, model).map(str::to_string) else {
        return;
    };
    let sibling = sibling.as_str();

    copy_entry(&mut provider.model_protocols, sibling, model);
    copy_entry(&mut provider.model_input_modalities, sibling, model);
    copy_entry(&mut provider.model_reasoning_effort_map, sibling, model);
    if let Some(window) = provider.model_context_windows.get(sibling).copied() {
        if !provider.model_context_windows.contains_key(model)
            && window_fits_saved_limits(provider, model, window)
        {
            provider
                .model_context_windows
                .insert(model.to_string(), window);
        }
    }

    // Validation rejects a limit above the window, so the smallest window the model
    // is known to have caps what it inherits.
    let window = provider
        .model_context_windows
        .get(model)
        .copied()
        .into_iter()
        .chain(reported_window)
        .min();
    if !provider.model_max_input_tokens.contains_key(model) {
        if let Some(limit) = provider.model_max_input_tokens.get(sibling).copied() {
            provider.model_max_input_tokens.insert(
                model.to_string(),
                window.map_or(limit, |window| limit.min(window)),
            );
        }
    }
    // An output reserve comes off the compaction budget. Capping one at the window
    // would leave no budget at all, and a sibling's reserve that takes over half of
    // this model's window was sized for a bigger model, so it is not inherited.
    if !provider.model_max_output_tokens.contains_key(model) {
        if let Some(limit) = provider
            .model_max_output_tokens
            .get(sibling)
            .copied()
            .filter(|limit| window.is_none_or(|window| limit.saturating_mul(2) <= window))
        {
            provider
                .model_max_output_tokens
                .insert(model.to_string(), limit);
        }
    }

    // Validation requires the default effort to be in the model's own list, so
    // take them together, and only when the model has neither.
    if !provider.model_reasoning_efforts.contains_key(model)
        && !provider.model_default_reasoning_efforts.contains_key(model)
    {
        copy_entry(&mut provider.model_reasoning_efforts, sibling, model);
        copy_entry(
            &mut provider.model_default_reasoning_efforts,
            sibling,
            model,
        );
    }

    for list in [
        &mut provider.preserve_reasoning_content_models,
        &mut provider.no_reasoning_models,
        &mut provider.no_temperature_models,
        &mut provider.no_top_p_models,
        &mut provider.no_penalty_models,
        &mut provider.no_structured_output_models,
        &mut provider.auto_tool_choice_only_models,
        &mut provider.reasoning_split_models,
        &mut provider.thinking_toggle_models,
        &mut provider.thinking_budget_models,
        &mut provider.empty_completion_retry_models,
        &mut provider.terminal_continuation_guard_models,
    ] {
        copy_membership(list, sibling, model);
    }
    if let Some(list) = provider.requires_reasoning_placeholder_models.as_mut() {
        copy_membership(list, sibling, model);
    }
}

#[derive(Deserialize)]
struct PublishedModelLimit {
    context: u64,
    input: Option<u64>,
}

/// Limits for providers whose `/models` reports none, keyed by provider id. This
/// is generated by `scripts/update_published_limits.py`, so refresh it there rather
/// than editing it by hand.
static PUBLISHED_MODEL_LIMITS: LazyLock<BTreeMap<String, BTreeMap<String, PublishedModelLimit>>> =
    LazyLock::new(|| {
        serde_json::from_str(include_str!("published_limits.json")).unwrap_or_default()
    });

static SMALLEST_KNOWN_WINDOWS: LazyLock<BTreeMap<String, u64>> = LazyLock::new(|| {
    let mut smallest = BTreeMap::<String, u64>::new();
    let mut note = |model: &str, window: u64| {
        smallest
            .entry(model.to_ascii_lowercase())
            .and_modify(|known| *known = window.min(*known))
            .or_insert(window);
    };
    for preset in provider_presets() {
        let mut provider = ProviderDefinition {
            id: preset.id.into(),
            protocol: preset.protocol,
            ..ProviderDefinition::default()
        };
        apply_registry_defaults(&mut provider);
        for (model, window) in &provider.model_context_windows {
            note(model, *window);
        }
    }
    // Some tables belong to a provider that has no preset.
    for limits in PUBLISHED_MODEL_LIMITS.values() {
        for (model, limit) in limits {
            note(model, limit.context);
        }
    }
    smallest
});

/// The smallest window any preset or published table states for this exact model id,
/// ignoring case.
pub(crate) fn smallest_known_context_window(model_id: &str) -> Option<u64> {
    SMALLEST_KNOWN_WINDOWS
        .get(&model_id.to_ascii_lowercase())
        .copied()
}

/// Fills only what the registry left open, so its own rows always win.
/// `backfill_registry_input_limits` carries these into saved settings for the
/// models a provider actually lists.
fn insert_published_limits(provider: &mut ProviderDefinition) {
    // `opencode-free` is Zen's endpoint without a key, so it serves Zen's `-free`
    // models. `account_usage` accepts both spellings of Command Code.
    let table = match provider.id.as_str() {
        "opencode-free" => "opencode-zen",
        "commandcode" => "command-code",
        id => id,
    };
    let Some(limits) = PUBLISHED_MODEL_LIMITS.get(table) else {
        return;
    };
    for (model, limit) in limits {
        let window = *provider
            .model_context_windows
            .entry(model.clone())
            .or_insert(limit.context);
        if let Some(input) = limit.input {
            provider
                .model_max_input_tokens
                .entry(model.clone())
                .or_insert(input.min(window));
        }
    }
}

/// The candidate that a new `model_id` most likely behaves like: the same family
/// and suffix, the same major version, and the newest minor version not above its
/// own. `deepseek-v4.1-flash` matches `deepseek-v4-flash`, but not
/// `deepseek-v4-pro` or `deepseek-v3-flash`.
fn closest_older_sibling<'a>(
    candidates: impl Iterator<Item = &'a str>,
    model_id: &str,
) -> Option<&'a str> {
    let target = parse_model_version(model_id)?;
    candidates
        .filter_map(|candidate| {
            let other = parse_model_version(candidate)?;
            (other.family == target.family
                && other.suffix == target.suffix
                && other.major == target.major
                && other.minor <= target.minor)
                .then_some((other.minor, candidate))
        })
        .max_by_key(|(minor, _)| *minor)
        .map(|(_, candidate)| candidate)
}

struct ModelVersion<'a> {
    family: &'a str,
    major: u32,
    minor: u32,
    suffix: &'a str,
}

/// Splits `deepseek-v4.1-flash` into family `deepseek-v`, version 4.1 and suffix
/// `-flash`. A dotted segment wins over an integer one, so the `0309` in
/// `grok-4.20-0309-reasoning` stays part of the suffix.
fn parse_model_version(model_id: &str) -> Option<ModelVersion<'_>> {
    let mut first_integer = None;
    let mut start = 0;
    for segment in model_id.split('-') {
        let end = start + segment.len();
        if let Some((digits_at, major, minor)) = parse_version_segment(segment) {
            let version = ModelVersion {
                family: &model_id[..start + digits_at],
                major,
                minor: minor.unwrap_or(0),
                suffix: &model_id[end..],
            };
            if minor.is_some() {
                return Some(version);
            }
            first_integer = first_integer.or(Some(version));
        }
        start = end + 1;
    }
    first_integer
}

/// Parses a segment such as `4.1`, `v4` or `qwen3.8` into the offset of its first
/// digit and its version. `/` is allowed for ids like `qwen/qwen3.8`.
fn parse_version_segment(segment: &str) -> Option<(usize, u32, Option<u32>)> {
    let digits_at = segment.find(|character: char| character.is_ascii_digit())?;
    let (name, version) = segment.split_at(digits_at);
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphabetic() || byte == b'/')
    {
        return None;
    }
    let (major, minor) = match version.split_once('.') {
        Some((major, minor)) => (parse_digits(major)?, Some(parse_digits(minor)?)),
        None => (parse_digits(version)?, None),
    };
    Some((digits_at, major, minor))
}

fn parse_digits(value: &str) -> Option<u32> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn insert_limits(target: &mut BTreeMap<String, u64>, values: &[(&str, u64)]) {
    for (model, limit) in values {
        target.insert((*model).into(), *limit);
    }
}

fn set_model_modalities(provider: &mut ProviderDefinition, model: &str, values: &[&str]) {
    provider
        .model_input_modalities
        .insert(model.into(), strings(values));
    if values.contains(&"image") {
        provider.capabilities.vision = true;
    }
}

fn set_efforts(provider: &mut ProviderDefinition, models: &[&str], efforts: &[&str]) {
    for model in models {
        provider
            .model_reasoning_efforts
            .insert((*model).into(), strings(efforts));
    }
    if !models.is_empty() && !efforts.is_empty() {
        provider.capabilities.reasoning = true;
    }
}

fn set_wire_map(provider: &mut ProviderDefinition, models: &[&str], values: &[(&str, &str)]) {
    let mapping = values
        .iter()
        .map(|(effort, wire)| ((*effort).to_string(), (*wire).to_string()))
        .collect::<BTreeMap<_, _>>();
    for model in models {
        provider
            .model_reasoning_effort_map
            .insert((*model).into(), mapping.clone());
    }
}

pub fn provider_presets() -> Vec<ProviderPreset> {
    let mut presets = vec![
        forward_preset("openai", "OpenAI (Codex login)", "Forward a caller-owned Codex login through a strict header allowlist", "https://chatgpt.com/backend-api/codex", ProviderProtocol::Responses),
        advanced_preset("openai-api", "OpenAI API", "Native Responses API", "https://api.openai.com/v1", ProviderProtocol::Responses, Some("OPENAI_API_KEY")),
        advanced_preset("openai-apikey", "OpenAI API (legacy ID)", "Native Responses API", "https://api.openai.com/v1", ProviderProtocol::Responses, Some("OPENAI_API_KEY")),
        native_preset("anthropic", "Anthropic", "Native Messages API", "https://api.anthropic.com/v1", ProviderProtocol::AnthropicMessages, "ANTHROPIC_API_KEY", CredentialTransport::XApiKey),
        native_preset("anthropic-apikey", "Anthropic (API key)", "Native Messages API", "https://api.anthropic.com/v1", ProviderProtocol::AnthropicMessages, "ANTHROPIC_API_KEY", CredentialTransport::XApiKey),
        native_preset("google", "Google Gemini", "Native generateContent API", "https://generativelanguage.googleapis.com/v1beta", ProviderProtocol::GeminiGenerateContent, "GEMINI_API_KEY", CredentialTransport::CustomHeader),
        native_preset("google-antigravity", "Google Antigravity", "Cloud Code Assist envelope using an externally brokered OAuth access token", "https://daily-cloudcode-pa.googleapis.com", ProviderProtocol::GeminiGenerateContent, "GOOGLE_ANTIGRAVITY_ACCESS_TOKEN", CredentialTransport::Bearer),
        no_discovery_preset("kiro", "Kiro", "Native CodeWhisperer event-stream transport using a user-owned Kiro token", "https://runtime.us-east-1.kiro.dev", ProviderProtocol::Responses, Some("KIRO_ACCESS_TOKEN")),
        preset("xai", "xAI", "OpenAI-compatible API", "https://api.x.ai/v1", ProviderProtocol::ChatCompletions, Some("XAI_API_KEY")),
        advanced_preset("meta", "Meta Model API", "OpenAI-compatible Muse Spark Chat Completions API", "https://api.meta.ai/v1", ProviderProtocol::ChatCompletions, Some("META_MODEL_API_KEY")),
        preset("openrouter", "OpenRouter", "Multi-provider OpenAI-compatible router", "https://openrouter.ai/api/v1", ProviderProtocol::ChatCompletions, Some("OPENROUTER_API_KEY")),
        preset("deepseek", "DeepSeek", "DeepSeek API", "https://api.deepseek.com", ProviderProtocol::ChatCompletions, Some("DEEPSEEK_API_KEY")),
        preset("mistral", "Mistral", "Mistral API", "https://api.mistral.ai/v1", ProviderProtocol::ChatCompletions, Some("MISTRAL_API_KEY")),
        preset("opendesign", "OpenDesign Cloud", "OpenDesign Cloud model API (Go/Plus/Pro/Max plans); sign in to add accounts and see plan usage", "https://amr-link.open-design.ai/v1", ProviderProtocol::ChatCompletions, Some("OPENDESIGN_API_KEY")),
        ProviderPreset {
            allow_private_network: true,
            ..no_discovery_preset("deepai", "DeepAI Code", "Running DeepAI Code app's loopback model transport; CODETAS imports its current port, token, and models", "http://127.0.0.1:60011", ProviderProtocol::ChatCompletions, None)
        },
        preset("groq", "Groq", "Groq OpenAI-compatible API", "https://api.groq.com/openai/v1", ProviderProtocol::ChatCompletions, Some("GROQ_API_KEY")),
        preset("together", "Together AI", "Together OpenAI-compatible API", "https://api.together.xyz/v1", ProviderProtocol::ChatCompletions, Some("TOGETHER_API_KEY")),
        preset("fireworks", "Fireworks AI", "Fireworks inference API", "https://api.fireworks.ai/inference/v1", ProviderProtocol::ChatCompletions, Some("FIREWORKS_API_KEY")),
        preset("cerebras", "Cerebras", "Cerebras inference API", "https://api.cerebras.ai/v1", ProviderProtocol::ChatCompletions, Some("CEREBRAS_API_KEY")),
        preset("nvidia", "NVIDIA NIM", "NVIDIA hosted inference API", "https://integrate.api.nvidia.com/v1", ProviderProtocol::ChatCompletions, Some("NVIDIA_API_KEY")),
        preset("huggingface", "Hugging Face", "Hugging Face inference router", "https://router.huggingface.co/v1", ProviderProtocol::ChatCompletions, Some("HF_TOKEN")),
        preset("siliconflow", "SiliconFlow", "SiliconFlow OpenAI-compatible API", "https://api.siliconflow.com/v1", ProviderProtocol::ChatCompletions, Some("SILICONFLOW_API_KEY")),
        preset("moonshot", "Moonshot", "Moonshot OpenAI-compatible API", "https://api.moonshot.ai/v1", ProviderProtocol::ChatCompletions, Some("MOONSHOT_API_KEY")),
        preset("kimi", "Kimi", "Kimi Coding API; OAuth may be supplied by an external token broker", "https://api.kimi.com/coding/v1", ProviderProtocol::ChatCompletions, Some("KIMI_API_KEY")),
        preset("kimi-code", "Kimi Coding", "Kimi Coding API-key route", "https://api.kimi.com/coding/v1", ProviderProtocol::ChatCompletions, Some("KIMI_API_KEY")),
        preset("minimax", "MiniMax", "MiniMax OpenAI-compatible API", "https://api.minimax.io/v1", ProviderProtocol::ChatCompletions, Some("MINIMAX_API_KEY")),
        preset("qwen", "Qwen International", "Alibaba Model Studio compatible API", "https://dashscope-intl.aliyuncs.com/compatible-mode/v1", ProviderProtocol::ChatCompletions, Some("DASHSCOPE_API_KEY")),
        preset("qwen-cloud", "Qwen Cloud", "Qwen Cloud token-plan endpoint", "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1", ProviderProtocol::ChatCompletions, Some("QWEN_API_KEY")),
        advanced_preset("perplexity", "Perplexity", "OpenAI-compatible Agent and Responses API", "https://api.perplexity.ai/v1", ProviderProtocol::Responses, Some("PERPLEXITY_API_KEY")),
        preset("cohere", "Cohere", "Cohere OpenAI compatibility API", "https://api.cohere.ai/compatibility/v1", ProviderProtocol::ChatCompletions, Some("COHERE_API_KEY")),
        advanced_preset("sambanova", "SambaNova", "SambaCloud Responses API", "https://api.sambanova.ai/v1", ProviderProtocol::Responses, Some("SAMBANOVA_API_KEY")),
        no_discovery_preset("github-models", "GitHub Models", "GitHub Models inference API", "https://models.github.ai/inference", ProviderProtocol::ChatCompletions, Some("GITHUB_TOKEN")),
        no_discovery_preset("github-copilot", "GitHub Copilot", "Transiently exchanges a user-owned GitHub token for a short-lived Copilot session token", "https://api.githubcopilot.com", ProviderProtocol::ChatCompletions, Some("GITHUB_TOKEN")),
        preset("deepinfra", "DeepInfra", "OpenAI-compatible inference API", "https://api.deepinfra.com/v1/openai", ProviderProtocol::ChatCompletions, Some("DEEPINFRA_API_KEY")),
        preset("hyperbolic", "Hyperbolic", "OpenAI-compatible inference API", "https://api.hyperbolic.xyz/v1", ProviderProtocol::ChatCompletions, Some("HYPERBOLIC_API_KEY")),
        preset("firepass", "Fire Pass", "Fireworks Kimi plan endpoint", "https://api.fireworks.ai/inference/v1", ProviderProtocol::ChatCompletions, Some("FIREWORKS_API_KEY")),
        preset("venice", "Venice", "Venice OpenAI-compatible API", "https://api.venice.ai/api/v1", ProviderProtocol::ChatCompletions, Some("VENICE_API_KEY")),
        preset("zai", "Z.AI GLM Coding Plan", "Z.AI coding subscription endpoint", "https://api.z.ai/api/coding/paas/v4", ProviderProtocol::ChatCompletions, Some("ZAI_API_KEY")),
        preset("zhipu-bigmodel", "Zhipu AI BigModel", "BigModel OpenAI-compatible API", "https://open.bigmodel.cn/api/paas/v4", ProviderProtocol::ChatCompletions, Some("ZHIPU_API_KEY")),
        preset("nanogpt", "NanoGPT", "NanoGPT OpenAI-compatible API", "https://nano-gpt.com/api/v1", ProviderProtocol::ChatCompletions, Some("NANOGPT_API_KEY")),
        preset("synthetic", "Synthetic", "Synthetic OpenAI-compatible API", "https://api.synthetic.new/openai/v1", ProviderProtocol::ChatCompletions, Some("SYNTHETIC_API_KEY")),
        preset("tencent-coding-plan", "Tencent Cloud Coding Plan", "Tencent coding endpoint", "https://api.lkeap.cloud.tencent.com/coding/v3", ProviderProtocol::ChatCompletions, Some("TENCENT_CODING_API_KEY")),
        preset("volcengine", "Volcengine Ark", "Volcengine Ark OpenAI-compatible endpoint", "https://ark.cn-beijing.volces.com/api/v3", ProviderProtocol::ChatCompletions, Some("VOLCENGINE_API_KEY")),
        preset("volcengine-coding-plan", "Volcengine Ark Coding Plan", "Volcengine coding-plan endpoint", "https://ark.cn-beijing.volces.com/api/coding/v3", ProviderProtocol::ChatCompletions, Some("VOLCENGINE_API_KEY")),
        advanced_preset("volcengine-agent-plan", "Volcengine Ark Agent Plan", "Volcengine Responses-compatible agent-plan endpoint", "https://ark.cn-beijing.volces.com/api/plan/v3", ProviderProtocol::Responses, Some("VOLCENGINE_API_KEY")),
        preset("qianfan", "Qianfan", "Baidu Qianfan OpenAI-compatible API", "https://qianfan.baidubce.com/v2", ProviderProtocol::ChatCompletions, Some("QIANFAN_API_KEY")),
        preset("alibaba", "Alibaba Coding Plan", "Alibaba international coding endpoint", "https://coding-intl.dashscope.aliyuncs.com/v1", ProviderProtocol::ChatCompletions, Some("DASHSCOPE_API_KEY")),
        preset("alibaba-token-plan", "Alibaba Token Plan (Beijing)", "Alibaba Beijing token-plan endpoint", "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1", ProviderProtocol::ChatCompletions, Some("DASHSCOPE_API_KEY")),
        preset("alibaba-token-plan-intl", "Alibaba Token Plan (International)", "Alibaba international token-plan endpoint", "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1", ProviderProtocol::ChatCompletions, Some("DASHSCOPE_API_KEY")),
        preset("parallel", "Parallel", "Parallel OpenAI-compatible endpoint", "https://platform.parallel.ai", ProviderProtocol::ChatCompletions, Some("PARALLEL_API_KEY")),
        preset("zenmux", "ZenMux", "ZenMux multi-provider endpoint", "https://zenmux.ai/api/v1", ProviderProtocol::ChatCompletions, Some("ZENMUX_API_KEY")),
        preset("ollama-cloud", "Ollama Cloud", "Ollama hosted OpenAI-compatible API", "https://ollama.com/v1", ProviderProtocol::ChatCompletions, Some("OLLAMA_API_KEY")),
        preset("minimax-cn", "MiniMax Coding Plan (CN)", "MiniMax China endpoint", "https://api.minimaxi.com/v1", ProviderProtocol::ChatCompletions, Some("MINIMAX_API_KEY")),
        preset("opencode-zen", "OpenCode Zen", "OpenCode Zen provider API", "https://opencode.ai/zen/v1", ProviderProtocol::ChatCompletions, Some("OPENCODE_API_KEY")),
        preset("opencode-go", "OpenCode Go", "OpenCode Go provider API", "https://opencode.ai/zen/go/v1", ProviderProtocol::ChatCompletions, Some("OPENCODE_API_KEY")),
        preset("opencode-free", "OpenCode Free", "Key-optional OpenCode free endpoint", "https://opencode.ai/zen/v1", ProviderProtocol::ChatCompletions, None),
        native_preset("xiaomi", "Xiaomi MiMo", "Native Anthropic-compatible MiMo endpoint", "https://api.xiaomimimo.com/anthropic", ProviderProtocol::AnthropicMessages, "XIAOMI_API_KEY", CredentialTransport::Bearer),
        preset("mimo-free", "MiMo Free", "Key-optional MiMo chat endpoint", "https://api.xiaomimimo.com/api/free-ai/openai/chat", ProviderProtocol::ChatCompletions, None),
        preset("kilo", "Kilo", "Kilo AI gateway", "https://api.kilo.ai/api/gateway", ProviderProtocol::ChatCompletions, Some("KILO_API_KEY")),
        preset("neuralwatt", "Neuralwatt Cloud", "Neuralwatt OpenAI-compatible API", "https://api.neuralwatt.com/v1", ProviderProtocol::ChatCompletions, Some("NEURALWATT_API_KEY")),
        preset("orcarouter", "OrcaRouter", "Multi-provider OpenAI-compatible router", "https://api.orcarouter.ai/v1", ProviderProtocol::ChatCompletions, Some("ORCAROUTER_API_KEY")),
        preset("bizrouter", "BizRouter", "Enterprise OpenAI-compatible router", "https://api.bizrouter.ai/v1", ProviderProtocol::ChatCompletions, Some("BIZROUTER_API_KEY")),
        native_preset("umans", "Umans AI Coding Plan", "Anthropic-compatible coding endpoint", "https://api.code.umans.ai", ProviderProtocol::AnthropicMessages, "UMANS_API_KEY", CredentialTransport::Bearer),
        custom_preset("cloudflare-ai-gateway", "Cloudflare AI Gateway", "Account and gateway-specific Anthropic endpoint", ProviderProtocol::AnthropicMessages, Some("CLOUDFLARE_API_TOKEN")),
        preset("gitlab-duo", "GitLab Duo", "GitLab OpenAI-compatible proxy", "https://cloud.gitlab.com/ai/v1/proxy/openai/v1", ProviderProtocol::ChatCompletions, Some("GITLAB_TOKEN")),
        local_preset("ollama", "Ollama", "Local Ollama OpenAI-compatible API", "http://127.0.0.1:11434/v1"),
        local_preset("lm-studio", "LM Studio (legacy ID)", "Local LM Studio OpenAI-compatible API", "http://127.0.0.1:1234/v1"),
        local_preset("lmstudio", "LM Studio", "Local LM Studio OpenAI-compatible API", "http://127.0.0.1:1234/v1"),
        local_preset("litellm", "LiteLLM", "Self-hosted LiteLLM gateway", "http://127.0.0.1:4000/v1"),
        local_preset("vllm", "vLLM", "Local vLLM OpenAI-compatible server", "http://127.0.0.1:8000/v1"),
        local_preset("localai", "LocalAI", "LocalAI OpenAI-compatible server", "http://127.0.0.1:8080/v1"),
        local_preset("llamacpp", "llama.cpp", "llama.cpp OpenAI-compatible server", "http://127.0.0.1:8080/v1"),
        local_preset("sglang", "SGLang", "Local SGLang OpenAI-compatible server", "http://127.0.0.1:30000/v1"),
        local_preset("jan", "Jan", "Local Jan OpenAI-compatible server", "http://127.0.0.1:1337/v1"),
        local_preset("koboldcpp", "KoboldCpp", "Local KoboldCpp OpenAI-compatible server", "http://127.0.0.1:5001/v1"),
        local_preset("textgen-webui", "Text generation web UI", "Local OpenAI-compatible extension", "http://127.0.0.1:5000/v1"),
        local_preset("llamafile", "llamafile", "Local llamafile OpenAI-compatible server", "http://127.0.0.1:8080/v1"),
        custom_preset("azure-openai", "Azure OpenAI", "Azure resource endpoint", ProviderProtocol::Responses, Some("AZURE_OPENAI_API_KEY")),
        custom_preset("azure-ai-foundry", "Azure AI Foundry", "Resource-specific OpenAI-compatible endpoint", ProviderProtocol::Responses, Some("AZURE_AI_FOUNDRY_API_KEY")),
        custom_preset("aws-bedrock", "Amazon Bedrock", "Account and region-specific compatibility endpoint", ProviderProtocol::ChatCompletions, Some("AWS_BEDROCK_API_KEY")),
        native_preset("google-vertex", "Google Vertex AI", "Project/location Vertex Gemini endpoint", "https://aiplatform.googleapis.com", ProviderProtocol::GeminiGenerateContent, "GOOGLE_VERTEX_ACCESS_TOKEN", CredentialTransport::Bearer),
        custom_preset("databricks", "Databricks Model Serving", "Workspace-specific OpenAI-compatible endpoint", ProviderProtocol::ChatCompletions, Some("DATABRICKS_TOKEN")),
        custom_preset("snowflake-cortex", "Snowflake Cortex", "Account-specific OpenAI-compatible endpoint", ProviderProtocol::ChatCompletions, Some("SNOWFLAKE_TOKEN")),
        custom_preset("baseten", "Baseten", "Deployment-specific OpenAI-compatible endpoint", ProviderProtocol::ChatCompletions, Some("BASETEN_API_KEY")),
        custom_preset("vercel-ai-gateway", "Vercel AI Gateway", "Team-specific OpenAI-compatible gateway", ProviderProtocol::ChatCompletions, Some("AI_GATEWAY_API_KEY")),
        custom_preset("portkey", "Portkey", "Workspace-specific AI gateway endpoint", ProviderProtocol::ChatCompletions, Some("PORTKEY_API_KEY")),
        custom_preset("generic-openai", "Custom OpenAI-compatible", "Any reviewed OpenAI-compatible endpoint", ProviderProtocol::ChatCompletions, Some("PROVIDER_API_KEY")),
        custom_preset("cloudflare-workers-ai", "Cloudflare Workers AI", "Account-specific Workers AI endpoint", ProviderProtocol::ChatCompletions, Some("CLOUDFLARE_API_TOKEN")),
    ];
    for preset in &mut presets {
        match preset.id {
            "openai" | "openai-api" | "openai-apikey" => {
                preset.capabilities.web_search = true;
                preset.capabilities.image_generation = true;
                preset.capabilities.realtime = true;
            }
            "xai" => {
                preset.capabilities.image_generation = true;
                preset.capabilities.video_generation = true;
            }
            "perplexity" => preset.capabilities.web_search = true,
            _ => {}
        }
    }
    presets
}

fn preset(
    id: &'static str,
    name: &'static str,
    description: &'static str,
    base_url: &'static str,
    protocol: ProviderProtocol,
    api_key_env: Option<&'static str>,
) -> ProviderPreset {
    ProviderPreset {
        id,
        name,
        description,
        base_url,
        protocol,
        api_key_env,
        credential_source: if api_key_env.is_some() {
            CredentialSource::Environment
        } else {
            CredentialSource::None
        },
        credential_transport: CredentialTransport::Bearer,
        allow_private_network: false,
        discovery: true,
        requires_custom_url: false,
        capabilities: registry_capabilities(),
    }
}

fn registry_capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        vision: true,
        structured_output: true,
        custom_tools: true,
        tool_search: true,
        mcp_namespaces: true,
        ..ProviderCapabilities::default()
    }
}

fn forward_preset(
    id: &'static str,
    name: &'static str,
    description: &'static str,
    base_url: &'static str,
    protocol: ProviderProtocol,
) -> ProviderPreset {
    ProviderPreset {
        credential_source: CredentialSource::Forward,
        discovery: false,
        capabilities: ProviderCapabilities {
            vision: true,
            reasoning: true,
            parallel_tools: true,
            ..registry_capabilities()
        },
        ..preset(id, name, description, base_url, protocol, None)
    }
}

fn advanced_preset(
    id: &'static str,
    name: &'static str,
    description: &'static str,
    base_url: &'static str,
    protocol: ProviderProtocol,
    api_key_env: Option<&'static str>,
) -> ProviderPreset {
    ProviderPreset {
        capabilities: ProviderCapabilities {
            vision: true,
            reasoning: true,
            parallel_tools: true,
            ..registry_capabilities()
        },
        ..preset(id, name, description, base_url, protocol, api_key_env)
    }
}

fn no_discovery_preset(
    id: &'static str,
    name: &'static str,
    description: &'static str,
    base_url: &'static str,
    protocol: ProviderProtocol,
    api_key_env: Option<&'static str>,
) -> ProviderPreset {
    ProviderPreset {
        discovery: false,
        ..preset(id, name, description, base_url, protocol, api_key_env)
    }
}

fn native_preset(
    id: &'static str,
    name: &'static str,
    description: &'static str,
    base_url: &'static str,
    protocol: ProviderProtocol,
    api_key_env: &'static str,
    transport: CredentialTransport,
) -> ProviderPreset {
    ProviderPreset {
        credential_transport: transport,
        ..advanced_preset(id, name, description, base_url, protocol, Some(api_key_env))
    }
}

fn local_preset(
    id: &'static str,
    name: &'static str,
    description: &'static str,
    base_url: &'static str,
) -> ProviderPreset {
    ProviderPreset {
        allow_private_network: true,
        capabilities: ProviderCapabilities {
            vision: true,
            reasoning: false,
            parallel_tools: false,
            ..registry_capabilities()
        },
        ..preset(
            id,
            name,
            description,
            base_url,
            ProviderProtocol::ChatCompletions,
            None,
        )
    }
}

fn custom_preset(
    id: &'static str,
    name: &'static str,
    description: &'static str,
    protocol: ProviderProtocol,
    api_key_env: Option<&'static str>,
) -> ProviderPreset {
    ProviderPreset {
        requires_custom_url: true,
        discovery: false,
        // Account-, deployment-, and user-defined compatibility endpoints can
        // differ from the nominal wire protocol. Keep advanced adapter
        // features opt-in until the configured endpoint is conformance-tested.
        capabilities: ProviderCapabilities::default(),
        ..preset(id, name, description, "", protocol, api_key_env)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelMetadata;

    #[test]
    fn response_id_repair_is_disabled_by_default_and_deepseek_explicitly_opts_in() {
        assert!(!ProviderDefinition::default().repair_invalid_response_item_ids);
        assert!(
            !ProviderDefinition::default()
                .response_item_id_repair
                .repair_missing_terminal_ids
        );

        let deepseek = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "deepseek")
            .unwrap()
            .instantiate(None)
            .unwrap();
        assert!(deepseek.repair_invalid_response_item_ids);
        assert!(deepseek.response_item_id_repair.repair_missing_terminal_ids);
    }

    #[test]
    fn deepseek_api_exposes_v41_flash_with_current_model_id() {
        let deepseek = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "deepseek")
            .unwrap()
            .instantiate(None)
            .unwrap();

        assert_eq!(deepseek.default_model.as_deref(), Some("deepseek-flash"));
        assert!(deepseek
            .models
            .iter()
            .any(|model| model == "deepseek-flash"));
        assert_eq!(
            deepseek.model_protocols.get("deepseek-flash"),
            Some(&ProviderProtocol::Responses)
        );
        assert_eq!(
            deepseek
                .model_context_windows
                .get("deepseek-flash")
                .copied(),
            Some(1_000_000)
        );
        assert!(deepseek.capabilities.vision);
        assert_eq!(
            deepseek.model_input_modalities.get("deepseek-flash"),
            Some(&vec!["text".into(), "image".into()])
        );
        assert!(ProviderCapabilities::default().vision);
        assert_eq!(
            deepseek.model_reasoning_efforts.get("deepseek-flash"),
            Some(&vec!["high".into(), "xhigh".into(), "max".into()])
        );
    }

    #[test]
    fn migrates_existing_deepseek_api_to_v41_flash_without_changing_default() {
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "deepseek")
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.default_model = Some("deepseek-v4-flash".into());
        provider.models.retain(|model| model != "deepseek-flash");
        provider
            .no_structured_output_models
            .retain(|model| model != "deepseek-flash");
        provider.capabilities.vision = false;
        provider.model_input_modalities.clear();
        provider.model_protocols.remove("deepseek-flash");
        let mut settings = GatewaySettings {
            registry_revision: DEEPSEEK_V41_FLASH_MODEL_REVISION - 1,
            providers: vec![provider],
            model_catalog: vec![ModelMetadata {
                provider_id: "deepseek".into(),
                model_id: "deepseek-flash".into(),
                input_modalities: vec!["text".into()],
                capabilities: ProviderCapabilities {
                    vision: false,
                    ..ProviderCapabilities::default()
                },
                ..ModelMetadata::default()
            }],
            ..GatewaySettings::default()
        };
        settings.catalog.selected_models = vec!["deepseek/deepseek-v4-flash".into()];

        assert!(backfill_registry_input_limits(&mut settings));
        let provider = &settings.providers[0];
        assert_eq!(provider.default_model.as_deref(), Some("deepseek-v4-flash"));
        assert!(provider
            .models
            .iter()
            .any(|model| model == "deepseek-flash"));
        assert!(provider
            .no_structured_output_models
            .iter()
            .any(|model| model == "deepseek-flash"));
        assert!(provider.capabilities.vision);
        assert_eq!(
            provider.model_input_modalities.get("deepseek-flash"),
            Some(&vec!["text".into(), "image".into()])
        );
        let metadata = &settings.model_catalog[0];
        assert!(metadata.capabilities.vision);
        assert!(metadata
            .input_modalities
            .iter()
            .any(|modality| modality == "image"));
        assert_eq!(
            provider.model_protocols.get("deepseek-flash"),
            Some(&ProviderProtocol::Responses)
        );
        assert!(settings
            .catalog
            .selected_models
            .iter()
            .any(|model| model == "deepseek/deepseek-flash"));
        assert_eq!(settings.registry_revision, REGISTRY_REVISION);
    }

    #[test]
    fn openai_request_budget_presets_and_migration() {
        for id in ["openai", "openai-api", "openai-apikey"] {
            let provider = provider_presets()
                .into_iter()
                .find(|preset| preset.id == id)
                .unwrap()
                .instantiate(None)
                .unwrap();
            assert_eq!(provider.limits.max_request_bytes, OPENAI_MAX_REQUEST_BYTES);
            for old_limit in [8, 16, 32, 64].map(|mib| mib * 1024 * 1024) {
                let mut provider = provider.clone();
                provider.limits.max_request_bytes = old_limit;
                let mut settings = GatewaySettings {
                    registry_revision: OPENAI_REQUEST_BUDGET_REVISION - 1,
                    providers: vec![provider],
                    ..GatewaySettings::default()
                };
                // Exercise the real persisted-settings loading path.
                let (migrated, changed) = crate::config::parse_gateway_settings_json(
                    &serde_json::to_vec(&settings).unwrap(),
                )
                .unwrap();
                assert!(changed);
                assert_eq!(
                    migrated.providers[0].limits.max_request_bytes,
                    if old_limit == 16 * 1024 * 1024 {
                        OPENAI_MAX_REQUEST_BYTES
                    } else {
                        old_limit
                    }
                );
                let (again, changed) = crate::config::parse_gateway_settings_json(
                    &serde_json::to_vec(&migrated).unwrap(),
                )
                .unwrap();
                assert!(!changed);
                assert_eq!(
                    again.providers[0].limits.max_request_bytes,
                    migrated.providers[0].limits.max_request_bytes
                );
                // An explicit cap saved after migration must remain authoritative.
                settings.registry_revision = REGISTRY_REVISION;
                backfill_registry_input_limits(&mut settings);
                assert_eq!(settings.providers[0].limits.max_request_bytes, old_limit);
            }
        }
        let mut settings = GatewaySettings {
            registry_revision: OPENAI_REQUEST_BUDGET_REVISION - 1,
            providers: vec![ProviderDefinition {
                id: "custom".into(),
                ..ProviderDefinition::default()
            }],
            ..GatewaySettings::default()
        };
        backfill_registry_input_limits(&mut settings);
        assert_eq!(
            settings.providers[0].limits.max_request_bytes,
            16 * 1024 * 1024
        );
    }

    #[test]
    fn codex_login_models_have_explicit_input_contracts() {
        let provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "openai")
            .unwrap()
            .instantiate(None)
            .unwrap();

        assert_eq!(
            provider.model_context_windows.get("gpt-6-astra").copied(),
            Some(1_050_000)
        );
        assert_eq!(
            provider.model_max_input_tokens.get("gpt-6-astra").copied(),
            Some(922_000)
        );
        assert_eq!(
            provider.model_max_output_tokens.get("gpt-6-astra").copied(),
            Some(128_000)
        );
        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            assert_eq!(
                provider.model_context_windows.get(model).copied(),
                Some(372_000)
            );
            assert_eq!(
                provider.model_max_input_tokens.get(model).copied(),
                Some(272_000)
            );
            assert_eq!(provider.model_max_output_tokens.get(model), None);
        }
    }

    #[test]
    fn migrates_gpt6_astra_into_existing_openai_providers() {
        for preset_id in ["openai", "openai-api", "openai-apikey"] {
            let mut provider = provider_presets()
                .into_iter()
                .find(|preset| preset.id == preset_id)
                .unwrap()
                .instantiate(None)
                .unwrap();
            provider.models.retain(|model| model != "gpt-6-astra");
            provider
                .service_tier_models
                .retain(|model| model != "gpt-6-astra");
            provider.model_context_windows.remove("gpt-6-astra");
            provider.model_max_input_tokens.remove("gpt-6-astra");
            provider.model_max_output_tokens.remove("gpt-6-astra");
            provider.model_input_modalities.remove("gpt-6-astra");
            provider.model_reasoning_efforts.remove("gpt-6-astra");
            let original_default = provider.default_model.clone();
            let mut settings = GatewaySettings {
                registry_revision: GPT6_ASTRA_MODEL_REVISION - 1,
                providers: vec![provider],
                ..GatewaySettings::default()
            };

            assert!(backfill_registry_input_limits(&mut settings));
            let provider = &settings.providers[0];
            assert_eq!(
                provider.models.first().map(String::as_str),
                Some("gpt-6-astra")
            );
            assert!(provider
                .service_tier_models
                .iter()
                .any(|model| model == "gpt-6-astra"));
            assert_eq!(
                provider.model_context_windows.get("gpt-6-astra").copied(),
                Some(1_050_000)
            );
            assert_eq!(
                provider.model_max_input_tokens.get("gpt-6-astra").copied(),
                Some(922_000)
            );
            assert_eq!(
                provider.model_max_output_tokens.get("gpt-6-astra").copied(),
                Some(128_000)
            );
            assert_eq!(provider.default_model, original_default);
        }
    }

    #[test]
    fn anthropic_request_budget_presets_and_migration() {
        for id in ["anthropic", "anthropic-apikey"] {
            let provider = provider_presets()
                .into_iter()
                .find(|preset| preset.id == id)
                .unwrap()
                .instantiate(None)
                .unwrap();
            assert_eq!(
                provider.limits.max_request_bytes,
                ANTHROPIC_MAX_REQUEST_BYTES
            );
            for old_limit in [8, 16, 32].map(|mib| mib * 1024 * 1024) {
                let mut provider = provider.clone();
                provider.limits.max_request_bytes = old_limit;
                let settings = GatewaySettings {
                    registry_revision: ANTHROPIC_REQUEST_BUDGET_REVISION - 1,
                    providers: vec![provider],
                    ..GatewaySettings::default()
                };
                let (migrated, changed) = crate::config::parse_gateway_settings_json(
                    &serde_json::to_vec(&settings).unwrap(),
                )
                .unwrap();
                assert!(changed);
                assert_eq!(
                    migrated.providers[0].limits.max_request_bytes,
                    if old_limit == 16 * 1024 * 1024 {
                        ANTHROPIC_MAX_REQUEST_BYTES
                    } else {
                        old_limit
                    }
                );
            }
        }
    }

    #[test]
    fn gives_the_september_anthropic_models_their_real_context_window() {
        for id in ["anthropic", "anthropic-apikey"] {
            let mut provider = ProviderDefinition {
                id: id.into(),
                ..ProviderDefinition::default()
            };
            apply_registry_defaults(&mut provider);
            // The registry ships the window directly, so it must already be
            // right without any migration.
            for (model, window) in ANTHROPIC_EXPLICIT_CONTEXT_MODELS {
                assert_eq!(
                    provider.model_context_windows.get(*model).copied(),
                    Some(*window),
                    "{id}: {model} must ship a 1M window"
                );
            }

            // A settings file written before the fix has no entry and must be
            // repaired, otherwise the catalog has to guess the window.
            for (model, window) in ANTHROPIC_EXPLICIT_CONTEXT_MODELS {
                provider.model_context_windows.remove(*model);
                let mut settings = GatewaySettings {
                    registry_revision: ANTHROPIC_FABLE_OPUS_CONTEXT_REVISION - 1,
                    providers: vec![provider.clone()],
                    ..GatewaySettings::default()
                };
                assert!(backfill_registry_input_limits(&mut settings));
                assert_eq!(
                    settings.providers[0]
                        .model_context_windows
                        .get(*model)
                        .copied(),
                    Some(*window),
                    "{id}: {model} must be repaired"
                );
            }
        }
    }

    #[test]
    fn keeps_a_smaller_user_context_window_for_the_anthropic_models() {
        // A serialized window cannot say whether it is the old 128k default or
        // the user's own choice, so the migration must never overwrite an
        // existing value — including a value that happens to be small.
        for user_value in [8_192u64, 64_000, 128_000, 128_001, 900_000] {
            let mut provider = ProviderDefinition {
                id: "anthropic".into(),
                ..ProviderDefinition::default()
            };
            apply_registry_defaults(&mut provider);
            for (model, _) in ANTHROPIC_EXPLICIT_CONTEXT_MODELS {
                provider.model_context_windows.remove(*model);
            }
            provider
                .model_context_windows
                .insert("claude-opus-5-5".into(), user_value);
            let mut settings = GatewaySettings {
                registry_revision: 15,
                providers: vec![provider],
                ..GatewaySettings::default()
            };

            backfill_registry_input_limits(&mut settings);

            assert_eq!(
                settings.providers[0]
                    .model_context_windows
                    .get("claude-opus-5-5")
                    .copied(),
                Some(user_value),
                "an explicit {user_value} must survive the migration"
            );
        }
    }

    #[test]
    fn fills_only_a_missing_anthropic_context_window() {
        for id in ["anthropic", "anthropic-apikey"] {
            let mut provider = ProviderDefinition {
                id: id.into(),
                ..ProviderDefinition::default()
            };
            apply_registry_defaults(&mut provider);
            // Simulate settings written before the window existed.
            for (model, _) in ANTHROPIC_EXPLICIT_CONTEXT_MODELS {
                provider.model_context_windows.remove(*model);
            }
            let mut settings = GatewaySettings {
                registry_revision: 15,
                providers: vec![provider],
                ..GatewaySettings::default()
            };

            assert!(backfill_registry_input_limits(&mut settings));

            for (model, window) in ANTHROPIC_EXPLICIT_CONTEXT_MODELS {
                assert_eq!(
                    settings.providers[0]
                        .model_context_windows
                        .get(*model)
                        .copied(),
                    Some(*window),
                    "{id}: {model} must be filled when missing"
                );
            }
        }
    }

    #[test]
    fn fills_opencode_limits_for_models_found_by_discovery() {
        for id in ["opencode-go", "opencode-zen"] {
            // No revision gate: discovery can add these at any time, and the
            // settings file already sits at the current revision by then.
            let mut settings = GatewaySettings {
                registry_revision: REGISTRY_REVISION,
                providers: vec![ProviderDefinition {
                    id: id.into(),
                    models: strings(&["deepseek-v4.1-flash", "deepseek-v4.1-flash-free"]),
                    ..ProviderDefinition::default()
                }],
                ..GatewaySettings::default()
            };

            assert!(backfill_registry_input_limits(&mut settings));

            let provider = &settings.providers[0];
            let paid = "deepseek-v4.1-flash";
            assert_eq!(provider.model_context_windows[paid], 1_000_000, "{id}");
            assert_eq!(provider.model_max_input_tokens[paid], 616_000, "{id}");
            if id == "opencode-zen" {
                let free = "deepseek-v4.1-flash-free";
                assert_eq!(provider.model_context_windows[free], 200_000);
                // An input limit here would put the compaction trigger below what
                // the tool definitions and a summary already take.
                assert!(!provider.model_max_input_tokens.contains_key(free));
            }
        }
    }

    #[test]
    fn filling_a_window_never_contradicts_a_larger_saved_limit() {
        for (id, model, saved_input) in [
            // The registry's own window is smaller than the saved limit.
            ("opencode-zen", "deepseek-v4.1-flash-free", 300_000_u64),
            // So is the window inherited from an older sibling.
            ("opencode-go", "deepseek-v4.2-flash", 2_000_000),
        ] {
            let mut provider = provider_presets()
                .into_iter()
                .find(|preset| preset.id == id)
                .unwrap()
                .instantiate(None)
                .unwrap();
            provider.models.push(model.into());
            provider.model_context_windows.remove(model);
            provider
                .model_max_input_tokens
                .insert(model.into(), saved_input);
            let mut settings = GatewaySettings {
                registry_revision: REGISTRY_REVISION,
                providers: vec![provider],
                ..GatewaySettings::default()
            };

            backfill_registry_input_limits(&mut settings);

            let provider = &mut settings.providers[0];
            assert!(!provider.model_context_windows.contains_key(model), "{id}");
            assert_eq!(provider.model_max_input_tokens[model], saved_input);
            provider.validate().unwrap();

            // Discovery must not contradict it either.
            inherit_settings_from_older_sibling(provider, model);
            assert!(!provider.model_context_windows.contains_key(model), "{id}");
            provider.validate().unwrap();
        }
    }

    #[test]
    fn published_limits_parse_and_every_provider_fits_validation() {
        assert!(!PUBLISHED_MODEL_LIMITS.is_empty());
        for (id, limits) in PUBLISHED_MODEL_LIMITS.iter() {
            assert!(!limits.is_empty(), "{id}");
            let mut provider = ProviderDefinition {
                id: id.clone(),
                ..ProviderDefinition::default()
            };
            apply_registry_defaults(&mut provider);
            assert!(provider.model_context_windows.len() <= 250, "{id}");
            assert!(provider.model_max_input_tokens.len() <= 250, "{id}");
            for (model, limit) in &provider.model_max_input_tokens {
                assert!(
                    *limit <= provider.model_context_windows[model],
                    "{id}: {model}"
                );
            }
        }
    }

    #[test]
    fn registry_rows_win_over_published_limits() {
        let mut provider = ProviderDefinition {
            id: "opencode-go".into(),
            ..ProviderDefinition::default()
        };
        apply_registry_defaults(&mut provider);
        assert_eq!(provider.model_context_windows["qwen3.8-max"], 983_616);
        assert_eq!(
            provider.model_max_input_tokens["deepseek-v4.1-flash"],
            616_000
        );
    }

    #[test]
    fn backfills_opencode_deepseek_v41_reasoning_settings_only_once() {
        let model = "deepseek-v4.1-flash";
        for id in ["opencode-go", "opencode-zen"] {
            let mut settings = GatewaySettings {
                registry_revision: OPENCODE_DEEPSEEK_V41_REASONING_REVISION - 1,
                providers: vec![ProviderDefinition {
                    id: id.into(),
                    name: id.into(),
                    base_url: "https://opencode.ai/zen/v1".into(),
                    models: strings(&[model]),
                    ..ProviderDefinition::default()
                }],
                ..GatewaySettings::default()
            };

            assert!(backfill_registry_input_limits(&mut settings));

            let provider = &mut settings.providers[0];
            provider.validate().unwrap();
            if id == "opencode-go" {
                assert_eq!(provider.model_reasoning_effort_map[model]["low"], "high");
                assert_eq!(
                    provider.model_reasoning_efforts[model],
                    strings(DEEPSEEK_EFFORTS)
                );
            } else {
                // Zen does not advertise reasoning, so it gets no effort levels.
                assert!(!provider.model_reasoning_efforts.contains_key(model));
                assert!(!provider.model_reasoning_effort_map.contains_key(model));
            }
            assert!(provider
                .preserve_reasoning_content_models
                .iter()
                .any(|configured| configured == model));

            // A user who turns preservation off afterwards keeps that choice.
            provider.preserve_reasoning_content_models.clear();
            assert!(!backfill_registry_input_limits(&mut settings));
            assert!(settings.providers[0]
                .preserve_reasoning_content_models
                .is_empty());
        }
    }

    // DeepSeek and Kimi can fail without preservation, and a sibling is not always
    // there to copy it from.
    #[test]
    fn a_model_that_first_appears_later_gets_preservation_from_the_preset() {
        let model = "kimi-k3";
        let mut settings = GatewaySettings {
            registry_revision: REGISTRY_REVISION,
            providers: vec![ProviderDefinition {
                id: "opencode-go".into(),
                name: "OpenCode Go".into(),
                base_url: "https://opencode.ai/zen/go/v1".into(),
                models: strings(&[model]),
                ..ProviderDefinition::default()
            }],
            ..GatewaySettings::default()
        };
        backfill_registry_input_limits(&mut settings);
        let provider = &mut settings.providers[0];
        assert!(provider.preserve_reasoning_content_models.is_empty());

        inherit_settings_from_older_sibling(provider, model);
        inherit_settings_from_older_sibling(provider, "not-in-the-preset");

        assert_eq!(provider.preserve_reasoning_content_models, [model]);
        provider.validate().unwrap();
    }

    // The fixtures hold choices no fill may contradict, and a contradiction would stop
    // every provider's settings from loading.
    #[test]
    fn saved_choices_never_make_a_backfill_produce_settings_that_cannot_load() {
        let large = "mistral-large-4-0";
        let medium = "mistral-medium-latest";
        let glm = "zai-glm-5-3";
        let saved = |id: &str, base_url: &str, models: &[&str]| ProviderDefinition {
            id: id.into(),
            name: id.into(),
            base_url: base_url.into(),
            models: strings(models),
            ..ProviderDefinition::default()
        };
        let mistral = saved("mistral", "https://api.mistral.ai/v1", &["my-pick"]);
        let proxied = saved("mistral", "https://proxy.example/v1", &[glm, medium]);
        let go = saved(
            "opencode-go",
            "https://opencode.ai/zen/go/v1",
            &["deepseek-v4.1-flash"],
        );

        // A default effort that the saved list does not contain.
        let mut list_without_high = mistral.clone();
        list_without_high
            .model_reasoning_efforts
            .insert(glm.into(), strings(&["low"]));
        let mut large_without_high = mistral.clone();
        large_without_high
            .model_reasoning_efforts
            .insert(large.into(), strings(&["medium"]));
        let mut proxied_list = proxied.clone();
        proxied_list
            .model_reasoning_efforts
            .insert(glm.into(), strings(&["low"]));
        // A list that does not contain the saved default.
        let mut default_outside_list = mistral.clone();
        default_outside_list
            .model_default_reasoning_efforts
            .insert(medium.into(), "low".into());
        let mut proxied_default = proxied.clone();
        proxied_default
            .model_default_reasoning_efforts
            .insert(medium.into(), "low".into());
        let mut go_default = go.clone();
        go_default
            .model_default_reasoning_efforts
            .insert("deepseek-v4.1-flash".into(), "medium".into());
        // A window smaller than a saved limit.
        let mut input_above_window = mistral.clone();
        input_above_window
            .model_max_input_tokens
            .insert(medium.into(), 2_000_000);
        let mut output_above_window = mistral.clone();
        output_above_window
            .model_max_output_tokens
            .insert(medium.into(), 300_000);

        for provider in [
            list_without_high,
            large_without_high,
            proxied_list,
            default_outside_list,
            proxied_default,
            go_default,
            input_above_window,
            output_above_window,
        ] {
            let settings = GatewaySettings {
                registry_revision: OPENCODE_DEEPSEEK_V41_REASONING_REVISION - 1,
                providers: vec![provider],
                ..GatewaySettings::default()
            };
            // Runs the backfill, then the validation that decides whether the app loads.
            let (migrated, _) =
                crate::config::parse_gateway_settings_json(&serde_json::to_vec(&settings).unwrap())
                    .unwrap();
            // Nothing was left to do on the next load.
            let mut again = migrated;
            assert!(!backfill_registry_input_limits(&mut again));
        }
    }

    #[test]
    fn saved_free_deepseek_connections_get_reasoning_preservation_once() {
        for (id, model) in [
            ("opencode-free", "deepseek-v4.1-flash-free"),
            ("opencode-zen", "deepseek-v4-flash-free"),
        ] {
            let settings = GatewaySettings {
                registry_revision: OPENCODE_DEEPSEEK_V41_REASONING_REVISION - 1,
                providers: vec![ProviderDefinition {
                    id: id.into(),
                    name: id.into(),
                    base_url: "https://opencode.ai/zen/v1".into(),
                    models: strings(&[model]),
                    ..ProviderDefinition::default()
                }],
                ..GatewaySettings::default()
            };

            let (mut loaded, _) =
                crate::config::parse_gateway_settings_json(&serde_json::to_vec(&settings).unwrap())
                    .unwrap();

            loaded.validate().unwrap();
            assert_eq!(
                loaded.providers[0].preserve_reasoning_content_models,
                [model],
                "{id}"
            );
            if id == "opencode-free" {
                assert_eq!(
                    loaded.providers[0].model_reasoning_efforts[model],
                    strings(DEEPSEEK_EFFORTS)
                );
                assert_eq!(
                    loaded.providers[0].model_reasoning_effort_map[model]["xhigh"],
                    "max"
                );
            }

            // A user who removes it afterwards keeps that choice.
            loaded.providers[0]
                .preserve_reasoning_content_models
                .clear();
            assert!(!backfill_registry_input_limits(&mut loaded));
            assert!(loaded.providers[0]
                .preserve_reasoning_content_models
                .is_empty());
        }
    }

    #[test]
    fn a_text_only_row_waits_while_a_vision_sidecar_needs_the_model() {
        let glm = "zai-glm-5-3";
        let target = format!("mistral/{glm}");
        let provider = ProviderDefinition {
            id: "mistral".into(),
            name: "Mistral".into(),
            base_url: "https://api.mistral.ai/v1".into(),
            models: strings(&[glm]),
            ..ProviderDefinition::default()
        };
        let route = crate::config::RouteDefinition {
            id: "glm-route".into(),
            name: "GLM route".into(),
            enabled: true,
            targets: vec![crate::config::RouteTarget {
                model: target.clone(),
                weight: 1,
            }],
            ..crate::config::RouteDefinition::default()
        };
        let sidecars =
            |vision: Option<&str>, document: Option<&str>| crate::config::SidecarSettings {
                vision_model: vision.map(str::to_string),
                document_model: document.map(str::to_string),
                ..crate::config::SidecarSettings::default()
            };
        for (sidecars, routes) in [
            (sidecars(Some(&target), None), Vec::new()),
            (sidecars(None, Some("glm-route")), vec![route]),
        ] {
            let settings = GatewaySettings {
                registry_revision: MISTRAL_VIBE_PLAN_REVISION - 1,
                providers: vec![provider.clone()],
                routes,
                sidecars,
                ..GatewaySettings::default()
            };
            settings.validate().unwrap();

            let (mut loaded, _) =
                crate::config::parse_gateway_settings_json(&serde_json::to_vec(&settings).unwrap())
                    .unwrap();

            assert!(!loaded.providers[0].model_input_modalities.contains_key(glm));
            // The rest of the migration ran, so this was not the load-time safety net.
            assert_eq!(loaded.registry_revision, REGISTRY_REVISION);
            assert!(loaded.providers[0].prompt_cache_key);
            assert!(!backfill_registry_input_limits(&mut loaded));

            // Nothing needs it to take images any more.
            loaded.sidecars = crate::config::SidecarSettings::default();
            assert!(backfill_registry_input_limits(&mut loaded));
            assert_eq!(loaded.providers[0].model_input_modalities[glm], ["text"]);
            loaded.validate().unwrap();
        }
    }

    #[test]
    fn a_backfill_that_would_break_valid_settings_leaves_them_as_they_were() {
        let model = "deepseek-v4.1-flash";
        let mut provider = ProviderDefinition {
            id: "opencode-go".into(),
            name: "OpenCode Go".into(),
            base_url: "https://opencode.ai/zen/go/v1".into(),
            models: strings(&[model]),
            ..ProviderDefinition::default()
        };
        // Full, so the backfill's row for the listed model is one too many.
        for index in 0..250 {
            provider
                .model_context_windows
                .insert(format!("old-model-{index}"), 100_000);
        }
        let settings = GatewaySettings {
            registry_revision: OPENCODE_DEEPSEEK_V41_REASONING_REVISION - 1,
            providers: vec![provider],
            ..GatewaySettings::default()
        };
        settings.validate().unwrap();

        let (loaded, changed) =
            crate::config::parse_gateway_settings_json(&serde_json::to_vec(&settings).unwrap())
                .unwrap();

        assert!(!changed);
        assert_eq!(loaded.registry_revision, settings.registry_revision);
        assert!(!loaded.providers[0]
            .model_context_windows
            .contains_key(model));
        assert!(loaded.providers[0]
            .preserve_reasoning_content_models
            .is_empty());
        loaded.validate().unwrap();

        // Settings that were already invalid still fail, with the migrated error.
        let mut invalid = settings;
        invalid.providers[0]
            .model_context_windows
            .insert("old-model-0".into(), 0);
        assert!(
            crate::config::parse_gateway_settings_json(&serde_json::to_vec(&invalid).unwrap())
                .is_err()
        );
    }

    // These blocks only run for old settings, and could break them the same way.
    #[test]
    fn older_migrations_also_respect_saved_limits_and_efforts() {
        let preset = |id: &str| {
            provider_presets()
                .into_iter()
                .find(|preset| preset.id == id)
                .unwrap()
                .instantiate(None)
                .unwrap()
        };
        // Revisions 10, 14 and 19 add these OpenAI models with their limits.
        let mut openai = preset("openai-api");
        for model in ["gpt-6-astra", "gpt-6-sol", "gpt-6.1-sol"] {
            openai.model_context_windows.insert(model.into(), 100_000);
            openai.model_max_input_tokens.remove(model);
            openai.model_max_output_tokens.remove(model);
            openai.model_default_reasoning_efforts.remove(model);
        }
        openai.model_reasoning_efforts.remove("gpt-6.1-sol");
        openai
            .model_default_reasoning_efforts
            .insert("gpt-6.1-sol".into(), "ultra".into());
        // Revisions 16 and 18 add these Anthropic windows.
        let mut anthropic = preset("anthropic");
        for model in ["claude-opus-5-5", "claude-sonnet-5-5"] {
            anthropic.model_context_windows.remove(model);
            anthropic
                .model_max_output_tokens
                .insert(model.into(), 2_000_000);
        }
        anthropic
            .model_reasoning_efforts
            .remove("claude-sonnet-5-5");
        anthropic
            .model_default_reasoning_efforts
            .insert("claude-sonnet-5-5".into(), "minimal".into());
        // Revisions 1 and 24 add these Antigravity rows.
        let mut antigravity = preset("google-antigravity");
        antigravity.model_context_windows.remove("gemini-3.7-flash");
        antigravity
            .model_max_input_tokens
            .insert("gemini-3.7-flash".into(), 2_000_000);
        antigravity.model_reasoning_efforts.remove("gemini-4-argon");
        antigravity
            .model_default_reasoning_efforts
            .insert("gemini-4-argon".into(), "max".into());
        antigravity
            .model_reasoning_efforts
            .insert("gemini-3.8-flash".into(), strings(&["low"]));
        antigravity
            .model_default_reasoning_efforts
            .remove("gemini-3.8-flash");
        antigravity.model_context_windows.remove("gemini-4-argon");
        antigravity
            .model_max_output_tokens
            .insert("gemini-4-argon".into(), 2_000_000);

        for provider in [openai, anthropic, antigravity] {
            let mut settings = GatewaySettings {
                registry_revision: 0,
                providers: vec![provider],
                ..GatewaySettings::default()
            };
            backfill_registry_input_limits(&mut settings);
            settings.providers[0].validate().unwrap();
        }
    }

    #[test]
    fn migrating_the_antigravity_sonnet_46_window_changes_only_the_old_default() {
        let model = ANTIGRAVITY_SONNET_46;
        for (saved, expected) in [
            (ANTIGRAVITY_SONNET_46_OLD_WINDOW, None),
            (150_000, Some(150_000)),
        ] {
            let mut provider = provider_presets()
                .into_iter()
                .find(|preset| preset.id == "google-antigravity")
                .unwrap()
                .instantiate(None)
                .unwrap();
            assert!(!provider.model_context_windows.contains_key(model));
            provider.model_context_windows.insert(model.into(), saved);
            let mut settings = GatewaySettings {
                registry_revision: ANTIGRAVITY_SONNET_46_WINDOW_REVISION - 1,
                providers: vec![provider],
                model_catalog: vec![crate::config::ModelMetadata {
                    provider_id: "google-antigravity".into(),
                    model_id: model.into(),
                    enabled: true,
                    context_window: Some(saved),
                    ..crate::config::ModelMetadata::default()
                }],
                ..GatewaySettings::default()
            };

            assert!(backfill_registry_input_limits(&mut settings));

            let provider = &settings.providers[0];
            assert_eq!(provider.model_context_windows.get(model).copied(), expected);
            assert_eq!(settings.model_catalog[0].context_window, expected);
            settings.validate().unwrap();

            // A window the user sets to the same number afterwards is theirs.
            settings.providers[0]
                .model_context_windows
                .insert(model.into(), ANTIGRAVITY_SONNET_46_OLD_WINDOW);
            backfill_registry_input_limits(&mut settings);
            assert_eq!(
                settings.providers[0].model_context_windows.get(model),
                Some(&ANTIGRAVITY_SONNET_46_OLD_WINDOW)
            );
        }
    }

    #[test]
    fn new_versions_inherit_the_window_of_the_closest_older_sibling() {
        let windows = BTreeMap::from([
            ("deepseek-v4-flash".to_string(), 1_000_000_u64),
            ("deepseek-v4.1-flash".to_string(), 900_000),
            ("deepseek-v4-pro".to_string(), 800_000),
            ("deepseek/deepseek-v4-flash".to_string(), 700_000),
            ("qwen3.7-max".to_string(), 600_000),
            ("kimi-k3".to_string(), 500_000),
        ]);
        for (model, expected) in [
            ("deepseek-v4.2-flash", Some(900_000)),
            ("deepseek-v4.1-pro", Some(800_000)),
            ("deepseek/deepseek-v4.1-flash", Some(700_000)),
            ("qwen3.8-max", Some(600_000)),
            // A different suffix or major version is a different model.
            ("deepseek-v4.1-flash-free", None),
            ("deepseek-v5-flash", None),
            ("kimi-k3.1-pro", None),
            // An older version does not inherit from a newer one.
            ("deepseek-v3.9-flash", None),
            ("qwen3.6-max", None),
        ] {
            assert_eq!(
                resolve_model_context_window(&windows, model),
                expected,
                "{model}"
            );
        }
    }

    #[test]
    fn a_new_model_inherits_its_older_siblings_settings() {
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "opencode-go")
            .unwrap()
            .instantiate(None)
            .unwrap();
        let model = "deepseek-v4.2-flash";

        inherit_settings_from_older_sibling(&mut provider, model);

        assert_eq!(provider.model_context_windows[model], 1_000_000);
        assert_eq!(provider.model_max_input_tokens[model], 616_000);
        assert_eq!(
            provider.model_reasoning_efforts[model],
            strings(DEEPSEEK_EFFORTS)
        );
        assert_eq!(provider.model_reasoning_effort_map[model]["low"], "high");
        assert!(provider
            .preserve_reasoning_content_models
            .iter()
            .any(|configured| configured == model));
        provider.validate().unwrap();
    }

    #[test]
    fn a_new_model_takes_nothing_from_versions_older_than_its_closest_sibling() {
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "opencode-go")
            .unwrap()
            .instantiate(None)
            .unwrap();
        // `glm-5` and `glm-5.1` use the thinking toggle, and `glm-5.3` does not.
        assert!(provider
            .thinking_toggle_models
            .contains(&"glm-5.1".to_string()));
        provider.models.push("glm-5.3".into());
        let model = "glm-5.4";

        inherit_settings_from_older_sibling(&mut provider, model);

        assert!(!provider.thinking_toggle_models.contains(&model.to_string()));
        assert!(!provider.model_reasoning_effort_map.contains_key(model));
        provider.validate().unwrap();
    }

    #[test]
    fn inheriting_keeps_existing_values_and_stays_valid() {
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "opencode-go")
            .unwrap()
            .instantiate(None)
            .unwrap();

        let smaller = "deepseek-v4.3-flash";
        provider
            .model_context_windows
            .insert(smaller.into(), 400_000);
        provider
            .model_reasoning_efforts
            .insert(smaller.into(), strings(&["high"]));
        inherit_settings_from_older_sibling(&mut provider, smaller);
        assert_eq!(provider.model_context_windows[smaller], 400_000);
        assert_eq!(provider.model_max_input_tokens[smaller], 400_000);
        assert_eq!(
            provider.model_reasoning_efforts[smaller],
            strings(&["high"])
        );

        let unrelated = "brand-new-model";
        inherit_settings_from_older_sibling(&mut provider, unrelated);
        assert!(!provider.model_context_windows.contains_key(unrelated));
        assert!(!provider.model_reasoning_efforts.contains_key(unrelated));

        provider.validate().unwrap();
    }

    // An output reserve capped at the window would publish a trigger of 1.
    #[test]
    fn a_new_model_skips_an_output_reserve_that_takes_over_half_its_window() {
        // `muse-spark-1.3` reserves 131,072 of its 1,048,576.
        let sibling_output = 131_072_u64;
        let fresh = || {
            provider_presets()
                .into_iter()
                .find(|preset| preset.id == "meta")
                .unwrap()
                .instantiate(None)
                .unwrap()
        };
        let model = "muse-spark-1.4";
        for (saved_window, reported_window, expected) in [
            // Nothing says this model is smaller than its sibling.
            (None, None, Some(sibling_output)),
            // Sane against a smaller window, and exactly half still leaves half.
            (Some(sibling_output * 2), None, Some(sibling_output)),
            (Some(sibling_output * 2 - 1), None, None),
            (Some(sibling_output), None, None),
            (Some(128_000), None, None),
            // Discovery reports a small window that the provider's map does not hold.
            (None, Some(128_000), None),
            (None, Some(sibling_output * 2), Some(sibling_output)),
        ] {
            let mut provider = fresh();
            if let Some(window) = saved_window {
                provider.model_context_windows.insert(model.into(), window);
            }

            inherit_settings_with_reported_window(&mut provider, model, reported_window);

            assert_eq!(
                provider.model_max_output_tokens.get(model).copied(),
                expected,
                "saved {saved_window:?} reported {reported_window:?}"
            );
            provider.validate().unwrap();
        }
    }

    #[test]
    fn a_new_model_inherits_its_siblings_retry_and_continuation_quirks() {
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "moonshot")
            .unwrap()
            .instantiate(None)
            .unwrap();
        let model = "kimi-k3.1";

        inherit_settings_from_older_sibling(&mut provider, model);

        assert!(provider
            .empty_completion_retry_models
            .iter()
            .any(|entry| entry == model));
        assert!(provider
            .terminal_continuation_guard_models
            .iter()
            .any(|entry| entry == model));
        provider.validate().unwrap();
    }

    #[test]
    fn retires_the_anthropic_model_the_api_never_served() {
        for id in ["anthropic", "anthropic-apikey"] {
            // A settings file written by the September refresh still lists the
            // retired ID in every place it could be selected or described.
            let mut provider = ProviderDefinition {
                id: id.into(),
                ..ProviderDefinition::default()
            };
            apply_registry_defaults(&mut provider);
            provider.models.push("claude-mythos-5-1".into());
            provider
                .model_context_windows
                .insert("claude-mythos-5-1".into(), 1_000_000);
            provider
                .model_max_output_tokens
                .insert("claude-mythos-5-1".into(), 32_000);
            provider
                .model_input_modalities
                .insert("claude-mythos-5-1".into(), vec!["text".into()]);
            provider
                .model_reasoning_efforts
                .insert("claude-mythos-5-1".into(), vec!["high".into()]);
            provider
                .model_default_reasoning_efforts
                .insert("claude-mythos-5-1".into(), "high".into());
            provider
                .model_wire_ids
                .insert("claude-mythos-5-1-alias".into(), "claude-mythos-5-1".into());
            provider
                .service_tier_models
                .push("claude-mythos-5-1".into());
            provider
                .no_structured_output_models
                .push("claude-mythos-5-1".into());
            provider
                .preserve_reasoning_content_models
                .push("claude-mythos-5-1".into());

            let mut settings = GatewaySettings {
                registry_revision: RETIRED_ANTHROPIC_MYTHOS_REVISION - 1,
                providers: vec![provider],
                ..GatewaySettings::default()
            };
            // A user selection for the retired model must go too.
            settings.catalog.selected_models = vec![
                "anthropic/claude-mythos-5-1".into(),
                "anthropic/claude-opus-5-5".into(),
            ];

            assert!(backfill_registry_input_limits(&mut settings));

            let provider = &settings.providers[0];
            assert!(
                !provider.models.iter().any(|m| m == "claude-mythos-5-1"),
                "{id}: retired model still listed"
            );
            assert!(!provider
                .model_context_windows
                .contains_key("claude-mythos-5-1"));
            assert!(!provider
                .model_max_output_tokens
                .contains_key("claude-mythos-5-1"));
            assert!(!provider
                .model_input_modalities
                .contains_key("claude-mythos-5-1"));
            assert!(!provider
                .model_reasoning_efforts
                .contains_key("claude-mythos-5-1"));
            assert!(!provider
                .model_default_reasoning_efforts
                .contains_key("claude-mythos-5-1"));
            assert!(!provider
                .service_tier_models
                .iter()
                .any(|m| m == "claude-mythos-5-1"));
            assert!(!provider
                .no_structured_output_models
                .iter()
                .any(|m| m == "claude-mythos-5-1"));
            assert!(!provider
                .preserve_reasoning_content_models
                .iter()
                .any(|m| m == "claude-mythos-5-1"));

            // Other models and the user's unrelated selection survive.
            assert!(provider.models.iter().any(|m| m == "claude-opus-5-5"));
            assert!(settings
                .catalog
                .selected_models
                .iter()
                .any(|m| m == "anthropic/claude-opus-5-5"));
            assert!(!settings
                .catalog
                .selected_models
                .iter()
                .any(|m| m.ends_with("claude-mythos-5-1")));
        }
    }

    #[test]
    fn retiring_the_anthropic_model_is_not_undone_on_every_load() {
        let mut settings = GatewaySettings::default();
        backfill_registry_input_limits(&mut settings);
        assert!(!backfill_registry_input_limits(&mut settings));
    }

    #[test]
    fn gpt61_sol_migrates_into_existing_openai_providers() {
        let model = "gpt-6.1-sol";
        for (id, qualified) in [
            ("openai", model.to_string()),
            ("openai-api", format!("openai-api/{model}")),
            ("openai-apikey", format!("openai-apikey/{model}")),
        ] {
            let mut provider = ProviderDefinition {
                id: id.into(),
                ..ProviderDefinition::default()
            };
            apply_registry_defaults(&mut provider);
            provider.models.retain(|value| value != model);
            provider.model_context_windows.remove(model);
            provider.model_max_input_tokens.remove(model);
            provider.model_max_output_tokens.remove(model);
            provider.model_input_modalities.remove(model);
            provider.model_reasoning_efforts.remove(model);
            provider.model_default_reasoning_efforts.remove(model);
            let default_model = provider.default_model.clone();
            let mut settings = GatewaySettings {
                registry_revision: GPT61_SOL_MODEL_REVISION - 1,
                providers: vec![provider],
                ..GatewaySettings::default()
            };
            // A user-curated allowlist that already publishes this provider
            // must gain the new model; an unrelated one must not.
            settings.catalog.selected_models = vec!["gpt-6-sol".into()];

            assert!(backfill_registry_input_limits(&mut settings));
            let provider = &settings.providers[0];
            assert!(provider.models.iter().any(|value| value == model), "{id}");
            assert_eq!(
                provider.model_context_windows.get(model).copied(),
                Some(1_050_000)
            );
            assert_eq!(
                provider.model_max_input_tokens.get(model).copied(),
                Some(922_000)
            );
            assert_eq!(
                provider.model_max_output_tokens.get(model).copied(),
                Some(128_000)
            );
            assert_eq!(
                provider.model_input_modalities.get(model),
                Some(&strings(&["text", "image"]))
            );
            assert!(provider.model_reasoning_efforts.contains_key(model));
            assert_eq!(
                provider.model_default_reasoning_efforts.get(model),
                Some(&"low".to_string())
            );
            // The preset default is preserved; a user-picked one would be too.
            assert_eq!(provider.default_model, default_model);
            assert!(settings.catalog.selected_models.contains(&qualified));
            assert!(!backfill_registry_input_limits(&mut settings));
        }
    }

    #[test]
    fn gpt61_sol_does_not_opt_an_unpublished_provider_in() {
        let model = "gpt-6.1-sol";
        for id in ["openai", "openai-api", "openai-apikey"] {
            let mut provider = ProviderDefinition {
                id: id.into(),
                ..ProviderDefinition::default()
            };
            apply_registry_defaults(&mut provider);
            provider.models.retain(|value| value != model);
            let mut settings = GatewaySettings {
                registry_revision: GPT61_SOL_MODEL_REVISION - 1,
                providers: vec![provider],
                ..GatewaySettings::default()
            };
            settings.catalog.selected_models = vec!["xai/grok-4.7".into()];

            assert!(backfill_registry_input_limits(&mut settings));
            assert!(settings.providers[0]
                .models
                .iter()
                .any(|value| value == model));
            assert_eq!(settings.catalog.selected_models, vec!["xai/grok-4.7"]);
        }
    }

    #[test]
    fn gpt61_sol_keeps_an_empty_selection_empty() {
        for id in ["openai", "openai-api", "openai-apikey"] {
            let mut provider = ProviderDefinition {
                id: id.into(),
                ..ProviderDefinition::default()
            };
            apply_registry_defaults(&mut provider);
            provider.models.retain(|value| value != "gpt-6.1-sol");
            let mut settings = GatewaySettings {
                registry_revision: GPT61_SOL_MODEL_REVISION - 1,
                providers: vec![provider],
                ..GatewaySettings::default()
            };
            assert!(settings.catalog.selected_models.is_empty());

            assert!(backfill_registry_input_limits(&mut settings));
            assert!(settings.providers[0]
                .models
                .iter()
                .any(|value| value == "gpt-6.1-sol"));
            assert!(settings.catalog.selected_models.is_empty(), "{id}");
        }
    }

    #[test]
    fn sonnet_55_migrates_into_existing_anthropic_providers() {
        for id in ["anthropic", "anthropic-apikey"] {
            let mut provider = ProviderDefinition {
                id: id.into(),
                ..ProviderDefinition::default()
            };
            apply_registry_defaults(&mut provider);
            let model = "claude-sonnet-5-5";
            provider.models.retain(|value| value != model);
            provider.model_context_windows.remove(model);
            provider.model_reasoning_efforts.remove(model);
            provider.default_model = Some("claude-sonnet-5".into());
            let mut settings = GatewaySettings {
                registry_revision: ANTHROPIC_SONNET_55_REVISION - 1,
                providers: vec![provider],
                ..GatewaySettings::default()
            };
            settings.catalog.selected_models = vec![format!("{id}/claude-opus-5-5")];

            assert!(backfill_registry_input_limits(&mut settings));
            let provider = &settings.providers[0];
            assert!(provider.models.iter().any(|value| value == model), "{id}");
            assert_eq!(
                provider.model_context_windows.get(model).copied(),
                Some(1_000_000)
            );
            assert!(provider.model_reasoning_efforts.contains_key(model));
            assert_eq!(provider.default_model.as_deref(), Some(model));
            assert!(settings
                .catalog
                .selected_models
                .contains(&format!("{id}/{model}")));
            assert!(!backfill_registry_input_limits(&mut settings));
        }
    }

    #[test]
    fn september_models_migrate_without_overwriting_user_choices() {
        for (id, models) in [
            ("openai", vec!["gpt-6-sol", "gpt-6-luna"]),
            ("openai-api", vec!["gpt-6-sol", "gpt-6-luna"]),
            ("openai-apikey", vec!["gpt-6-sol", "gpt-6-luna"]),
            ("anthropic", vec!["claude-fable-5-1", "claude-opus-5-5"]),
            (
                "anthropic-apikey",
                vec!["claude-fable-5-1", "claude-opus-5-5"],
            ),
            ("xai", vec!["grok-4.7"]),
        ] {
            let mut provider = ProviderDefinition {
                id: id.into(),
                ..ProviderDefinition::default()
            };
            apply_registry_defaults(&mut provider);
            for model in &models {
                assert!(provider.models.iter().any(|value| value == model));
            }
            let default_model = provider.default_model.clone();
            provider
                .models
                .retain(|model| !models.contains(&model.as_str()));
            provider.model_context_windows.clear();
            provider.model_max_input_tokens.clear();
            provider.model_max_output_tokens.clear();
            provider.model_input_modalities.clear();
            provider.model_reasoning_efforts.clear();
            provider.model_default_reasoning_efforts.clear();
            provider.service_tier_models.clear();
            provider.preserve_reasoning_content_models.clear();
            let mut settings = GatewaySettings {
                registry_revision: SEPTEMBER_MODEL_REFRESH_REVISION - 1,
                providers: vec![provider],
                ..GatewaySettings::default()
            };
            settings.catalog.selected_models = vec!["saved-selection".into()];
            assert!(backfill_registry_input_limits(&mut settings));
            let provider = &settings.providers[0];
            assert_eq!(provider.default_model, default_model);
            assert_eq!(settings.catalog.selected_models, vec!["saved-selection"]);
            for model in &models {
                assert!(provider.models.iter().any(|value| value == model));
                if model.starts_with("gpt-") {
                    assert_eq!(provider.model_context_windows[*model], 1_050_000);
                    assert_eq!(provider.model_max_input_tokens[*model], 922_000);
                    assert_eq!(provider.model_max_output_tokens[*model], 128_000);
                    assert_eq!(provider.model_default_reasoning_efforts[*model], "medium");
                    assert!(provider.model_reasoning_efforts[*model].contains(&"none".into()));
                    assert!(provider.service_tier_models.contains(&model.to_string()));
                } else if model.starts_with("grok-") {
                    assert_eq!(provider.model_context_windows[*model], 500_000);
                    assert_eq!(provider.model_default_reasoning_efforts[*model], "high");
                    assert!(provider
                        .preserve_reasoning_content_models
                        .contains(&model.to_string()));
                }
            }
            assert!(!backfill_registry_input_limits(&mut settings));
            // An intentional removal after migration is not undone on every load.
            settings.providers[0]
                .models
                .retain(|model| !models.contains(&model.as_str()));
            backfill_registry_input_limits(&mut settings);
            assert!(settings.providers[0]
                .models
                .iter()
                .all(|model| !models.contains(&model.as_str())));
        }
    }

    #[test]
    fn newer_grok_4_minor_inherits_the_latest_registered_window() {
        let windows = BTreeMap::from([
            ("grok-4.3".into(), 1_000_000),
            ("grok-4.5".into(), 500_000),
            ("grok-4.6".into(), 500_000),
            ("grok-4.20-0309-reasoning".into(), 1_000_000),
        ]);
        assert_eq!(
            resolve_model_context_window(&windows, "grok-4.6"),
            Some(500_000)
        );
        assert_eq!(
            resolve_model_context_window(&windows, "grok-4.7"),
            Some(500_000)
        );
        assert_eq!(
            resolve_model_context_window(&windows, "grok-4.20-0309-reasoning"),
            Some(1_000_000)
        );
        assert_eq!(
            resolve_model_context_window(&windows, "grok-imagine-image"),
            None
        );
    }

    #[test]
    fn xai_grok_46_uses_the_documented_500k_window() {
        let provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "xai")
            .unwrap()
            .instantiate(None)
            .unwrap();
        assert!(provider.models.iter().any(|model| model == "grok-4.6"));
        assert_eq!(
            provider.model_context_windows.get("grok-4.6").copied(),
            Some(500_000)
        );
        assert_eq!(
            provider.model_context_windows.get("grok-4.5").copied(),
            Some(500_000)
        );
        assert_eq!(
            provider
                .model_default_reasoning_efforts
                .get("grok-4.6")
                .map(String::as_str),
            Some("high")
        );
        assert!(provider
            .model_reasoning_efforts
            .get("grok-4.6")
            .is_some_and(|efforts| efforts.iter().any(|effort| effort == "xhigh")));
    }

    #[test]
    fn meta_muse_spark_uses_the_documented_chat_completions_contract() {
        {
            let id = "meta";
            let provider = provider_presets()
                .into_iter()
                .find(|preset| preset.id == id)
                .unwrap()
                .instantiate(None)
                .unwrap();
            assert_eq!(provider.protocol, ProviderProtocol::ChatCompletions);
            assert_eq!(provider.base_url, "https://api.meta.ai/v1");
            assert_eq!(
                provider.credential.reference.as_deref(),
                Some("META_MODEL_API_KEY")
            );
            assert_eq!(provider.default_model.as_deref(), Some("muse-spark-1.3"));
            assert_eq!(
                &provider.models,
                &[
                    "muse-spark-1.3".to_string(),
                    "muse-spark-1.3-contributor".to_string(),
                    "muse-spark-1.2".to_string(),
                    "muse-spark-1.2-contributor".to_string(),
                    "muse-spark-1.1".to_string(),
                ]
            );
            assert_eq!(
                provider
                    .model_context_windows
                    .get("muse-spark-1.3")
                    .copied(),
                Some(1_048_576)
            );
            assert_eq!(
                provider
                    .model_max_output_tokens
                    .get("muse-spark-1.3")
                    .copied(),
                Some(131_072)
            );
            assert_eq!(
                provider
                    .model_context_windows
                    .get("muse-spark-1.2")
                    .copied(),
                Some(1_048_576)
            );
            assert_eq!(
                provider
                    .model_context_windows
                    .get("muse-spark-1.2-contributor")
                    .copied(),
                Some(1_048_576)
            );
            assert_eq!(
                provider
                    .model_context_windows
                    .get("muse-spark-1.1")
                    .copied(),
                Some(1_000_000)
            );
            assert_eq!(
                provider
                    .model_max_output_tokens
                    .get("muse-spark-1.2")
                    .copied(),
                Some(131_072)
            );
            assert_eq!(
                provider
                    .model_reasoning_effort_map
                    .get("muse-spark-1.2")
                    .and_then(|mapping| mapping.get("none"))
                    .map(String::as_str),
                Some("minimal")
            );
            assert!(provider.capabilities.vision);
            assert!(provider.capabilities.reasoning);
            assert!(provider.prompt_cache_key);
            assert!(provider.validate().is_ok());
        }
    }

    #[test]
    fn mistral_backfill_reaches_saved_connections_without_undoing_choices() {
        let saved = |base_url: &str| ProviderDefinition {
            id: "mistral".into(),
            name: "Mistral".into(),
            base_url: base_url.into(),
            models: vec!["my-pick".into()],
            default_model: Some("my-pick".into()),
            model_reasoning_efforts: BTreeMap::from([(
                "zai-glm-5-3".to_string(),
                vec!["low".to_string()],
            )]),
            ..ProviderDefinition::default()
        };
        let mut settings = GatewaySettings {
            registry_revision: MISTRAL_VIBE_PLAN_REVISION - 1,
            providers: vec![
                saved("https://api.mistral.ai/v1"),
                saved("https://proxy.example/v1"),
            ],
            ..GatewaySettings::default()
        };
        assert!(backfill_registry_input_limits(&mut settings));
        let official = &settings.providers[0];
        assert!(official.prompt_cache_key);
        assert_eq!(official.discovery.path, "/model-catalog");
        assert!(official.models.iter().any(|model| model == "my-pick"));
        assert!(official
            .models
            .iter()
            .any(|model| model == "mistral-large-4-0"));
        assert_eq!(official.default_model.as_deref(), Some("my-pick"));
        assert_eq!(official.model_reasoning_efforts["zai-glm-5-3"], ["low"]);
        assert_eq!(
            official.model_reasoning_efforts["mistral-medium-latest"],
            ["high"]
        );
        // The migration's `high` is not in the list the user kept.
        assert!(!official
            .model_default_reasoning_efforts
            .contains_key("zai-glm-5-3"));
        official.validate().unwrap();
        // A proxy may reject the field, and the migration is not for it.
        assert!(!settings.providers[1].prompt_cache_key);
        settings.providers[1].validate().unwrap();
    }

    #[test]
    fn mistral_migration_moves_only_the_old_default_discovery_path() {
        for (saved_path, expected) in [
            ("/models", "/model-catalog"),
            ("/my/own/models", "/my/own/models"),
        ] {
            let mut provider = ProviderDefinition {
                id: "mistral".into(),
                name: "Mistral".into(),
                base_url: "https://api.mistral.ai/v1".into(),
                ..ProviderDefinition::default()
            };
            provider.discovery.path = saved_path.into();
            let mut settings = GatewaySettings {
                registry_revision: MISTRAL_VIBE_PLAN_REVISION - 1,
                providers: vec![provider],
                ..GatewaySettings::default()
            };

            assert!(backfill_registry_input_limits(&mut settings));

            assert_eq!(settings.providers[0].discovery.path, expected);
            settings.validate().unwrap();
        }
    }

    #[test]
    fn mistral_preset_offers_only_efforts_each_model_accepts() {
        let provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "mistral")
            .unwrap()
            .instantiate(None)
            .unwrap();
        assert_eq!(provider.default_model.as_deref(), Some("mistral-large-4-0"));
        assert!(provider.prompt_cache_key);
        // Mistral rejects `reasoning_content` on an assistant message with 422.
        assert!(provider.preserve_reasoning_content_models.is_empty());
        for (model, efforts) in [
            ("mistral-large-4-0", vec!["low", "medium", "high"]),
            ("mistral-medium-latest", vec!["high"]),
            ("zai-glm-5-3", vec!["low", "high", "max"]),
            ("zai-glm-5-3-flash", vec!["low", "high", "max"]),
        ] {
            assert!(provider.models.iter().any(|known| known == model));
            assert_eq!(provider.model_reasoning_efforts[model], efforts);
        }
        assert_eq!(provider.model_input_modalities["zai-glm-5-3"], ["text"]);
        assert!(provider.validate().is_ok());
    }

    #[test]
    fn backfills_missing_xai_grok_46_window_on_existing_settings() {
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "xai")
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.model_context_windows.remove("grok-4.6");
        let mut settings = GatewaySettings {
            registry_revision: REGISTRY_REVISION,
            providers: vec![provider],
            model_catalog: vec![ModelMetadata {
                provider_id: "xai".into(),
                model_id: "grok-4.6".into(),
                enabled: true,
                ..ModelMetadata::default()
            }],
            ..GatewaySettings::default()
        };
        assert!(backfill_registry_input_limits(&mut settings));
        assert_eq!(
            settings.providers[0]
                .model_context_windows
                .get("grok-4.6")
                .copied(),
            Some(500_000)
        );
    }

    #[test]
    fn openai_presets_declare_canonical_image_model_without_self_mapping() {
        for id in ["openai", "openai-api", "openai-apikey"] {
            let provider = provider_presets()
                .into_iter()
                .find(|preset| preset.id == id)
                .unwrap()
                .instantiate(None)
                .unwrap();

            assert!(!provider.models.iter().any(|model| model == "gpt-image-2"));
            assert!(provider
                .image_generation_models
                .iter()
                .any(|model| model == "imagegen-2"));
            assert!(provider
                .image_generation_models
                .iter()
                .any(|model| model == "gpt-image-2"));
            assert_eq!(
                provider
                    .model_wire_ids
                    .get("imagegen-2")
                    .map(String::as_str),
                Some("gpt-image-2")
            );
            assert!(!provider.model_wire_ids.contains_key("gpt-image-2"));
            assert!(!provider.model_context_windows.contains_key("gpt-image-2"));
            assert!(!provider.model_max_input_tokens.contains_key("gpt-image-2"));
            assert!(!provider.model_input_modalities.contains_key("gpt-image-2"));
            assert!(provider.validate().is_ok());

            let settings = GatewaySettings {
                providers: vec![provider.clone()],
                ..GatewaySettings::default()
            };
            assert!(crate::config::image_model_is_available(
                &settings,
                &provider,
                "imagegen-2"
            ));
            assert!(crate::config::image_model_is_available(
                &settings,
                &provider,
                "gpt-image-2"
            ));
        }
    }

    #[test]
    fn backfills_codex_image_aliases_without_overwriting_explicit_capability_opt_out() {
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "openai")
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.capabilities.image_generation = false;
        provider.image_generation_models.clear();
        provider.model_wire_ids.remove("imagegen-2");
        provider
            .model_wire_ids
            .insert("gpt-image-2".into(), "gpt-image-2".into());
        provider.models.push("gpt-image-2".into());
        provider
            .model_context_windows
            .insert("gpt-image-2".into(), 1_050_000);
        provider
            .model_input_modalities
            .insert("gpt-image-2".into(), vec!["text".into(), "image".into()]);
        let mut settings = GatewaySettings {
            registry_revision: IMAGE_MODEL_ISOLATION_REVISION - 1,
            providers: vec![provider],
            ..GatewaySettings::default()
        };

        assert!(backfill_registry_input_limits(&mut settings));
        assert_eq!(settings.registry_revision, REGISTRY_REVISION);
        assert!(!settings.providers[0].capabilities.image_generation);
        assert_eq!(
            settings.providers[0]
                .model_wire_ids
                .get("imagegen-2")
                .map(String::as_str),
            Some("gpt-image-2")
        );
        assert!(!settings.providers[0]
            .models
            .iter()
            .any(|model| model == "gpt-image-2"));
        assert!(settings.providers[0]
            .image_generation_models
            .iter()
            .any(|model| model == "imagegen-2"));
        assert!(settings.providers[0]
            .image_generation_models
            .iter()
            .any(|model| model == "gpt-image-2"));
        assert!(!settings.providers[0]
            .model_wire_ids
            .contains_key("gpt-image-2"));
        assert!(!settings.providers[0]
            .model_context_windows
            .contains_key("gpt-image-2"));
        assert!(!settings.providers[0]
            .model_input_modalities
            .contains_key("gpt-image-2"));
        assert!(!crate::config::image_model_is_available(
            &settings,
            &settings.providers[0],
            "imagegen-2"
        ));
    }

    #[test]
    fn responses_snapshot_repair_remains_explicit_opt_in_across_migration() {
        let provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "openai-api")
            .unwrap()
            .instantiate(None)
            .unwrap();
        assert!(!provider.responses_snapshot_repair);

        let mut settings = GatewaySettings {
            registry_revision: CONFORMANCE_POLICY_REVISION - 1,
            providers: vec![provider],
            ..GatewaySettings::default()
        };
        backfill_registry_input_limits(&mut settings);

        assert!(!settings.providers[0].responses_snapshot_repair);
    }

    #[test]
    fn backfills_missing_input_limits_without_overwriting_user_values() {
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "openai")
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.model_max_input_tokens.clear();
        provider
            .model_context_windows
            .insert("gpt-5.6-sol".into(), 400_000);
        provider
            .model_context_windows
            .insert("gpt-5.6-terra".into(), 200_000);
        provider.model_context_windows.remove("gpt-5.6-luna");
        let mut settings = GatewaySettings {
            providers: vec![provider],
            ..GatewaySettings::default()
        };

        assert!(backfill_registry_input_limits(&mut settings));
        let provider = &settings.providers[0];
        assert_eq!(
            provider.model_context_windows.get("gpt-5.6-sol").copied(),
            Some(400_000)
        );
        assert_eq!(
            provider.model_max_input_tokens.get("gpt-5.6-sol").copied(),
            Some(272_000)
        );
        assert_eq!(
            provider
                .model_max_input_tokens
                .get("gpt-5.6-terra")
                .copied(),
            Some(200_000)
        );
        assert_eq!(
            provider.model_context_windows.get("gpt-5.6-luna").copied(),
            Some(372_000)
        );
        assert_eq!(
            provider.model_max_input_tokens.get("gpt-5.6-luna").copied(),
            Some(272_000)
        );
    }

    #[test]
    fn backfills_enabled_models_referenced_only_by_the_model_catalog() {
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "openai")
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.models.clear();
        provider.default_model = None;
        provider.model_context_windows.remove("gpt-5.6-terra");
        provider.model_max_input_tokens.remove("gpt-5.6-terra");
        let mut settings = GatewaySettings {
            providers: vec![provider],
            model_catalog: vec![crate::config::ModelMetadata {
                provider_id: "openai".into(),
                model_id: "gpt-5.6-terra".into(),
                enabled: true,
                ..crate::config::ModelMetadata::default()
            }],
            ..GatewaySettings::default()
        };

        assert!(backfill_registry_input_limits(&mut settings));
        let provider = &settings.providers[0];
        assert_eq!(
            provider.model_context_windows.get("gpt-5.6-terra").copied(),
            Some(372_000)
        );
        assert_eq!(
            provider
                .model_max_input_tokens
                .get("gpt-5.6-terra")
                .copied(),
            Some(272_000)
        );
    }

    #[test]
    fn backfills_current_antigravity_models_without_changing_the_existing_default() {
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "google-antigravity")
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.default_model = Some("gemini-3.6-flash".into());
        provider
            .models
            .retain(|model| !matches!(model.as_str(), "gemini-3.7-flash" | "gemini-3.5-flash"));
        provider.model_context_windows.remove("gemini-3.7-flash");
        provider.model_context_windows.remove("gemini-3.5-flash");
        provider.model_reasoning_efforts.remove("gemini-3.7-flash");
        provider.model_reasoning_efforts.remove("gemini-3.5-flash");
        provider
            .model_default_reasoning_efforts
            .remove("gemini-3.7-flash");
        provider
            .model_default_reasoning_efforts
            .remove("gemini-3.5-flash");
        let mut settings = GatewaySettings {
            registry_revision: 0,
            providers: vec![provider],
            ..GatewaySettings::default()
        };

        assert!(backfill_registry_input_limits(&mut settings));
        let provider = &settings.providers[0];
        assert_eq!(provider.default_model.as_deref(), Some("gemini-3.6-flash"));
        assert_eq!(
            &provider.models[..4],
            &[
                "gemini-4-argon",
                "gemini-3.8-flash",
                "gemini-3.7-flash",
                "gemini-3.6-flash"
            ]
        );
        assert_eq!(
            provider
                .model_context_windows
                .get("gemini-4-argon")
                .copied(),
            Some(1_048_576)
        );
        assert!(provider
            .model_wire_ids
            .keys()
            .all(|alias| !alias.starts_with("gemini-4-argon")));
        assert!(settings
            .catalog
            .selected_models
            .iter()
            .any(|selected| selected == "google-antigravity/gemini-4-argon"));
        assert_eq!(
            provider
                .model_default_reasoning_efforts
                .get("gemini-3.7-flash")
                .map(String::as_str),
            Some("medium")
        );
    }

    #[test]
    fn backfills_empty_completion_retry_for_existing_antigravity_settings() {
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "google-antigravity")
            .unwrap()
            .instantiate(None)
            .unwrap();
        assert_eq!(
            provider.empty_completion_retry_models,
            vec!["*".to_string()]
        );
        provider.empty_completion_retry_models.clear();
        let mut settings = GatewaySettings {
            registry_revision: ANTIGRAVITY_EMPTY_RETRY_REVISION - 1,
            providers: vec![provider],
            ..GatewaySettings::default()
        };

        assert!(backfill_registry_input_limits(&mut settings));
        assert_eq!(
            settings.providers[0].empty_completion_retry_models,
            vec!["*".to_string()]
        );
    }

    #[test]
    fn restores_empty_antigravity_models_from_registry_defaults() {
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "google-antigravity")
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.models.clear();
        let mut settings = GatewaySettings {
            registry_revision: REGISTRY_REVISION,
            providers: vec![provider],
            ..GatewaySettings::default()
        };

        assert!(backfill_registry_input_limits(&mut settings));
        assert!(settings.providers[0]
            .models
            .iter()
            .any(|model| model == "gemini-3.6-flash"));
    }

    #[test]
    fn does_not_restore_antigravity_models_after_registry_migration() {
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "google-antigravity")
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.models.retain(|model| model != "gemini-3.7-flash");
        let mut settings = GatewaySettings {
            registry_revision: ANTIGRAVITY_MODEL_MIGRATION_REVISION,
            providers: vec![provider],
            ..GatewaySettings::default()
        };

        assert!(backfill_registry_input_limits(&mut settings));
        assert!(!settings.providers[0]
            .models
            .iter()
            .any(|model| model == "gemini-3.7-flash"));
    }

    #[test]
    fn backfills_strict_responses_tool_result_adjacency() {
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "deepseek")
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.requires_adjacent_responses_tool_results = false;
        let mut settings = GatewaySettings {
            registry_revision: STRICT_RESPONSES_TOOL_ADJACENCY_REVISION - 1,
            providers: vec![provider],
            ..GatewaySettings::default()
        };

        assert!(backfill_registry_input_limits(&mut settings));
        assert!(settings.providers[0].requires_adjacent_responses_tool_results);
        assert!(settings.providers[0]
            .requires_reasoning_placeholder_models
            .is_none());
        assert_eq!(settings.registry_revision, REGISTRY_REVISION);
    }

    #[test]
    fn preserves_reasoning_placeholder_default_and_explicit_opt_out() {
        let deepseek = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "deepseek")
            .unwrap()
            .instantiate(None)
            .unwrap();
        assert!(deepseek.requires_reasoning_placeholder_models.is_none());

        let minimax = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "minimax")
            .unwrap()
            .instantiate(None)
            .unwrap();
        assert_eq!(
            minimax.requires_reasoning_placeholder_models,
            Some(Vec::new())
        );
    }

    #[test]
    fn backfills_reasoning_placeholder_opt_outs() {
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "minimax")
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.requires_reasoning_placeholder_models = None;
        let mut settings = GatewaySettings {
            registry_revision: STRICT_RESPONSES_TOOL_ADJACENCY_REVISION - 1,
            providers: vec![provider],
            ..GatewaySettings::default()
        };

        assert!(backfill_registry_input_limits(&mut settings));
        assert_eq!(
            settings.providers[0].requires_reasoning_placeholder_models,
            Some(Vec::new())
        );
    }

    #[test]
    fn registry_presets_explicitly_enable_safe_adapter_capabilities() {
        for preset in provider_presets() {
            let requires_custom_url = preset.requires_custom_url;
            let id = preset.id;
            let provider = preset
                .instantiate(requires_custom_url.then_some("https://fixture.invalid/v1"))
                .expect("registry preset");
            if requires_custom_url {
                assert!(!provider.capabilities.structured_output, "{id}");
                assert!(!provider.capabilities.custom_tools, "{id}");
                assert!(!provider.capabilities.tool_search, "{id}");
                assert!(!provider.capabilities.mcp_namespaces, "{id}");
                continue;
            }
            if id == "kiro" {
                assert!(!provider.capabilities.structured_output);
                assert!(!provider.capabilities.custom_tools);
                assert!(!provider.capabilities.tool_search);
                assert!(!provider.capabilities.mcp_namespaces);
                continue;
            }
            assert!(provider.capabilities.structured_output, "{id}");
            if provider.capabilities.tools {
                assert!(provider.capabilities.custom_tools, "{id}");
                assert!(provider.capabilities.tool_search, "{id}");
                assert!(provider.capabilities.mcp_namespaces, "{id}");
            }
        }
    }

    #[test]
    fn registry_capabilities_backfill_without_upgrading_unknown_custom_providers() {
        let mut registry_provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "deepseek")
            .unwrap()
            .instantiate(None)
            .unwrap();
        registry_provider.capabilities.structured_output = false;
        registry_provider.capabilities.custom_tools = false;
        registry_provider.capabilities.tool_search = false;
        registry_provider.capabilities.mcp_namespaces = false;
        let mut custom = ProviderDefinition {
            id: "private-custom".into(),
            name: "Private custom".into(),
            base_url: "https://custom.example/v1".into(),
            capabilities: ProviderCapabilities {
                tools: true,
                ..ProviderCapabilities::default()
            },
            ..ProviderDefinition::default()
        };
        custom.capabilities.structured_output = false;
        let mut settings = GatewaySettings {
            registry_revision: SAFE_CAPABILITY_DEFAULTS_REVISION - 1,
            providers: vec![registry_provider, custom],
            ..GatewaySettings::default()
        };

        assert!(backfill_registry_input_limits(&mut settings));
        let registry = &settings.providers[0].capabilities;
        assert!(registry.structured_output);
        assert!(registry.custom_tools && registry.tool_search && registry.mcp_namespaces);
        let custom = &settings.providers[1].capabilities;
        assert!(!custom.structured_output);
        assert!(!custom.custom_tools && !custom.tool_search && !custom.mcp_namespaces);
    }

    #[test]
    fn isolates_stale_discovered_image_capabilities_during_registry_migration() {
        let provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "openai")
            .unwrap()
            .instantiate(None)
            .unwrap();
        let metadata = |model_id: &str| ModelMetadata {
            provider_id: "openai".into(),
            model_id: model_id.into(),
            enabled: true,
            capabilities: ProviderCapabilities {
                image_generation: true,
                ..ProviderCapabilities::default()
            },
            ..ModelMetadata::default()
        };
        let mut settings = GatewaySettings {
            registry_revision: MODEL_CAPABILITY_ISOLATION_REVISION - 1,
            providers: vec![provider],
            model_catalog: vec![
                metadata("gpt-5.6-sol"),
                metadata("imagegen-2"),
                metadata("gpt-image-2"),
                metadata("user-defined-image"),
            ],
            ..GatewaySettings::default()
        };

        assert!(backfill_registry_input_limits(&mut settings));
        assert_eq!(settings.registry_revision, REGISTRY_REVISION);
        assert!(!settings.model_catalog[0].capabilities.image_generation);
        assert!(settings.model_catalog[1].capabilities.image_generation);
        assert!(settings.model_catalog[2].capabilities.image_generation);
        assert!(settings.model_catalog[3].capabilities.image_generation);
    }

    #[test]
    fn backfills_xai_image_model_identity_before_capability_isolation() {
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "xai")
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.image_generation_models.clear();
        let metadata = |model_id: &str| ModelMetadata {
            provider_id: "xai".into(),
            model_id: model_id.into(),
            enabled: true,
            capabilities: ProviderCapabilities {
                image_generation: true,
                ..ProviderCapabilities::default()
            },
            ..ModelMetadata::default()
        };
        let mut settings = GatewaySettings {
            registry_revision: MODEL_CAPABILITY_ISOLATION_REVISION - 1,
            providers: vec![provider],
            model_catalog: vec![metadata("grok-4.5"), metadata("grok-imagine-image-quality")],
            ..GatewaySettings::default()
        };

        assert!(backfill_registry_input_limits(&mut settings));
        assert_eq!(
            settings.providers[0].image_generation_models,
            vec!["grok-imagine-image-quality"]
        );
        assert!(!settings.model_catalog[0].capabilities.image_generation);
        assert!(settings.model_catalog[1].capabilities.image_generation);
    }

    #[test]
    fn model_catalog_capabilities_backfill_and_preserve_model_exceptions() {
        let provider = ProviderDefinition {
            id: "fixture".into(),
            name: "Fixture".into(),
            base_url: "https://fixture.example/v1".into(),
            capabilities: ProviderCapabilities {
                structured_output: true,
                service_tier: false,
                custom_tools: true,
                tool_search: true,
                mcp_namespaces: true,
                provider_metadata: true,
                ..ProviderCapabilities::default()
            },
            no_structured_output_models: vec!["legacy".into()],
            service_tier_models: vec!["tiered".into()],
            ..ProviderDefinition::default()
        };
        let metadata = |model_id: &str| ModelMetadata {
            provider_id: "fixture".into(),
            model_id: model_id.into(),
            ..ModelMetadata::default()
        };
        let mut settings = GatewaySettings {
            registry_revision: SAFE_CAPABILITY_DEFAULTS_REVISION - 1,
            providers: vec![provider],
            model_catalog: vec![metadata("normal"), metadata("legacy"), metadata("tiered")],
            ..GatewaySettings::default()
        };

        assert!(backfill_registry_input_limits(&mut settings));
        let normal = &settings.model_catalog[0].capabilities;
        assert!(normal.structured_output);
        assert!(normal.custom_tools && normal.tool_search && normal.mcp_namespaces);
        assert!(normal.provider_metadata);
        assert!(!settings.model_catalog[1].capabilities.structured_output);
        assert!(settings.model_catalog[2].capabilities.service_tier);

        let effective = crate::config::effective_model_capabilities(
            &settings.providers[0],
            Some(&settings.model_catalog[1]),
            "legacy",
        );
        assert!(!effective.structured_output);
    }
}

#[cfg(test)]
mod argon_wire_id_migration_tests {
    use super::*;

    #[test]
    fn revision_23_settings_with_stale_argon_wire_ids_are_repaired() {
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "google-antigravity")
            .unwrap()
            .instantiate(None)
            .unwrap();
        for alias in [
            "gemini-4-argon",
            "gemini-4-argon-low",
            "gemini-4-argon-medium",
            "gemini-4-argon-high",
        ] {
            provider
                .model_wire_ids
                .insert(alias.into(), "gemini-4-argon-tiered".into());
        }
        let mut settings = GatewaySettings {
            registry_revision: 23,
            providers: vec![provider],
            ..GatewaySettings::default()
        };
        assert!(backfill_registry_input_limits(&mut settings));
        let wire = &settings.providers[0].model_wire_ids;
        // The model is addressed by its own id, so no argon alias keeps a mapping.
        for alias in [
            "gemini-4-argon",
            "gemini-4-argon-low",
            "gemini-4-argon-medium",
            "gemini-4-argon-high",
        ] {
            assert!(
                wire.get(alias).is_none(),
                "{alias} must not keep a wire mapping"
            );
        }
        // 3.8 Flash keeps its working -tiered id.
        assert_eq!(
            wire.get("gemini-3.8-flash").map(String::as_str),
            Some("gemini-3.8-flash-tiered")
        );
        assert_eq!(settings.registry_revision, REGISTRY_REVISION);
        // A wire mapping that points at itself fails validation and stops the gateway.
        settings.providers[0]
            .validate()
            .expect("migrated Antigravity provider must validate");
    }

    #[test]
    fn antigravity_preset_validates_with_its_wire_aliases() {
        let provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "google-antigravity")
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.validate().expect("preset must validate");
        assert!(provider
            .model_wire_ids
            .keys()
            .all(|alias| !alias.starts_with("gemini-4-argon")));
    }
}
