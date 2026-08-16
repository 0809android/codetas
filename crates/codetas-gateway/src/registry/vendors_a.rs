use super::{insert_limits, set_efforts, set_model_modalities, strings, FULL_EFFORTS};
use crate::config::{GoogleMode, ProviderDefinition, ProviderTransport};

pub(super) fn apply_openai(provider: &mut ProviderDefinition) {
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
    insert_limits(
        &mut provider.model_max_input_tokens,
        &[
            ("gpt-5.6-sol", 272_000),
            ("gpt-5.6-terra", 272_000),
            ("gpt-5.6-luna", 272_000),
        ],
    );
    set_efforts(
        provider,
        &["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"],
        FULL_EFFORTS,
    );
}

pub(super) fn apply_openai_api(provider: &mut ProviderDefinition) {
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

pub(super) fn apply_anthropic(provider: &mut ProviderDefinition) {
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

pub(super) fn apply_google(provider: &mut ProviderDefinition) {
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

pub(super) fn apply_google_vertex(provider: &mut ProviderDefinition) {
    provider.google_mode = GoogleMode::Vertex;
    provider.default_model = Some("gemini-3-pro".into());
    provider.discovery.enabled = false;
}

pub(super) fn apply_google_antigravity(provider: &mut ProviderDefinition) {
    provider.google_mode = GoogleMode::CloudCodeAssist;
    provider.default_model = Some("gemini-3.7-flash".into());
    provider.models = strings(&[
        "gemini-3.7-flash",
        "gemini-3.6-flash",
        "gemini-3.5-flash",
        "gemini-3.1-pro",
        "gemini-3.1-flash-image",
        "claude-sonnet-4-6",
        "claude-opus-4-6-thinking",
        "gpt-oss-120b-medium",
    ]);
    provider.discovery.enabled = false;
    set_efforts(
        provider,
        &["gemini-3.7-flash", "gemini-3.6-flash", "gemini-3.5-flash"],
        &["low", "medium", "high"],
    );
    set_efforts(provider, &["gemini-3.1-pro"], &["low", "high"]);
    set_efforts(
        provider,
        &["claude-sonnet-4-6", "claude-opus-4-6-thinking"],
        &["low", "medium", "high", "max"],
    );
    provider
        .model_default_reasoning_efforts
        .insert("gemini-3.7-flash".into(), "medium".into());
    provider
        .model_default_reasoning_efforts
        .insert("gemini-3.6-flash".into(), "medium".into());
    provider
        .model_default_reasoning_efforts
        .insert("gemini-3.5-flash".into(), "medium".into());
    provider
        .model_default_reasoning_efforts
        .insert("gemini-3.1-pro".into(), "high".into());
    insert_limits(
        &mut provider.model_context_windows,
        &[
            ("gemini-3.7-flash", 1_048_576),
            ("gemini-3.6-flash", 1_048_576),
            ("gemini-3.5-flash", 1_048_576),
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

pub(super) fn apply_kiro(provider: &mut ProviderDefinition) {
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

pub(super) fn apply_github_copilot(provider: &mut ProviderDefinition) {
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

pub(super) fn apply_xai(provider: &mut ProviderDefinition) {
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
