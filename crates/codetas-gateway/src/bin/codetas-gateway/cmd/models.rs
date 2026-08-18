use super::*;
use codetas_gateway::build_codex_catalog;
use codetas_gateway::discover_provider_models;
use codetas_gateway::ModelMetadata;

use std::path::Path;
pub(crate) async fn models(arguments: &[String], config: &Path) -> Result<(), String> {
    let mut args = arguments.to_vec();
    if help_requested(&args) {
        println!("{MODELS_USAGE}");
        return Ok(());
    }
    let action = positional(&mut args).unwrap_or_else(|| "list".into());
    let json_output = take_flag(&mut args, "--json")?;
    match action.as_str() {
        "list" => {
            let provider = positional(&mut args);
            finish_args(&args, MODELS_USAGE)?;
            let settings = read_valid_settings(config)?;
            let items = model_rows(&settings, provider.as_deref());
            if json_output {
                print_json(&items)
            } else {
                for item in items {
                    println!(
                        "{}  {}",
                        item["id"].as_str().unwrap_or_default(),
                        if item["enabled"].as_bool().unwrap_or(false) {
                            "enabled"
                        } else {
                            "disabled"
                        }
                    );
                }
                Ok(())
            }
        }
        "catalog" => {
            finish_args(&args, MODELS_USAGE)?;
            let settings = read_valid_settings(config)?;
            let catalog = build_codex_catalog(&settings);
            if json_output {
                print_json(&catalog)
            } else {
                for item in catalog.models {
                    println!(
                        "{}",
                        item.get("slug")
                            .and_then(|value| value.as_str())
                            .unwrap_or_default()
                    );
                }
                Ok(())
            }
        }
        "add" => {
            reject_json_for_mutation(json_output)?;
            let provider = required_positional(&mut args, "provider id", MODELS_USAGE)?;
            let model = required_positional(&mut args, "model id", MODELS_USAGE)?;
            let display_name = take_option(&mut args, "--display-name")?;
            let context_window = optional_u64(&mut args, "--context-window")?;
            let max_output_tokens = optional_u64(&mut args, "--max-output-tokens")?;
            let modalities = take_option(&mut args, "--modalities")?.map(|value| csv(&value));
            finish_args(&args, MODELS_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            let provider_item = provider_ref(&settings, &provider)?.clone();
            let model_wire_id = provider_item.wire_model_id(&model);
            let mut capabilities = provider_item.capabilities.clone();
            capabilities.image_generation = provider_item.capabilities.image_generation
                && provider_item
                    .image_generation_models
                    .iter()
                    .any(|configured| provider_item.wire_model_id(configured) == model_wire_id);
            if settings
                .model_catalog
                .iter()
                .any(|item| item.provider_id == provider && item.model_id == model)
            {
                return Err(format!("model metadata already exists: {provider}/{model}"));
            }
            let provider_item = provider_mut(&mut settings, &provider)?;
            if !provider_item.models.contains(&model) {
                provider_item.models.push(model.clone());
            }
            settings.model_catalog.push(ModelMetadata {
                provider_id: provider.clone(),
                model_id: model.clone(),
                display_name,
                enabled: true,
                context_window,
                max_input_tokens: None,
                max_output_tokens,
                input_modalities: modalities.unwrap_or_else(|| vec!["text".into()]),
                reasoning_efforts: Vec::new(),
                default_reasoning_effort: None,
                capabilities,
                input_price_per_million: None,
                output_price_per_million: None,
                instructions_template: None,
            });
            save_settings(config, &settings)?;
            println!("Added model {provider}/{model}.");
            Ok(())
        }
        "edit" => {
            reject_json_for_mutation(json_output)?;
            let provider = required_positional(&mut args, "provider id", MODELS_USAGE)?;
            let model = required_positional(&mut args, "model id", MODELS_USAGE)?;
            let display_name = take_option(&mut args, "--display-name")?;
            let context = take_option(&mut args, "--context-window")?
                .map(|value| parse_clearable_u64(&value, "--context-window"))
                .transpose()?;
            let output = take_option(&mut args, "--max-output-tokens")?
                .map(|value| parse_clearable_u64(&value, "--max-output-tokens"))
                .transpose()?;
            let modalities = take_option(&mut args, "--modalities")?;
            finish_args(&args, MODELS_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            let item = model_mut(&mut settings, &provider, &model)?;
            if let Some(name) = display_name {
                item.display_name = clearable(name);
            }
            if let Some(value) = context {
                item.context_window = value;
            }
            if let Some(value) = output {
                item.max_output_tokens = value;
            }
            if let Some(value) = modalities {
                item.input_modalities = if value == "-" {
                    Vec::new()
                } else {
                    csv(&value)
                };
            }
            save_settings(config, &settings)?;
            println!("Updated model {provider}/{model}.");
            Ok(())
        }
        "enable" | "disable" => {
            reject_json_for_mutation(json_output)?;
            let provider = required_positional(&mut args, "provider id", MODELS_USAGE)?;
            let model = required_positional(&mut args, "model id", MODELS_USAGE)?;
            finish_args(&args, MODELS_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            model_mut(&mut settings, &provider, &model)?.enabled = action == "enable";
            save_settings(config, &settings)?;
            println!(
                "{} model {provider}/{model}.",
                if action == "enable" {
                    "Enabled"
                } else {
                    "Disabled"
                }
            );
            Ok(())
        }
        "remove" => {
            reject_json_for_mutation(json_output)?;
            let provider = required_positional(&mut args, "provider id", MODELS_USAGE)?;
            let model = required_positional(&mut args, "model id", MODELS_USAGE)?;
            finish_args(&args, MODELS_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            let selected = provider_ref(&settings, &provider)?.models.contains(&model);
            let original = settings.model_catalog.len();
            settings
                .model_catalog
                .retain(|item| !(item.provider_id == provider && item.model_id == model));
            if let Ok(item) = provider_mut(&mut settings, &provider) {
                item.models.retain(|value| value != &model);
                if item.default_model.as_deref() == Some(&model) {
                    item.default_model = None;
                }
            }
            if settings.model_catalog.len() == original && !selected {
                return Err(format!("unknown model metadata: {provider}/{model}"));
            }
            save_settings(config, &settings)?;
            println!("Removed model {provider}/{model}.");
            Ok(())
        }
        "selected" => {
            reject_json_for_mutation(json_output)?;
            let provider = required_positional(&mut args, "provider id", MODELS_USAGE)?;
            let selected = take_option(&mut args, "--set")?;
            let clear = take_flag(&mut args, "--clear")?;
            if selected.is_some() == clear {
                return Err("selected requires exactly one of --set or --clear".into());
            }
            finish_args(&args, MODELS_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            provider_mut(&mut settings, &provider)?.models =
                selected.map(|value| csv(&value)).unwrap_or_default();
            save_settings(config, &settings)?;
            println!("Updated selected models for {provider}.");
            Ok(())
        }
        "publish" => {
            let selected = take_option(&mut args, "--set")?;
            let clear = take_flag(&mut args, "--clear")?;
            let order = take_option(&mut args, "--order")?;
            let clear_order = take_flag(&mut args, "--clear-order")?;
            if selected.is_some() && clear {
                return Err("publish accepts only one of --set or --clear".into());
            }
            if order.is_some() && clear_order {
                return Err("publish accepts only one of --order or --clear-order".into());
            }
            finish_args(&args, MODELS_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            let mut changed = false;
            if let Some(value) = selected {
                settings.catalog.selected_models = csv(&value);
                changed = true;
            } else if clear {
                settings.catalog.selected_models.clear();
                changed = true;
            }
            if let Some(value) = order {
                settings.catalog.model_picker_order = csv(&value);
                changed = true;
            } else if clear_order {
                settings.catalog.model_picker_order.clear();
                changed = true;
            }
            if changed {
                reject_json_for_mutation(json_output)?;
                save_settings(config, &settings)?;
                println!("Updated the public model allowlist and picker order.");
                Ok(())
            } else if json_output {
                print_json(&settings.catalog)
            } else {
                println!("selectedModels={}", settings.catalog.selected_models.join(","));
                println!("modelPickerOrder={}", settings.catalog.model_picker_order.join(","));
                Ok(())
            }
        }
        "sync" => {
            let target = required_positional(&mut args, "provider id or all", MODELS_USAGE)?;
            finish_args(&args, MODELS_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            let providers = settings
                .providers
                .iter()
                .filter(|item| target == "all" || item.id == target)
                .cloned()
                .collect::<Vec<_>>();
            if providers.is_empty() {
                return Err(format!("unknown provider: {target}"));
            }
            let mut result = Vec::new();
            for mut provider in providers {
                provider.discovery.enabled = true;
                let discovered = discover_provider_models(&provider)
                    .await
                    .map_err(|error| format!("{}: {error}", provider.id))?;
                provider_mut(&mut settings, &provider.id)?.models = discovered
                    .iter()
                    .map(|item| item.model_id.clone())
                    .collect();
                settings
                    .model_catalog
                    .retain(|item| item.provider_id != provider.id);
                settings.model_catalog.extend(discovered.clone());
                result.extend(discovered);
            }
            save_settings(config, &settings)?;
            if json_output {
                print_json(&result)
            } else {
                println!("Synchronized {} models.", result.len());
                Ok(())
            }
        }
        _ => Err(format!("unknown models action: {action}\n{MODELS_USAGE}")),
    }
}
