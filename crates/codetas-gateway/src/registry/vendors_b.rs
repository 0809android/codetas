use super::{
    insert_limits, set_efforts, set_model_modalities, set_wire_map, strings, DEEPSEEK_EFFORTS,
    FULL_EFFORTS,
};
use crate::config::{ProviderDefinition, ProviderProtocol};
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

pub(super) fn apply_kimi(provider: &mut ProviderDefinition) {
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
    provider.terminal_continuation_guard_models = strings(&["k3", "k3[1m]"]);
    provider.empty_completion_retry_models = strings(&["k3", "k3[1m]"]);
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

pub(super) fn apply_moonshot(provider: &mut ProviderDefinition) {
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
    provider.terminal_continuation_guard_models = strings(&["kimi-k3"]);
    provider.empty_completion_retry_models = strings(&["kimi-k3"]);
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

pub(super) fn apply_deepseek(provider: &mut ProviderDefinition) {
    provider.default_model = Some("deepseek-v4-flash".into());
    provider.models = strings(&[
        "deepseek-chat",
        "deepseek-reasoner",
        "deepseek-v4-pro",
        "deepseek-v4-flash",
    ]);
    provider.responses_path = Some("/responses".into());
    provider.stateless_responses = true;
    provider.requires_adjacent_responses_tool_results = true;
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

pub(super) fn apply_opencode_go(provider: &mut ProviderDefinition) {
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

pub(super) fn apply_nvidia(provider: &mut ProviderDefinition) {
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

pub(super) fn apply_neuralwatt(provider: &mut ProviderDefinition) {
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

pub(super) fn apply_zai(provider: &mut ProviderDefinition) {
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

pub(super) fn apply_zhipu_bigmodel(provider: &mut ProviderDefinition) {
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
    provider.requires_reasoning_placeholder_models = Some(Vec::new());
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

pub(super) fn apply_minimax(provider: &mut ProviderDefinition) {
    provider.default_model = Some("MiniMax-M3".into());
    provider.models = strings(MINIMAX);
    provider.preserve_reasoning_content_models = strings(MINIMAX);
    provider.requires_reasoning_placeholder_models = Some(Vec::new());
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

pub(super) fn apply_alibaba_token_plan(provider: &mut ProviderDefinition) {
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

pub(super) fn apply_alibaba_token_plan_intl(provider: &mut ProviderDefinition) {
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

pub(super) fn apply_tencent_coding_plan(provider: &mut ProviderDefinition) {
    provider.models = strings(&["tc-code-latest", "glm-5", "kimi-k2.5", "minimax-m2.5"]);
    provider.default_model = Some("tc-code-latest".into());
    for model in provider.models.clone() {
        set_model_modalities(provider, &model, &["text"]);
    }
}

pub(super) fn apply_volcengine(provider: &mut ProviderDefinition) {
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

pub(super) fn apply_volcengine_coding_plan(provider: &mut ProviderDefinition) {
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

pub(super) fn apply_orcarouter(provider: &mut ProviderDefinition) {
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

pub(super) fn apply_bizrouter(provider: &mut ProviderDefinition) {
    provider.default_model = Some("openai/gpt-5.6-sol".into());
    provider.models = strings(&[
        "openai/gpt-5.6-sol",
        "anthropic/claude-sonnet-5",
        "google/gemini-3.5-flash",
    ]);
}

pub(super) fn apply_ollama_cloud(provider: &mut ProviderDefinition) {
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

pub(super) fn apply_umans(provider: &mut ProviderDefinition) {
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

pub(super) fn apply_opencode_free(provider: &mut ProviderDefinition) {
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
