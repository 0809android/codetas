use crate::config::{
    CredentialSource, CredentialTransport, GoogleMode, ModelDiscoverySettings,
    ProviderCapabilities, ProviderCredential, ProviderDefinition, ProviderProtocol,
    ProviderTransport,
};
use serde::Serialize;
use std::collections::BTreeMap;

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

fn apply_registry_defaults(provider: &mut ProviderDefinition) {
    const FULL_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
    const DEEPSEEK_EFFORTS: &[&str] = &["high", "xhigh", "max"];
    const KIMI_CODING: &[&str] = &[
        "k3",
        "k3[1m]",
        "kimi-k2.7-code",
        "kimi-k2.7-code-highspeed",
        "kimi-k2.6",
        "kimi-k2.5",
        "kimi-for-coding",
    ];
    const KIMI_LEGACY: &[&str] = &[
        "kimi-k2.7-code",
        "kimi-k2.7-code-highspeed",
        "kimi-k2.6",
        "kimi-k2.5",
    ];
    const DEEPSEEK_THINKING: &[&str] = &["deepseek-v4-pro", "deepseek-v4-flash"];
    const THINKING_TOGGLE: &[&str] = &[
        "mimo-v2.5",
        "mimo-v2.5-pro",
        "mimo-v2-omni",
        "mimo-v2-pro",
        "glm-5",
        "glm-5.1",
    ];
    const THINKING_BUDGET: &[&str] = &[
        "qwen3.5-397b",
        "qwen3.6-35b",
        "qwen3.5-plus",
        "qwen3.6-plus",
        "qwen3.7-max",
        "qwen3.7-plus",
    ];
    const MINIMAX: &[&str] = &[
        "MiniMax-M3",
        "MiniMax-M2.7",
        "MiniMax-M2.7-highspeed",
        "MiniMax-M2.5",
        "MiniMax-M2.5-highspeed",
        "MiniMax-M2.1",
        "MiniMax-M2.1-highspeed",
        "MiniMax-M2",
    ];

    match provider.id.as_str() {
        "openai" => {
            provider.default_model = Some("gpt-5.6-sol".into());
            provider.models = strings(&["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"]);
            insert_limits(
                &mut provider.model_context_windows,
                &[
                    ("gpt-5.6-sol", 372_000),
                    ("gpt-5.6-terra", 372_000),
                    ("gpt-5.6-luna", 372_000),
                ],
            );
            set_efforts(
                provider,
                &["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"],
                FULL_EFFORTS,
            );
        }
        "openai-api" | "openai-apikey" => {
            provider.default_model = Some("gpt-5.5".into());
            provider.models = strings(&[
                "gpt-5.5",
                "gpt-5.6",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
                "gpt-5.6-sol-pro",
                "gpt-5.6-terra-pro",
                "gpt-5.6-luna-pro",
            ]);
            for model in &provider.models {
                provider
                    .model_context_windows
                    .insert(model.clone(), 1_050_000);
                provider
                    .model_max_input_tokens
                    .insert(model.clone(), 922_000);
                provider
                    .model_input_modalities
                    .insert(model.clone(), strings(&["text", "image"]));
            }
            for (virtual_model, wire_model) in [
                ("gpt-5.6-sol-pro", "gpt-5.6-sol"),
                ("gpt-5.6-terra-pro", "gpt-5.6-terra"),
                ("gpt-5.6-luna-pro", "gpt-5.6-luna"),
            ] {
                provider
                    .model_wire_ids
                    .insert(virtual_model.into(), wire_model.into());
                provider
                    .model_reasoning_modes
                    .insert(virtual_model.into(), "pro".into());
            }
            set_efforts(
                provider,
                &[
                    "gpt-5.6",
                    "gpt-5.6-sol",
                    "gpt-5.6-terra",
                    "gpt-5.6-luna",
                    "gpt-5.6-sol-pro",
                    "gpt-5.6-terra-pro",
                    "gpt-5.6-luna-pro",
                ],
                FULL_EFFORTS,
            );
        }
        "anthropic" | "anthropic-apikey" => {
            provider.default_model = Some("claude-sonnet-5".into());
            provider.models = strings(&[
                "claude-fable-5",
                "claude-sonnet-5",
                "claude-opus-5",
                "claude-opus-4-8",
                "claude-opus-4-7",
                "claude-opus-4-6",
                "claude-sonnet-4-6",
                "claude-haiku-4-5",
            ]);
            insert_limits(
                &mut provider.model_context_windows,
                &[
                    ("claude-fable-5", 1_000_000),
                    ("claude-sonnet-5", 1_000_000),
                    ("claude-opus-5", 1_000_000),
                    ("claude-opus-4-8", 1_000_000),
                    ("claude-haiku-4-5", 200_000),
                ],
            );
            set_efforts(
                provider,
                &[
                    "claude-fable-5",
                    "claude-sonnet-5",
                    "claude-opus-5",
                    "claude-opus-4-8",
                    "claude-opus-4-7",
                    "claude-opus-4-6",
                    "claude-sonnet-4-6",
                    "claude-haiku-4-5",
                ],
                FULL_EFFORTS,
            );
        }
        "google" => {
            provider.default_model = Some("gemini-3.5-flash".into());
            provider.models = strings(&[
                "gemini-3.6-flash",
                "gemini-3.5-flash",
                "gemini-3.5-flash-lite",
                "gemini-3.1-pro-preview",
            ]);
            insert_limits(
                &mut provider.model_context_windows,
                &[
                    ("gemini-3.6-flash", 1_048_576),
                    ("gemini-3.5-flash", 1_000_000),
                    ("gemini-3.5-flash-lite", 1_048_576),
                ],
            );
            set_efforts(
                provider,
                &["gemini-3.6-flash", "gemini-3.5-flash"],
                &["minimal", "low", "medium", "high"],
            );
            set_efforts(
                provider,
                &["gemini-3.1-pro-preview"],
                &["low", "medium", "high"],
            );
            for model in ["gemini-3.6-flash", "gemini-3.5-flash-lite"] {
                provider
                    .model_input_modalities
                    .insert(model.into(), strings(&["text", "image"]));
            }
        }
        "google-vertex" => {
            provider.google_mode = GoogleMode::Vertex;
            provider.default_model = Some("gemini-3-pro".into());
            provider.discovery.enabled = false;
        }
        "google-antigravity" => {
            provider.google_mode = GoogleMode::CloudCodeAssist;
            provider.default_model = Some("gemini-3.6-flash".into());
            provider.models = strings(&[
                "gemini-3.6-flash",
                "gemini-3.1-pro",
                "gemini-3.1-flash-image",
                "claude-sonnet-4-6",
                "claude-opus-4-6-thinking",
                "gpt-oss-120b-medium",
            ]);
            provider.discovery.enabled = false;
            set_efforts(provider, &["gemini-3.6-flash"], &["low", "medium", "high"]);
            set_efforts(provider, &["gemini-3.1-pro"], &["low", "high"]);
            set_efforts(
                provider,
                &["claude-sonnet-4-6", "claude-opus-4-6-thinking"],
                &["low", "medium", "high", "max"],
            );
            provider
                .model_default_reasoning_efforts
                .insert("gemini-3.6-flash".into(), "medium".into());
            provider
                .model_default_reasoning_efforts
                .insert("gemini-3.1-pro".into(), "high".into());
            insert_limits(
                &mut provider.model_context_windows,
                &[
                    ("gemini-3.6-flash", 1_048_576),
                    ("gemini-3.1-pro", 1_048_576),
                    ("gemini-3.1-flash-image", 1_048_576),
                    ("claude-sonnet-4-6", 200_000),
                    ("claude-opus-4-6-thinking", 1_000_000),
                    ("gpt-oss-120b-medium", 131_072),
                ],
            );
            for (alias, wire) in [
                ("gemini-3.1-pro-high", "gemini-pro-agent"),
                ("gemini-3.1-pro-preview", "gemini-pro-agent"),
                ("gemini-3.5-flash-extra-low", "gemini-3.6-flash-low"),
                ("gemini-3.5-flash-low", "gemini-3.6-flash-medium"),
                ("gemini-3.5-flash-mid", "gemini-3.6-flash-medium"),
                ("gemini-3.5-flash-high", "gemini-3.6-flash-high"),
                ("gemini-3-flash-agent", "gemini-3.6-flash-high"),
            ] {
                provider.model_wire_ids.insert(alias.into(), wire.into());
            }
            set_model_modalities(provider, "gemini-3.1-flash-image", &["text", "image"]);
        }
        "kiro" => {
            provider.transport = ProviderTransport::Kiro;
            provider.default_model = Some("kiro-auto".into());
            provider.models = strings(&[
                "kiro-auto",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
                "claude-sonnet-5",
                "claude-opus-5",
                "claude-opus-4.8",
                "claude-opus-4.7",
                "claude-opus-4.6",
                "claude-opus-4.5",
                "claude-sonnet-4.6",
                "claude-sonnet-4.5",
                "claude-sonnet-4.0",
                "claude-haiku-4.5",
                "deepseek-3.2",
                "minimax-m2.5",
                "minimax-m2.1",
                "glm-5",
                "qwen3-coder-next",
            ]);
            provider.discovery.enabled = false;
            provider.capabilities.vision = true;
            provider.capabilities.reasoning = true;
            provider.capabilities.parallel_tools = false;
            insert_limits(
                &mut provider.model_context_windows,
                &[
                    ("gpt-5.6-sol", 272_000),
                    ("gpt-5.6-terra", 272_000),
                    ("gpt-5.6-luna", 272_000),
                    ("claude-sonnet-5", 1_000_000),
                    ("claude-opus-5", 1_000_000),
                    ("claude-opus-4.8", 1_000_000),
                    ("claude-opus-4.7", 1_000_000),
                    ("claude-opus-4.6", 1_000_000),
                    ("claude-opus-4.5", 200_000),
                    ("claude-sonnet-4.6", 1_000_000),
                    ("claude-sonnet-4.5", 200_000),
                    ("claude-sonnet-4.0", 200_000),
                    ("claude-haiku-4.5", 200_000),
                    ("deepseek-3.2", 128_000),
                    ("minimax-m2.5", 200_000),
                    ("minimax-m2.1", 200_000),
                    ("glm-5", 200_000),
                    ("qwen3-coder-next", 256_000),
                ],
            );
            set_efforts(
                provider,
                &[
                    "kiro-auto",
                    "gpt-5.6-sol",
                    "gpt-5.6-terra",
                    "gpt-5.6-luna",
                    "claude-sonnet-5",
                    "claude-opus-5",
                    "claude-opus-4.8",
                    "claude-opus-4.7",
                    "claude-opus-4.6",
                    "claude-opus-4.5",
                    "claude-sonnet-4.6",
                    "claude-sonnet-4.5",
                    "claude-sonnet-4.0",
                    "claude-haiku-4.5",
                    "deepseek-3.2",
                    "minimax-m2.5",
                    "minimax-m2.1",
                    "glm-5",
                    "qwen3-coder-next",
                ],
                FULL_EFFORTS,
            );
            for model in provider.models.clone() {
                provider
                    .model_input_modalities
                    .insert(model, strings(&["text", "image"]));
            }
        }
        "github-copilot" => {
            provider.transport = ProviderTransport::GithubCopilot;
            provider.default_model = Some("gpt-4o".into());
            provider.models = strings(&[
                "gpt-4o",
                "gpt-4.1",
                "gpt-4.1-mini",
                "claude-sonnet-4",
                "gemini-2.5-pro",
            ]);
            provider.discovery.enabled = false;
            provider.capabilities.vision = true;
            provider.capabilities.reasoning = true;
            provider.capabilities.parallel_tools = true;
            insert_limits(
                &mut provider.model_context_windows,
                &[
                    ("gpt-4o", 128_000),
                    ("gpt-4.1", 1_000_000),
                    ("gpt-4.1-mini", 1_000_000),
                    ("claude-sonnet-4", 200_000),
                    ("gemini-2.5-pro", 1_048_576),
                ],
            );
            for model in provider.models.clone() {
                provider
                    .model_input_modalities
                    .insert(model, strings(&["text", "image"]));
            }
        }
        "xai" => {
            provider.capabilities.vision = true;
            provider.default_model = Some("grok-4.5".into());
            provider.models = strings(&[
                "grok-4.5",
                "grok-4.3",
                "grok-4.20-0309-reasoning",
                "grok-4.20-0309-non-reasoning",
                "grok-build-0.1",
                "grok-composer-2.5-fast",
            ]);
            provider.no_reasoning_models = strings(&[
                "grok-4.20-0309-non-reasoning",
                "grok-build-0.1",
                "grok-composer-2.5-fast",
            ]);
            provider.preserve_reasoning_content_models =
                strings(&["grok-4.5", "grok-4.3", "grok-4.20-0309-reasoning"]);
            set_efforts(provider, &["grok-4.5"], &["low", "medium", "high"]);
            insert_limits(
                &mut provider.model_context_windows,
                &[
                    ("grok-4.5", 500_000),
                    ("grok-4.3", 1_000_000),
                    ("grok-4.20-0309-reasoning", 1_000_000),
                    ("grok-4.20-0309-non-reasoning", 1_000_000),
                    ("grok-build-0.1", 256_000),
                ],
            );
            for model in [
                "grok-4.5",
                "grok-4.3",
                "grok-4.20-0309-reasoning",
                "grok-4.20-0309-non-reasoning",
            ] {
                provider
                    .model_input_modalities
                    .insert(model.into(), strings(&["text", "image"]));
            }
        }
        "kimi" | "kimi-code" => {
            provider.strip_model_bracket_suffix = true;
            provider.default_model = Some("kimi-k2.7-code".into());
            provider.models = strings(KIMI_CODING);
            provider.no_reasoning_models = strings(KIMI_LEGACY);
            provider.no_temperature_models = strings(KIMI_CODING);
            provider.no_top_p_models = strings(KIMI_CODING);
            provider.no_penalty_models = strings(KIMI_CODING);
            provider.auto_tool_choice_only_models = strings(&[
                "kimi-k2.7-code",
                "kimi-k2.7-code-highspeed",
                "kimi-for-coding",
            ]);
            provider.preserve_reasoning_content_models = strings(KIMI_CODING);
            provider.prompt_cache_key = true;
            set_efforts(provider, &["k3", "k3[1m]"], &["low", "high", "max"]);
            set_wire_map(
                provider,
                &["k3", "k3[1m]"],
                &[
                    ("none", "none"),
                    ("minimal", "low"),
                    ("low", "low"),
                    ("medium", "high"),
                    ("high", "high"),
                    ("xhigh", "max"),
                    ("max", "max"),
                    ("ultra", "max"),
                ],
            );
            for model in ["k3", "k3[1m]"] {
                provider
                    .model_default_reasoning_efforts
                    .insert(model.into(), "max".into());
                provider
                    .model_input_modalities
                    .insert(model.into(), strings(&["text", "image"]));
            }
            for model in KIMI_CODING {
                provider.model_context_windows.insert(
                    (*model).into(),
                    if *model == "k3[1m]" {
                        1_048_576
                    } else {
                        262_144
                    },
                );
            }
        }
        "moonshot" => {
            provider.default_model = Some("kimi-k2.7-code".into());
            provider.models = strings(&[
                "kimi-k3",
                "kimi-k2.7-code",
                "kimi-k2.7-code-highspeed",
                "kimi-k2.6",
                "kimi-k2.5",
            ]);
            provider.no_reasoning_models = strings(KIMI_LEGACY);
            provider.no_temperature_models = provider.models.clone();
            provider.no_top_p_models = provider.models.clone();
            provider.no_penalty_models = provider.models.clone();
            provider.auto_tool_choice_only_models =
                strings(&["kimi-k2.7-code", "kimi-k2.7-code-highspeed"]);
            provider.preserve_reasoning_content_models = provider.models.clone();
            set_efforts(provider, &["kimi-k3"], &["max"]);
            for model in &provider.models {
                provider.model_context_windows.insert(
                    model.clone(),
                    if model == "kimi-k3" {
                        1_048_576
                    } else {
                        262_144
                    },
                );
            }
            provider
                .model_input_modalities
                .insert("kimi-k3".into(), strings(&["text", "image"]));
        }
        "deepseek" => {
            provider.default_model = Some("deepseek-v4-flash".into());
            provider.models = strings(&[
                "deepseek-chat",
                "deepseek-reasoner",
                "deepseek-v4-pro",
                "deepseek-v4-flash",
            ]);
            provider.responses_path = Some("/responses".into());
            provider.stateless_responses = true;
            provider
                .model_protocols
                .insert("deepseek-v4-flash".into(), ProviderProtocol::Responses);
            provider.preserve_reasoning_content_models = strings(DEEPSEEK_THINKING);
            set_efforts(provider, DEEPSEEK_THINKING, DEEPSEEK_EFFORTS);
            set_wire_map(
                provider,
                DEEPSEEK_THINKING,
                &[
                    ("none", "high"),
                    ("minimal", "high"),
                    ("low", "high"),
                    ("medium", "high"),
                    ("high", "high"),
                    ("xhigh", "max"),
                    ("max", "max"),
                    ("ultra", "max"),
                ],
            );
            insert_limits(
                &mut provider.model_context_windows,
                &[
                    ("deepseek-v4-pro", 1_000_000),
                    ("deepseek-v4-flash", 1_000_000),
                ],
            );
        }
        "opencode-go" => {
            provider.default_model = Some("kimi-k2.7-code".into());
            provider.thinking_toggle_models = strings(THINKING_TOGGLE);
            provider.thinking_budget_models = strings(THINKING_BUDGET);
            provider.no_reasoning_models = strings(&["kimi-k2.7-code", "kimi-k2.7-code-highspeed"]);
            provider.no_temperature_models =
                strings(&["kimi-k3", "kimi-k2.7-code", "kimi-k2.7-code-highspeed"]);
            provider.no_top_p_models = provider.no_temperature_models.clone();
            provider.no_penalty_models = provider.no_temperature_models.clone();
            provider.auto_tool_choice_only_models =
                strings(&["kimi-k2.7-code", "kimi-k2.7-code-highspeed"]);
            provider.preserve_reasoning_content_models = strings(&[
                "glm-5.2",
                "kimi-k3",
                "kimi-k2.7-code",
                "kimi-k2.7-code-highspeed",
                "deepseek-v4-pro",
                "deepseek-v4-flash",
            ]);
            set_efforts(provider, THINKING_TOGGLE, FULL_EFFORTS);
            set_efforts(provider, THINKING_BUDGET, FULL_EFFORTS);
            set_efforts(provider, DEEPSEEK_THINKING, DEEPSEEK_EFFORTS);
            set_wire_map(
                provider,
                THINKING_TOGGLE,
                &[
                    ("none", "disabled"),
                    ("minimal", "disabled"),
                    ("low", "disabled"),
                    ("medium", "enabled"),
                    ("high", "enabled"),
                    ("xhigh", "enabled"),
                    ("max", "enabled"),
                    ("ultra", "enabled"),
                ],
            );
            set_wire_map(
                provider,
                &["kimi-k3"],
                &[
                    ("none", "none"),
                    ("minimal", "low"),
                    ("low", "low"),
                    ("medium", "high"),
                    ("high", "high"),
                    ("xhigh", "max"),
                    ("max", "max"),
                    ("ultra", "max"),
                ],
            );
            set_wire_map(
                provider,
                DEEPSEEK_THINKING,
                &[
                    ("none", "high"),
                    ("minimal", "high"),
                    ("low", "high"),
                    ("medium", "high"),
                    ("high", "high"),
                    ("xhigh", "max"),
                    ("max", "max"),
                    ("ultra", "max"),
                ],
            );
        }
        "nvidia" => {
            let kimi = [
                "moonshotai/kimi-k2.6",
                "moonshotai/kimi-k2.5",
                "moonshotai/kimi-k2-thinking",
                "moonshotai/kimi-k2-instruct",
                "moonshotai/kimi-k2-instruct-0905",
            ];
            provider.parallel_tool_calls = Some(false);
            provider.no_reasoning_models = strings(&kimi);
            provider.preserve_reasoning_content_models = strings(&kimi[..3]);
            for model in kimi {
                provider
                    .model_reasoning_efforts
                    .insert(model.into(), Vec::new());
            }
        }
        "neuralwatt" => {
            provider.default_model = Some("glm-5.2".into());
            provider.models = strings(&[
                "glm-5.2",
                "glm-5.2-fast",
                "glm-5.2-short",
                "glm-5.2-short-fast",
                "kimi-k2.6",
                "kimi-k2.6-fast",
                "kimi-k2.7-code",
                "qwen3.5-397b",
                "qwen3.5-397b-fast",
                "qwen3.6-35b",
                "qwen3.6-35b-fast",
            ]);
            provider.thinking_budget_models = strings(&["qwen3.5-397b", "qwen3.6-35b"]);
            provider.no_reasoning_models = strings(&[
                "glm-5.2-fast",
                "glm-5.2-short-fast",
                "kimi-k2.6-fast",
                "qwen3.5-397b-fast",
                "qwen3.6-35b-fast",
            ]);
            provider.no_temperature_models = strings(&["kimi-k2.7-code"]);
            provider.no_top_p_models = provider.no_temperature_models.clone();
            provider.no_penalty_models = provider.no_temperature_models.clone();
            provider.auto_tool_choice_only_models = strings(&["kimi-k2.7-code"]);
            provider.preserve_reasoning_content_models = strings(&[
                "glm-5.2",
                "glm-5.2-short",
                "kimi-k2.6",
                "kimi-k2.7-code",
                "qwen3.5-397b",
                "qwen3.6-35b",
            ]);
            set_efforts(provider, &["glm-5.2", "glm-5.2-short"], FULL_EFFORTS);
            set_efforts(provider, &["qwen3.5-397b", "qwen3.6-35b"], FULL_EFFORTS);
        }
        "zai" => {
            provider.strip_model_bracket_suffix = true;
            provider.default_model = Some("glm-5.2".into());
            provider.models = strings(&["glm-5.2", "glm-5.2[1m]", "glm-5.1", "glm-5", "glm-4.6"]);
            provider.preserve_reasoning_content_models = strings(&["glm-5.2", "glm-5.2[1m]"]);
            set_efforts(provider, &["glm-5.2", "glm-5.2[1m]"], FULL_EFFORTS);
            insert_limits(
                &mut provider.model_context_windows,
                &[("glm-5.2", 1_000_000), ("glm-5.2[1m]", 1_000_000)],
            );
        }
        "zhipu-bigmodel" => {
            let toggles = ["glm-4.6", "glm-4.7", "glm-5", "glm-5.1"];
            provider.default_model = Some("glm-4.6".into());
            provider.models = strings(&[
                "glm-4.6",
                "glm-4.7",
                "glm-4.7-flash",
                "glm-5",
                "glm-5.1",
                "glm-4.6v",
            ]);
            provider.thinking_toggle_models = strings(&toggles);
            provider.preserve_reasoning_content_models = strings(&toggles);
            set_efforts(provider, &toggles, FULL_EFFORTS);
            set_wire_map(
                provider,
                &toggles,
                &[
                    ("none", "disabled"),
                    ("minimal", "disabled"),
                    ("low", "disabled"),
                    ("medium", "enabled"),
                    ("high", "enabled"),
                    ("xhigh", "enabled"),
                    ("max", "enabled"),
                    ("ultra", "enabled"),
                ],
            );
            provider
                .model_context_windows
                .insert("glm-4.6".into(), 204_800);
            provider
                .model_input_modalities
                .insert("glm-4.6v".into(), strings(&["text", "image"]));
        }
        "minimax" | "minimax-cn" => {
            provider.default_model = Some("MiniMax-M3".into());
            provider.models = strings(MINIMAX);
            provider.preserve_reasoning_content_models = strings(MINIMAX);
            provider.reasoning_split_models = strings(MINIMAX);
            provider.thinking_toggle_models = strings(&["MiniMax-M3"]);
            set_efforts(provider, &["MiniMax-M3"], FULL_EFFORTS);
            set_wire_map(
                provider,
                &["MiniMax-M3"],
                &[
                    ("none", "disabled"),
                    ("minimal", "disabled"),
                    ("low", "disabled"),
                    ("medium", "adaptive"),
                    ("high", "adaptive"),
                    ("xhigh", "adaptive"),
                    ("max", "adaptive"),
                    ("ultra", "adaptive"),
                ],
            );
            provider
                .model_default_reasoning_efforts
                .insert("MiniMax-M3".into(), "medium".into());
            for model in MINIMAX {
                provider.model_context_windows.insert(
                    (*model).into(),
                    if *model == "MiniMax-M3" {
                        1_000_000
                    } else {
                        204_800
                    },
                );
            }
        }
        "alibaba-token-plan" => {
            let qwen = [
                "qwen3.8-max-preview",
                "qwen3.7-max",
                "qwen3.7-plus",
                "qwen3.6-flash",
            ];
            provider.default_model = Some("qwen3.8-max-preview".into());
            provider.models = strings(&[
                "qwen3.8-max-preview",
                "qwen3.7-max",
                "qwen3.7-plus",
                "qwen3.6-flash",
                "glm-5.2",
                "deepseek-v4-pro",
            ]);
            provider.discovery.enabled = false;
            provider.thinking_budget_models = strings(&qwen);
            provider.preserve_reasoning_content_models = provider.models.clone();
            set_efforts(provider, &qwen, FULL_EFFORTS);
            set_efforts(provider, &["glm-5.2"], FULL_EFFORTS);
            set_efforts(provider, &["deepseek-v4-pro"], DEEPSEEK_EFFORTS);
            set_wire_map(
                provider,
                &["deepseek-v4-pro"],
                &[
                    ("none", "high"),
                    ("minimal", "high"),
                    ("low", "high"),
                    ("medium", "high"),
                    ("high", "high"),
                    ("xhigh", "max"),
                    ("max", "max"),
                    ("ultra", "max"),
                ],
            );
            insert_limits(
                &mut provider.model_context_windows,
                &[
                    ("qwen3.8-max-preview", 983_616),
                    ("qwen3.7-max", 1_000_000),
                    ("qwen3.7-plus", 1_000_000),
                    ("qwen3.6-flash", 1_000_000),
                    ("glm-5.2", 1_000_000),
                    ("deepseek-v4-pro", 1_000_000),
                    ("deepseek-v4-flash", 1_000_000),
                ],
            );
            for model in qwen {
                set_model_modalities(provider, model, &["text", "image"]);
            }
            for model in ["glm-5.2", "deepseek-v4-pro"] {
                set_model_modalities(provider, model, &["text"]);
            }
        }
        "alibaba-token-plan-intl" => {
            let qwen = [
                "qwen3.8-max-preview",
                "qwen3.7-max",
                "qwen3.7-plus",
                "qwen3.6-plus",
                "qwen3.6-flash",
            ];
            provider.default_model = Some("qwen3.7-max".into());
            provider.models = strings(&[
                "qwen3.8-max-preview",
                "qwen3.7-max",
                "qwen3.7-plus",
                "qwen3.6-plus",
                "qwen3.6-flash",
                "deepseek-v4-pro",
                "deepseek-v4-flash",
                "deepseek-v3.2",
                "kimi-k2.7-code",
                "kimi-k2.6",
                "kimi-k2.5",
                "glm-5.2",
                "glm-5.1",
                "glm-5",
                "MiniMax-M2.5",
            ]);
            provider.discovery.enabled = false;
            provider.thinking_budget_models = strings(&qwen);
            provider.preserve_reasoning_content_models = strings(&[
                "glm-5.2",
                "deepseek-v4-pro",
                "deepseek-v4-flash",
                "qwen3.8-max-preview",
                "qwen3.7-max",
                "qwen3.7-plus",
                "qwen3.6-plus",
                "qwen3.6-flash",
            ]);
            provider.no_reasoning_models = strings(&[
                "kimi-k2.7-code",
                "kimi-k2.6",
                "kimi-k2.5",
                "deepseek-v3.2",
                "glm-5.1",
                "glm-5",
                "MiniMax-M2.5",
            ]);
            set_efforts(provider, &qwen, FULL_EFFORTS);
            set_efforts(
                provider,
                &["deepseek-v4-pro", "deepseek-v4-flash"],
                DEEPSEEK_EFFORTS,
            );
            set_efforts(provider, &["glm-5.2"], FULL_EFFORTS);
            set_wire_map(
                provider,
                &["deepseek-v4-pro", "deepseek-v4-flash"],
                &[
                    ("none", "high"),
                    ("minimal", "high"),
                    ("low", "high"),
                    ("medium", "high"),
                    ("high", "high"),
                    ("xhigh", "max"),
                    ("max", "max"),
                    ("ultra", "max"),
                ],
            );
            provider
                .model_default_reasoning_efforts
                .insert("qwen3.8-max-preview".into(), "xhigh".into());
            for model in qwen {
                set_model_modalities(provider, model, &["text", "image"]);
            }
            for model in ["kimi-k2.7-code", "kimi-k2.6", "kimi-k2.5"] {
                set_model_modalities(provider, model, &["text", "image"]);
            }
            for model in [
                "deepseek-v4-pro",
                "deepseek-v4-flash",
                "deepseek-v3.2",
                "glm-5.2",
                "glm-5.1",
                "glm-5",
                "MiniMax-M2.5",
            ] {
                set_model_modalities(provider, model, &["text"]);
            }
        }
        "tencent-coding-plan" => {
            provider.models = strings(&["tc-code-latest", "glm-5", "kimi-k2.5", "minimax-m2.5"]);
            provider.default_model = Some("tc-code-latest".into());
            for model in provider.models.clone() {
                set_model_modalities(provider, &model, &["text"]);
            }
        }
        "volcengine" => {
            let toggles = [
                "doubao-seed-2-1-pro-260628",
                "doubao-seed-2-1-turbo-260628",
                "doubao-seed-evolving",
            ];
            provider.default_model = Some("doubao-seed-2-1-pro-260628".into());
            provider.models = strings(&[
                "doubao-seed-2-1-pro-260628",
                "doubao-seed-2-1-turbo-260628",
                "doubao-seed-evolving",
                "deepseek-v4-pro-260425",
                "deepseek-v4-flash-260425",
                "deepseek-v3-2-251201",
                "glm-5-2-260617",
                "glm-4-7-251222",
            ]);
            provider.discovery.enabled = false;
            provider.thinking_toggle_models = strings(&toggles);
            provider.preserve_reasoning_content_models = strings(&[
                "deepseek-v4-pro-260425",
                "deepseek-v4-flash-260425",
                "glm-5-2-260617",
                "glm-4-7-251222",
            ]);
            set_efforts(provider, &toggles, FULL_EFFORTS);
            set_wire_map(
                provider,
                &toggles,
                &[
                    ("none", "disabled"),
                    ("minimal", "disabled"),
                    ("low", "disabled"),
                    ("medium", "enabled"),
                    ("high", "enabled"),
                    ("xhigh", "enabled"),
                    ("max", "enabled"),
                    ("ultra", "enabled"),
                ],
            );
        }
        "volcengine-coding-plan" | "volcengine-agent-plan" => {
            if provider.id == "volcengine-coding-plan" {
                provider.models = strings(&[
                    "ark-code-latest",
                    "doubao-seed-2.0-code",
                    "deepseek-v4-pro",
                    "deepseek-v4-flash",
                    "glm-5.2",
                    "kimi-k2.6",
                    "minimax-m3",
                ]);
                provider.default_model = Some("ark-code-latest".into());
            } else {
                provider.models = strings(&[
                    "deepseek-v4-pro",
                    "deepseek-v4-flash",
                    "glm-5.2",
                    "kimi-k2.6",
                    "minimax-m3",
                    "doubao-seed-2.0-pro",
                ]);
                provider.default_model = Some("deepseek-v4-pro".into());
                provider.responses_path = Some("/responses".into());
            }
            provider.discovery.enabled = false;
            provider.preserve_reasoning_content_models = strings(DEEPSEEK_THINKING);
            set_efforts(provider, DEEPSEEK_THINKING, DEEPSEEK_EFFORTS);
            set_wire_map(
                provider,
                DEEPSEEK_THINKING,
                &[
                    ("none", "high"),
                    ("minimal", "high"),
                    ("low", "high"),
                    ("medium", "high"),
                    ("high", "high"),
                    ("xhigh", "max"),
                    ("max", "max"),
                    ("ultra", "max"),
                ],
            );
            for model in ["kimi-k2.6", "minimax-m3"] {
                set_model_modalities(provider, model, &["text", "image"]);
            }
        }
        "orcarouter" => {
            provider.default_model = Some("openai/gpt-5.5".into());
            provider.models = strings(&[
                "openai/gpt-5.5",
                "anthropic/claude-opus-4.8",
                "google/gemini-3.5-flash",
                "deepseek/deepseek-v4-pro",
                "orcarouter/auto",
            ]);
            provider.preserve_reasoning_content_models = strings(&["deepseek/deepseek-v4-pro"]);
            set_efforts(
                provider,
                &["openai/gpt-5.5"],
                &["low", "medium", "high", "xhigh"],
            );
            set_efforts(provider, &["deepseek/deepseek-v4-pro"], DEEPSEEK_EFFORTS);
            set_wire_map(
                provider,
                &["deepseek/deepseek-v4-pro"],
                &[
                    ("none", "high"),
                    ("minimal", "high"),
                    ("low", "high"),
                    ("medium", "high"),
                    ("high", "high"),
                    ("xhigh", "max"),
                    ("max", "max"),
                    ("ultra", "max"),
                ],
            );
        }
        "bizrouter" => {
            provider.default_model = Some("openai/gpt-5.6-sol".into());
            provider.models = strings(&[
                "openai/gpt-5.6-sol",
                "anthropic/claude-sonnet-5",
                "google/gemini-3.5-flash",
            ]);
        }
        "ollama-cloud" => {
            provider.default_model = Some("glm-5.2".into());
            provider.models = strings(&[
                "glm-5.2",
                "deepseek-v4-pro",
                "qwen3-coder:480b",
                "gpt-oss:120b",
                "kimi-k2.6",
                "minimax-m3",
                "qwen3.5:397b",
                "gemma4:31b",
            ]);
        }
        "umans" => {
            provider.default_model = Some("umans-coder".into());
            provider.models = strings(&[
                "umans-coder",
                "umans-kimi-k2.7",
                "umans-flash",
                "umans-glm-5.2",
                "umans-glm-5.1",
                "umans-qwen3.6-35b-a3b",
            ]);
            set_efforts(
                provider,
                &[
                    "umans-coder",
                    "umans-kimi-k2.7",
                    "umans-flash",
                    "umans-qwen3.6-35b-a3b",
                ],
                FULL_EFFORTS,
            );
            set_efforts(
                provider,
                &["umans-glm-5.2", "umans-glm-5.1"],
                DEEPSEEK_EFFORTS,
            );
            insert_limits(
                &mut provider.model_context_windows,
                &[
                    ("umans-coder", 262_144),
                    ("umans-kimi-k2.7", 262_144),
                    ("umans-flash", 262_144),
                    ("umans-glm-5.2", 405_504),
                    ("umans-glm-5.1", 202_752),
                    ("umans-qwen3.6-35b-a3b", 262_144),
                ],
            );
            for model in ["umans-glm-5.2", "umans-glm-5.1"] {
                set_model_modalities(provider, model, &["text"]);
            }
            for model in [
                "umans-coder",
                "umans-kimi-k2.7",
                "umans-flash",
                "umans-qwen3.6-35b-a3b",
            ] {
                set_model_modalities(provider, model, &["text", "image"]);
            }
        }
        "opencode-free" => {
            provider
                .headers
                .insert("x-opencode-client".into(), "desktop".into());
            provider.preserve_reasoning_content_models = strings(&["deepseek-v4-flash-free"]);
            set_efforts(provider, &["deepseek-v4-flash-free"], DEEPSEEK_EFFORTS);
            set_wire_map(
                provider,
                &["deepseek-v4-flash-free"],
                &[
                    ("none", "high"),
                    ("minimal", "high"),
                    ("low", "high"),
                    ("medium", "high"),
                    ("high", "high"),
                    ("xhigh", "max"),
                    ("max", "max"),
                    ("ultra", "max"),
                ],
            );
        }
        _ => {}
    }
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
        advanced_preset("openai-apikey", "OpenAI API (OpenCodex ID)", "Native Responses API", "https://api.openai.com/v1", ProviderProtocol::Responses, Some("OPENAI_API_KEY")),
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
        local_preset("lm-studio", "LM Studio (OpenCodex ID)", "Local LM Studio OpenAI-compatible API", "http://127.0.0.1:1234/v1"),
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
        capabilities: ProviderCapabilities::default(),
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
            ..ProviderCapabilities::default()
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
            ..ProviderCapabilities::default()
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
            ..ProviderCapabilities::default()
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
        ..preset(id, name, description, "", protocol, api_key_env)
    }
}
