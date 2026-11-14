#[test]
fn removed_learning_setting_is_ignored_in_legacy_configuration() {
    let value: crate::config::CodexIntegrationSettings =
        serde_json::from_value(serde_json::json!({"selfImprovementMode": true})).unwrap();
    let encoded = serde_json::to_value(value).unwrap();
    assert!(encoded.get("selfImprovementMode").is_none());
    assert_eq!(encoded["autoConnect"], true);
}

#[test]
fn ipv6_forms_that_reach_private_ipv4_or_unseen_endpoints_are_private() {
    for address in [
        "64:ff9b::7f00:1",
        "64:ff9b::a9fe:a9fe",
        "64:ff9b::c0a8:101",
        "64:ff9b:1::1",
        "2002:7f00:1::1",
        "2002:a00:1::1",
        "::7f00:1",
        "::a00:1",
        "2001:0:4136:e378:8000:63bf:3fff:fdd2",
        "2001:2::1",
        "100::1",
        "fec0::1",
        "2001:db8::1",
    ] {
        let ip: IpAddr = address.parse().unwrap();
        assert!(is_private_ip(ip), "{address}");
    }
}

#[test]
fn ipv6_forms_of_public_hosts_stay_public() {
    for address in [
        "2606:4700:4700::1111",
        "2001:4860:4860::8888",
        // DNS64 on IPv6-only networks synthesizes these for public IPv4 hosts.
        "64:ff9b::808:808",
        "2002:808:808::1",
        "::ffff:8.8.8.8",
    ] {
        let ip: IpAddr = address.parse().unwrap();
        assert!(!is_private_ip(ip), "{address}");
    }
}

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
fn managed_client_ids_accept_all_owned_artifact_keys() {
    for client in [
        "claudeCode",
        "claudeDesktop",
        "claudeDesktopInference",
        "opencode",
        "grok",
        "pi",
        "hermes",
    ] {
        assert!(validate_managed_client_id(client).is_ok(), "{client}");
        let mut settings = GatewaySettings::default();
        settings
            .integrations
            .managed_clients
            .insert(client.into(), ManagedClientSettings::default());
        assert!(settings.validate().is_ok(), "{client}");
    }
}

#[test]
fn managed_client_ids_reject_controls_paths_and_oversized_values() {
    for client in ["", "claude\nCode", "../claudeCode", "claude/Code"] {
        assert!(validate_managed_client_id(client).is_err(), "{client:?}");
    }
    assert!(validate_managed_client_id(&"a".repeat(81)).is_err());
}

#[test]
fn omitted_advanced_capabilities_default_to_false_for_custom_providers() {
    let capabilities: ProviderCapabilities =
        serde_json::from_value(serde_json::json!({"tools": true})).expect("capabilities");
    assert!(!capabilities.structured_output);
    assert!(!capabilities.custom_tools);
    assert!(!capabilities.tool_search);
    assert!(!capabilities.mcp_namespaces);
}

#[test]
fn tool_subcapabilities_require_the_base_tools_capability() {
    let configurations: [fn(&mut ProviderCapabilities); 4] = [
        |capabilities: &mut ProviderCapabilities| capabilities.parallel_tools = true,
        |capabilities: &mut ProviderCapabilities| capabilities.custom_tools = true,
        |capabilities: &mut ProviderCapabilities| capabilities.tool_search = true,
        |capabilities: &mut ProviderCapabilities| capabilities.mcp_namespaces = true,
    ];
    for configure in configurations {
        let mut provider = provider("https://models.example/v1");
        provider.capabilities.tools = false;
        configure(&mut provider.capabilities);
        assert!(provider.validate().is_err());
    }
}

#[test]
fn model_tool_subcapabilities_require_the_base_tools_capability() {
    let mut settings = GatewaySettings::default();
    let provider = provider("https://models.example/v1");
    let provider_id = provider.id.clone();
    settings.providers = vec![provider];
    settings.default_provider = Some(provider_id.clone());
    let mut metadata = ModelMetadata {
        provider_id,
        model_id: "fixture-model".into(),
        ..ModelMetadata::default()
    };
    metadata.capabilities.tools = false;
    metadata.capabilities.custom_tools = true;
    settings.model_catalog = vec![metadata];

    assert!(settings.validate().is_err());
}

