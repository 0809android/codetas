use super::*;

pub(crate) fn backfill_registry_input_limits(settings: &mut GatewaySettings) -> bool {
    let mut changed = false;
    let migrate_antigravity_models =
        settings.registry_revision < ANTIGRAVITY_MODEL_MIGRATION_REVISION;
    let migrate_deepseek_v41_flash = settings.registry_revision < DEEPSEEK_V41_FLASH_MODEL_REVISION;
    let migrate_default_vision = settings.registry_revision < DEFAULT_VISION_CAPABILITY_REVISION;
    let sidecar_image_targets = models_sidecars_send_images_to(settings);
    let mut catalog_models = BTreeMap::<String, HashSet<String>>::new();
    let registry_capabilities = provider_presets()
        .into_iter()
        .filter_map(|preset| {
            let id = preset.id.to_string();
            preset
                .instantiate(
                    preset
                        .requires_custom_url
                        .then_some("https://fixture.invalid/v1"),
                )
                .ok()
                .map(|provider| (id, provider.capabilities))
        })
        .collect::<BTreeMap<_, _>>();
    for model in settings.model_catalog.iter().filter(|model| model.enabled) {
        catalog_models
            .entry(model.provider_id.clone())
            .or_default()
            .insert(model.model_id.clone());
    }
    for provider in &mut settings.providers {
        let mut defaults = ProviderDefinition {
            id: provider.id.clone(),
            protocol: provider.protocol,
            ..ProviderDefinition::default()
        };
        apply_registry_defaults(&mut defaults);
        if settings.registry_revision < MODEL_CAPABILITY_ISOLATION_REVISION
            && provider.id == "xai"
            && provider.capabilities.image_generation
            && provider.image_generation_models.is_empty()
        {
            // xAI historically exposed image support only as a provider-wide
            // capability. Preserve the built-in image sidecar for existing
            // settings before model-level capability isolation removes the
            // stale image identity from normal discovered models.
            provider.image_generation_models = defaults.image_generation_models.clone();
            changed = true;
        }
        if settings.registry_revision < IMAGE_MODEL_ISOLATION_REVISION
            && matches!(
                provider.id.as_str(),
                "openai" | "openai-api" | "openai-apikey"
            )
        {
            // A serialized boolean cannot distinguish an omitted legacy value
            // from an explicit user opt-out. Preserve `false` fail-closed while
            // still backfilling the model aliases needed when the capability is
            // enabled. New preset instances retain the registry's `true` default.
            for (alias, wire_model) in &defaults.model_wire_ids {
                if alias != "imagegen-2" {
                    continue;
                }
                if !provider.model_wire_ids.contains_key(alias) {
                    provider
                        .model_wire_ids
                        .insert(alias.clone(), wire_model.clone());
                    changed = true;
                }
            }
            if provider
                .model_wire_ids
                .get("gpt-image-2")
                .is_some_and(|wire| wire == "gpt-image-2")
            {
                provider.model_wire_ids.remove("gpt-image-2");
                changed = true;
            }
            for model in ["imagegen-2", "gpt-image-2"] {
                if !provider
                    .image_generation_models
                    .iter()
                    .any(|configured| configured == model)
                {
                    provider.image_generation_models.push(model.into());
                    changed = true;
                }
            }
            let previous_model_count = provider.models.len();
            provider
                .models
                .retain(|model| !matches!(model.as_str(), "imagegen-2" | "gpt-image-2"));
            if provider.models.len() != previous_model_count {
                changed = true;
            }
            for model in ["imagegen-2", "gpt-image-2"] {
                changed |= provider.model_context_windows.remove(model).is_some();
                changed |= provider.model_max_input_tokens.remove(model).is_some();
                changed |= provider.model_max_output_tokens.remove(model).is_some();
                changed |= provider.model_input_modalities.remove(model).is_some();
                changed |= provider.model_reasoning_efforts.remove(model).is_some();
                changed |= provider
                    .model_default_reasoning_efforts
                    .remove(model)
                    .is_some();
            }
        }
        if settings.registry_revision < SAFE_CAPABILITY_DEFAULTS_REVISION {
            if let Some(registry) = registry_capabilities.get(&provider.id) {
                if registry.structured_output && !provider.capabilities.structured_output {
                    provider.capabilities.structured_output = true;
                    changed = true;
                }
                if provider.capabilities.tools {
                    changed |= enable_capability(
                        &mut provider.capabilities.custom_tools,
                        registry.custom_tools,
                    );
                    changed |= enable_capability(
                        &mut provider.capabilities.tool_search,
                        registry.tool_search,
                    );
                    changed |= enable_capability(
                        &mut provider.capabilities.mcp_namespaces,
                        registry.mcp_namespaces,
                    );
                }
            }
            if !provider.capabilities.tools {
                changed |= disable_capability(&mut provider.capabilities.parallel_tools);
                changed |= disable_capability(&mut provider.capabilities.custom_tools);
                changed |= disable_capability(&mut provider.capabilities.tool_search);
                changed |= disable_capability(&mut provider.capabilities.mcp_namespaces);
            }
        }
        if settings.registry_revision < STRICT_RESPONSES_TOOL_ADJACENCY_REVISION
            && defaults.requires_adjacent_responses_tool_results
            && !provider.requires_adjacent_responses_tool_results
        {
            provider.requires_adjacent_responses_tool_results = true;
            changed = true;
        }
        if settings.registry_revision < CONFORMANCE_POLICY_REVISION {
            if defaults.capabilities.service_tier && !provider.capabilities.service_tier {
                provider.capabilities.service_tier = true;
                changed = true;
            }
            changed |= backfill_non_empty_strings(
                &mut provider.service_tier_models,
                &defaults.service_tier_models,
            );
            changed |= backfill_non_empty_strings(
                &mut provider.no_structured_output_models,
                &defaults.no_structured_output_models,
            );
            changed |= backfill_non_empty_strings(
                &mut provider.anthropic_eof_tolerance_models,
                &defaults.anthropic_eof_tolerance_models,
            );
            changed |= backfill_non_empty_strings(
                &mut provider.terminal_continuation_guard_models,
                &defaults.terminal_continuation_guard_models,
            );
            changed |= backfill_non_empty_strings(
                &mut provider.empty_completion_retry_models,
                &defaults.empty_completion_retry_models,
            );
        }
        if settings.registry_revision < STRICT_RESPONSES_TOOL_ADJACENCY_REVISION
            && provider.requires_reasoning_placeholder_models.is_none()
        {
            if let Some(models) = &defaults.requires_reasoning_placeholder_models {
                provider.requires_reasoning_placeholder_models = Some(models.clone());
                changed = true;
            }
        }
        if provider.id == "google-antigravity"
            && provider.models.is_empty()
            && !defaults.models.is_empty()
        {
            provider.models = defaults.models.clone();
            changed = true;
        }
        if settings.registry_revision < ANTIGRAVITY_GEMINI4_MODEL_REVISION
            && provider.id == "google-antigravity"
        {
            let model = "gemini-4-argon";
            if !provider.models.iter().any(|configured| configured == model) {
                provider.models.insert(0, model.into());
                changed = true;
            }
            if !provider.model_context_windows.contains_key(model) {
                if let Some(window) = defaults
                    .model_context_windows
                    .get(model)
                    .filter(|window| window_fits_saved_limits(provider, model, **window))
                {
                    provider.model_context_windows.insert(model.into(), *window);
                    changed = true;
                }
            }
            if !provider.model_reasoning_efforts.contains_key(model) {
                if let Some(efforts) = defaults
                    .model_reasoning_efforts
                    .get(model)
                    .filter(|efforts| efforts_fit_saved_default(provider, model, efforts))
                {
                    provider
                        .model_reasoning_efforts
                        .insert(model.into(), efforts.clone());
                    changed = true;
                }
            }
            if !provider.model_default_reasoning_efforts.contains_key(model) {
                if let Some(effort) = defaults
                    .model_default_reasoning_efforts
                    .get(model)
                    .filter(|effort| default_effort_fits_saved_efforts(provider, model, effort))
                {
                    provider
                        .model_default_reasoning_efforts
                        .insert(model.into(), effort.clone());
                    changed = true;
                }
            }
            // Revisions 20-22 pointed the aliases at `-tiered` / `-medium` ids that
            // Antigravity does not serve; the model is addressed by its own id.
            let before = provider.model_wire_ids.len();
            provider.model_wire_ids.retain(|alias, wire| {
                !(alias.starts_with("gemini-4-argon") && wire.starts_with("gemini-4-argon-"))
            });
            changed |= provider.model_wire_ids.len() != before;
            let published = format!("{}/{}", provider.id, model);
            if provider.enabled
                && !settings
                    .catalog
                    .selected_models
                    .iter()
                    .any(|id| id == &published)
            {
                settings.catalog.selected_models.push(published);
                changed = true;
            }
        }
        if settings.registry_revision < ANTIGRAVITY_EMPTY_RETRY_REVISION
            && provider.id == "google-antigravity"
        {
            changed |= backfill_non_empty_strings(
                &mut provider.empty_completion_retry_models,
                &defaults.empty_completion_retry_models,
            );
        }
        if settings.registry_revision < OPENDESIGN_EMPTY_RETRY_REVISION
            && provider.id == "opendesign"
        {
            changed |= backfill_non_empty_strings(
                &mut provider.empty_completion_retry_models,
                &defaults.empty_completion_retry_models,
            );
        }
        if migrate_antigravity_models && provider.id == "google-antigravity" {
            for model in ["gemini-3.7-flash", "gemini-3.5-flash"] {
                if !provider.models.iter().any(|configured| configured == model) {
                    let default_index = defaults
                        .models
                        .iter()
                        .position(|candidate| candidate == model)
                        .unwrap_or(provider.models.len());
                    provider
                        .models
                        .insert(default_index.min(provider.models.len()), model.into());
                    changed = true;
                }
                if !provider.model_context_windows.contains_key(model) {
                    if let Some(context) = defaults
                        .model_context_windows
                        .get(model)
                        .filter(|context| window_fits_saved_limits(provider, model, **context))
                    {
                        provider
                            .model_context_windows
                            .insert(model.into(), *context);
                        changed = true;
                    }
                }
                if !provider.model_reasoning_efforts.contains_key(model) {
                    if let Some(efforts) = defaults
                        .model_reasoning_efforts
                        .get(model)
                        .filter(|efforts| efforts_fit_saved_default(provider, model, efforts))
                    {
                        provider
                            .model_reasoning_efforts
                            .insert(model.into(), efforts.clone());
                        changed = true;
                    }
                }
                if !provider.model_default_reasoning_efforts.contains_key(model) {
                    if let Some(effort) = defaults
                        .model_default_reasoning_efforts
                        .get(model)
                        .filter(|effort| default_effort_fits_saved_efforts(provider, model, effort))
                    {
                        provider
                            .model_default_reasoning_efforts
                            .insert(model.into(), effort.clone());
                        changed = true;
                    }
                }
            }
        }
        if migrate_deepseek_v41_flash && provider.id == "deepseek" {
            // DeepSeek's current V4.1 Flash API model is exposed as
            // `deepseek-flash`; keep the old `deepseek-v4-flash` alias for
            // existing routes and configurations.
            let model = "deepseek-flash";
            if !provider.models.iter().any(|configured| configured == model) {
                let insert_at = provider
                    .models
                    .iter()
                    .position(|configured| configured == "deepseek-v4-flash")
                    .unwrap_or(provider.models.len());
                provider.models.insert(insert_at, model.into());
                changed = true;
            }
            if !provider
                .no_structured_output_models
                .iter()
                .any(|configured| configured == model)
            {
                provider.no_structured_output_models.push(model.into());
                changed = true;
            }
            if provider
                .model_protocols
                .insert(model.into(), ProviderProtocol::Responses)
                != Some(ProviderProtocol::Responses)
            {
                changed = true;
            }
            if settings.catalog.selected_models.iter().any(|selected| {
                selected == "deepseek/deepseek-v4-flash" || selected == "deepseek-v4-flash"
            }) && !settings
                .catalog
                .selected_models
                .iter()
                .any(|selected| selected == "deepseek/deepseek-flash")
            {
                settings
                    .catalog
                    .selected_models
                    .push("deepseek/deepseek-flash".into());
                changed = true;
            }
        }
        if migrate_default_vision && !provider.capabilities.vision {
            provider.capabilities.vision = true;
            changed = true;
        }
        if settings.registry_revision < ANTHROPIC_FABLE_OPUS_CONTEXT_REVISION
            && matches!(provider.id.as_str(), "anthropic" | "anthropic-apikey")
        {
            // Fill a missing window only. An existing value is left untouched
            // because a serialized number cannot say whether it came from the
            // old 128k default or from the user's own choice: a user who set
            // 64_000 or 128_000 deliberately would otherwise be silently
            // raised to 1,000,000. Recovering an intended smaller value after
            // the fact is not possible, so never overwrite.
            for (model, window) in ANTHROPIC_EXPLICIT_CONTEXT_MODELS {
                if !provider.models.iter().any(|configured| configured == model) {
                    continue;
                }
                if !provider.model_context_windows.contains_key(*model)
                    && window_fits_saved_limits(provider, model, *window)
                {
                    provider
                        .model_context_windows
                        .insert((*model).into(), *window);
                    changed = true;
                }
            }
        }
        if settings.registry_revision < ANTIGRAVITY_SONNET_46_WINDOW_REVISION
            && provider.id == "google-antigravity"
            && provider.model_context_windows.get(ANTIGRAVITY_SONNET_46)
                == Some(&ANTIGRAVITY_SONNET_46_OLD_WINDOW)
        {
            provider.model_context_windows.remove(ANTIGRAVITY_SONNET_46);
            changed = true;
        }
        if settings.registry_revision < MISTRAL_VIBE_PLAN_REVISION
            && provider.id == "mistral"
            && crate::compat::is_mistral_chat_endpoint(&provider.base_url)
        {
            // The per-model rows arrive through the fills below, which know how to
            // keep them consistent with what the connection already saved, because
            // these models are configured from here on.
            provider.prompt_cache_key = true;
            // Any path other than the old default is the user's own.
            if provider.discovery.path == ModelDiscoverySettings::default().path {
                provider.discovery.path = defaults.discovery.path.clone();
            }
            provider.capabilities.reasoning = true;
            for model in &defaults.models {
                if !provider.models.contains(model) {
                    provider.models.push(model.clone());
                }
            }
            if provider.default_model.is_none() {
                provider.default_model = defaults.default_model.clone();
            }
            changed = true;
        }
        if settings.registry_revision < OPENCODE_DEEPSEEK_V41_REASONING_REVISION
            && matches!(
                provider.id.as_str(),
                "opencode-go" | "opencode-zen" | "opencode-free"
            )
        {
            for (model, mapping) in &defaults.model_reasoning_effort_map {
                if model.starts_with("deepseek-v4.1")
                    && !provider.model_reasoning_effort_map.contains_key(model)
                {
                    provider
                        .model_reasoning_effort_map
                        .insert(model.clone(), mapping.clone());
                    changed = true;
                }
            }
        }
        // Migrate only the old preset budget once; retain non-default limits
        // and allow users to explicitly restore a smaller cap after migration.
        if settings.registry_revision < OPENAI_REQUEST_BUDGET_REVISION
            && matches!(
                provider.id.as_str(),
                "openai" | "openai-api" | "openai-apikey"
            )
            && provider.limits.max_request_bytes == 16 * 1024 * 1024
        {
            provider.limits.max_request_bytes = OPENAI_MAX_REQUEST_BYTES;
            changed = true;
        }
        // Same one-shot rule as the OpenAI budget: only the old preset value
        // is raised; a limit the user chose is kept.
        if settings.registry_revision < ANTHROPIC_REQUEST_BUDGET_REVISION
            && matches!(provider.id.as_str(), "anthropic" | "anthropic-apikey")
            && provider.limits.max_request_bytes == 16 * 1024 * 1024
        {
            provider.limits.max_request_bytes = ANTHROPIC_MAX_REQUEST_BYTES;
            changed = true;
        }
        if settings.registry_revision < GPT6_ASTRA_MODEL_REVISION
            && matches!(
                provider.id.as_str(),
                "openai" | "openai-api" | "openai-apikey"
            )
        {
            let model = "gpt-6-astra";
            if !provider.models.iter().any(|configured| configured == model) {
                provider.models.insert(0, model.into());
                changed = true;
            }
            if !provider
                .service_tier_models
                .iter()
                .any(|configured| configured == model)
            {
                provider.service_tier_models.insert(0, model.into());
                changed = true;
            }
            if !provider.model_context_windows.contains_key(model) {
                if let Some(value) = defaults
                    .model_context_windows
                    .get(model)
                    .filter(|value| window_fits_saved_limits(provider, model, **value))
                {
                    provider.model_context_windows.insert(model.into(), *value);
                    changed = true;
                }
            }
            if !provider.model_max_input_tokens.contains_key(model) {
                if let Some(value) = defaults
                    .model_max_input_tokens
                    .get(model)
                    .filter(|value| limit_fits_saved_window(provider, model, **value))
                {
                    provider.model_max_input_tokens.insert(model.into(), *value);
                    changed = true;
                }
            }
            if !provider.model_max_output_tokens.contains_key(model) {
                if let Some(value) = defaults
                    .model_max_output_tokens
                    .get(model)
                    .filter(|value| limit_fits_saved_window(provider, model, **value))
                {
                    provider
                        .model_max_output_tokens
                        .insert(model.into(), *value);
                    changed = true;
                }
            }
            if !provider.model_input_modalities.contains_key(model) {
                if let Some(value) = defaults.model_input_modalities.get(model) {
                    provider
                        .model_input_modalities
                        .insert(model.into(), value.clone());
                    changed = true;
                }
            }
            if !provider.model_reasoning_efforts.contains_key(model) {
                if let Some(value) = defaults
                    .model_reasoning_efforts
                    .get(model)
                    .filter(|value| efforts_fit_saved_default(provider, model, value))
                {
                    provider
                        .model_reasoning_efforts
                        .insert(model.into(), value.clone());
                    changed = true;
                }
            }
        }
        if settings.registry_revision < SEPTEMBER_MODEL_REFRESH_REVISION {
            let additions: &[&str] = match provider.id.as_str() {
                "openai" | "openai-api" | "openai-apikey" => &["gpt-6-sol", "gpt-6-luna"],
                "anthropic" | "anthropic-apikey" => &["claude-fable-5-1", "claude-opus-5-5"],
                "xai" => &["grok-4.7"],
                _ => &[],
            };
            for &model in additions {
                if !provider.models.iter().any(|existing| existing == model) {
                    provider.models.push(model.into());
                    changed = true;
                }
                if defaults
                    .service_tier_models
                    .iter()
                    .any(|existing| existing == model)
                    && !provider
                        .service_tier_models
                        .iter()
                        .any(|existing| existing == model)
                {
                    provider.service_tier_models.push(model.into());
                    changed = true;
                }
                if !provider.model_max_output_tokens.contains_key(model) {
                    if let Some(limit) = defaults
                        .model_max_output_tokens
                        .get(model)
                        .filter(|limit| limit_fits_saved_window(provider, model, **limit))
                    {
                        provider
                            .model_max_output_tokens
                            .insert(model.into(), *limit);
                        changed = true;
                    }
                }
            }
        }
        if settings.registry_revision < ANTHROPIC_SONNET_55_REVISION
            && matches!(provider.id.as_str(), "anthropic" | "anthropic-apikey")
        {
            // Add the new model and fill only missing per-model metadata, so
            // user-edited windows or efforts survive the migration.
            let model = "claude-sonnet-5-5";
            // Move only the previous preset default; a user-picked default stays.
            if provider.default_model.as_deref() == Some("claude-sonnet-5") {
                provider.default_model = Some(model.into());
                changed = true;
            }
            if !provider.models.iter().any(|existing| existing == model) {
                provider.models.push(model.into());
                changed = true;
            }
            if !provider.model_context_windows.contains_key(model) {
                if let Some(value) = defaults
                    .model_context_windows
                    .get(model)
                    .filter(|value| window_fits_saved_limits(provider, model, **value))
                {
                    provider.model_context_windows.insert(model.into(), *value);
                    changed = true;
                }
            }
            if !provider.model_reasoning_efforts.contains_key(model) {
                if let Some(value) = defaults
                    .model_reasoning_efforts
                    .get(model)
                    .filter(|value| efforts_fit_saved_default(provider, model, value))
                {
                    provider
                        .model_reasoning_efforts
                        .insert(model.into(), value.clone());
                    changed = true;
                }
            }
            // Surface it in Codex only when this provider's Claude models are
            // already selected; never opt an unused provider in.
            let prefix = format!("{}/", provider.id);
            let qualified = format!("{prefix}{model}");
            let selected = &mut settings.catalog.selected_models;
            if selected.iter().any(|entry| entry.starts_with(&prefix))
                && !selected.iter().any(|entry| entry == &qualified)
            {
                selected.push(qualified);
                changed = true;
            }
        }
        if settings.registry_revision < GPT61_SOL_MODEL_REVISION
            && matches!(
                provider.id.as_str(),
                "openai" | "openai-api" | "openai-apikey"
            )
        {
            // Fill only missing per-model metadata so a user-edited window or
            // effort list survives the migration. `gpt-6-sol` and `gpt-6-luna`
            // shipped in the September refresh without a picker entry; this
            // revision adds the current default model next to them.
            let model = "gpt-6.1-sol";
            let published = if provider.id == "openai" {
                model.to_string()
            } else {
                format!("{}/{}", provider.id, model)
            };
            // Surface it in Codex only when this provider already publishes
            // models; never opt an unused provider in. `selected_models`
            // entries for native OpenAI are bare slugs.
            // An empty list means "publish everything"; pushing an entry
            // would turn it into a one-model allowlist, so leave it empty.
            let provider_is_published = !settings.catalog.selected_models.is_empty()
                && settings.catalog.selected_models.iter().any(|entry| {
                    entry == model
                        || entry == &published
                        || provider.models.iter().any(|configured| {
                            entry == configured
                                || entry == &format!("{}/{}", provider.id, configured)
                        })
                });
            if !provider.models.iter().any(|existing| existing == model) {
                // Keep the family order by following `gpt-6-sol` when present.
                let insert_at = provider
                    .models
                    .iter()
                    .position(|configured| configured == "gpt-6-sol")
                    .map(|index| index + 1)
                    .unwrap_or(provider.models.len());
                provider
                    .models
                    .insert(insert_at.min(provider.models.len()), model.into());
                changed = true;
            }
            if !provider.model_context_windows.contains_key(model) {
                if let Some(value) = defaults
                    .model_context_windows
                    .get(model)
                    .filter(|value| window_fits_saved_limits(provider, model, **value))
                {
                    provider.model_context_windows.insert(model.into(), *value);
                    changed = true;
                }
            }
            if !provider.model_max_input_tokens.contains_key(model) {
                if let Some(value) = defaults
                    .model_max_input_tokens
                    .get(model)
                    .filter(|value| limit_fits_saved_window(provider, model, **value))
                {
                    provider.model_max_input_tokens.insert(model.into(), *value);
                    changed = true;
                }
            }
            if !provider.model_max_output_tokens.contains_key(model) {
                if let Some(value) = defaults
                    .model_max_output_tokens
                    .get(model)
                    .filter(|value| limit_fits_saved_window(provider, model, **value))
                {
                    provider
                        .model_max_output_tokens
                        .insert(model.into(), *value);
                    changed = true;
                }
            }
            if !provider.model_input_modalities.contains_key(model) {
                if let Some(value) = defaults.model_input_modalities.get(model) {
                    provider
                        .model_input_modalities
                        .insert(model.into(), value.clone());
                    changed = true;
                }
            }
            if !provider.model_reasoning_efforts.contains_key(model) {
                if let Some(value) = defaults
                    .model_reasoning_efforts
                    .get(model)
                    .filter(|value| efforts_fit_saved_default(provider, model, value))
                {
                    provider
                        .model_reasoning_efforts
                        .insert(model.into(), value.clone());
                    changed = true;
                }
            }
            if !provider.model_default_reasoning_efforts.contains_key(model) {
                if let Some(value) = defaults
                    .model_default_reasoning_efforts
                    .get(model)
                    .filter(|value| default_effort_fits_saved_efforts(provider, model, value))
                {
                    provider
                        .model_default_reasoning_efforts
                        .insert(model.into(), value.clone());
                    changed = true;
                }
            }
            if provider
                .service_tier_models
                .iter()
                .any(|configured| configured == "gpt-6-sol")
                && !provider
                    .service_tier_models
                    .iter()
                    .any(|configured| configured == model)
            {
                provider.service_tier_models.push(model.into());
                changed = true;
            }
            let selected = &mut settings.catalog.selected_models;
            if provider_is_published && !selected.iter().any(|entry| entry == &published) {
                selected.push(published);
                changed = true;
            }
        }
        if settings.registry_revision < RETIRED_ANTHROPIC_MYTHOS_REVISION
            && matches!(provider.id.as_str(), "anthropic" | "anthropic-apikey")
        {
            // Drop the retired ID everywhere it can be selected or described.
            // Leave the default model alone unless it *is* the retired ID,
            // which can only ever be an upstream 404.
            let retired = RETIRED_ANTHROPIC_MODELS;
            let previous_len = provider.models.len();
            provider
                .models
                .retain(|model| !retired.contains(&model.as_str()));
            changed |= provider.models.len() != previous_len;
            if provider
                .default_model
                .as_deref()
                .is_some_and(|model| retired.contains(&model))
            {
                provider.default_model = None;
                changed = true;
            }
            for model in retired {
                changed |= provider.model_context_windows.remove(*model).is_some();
                changed |= provider.model_max_input_tokens.remove(*model).is_some();
                changed |= provider.model_max_output_tokens.remove(*model).is_some();
                changed |= provider.model_input_modalities.remove(*model).is_some();
                changed |= provider.model_reasoning_efforts.remove(*model).is_some();
                changed |= provider
                    .model_default_reasoning_efforts
                    .remove(*model)
                    .is_some();
                changed |= provider.model_reasoning_modes.remove(*model).is_some();
                changed |= provider.model_protocols.remove(*model).is_some();
                changed |= provider.model_wire_ids.remove(*model).is_some();
                changed |= provider.model_reasoning_effort_map.remove(*model).is_some();
                for list in [
                    &mut provider.no_reasoning_models,
                    &mut provider.no_temperature_models,
                    &mut provider.no_top_p_models,
                    &mut provider.no_penalty_models,
                    &mut provider.no_structured_output_models,
                    &mut provider.service_tier_models,
                    &mut provider.anthropic_eof_tolerance_models,
                    &mut provider.terminal_continuation_guard_models,
                    &mut provider.empty_completion_retry_models,
                    &mut provider.auto_tool_choice_only_models,
                    &mut provider.preserve_reasoning_content_models,
                    &mut provider.reasoning_split_models,
                    &mut provider.thinking_toggle_models,
                    &mut provider.thinking_budget_models,
                ] {
                    let previous_len = list.len();
                    list.retain(|entry| !retired.contains(&entry.as_str()));
                    changed |= list.len() != previous_len;
                }
            }
        }
        let configured_models = provider
            .models
            .iter()
            .cloned()
            .chain(provider.default_model.iter().cloned())
            .chain(
                catalog_models
                    .get(&provider.id)
                    .into_iter()
                    .flat_map(|models| models.iter().cloned()),
            )
            .collect::<HashSet<_>>();
        for (model, limit) in &defaults.model_max_input_tokens {
            if !configured_models.contains(model) {
                continue;
            }
            if !provider.model_context_windows.contains_key(model) {
                if let Some(context) = defaults
                    .model_context_windows
                    .get(model)
                    .filter(|context| window_fits_saved_limits(provider, model, **context))
                {
                    provider
                        .model_context_windows
                        .insert(model.clone(), *context);
                    changed = true;
                }
            }
            if !provider.model_max_input_tokens.contains_key(model) {
                let input_limit = provider
                    .model_context_windows
                    .get(model)
                    .map(|context| (*limit).min(*context))
                    .unwrap_or(*limit);
                provider
                    .model_max_input_tokens
                    .insert(model.clone(), input_limit);
                changed = true;
            }
        }
        // Context windows are independent of max-input contracts. Discovered
        // models such as grok-4.6 used to keep the 128k catalog fallback until
        // a matching max-input row existed.
        for (model, context) in &defaults.model_context_windows {
            if !configured_models.contains(model)
                || provider.model_context_windows.contains_key(model)
                || !window_fits_saved_limits(provider, model, *context)
            {
                continue;
            }
            provider
                .model_context_windows
                .insert(model.clone(), *context);
            changed = true;
        }
        let inherited = configured_models
            .iter()
            .filter(|model| !provider.model_context_windows.contains_key(model.as_str()))
            .filter_map(|model| {
                resolve_model_context_window(&provider.model_context_windows, model)
                    .filter(|window| window_fits_saved_limits(provider, model, *window))
                    .map(|window| (model.clone(), window))
            })
            .collect::<Vec<_>>();
        for (model, window) in inherited {
            provider.model_context_windows.insert(model, window);
            changed = true;
        }
        for (model, efforts) in &defaults.model_reasoning_efforts {
            if !configured_models.contains(model)
                || provider.model_reasoning_efforts.contains_key(model)
                || !efforts_fit_saved_default(provider, model, efforts)
            {
                continue;
            }
            provider
                .model_reasoning_efforts
                .insert(model.clone(), efforts.clone());
            changed = true;
        }
        for (model, effort) in &defaults.model_default_reasoning_efforts {
            if !configured_models.contains(model)
                || provider.model_default_reasoning_efforts.contains_key(model)
                || !default_effort_fits_saved_efforts(provider, model, effort)
            {
                continue;
            }
            provider
                .model_default_reasoning_efforts
                .insert(model.clone(), effort.clone());
            changed = true;
        }
        for (model, modalities) in &defaults.model_input_modalities {
            if !configured_models.contains(model)
                || provider.model_input_modalities.contains_key(model)
                || (!modalities.iter().any(|modality| modality == "image")
                    && sidecar_image_targets.contains(&format!("{}/{}", provider.id, model)))
            {
                continue;
            }
            provider
                .model_input_modalities
                .insert(model.clone(), modalities.clone());
            changed = true;
        }
        if settings.registry_revision < PRESERVE_REASONING_ONCE_REVISION {
            for model in &defaults.preserve_reasoning_content_models {
                if !configured_models.contains(model)
                    || provider
                        .preserve_reasoning_content_models
                        .iter()
                        .any(|existing| existing == model)
                {
                    continue;
                }
                provider
                    .preserve_reasoning_content_models
                    .push(model.clone());
                changed = true;
            }
        }
    }
    if settings.registry_revision < ANTIGRAVITY_SONNET_46_WINDOW_REVISION {
        // Discovery copies the window into the catalog row, which outranks the
        // provider's own map.
        for metadata in &mut settings.model_catalog {
            if metadata.provider_id == "google-antigravity"
                && metadata.model_id == ANTIGRAVITY_SONNET_46
                && metadata.context_window == Some(ANTIGRAVITY_SONNET_46_OLD_WINDOW)
            {
                metadata.context_window = None;
                changed = true;
            }
        }
    }
    if migrate_default_vision {
        for metadata in &mut settings.model_catalog {
            if !metadata.capabilities.vision {
                metadata.capabilities.vision = true;
                changed = true;
            }
            if !metadata
                .input_modalities
                .iter()
                .any(|modality| modality == "image")
            {
                metadata.input_modalities.push("image".into());
                changed = true;
            }
        }
    }
    if settings.registry_revision < MODEL_CAPABILITY_ISOLATION_REVISION {
        // Before model capability isolation, discovery copied the provider-wide
        // image flag into every catalog row. Only models explicitly declared as
        // image models may retain that identity during migration. Restrict the
        // repair to models already configured on the provider so user-defined
        // metadata-only overrides remain intact.
        let provider_image_contracts = settings
            .providers
            .iter()
            .map(|provider| {
                (
                    provider.id.clone(),
                    (
                        provider.capabilities.image_generation,
                        provider.models.clone(),
                        provider.default_model.clone(),
                        provider.image_generation_models.clone(),
                        provider.model_wire_ids.clone(),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        for metadata in &mut settings.model_catalog {
            let Some((provider_image_generation, models, default_model, image_models, wire_ids)) =
                provider_image_contracts.get(&metadata.provider_id)
            else {
                continue;
            };
            let is_configured_model = model_matches_registry(&metadata.model_id, models)
                || default_model.as_deref() == Some(metadata.model_id.as_str());
            let wire_model = wire_ids
                .get(&metadata.model_id)
                .unwrap_or(&metadata.model_id);
            let explicit_image_model = image_models
                .iter()
                .any(|configured| wire_ids.get(configured).unwrap_or(configured) == wire_model);
            if !*provider_image_generation || (!is_configured_model && !explicit_image_model) {
                continue;
            }
            if !explicit_image_model {
                changed |= disable_capability(&mut metadata.capabilities.image_generation);
            }
        }
    }
    if settings.registry_revision < SAFE_CAPABILITY_DEFAULTS_REVISION {
        let provider_defaults = settings
            .providers
            .iter()
            .map(|provider| {
                (
                    provider.id.clone(),
                    (
                        provider.capabilities.clone(),
                        provider.no_structured_output_models.clone(),
                        provider.service_tier_models.clone(),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        for metadata in &mut settings.model_catalog {
            let Some((provider, no_structured, service_tier)) =
                provider_defaults.get(&metadata.provider_id)
            else {
                continue;
            };
            let structured_output = provider.structured_output
                && !model_matches_registry(&metadata.model_id, no_structured);
            let service_tier =
                provider.service_tier || model_matches_registry(&metadata.model_id, service_tier);
            changed |= set_capability(
                &mut metadata.capabilities.structured_output,
                structured_output,
            );
            changed |= set_capability(&mut metadata.capabilities.service_tier, service_tier);
            changed |= set_capability(
                &mut metadata.capabilities.custom_tools,
                provider.tools && provider.custom_tools,
            );
            changed |= set_capability(
                &mut metadata.capabilities.tool_search,
                provider.tools && provider.tool_search,
            );
            changed |= set_capability(
                &mut metadata.capabilities.mcp_namespaces,
                provider.tools && provider.mcp_namespaces,
            );
            changed |= set_capability(
                &mut metadata.capabilities.provider_metadata,
                provider.provider_metadata,
            );
            if !metadata.capabilities.tools {
                changed |= disable_capability(&mut metadata.capabilities.parallel_tools);
                changed |= disable_capability(&mut metadata.capabilities.custom_tools);
                changed |= disable_capability(&mut metadata.capabilities.tool_search);
                changed |= disable_capability(&mut metadata.capabilities.mcp_namespaces);
            }
        }
    }
    if settings.registry_revision < RETIRED_ANTHROPIC_MYTHOS_REVISION {
        // The retired ID can also linger in the catalog, in a route target, or
        // in the user's picker selection. Remove it from all three so nothing
        // offers a model that can only return an upstream 404.
        for entry in RETIRED_ANTHROPIC_MODELS {
            let route_suffix = format!("/{entry}");
            let previous_len = settings.model_catalog.len();
            settings.model_catalog.retain(|metadata| {
                !(metadata.provider_id == "anthropic"
                    && (metadata.model_id == *entry || metadata.model_id.ends_with(&route_suffix)))
            });
            changed |= settings.model_catalog.len() != previous_len;

            let previous_len = settings.catalog.selected_models.len();
            settings
                .catalog
                .selected_models
                .retain(|route| !route.ends_with(&route_suffix));
            changed |= settings.catalog.selected_models.len() != previous_len;

            let previous_len = settings.catalog.model_picker_order.len();
            settings
                .catalog
                .model_picker_order
                .retain(|route| !route.ends_with(&route_suffix));
            changed |= settings.catalog.model_picker_order.len() != previous_len;

            for route in &mut settings.routes {
                for target in &mut route.targets {
                    // Route targets are `provider/model` route names.
                    if target.model.ends_with(&route_suffix) {
                        let replacement = settings
                            .providers
                            .iter()
                            .find(|provider| provider.id == "anthropic")
                            .and_then(|provider| provider.default_model.clone())
                            .or_else(|| provider_default_model("anthropic"));
                        let Some(replacement) = replacement else {
                            continue;
                        };
                        target.model = format!("anthropic/{replacement}");
                        changed = true;
                    }
                }
            }
        }
    }
    if settings.registry_revision < REGISTRY_REVISION {
        settings.registry_revision = REGISTRY_REVISION;
        changed = true;
    }
    changed
}

/// `provider/model` targets that a saved sidecar sends images to, directly or through a
/// route. Validation rejects a text-only row on any of them, and settings that fail
/// validation cannot be loaded, so the model's real limit shows up as a sidecar error.
fn models_sidecars_send_images_to(settings: &GatewaySettings) -> HashSet<String> {
    [
        settings.sidecars.vision_model.as_deref(),
        settings.sidecars.video_input_model.as_deref(),
        settings.sidecars.document_model.as_deref(),
    ]
    .into_iter()
    .flatten()
    .flat_map(|target| {
        settings
            .routes
            .iter()
            .find(|route| {
                route.enabled && (route.id == target || route.alias.as_deref() == Some(target))
            })
            .map_or_else(
                || vec![target.to_string()],
                |route| {
                    route
                        .targets
                        .iter()
                        .map(|item| item.model.clone())
                        .collect()
                },
            )
    })
    .collect()
}

fn provider_default_model(provider_id: &str) -> Option<String> {
    provider_presets()
        .into_iter()
        .find(|preset| preset.id == provider_id)
        .and_then(|preset| preset.instantiate(None).ok())
        .and_then(|provider| provider.default_model)
}

fn enable_capability(current: &mut bool, desired: bool) -> bool {
    if desired && !*current {
        *current = true;
        true
    } else {
        false
    }
}

fn disable_capability(current: &mut bool) -> bool {
    if *current {
        *current = false;
        true
    } else {
        false
    }
}

fn set_capability(current: &mut bool, desired: bool) -> bool {
    if *current == desired {
        false
    } else {
        *current = desired;
        true
    }
}

fn model_matches_registry(model: &str, configured: &[String]) -> bool {
    let model_folded = model.to_ascii_lowercase();
    configured.iter().any(|candidate| {
        let candidate_folded = candidate.to_ascii_lowercase();
        candidate_folded == "*"
            || model_folded == candidate_folded
            || model_folded
                .strip_prefix(&candidate_folded)
                .is_some_and(|suffix| suffix.starts_with(':'))
            || candidate_folded
                .strip_prefix(&model_folded)
                .is_some_and(|suffix| suffix.starts_with(':'))
    })
}

pub(super) fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

/// Exact window, or the window of the closest older sibling. `grok-4.7` therefore
/// inherits `grok-4.6` (500k) instead of falling back to the catalog's default,
/// and `deepseek-v4.1-flash` inherits `deepseek-v4-flash`.
pub(crate) fn resolve_model_context_window(
    windows: &BTreeMap<String, u64>,
    model_id: &str,
) -> Option<u64> {
    if let Some(window) = windows.get(model_id) {
        return Some(*window);
    }
    let sibling = closest_older_sibling(windows.keys().map(String::as_str), model_id)?;
    windows.get(sibling).copied()
}

/// Validation rejects a max input or output above the window, and settings that
/// fail validation cannot be loaded at all, so a window filled in automatically must
/// not contradict a limit the user saved. The catalog's default covers the gap.
pub(super) fn window_fits_saved_limits(
    provider: &ProviderDefinition,
    model: &str,
    window: u64,
) -> bool {
    [
        &provider.model_max_input_tokens,
        &provider.model_max_output_tokens,
    ]
    .iter()
    .all(|limits| limits.get(model).is_none_or(|limit| *limit <= window))
}

/// The reverse of `window_fits_saved_limits`, for a limit filled in under a window
/// the user saved.
fn limit_fits_saved_window(provider: &ProviderDefinition, model: &str, limit: u64) -> bool {
    provider
        .model_context_windows
        .get(model)
        .is_none_or(|window| limit <= *window)
}

/// Validation requires a default effort to be in the model's own list, so an effort
/// list filled in automatically must contain the default the user saved.
fn efforts_fit_saved_default(
    provider: &ProviderDefinition,
    model: &str,
    efforts: &[String],
) -> bool {
    provider
        .model_default_reasoning_efforts
        .get(model)
        .is_none_or(|default| efforts.is_empty() || efforts.contains(default))
}

/// The reverse of `efforts_fit_saved_default`, for a default filled in under a list
/// the user saved.
fn default_effort_fits_saved_efforts(
    provider: &ProviderDefinition,
    model: &str,
    default: &str,
) -> bool {
    provider
        .model_reasoning_efforts
        .get(model)
        .is_none_or(|efforts| efforts.is_empty() || efforts.iter().any(|effort| effort == default))
}
