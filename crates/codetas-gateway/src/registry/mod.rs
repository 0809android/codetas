use crate::config::{
    CredentialSource, CredentialTransport, GatewaySettings, ModelDiscoverySettings,
    ProviderCapabilities, ProviderCredential, ProviderDefinition, ProviderProtocol,
    REGISTRY_REVISION,
};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};

mod vendors_a;
mod vendors_b;

const ANTIGRAVITY_MODEL_MIGRATION_REVISION: u32 = 1;
const STRICT_RESPONSES_TOOL_ADJACENCY_REVISION: u32 = 2;
const CONFORMANCE_POLICY_REVISION: u32 = 3;
const SAFE_CAPABILITY_DEFAULTS_REVISION: u32 = 4;
const CODEX_IMAGE_ROUTING_REVISION: u32 = 5;

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
        "kimi" | "kimi-code" => vendors_b::apply_kimi(provider),
        "moonshot" => vendors_b::apply_moonshot(provider),
        "deepseek" => vendors_b::apply_deepseek(provider),
        "opencode-go" => vendors_b::apply_opencode_go(provider),
        "nvidia" => vendors_b::apply_nvidia(provider),
        "neuralwatt" => vendors_b::apply_neuralwatt(provider),
        "zai" => vendors_b::apply_zai(provider),
        "zhipu-bigmodel" => vendors_b::apply_zhipu_bigmodel(provider),
        "minimax" | "minimax-cn" => vendors_b::apply_minimax(provider),
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
    match provider.protocol {
        ProviderProtocol::Responses => {
            provider.responses_snapshot_repair = true;
        }
        ProviderProtocol::AnthropicMessages => {
            provider.anthropic_eof_tolerance_models = vec!["*".into()];
        }
        _ => {}
    }
    if matches!(provider.id.as_str(), "openai" | "openai-api" | "openai-apikey") {
        provider.capabilities.service_tier = true;
        provider.service_tier_models = provider.models.clone();
    }
    if matches!(provider.id.as_str(), "deepseek" | "minimax" | "minimax-cn" | "zhipu-bigmodel") {
        provider.no_structured_output_models = provider.models.clone();
    }
}

pub(crate) fn backfill_registry_input_limits(settings: &mut GatewaySettings) -> bool {
    let mut changed = false;
    let migrate_antigravity_models =
        settings.registry_revision < ANTIGRAVITY_MODEL_MIGRATION_REVISION;
    let mut catalog_models = BTreeMap::<String, HashSet<String>>::new();
    let registry_capabilities = provider_presets()
        .into_iter()
        .filter_map(|preset| {
            let id = preset.id.to_string();
            preset
                .instantiate(preset.requires_custom_url.then_some("https://fixture.invalid/v1"))
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
        if settings.registry_revision < CODEX_IMAGE_ROUTING_REVISION
            && matches!(provider.id.as_str(), "openai" | "openai-api" | "openai-apikey")
        {
            changed |= enable_capability(
                &mut provider.capabilities.image_generation,
                defaults.capabilities.image_generation,
            );
            for (alias, wire_model) in &defaults.model_wire_ids {
                if !matches!(alias.as_str(), "imagegen-2" | "gpt-image-2") {
                    continue;
                }
                if !provider.model_wire_ids.contains_key(alias) {
                    provider
                        .model_wire_ids
                        .insert(alias.clone(), wire_model.clone());
                    changed = true;
                }
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
            if defaults.responses_snapshot_repair && !provider.responses_snapshot_repair {
                provider.responses_snapshot_repair = true;
                changed = true;
            }
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
                    if let Some(context) = defaults.model_context_windows.get(model) {
                        provider
                            .model_context_windows
                            .insert(model.into(), *context);
                        changed = true;
                    }
                }
                if !provider.model_reasoning_efforts.contains_key(model) {
                    if let Some(efforts) = defaults.model_reasoning_efforts.get(model) {
                        provider
                            .model_reasoning_efforts
                            .insert(model.into(), efforts.clone());
                        changed = true;
                    }
                }
                if !provider.model_default_reasoning_efforts.contains_key(model) {
                    if let Some(effort) = defaults.model_default_reasoning_efforts.get(model) {
                        provider
                            .model_default_reasoning_efforts
                            .insert(model.into(), effort.clone());
                        changed = true;
                    }
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
                if let Some(context) = defaults.model_context_windows.get(model) {
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
            let service_tier = provider.service_tier
                || model_matches_registry(&metadata.model_id, service_tier);
            changed |= set_capability(
                &mut metadata.capabilities.structured_output,
                structured_output,
            );
            changed |= set_capability(
                &mut metadata.capabilities.service_tier,
                service_tier,
            );
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
    if settings.registry_revision < REGISTRY_REVISION {
        settings.registry_revision = REGISTRY_REVISION;
        changed = true;
    }
    changed
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
        preset("openrouter", "OpenRouter", "Multi-provider OpenAI-compatible router", "https://openrouter.ai/api/v1", ProviderProtocol::ChatCompletions, Some("OPENROUTER_API_KEY")),
        preset("deepseek", "DeepSeek", "DeepSeek API", "https://api.deepseek.com", ProviderProtocol::ChatCompletions, Some("DEEPSEEK_API_KEY")),
        preset("mistral", "Mistral", "Mistral API", "https://api.mistral.ai/v1", ProviderProtocol::ChatCompletions, Some("MISTRAL_API_KEY")),
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
            vision: false,
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

    #[test]
    fn codex_login_models_have_explicit_input_contracts() {
        let provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "openai")
            .unwrap()
            .instantiate(None)
            .unwrap();

        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            assert_eq!(provider.model_context_windows.get(model).copied(), Some(372_000));
            assert_eq!(provider.model_max_input_tokens.get(model).copied(), Some(272_000));
            assert_eq!(provider.model_max_output_tokens.get(model), None);
        }
    }

    #[test]
    fn backfills_codex_image_aliases_and_capability_for_existing_settings() {
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "openai")
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.capabilities.image_generation = false;
        provider.model_wire_ids.remove("imagegen-2");
        provider.model_wire_ids.remove("gpt-image-2");
        let mut settings = GatewaySettings {
            registry_revision: SAFE_CAPABILITY_DEFAULTS_REVISION,
            providers: vec![provider],
            ..GatewaySettings::default()
        };

        assert!(backfill_registry_input_limits(&mut settings));
        assert_eq!(settings.registry_revision, REGISTRY_REVISION);
        assert!(settings.providers[0].capabilities.image_generation);
        assert_eq!(
            settings.providers[0]
                .model_wire_ids
                .get("imagegen-2")
                .map(String::as_str),
            Some("gpt-image-2")
        );
        assert_eq!(
            settings.providers[0]
                .model_wire_ids
                .get("gpt-image-2")
                .map(String::as_str),
            Some("gpt-image-2")
        );
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
            provider.model_max_input_tokens.get("gpt-5.6-terra").copied(),
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
            provider.model_max_input_tokens.get("gpt-5.6-terra").copied(),
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
            &provider.models[..3],
            &["gemini-3.7-flash", "gemini-3.6-flash", "gemini-3.5-flash"]
        );
        assert_eq!(
            provider.model_default_reasoning_efforts.get("gemini-3.7-flash").map(String::as_str),
            Some("medium")
        );
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