#[test]
fn effective_model_capabilities_keep_explicit_metadata_and_provider_exceptions() {
    let mut provider = provider("https://models.example/v1");
    provider.capabilities.structured_output = true;
    provider.capabilities.service_tier = false;
    provider.no_structured_output_models = vec!["legacy".into()];
    provider.service_tier_models = vec!["tiered".into()];
    let mut metadata = ModelMetadata {
        provider_id: provider.id.clone(),
        model_id: "explicit".into(),
        ..ModelMetadata::default()
    };
    metadata.capabilities.structured_output = false;

    assert!(
        !effective_model_capabilities(&provider, Some(&metadata), "explicit").structured_output
    );
    metadata.capabilities.structured_output = true;
    assert!(!effective_model_capabilities(&provider, Some(&metadata), "legacy").structured_output);
    assert!(effective_model_capabilities(&provider, Some(&metadata), "tiered").service_tier);
}

fn image_alias_provider() -> ProviderDefinition {
    let mut provider = ProviderDefinition {
        id: "openai".into(),
        name: "OpenAI".into(),
        base_url: "https://api.openai.com/v1".into(),
        capabilities: ProviderCapabilities {
            image_generation: true,
            ..ProviderCapabilities::default()
        },
        image_generation_models: vec!["imagegen-2".into(), "gpt-image-2".into()],
        ..ProviderDefinition::default()
    };
    provider
        .model_wire_ids
        .insert("imagegen-2".into(), "gpt-image-2".into());
    provider
}

fn image_metadata(model_id: &str, enabled: bool, image_generation: bool) -> ModelMetadata {
    ModelMetadata {
        provider_id: "openai".into(),
        model_id: model_id.into(),
        enabled,
        capabilities: ProviderCapabilities {
            image_generation,
            ..ProviderCapabilities::default()
        },
        ..ModelMetadata::default()
    }
}

#[test]
fn image_alias_is_rejected_when_canonical_catalog_model_is_disabled() {
    let provider = image_alias_provider();
    let settings = GatewaySettings {
        model_catalog: vec![image_metadata("gpt-image-2", false, true)],
        ..GatewaySettings::default()
    };

    assert!(!image_model_is_available(
        &settings,
        &provider,
        "imagegen-2"
    ));
    assert!(model_has_image_generation_identity(
        &settings,
        &provider,
        "imagegen-2"
    ));
}

#[test]
fn image_alias_is_rejected_when_canonical_capability_is_false() {
    let provider = image_alias_provider();
    let settings = GatewaySettings {
        model_catalog: vec![image_metadata("gpt-image-2", true, false)],
        ..GatewaySettings::default()
    };

    assert!(!image_model_is_available(
        &settings,
        &provider,
        "imagegen-2"
    ));
}

#[test]
fn image_alias_is_allowed_when_canonical_catalog_model_is_enabled() {
    let provider = image_alias_provider();
    let settings = GatewaySettings {
        model_catalog: vec![image_metadata("gpt-image-2", true, true)],
        ..GatewaySettings::default()
    };

    assert!(image_model_is_available(&settings, &provider, "imagegen-2"));
}

#[test]
fn explicitly_disabled_image_alias_is_rejected() {
    let provider = image_alias_provider();
    let settings = GatewaySettings {
        model_catalog: vec![
            image_metadata("imagegen-2", false, true),
            image_metadata("gpt-image-2", true, true),
        ],
        ..GatewaySettings::default()
    };

    assert!(!image_model_is_available(
        &settings,
        &provider,
        "imagegen-2"
    ));
}

#[test]
fn image_alias_with_explicitly_false_capability_is_rejected() {
    let provider = image_alias_provider();
    let settings = GatewaySettings {
        model_catalog: vec![
            image_metadata("imagegen-2", true, false),
            image_metadata("gpt-image-2", true, true),
        ],
        ..GatewaySettings::default()
    };

    assert!(!image_model_is_available(
        &settings,
        &provider,
        "imagegen-2"
    ));
}

