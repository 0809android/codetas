use super::*;
use crate::config::{ProviderDefinition, ProviderProtocol};

fn openai_settings() -> GatewaySettings {
    GatewaySettings {
        providers: vec![ProviderDefinition {
            id: "openai".into(),
            name: "OpenAI".into(),
            models: vec!["gpt-test".into(), "gpt-second".into()],
            default_model: Some("gpt-test".into()),
            ..ProviderDefinition::default()
        }],
        ..GatewaySettings::default()
    }
}

#[test]
fn api_and_subscription_picker_entries_stay_distinct_in_every_display_format() {
    use crate::config::ProviderCredential;
    let mut settings = GatewaySettings {
        providers: [
            ("openai", "gpt-test", CredentialSource::Forward),
            ("openai-api", "gpt-test", CredentialSource::Keychain),
            ("openai-apikey", "gpt-test", CredentialSource::Environment),
            ("anthropic", "claude-test", CredentialSource::OAuth),
            (
                "anthropic-apikey",
                "claude-test",
                CredentialSource::Keychain,
            ),
        ]
        .into_iter()
        .map(|(id, model, source)| ProviderDefinition {
            id: id.into(),
            name: "Same connection name".into(),
            models: vec![model.into()],
            credential: ProviderCredential {
                source,
                ..ProviderCredential::default()
            },
            ..ProviderDefinition::default()
        })
        .collect(),
        ..GatewaySettings::default()
    };
    settings.model_catalog = settings
        .providers
        .iter()
        .map(|provider| ModelMetadata {
            provider_id: provider.id.clone(),
            model_id: provider.models[0].clone(),
            display_name: Some("Same custom model name".into()),
            enabled: true,
            ..ModelMetadata::default()
        })
        .collect();
    for format in [
        CatalogDisplayNameFormat::Default,
        CatalogDisplayNameFormat::ModelId,
        CatalogDisplayNameFormat::ProviderModel,
        CatalogDisplayNameFormat::ProviderIdModel,
        CatalogDisplayNameFormat::Custom,
    ] {
        settings.catalog.display_name_format = format;
        let catalog = build_codex_catalog(&settings);
        catalog.validate_for_codex(Some("gpt-test")).unwrap();
        assert_eq!(catalog.models.len(), 5);
        for (slug, label) in [
            ("gpt-test", None),
            ("openai-api/gpt-test", Some("API")),
            ("openai-apikey/gpt-test", Some("API")),
            ("anthropic/claude-test", None),
            ("anthropic-apikey/claude-test", Some("API")),
        ] {
            let model = catalog
                .models
                .iter()
                .find(|entry| entry["slug"] == slug)
                .unwrap();
            let name = model["display_name"].as_str().unwrap();
            match label {
                Some(label) => assert!(name.ends_with(&format!("({label})"))),
                None => assert!(!name.ends_with(')')),
            }
        }
    }
    // Publishing one connection must not implicitly publish its paid counterpart.
    settings.catalog.selected_models = vec!["openai/gpt-test".into()];
    let catalog = build_codex_catalog(&settings);
    assert_eq!(catalog.models.len(), 1);
    assert_eq!(catalog.models[0]["slug"], "gpt-test");
}

#[test]
fn connection_labels_follow_credentials_without_guessing_other_oauth_billing() {
    for (id, source, expected) in [
        ("anthropic", CredentialSource::Environment, Some("API")),
        ("openai", CredentialSource::OAuth, None),
        ("openai", CredentialSource::Forward, None),
        ("openai", CredentialSource::None, None),
        ("google", CredentialSource::OAuth, None),
    ] {
        let mut provider = ProviderDefinition {
            id: id.into(),
            ..ProviderDefinition::default()
        };
        provider.credential.source = source;
        assert_eq!(provider_connection_label(&provider), expected);
    }
}

