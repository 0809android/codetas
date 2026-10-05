use super::*;
use codetas_gateway::ModelMetadata;

/// Refresh only fields that still match their last accepted automatic value. User-owned
/// enablement, prices and instruction templates are never replaced by discovery.
fn merge_metadata(
    current: &mut ModelMetadata,
    fresh: &ModelMetadata,
    previous: Option<&ModelMetadata>,
    provider: &ProviderDefinition,
) -> ModelMetadata {
    let id = &fresh.model_id;
    // Older settings have no provenance. Only recognize registry defaults as
    // automatic values; retain ambiguous legacy overrides rather than erase them.
    let legacy = ModelMetadata {
        provider_id: fresh.provider_id.clone(),
        model_id: fresh.model_id.clone(),
        context_window: provider.model_context_windows.get(id).copied(),
        max_input_tokens: provider.model_max_input_tokens.get(id).copied(),
        max_output_tokens: provider.model_max_output_tokens.get(id).copied(),
        input_modalities: provider
            .model_input_modalities
            .get(id)
            .cloned()
            .unwrap_or_default(),
        reasoning_efforts: provider
            .model_reasoning_efforts
            .get(id)
            .cloned()
            .unwrap_or_default(),
        default_reasoning_effort: provider.model_default_reasoning_efforts.get(id).cloned(),
        ..ModelMetadata::default()
    };
    let baseline = previous.unwrap_or(&legacy);
    let before = current.clone();
    // Record accepted automatic values, not every incoming value. Missing data
    // must not erase provenance; manual overrides must not become automatic
    // merely because an upstream value happens to match them on a later fetch.
    let mut next_snapshot = baseline.clone();
    macro_rules! merge_optional {
        ($field:ident) => {
            if fresh.$field.is_some()
                && (current.$field.is_none() || current.$field == baseline.$field)
            {
                current.$field = fresh.$field.clone();
                next_snapshot.$field = fresh.$field.clone();
            }
        };
    }
    merge_optional!(display_name);
    merge_optional!(context_window);
    merge_optional!(max_input_tokens);
    merge_optional!(max_output_tokens);
    merge_optional!(default_reasoning_effort);
    for (current, incoming, baseline, snapshot) in [
        (
            &mut current.input_modalities,
            &fresh.input_modalities,
            &baseline.input_modalities,
            &mut next_snapshot.input_modalities,
        ),
        (
            &mut current.reasoning_efforts,
            &fresh.reasoning_efforts,
            &baseline.reasoning_efforts,
            &mut next_snapshot.reasoning_efforts,
        ),
    ] {
        if !incoming.is_empty() && (current.is_empty() || current == baseline) {
            *current = incoming.clone();
            *snapshot = incoming.clone();
        }
    }
    // Related fields form an atomic update. If new automatic values conflict
    // with retained manual/omitted values, preserve the valid old group AND its
    // baseline so a subsequent compatible response can still update it.
    let limits_valid = [
        current.context_window,
        current.max_input_tokens,
        current.max_output_tokens,
    ]
    .iter()
    .all(|limit| *limit != Some(0))
        && current.context_window.is_none_or(|context| {
            [current.max_input_tokens, current.max_output_tokens]
                .iter()
                .flatten()
                .all(|limit| *limit <= context)
        });
    if !limits_valid {
        current.context_window = before.context_window;
        current.max_input_tokens = before.max_input_tokens;
        current.max_output_tokens = before.max_output_tokens;
        next_snapshot.context_window = baseline.context_window;
        next_snapshot.max_input_tokens = baseline.max_input_tokens;
        next_snapshot.max_output_tokens = baseline.max_output_tokens;
    }
    let reasoning_valid = current
        .default_reasoning_effort
        .as_ref()
        .is_none_or(|default| {
            current.reasoning_efforts.is_empty() || current.reasoning_efforts.contains(default)
        });
    if !reasoning_valid {
        current.reasoning_efforts = before.reasoning_efforts;
        current.default_reasoning_effort = before.default_reasoning_effort;
        next_snapshot.reasoning_efforts = baseline.reasoning_efforts.clone();
        next_snapshot.default_reasoning_effort = baseline.default_reasoning_effort.clone();
    }
    // Keep the existing image-capability contract, independent of overrides.
    let wire = provider.wire_model_id(id);
    let managed = provider
        .models
        .iter()
        .chain(provider.image_generation_models.iter())
        .any(|model| provider.wire_model_id(model) == wire)
        || provider.default_model.as_deref() == Some(id);
    if managed {
        current.capabilities.image_generation = fresh.capabilities.image_generation;
    }
    next_snapshot
}