#[test]
fn provider_wide_image_capability_does_not_promote_chat_models() {
    let mut provider = image_alias_provider();
    provider.models.push("gpt-5.5".into());

    assert!(!image_model_is_available(
        &GatewaySettings::default(),
        &provider,
        "gpt-5.5"
    ));
}

#[test]
fn image_identity_is_independent_from_provider_availability() {
    let mut provider = image_alias_provider();
    provider.capabilities.image_generation = false;

    assert!(model_has_image_generation_identity(
        &GatewaySettings::default(),
        &provider,
        "imagegen-2"
    ));
    assert!(!image_model_is_available(
        &GatewaySettings::default(),
        &provider,
        "imagegen-2"
    ));
}

#[test]
fn disabled_model_metadata_still_defines_image_identity() {
    let mut provider = image_alias_provider();
    provider.image_generation_models.clear();
    provider.model_wire_ids.clear();
    let settings = GatewaySettings {
        model_catalog: vec![image_metadata("render-v3", false, true)],
        ..GatewaySettings::default()
    };

    assert!(model_has_image_generation_identity(
        &settings,
        &provider,
        "render-v3"
    ));
    assert!(!image_model_is_available(&settings, &provider, "render-v3"));
}

#[test]
fn legacy_image_identity_uses_name_policy_only_without_explicit_contract() {
    let mut provider = image_alias_provider();
    provider.image_generation_models.clear();
    provider.model_wire_ids.clear();

    assert!(model_has_image_generation_identity(
        &GatewaySettings::default(),
        &provider,
        "legacy-imagegen-v1"
    ));
    assert!(!model_has_image_generation_identity(
        &GatewaySettings::default(),
        &provider,
        "gpt-5.5"
    ));
}

#[test]
fn arbitrary_explicit_image_model_and_wire_alias_are_available() {
    let mut provider = image_alias_provider();
    provider.image_generation_models = vec!["studio-art".into()];
    provider
        .model_wire_ids
        .insert("studio-art".into(), "art-v2".into());

    assert!(image_model_is_available(
        &GatewaySettings::default(),
        &provider,
        "art-v2"
    ));
    assert!(image_model_is_available(
        &GatewaySettings::default(),
        &provider,
        "studio-art"
    ));
}

