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
            let (migrated, changed) =
                crate::config::parse_gateway_settings_json(&serde_json::to_vec(&settings).unwrap())
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
            let (again, changed) =
                crate::config::parse_gateway_settings_json(&serde_json::to_vec(&migrated).unwrap())
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
            let (migrated, changed) =
                crate::config::parse_gateway_settings_json(&serde_json::to_vec(&settings).unwrap())
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
    let sidecars = |vision: Option<&str>, document: Option<&str>| crate::config::SidecarSettings {
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
        crate::config::parse_gateway_settings_json(&serde_json::to_vec(&invalid).unwrap()).is_err()
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
fn backfills_opendesign_empty_completion_retry_once() {
    let mut provider = provider_presets()
        .into_iter()
        .find(|preset| preset.id == "opendesign")
        .unwrap()
        .instantiate(None)
        .unwrap();
    assert_eq!(
        provider.empty_completion_retry_models,
        vec!["*".to_string()]
    );
    provider.empty_completion_retry_models.clear();
    let mut settings = GatewaySettings {
        registry_revision: OPENDESIGN_EMPTY_RETRY_REVISION - 1,
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
