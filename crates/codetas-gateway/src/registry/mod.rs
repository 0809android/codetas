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

mod backfill;
mod inherit;

pub(crate) use backfill::*;
pub use inherit::*;

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
// OpenDesign's models sometimes end a turn with no text and no tool call (one
// thread saw 15 of them in a day). The preset had no retry list, so each one
// reached Codex as `empty_response` and stopped the turn. Existing settings never
// had the list, so backfill it once.
const OPENDESIGN_EMPTY_RETRY_REVISION: u32 = 29;
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
        "opendesign" => vendors_b::apply_opendesign(provider),
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
mod tests;

#[cfg(test)]
mod argon_wire_id_migration_tests;