#[test]
fn image_only_models_never_enter_the_codex_catalog() {
    let settings = GatewaySettings {
        providers: vec![ProviderDefinition {
            id: "openai".into(),
            name: "OpenAI".into(),
            models: vec!["gpt-test".into(), "gpt-image-2".into()],
            image_generation_models: vec!["imagegen-2".into(), "gpt-image-2".into()],
            ..ProviderDefinition::default()
        }],
        model_catalog: vec![crate::config::ModelMetadata {
            provider_id: "openai".into(),
            model_id: "imagegen-2".into(),
            enabled: true,
            ..crate::config::ModelMetadata::default()
        }],
        ..GatewaySettings::default()
    };

    let slugs = build_codex_catalog(&settings)
        .models
        .into_iter()
        .filter_map(|model| {
            model
                .get("slug")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect::<Vec<_>>();

    assert_eq!(slugs, vec!["gpt-test"]);
}

#[test]
fn image_only_routes_and_image_sidecar_never_enter_the_normal_catalog() {
    let settings = GatewaySettings {
        providers: vec![ProviderDefinition {
            id: "openai-api".into(),
            name: "OpenAI API".into(),
            models: vec!["gpt-5.5".into()],
            image_generation_models: vec!["gpt-image-2".into()],
            capabilities: ProviderCapabilities {
                image_generation: false,
                ..ProviderCapabilities::default()
            },
            ..ProviderDefinition::default()
        }],
        routes: vec![
            RouteDefinition {
                id: "only-image".into(),
                name: "Only image".into(),
                targets: vec![RouteTarget {
                    model: "openai-api/gpt-image-2".into(),
                    weight: 1,
                }],
                enabled: true,
                ..RouteDefinition::default()
            },
            RouteDefinition {
                id: "mixed".into(),
                name: "Mixed".into(),
                targets: vec![
                    RouteTarget {
                        model: "openai-api/gpt-image-2".into(),
                        weight: 1,
                    },
                    RouteTarget {
                        model: "openai-api/gpt-5.5".into(),
                        weight: 1,
                    },
                ],
                enabled: true,
                ..RouteDefinition::default()
            },
        ],
        sidecars: crate::config::SidecarSettings {
            image_model: Some("openai-api/gpt-image-2".into()),
            ..crate::config::SidecarSettings::default()
        },
        ..GatewaySettings::default()
    };

    let slugs = build_codex_catalog(&settings)
        .models
        .into_iter()
        .filter_map(|model| {
            model
                .get("slug")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect::<Vec<_>>();

    assert!(!slugs.iter().any(|slug| slug == "only-image"));
    assert!(!slugs.iter().any(|slug| slug == "codetas-sidecar/image"));
    assert!(slugs.iter().any(|slug| slug == "mixed"));
    assert!(slugs.iter().any(|slug| slug == "openai-api/gpt-5.5"));
}

#[test]
fn openai_allowlist_accepts_native_and_qualified_public_ids() {
    for selected in ["gpt-test", "openai/gpt-test"] {
        let mut settings = openai_settings();
        settings.catalog.selected_models = vec![selected.into()];

        let catalog = build_codex_catalog(&settings);

        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0]["slug"], "gpt-test");
        assert!(public_model_id_matches(
            &settings,
            selected,
            "openai/gpt-test"
        ));
        assert!(public_model_id_matches(&settings, selected, "gpt-test"));
    }
}

#[test]
fn qualified_openai_picker_order_applies_to_native_codex_slug() {
    let mut settings = openai_settings();
    settings.catalog.model_picker_order =
        vec!["openai/gpt-second".into(), "openai/gpt-test".into()];

    let catalog = build_codex_catalog(&settings);
    let slugs = catalog
        .models
        .iter()
        .filter_map(|model| model["slug"].as_str())
        .collect::<Vec<_>>();

    assert_eq!(slugs, vec!["gpt-second", "gpt-test"]);
}

#[test]
fn openai_alias_matching_does_not_reinterpret_routes_or_unknown_models() {
    let mut settings = openai_settings();
    settings.routes.push(RouteDefinition {
        id: "gpt-route".into(),
        name: "GPT route".into(),
        alias: Some("gpt-test".into()),
        targets: vec![crate::config::RouteTarget {
            model: "openai/gpt-test".into(),
            weight: 1,
        }],
        enabled: true,
        ..RouteDefinition::default()
    });

    assert!(!public_model_id_matches(
        &settings,
        "openai/gpt-test",
        "gpt-test"
    ));
    assert!(!public_model_id_matches(
        &settings,
        "openai/not-configured",
        "not-configured"
    ));
}

#[test]
fn builds_provider_and_route_catalog_entries() {
    let provider = ProviderDefinition {
        id: "test".into(),
        name: "Test provider".into(),
        base_url: "https://models.example/v1".into(),
        protocol: ProviderProtocol::Responses,
        models: vec!["model-a".into()],
        ..ProviderDefinition::default()
    };
    let settings = GatewaySettings {
        default_provider: Some("test".into()),
        providers: vec![provider],
        routes: vec![RouteDefinition {
            id: "reliable".into(),
            name: "Reliable".into(),
            targets: vec![crate::config::RouteTarget {
                model: "test/model-a".into(),
                weight: 1,
            }],
            enabled: true,
            ..RouteDefinition::default()
        }],
        ..GatewaySettings::default()
    };
    let catalog = build_codex_catalog(&settings);
    assert_eq!(catalog.models.len(), 2);
    assert!(catalog
        .models
        .iter()
        .any(|model| model["slug"] == "test/model-a"));
    assert!(catalog
        .models
        .iter()
        .any(|model| model["slug"] == "reliable"));
    assert!(catalog.validate_for_codex(Some("test/model-a")).is_ok());
    assert!(catalog.validate_for_codex(Some("missing/model")).is_err());
    let routed = catalog
        .models
        .iter()
        .find(|model| model["slug"] == "test/model-a")
        .unwrap();
    assert_eq!(routed["display_name"], "model-a");
    assert_eq!(routed["include_apps_usage_instructions"], true);
    assert_eq!(routed["include_plugin_usage_instructions"], true);
    assert!(routed.get("additional_speed_tiers").is_none());
    assert!(routed.get("service_tiers").is_none());

    let route = catalog
        .models
        .iter()
        .find(|model| model["slug"] == "reliable")
        .unwrap();
    assert_eq!(route["include_apps_usage_instructions"], true);
    assert_eq!(route["include_plugin_usage_instructions"], true);
}

#[test]
fn stale_text_only_provider_modalities_do_not_hide_default_vision() {
    let mut provider = ProviderDefinition {
        id: "command-code".into(),
        name: "Command Code".into(),
        base_url: "https://models.example/v1".into(),
        models: vec!["deepseek/deepseek-v4.1-flash".into()],
        ..ProviderDefinition::default()
    };
    provider
        .model_input_modalities
        .insert("deepseek/deepseek-v4.1-flash".into(), vec!["text".into()]);

    let catalog = build_codex_catalog(&GatewaySettings {
        providers: vec![provider],
        ..GatewaySettings::default()
    });
    let model = catalog
        .models
        .iter()
        .find(|model| model["slug"] == "command-code/deepseek/deepseek-v4.1-flash")
        .expect("DeepSeek V4.1 Flash catalog entry");
    assert_eq!(model["input_modalities"], json!(["text", "image"]));
}

#[test]
fn presets_without_discovery_limits_do_not_use_the_128k_default() {
    for (provider_id, model, window, auto_compact) in [
        (
            "opencode-go",
            "deepseek-v4.1-flash",
            1_000_000_u64,
            554_400_u64,
        ),
        ("opencode-zen", "deepseek-v4.1-flash", 1_000_000, 554_400),
        ("opencode-zen", "deepseek-v4.1-flash-free", 200_000, 180_000),
        ("opencode-go", "hy4", 1_048_576, 884_709),
        ("google-antigravity", "claude-opus-5-5", 400_000, 360_000),
        ("google-antigravity", "claude-sonnet-5-5", 400_000, 360_000),
        ("google-antigravity", "claude-sonnet-4-6", 400_000, 360_000),
        ("opencode-free", "deepseek-v4-flash-free", 200_000, 180_000),
    ] {
        let mut provider = crate::registry::provider_presets()
            .into_iter()
            .find(|preset| preset.id == provider_id)
            .unwrap()
            .instantiate(None)
            .unwrap();
        // Both providers list this model through discovery.
        provider.models.push(model.into());

        let catalog = build_codex_catalog(&GatewaySettings {
            providers: vec![provider],
            ..GatewaySettings::default()
        });
        let slug = format!("{provider_id}/{model}");
        let row = catalog
            .models
            .iter()
            .find(|row| row["slug"] == slug.as_str())
            .unwrap();

        assert_eq!(row["context_window"], window, "{slug}");
        assert_eq!(row["auto_compact_token_limit"], auto_compact, "{slug}");
    }
}

// Account usage accepts both spellings, so the limits table has to as well.
#[test]
fn the_alternate_command_code_id_finds_its_limits_table() {
    let mut settings = GatewaySettings {
        providers: vec![ProviderDefinition {
            id: "commandcode".into(),
            name: "Command Code".into(),
            base_url: "https://models.example/v1".into(),
            models: vec!["claude-haiku-4-5-20251001".into()],
            ..ProviderDefinition::default()
        }],
        ..GatewaySettings::default()
    };
    crate::registry::backfill_registry_input_limits(&mut settings);

    let catalog = build_codex_catalog(&settings);
    let row = catalog
        .models
        .iter()
        .find(|row| row["slug"] == "commandcode/claude-haiku-4-5-20251001")
        .unwrap();

    assert_eq!(row["context_window"], 200_000);
}

#[test]
fn github_models_gets_the_small_default_because_it_has_no_per_model_data() {
    let mut provider = crate::registry::provider_presets()
        .into_iter()
        .find(|preset| preset.id == "github-models")
        .unwrap()
        .instantiate(None)
        .unwrap();
    provider.models.push("openai/gpt-4o".into());
    let settings = GatewaySettings {
        providers: vec![provider],
        routes: vec![RouteDefinition {
            id: "github-route".into(),
            name: "GitHub route".into(),
            targets: vec![crate::config::RouteTarget {
                model: "github-models/openai/gpt-4o".into(),
                weight: 1,
            }],
            enabled: true,
            ..RouteDefinition::default()
        }],
        sidecars: crate::config::SidecarSettings {
            vision_model: Some("github-route".into()),
            ..crate::config::SidecarSettings::default()
        },
        ..GatewaySettings::default()
    };

    let catalog = build_codex_catalog(&settings);
    for slug in [
        "github-models/openai/gpt-4o",
        "github-route",
        "codetas-sidecar/vision",
    ] {
        let row = catalog
            .models
            .iter()
            .find(|row| row["slug"] == slug)
            .unwrap();
        assert_eq!(row["context_window"], 128_000, "{slug}");
        assert_eq!(row["auto_compact_token_limit"], 115_200, "{slug}");
    }
}

#[test]
fn an_output_limit_cannot_pull_the_trigger_below_half_the_window() {
    for (output, trigger) in [
        // Mismatched pairs: the floor applies.
        (128_000, 57_600),
        (131_072, 57_600),
        (64_001, 57_600),
        // Half the window is the last reserve taken at face value.
        (64_000, 57_600),
        (28_000, 90_000),
    ] {
        assert_eq!(
            catalog_auto_compact_token_limit(128_000, None, Some(output)),
            trigger,
            "{output}"
        );
    }
    // The output limits the registry ships keep their trigger.
    assert_eq!(
        catalog_auto_compact_token_limit(1_050_000, Some(922_000), Some(128_000)),
        829_800
    );
    assert_eq!(
        catalog_auto_compact_token_limit(1_048_576, None, Some(131_072)),
        825_753
    );
    assert_eq!(
        catalog_auto_compact_token_limit(1_000_000, None, Some(32_000)),
        871_200
    );
}

// Discovery reported the window, and the output limit came from a bigger sibling.
#[test]
fn a_reported_small_window_with_an_inherited_output_limit_keeps_a_usable_trigger() {
    let mut provider = ProviderDefinition {
        id: "test".into(),
        name: "Test".into(),
        base_url: "https://models.example/v1".into(),
        models: vec!["small-model".into()],
        ..ProviderDefinition::default()
    };
    provider
        .model_max_output_tokens
        .insert("small-model".into(), 131_072);
    let metadata = ModelMetadata {
        provider_id: "test".into(),
        model_id: "small-model".into(),
        enabled: true,
        context_window: Some(128_000),
        ..ModelMetadata::default()
    };
    let settings = GatewaySettings {
        providers: vec![provider],
        model_catalog: vec![metadata],
        routes: vec![RouteDefinition {
            id: "small-route".into(),
            name: "Small route".into(),
            targets: vec![crate::config::RouteTarget {
                model: "test/small-model".into(),
                weight: 1,
            }],
            enabled: true,
            ..RouteDefinition::default()
        }],
        ..GatewaySettings::default()
    };

    let catalog = build_codex_catalog(&settings);
    for slug in ["test/small-model", "small-route"] {
        let row = catalog
            .models
            .iter()
            .find(|row| row["slug"] == slug)
            .unwrap();
        assert_eq!(row["auto_compact_token_limit"], 57_600, "{slug}");
    }
}

#[test]
fn the_published_trigger_is_what_the_catalog_lists_for_that_slug() {
    let provider = ProviderDefinition {
        id: "test".into(),
        name: "Test".into(),
        base_url: "https://models.example/v1".into(),
        models: vec!["listed".into(), "unselected".into()],
        model_context_windows: BTreeMap::from([("listed".to_string(), 200_000)]),
        ..ProviderDefinition::default()
    };
    let mut settings = GatewaySettings {
        providers: vec![provider],
        ..GatewaySettings::default()
    };
    assert_eq!(
        published_auto_compact_limit(&settings, "test/listed"),
        Some(180_000)
    );
    assert_eq!(
        published_auto_compact_limit(&settings, "test/nothing"),
        None
    );

    settings.catalog.selected_models = vec!["test/listed".into()];
    assert_eq!(
        published_auto_compact_limit(&settings, "test/unselected"),
        None
    );
}

fn mistral_catalog(extra_models: &[&str], metadata: Vec<ModelMetadata>) -> CodexCatalog {
    let mut provider = crate::registry::provider_presets()
        .into_iter()
        .find(|preset| preset.id == "mistral")
        .unwrap()
        .instantiate(None)
        .unwrap();
    provider
        .models
        .extend(extra_models.iter().map(|model| (*model).to_string()));
    build_codex_catalog(&GatewaySettings {
        providers: vec![provider],
        model_catalog: metadata,
        ..GatewaySettings::default()
    })
}

fn catalog_row<'a>(catalog: &'a CodexCatalog, slug: &str) -> &'a Value {
    catalog
        .models
        .iter()
        .find(|row| row["slug"] == slug)
        .unwrap()
}