#[test]
fn explicit_model_level_image_capability_is_sufficient() {
    let mut provider = image_alias_provider();
    provider.image_generation_models.clear();
    provider.model_wire_ids.clear();
    let settings = GatewaySettings {
        model_catalog: vec![image_metadata("render-v3", true, true)],
        ..GatewaySettings::default()
    };

    assert!(image_model_is_available(&settings, &provider, "render-v3"));
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
fn route_id_error_names_the_offending_id() {
    let provider = provider("https://models.example/v1");
    let settings = GatewaySettings {
        default_provider: Some(provider.id.clone()),
        providers: vec![provider],
        routes: vec![RouteDefinition {
            id: "deepseek-v4.1-flash-failover".into(),
            name: "Dotted id".into(),
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
    let error = settings.validate().unwrap_err();
    assert!(error.contains("route id"), "{error}");
    assert!(error.contains("deepseek-v4.1-flash-failover"), "{error}");
}

#[test]
fn creates_native_adapter_endpoints() {
    let mut provider = provider("https://api.anthropic.com/v1");
    provider.protocol = ProviderProtocol::AnthropicMessages;
    assert_eq!(
        provider.endpoint_for_model("claude-model"),
        "https://api.anthropic.com/v1/messages"
    );

    // A base saved without the version prefix must not degrade into a
    // `POST /messages` 404. Anthropic answers those with an empty body, so
    // the failure looks like an endpoint outage instead of a config bug.
    provider.base_url = "https://api.anthropic.com".into();
    assert_eq!(
        provider.endpoint_for_model("claude-model"),
        "https://api.anthropic.com/v1/messages"
    );

    provider.base_url = "https://api.anthropic.com/".into();
    assert_eq!(
        provider.endpoint_for_model("claude-model"),
        "https://api.anthropic.com/v1/messages"
    );

    // Non-Anthropic protocols keep their existing contract.
    provider.base_url = "https://generativelanguage.googleapis.com/v1beta".into();
    provider.protocol = ProviderProtocol::GeminiGenerateContent;
    assert_eq!(
        provider.endpoint_for_model("gemini-model"),
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-model:generateContent"
    );
}

#[test]
fn versions_anthropic_bases_exactly_once() {
    for (input, expected) in [
        ("https://api.anthropic.com", "https://api.anthropic.com/v1"),
        ("https://api.anthropic.com/", "https://api.anthropic.com/v1"),
        (
            "https://api.anthropic.com/v1",
            "https://api.anthropic.com/v1",
        ),
        (
            "https://api.anthropic.com/v1/",
            "https://api.anthropic.com/v1",
        ),
        // Proxies commonly mount the Anthropic API under a path.
        (
            "https://gateway.example/anthropic",
            "https://gateway.example/anthropic/v1",
        ),
        (
            "https://gateway.example/anthropic/v2",
            "https://gateway.example/anthropic/v2",
        ),
    ] {
        assert_eq!(versioned_anthropic_base(input), expected, "input={input}");
    }
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
    assert!(settings.catalog.selected_models.is_empty());
    assert!(settings.catalog.model_picker_order.is_empty());
    assert!(!settings.catalog.compatibility_lab);
    assert_eq!(
        settings.runtime.memory_budget_bytes,
        default_memory_budget_bytes()
    );
    assert_eq!(
        settings.runtime.max_inflight_requests,
        default_max_inflight_requests()
    );
}

#[test]
fn compatibility_lab_is_opt_in_and_preserves_explicit_values() {
    let (missing, _) = parse_gateway_settings_json(
        br#"{"version":2,"defaultProvider":null,"providers":[],"catalog":{}}"#,
    )
    .expect("settings without compatibility lab flag");
    assert!(!missing.catalog.compatibility_lab);

    for expected in [false, true] {
        let content = format!(
                "{{\"version\":2,\"defaultProvider\":null,\"providers\":[],\"catalog\":{{\"compatibilityLab\":{expected}}}}}"
            );
        let (settings, _) = parse_gateway_settings_json(content.as_bytes())
            .expect("settings with explicit compatibility lab flag");
        assert_eq!(settings.catalog.compatibility_lab, expected);
    }
}

#[test]
fn deepseek_id_repair_migration_opts_in_missing_fields_but_preserves_false() {
    let deepseek = crate::registry::provider_presets()
        .into_iter()
        .find(|preset| preset.id == "deepseek")
        .unwrap()
        .instantiate(None)
        .unwrap();
    let mut missing = serde_json::to_value(GatewaySettings {
        providers: vec![deepseek.clone()],
        ..GatewaySettings::default()
    })
    .unwrap();
    let provider = missing["providers"][0].as_object_mut().unwrap();
    provider.remove("repairInvalidResponseItemIds");
    provider
        .get_mut("responseItemIdRepair")
        .and_then(serde_json::Value::as_object_mut)
        .unwrap()
        .remove("repairMissingTerminalIds");

    let (settings, migrated) =
        parse_gateway_settings_json(&serde_json::to_vec(&missing).unwrap()).unwrap();
    assert!(migrated);
    assert!(settings.providers[0].repair_invalid_response_item_ids);
    assert!(
        settings.providers[0]
            .response_item_id_repair
            .repair_missing_terminal_ids
    );

    let mut opted_out = serde_json::to_value(GatewaySettings {
        providers: vec![deepseek],
        ..GatewaySettings::default()
    })
    .unwrap();
    opted_out["providers"][0]["repairInvalidResponseItemIds"] = serde_json::json!(false);
    opted_out["providers"][0]["responseItemIdRepair"]["repairMissingTerminalIds"] =
        serde_json::json!(false);
    let (settings, _) =
        parse_gateway_settings_json(&serde_json::to_vec(&opted_out).unwrap()).unwrap();
    assert!(!settings.providers[0].repair_invalid_response_item_ids);
    assert!(
        !settings.providers[0]
            .response_item_id_repair
            .repair_missing_terminal_ids
    );
}

#[test]
fn local_compaction_settings_default_to_v2_and_accept_v1_rollback() {
    let defaulted =
        parse_gateway_settings_json(br#"{"version":2,"providers":[],"defaultProvider":null}"#)
            .unwrap()
            .0;
    assert!(defaulted.local_compaction.generate_v2());
    assert_eq!(defaulted.local_compaction.tail_token_limit(), 20_000);

    let rolled_back = parse_gateway_settings_json(
            br#"{"version":2,"providers":[],"defaultProvider":null,"localCompaction":{"envelope":"v1","tailTokenLimit":12000}}"#,
        )
        .unwrap()
        .0;
    assert!(!rolled_back.local_compaction.generate_v2());
    assert_eq!(rolled_back.local_compaction.tail_token_limit(), 12_000);
}

#[test]
fn migrates_missing_registry_input_limits_in_v2_settings() {
    let mut provider = crate::registry::provider_presets()
        .into_iter()
        .find(|preset| preset.id == "openai")
        .unwrap()
        .instantiate(None)
        .unwrap();
    provider.model_max_input_tokens.clear();
    let content = serde_json::to_vec(&GatewaySettings {
        providers: vec![provider],
        ..GatewaySettings::default()
    })
    .unwrap();

    let (settings, migrated) = parse_gateway_settings_json(&content).unwrap();

    assert!(migrated);
    assert_eq!(
        settings.providers[0]
            .model_max_input_tokens
            .get("gpt-5.6-sol")
            .copied(),
        Some(272_000)
    );
}

#[test]
fn rejects_future_registry_revisions() {
    let mut settings = GatewaySettings::default();
    settings.registry_revision = REGISTRY_REVISION + 1;
    assert!(settings.validate().is_err());
}

#[test]
fn loads_persisted_registry_revision_ten() {
    let mut settings = GatewaySettings::default();
    settings.registry_revision = 10;
    let content = serde_json::to_vec(&settings).unwrap();
    let (parsed, _) = parse_gateway_settings_json(&content).unwrap();
    assert_eq!(parsed.registry_revision, REGISTRY_REVISION);
    assert!(parsed.validate().is_ok());
}

#[test]
fn upgrades_registry_revision_nine_to_current() {
    let mut settings = GatewaySettings::default();
    settings.registry_revision = 9;
    let content = serde_json::to_vec(&settings).unwrap();
    let (parsed, migrated) = parse_gateway_settings_json(&content).unwrap();
    assert!(migrated);
    assert_eq!(parsed.registry_revision, REGISTRY_REVISION);
}

#[test]
fn sanitizes_empty_and_duplicate_catalog_models_on_load() {
    let raw = serde_json::json!({
        "version": 2,
        "defaultProvider": null,
        "providers": [],
        "catalog": {
            "selectedModels": ["gpt-6-astra", " ", "gpt-5.6-sol", "gpt-6-astra"],
            "modelPickerOrder": ["gpt-6-astra", "gpt-6-astra", ""]
        }
    });
    let (settings, migrated) =
        parse_gateway_settings_json(&serde_json::to_vec(&raw).unwrap()).unwrap();
    assert!(migrated);
    assert_eq!(
        settings.catalog.selected_models,
        vec!["gpt-6-astra", "gpt-5.6-sol"]
    );
    assert_eq!(settings.catalog.model_picker_order, vec!["gpt-6-astra"]);
}

#[test]
fn keeps_clean_catalog_model_lists_without_migration() {
    let mut settings = GatewaySettings::default();
    settings.catalog.selected_models = vec!["gpt-6-astra".into(), "gpt-5.6-sol".into()];
    let content = serde_json::to_vec(&settings).unwrap();
    let (parsed, migrated) = parse_gateway_settings_json(&content).unwrap();
    assert!(!migrated);
    assert_eq!(
        parsed.catalog.selected_models,
        vec!["gpt-6-astra", "gpt-5.6-sol"]
    );
}
