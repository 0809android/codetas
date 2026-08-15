use super::*;

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
    #[serde(default, alias = "image_input_mode")]
    pub image_input_mode: AuxiliaryInputMode,
    #[serde(default, alias = "video_input_mode")]
    pub video_input_mode: AuxiliaryInputMode,
    #[serde(default, alias = "document_input_mode")]
    pub document_input_mode: AuxiliaryInputMode,
    #[serde(default = "default_auxiliary_timeout_ms", alias = "auxiliary_timeout_ms")]
    pub auxiliary_timeout_ms: u64,
    #[serde(default = "default_video_sample_frames", alias = "video_sample_frames")]
    pub video_sample_frames: u16,
    #[serde(default = "default_document_max_pages", alias = "document_max_pages")]
    pub document_max_pages: u16,
    #[serde(default = "enabled_by_default", alias = "ocr_enabled")]
    pub ocr_enabled: bool,
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
            image_input_mode: AuxiliaryInputMode::default(),
            video_input_mode: AuxiliaryInputMode::default(),
            document_input_mode: AuxiliaryInputMode::default(),
            auxiliary_timeout_ms: default_auxiliary_timeout_ms(),
            video_sample_frames: default_video_sample_frames(),
            document_max_pages: default_document_max_pages(),
            ocr_enabled: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AuxiliaryInputMode {
    #[default]
    Auto,
    Native,
    Text,
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
    #[serde(default, alias = "video_input_model")]
    pub video_input_model: Option<String>,
    #[serde(default, alias = "document_model")]
    pub document_model: Option<String>,
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
    pub fn validate(&self) -> Result<(), String> {
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