#[test]
fn a_mistral_model_with_no_listed_efforts_publishes_no_reasoning_levels() {
    let row_with = |model: &str, efforts: &[&str]| ModelMetadata {
        provider_id: "mistral".into(),
        model_id: model.into(),
        enabled: true,
        reasoning_efforts: efforts.iter().map(|effort| (*effort).to_string()).collect(),
        capabilities: ProviderCapabilities {
            reasoning: true,
            ..ProviderCapabilities::default()
        },
        ..ModelMetadata::default()
    };
    let catalog = mistral_catalog(
        &["codestral-latest"],
        vec![
            // What the `/models` fallback and a catalog row with no levels leave.
            row_with("mistral-small-latest", &[]),
            row_with("new-catalog-model", &["low", "high"]),
        ],
    );

    for slug in ["mistral/codestral-latest", "mistral/mistral-small-latest"] {
        let row = catalog_row(&catalog, slug);
        assert_eq!(row["supported_reasoning_levels"], json!([]), "{slug}");
        assert!(row.get("default_reasoning_level").is_none(), "{slug}");
    }
    for (slug, levels) in [
        ("mistral/mistral-large-4-0", 3),
        ("mistral/mistral-medium-latest", 1),
        ("mistral/new-catalog-model", 2),
    ] {
        let row = catalog_row(&catalog, slug);
        assert_eq!(
            row["supported_reasoning_levels"].as_array().unwrap().len(),
            levels,
            "{slug}"
        );
    }
}

