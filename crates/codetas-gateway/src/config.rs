use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};
use url::Url;

pub const SETTINGS_VERSION: u8 = 2;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ProviderProtocol {
    Responses,
    ChatCompletions,
    AnthropicMessages,
    GeminiGenerateContent,
}

impl Default for ProviderProtocol {
    fn default() -> Self {
        Self::Responses
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum GoogleMode {
    #[default]
    AiStudio,
    Vertex,
    CloudCodeAssist,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ProviderTransport {
    #[default]
    Standard,
    Kiro,
    GithubCopilot,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CredentialSource {
    #[default]
    None,
    Environment,
    Keychain,
    OAuth,
    Command,
    Forward,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CredentialTransport {
    #[default]
    Bearer,
    XApiKey,
    CustomHeader,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialCommand {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default = "default_auth_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_auth_refresh_ms")]
    pub refresh_interval_ms: u64,
}

impl Default for CredentialCommand {
    fn default() -> Self {
        Self {
            program: String::new(),
            args: Vec::new(),
            cwd: None,
            timeout_ms: default_auth_timeout_ms(),
            refresh_interval_ms: default_auth_refresh_ms(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCredential {
    #[serde(default)]
    pub source: CredentialSource,
    pub reference: Option<String>,
    #[serde(default)]
    pub transport: CredentialTransport,
    pub header_name: Option<String>,
    pub command: Option<CredentialCommand>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCapabilities {
    #[serde(default = "enabled_by_default")]
    pub streaming: bool,
    #[serde(default = "enabled_by_default")]
    pub tools: bool,
    #[serde(default)]
    pub parallel_tools: bool,
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub audio: bool,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub web_search: bool,
    #[serde(default)]
    pub image_generation: bool,
    #[serde(default)]
    pub video_generation: bool,
    #[serde(default)]
    pub realtime: bool,
    #[serde(default)]
    pub websockets: bool,
    #[serde(default)]
    pub stateful_responses: bool,
}

impl Default for ProviderCapabilities {
    fn default() -> Self {
        Self {
            streaming: true,
            tools: true,
            parallel_tools: false,
            vision: false,
            audio: false,
            reasoning: false,
            web_search: false,
            image_generation: false,
            video_generation: false,
            realtime: false,
            websockets: false,
            stateful_responses: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderLimits {
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
    #[serde(default = "default_stream_idle_timeout_ms")]
    pub stream_idle_timeout_ms: u64,
    #[serde(default = "default_request_retries")]
    pub request_retries: u8,
    #[serde(default = "default_stream_retries")]
    pub stream_retries: u8,
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: u64,
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: u64,
}

impl Default for ProviderLimits {
    fn default() -> Self {
        Self {
            connect_timeout_ms: default_connect_timeout_ms(),
            request_timeout_ms: default_request_timeout_ms(),
            stream_idle_timeout_ms: default_stream_idle_timeout_ms(),
            request_retries: default_request_retries(),
            stream_retries: default_stream_retries(),
            max_request_bytes: default_max_request_bytes(),
            max_response_bytes: default_max_response_bytes(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDiscoverySettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_model_discovery_path")]
    pub path: String,
    #[serde(default = "default_max_models")]
    pub max_models: usize,
}

impl Default for ModelDiscoverySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            path: default_model_discovery_path(),
            max_models: default_max_models(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelMetadata {
    pub provider_id: String,
    pub model_id: String,
    pub display_name: Option<String>,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    pub context_window: Option<u64>,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub input_modalities: Vec<String>,
    #[serde(default)]
    pub reasoning_efforts: Vec<String>,
    pub default_reasoning_effort: Option<String>,
    #[serde(default)]
    pub capabilities: ProviderCapabilities,
    pub input_price_per_million: Option<f64>,
    pub output_price_per_million: Option<f64>,
    /// Complete Codex system prompt template forwarded to the ChatGPT backend as
    /// `model_messages.instructions_template` in the catalog. Without it, Codex CLI
    /// sends only the short `base_instructions` above, and plan-mode models lose the
    /// state-transition guidance needed to finish a plan and end the turn.
    #[serde(default)]
    pub instructions_template: Option<String>,
}

impl Default for ModelMetadata {
    fn default() -> Self {
        Self {
            provider_id: String::new(),
            model_id: String::new(),
            display_name: None,
            enabled: true,
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
            input_modalities: Vec::new(),
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            capabilities: ProviderCapabilities::default(),
            input_price_per_million: None,
            output_price_per_million: None,
            instructions_template: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RouteStrategy {
    #[default]
    Failover,
    WeightedRoundRobin,
    LeastUsage,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteTarget {
    pub model: String,
    #[serde(default = "default_route_weight")]
    pub weight: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteDefinition {
    pub id: String,
    pub name: String,
    pub alias: Option<String>,
    #[serde(default)]
    pub strategy: RouteStrategy,
    #[serde(default)]
    pub targets: Vec<RouteTarget>,
    #[serde(default = "default_sticky_requests")]
    pub sticky_requests: u32,
    #[serde(default = "default_failover_threshold")]
    pub failure_threshold: u8,
    pub default_reasoning_effort: Option<String>,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

impl Default for RouteDefinition {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            alias: None,
            strategy: RouteStrategy::default(),
            targets: Vec::new(),
            sticky_requests: default_sticky_requests(),
            failure_threshold: default_failover_threshold(),
            default_reasoning_effort: None,
            enabled: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSettings {
    #[serde(default = "default_gateway_host")]
    pub host: String,
    #[serde(default = "default_gateway_port")]
    pub port: u16,
    #[serde(default = "enabled_by_default")]
    pub auto_start: bool,
    #[serde(default)]
    pub standalone_service: bool,
    #[serde(default = "enabled_by_default")]
    pub dynamic_port_fallback: bool,
    #[serde(default = "default_shutdown_timeout_ms")]
    pub shutdown_timeout_ms: u64,
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            host: default_gateway_host(),
            port: default_gateway_port(),
            auto_start: true,
            standalone_service: false,
            dynamic_port_fallback: true,
            shutdown_timeout_ms: default_shutdown_timeout_ms(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalAccessKey {
    pub id: String,
    pub label: String,
    pub env_var: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    pub expires_at_unix: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecuritySettings {
    #[serde(default)]
    pub require_local_token: bool,
    #[serde(default)]
    pub allow_remote: bool,
    #[serde(default = "enabled_by_default")]
    pub dns_pinning: bool,
    #[serde(default)]
    pub cors_allow_origins: Vec<String>,
    #[serde(default)]
    pub external_access_keys: Vec<ExternalAccessKey>,
}

impl Default for SecuritySettings {
    fn default() -> Self {
        Self {
            require_local_token: false,
            allow_remote: false,
            dns_pinning: true,
            cors_allow_origins: Vec::new(),
            external_access_keys: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservabilitySettings {
    #[serde(default)]
    pub request_log: bool,
    #[serde(default = "enabled_by_default")]
    pub usage_log: bool,
    #[serde(default = "enabled_by_default")]
    pub redact_content: bool,
    #[serde(default = "default_log_retention_days")]
    pub retention_days: u16,
    #[serde(default = "default_log_budget_bytes")]
    pub max_storage_bytes: u64,
    #[serde(default = "default_trash_retention_days")]
    pub trash_retention_days: u16,
    #[serde(default = "default_log_budget_bytes")]
    pub max_trash_bytes: u64,
}

impl Default for ObservabilitySettings {
    fn default() -> Self {
        Self {
            request_log: false,
            usage_log: true,
            redact_content: true,
            retention_days: default_log_retention_days(),
            max_storage_bytes: default_log_budget_bytes(),
            trash_retention_days: default_trash_retention_days(),
            max_trash_bytes: default_log_budget_bytes(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountReference {
    pub id: String,
    pub provider_id: String,
    pub label: String,
    #[serde(default)]
    pub credential: ProviderCredential,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

impl Default for AccountReference {
    fn default() -> Self {
        Self {
            id: String::new(),
            provider_id: String::new(),
            label: String::new(),
            credential: ProviderCredential::default(),
            enabled: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountPoolSettings {
    #[serde(default)]
    pub accounts: Vec<AccountReference>,
    #[serde(default)]
    pub strategy: AccountPoolStrategy,
    #[serde(default)]
    pub active_accounts: BTreeMap<String, String>,
    #[serde(default = "default_auto_switch_threshold")]
    pub auto_switch_threshold_percent: u8,
    #[serde(default = "default_sticky_requests")]
    pub sticky_requests: u32,
}

impl Default for AccountPoolSettings {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            strategy: AccountPoolStrategy::default(),
            active_accounts: BTreeMap::new(),
            auto_switch_threshold_percent: default_auto_switch_threshold(),
            sticky_requests: default_sticky_requests(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AccountPoolStrategy {
    #[default]
    Quota,
    RoundRobin,
    FillFirst,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSettings {
    #[serde(default)]
    pub multi_agent_v2: bool,
    #[serde(default)]
    pub surface_mode: AgentSurfaceMode,
    #[serde(default = "default_agent_threads")]
    pub max_threads: u16,
    #[serde(default)]
    pub subagent_models: Vec<String>,
    #[serde(default)]
    pub subagent_fallback: Vec<String>,
    pub effort_cap: Option<String>,
    pub subagent_effort_cap: Option<String>,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            multi_agent_v2: false,
            surface_mode: AgentSurfaceMode::default(),
            max_threads: default_agent_threads(),
            subagent_models: Vec::new(),
            subagent_fallback: Vec::new(),
            effort_cap: None,
            subagent_effort_cap: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AgentSurfaceMode {
    V1,
    #[default]
    Default,
    V2,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SidecarSettings {
    pub web_search_model: Option<String>,
    pub vision_model: Option<String>,
    pub image_model: Option<String>,
    pub video_model: Option<String>,
    pub live_model: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseItemIdRepairSettings {
    #[serde(default)]
    pub message: Vec<String>,
    #[serde(default)]
    pub reasoning: Vec<String>,
    #[serde(default)]
    pub repair_missing_terminal_ids: bool,
}

impl ResponseItemIdRepairSettings {
    fn validate(&self) -> Result<(), String> {
        for (label, values) in [
            ("responseItemIdRepair.message", &self.message),
            ("responseItemIdRepair.reasoning", &self.reasoning),
        ] {
            if values.len() > 64 {
                return Err(format!("{label} may contain at most 64 IDs"));
            }
            let mut seen = HashSet::new();
            for value in values {
                validate_single_line(label, value, 128)?;
                if value.trim().is_empty() || !seen.insert(value.as_str()) {
                    return Err(format!("{label} contains an empty or duplicate ID"));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShadowRule {
    pub id: String,
    pub source_model: String,
    #[serde(default)]
    pub targets: Vec<String>,
    #[serde(default = "default_shadow_sample_percent")]
    pub sample_percent: u8,
    #[serde(default = "default_shadow_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_shadow_response_bytes")]
    pub max_response_bytes: u64,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperInterceptSettings {
    #[serde(default)]
    pub enabled: bool,
    pub target_model: Option<String>,
    #[serde(default = "default_helper_source_models")]
    pub source_models: Vec<String>,
}

impl Default for HelperInterceptSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            target_model: None,
            source_models: default_helper_source_models(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexIntegrationSettings {
    #[serde(default = "enabled_by_default")]
    pub auto_connect: bool,
    #[serde(default = "enabled_by_default")]
    pub auto_sync_catalog: bool,
}

impl Default for CodexIntegrationSettings {
    fn default() -> Self {
        Self {
            auto_connect: true,
            auto_sync_catalog: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientIntegrationSettings {
    #[serde(default = "enabled_by_default")]
    pub codex: bool,
    #[serde(default)]
    pub claude_code: bool,
    #[serde(default)]
    pub claude_desktop: bool,
    #[serde(default)]
    pub opencode: bool,
    #[serde(default)]
    pub grok: bool,
    #[serde(default)]
    pub pi: bool,
    #[serde(default)]
    pub claude_desktop_aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub claude_desktop_families: BTreeMap<String, String>,
    #[serde(default)]
    pub claude_desktop_defaults: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum UpdateChannel {
    #[default]
    Stable,
    Beta,
    Nightly,
    Custom,
}

impl UpdateChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
            Self::Nightly => "nightly",
            Self::Custom => "custom",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSettings {
    #[serde(default)]
    pub channel: UpdateChannel,
    #[serde(default)]
    pub auto_check: bool,
    pub manifest_url: Option<String>,
    pub public_key_base64: Option<String>,
    pub installer_endpoint: Option<String>,
    pub installer_public_key: Option<String>,
}

impl Default for ClientIntegrationSettings {
    fn default() -> Self {
        Self {
            codex: true,
            claude_code: false,
            claude_desktop: false,
            opencode: false,
            grok: false,
            pi: false,
            claude_desktop_aliases: BTreeMap::new(),
            claude_desktop_families: BTreeMap::new(),
            claude_desktop_defaults: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDefinition {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub protocol: ProviderProtocol,
    #[serde(default)]
    pub transport: ProviderTransport,
    #[serde(default)]
    pub google_mode: GoogleMode,
    pub project: Option<String>,
    pub location: Option<String>,
    pub azure_deployment: Option<String>,
    pub azure_api_version: Option<String>,
    pub kiro_profile_arn: Option<String>,
    #[serde(default)]
    pub model_protocols: BTreeMap<String, ProviderProtocol>,
    #[serde(default)]
    pub model_wire_ids: BTreeMap<String, String>,
    #[serde(default)]
    pub model_reasoning_modes: BTreeMap<String, String>,
    #[serde(default)]
    pub strip_model_bracket_suffix: bool,
    pub responses_path: Option<String>,
    pub realtime_ws_base_url: Option<String>,
    #[serde(default)]
    pub stateless_responses: bool,
    pub api_key_env: Option<String>,
    pub default_model: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub model_context_windows: BTreeMap<String, u64>,
    #[serde(default)]
    pub model_max_input_tokens: BTreeMap<String, u64>,
    #[serde(default)]
    pub model_max_output_tokens: BTreeMap<String, u64>,
    #[serde(default)]
    pub model_input_modalities: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub model_reasoning_efforts: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub model_default_reasoning_efforts: BTreeMap<String, String>,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    #[serde(default)]
    pub allow_private_network: bool,
    #[serde(default)]
    pub credential: ProviderCredential,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub env_headers: BTreeMap<String, String>,
    #[serde(default)]
    pub query_params: BTreeMap<String, String>,
    #[serde(default)]
    pub reasoning_effort_map: BTreeMap<String, String>,
    #[serde(default)]
    pub model_reasoning_effort_map: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    pub no_reasoning_models: Vec<String>,
    #[serde(default)]
    pub no_temperature_models: Vec<String>,
    #[serde(default)]
    pub no_top_p_models: Vec<String>,
    #[serde(default)]
    pub no_penalty_models: Vec<String>,
    #[serde(default)]
    pub auto_tool_choice_only_models: Vec<String>,
    #[serde(default)]
    pub preserve_reasoning_content_models: Vec<String>,
    #[serde(default)]
    pub reasoning_split_models: Vec<String>,
    #[serde(default)]
    pub thinking_toggle_models: Vec<String>,
    #[serde(default)]
    pub thinking_budget_models: Vec<String>,
    #[serde(default)]
    pub escape_builtin_tool_names: bool,
    #[serde(default)]
    pub response_item_id_repair: ResponseItemIdRepairSettings,
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub prompt_cache_key: bool,
    #[serde(default)]
    pub capabilities: ProviderCapabilities,
    #[serde(default)]
    pub limits: ProviderLimits,
    #[serde(default)]
    pub discovery: ModelDiscoverySettings,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewaySettings {
    pub version: u8,
    pub default_provider: Option<String>,
    #[serde(default)]
    pub providers: Vec<ProviderDefinition>,
    #[serde(default)]
    pub model_catalog: Vec<ModelMetadata>,
    #[serde(default)]
    pub routes: Vec<RouteDefinition>,
    #[serde(default)]
    pub runtime: RuntimeSettings,
    #[serde(default)]
    pub security: SecuritySettings,
    #[serde(default)]
    pub observability: ObservabilitySettings,
    #[serde(default)]
    pub account_pool: AccountPoolSettings,
    #[serde(default)]
    pub agents: AgentSettings,
    #[serde(default)]
    pub sidecars: SidecarSettings,
    #[serde(default)]
    pub shadows: Vec<ShadowRule>,
    #[serde(default)]
    pub helper_intercept: HelperInterceptSettings,
    #[serde(default)]
    pub codex: CodexIntegrationSettings,
    #[serde(default)]
    pub integrations: ClientIntegrationSettings,
    #[serde(default)]
    pub updates: UpdateSettings,
}

impl Default for GatewaySettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            default_provider: None,
            providers: Vec::new(),
            model_catalog: Vec::new(),
            routes: Vec::new(),
            runtime: RuntimeSettings::default(),
            security: SecuritySettings::default(),
            observability: ObservabilitySettings::default(),
            account_pool: AccountPoolSettings::default(),
            agents: AgentSettings::default(),
            sidecars: SidecarSettings::default(),
            shadows: Vec::new(),
            helper_intercept: HelperInterceptSettings::default(),
            codex: CodexIntegrationSettings::default(),
            integrations: ClientIntegrationSettings::default(),
            updates: UpdateSettings::default(),
        }
    }
}

impl Default for ProviderDefinition {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            base_url: String::new(),
            protocol: ProviderProtocol::default(),
            transport: ProviderTransport::default(),
            google_mode: GoogleMode::default(),
            project: None,
            location: None,
            azure_deployment: None,
            azure_api_version: None,
            kiro_profile_arn: None,
            model_protocols: BTreeMap::new(),
            model_wire_ids: BTreeMap::new(),
            model_reasoning_modes: BTreeMap::new(),
            strip_model_bracket_suffix: false,
            responses_path: None,
            realtime_ws_base_url: None,
            stateless_responses: false,
            api_key_env: None,
            default_model: None,
            models: Vec::new(),
            model_context_windows: BTreeMap::new(),
            model_max_input_tokens: BTreeMap::new(),
            model_max_output_tokens: BTreeMap::new(),
            model_input_modalities: BTreeMap::new(),
            model_reasoning_efforts: BTreeMap::new(),
            model_default_reasoning_efforts: BTreeMap::new(),
            enabled: true,
            allow_private_network: false,
            credential: ProviderCredential::default(),
            headers: BTreeMap::new(),
            env_headers: BTreeMap::new(),
            query_params: BTreeMap::new(),
            reasoning_effort_map: BTreeMap::new(),
            model_reasoning_effort_map: BTreeMap::new(),
            no_reasoning_models: Vec::new(),
            no_temperature_models: Vec::new(),
            no_top_p_models: Vec::new(),
            no_penalty_models: Vec::new(),
            auto_tool_choice_only_models: Vec::new(),
            preserve_reasoning_content_models: Vec::new(),
            reasoning_split_models: Vec::new(),
            thinking_toggle_models: Vec::new(),
            thinking_budget_models: Vec::new(),
            escape_builtin_tool_names: false,
            response_item_id_repair: ResponseItemIdRepairSettings::default(),
            parallel_tool_calls: None,
            prompt_cache_key: false,
            capabilities: ProviderCapabilities::default(),
            limits: ProviderLimits::default(),
            discovery: ModelDiscoverySettings::default(),
        }
    }
}

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

impl ProviderDefinition {
    pub fn validate(&self) -> Result<(), String> {
        validate_provider_id(&self.id)?;
        validate_single_line("name", &self.name, 120)?;
        if self.name.trim().is_empty() {
            return Err("name is required".into());
        }

        let url = Url::parse(self.base_url.trim()).map_err(|_| "baseUrl must be a valid URL")?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("baseUrl must use http or https".into());
        }
        if url.host_str().is_none() {
            return Err("baseUrl must include a host".into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("baseUrl must not contain credentials".into());
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err("baseUrl must not contain a query or fragment".into());
        }
        if is_private_destination(&url) && !self.allow_private_network {
            return Err("local or private baseUrl requires allowPrivateNetwork".into());
        }
        if url.scheme() == "http" && !(self.allow_private_network && is_private_destination(&url)) {
            return Err(
                "plaintext HTTP baseUrl is allowed only for an explicitly enabled localhost or private IP"
                    .into(),
            );
        }
        if let Some(path) = self.responses_path.as_deref() {
            if !path.starts_with('/')
                || path.contains("://")
                || path.contains('?')
                || path.contains('#')
            {
                return Err(
                    "responsesPath must be an absolute path without URL, query, or fragment".into(),
                );
            }
        }
        if let Some(value) = self.realtime_ws_base_url.as_deref() {
            let realtime =
                Url::parse(value).map_err(|_| "realtimeWsBaseUrl must be a valid URL")?;
            let secure = matches!(realtime.scheme(), "https" | "wss");
            let local_plaintext = matches!(realtime.scheme(), "http" | "ws")
                && self.allow_private_network
                && is_private_destination(&realtime);
            if (!secure && !local_plaintext)
                || realtime.host_str().is_none()
                || !realtime.username().is_empty()
                || realtime.password().is_some()
                || realtime.query().is_some()
                || realtime.fragment().is_some()
            {
                return Err(
                    "realtimeWsBaseUrl must be credential-free wss/https, or ws/http for an allowed private destination"
                        .into(),
                );
            }
        }
        if self.google_mode != GoogleMode::AiStudio
            && self.protocol != ProviderProtocol::GeminiGenerateContent
        {
            return Err("googleMode vertex/cloudCodeAssist requires geminiGenerateContent".into());
        }
        if self.transport == ProviderTransport::Kiro {
            if self.protocol != ProviderProtocol::Responses {
                return Err("Kiro transport requires the Responses protocol surface".into());
            }
            if let Some(profile_arn) = self.kiro_profile_arn.as_deref() {
                validate_single_line("kiroProfileArn", profile_arn, 1_024)?;
                if !profile_arn.starts_with("arn:") {
                    return Err("kiroProfileArn must be an ARN".into());
                }
            }
        } else if self.kiro_profile_arn.is_some() {
            return Err("kiroProfileArn requires the Kiro transport".into());
        }
        if self.transport == ProviderTransport::GithubCopilot {
            if self.protocol != ProviderProtocol::ChatCompletions {
                return Err("GitHub Copilot transport requires chatCompletions".into());
            }
            let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
            if url.scheme() != "https"
                || (host != "api.githubcopilot.com" && !host.ends_with(".githubcopilot.com"))
            {
                return Err(
                    "GitHub Copilot transport requires an HTTPS *.githubcopilot.com baseUrl".into(),
                );
            }
        }
        for (label, value) in [
            ("project", self.project.as_deref()),
            ("location", self.location.as_deref()),
        ] {
            if let Some(value) = value {
                validate_single_line(label, value, 240)?;
                if value.trim().is_empty()
                    || value.bytes().any(|byte| {
                        !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
                    })
                {
                    return Err(format!("{label} must use a printable cloud identifier"));
                }
            }
        }
        match (
            self.azure_deployment.as_deref(),
            self.azure_api_version.as_deref(),
        ) {
            (None, None) => {}
            (Some(deployment), Some(version)) => {
                if self.transport != ProviderTransport::Standard
                    || !matches!(
                        self.protocol,
                        ProviderProtocol::Responses | ProviderProtocol::ChatCompletions
                    )
                    || !url
                        .host_str()
                        .is_some_and(|host| host.ends_with(".openai.azure.com"))
                {
                    return Err(
                        "azureDeployment requires a Standard Responses/Chat provider on *.openai.azure.com"
                            .into(),
                    );
                }
                if self.model_protocols.values().any(|protocol| {
                    !matches!(
                        protocol,
                        ProviderProtocol::Responses | ProviderProtocol::ChatCompletions
                    )
                }) {
                    return Err(
                        "Azure deployment model protocol overrides must remain Responses or Chat"
                            .into(),
                    );
                }
                for (label, value) in [
                    ("azureDeployment", deployment),
                    ("azureApiVersion", version),
                ] {
                    validate_single_line(label, value, 240)?;
                    if value.is_empty()
                        || value.bytes().any(|byte| {
                            !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
                        })
                    {
                        return Err(format!("{label} contains unsupported characters"));
                    }
                }
            }
            _ => {
                return Err(
                    "azureDeployment and azureApiVersion must be configured together".into(),
                )
            }
        }

        if let Some(env_key) = self.api_key_env.as_deref() {
            validate_env_key(env_key)?;
        }
        self.credential.validate()?;
        for (name, value) in &self.headers {
            validate_header_name(name)?;
            if is_forbidden_transport_header(name) {
                return Err(format!(
                    "transport-owned header {name} cannot be configured"
                ));
            }
            if is_sensitive_name(name) {
                return Err(format!(
                    "sensitive header {name} must use credential or envHeaders instead of persisted headers"
                ));
            }
            validate_single_line("header value", value, 4_096)?;
        }
        for (name, env_key) in &self.env_headers {
            validate_header_name(name)?;
            if is_forbidden_transport_header(name) {
                return Err(format!(
                    "transport-owned header {name} cannot be configured"
                ));
            }
            validate_env_key(env_key)?;
        }
        for (name, value) in &self.query_params {
            validate_single_line("query parameter name", name, 256)?;
            validate_single_line("query parameter value", value, 2_048)?;
            if name.trim().is_empty() {
                return Err("query parameter name cannot be empty".into());
            }
            if is_sensitive_name(name) {
                return Err(format!(
                    "sensitive query parameter {name} cannot be persisted"
                ));
            }
        }
        if let Some(model) = self.default_model.as_deref() {
            validate_model_id(model)?;
        }
        if self.models.len() > 250 {
            return Err("models must contain at most 250 entries".into());
        }
        for model in &self.models {
            validate_model_id(model)?;
        }
        for model in self.model_protocols.keys() {
            validate_model_id(model)?;
        }
        if self.model_wire_ids.len() > 250 || self.model_reasoning_modes.len() > 250 {
            return Err(
                "modelWireIds and modelReasoningModes must contain at most 250 entries".into(),
            );
        }
        for (model, wire_model) in &self.model_wire_ids {
            validate_model_id(model)?;
            validate_model_id(wire_model)?;
            if model == wire_model {
                return Err(format!(
                    "modelWireIds.{model} must map to a different model id"
                ));
            }
        }
        for (model, mode) in &self.model_reasoning_modes {
            validate_model_id(model)?;
            validate_single_line("model reasoning mode", mode, 64)?;
            if mode.trim().is_empty() {
                return Err(format!("modelReasoningModes.{model} cannot be empty"));
            }
        }
        for (label, limits) in [
            ("modelContextWindows", &self.model_context_windows),
            ("modelMaxInputTokens", &self.model_max_input_tokens),
            ("modelMaxOutputTokens", &self.model_max_output_tokens),
        ] {
            if limits.len() > 250 {
                return Err(format!("{label} must contain at most 250 entries"));
            }
            for (model, limit) in limits {
                validate_model_id(model)?;
                if *limit == 0 || *limit > 100_000_000 {
                    return Err(format!("{label}.{model} must be between 1 and 100000000"));
                }
            }
        }
        for (model, max_input) in &self.model_max_input_tokens {
            if self
                .model_context_windows
                .get(model)
                .is_some_and(|context| max_input > context)
            {
                return Err(format!(
                    "modelMaxInputTokens.{model} cannot exceed modelContextWindows"
                ));
            }
        }
        for (model, max_output) in &self.model_max_output_tokens {
            if self
                .model_context_windows
                .get(model)
                .is_some_and(|context| max_output > context)
            {
                return Err(format!(
                    "modelMaxOutputTokens.{model} cannot exceed modelContextWindows"
                ));
            }
        }
        if self.model_input_modalities.len() > 250 {
            return Err("modelInputModalities must contain at most 250 entries".into());
        }
        for (model, modalities) in &self.model_input_modalities {
            validate_model_id(model)?;
            if modalities.is_empty() || modalities.len() > 4 {
                return Err(format!(
                    "modelInputModalities.{model} must contain between 1 and 4 values"
                ));
            }
            let mut seen = HashSet::new();
            for modality in modalities {
                if !matches!(modality.as_str(), "text" | "image" | "audio" | "video")
                    || !seen.insert(modality.as_str())
                {
                    return Err(format!(
                        "modelInputModalities.{model} contains an unsupported or duplicate value"
                    ));
                }
            }
        }
        if self.model_reasoning_efforts.len() > 250
            || self.model_default_reasoning_efforts.len() > 250
        {
            return Err(
                "modelReasoningEfforts and modelDefaultReasoningEfforts must contain at most 250 entries"
                    .into(),
            );
        }
        for (model, efforts) in &self.model_reasoning_efforts {
            validate_model_id(model)?;
            let mut seen = HashSet::new();
            for effort in efforts {
                if !is_reasoning_effort(effort) || !seen.insert(effort.as_str()) {
                    return Err(format!(
                        "modelReasoningEfforts.{model} contains an unsupported or duplicate effort"
                    ));
                }
            }
        }
        for (model, default) in &self.model_default_reasoning_efforts {
            validate_model_id(model)?;
            if !is_reasoning_effort(default)
                || self
                    .model_reasoning_efforts
                    .get(model)
                    .is_some_and(|efforts| !efforts.is_empty() && !efforts.contains(default))
            {
                return Err(format!(
                    "modelDefaultReasoningEfforts.{model} is not supported by that model"
                ));
            }
        }
        for (label, models) in [
            ("noReasoningModels", &self.no_reasoning_models),
            ("noTemperatureModels", &self.no_temperature_models),
            ("noTopPModels", &self.no_top_p_models),
            ("noPenaltyModels", &self.no_penalty_models),
            (
                "autoToolChoiceOnlyModels",
                &self.auto_tool_choice_only_models,
            ),
            (
                "preserveReasoningContentModels",
                &self.preserve_reasoning_content_models,
            ),
            ("reasoningSplitModels", &self.reasoning_split_models),
            ("thinkingToggleModels", &self.thinking_toggle_models),
            ("thinkingBudgetModels", &self.thinking_budget_models),
        ] {
            if models.len() > 250 {
                return Err(format!("{label} must contain at most 250 entries"));
            }
            let mut seen = HashSet::new();
            for model in models {
                validate_model_id(model)?;
                if !seen.insert(model.as_str()) {
                    return Err(format!("{label} contains a duplicate model: {model}"));
                }
            }
        }
        self.response_item_id_repair.validate()?;
        for (effort, wire) in &self.reasoning_effort_map {
            if !is_reasoning_effort(effort) {
                return Err(format!("unsupported reasoning effort map key: {effort}"));
            }
            validate_single_line("reasoning effort wire value", wire, 64)?;
            if wire.trim().is_empty() {
                return Err("reasoning effort wire value cannot be empty".into());
            }
        }
        for (model, mapping) in &self.model_reasoning_effort_map {
            validate_model_id(model)?;
            for (effort, wire) in mapping {
                if !is_reasoning_effort(effort) {
                    return Err(format!(
                        "unsupported model reasoning effort map key: {effort}"
                    ));
                }
                validate_single_line("model reasoning effort wire value", wire, 64)?;
                if wire.trim().is_empty() {
                    return Err("model reasoning effort wire value cannot be empty".into());
                }
            }
        }
        if self.limits.connect_timeout_ms == 0
            || self.limits.request_timeout_ms == 0
            || self.limits.stream_idle_timeout_ms == 0
        {
            return Err("provider timeouts must be greater than zero".into());
        }
        if self.limits.request_retries > 10 || self.limits.stream_retries > 10 {
            return Err("provider retries must be between 0 and 10".into());
        }
        if !(1_024..=64 * 1024 * 1024).contains(&self.limits.max_request_bytes) {
            return Err("provider request limit must be between 1 KiB and 64 MiB".into());
        }
        if !(1_024..=512 * 1024 * 1024).contains(&self.limits.max_response_bytes) {
            return Err("provider response limit must be between 1 KiB and 512 MiB".into());
        }
        if self.discovery.enabled {
            if !self.discovery.path.starts_with('/')
                || self.discovery.path.contains("://")
                || self.discovery.path.contains('?')
                || self.discovery.path.contains('#')
            {
                return Err(
                    "model discovery path must be an absolute path without URL, query, or fragment"
                        .into(),
                );
            }
            if self.discovery.max_models == 0 || self.discovery.max_models > 5_000 {
                return Err("model discovery maxModels must be between 1 and 5000".into());
            }
        }
        Ok(())
    }

    pub fn endpoint(&self) -> String {
        self.endpoint_for_model(self.default_model.as_deref().unwrap_or("model"))
    }

    pub fn wire_model_id(&self, model: &str) -> String {
        if let Some(wire) = self.model_wire_ids.get(model) {
            return wire.clone();
        }
        if self.strip_model_bracket_suffix && model.ends_with(']') {
            if let Some(index) = model.rfind('[') {
                if index > 0 {
                    return model[..index].to_string();
                }
            }
        }
        model.to_string()
    }

    pub fn endpoint_for_model(&self, model: &str) -> String {
        self.endpoint_for_model_streaming(model, false)
    }

    pub fn endpoint_for_model_streaming(&self, model: &str, streaming: bool) -> String {
        let base = self.base_url.trim().trim_end_matches('/');
        if let Some(deployment) = self.azure_deployment.as_deref() {
            let root = base
                .split_once("/openai/")
                .map(|(root, _)| root)
                .unwrap_or(base);
            let resource = match self.protocol_for_model(model) {
                ProviderProtocol::Responses => "responses",
                ProviderProtocol::ChatCompletions => "chat/completions",
                _ => unreachable!("Azure deployment protocol is validated"),
            };
            return format!("{root}/openai/deployments/{deployment}/{resource}");
        }
        match self.protocol_for_model(model) {
            ProviderProtocol::Responses if self.responses_path.is_some() => {
                match Url::parse(base) {
                    Ok(mut url) => {
                        url.set_path(self.responses_path.as_deref().unwrap_or("/responses"));
                        url.set_query(None);
                        url.set_fragment(None);
                        url.to_string()
                    }
                    Err(_) => format!(
                        "{base}{}",
                        self.responses_path.as_deref().unwrap_or("/responses")
                    ),
                }
            }
            ProviderProtocol::Responses if base.ends_with("/responses") => base.to_string(),
            ProviderProtocol::Responses => format!("{base}/responses"),
            ProviderProtocol::ChatCompletions if base.ends_with("/chat/completions") => {
                base.to_string()
            }
            ProviderProtocol::ChatCompletions => format!("{base}/chat/completions"),
            ProviderProtocol::AnthropicMessages if base.ends_with("/messages") => base.to_string(),
            ProviderProtocol::AnthropicMessages => format!("{base}/messages"),
            ProviderProtocol::GeminiGenerateContent
                if self.google_mode == GoogleMode::CloudCodeAssist =>
            {
                let method = if streaming {
                    "streamGenerateContent"
                } else {
                    "generateContent"
                };
                format!("{base}/v1internal:{method}")
            }
            ProviderProtocol::GeminiGenerateContent if self.google_mode == GoogleMode::Vertex => {
                let method = if streaming {
                    "streamGenerateContent"
                } else {
                    "generateContent"
                };
                match (self.project.as_deref(), self.location.as_deref()) {
                    (Some(project), Some(location)) => {
                        let root = if base == "https://aiplatform.googleapis.com"
                            && location != "global"
                        {
                            format!("https://{location}-aiplatform.googleapis.com")
                        } else {
                            base.to_string()
                        };
                        format!(
                            "{root}/v1/projects/{project}/locations/{location}/publishers/google/models/{}:{method}",
                            model.replace('/', "%2F")
                        )
                    }
                    _ => format!(
                        "{base}/v1/publishers/google/models/{}:{method}",
                        model.replace('/', "%2F")
                    ),
                }
            }
            ProviderProtocol::GeminiGenerateContent if base.contains("{model}") => {
                base.replace("{model}", &model.replace('/', "%2F")).replace(
                    "{method}",
                    if streaming {
                        "streamGenerateContent"
                    } else {
                        "generateContent"
                    },
                )
            }
            ProviderProtocol::GeminiGenerateContent => {
                let method = if streaming {
                    "streamGenerateContent"
                } else {
                    "generateContent"
                };
                format!("{base}/models/{}:{method}", model.replace('/', "%2F"))
            }
        }
    }

    pub fn protocol_for_model(&self, model: &str) -> ProviderProtocol {
        self.model_protocols
            .get(model)
            .copied()
            .unwrap_or(self.protocol)
    }

    pub fn compact_endpoint(&self) -> String {
        let base = self.base_url.trim().trim_end_matches('/');
        if base.ends_with("/responses/compact") {
            base.to_string()
        } else if base.ends_with("/responses") {
            format!("{base}/compact")
        } else {
            format!("{base}/responses/compact")
        }
    }

    pub fn image_endpoint(&self, edits: bool) -> String {
        resource_endpoint(
            &self.base_url,
            if edits {
                "images/edits"
            } else {
                "images/generations"
            },
        )
    }

    pub fn search_endpoint(&self) -> String {
        resource_endpoint(&self.base_url, "alpha/search")
    }

    pub fn video_endpoint(&self) -> String {
        resource_endpoint(&self.base_url, "videos/generations")
    }
}

fn resource_endpoint(base_url: &str, resource: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    let suffix = format!("/{resource}");
    if base.ends_with(&suffix) {
        base.to_string()
    } else {
        format!("{base}{suffix}")
    }
}

impl ProviderCredential {
    fn validate(&self) -> Result<(), String> {
        match self.source {
            CredentialSource::None => {
                if self.command.is_some() {
                    return Err("credential command requires command source".into());
                }
            }
            CredentialSource::Environment => {
                let reference = self
                    .reference
                    .as_deref()
                    .ok_or("environment credential requires a reference")?;
                validate_env_key(reference)?;
            }
            CredentialSource::Keychain => {
                let reference = self
                    .reference
                    .as_deref()
                    .ok_or("credential source requires a reference")?;
                validate_single_line("credential reference", reference, 240)?;
                if reference.trim().is_empty() {
                    return Err("credential reference cannot be empty".into());
                }
            }
            CredentialSource::OAuth => {
                if let Some(command) = self.command.as_ref() {
                    validate_credential_command(command)?;
                } else {
                    let reference = self.reference.as_deref().ok_or(
                        "OAuth credential requires a provider reference or broker command",
                    )?;
                    validate_single_line("OAuth credential reference", reference, 240)?;
                    if reference.trim().is_empty() {
                        return Err("OAuth credential reference cannot be empty".into());
                    }
                }
            }
            CredentialSource::Command => {
                let command = self
                    .command
                    .as_ref()
                    .ok_or("command credential requires command settings")?;
                validate_credential_command(command)?;
            }
            CredentialSource::Forward => {
                if self.reference.is_some() || self.command.is_some() {
                    return Err("forward credential cannot store a reference or command".into());
                }
            }
        }
        if self.source != CredentialSource::Forward
            && self.transport == CredentialTransport::CustomHeader
        {
            validate_header_name(
                self.header_name
                    .as_deref()
                    .ok_or("custom header credential requires headerName")?,
            )?;
        }
        Ok(())
    }
}

fn validate_credential_command(command: &CredentialCommand) -> Result<(), String> {
    validate_single_line("credential command", &command.program, 1_024)?;
    if command.program.trim().is_empty()
        || command.timeout_ms == 0
        || command.timeout_ms > 60_000
        || command.refresh_interval_ms > 86_400_000
    {
        return Err(
            "credential command requires a program, timeout 1-60000 ms, and refresh interval up to 24 hours"
                .into(),
        );
    }
    for argument in &command.args {
        validate_single_line("credential command argument", argument, 4_096)?;
    }
    if let Some(cwd) = command.cwd.as_deref() {
        validate_single_line("credential command cwd", cwd, 4_096)?;
    }
    Ok(())
}

impl GatewaySettings {
    pub fn prune_stale_client_integrations(&mut self) {
        if !self.integrations.claude_desktop {
            self.integrations.claude_desktop_aliases.clear();
            self.integrations.claude_desktop_families.clear();
            self.integrations.claude_desktop_defaults.clear();
            return;
        }
        let provider_ids = self
            .providers
            .iter()
            .filter(|provider| provider.enabled)
            .map(|provider| provider.id.as_str())
            .collect::<HashSet<_>>();
        let route_ids = self
            .routes
            .iter()
            .filter(|route| route.enabled)
            .flat_map(|route| [Some(route.id.as_str()), route.alias.as_deref()])
            .flatten()
            .collect::<HashSet<_>>();
        self.integrations
            .claude_desktop_aliases
            .retain(|_, target| {
                validate_routing_reference(target, &provider_ids, &route_ids).is_ok()
            });
        let targets = self
            .integrations
            .claude_desktop_aliases
            .values()
            .cloned()
            .collect::<HashSet<_>>();
        self.integrations
            .claude_desktop_families
            .retain(|target, family| {
                targets.contains(target)
                    && matches!(family.as_str(), "opus" | "fable" | "sonnet" | "haiku")
            });
        let families = self.integrations.claude_desktop_families.clone();
        self.integrations
            .claude_desktop_defaults
            .retain(|family, target| {
                families
                    .get(target)
                    .is_some_and(|assigned| assigned == family)
            });
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != SETTINGS_VERSION {
            return Err(format!("unsupported settings version: {}", self.version));
        }
        let mut ids = HashSet::new();
        for provider in &self.providers {
            provider.validate()?;
            if !ids.insert(provider.id.as_str()) {
                return Err(format!("duplicate provider id: {}", provider.id));
            }
        }
        if let Some(default_provider) = self.default_provider.as_deref() {
            if !self
                .providers
                .iter()
                .any(|provider| provider.id == default_provider && provider.enabled)
            {
                return Err("defaultProvider must reference an enabled provider".into());
            }
        }
        if self.runtime.port == 0 {
            return Err("runtime port must be greater than zero".into());
        }
        if !(100..=300_000).contains(&self.runtime.shutdown_timeout_ms) {
            return Err("runtime shutdown timeout must be between 100 ms and 5 minutes".into());
        }
        validate_single_line("runtime host", &self.runtime.host, 255)?;
        let runtime_ip = if self.runtime.host == "localhost" {
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST))
        } else {
            Some(
                self.runtime
                    .host
                    .parse::<IpAddr>()
                    .map_err(|_| "runtime host must be localhost or an IP address")?,
            )
        };
        let loopback_host = runtime_ip.is_some_and(|ip| ip.is_loopback());
        if !loopback_host && !self.security.allow_remote {
            return Err("non-loopback runtime host requires allowRemote".into());
        }
        let external_auth_enabled = self
            .security
            .external_access_keys
            .iter()
            .any(|key| key.enabled);
        if self.security.allow_remote
            && !self.security.require_local_token
            && !external_auth_enabled
        {
            return Err("remote access requires local or scoped external authentication".into());
        }
        for origin in &self.security.cors_allow_origins {
            validate_cors_origin(origin)?;
        }
        match (
            self.updates.manifest_url.as_deref(),
            self.updates.public_key_base64.as_deref(),
        ) {
            (None, None) if !self.updates.auto_check => {}
            (Some(url), Some(key)) => {
                let url = Url::parse(url).map_err(|_| "update manifest URL is invalid")?;
                if url.scheme() != "https"
                    || url.host_str().is_none()
                    || url.username() != ""
                    || url.password().is_some()
                    || url.fragment().is_some()
                {
                    return Err("update manifest URL must be a credential-free HTTPS URL".into());
                }
                validate_single_line("update public key", key, 256)?;
                if key.trim().is_empty() {
                    return Err("update public key cannot be empty".into());
                }
            }
            _ => {
                return Err(
                    "update manifest URL and public key must be configured together; autoCheck requires both"
                        .into(),
                )
            }
        }
        match (
            self.updates.installer_endpoint.as_deref(),
            self.updates.installer_public_key.as_deref(),
        ) {
            (None, None) => {}
            (Some(endpoint), Some(key)) => {
                let url =
                    Url::parse(endpoint).map_err(|_| "update installer endpoint is invalid")?;
                if url.scheme() != "https"
                    || url.host_str().is_none()
                    || url.username() != ""
                    || url.password().is_some()
                    || url.fragment().is_some()
                {
                    return Err(
                        "update installer endpoint must be a credential-free HTTPS URL".into(),
                    );
                }
                validate_single_line("update installer public key", key, 4_096)?;
                if key.trim().is_empty() {
                    return Err("update installer public key cannot be empty".into());
                }
            }
            _ => return Err(
                "update installer endpoint and installer public key must be configured together"
                    .into(),
            ),
        }
        let mut access_key_ids = HashSet::new();
        for key in &self.security.external_access_keys {
            validate_provider_id(&key.id)?;
            validate_single_line("external access key label", &key.label, 120)?;
            validate_env_key(&key.env_var)?;
            if key.label.trim().is_empty() {
                return Err(format!("external access key {} requires a label", key.id));
            }
            if !access_key_ids.insert(key.id.as_str()) {
                return Err(format!("duplicate external access key id: {}", key.id));
            }
            if key.scopes.is_empty() {
                return Err(format!(
                    "external access key {} requires at least one scope",
                    key.id
                ));
            }
            let mut scopes = HashSet::new();
            for scope in &key.scopes {
                if !matches!(
                    scope.as_str(),
                    "gateway:*"
                        | "health:read"
                        | "models:read"
                        | "responses:write"
                        | "chat:write"
                        | "messages:write"
                        | "gemini:write"
                        | "tokens:count"
                        | "images:write"
                        | "search:write"
                        | "videos:write"
                        | "realtime:write"
                        | "sidecars:write"
                ) {
                    return Err(format!("unsupported external access scope: {scope}"));
                }
                if !scopes.insert(scope.as_str()) {
                    return Err(format!(
                        "duplicate scope on external access key {}: {scope}",
                        key.id
                    ));
                }
            }
        }
        if self.account_pool.auto_switch_threshold_percent > 100 {
            return Err("account auto-switch threshold must be between 0 and 100".into());
        }
        if self.agents.max_threads == 0 || self.agents.max_threads > 128 {
            return Err("agent maxThreads must be between 1 and 128".into());
        }
        if !self.observability.redact_content {
            return Err("observability redactContent must remain enabled".into());
        }
        if self.observability.retention_days == 0 || self.observability.retention_days > 3_650 {
            return Err("observability retentionDays must be between 1 and 3650".into());
        }
        if !(64_u64 * 1024..=10_u64 * 1024 * 1024 * 1024)
            .contains(&self.observability.max_storage_bytes)
        {
            return Err("observability maxStorageBytes must be between 64 KiB and 10 GiB".into());
        }
        if self.observability.trash_retention_days == 0
            || self.observability.trash_retention_days > 365
        {
            return Err("observability trashRetentionDays must be between 1 and 365".into());
        }
        if !(64_u64 * 1024..=10_u64 * 1024 * 1024 * 1024)
            .contains(&self.observability.max_trash_bytes)
        {
            return Err("observability maxTrashBytes must be between 64 KiB and 10 GiB".into());
        }

        let mut model_keys = HashSet::new();
        for model in &self.model_catalog {
            if !ids.contains(model.provider_id.as_str()) {
                return Err(format!(
                    "model references unknown provider: {}",
                    model.provider_id
                ));
            }
            validate_model_id(&model.model_id)?;
            let key = format!("{}/{}", model.provider_id, model.model_id);
            if !model_keys.insert(key.clone()) {
                return Err(format!("duplicate model metadata: {key}"));
            }
            for price in [
                model.input_price_per_million,
                model.output_price_per_million,
            ]
            .into_iter()
            .flatten()
            {
                if !price.is_finite() || price < 0.0 {
                    return Err(format!(
                        "model pricing must be finite and non-negative: {key}"
                    ));
                }
            }
            for modality in &model.input_modalities {
                if !matches!(modality.as_str(), "text" | "image" | "audio" | "video") {
                    return Err(format!("unsupported model input modality: {modality}"));
                }
            }
            for (name, limit) in [
                ("contextWindow", model.context_window),
                ("maxInputTokens", model.max_input_tokens),
                ("maxOutputTokens", model.max_output_tokens),
            ] {
                if limit == Some(0) {
                    return Err(format!("model {name} must be greater than zero: {key}"));
                }
            }
            if let Some(context) = model.context_window {
                if model.max_input_tokens.is_some_and(|limit| limit > context)
                    || model.max_output_tokens.is_some_and(|limit| limit > context)
                {
                    return Err(format!(
                        "model token limits cannot exceed contextWindow: {key}"
                    ));
                }
            }
            let mut reasoning_efforts = HashSet::new();
            for effort in &model.reasoning_efforts {
                if !is_reasoning_effort(effort) {
                    return Err(format!("unsupported model reasoning effort: {effort}"));
                }
                if !reasoning_efforts.insert(effort.as_str()) {
                    return Err(format!(
                        "duplicate model reasoning effort on {key}: {effort}"
                    ));
                }
            }
            if let Some(default) = model.default_reasoning_effort.as_deref() {
                if !is_reasoning_effort(default)
                    || (!model.reasoning_efforts.is_empty() && !reasoning_efforts.contains(default))
                {
                    return Err(format!(
                        "invalid default reasoning effort on {key}: {default}"
                    ));
                }
            }
        }

        let mut route_ids = HashSet::new();
        for route in &self.routes {
            validate_provider_id(&route.id)?;
            validate_single_line("route name", &route.name, 120)?;
            if !route_ids.insert(route.id.as_str()) || ids.contains(route.id.as_str()) {
                return Err(format!("duplicate or colliding route id: {}", route.id));
            }
            if let Some(alias) = route.alias.as_deref() {
                validate_model_id(alias)?;
                if alias.contains('/') || !route_ids.insert(alias) || ids.contains(alias) {
                    return Err(format!("duplicate or colliding route alias: {alias}"));
                }
            }
            if route
                .default_reasoning_effort
                .as_deref()
                .is_some_and(|effort| !is_reasoning_effort(effort))
            {
                return Err(format!(
                    "route {} has an invalid default reasoning effort",
                    route.id
                ));
            }
            if route.targets.is_empty() {
                return Err(format!("route {} requires at least one target", route.id));
            }
            if route.sticky_requests == 0 || route.failure_threshold == 0 {
                return Err(format!(
                    "route {} requires positive stickyRequests and failureThreshold values",
                    route.id
                ));
            }
            for target in &route.targets {
                let (provider_id, model_id) = target.model.split_once('/').ok_or_else(|| {
                    format!("route target must use provider/model: {}", target.model)
                })?;
                if !ids.contains(provider_id) {
                    return Err(format!(
                        "route target references unknown provider: {provider_id}"
                    ));
                }
                validate_model_id(model_id)?;
                if target.weight == 0 {
                    return Err("route target weight must be greater than zero".into());
                }
            }
        }

        if self.helper_intercept.enabled {
            let target = self
                .helper_intercept
                .target_model
                .as_deref()
                .ok_or("enabled helper intercept requires targetModel")?;
            validate_routing_reference(target, &ids, &route_ids)?;
            if self.helper_intercept.source_models.is_empty()
                || self.helper_intercept.source_models.len() > 32
            {
                return Err("helper intercept requires 1-32 source models".into());
            }
            let mut source_models = HashSet::new();
            for source in &self.helper_intercept.source_models {
                validate_model_id(source)?;
                if source == target {
                    return Err("helper intercept target cannot equal a source model".into());
                }
                if !source_models.insert(source.as_str()) {
                    return Err(format!("duplicate helper intercept source model: {source}"));
                }
            }
        } else if self.helper_intercept.target_model.is_some() {
            if let Some(target) = self.helper_intercept.target_model.as_deref() {
                validate_routing_reference(target, &ids, &route_ids)?;
            }
        }

        if self.agents.subagent_models.len() > 32 || self.agents.subagent_fallback.len() > 32 {
            return Err("agent model rosters may contain at most 32 entries".into());
        }
        for effort in [
            self.agents.effort_cap.as_deref(),
            self.agents.subagent_effort_cap.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if !is_reasoning_effort(effort) {
                return Err(format!("unsupported agent reasoning effort cap: {effort}"));
            }
        }
        for target in self
            .agents
            .subagent_models
            .iter()
            .chain(self.agents.subagent_fallback.iter())
        {
            validate_routing_reference(target, &ids, &route_ids)?;
        }

        let mut shadow_ids = HashSet::new();
        for rule in &self.shadows {
            validate_provider_id(&rule.id)?;
            if !shadow_ids.insert(rule.id.as_str()) {
                return Err(format!("duplicate shadow rule id: {}", rule.id));
            }
            validate_routing_reference(&rule.source_model, &ids, &route_ids)?;
            if rule.targets.is_empty() || rule.targets.len() > 4 {
                return Err(format!("shadow rule {} requires 1-4 targets", rule.id));
            }
            if rule.sample_percent == 0 || rule.sample_percent > 100 {
                return Err(format!(
                    "shadow rule {} samplePercent must be 1-100",
                    rule.id
                ));
            }
            if !(100..=60_000).contains(&rule.timeout_ms) {
                return Err(format!(
                    "shadow rule {} timeoutMs must be 100-60000",
                    rule.id
                ));
            }
            if !(1_024..=16 * 1024 * 1024).contains(&rule.max_response_bytes) {
                return Err(format!(
                    "shadow rule {} maxResponseBytes must be 1 KiB-16 MiB",
                    rule.id
                ));
            }
            let mut targets = HashSet::new();
            for target in &rule.targets {
                validate_routing_reference(target, &ids, &route_ids)?;
                if target == &rule.source_model {
                    return Err(format!(
                        "shadow rule {} cannot target its own source",
                        rule.id
                    ));
                }
                if !targets.insert(target.as_str()) {
                    return Err(format!(
                        "duplicate target in shadow rule {}: {target}",
                        rule.id
                    ));
                }
            }
        }

        for (kind, target, capability) in [
            (
                "webSearchModel",
                self.sidecars.web_search_model.as_deref(),
                SidecarCapability::WebSearch,
            ),
            (
                "visionModel",
                self.sidecars.vision_model.as_deref(),
                SidecarCapability::Vision,
            ),
            (
                "imageModel",
                self.sidecars.image_model.as_deref(),
                SidecarCapability::ImageGeneration,
            ),
            (
                "videoModel",
                self.sidecars.video_model.as_deref(),
                SidecarCapability::VideoGeneration,
            ),
            (
                "liveModel",
                self.sidecars.live_model.as_deref(),
                SidecarCapability::Realtime,
            ),
        ] {
            let Some(target) = target else { continue };
            validate_routing_reference(target, &ids, &route_ids)?;
            if !routing_reference_supports(self, target, capability) {
                return Err(format!(
                    "sidecar {kind} target does not advertise the required capability: {target}"
                ));
            }
        }

        if self.integrations.claude_desktop_aliases.len() > 365
            || self.integrations.claude_desktop_families.len() > 365
            || self.integrations.claude_desktop_defaults.len() > 4
        {
            return Err("Claude Desktop profile exceeds the CODETAS limits".into());
        }
        let desktop_routes = self
            .integrations
            .claude_desktop_aliases
            .values()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        for (alias, target) in &self.integrations.claude_desktop_aliases {
            validate_model_id(alias)?;
            if alias.contains('/')
                || ids.contains(alias.as_str())
                || route_ids.contains(alias.as_str())
            {
                return Err(format!(
                    "Claude Desktop alias collides with a provider or route: {alias}"
                ));
            }
            validate_routing_reference(target, &ids, &route_ids)?;
        }
        for (target, family) in &self.integrations.claude_desktop_families {
            if !desktop_routes.contains(target.as_str())
                || !matches!(family.as_str(), "opus" | "fable" | "sonnet" | "haiku")
            {
                return Err(format!(
                    "invalid Claude Desktop family assignment: {target}"
                ));
            }
        }
        for (family, target) in &self.integrations.claude_desktop_defaults {
            if !matches!(family.as_str(), "opus" | "fable" | "sonnet" | "haiku")
                || !desktop_routes.contains(target.as_str())
                || self
                    .integrations
                    .claude_desktop_families
                    .get(target)
                    .is_none_or(|assigned| assigned != family)
            {
                return Err(format!(
                    "invalid Claude Desktop default for family {family}"
                ));
            }
        }

        let mut account_ids = HashSet::new();
        for account in &self.account_pool.accounts {
            validate_provider_id(&account.id)?;
            validate_single_line("account label", &account.label, 120)?;
            if account.label.trim().is_empty() {
                return Err(format!("account {} requires a label", account.id));
            }
            if !account_ids.insert(account.id.as_str()) {
                return Err(format!("duplicate account id: {}", account.id));
            }
            if !ids.contains(account.provider_id.as_str()) {
                return Err(format!(
                    "account references unknown provider: {}",
                    account.provider_id
                ));
            }
            account.credential.validate()?;
        }
        for (provider_id, account_id) in &self.account_pool.active_accounts {
            if !ids.contains(provider_id.as_str()) {
                return Err(format!(
                    "active account references unknown provider: {provider_id}"
                ));
            }
            if !self.account_pool.accounts.iter().any(|account| {
                account.enabled && account.provider_id == *provider_id && account.id == *account_id
            }) {
                return Err(format!(
                    "active account {account_id} is not enabled for provider {provider_id}"
                ));
            }
        }
        Ok(())
    }
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