pub(super) fn merge_discovered_models(
    settings: &mut GatewaySettings,
    provider: &ProviderDefinition,
    discovered: Vec<ModelMetadata>,
) {
    if discovered.is_empty() {
        return;
    }
    let provider_id = &provider.id;
    let mut snapshots = provider.model_discovery_snapshots.clone();
    let mut aliases = provider.model_catalog_aliases.clone();
    let mut merged = settings
        .model_catalog
        .iter()
        .filter(|model| &model.provider_id == provider_id)
        .cloned()
        .map(|model| (model.model_id.clone(), model))
        .collect::<BTreeMap<_, _>>();
    let ids = discovered
        .iter()
        .map(|model| model.model_id.clone())
        .collect::<Vec<_>>();
    for model in discovered {
        let next_snapshot = if let Some(existing) = merged.get_mut(&model.model_id) {
            merge_metadata(existing, &model, snapshots.get(&model.model_id), provider)
        } else {
            let mut visible = model.clone();
            if provider_id == "google-antigravity"
                && matches!(
                    model.model_id.as_str(),
                    "claude-opus-5-5" | "claude-sonnet-5-5"
                )
            {
                let legacy = ["low", "medium", "high"]
                    .into_iter()
                    .filter_map(|effort| merged.get(&format!("{}-{effort}", model.model_id)))
                    .collect::<Vec<_>>();
                if !legacy.is_empty() {
                    // An entirely disabled family must not become enabled by migration.
                    visible.enabled = legacy.iter().any(|model| model.enabled);
                }
            }
            merged.insert(model.model_id.clone(), visible);
            model.clone()
        };
        if provider_id == "google-antigravity"
            && matches!(
                model.model_id.as_str(),
                "claude-opus-5-5" | "claude-sonnet-5-5"
            )
        {
            for effort in ["low", "medium", "high"] {
                aliases.insert(
                    format!("{}-{effort}", model.model_id),
                    model.model_id.clone(),
                );
            }
        }
        snapshots.insert(model.model_id.clone(), next_snapshot);
    }
    // Keep legacy metadata/defaults/route targets intact: old requests still
    // address their original wire variant. Hide those entries only in pickers.
    settings
        .model_catalog
        .retain(|model| &model.provider_id != provider_id);
    settings.model_catalog.extend(merged.into_values());
    if let Some(current) = settings
        .providers
        .iter_mut()
        .find(|item| &item.id == provider_id)
    {
        current.models = ids;
        current.model_discovery_snapshots = snapshots;
        current.model_catalog_aliases = aliases.clone();
    }
    // An empty list means publish all. Never turn it into an explicit allowlist.
    let prefix = format!("{provider_id}/");
    let mut seen = BTreeSet::new();
    for selected in &mut settings.catalog.selected_models {
        if let Some(base) = selected
            .strip_prefix(&prefix)
            .and_then(|id| aliases.get(id))
        {
            *selected = format!("{prefix}{base}");
        }
    }
    let canonical_publications = aliases
        .values()
        .map(|base| format!("{prefix}{base}"))
        .collect::<BTreeSet<_>>();
    settings
        .catalog
        .selected_models
        .retain(|id| !canonical_publications.contains(id) || seen.insert(id.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, window: u64) -> ModelMetadata {
        ModelMetadata {
            provider_id: "openai".into(),
            model_id: id.into(),
            display_name: Some(id.into()),
            context_window: Some(window),
            reasoning_efforts: vec!["low".into()],
            default_reasoning_effort: Some("low".into()),
            ..ModelMetadata::default()
        }
    }

    fn valid_settings() -> GatewaySettings {
        GatewaySettings {
            providers: vec![ProviderDefinition {
                id: "openai".into(),
                name: "OpenAI".into(),
                base_url: "https://api.openai.com/v1".into(),
                ..ProviderDefinition::default()
            }],
            ..GatewaySettings::default()
        }
    }

    fn refresh(settings: &mut GatewaySettings, fresh: ModelMetadata) {
        let provider = settings.providers[0].clone();
        merge_discovered_models(settings, &provider, vec![fresh]);
        settings
            .validate()
            .expect("refresh must keep settings valid");
    }

    #[test]
    fn missing_fields_keep_history_and_next_complete_response_updates() {
        let mut settings = valid_settings();
        let mut initial = model("test-model", 100000);
        initial.max_input_tokens = Some(80000);
        initial.max_output_tokens = Some(20000);
        initial.input_modalities = vec!["text".into()];
        refresh(&mut settings, initial.clone());
        refresh(
            &mut settings,
            ModelMetadata {
                provider_id: "openai".into(),
                model_id: "test-model".into(),
                ..ModelMetadata::default()
            },
        );
        let snapshot = &settings.providers[0].model_discovery_snapshots["test-model"];
        assert_eq!(snapshot.context_window, initial.context_window);
        assert_eq!(snapshot.max_input_tokens, initial.max_input_tokens);
        assert_eq!(snapshot.max_output_tokens, initial.max_output_tokens);
        assert_eq!(snapshot.display_name, initial.display_name);
        assert_eq!(snapshot.reasoning_efforts, initial.reasoning_efforts);
        assert_eq!(
            snapshot.default_reasoning_effort,
            initial.default_reasoning_effort
        );
        assert_eq!(snapshot.input_modalities, initial.input_modalities);
        // Provenance must also survive saving/reloading between partial responses.
        settings = serde_json::from_value(serde_json::to_value(settings).unwrap()).unwrap();
        let mut fresh = model("test-model", 200000);
        fresh.display_name = Some("updated".into());
        fresh.max_input_tokens = Some(180000);
        fresh.max_output_tokens = Some(40000);
        fresh.input_modalities = vec!["text".into(), "image".into()];
        fresh.reasoning_efforts = vec!["high".into()];
        fresh.default_reasoning_effort = Some("high".into());
        refresh(&mut settings, fresh.clone());
        let current = &settings.model_catalog[0];
        assert_eq!(current.context_window, fresh.context_window);
        assert_eq!(current.max_input_tokens, fresh.max_input_tokens);
        assert_eq!(current.max_output_tokens, fresh.max_output_tokens);
        assert_eq!(current.display_name, fresh.display_name);
        assert_eq!(current.reasoning_efforts, fresh.reasoning_efforts);
        assert_eq!(
            current.default_reasoning_effort,
            fresh.default_reasoning_effort
        );
        assert_eq!(current.input_modalities, fresh.input_modalities);
    }

    #[test]
    fn conflicting_limits_keep_valid_group_and_can_update_later() {
        for manual in [false, true] {
            for output_limit in [false, true] {
                let mut settings = valid_settings();
                let mut initial = model("test-model", 200000);
                if !manual {
                    if output_limit {
                        initial.max_output_tokens = Some(150000);
                    } else {
                        initial.max_input_tokens = Some(150000);
                    }
                }
                refresh(&mut settings, initial);
                if manual {
                    if output_limit {
                        settings.model_catalog[0].max_output_tokens = Some(150000);
                    } else {
                        settings.model_catalog[0].max_input_tokens = Some(150000);
                    }
                }
                settings.validate().unwrap();
                let mut conflict = model("test-model", 100000);
                conflict.display_name = Some("independent update".into());
                refresh(&mut settings, conflict);
                assert_eq!(settings.model_catalog[0].context_window, Some(200000));
                assert_eq!(
                    settings.model_catalog[0].display_name.as_deref(),
                    Some("independent update")
                );
                assert_eq!(
                    settings.providers[0].model_discovery_snapshots["test-model"].context_window,
                    Some(200000)
                );
                let mut compatible = model("test-model", if manual { 300000 } else { 100000 });
                if !manual {
                    if output_limit {
                        compatible.max_output_tokens = Some(90000);
                    } else {
                        compatible.max_input_tokens = Some(90000);
                    }
                }
                refresh(&mut settings, compatible.clone());
                assert_eq!(
                    settings.model_catalog[0].context_window,
                    compatible.context_window
                );
                let limit = if output_limit {
                    settings.model_catalog[0].max_output_tokens
                } else {
                    settings.model_catalog[0].max_input_tokens
                };
                assert_eq!(limit, Some(if manual { 150000 } else { 90000 }));
            }
        }
    }

    #[test]
    fn reasoning_group_preserves_manual_default_or_ladder_atomically() {
        for manual_ladder in [false, true] {
            let mut settings = valid_settings();
            let mut initial = model("test-model", 200000);
            initial.reasoning_efforts = vec!["low".into(), "high".into()];
            refresh(&mut settings, initial);
            if manual_ladder {
                settings.model_catalog[0].reasoning_efforts = vec!["low".into()];
            } else {
                settings.model_catalog[0].default_reasoning_effort = Some("high".into());
            }
            let before = settings.model_catalog[0].clone();
            let mut fresh = model("test-model", 200000);
            if manual_ladder {
                fresh.reasoning_efforts = vec!["low".into(), "high".into()];
                fresh.default_reasoning_effort = Some("high".into());
            }
            refresh(&mut settings, fresh);
            assert_eq!(
                settings.model_catalog[0].reasoning_efforts,
                before.reasoning_efforts
            );
            assert_eq!(
                settings.model_catalog[0].default_reasoning_effort,
                before.default_reasoning_effort
            );
            let mut compatible = model("test-model", 200000);
            compatible.reasoning_efforts = vec!["low".into(), "high".into(), "max".into()];
            refresh(&mut settings, compatible.clone());
            if manual_ladder {
                assert_eq!(settings.model_catalog[0].reasoning_efforts, ["low"]);
            } else {
                assert_eq!(
                    settings.model_catalog[0].reasoning_efforts,
                    compatible.reasoning_efforts
                );
                assert_eq!(
                    settings.model_catalog[0]
                        .default_reasoning_effort
                        .as_deref(),
                    Some("high")
                );
            }
        }
    }

    #[test]
    fn upstream_matching_manual_override_does_not_take_ownership() {
        let mut settings = valid_settings();
        refresh(&mut settings, model("test-model", 100000));
        settings.model_catalog[0].context_window = Some(150000);
        refresh(&mut settings, model("test-model", 150000));
        refresh(&mut settings, model("test-model", 200000));
        assert_eq!(settings.model_catalog[0].context_window, Some(150000));
        assert_eq!(
            settings.providers[0].model_discovery_snapshots["test-model"].context_window,
            Some(100000)
        );
    }

    #[test]
    fn repeated_discovery_refreshes_automatic_fields_and_retains_manual_edits() {
        let provider = ProviderDefinition {
            id: "openai".into(),
            ..ProviderDefinition::default()
        };
        let mut settings = GatewaySettings {
            providers: vec![provider.clone()],
            ..GatewaySettings::default()
        };
        merge_discovered_models(&mut settings, &provider, vec![model("gpt-6.1-sol", 272000)]);
        let provider = settings.providers[0].clone();
        let mut fresh = model("gpt-6.1-sol", 872000);
        fresh.reasoning_efforts.push("high".into());
        fresh.default_reasoning_effort = Some("high".into());
        merge_discovered_models(&mut settings, &provider, vec![fresh.clone()]);
        assert_eq!(settings.model_catalog[0].context_window, Some(872000));
        assert_eq!(settings.model_catalog[0].reasoning_efforts, ["low", "high"]);
        assert_eq!(
            settings.model_catalog[0]
                .default_reasoning_effort
                .as_deref(),
            Some("high")
        );
        settings.model_catalog[0].context_window = Some(123456);
        settings.model_catalog[0].display_name = Some("my model".into());
        settings.model_catalog[0].enabled = false;
        settings.model_catalog[0].instructions_template = Some("manual template".into());
        settings.model_catalog[0].input_price_per_million = Some(1.5);
        let provider = settings.providers[0].clone();
        fresh.context_window = Some(999999);
        fresh.display_name = Some("updated name".into());
        merge_discovered_models(&mut settings, &provider, vec![fresh]);
        let current = &settings.model_catalog[0];
        assert_eq!(current.context_window, Some(123456));
        assert_eq!(current.display_name.as_deref(), Some("my model"));
        assert!(!current.enabled);
        assert_eq!(current.input_price_per_million, Some(1.5));
        assert_eq!(
            current.instructions_template.as_deref(),
            Some("manual template")
        );
        // Provenance survives the same JSON round trip used by settings storage.
        let restored: GatewaySettings =
            serde_json::from_value(serde_json::to_value(&settings).unwrap()).unwrap();
        assert_eq!(
            restored.providers[0].model_discovery_snapshots["gpt-6.1-sol"].context_window,
            Some(872000)
        );
    }

    #[test]
    fn legacy_registry_values_refresh_but_ambiguous_manual_values_survive() {
        let mut provider = ProviderDefinition {
            id: "openai".into(),
            ..ProviderDefinition::default()
        };
        provider.model_context_windows.insert("auto".into(), 100000);
        provider
            .model_context_windows
            .insert("manual".into(), 100000);
        let mut settings = GatewaySettings {
            providers: vec![provider.clone()],
            model_catalog: vec![model("auto", 100000), model("manual", 50000)],
            ..GatewaySettings::default()
        };
        merge_discovered_models(
            &mut settings,
            &provider,
            vec![model("auto", 200000), model("manual", 200000)],
        );
        assert_eq!(settings.model_catalog[0].context_window, Some(200000));
        assert_eq!(settings.model_catalog[1].context_window, Some(50000));
    }

    #[test]
    fn antigravity_migration_hides_variants_and_preserves_wire_routes_and_publication() {
        let base = "claude-opus-5-5";
        let mut provider = ProviderDefinition {
            id: "google-antigravity".into(),
            name: "Antigravity".into(),
            default_model: Some(format!("{base}-high")),
            ..ProviderDefinition::default()
        };
        let mut settings = GatewaySettings::default();
        for effort in ["low", "medium", "high"] {
            let id = format!("{base}-{effort}");
            provider.models.push(id.clone());
            let mut metadata = model(&id, 200000);
            metadata.provider_id = provider.id.clone();
            settings.model_catalog.push(metadata);
            settings
                .catalog
                .selected_models
                .push(format!("{}/{}", provider.id, id));
        }
        settings.providers.push(provider.clone());
        settings.routes.push(codetas_gateway::RouteDefinition {
            id: "legacy-route".into(),
            enabled: false,
            targets: vec![codetas_gateway::RouteTarget {
                model: format!("google-antigravity/{base}-high"),
                weight: 1,
            }],
            ..codetas_gateway::RouteDefinition::default()
        });
        let routes_before = serde_json::to_value(&settings.routes).unwrap();
        let mut canonical = model(base, 1000000);
        canonical.provider_id = provider.id.clone();
        merge_discovered_models(&mut settings, &provider, vec![canonical.clone()]);
        assert_eq!(
            settings.catalog.selected_models,
            [format!("google-antigravity/{base}")]
        );
        assert_eq!(settings.providers[0].default_model, provider.default_model);
        assert_eq!(
            serde_json::to_value(&settings.routes).unwrap(),
            routes_before
        );
        // Legacy metadata stays available to requests that already name a variant.
        assert_eq!(settings.model_catalog.len(), 4);
        assert_eq!(settings.providers[0].model_catalog_aliases.len(), 3);
        let catalog = serde_json::to_value(build_codex_catalog(&settings)).unwrap();
        let models = catalog["models"].as_array().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["slug"], format!("google-antigravity/{base}"));
        settings.catalog.selected_models.clear();
        let provider = settings.providers[0].clone();
        merge_discovered_models(&mut settings, &provider, vec![canonical]);
        assert!(settings.catalog.selected_models.is_empty());
    }
}