#[test]
fn a_proxied_mistral_connection_keeps_the_generic_levels_for_an_unlisted_model() {
    let mut provider = crate::registry::provider_presets()
        .into_iter()
        .find(|preset| preset.id == "mistral")
        .unwrap()
        .instantiate(Some("https://proxy.example/v1"))
        .unwrap();
    provider.models.push("my-model".into());
    let catalog = build_codex_catalog(&GatewaySettings {
        providers: vec![provider],
        ..GatewaySettings::default()
    });
    assert_eq!(
        catalog_row(&catalog, "mistral/my-model")["supported_reasoning_levels"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn other_providers_keep_the_generic_levels_for_a_model_with_no_list() {
    let provider = ProviderDefinition {
        id: "test".into(),
        name: "Test".into(),
        base_url: "https://models.example/v1".into(),
        models: vec!["model-a".into()],
        capabilities: ProviderCapabilities {
            reasoning: true,
            ..ProviderCapabilities::default()
        },
        ..ProviderDefinition::default()
    };
    let catalog = build_codex_catalog(&GatewaySettings {
        providers: vec![provider],
        ..GatewaySettings::default()
    });
    let row = catalog_row(&catalog, "test/model-a");
    assert_eq!(
        row["supported_reasoning_levels"].as_array().unwrap().len(),
        3
    );
}

#[test]
fn zai_glm_5_3_publishes_as_text_only_whatever_its_rows_say() {
    let stale_row = ModelMetadata {
        provider_id: "mistral".into(),
        model_id: "zai-glm-5-3".into(),
        enabled: true,
        input_modalities: vec!["text".into(), "image".into()],
        capabilities: ProviderCapabilities::default(),
        ..ModelMetadata::default()
    };
    let catalog = mistral_catalog(&[], vec![stale_row.clone()]);
    assert_eq!(
        catalog_row(&catalog, "mistral/zai-glm-5-3")["input_modalities"],
        json!(["text"])
    );
    assert_eq!(
        catalog_row(&catalog, "mistral/zai-glm-5-3-flash")["input_modalities"],
        json!(["text", "image"])
    );

    let provider = crate::registry::provider_presets()
        .into_iter()
        .find(|preset| preset.id == "mistral")
        .unwrap()
        .instantiate(None)
        .unwrap();
    assert!(!effective_model_capabilities(&provider, Some(&stale_row), "zai-glm-5-3").vision);
    assert!(effective_model_capabilities(&provider, None, "zai-glm-5-3-flash").vision);

    // A route inherits the same limit from the catalog row it targets.
    let catalog = build_codex_catalog(&GatewaySettings {
        providers: vec![provider],
        model_catalog: vec![stale_row],
        routes: vec![RouteDefinition {
            id: "glm-route".into(),
            name: "GLM route".into(),
            targets: vec![crate::config::RouteTarget {
                model: "mistral/zai-glm-5-3".into(),
                weight: 1,
            }],
            enabled: true,
            ..RouteDefinition::default()
        }],
        ..GatewaySettings::default()
    });
    assert_eq!(
        catalog_row(&catalog, "glm-route")["input_modalities"],
        json!(["text"])
    );
}

#[test]
fn unknown_windows_use_the_recent_default_except_on_local_servers() {
    for (allow_private_network, window, auto_compact) in
        [(false, 400_000_u64, 360_000_u64), (true, 128_000, 115_200)]
    {
        let provider = ProviderDefinition {
            id: "test".into(),
            name: "Test".into(),
            base_url: "https://models.example/v1".into(),
            models: vec!["unknown-model".into()],
            allow_private_network,
            ..ProviderDefinition::default()
        };

        let catalog = build_codex_catalog(&GatewaySettings {
            providers: vec![provider],
            ..GatewaySettings::default()
        });
        let row = catalog
            .models
            .iter()
            .find(|row| row["slug"] == "test/unknown-model")
            .unwrap();

        assert_eq!(row["context_window"], window);
        assert_eq!(row["auto_compact_token_limit"], auto_compact);
    }
}

#[test]
fn a_sidecar_on_a_local_server_keeps_the_local_default_window() {
    let provider = ProviderDefinition {
        id: "local".into(),
        name: "Local".into(),
        base_url: "http://127.0.0.1:11434/v1".into(),
        models: vec!["vision-model".into()],
        allow_private_network: true,
        enabled: true,
        ..ProviderDefinition::default()
    };

    let catalog = build_codex_catalog(&GatewaySettings {
        providers: vec![provider],
        sidecars: crate::config::SidecarSettings {
            vision_model: Some("local/vision-model".into()),
            ..crate::config::SidecarSettings::default()
        },
        ..GatewaySettings::default()
    });
    let row = catalog
        .models
        .iter()
        .find(|row| row["slug"] == "codetas-sidecar/vision")
        .unwrap();

    assert_eq!(row["context_window"], 128_000);
}

#[test]
fn a_sidecar_routed_to_a_local_server_keeps_the_local_default_window() {
    let provider = ProviderDefinition {
        id: "local".into(),
        name: "Local".into(),
        base_url: "http://127.0.0.1:11434/v1".into(),
        models: vec!["vision-model".into()],
        allow_private_network: true,
        enabled: true,
        ..ProviderDefinition::default()
    };

    let catalog = build_codex_catalog(&GatewaySettings {
        providers: vec![provider],
        routes: vec![RouteDefinition {
            id: "local-vision".into(),
            name: "Local vision".into(),
            targets: vec![crate::config::RouteTarget {
                model: "local/vision-model".into(),
                weight: 1,
            }],
            enabled: true,
            ..RouteDefinition::default()
        }],
        sidecars: crate::config::SidecarSettings {
            vision_model: Some("local-vision".into()),
            ..crate::config::SidecarSettings::default()
        },
        ..GatewaySettings::default()
    });
    let row = catalog
        .models
        .iter()
        .find(|row| row["slug"] == "codetas-sidecar/vision")
        .unwrap();

    assert_eq!(row["context_window"], 128_000);
}

#[test]
fn a_sidecar_route_reads_its_targets_provider_window() {
    let provider = ProviderDefinition {
        id: "hosted".into(),
        name: "Hosted".into(),
        base_url: "https://models.example/v1".into(),
        models: vec!["vision-model".into()],
        model_context_windows: BTreeMap::from([("vision-model".to_string(), 64_000)]),
        ..ProviderDefinition::default()
    };
    let catalog = build_codex_catalog(&GatewaySettings {
        providers: vec![provider],
        routes: vec![RouteDefinition {
            id: "hosted-vision".into(),
            name: "Hosted vision".into(),
            targets: vec![crate::config::RouteTarget {
                model: "hosted/vision-model".into(),
                weight: 1,
            }],
            enabled: true,
            ..RouteDefinition::default()
        }],
        sidecars: crate::config::SidecarSettings {
            vision_model: Some("hosted-vision".into()),
            ..crate::config::SidecarSettings::default()
        },
        ..GatewaySettings::default()
    });
    assert_eq!(
        catalog_row(&catalog, "codetas-sidecar/vision")["context_window"],
        64_000
    );
}

fn hosted_provider(id: &str, models: &[&str]) -> ProviderDefinition {
    ProviderDefinition {
        id: id.into(),
        name: id.into(),
        base_url: "https://models.example/v1".into(),
        models: models.iter().map(|model| (*model).to_string()).collect(),
        ..ProviderDefinition::default()
    }
}

#[test]
fn a_model_the_registry_knows_is_small_does_not_get_the_hosted_default() {
    let catalog = build_codex_catalog(&GatewaySettings {
        providers: vec![hosted_provider(
            "other-host",
            // `minimax-m2.5` on Kiro is the smallest, and `anthropic` lists 1M for Opus.
            &["MINIMAX-M2.5", "claude-opus-5-5", "no-such-model"],
        )],
        routes: vec![RouteDefinition {
            id: "known-route".into(),
            name: "Known route".into(),
            targets: vec![crate::config::RouteTarget {
                model: "other-host/MINIMAX-M2.5".into(),
                weight: 1,
            }],
            enabled: true,
            ..RouteDefinition::default()
        }],
        sidecars: crate::config::SidecarSettings {
            vision_model: Some("other-host/MINIMAX-M2.5".into()),
            ..crate::config::SidecarSettings::default()
        },
        ..GatewaySettings::default()
    });
    for (slug, window, trigger) in [
        ("other-host/MINIMAX-M2.5", 200_000_u64, 180_000_u64),
        ("known-route", 200_000, 180_000),
        ("codetas-sidecar/vision", 200_000, 180_000),
        ("other-host/claude-opus-5-5", 400_000, 360_000),
        ("other-host/no-such-model", 400_000, 360_000),
    ] {
        let row = catalog_row(&catalog, slug);
        assert_eq!(row["context_window"], window, "{slug}");
        assert_eq!(row["auto_compact_token_limit"], trigger, "{slug}");
    }
}

#[test]
fn a_preset_model_with_only_a_lookup_or_explicit_row_is_not_given_400k() {
    for (provider_id, model, window) in [
        ("tencent-coding-plan", "minimax-m2.5", 200_000_u64),
        ("tencent-coding-plan", "kimi-k2.5", 262_144),
        ("ollama-cloud", "kimi-k2.6", 262_144),
        ("ollama-cloud", "gpt-oss:120b", 131_072),
    ] {
        let provider = crate::registry::provider_presets()
            .into_iter()
            .find(|preset| preset.id == provider_id)
            .unwrap()
            .instantiate(None)
            .unwrap();
        let catalog = build_codex_catalog(&GatewaySettings {
            providers: vec![provider],
            ..GatewaySettings::default()
        });
        let slug = format!("{provider_id}/{model}");
        assert_eq!(
            catalog_row(&catalog, &slug)["context_window"],
            window,
            "{slug}"
        );
    }
}

// A window saved for this provider outranks what any other provider states.
#[test]
fn a_stated_window_is_not_lowered_by_another_providers_row() {
    let mut provider = hosted_provider("other-host", &["minimax-m2.5"]);
    provider
        .model_context_windows
        .insert("minimax-m2.5".into(), 1_000_000);
    let catalog = build_codex_catalog(&GatewaySettings {
        providers: vec![provider],
        ..GatewaySettings::default()
    });
    assert_eq!(
        catalog_row(&catalog, "other-host/minimax-m2.5")["context_window"],
        1_000_000
    );
}

#[test]
fn litellm_routes_to_hosted_models_so_it_gets_the_hosted_default() {
    for (id, window) in [
        ("litellm", 400_000_u64),
        ("ollama", 128_000),
        ("lmstudio", 128_000),
        ("localai", 128_000),
    ] {
        let mut provider = crate::registry::provider_presets()
            .into_iter()
            .find(|preset| preset.id == id)
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.models.push("unknown-model".into());
        let catalog = build_codex_catalog(&GatewaySettings {
            providers: vec![provider],
            ..GatewaySettings::default()
        });
        let slug = format!("{id}/unknown-model");
        assert_eq!(
            catalog_row(&catalog, &slug)["context_window"],
            window,
            "{slug}"
        );
    }
}

#[test]
fn heterogeneous_routes_use_each_targets_real_usable_input_budget() {
    let mut openai = ProviderDefinition {
        id: "openai".into(),
        name: "OpenAI".into(),
        base_url: "https://chatgpt.com/backend-api/codex".into(),
        protocol: ProviderProtocol::Responses,
        models: vec!["gpt-5.6-sol".into()],
        ..ProviderDefinition::default()
    };
    openai
        .model_context_windows
        .insert("gpt-5.6-sol".into(), 372_000);
    let mut local = ProviderDefinition {
        id: "local".into(),
        name: "Local".into(),
        base_url: "https://models.example/v1".into(),
        protocol: ProviderProtocol::Responses,
        models: vec!["model-a".into()],
        ..ProviderDefinition::default()
    };
    local
        .model_context_windows
        .insert("model-a".into(), 128_000);
    let settings = GatewaySettings {
        providers: vec![openai, local],
        routes: vec![RouteDefinition {
            id: "mixed-budget".into(),
            name: "Mixed budget".into(),
            targets: vec![
                crate::config::RouteTarget {
                    model: "openai/gpt-5.6-sol".into(),
                    weight: 1,
                },
                crate::config::RouteTarget {
                    model: "local/model-a".into(),
                    weight: 1,
                },
            ],
            enabled: true,
            ..RouteDefinition::default()
        }],
        ..GatewaySettings::default()
    };

    let catalog = build_codex_catalog(&settings);
    let route = catalog
        .models
        .iter()
        .find(|model| model["slug"] == "mixed-budget")
        .unwrap();

    // min(372k - 100k OpenAI reserve, 128k local) * 90% = 115.2k.
    assert_eq!(route["auto_compact_token_limit"], 115_200);
}

#[test]
fn app_and_plugin_instructions_require_tools_on_every_route_target() {
    let provider = ProviderDefinition {
        id: "test".into(),
        name: "Test provider".into(),
        base_url: "https://models.example/v1".into(),
        protocol: ProviderProtocol::Responses,
        models: vec!["model-a".into(), "model-b".into()],
        ..ProviderDefinition::default()
    };
    let kiro_provider = ProviderDefinition {
        id: "kiro".into(),
        name: "Kiro".into(),
        base_url: "https://codewhisperer.us-east-1.amazonaws.com".into(),
        protocol: ProviderProtocol::Responses,
        transport: ProviderTransport::Kiro,
        models: vec!["claude-sonnet".into()],
        ..ProviderDefinition::default()
    };
    let settings = GatewaySettings {
        providers: vec![provider, kiro_provider],
        model_catalog: vec![
            ModelMetadata {
                provider_id: "test".into(),
                model_id: "model-a".into(),
                ..ModelMetadata::default()
            },
            ModelMetadata {
                provider_id: "test".into(),
                model_id: "model-b".into(),
                capabilities: ProviderCapabilities {
                    tools: false,
                    ..ProviderCapabilities::default()
                },
                ..ModelMetadata::default()
            },
        ],
        routes: vec![
            RouteDefinition {
                id: "mixed".into(),
                name: "Mixed".into(),
                targets: vec![
                    crate::config::RouteTarget {
                        model: "test/model-a".into(),
                        weight: 1,
                    },
                    crate::config::RouteTarget {
                        model: "test/model-b".into(),
                        weight: 1,
                    },
                ],
                enabled: true,
                ..RouteDefinition::default()
            },
            RouteDefinition {
                id: "kiro-route".into(),
                name: "Kiro route".into(),
                targets: vec![crate::config::RouteTarget {
                    model: "kiro/claude-sonnet".into(),
                    weight: 1,
                }],
                enabled: true,
                ..RouteDefinition::default()
            },
        ],
        ..GatewaySettings::default()
    };
    let catalog = build_codex_catalog(&settings);

    let tool_model = catalog
        .models
        .iter()
        .find(|model| model["slug"] == "test/model-a")
        .unwrap();
    assert_eq!(tool_model["include_apps_usage_instructions"], true);
    assert_eq!(tool_model["include_plugin_usage_instructions"], true);

    let no_tool_model = catalog
        .models
        .iter()
        .find(|model| model["slug"] == "test/model-b")
        .unwrap();
    assert_eq!(no_tool_model["include_apps_usage_instructions"], false);
    assert_eq!(no_tool_model["include_plugin_usage_instructions"], false);

    let mixed_route = catalog
        .models
        .iter()
        .find(|model| model["slug"] == "mixed")
        .unwrap();
    assert_eq!(mixed_route["include_apps_usage_instructions"], false);
    assert_eq!(mixed_route["include_plugin_usage_instructions"], false);

    let kiro_model = catalog
        .models
        .iter()
        .find(|model| model["slug"] == "kiro/claude-sonnet")
        .unwrap();
    assert_eq!(kiro_model["include_apps_usage_instructions"], false);
    assert_eq!(kiro_model["include_plugin_usage_instructions"], false);

    let kiro_route = catalog
        .models
        .iter()
        .find(|model| model["slug"] == "kiro-route")
        .unwrap();
    assert_eq!(kiro_route["include_apps_usage_instructions"], false);
    assert_eq!(kiro_route["include_plugin_usage_instructions"], false);
}

#[test]
fn native_openai_catalog_exposes_fast_toggle() {
    let provider = ProviderDefinition {
        id: "openai".into(),
        name: "OpenAI (Codex login)".into(),
        models: vec![
            "gpt-6-astra".into(),
            "gpt-5.6-sol".into(),
            "gpt-5.6-terra".into(),
            "gpt-5.3-codex-spark".into(),
        ],
        ..ProviderDefinition::default()
    };
    let catalog = build_codex_catalog(&GatewaySettings {
        providers: vec![provider],
        ..GatewaySettings::default()
    });
    let astra = catalog
        .models
        .iter()
        .find(|model| model["slug"] == "gpt-6-astra")
        .unwrap();
    assert_eq!(astra["display_name"], "GPT-6-Astra");
    assert_eq!(astra["additional_speed_tiers"], json!(["fast"]));

    let sol = catalog
        .models
        .iter()
        .find(|model| model["slug"] == "gpt-5.6-sol")
        .unwrap();
    assert_eq!(sol["display_name"], "GPT-5.6-Sol");
    assert_eq!(sol["additional_speed_tiers"], json!(["fast"]));
    assert_eq!(sol["service_tiers"][0]["id"], "priority");
    assert_eq!(sol["service_tiers"][0]["name"], "Fast");
    assert_eq!(sol["include_apps_usage_instructions"], true);
    assert_eq!(sol["include_plugin_usage_instructions"], true);
    assert_eq!(sol["include_skills_usage_instructions"], true);
    assert!(sol["base_instructions"]
        .as_str()
        .is_some_and(|text| text.contains("Skill and investigation contract")));

    let spark = catalog
        .models
        .iter()
        .find(|model| model["slug"] == "gpt-5.3-codex-spark")
        .unwrap();
    assert_eq!(spark["include_apps_usage_instructions"], false);
    assert_eq!(spark["include_plugin_usage_instructions"], false);
}

#[test]
fn auto_compaction_uses_ninety_percent_of_usable_input() {
    assert_eq!(
        catalog_auto_compact_token_limit(372_000, Some(272_000), None),
        244_800
    );
    assert_eq!(
        catalog_auto_compact_token_limit(200_000, Some(160_000), Some(20_000),),
        144_000
    );
    assert_eq!(
        catalog_auto_compact_token_limit(200_000, None, None),
        180_000
    );
}

#[test]
fn sidecars_never_expose_app_or_plugin_tools() {
    let capabilities = ProviderCapabilities::default();
    assert!(capabilities.tools);
    let sidecar = catalog_model(
        "codetas-sidecar/web-search",
        "CODETAS Web Search",
        "Capability-routed web search sidecar.",
        None,
        None,
        None,
        None,
        None,
        None,
        &capabilities,
        false,
        "v1",
        1,
        None,
        false,
    );
    assert_eq!(sidecar["include_apps_usage_instructions"], false);
    assert_eq!(sidecar["include_plugin_usage_instructions"], false);
}

#[test]
fn no_reasoning_models_omit_default_reasoning_level() {
    let provider = ProviderDefinition {
        id: "kimi".into(),
        name: "Kimi".into(),
        models: vec!["kimi-k2.7-code".into()],
        no_reasoning_models: vec!["kimi-k2.7-code".into()],
        ..ProviderDefinition::default()
    };
    let catalog = build_codex_catalog(&GatewaySettings {
        providers: vec![provider],
        ..GatewaySettings::default()
    });
    let kimi = catalog
        .models
        .iter()
        .find(|model| model["slug"] == "kimi/kimi-k2.7-code")
        .unwrap();
    assert_eq!(kimi["display_name"], "kimi-k2.7-code");
    assert_eq!(kimi["supported_reasoning_levels"], json!([]));
    assert!(kimi.get("default_reasoning_level").is_none());
    assert!(kimi.get("additional_speed_tiers").is_none());
}

#[test]
fn rejects_catalog_entries_without_instructions() {
    let catalog = CodexCatalog {
        models: vec![json!({"slug": "test/broken"})],
    };
    assert!(catalog.validate_for_codex(None).is_err());
}

#[test]
fn rejects_catalog_entries_whose_instructions_claim_another_model() {
    let catalog = CodexCatalog {
        models: vec![json!({
            "slug": "kimi/k3[1m]",
            "base_instructions": "Your model identifier is \"gpt-5.3-codex-spark\"."
        })],
    };
    assert!(catalog.validate_for_codex(None).is_err());
}

#[test]
fn generated_instructions_and_hash_are_bound_to_the_model_identity() {
    let provider = ProviderDefinition {
        id: "kimi".into(),
        name: "Kimi".into(),
        models: vec!["k3[1m]".into()],
        ..ProviderDefinition::default()
    };
    let catalog = build_codex_catalog(&GatewaySettings {
        providers: vec![provider],
        ..GatewaySettings::default()
    });
    let kimi = catalog
        .models
        .iter()
        .find(|model| model["slug"] == "kimi/k3[1m]")
        .unwrap();
    assert!(kimi["base_instructions"]
        .as_str()
        .is_some_and(|instructions| {
            instructions.contains("Your model identifier is \"kimi/k3[1m]\"")
                && instructions.contains("Skill and investigation contract")
                && instructions.contains("create_thread")
        }));
    assert_eq!(kimi["include_skills_usage_instructions"], true);
    assert!(kimi["comp_hash"]
        .as_str()
        .is_some_and(|hash| hash.starts_with("codetas-")));
    assert!(catalog.validate_for_codex(Some("kimi/k3[1m]")).is_ok());
}

#[test]
fn routed_vision_models_do_not_advertise_original_image_detail() {
    let mut capabilities = ProviderCapabilities::default();
    capabilities.vision = true;
    let modalities = ["text".to_string(), "image".to_string()];
    let routed = catalog_model(
        "alibaba-token-plan-intl/qwen3.8-max-preview",
        "Alibaba Token Plan (International) qwen3.8-max-preview",
        "Routed by CODETAS through Alibaba Token Plan (International).",
        Some(983_616),
        None,
        None,
        Some(modalities.as_slice()),
        None,
        None,
        &capabilities,
        true,
        "v2",
        1,
        None,
        false,
    );
    assert_eq!(routed["supports_image_detail_original"], false);
    let native = catalog_model(
        "gpt-5.6-sol",
        "GPT-5.6-Sol",
        "Routed by CODETAS through OpenAI (Codex login).",
        Some(272_000),
        None,
        None,
        Some(modalities.as_slice()),
        None,
        None,
        &capabilities,
        true,
        "v2",
        1,
        None,
        true,
    );
    assert_eq!(native["supports_image_detail_original"], true);
    let virtual_route = catalog_model(
        "fast-vision",
        "Fast Vision",
        "Virtual route managed by CODETAS.",
        Some(128_000),
        None,
        None,
        Some(modalities.as_slice()),
        None,
        None,
        &capabilities,
        true,
        "v2",
        1,
        None,
        false,
    );
    assert_eq!(virtual_route["supports_image_detail_original"], false);
    let openai_api = catalog_model(
        "gpt-5.6-sol",
        "GPT-5.6-Sol",
        "Routed by CODETAS through OpenAI API.",
        Some(272_000),
        None,
        None,
        Some(modalities.as_slice()),
        None,
        None,
        &capabilities,
        true,
        "v2",
        1,
        None,
        true,
    );
    assert_eq!(openai_api["supports_image_detail_original"], true);
    assert!(provider_advertises_image_detail_original("openai-api"));
    assert!(provider_advertises_image_detail_original("openai-apikey"));
    assert!(!provider_advertises_image_detail_original("openrouter"));
}

#[test]
fn instruction_templates_receive_the_skill_loop_guard() {
    let capabilities = ProviderCapabilities::default();
    let model = catalog_model(
        "gpt-5.6-sol",
        "GPT-5.6-Sol",
        "Routed by CODETAS through OpenAI (Codex login).",
        Some(272_000),
        None,
        None,
        None,
        None,
        None,
        &capabilities,
        true,
        "v2",
        1,
        Some("# Using skills\nRead SKILL.md completely before taking task actions."),
        true,
    );
    let template = model
        .pointer("/model_messages/instructions_template")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(template.contains("# Using skills"));
    assert!(template.contains("Skill and investigation contract"));
    assert!(template.contains("at most once per turn"));
    assert_eq!(model["include_skills_usage_instructions"], true);
}

#[test]
fn compatibility_hash_changes_when_model_instructions_change() {
    let capabilities = ProviderCapabilities::default();
    let first = catalog_compatibility_hash(
        "test/model",
        128_000,
        115_200,
        &["text".into()],
        &["low".into()],
        &capabilities,
        "v1",
        "Test Model",
        "Test description",
        "Your model identifier is \"test/model\".",
        None,
    );
    let second = catalog_compatibility_hash(
        "test/model",
        128_000,
        115_200,
        &["text".into()],
        &["low".into()],
        &capabilities,
        "v1",
        "Test Model",
        "Test description",
        "Your model identifier is \"another/model\".",
        None,
    );
    let renamed = catalog_compatibility_hash(
        "test/model",
        128_000,
        115_200,
        &["text".into()],
        &["low".into()],
        &capabilities,
        "v1",
        "Renamed Model",
        "Test description",
        "Your model identifier is \"test/model\".",
        None,
    );
    assert_ne!(first, second);
    assert_ne!(first, renamed);
}
