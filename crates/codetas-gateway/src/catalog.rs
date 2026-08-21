use crate::config::{
    effective_model_capabilities, model_has_image_generation_identity, AgentSurfaceMode,
    CatalogDisplayNameFormat, GatewaySettings, ModelMetadata, ProviderCapabilities,
    ProviderTransport, RouteDefinition, RouteTarget,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Overrides Codex Desktop's default skill trigger.
///
/// Desktop still injects a skill catalog and "read SKILL.md before acting /
/// do not carry skills across turns". Those three rules re-load the same
/// skill on every hop. CODETAS used to set
/// `include_skills_usage_instructions: false`, which hid skills from grok
/// and other non-template models without stopping Desktop's injection on
/// native Codex models. Keep skills available and replace the trigger.
const SKILL_LOOP_GUARD: &str = "\n\n# Skill and investigation contract\n\
This section overrides any earlier skill-trigger, Using skills, or \
investigation instructions.\n\
\n\
Skills are available and should be used when they genuinely help. \
Read a matching SKILL.md at most once per turn, then do the work. \
Do not re-read the same SKILL.md after a tool result, compaction, \
or a short follow-up. Do not re-announce a skill after the first \
announcement in the same turn. \"Do not carry skills across turns\" \
means drop an irrelevant skill; it does not mean reload SKILL.md \
from scratch on every hop.\n\
\n\
Do not call create_thread, fork_thread, or handoff_thread unless the \
user explicitly asks to create a separate Codex task or names that tool.\n\
\n\
After the files named in the user request have been read once, implement \
or answer. Re-running git status or re-reading the same file is not \
progress. Research is not a valid terminal state.\n";

fn base_instructions(
    slug: &str,
    context_window: u64,
    efforts: &[String],
    default_effort: Option<&str>,
) -> String {
    let effort_list = if efforts.is_empty() {
        "none configured".to_string()
    } else {
        efforts.join(", ")
    };
    let default = default_effort.unwrap_or("not set");
    format!(
        "You are a coding agent operating in the user's workspace. Follow the user's \
         instructions and applicable repository guidance. Use the available tools when needed, \
         preserve unrelated changes, and communicate completed work and blockers clearly. \
         Your model identifier is \"{slug}\" (assigned by the calling configuration), with a \
         context window of {context_window} tokens, supported reasoning efforts \
         ({effort_list}), and default reasoning effort \"{default}\". If the user asks what \
         model you are or what settings apply, reply with these exact values, and add that \
         your internal knowledge of your version may be outdated.{SKILL_LOOP_GUARD}"
    )
}

fn with_skill_loop_guard(template: &str) -> String {
    if template.contains("Skill and investigation contract") {
        template.to_string()
    } else {
        format!("{template}{SKILL_LOOP_GUARD}")
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CodexCatalog {
    pub models: Vec<Value>,
}

impl CodexCatalog {
    pub fn validate_for_codex(&self, selected_model: Option<&str>) -> Result<(), String> {
        if self.models.is_empty() {
            return Err("Codexモデルカタログに有効なモデルがありません".into());
        }
        let mut slugs = BTreeSet::new();
        for model in &self.models {
            let slug = model
                .get("slug")
                .and_then(Value::as_str)
                .filter(|slug| !slug.trim().is_empty() && !slug.chars().any(char::is_control))
                .ok_or("Codexモデルカタログに不正なモデルIDがあります")?;
            if !slugs.insert(slug) {
                return Err(format!(
                    "Codexモデルカタログに重複したモデルがあります: {slug}"
                ));
            }
            let has_base_instructions = model
                .get("base_instructions")
                .and_then(Value::as_str)
                .is_some_and(|instructions| !instructions.trim().is_empty());
            if let Some(instructions) = model.get("base_instructions").and_then(Value::as_str) {
                let expected_identity = format!("Your model identifier is \"{slug}\"");
                if !instructions.contains(&expected_identity) {
                    return Err(format!(
                        "Codexモデル {slug} のbase_instructionsが別のモデルIDを示しています"
                    ));
                }
            }
            let has_instructions_template = model
                .pointer("/model_messages/instructions_template")
                .and_then(Value::as_str)
                .is_some_and(|instructions| !instructions.trim().is_empty());
            if !has_base_instructions && !has_instructions_template {
                return Err(format!(
                    "Codexモデル {slug} にbase_instructionsまたはinstructions_templateがありません"
                ));
            }
        }
        if let Some(selected_model) = selected_model.filter(|model| !model.trim().is_empty()) {
            if !slugs.contains(selected_model) {
                return Err(format!(
                    "選択したCodexモデルが生成カタログにありません: {selected_model}"
                ));
            }
        }
        Ok(())
    }
}

pub fn build_codex_catalog(settings: &GatewaySettings) -> CodexCatalog {
    let metadata = settings
        .model_catalog
        .iter()
        .map(|model| (format!("{}/{}", model.provider_id, model.model_id), model))
        .collect::<BTreeMap<_, _>>();
    let mut models = BTreeMap::<String, Value>::new();
    let mut native_openai_slugs = BTreeSet::<String>::new();

    // Catalog lists every configured provider's models (enabled or
    // not) so the Codex model picker matches the reference catalog. Routing still
    // resolves only enabled providers at request time.
    for provider in settings.providers.iter() {
        let mut ids = provider
            .models
            .iter()
            .filter(|model| !model_has_image_generation_identity(settings, provider, model))
            .cloned()
            .collect::<Vec<_>>();
        if let Some(default_model) = provider.default_model.as_deref() {
            if !model_has_image_generation_identity(settings, provider, default_model)
                && !ids.iter().any(|model| model == default_model)
            {
                ids.insert(0, default_model.to_string());
            }
        }
        for model in settings
            .model_catalog
            .iter()
            .filter(|model| {
                model.enabled
                    && model.provider_id == provider.id
                    && !model_has_image_generation_identity(settings, provider, &model.model_id)
            })
        {
            if !ids.iter().any(|id| id == &model.model_id) {
                ids.push(model.model_id.clone());
            }
        }
        for model_id in ids {
            let target = format!("{}/{}", provider.id, model_id);
            let slug = codex_model_slug(&provider.id, &model_id);
            let details = metadata.get(&target).copied();
            if details.is_some_and(|model| !model.enabled) {
                continue;
            }
            let provider_modalities = provider
                .model_input_modalities
                .get(&model_id)
                .map(Vec::as_slice);
            let provider_efforts = if model_in_list(&provider.no_reasoning_models, &model_id) {
                Some(&[][..])
            } else {
                provider
                    .model_reasoning_efforts
                    .get(&model_id)
                    .map(Vec::as_slice)
            };
            let display_name = catalog_display_name(
                &settings.catalog.display_name_format,
                &provider.id,
                &provider.name,
                provider.display_prefix.as_deref(),
                &model_id,
                details.and_then(|model| model.display_name.as_deref()),
            );
            let capabilities = details
                .map(|model| &model.capabilities)
                .unwrap_or(&provider.capabilities);
            let allow_app_plugin_tools = if provider.id == "openai" {
                native_openai_supports_fast(&slug)
            } else {
                capabilities.tools && provider.transport != ProviderTransport::Kiro
            };
            models.insert(
                slug.clone(),
                catalog_model(
                    &slug,
                    &display_name,
                    &format!("Routed by CODETAS through {}.", provider.name),
                    details
                        .and_then(|model| model.context_window)
                        .or_else(|| {
                            crate::registry::resolve_model_context_window(
                                &provider.model_context_windows,
                                &model_id,
                            )
                        }),
                    details
                        .and_then(|model| model.max_input_tokens)
                        .or_else(|| provider.model_max_input_tokens.get(&model_id).copied()),
                    details
                        .and_then(|model| model.max_output_tokens)
                        .or_else(|| provider.model_max_output_tokens.get(&model_id).copied()),
                    // Prefer the provider's explicit input_modalities; ignore stale
                    // model_catalog entries (discovered when the provider had
                    // capabilities.vision=false).
                    provider_modalities,
                    details
                        .filter(|model| !model.reasoning_efforts.is_empty())
                        .map(|model| model.reasoning_efforts.as_slice())
                        .or(provider_efforts),
                    details
                        .and_then(|model| model.default_reasoning_effort.as_deref())
                        .or_else(|| {
                            provider
                                .model_default_reasoning_efforts
                                .get(&model_id)
                                .map(String::as_str)
                        }),
                    capabilities,
                    allow_app_plugin_tools,
                    multi_agent_version(settings, &slug),
                    models.len() + 1,
                    details.and_then(|model| model.instructions_template.as_deref()),
                    provider_advertises_image_detail_original(&provider.id),
                ),
            );
            if provider.id == "openai" {
                native_openai_slugs.insert(slug);
            }
        }
    }

    for route in settings
        .routes
        .iter()
        .filter(|route| route.enabled && route_has_normal_target(settings, route))
    {
        let route_model = catalog_route(settings, route, models.len() + 1);
        let slug = route.alias.clone().unwrap_or_else(|| route.id.clone());
        native_openai_slugs.remove(&slug);
        models.insert(slug, route_model);
    }
    for (slug, display_name, description, target) in [
        (
            "codetas-sidecar/web-search",
            "CODETAS Web Search",
            "Capability-routed web search sidecar.",
            settings.sidecars.web_search_model.as_deref(),
        ),
        (
            "codetas-sidecar/vision",
            "CODETAS Vision",
            "Capability-routed vision sidecar.",
            settings.sidecars.vision_model.as_deref(),
        ),
        (
            "codetas-sidecar/video-analysis",
            "CODETAS Video Analysis",
            "Capability-routed sampled-video analysis sidecar.",
            settings
                .sidecars
                .video_input_model
                .as_deref()
                .or(settings.sidecars.vision_model.as_deref()),
        ),
        (
            "codetas-sidecar/document",
            "CODETAS Document",
            "Capability-routed PDF and OCR analysis sidecar.",
            settings
                .sidecars
                .document_model
                .as_deref()
                .or(settings.sidecars.vision_model.as_deref()),
        ),
        (
            "codetas-sidecar/video",
            "CODETAS Video",
            "Capability-routed video generation sidecar.",
            settings.sidecars.video_model.as_deref(),
        ),
        (
            "codetas-sidecar/live",
            "CODETAS Realtime",
            "Capability-routed realtime and live voice sidecar.",
            settings.sidecars.live_model.as_deref(),
        ),
    ] {
        let Some(target) = target else { continue };
        let (context, modalities, capabilities) = target_profile(settings, target);
        models.insert(
            slug.into(),
            catalog_model(
                slug,
                display_name,
                description,
                context,
                None,
                None,
                Some(&modalities),
                None,
                None,
                &capabilities,
                false,
                multi_agent_version(settings, slug),
                models.len() + 1,
                None,
                false,
            ),
        );
    }

    if !settings.catalog.selected_models.is_empty() {
        models.retain(|slug, _| {
            settings.catalog.selected_models.iter().any(|selected| {
                selected == slug
                    || (native_openai_slugs.contains(slug)
                        && selected.strip_prefix("openai/") == Some(slug.as_str()))
            })
        });
    }
    let mut values = models.into_values().collect::<Vec<_>>();
    values.sort_by(|left, right| {
        let left_slug = left.get("slug").and_then(Value::as_str).unwrap_or_default();
        let right_slug = right.get("slug").and_then(Value::as_str).unwrap_or_default();
        catalog_picker_position(settings, &native_openai_slugs, left_slug)
            .cmp(&catalog_picker_position(
                settings,
                &native_openai_slugs,
                right_slug,
            ))
            .then_with(|| left_slug.cmp(right_slug))
    });
    for (index, value) in values.iter_mut().enumerate() {
        if let Some(object) = value.as_object_mut() {
            object.insert("priority".into(), json!(index + 1));
        }
    }
    CodexCatalog { models: values }
}

fn catalog_picker_position(
    settings: &GatewaySettings,
    native_openai_slugs: &BTreeSet<String>,
    published_slug: &str,
) -> usize {
    settings
        .catalog
        .model_picker_order
        .iter()
        .position(|configured| {
            configured == published_slug
                || (native_openai_slugs.contains(published_slug)
                    && configured.strip_prefix("openai/") == Some(published_slug))
        })
        .unwrap_or(usize::MAX)
}

pub(crate) fn public_model_id_matches(
    settings: &GatewaySettings,
    configured: &str,
    published: &str,
) -> bool {
    if configured == published {
        return true;
    }
    let bare = if let Some(bare) = published.strip_prefix("openai/") {
        if configured != bare {
            return false;
        }
        bare
    } else if let Some(bare) = configured.strip_prefix("openai/") {
        if published != bare
            || settings.routes.iter().any(|route| {
                route.enabled
                    && route.alias.as_deref().unwrap_or(route.id.as_str()) == published
            })
        {
            return false;
        }
        bare
    } else {
        return false;
    };
    settings.providers.iter().any(|provider| {
        provider.id == "openai"
            && (provider.default_model.as_deref() == Some(bare)
                || provider.models.iter().any(|model| model == bare))
    }) || settings.model_catalog.iter().any(|model| {
        model.enabled && model.provider_id == "openai" && model.model_id == bare
    })
}

fn codex_model_slug(provider_id: &str, model_id: &str) -> String {
    // Codex's built-in OpenAI provider must keep the native provider id so old
    // desktop sessions remain visible and resumable. The gateway is selected
    // with the loopback openai_base_url override, therefore native OpenAI model names stay
    // unqualified while routed models keep provider/model slugs.
    if provider_id == "openai" {
        model_id.to_string()
    } else {
        format!("{provider_id}/{model_id}")
    }
}

fn target_profile(
    settings: &GatewaySettings,
    target: &str,
) -> (Option<u64>, Vec<String>, ProviderCapabilities) {
    if let Some(route) = settings.routes.iter().find(|route| {
        route.enabled && (route.id == target || route.alias.as_deref() == Some(target))
    }) {
        let models = route
            .targets
            .iter()
            .filter_map(|route_target| {
                settings.model_catalog.iter().find(|model| {
                    format!("{}/{}", model.provider_id, model.model_id) == route_target.model
                })
            })
            .collect::<Vec<_>>();
        return (
            models.iter().filter_map(|model| model.context_window).min(),
            common_modalities(&models),
            common_capabilities(settings, route),
        );
    }
    let Some((provider_id, model_id)) = target.split_once('/') else {
        return (None, vec!["text".into()], ProviderCapabilities::default());
    };
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.enabled && provider.id == provider_id);
    let metadata = settings.model_catalog.iter().find(|model| {
        model.enabled && model.provider_id == provider_id && model.model_id == model_id
    });
    let capabilities = provider
        .map(|provider| effective_model_capabilities(provider, metadata, model_id))
        .or_else(|| metadata.map(|model| model.capabilities.clone()))
        .unwrap_or_default();
    let modalities = metadata
        .filter(|model| !model.input_modalities.is_empty())
        .map(|model| model.input_modalities.clone())
        .or_else(|| {
            provider.and_then(|provider| provider.model_input_modalities.get(model_id).cloned())
        })
        .unwrap_or_else(|| {
            let mut values = vec!["text".into()];
            if capabilities.vision {
                values.push("image".into());
            }
            values
        });
    (
        metadata.and_then(|model| model.context_window).or_else(|| {
            provider.and_then(|provider| {
                crate::registry::resolve_model_context_window(
                    &provider.model_context_windows,
                    model_id,
                )
            })
        }),
        modalities,
        capabilities,
    )
}

#[allow(clippy::too_many_arguments)]
fn catalog_model(
    slug: &str,
    display_name: &str,
    description: &str,
    context_window: Option<u64>,
    max_input_tokens: Option<u64>,
    max_output_tokens: Option<u64>,
    input_modalities: Option<&[String]>,
    reasoning_efforts: Option<&[String]>,
    default_reasoning_effort: Option<&str>,
    capabilities: &ProviderCapabilities,
    allow_app_plugin_tools: bool,
    multi_agent_version: &str,
    priority: usize,
    instructions_template: Option<&str>,
    native_openai: bool,
) -> Value {
    let context_window = context_window.unwrap_or(128_000).max(8_192);
    let efforts = reasoning_efforts
        .map(|values| values.to_vec())
        .unwrap_or_else(|| {
            if capabilities.reasoning {
                vec!["low".into(), "medium".into(), "high".into()]
            } else {
                Vec::new()
            }
        });
    let default_effort = default_reasoning_effort
        .filter(|value| efforts.iter().any(|effort| effort == value))
        .or_else(|| efforts.first().map(String::as_str));
    let levels = efforts
        .iter()
        .map(|effort| {
            json!({
                "effort": effort,
                "description": reasoning_description(effort)
            })
        })
        .collect::<Vec<_>>();
    let modalities = input_modalities
        .filter(|values| !values.is_empty())
        .map(|values| values.to_vec())
        .unwrap_or_else(|| {
            let mut values = vec!["text".into()];
            if capabilities.vision {
                values.push("image".into());
            }
            values
        });
    let auto_compact_token_limit = catalog_auto_compact_token_limit(
        context_window,
        max_input_tokens,
        max_output_tokens,
    );
    let generated_base_instructions =
        base_instructions(slug, context_window, &efforts, default_effort);
    let instructions_template =
        instructions_template.map(with_skill_loop_guard);
    let compatibility_hash = catalog_compatibility_hash(
        slug,
        context_window,
        auto_compact_token_limit,
        &modalities,
        &efforts,
        capabilities,
        multi_agent_version,
        display_name,
        description,
        &generated_base_instructions,
        instructions_template.as_deref(),
    );
    let mut entry = serde_json::Map::from_iter([
        ("slug".into(), json!(slug)),
        ("display_name".into(), json!(display_name)),
        ("description".into(), json!(description)),
        ("base_instructions".into(), json!(generated_base_instructions)),
        ("supported_reasoning_levels".into(), json!(levels)),
        ("shell_type".into(), json!("shell_command")),
        ("visibility".into(), json!("list")),
        ("supported_in_api".into(), json!(true)),
        ("priority".into(), json!(priority)),
        ("include_skills_usage_instructions".into(), json!(true)),
        (
            "include_apps_usage_instructions".into(),
            json!(allow_app_plugin_tools),
        ),
        (
            "include_plugin_usage_instructions".into(),
            json!(allow_app_plugin_tools),
        ),
        (
            "supports_reasoning_summaries".into(),
            json!(capabilities.reasoning),
        ),
        ("default_reasoning_summary".into(), json!("none")),
        ("support_verbosity".into(), json!(true)),
        (
            "default_verbosity".into(),
            json!(if native_openai_supports_fast(slug) {
                "low"
            } else {
                "medium"
            }),
        ),
        ("apply_patch_tool_type".into(), json!("freeform")),
        (
            "truncation_policy".into(),
            json!({"mode": "tokens", "limit": 10_000}),
        ),
        (
            "supports_parallel_tool_calls".into(),
            json!(capabilities.parallel_tools),
        ),
        (
            "supports_image_detail_original".into(),
            json!(capabilities.vision && native_openai),
        ),
        ("context_window".into(), json!(context_window)),
        ("max_context_window".into(), json!(context_window)),
        ("comp_hash".into(), json!(compatibility_hash)),
        (
            "effective_context_window_percent".into(),
            json!(if native_openai_supports_fast(slug) {
                95
            } else {
                90
            }),
        ),
        ("experimental_supported_tools".into(), json!([])),
        ("input_modalities".into(), json!(modalities)),
        (
            "supports_search_tool".into(),
            json!(capabilities.web_search),
        ),
        (
            "web_search_tool_type".into(),
            json!(if capabilities.web_search {
                "text_and_image"
            } else {
                "text"
            }),
        ),
        ("multi_agent_version".into(), json!(multi_agent_version)),
        // Responses-lite restructures tool delivery into `additional_tools`
        // namespaces (functions/collaboration) that omit the base tools the
        // ChatGPT backend expects (update_plan, exec_command, ...). Keep the
        // conventional top-level `tools` delivery so those tools reach the
        // model.
        ("use_responses_lite".into(), json!(false)),
        (
            "tool_mode".into(),
            json!(if native_openai_supports_fast(slug) {
                "code_mode_only"
            } else {
                "default"
            }),
        ),
        (
            "auto_compact_token_limit".into(),
            json!(auto_compact_token_limit),
        ),
    ]);
    if let Some(default_effort) = default_effort {
        entry.insert("default_reasoning_level".into(), json!(default_effort));
    }
    if let Some(template) = instructions_template.as_deref() {
        entry.insert(
            "model_messages".into(),
            json!({
                "instructions_template": template,
                "instructions_variables": {},
                "approvals": {}
            }),
        );
    }
    if native_openai_supports_fast(slug) {
        entry.insert("additional_speed_tiers".into(), json!(["fast"]));
        entry.insert(
            "service_tiers".into(),
            json!([{
                "id": "priority",
                "name": "Fast",
                "description": "1.5x speed, increased usage"
            }]),
        );
    }
    Value::Object(entry)
}

/// Derive automatic compaction from the model's explicitly configured usable
/// input budget rather than inferring token contracts from a model name.
fn catalog_auto_compact_token_limit(
    context_window: u64,
    max_input_tokens: Option<u64>,
    max_output_tokens: Option<u64>,
) -> u64 {
    const AUTO_COMPACT_PERCENT: u64 = 90;
    let context_input_budget = context_window.saturating_sub(max_output_tokens.unwrap_or(0));
    let input_budget = max_input_tokens
        .map(|limit| limit.min(context_input_budget))
        .unwrap_or(context_input_budget);
    input_budget
        .saturating_mul(AUTO_COMPACT_PERCENT)
        .checked_div(100)
        .unwrap_or(0)
        .max(1)
}

fn catalog_display_name(
    format: &CatalogDisplayNameFormat,
    provider_id: &str,
    provider_name: &str,
    display_prefix: Option<&str>,
    model_id: &str,
    custom_name: Option<&str>,
) -> String {
    let custom_name = custom_name
        .map(str::trim)
        .filter(|name| !name.is_empty());
    let prefix = display_prefix
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(custom_name) = custom_name {
        if prefix.is_none() && *format != CatalogDisplayNameFormat::ProviderModel {
            return custom_name.to_string();
        }
    }
    let provider_label = prefix.unwrap_or(provider_name);
    let model_label = custom_name.unwrap_or(model_id);
    match format {
        CatalogDisplayNameFormat::Custom => join_display_prefix(prefix.unwrap_or(""), model_label),
        CatalogDisplayNameFormat::ModelId => model_label.to_string(),
        CatalogDisplayNameFormat::ProviderModel => join_display_prefix(provider_label, model_label),
        CatalogDisplayNameFormat::ProviderIdModel => custom_name
            .map(str::to_string)
            .unwrap_or_else(|| format!("{provider_id}/{model_id}")),
        CatalogDisplayNameFormat::Default => {
            if provider_id == "openai" {
                return join_display_prefix(
                    prefix.unwrap_or(""),
                    custom_name.unwrap_or_else(|| native_openai_display_name(model_id).unwrap_or(model_id)),
                );
            }
            let trimmed = custom_name.unwrap_or_else(|| {
                model_id
                    .strip_prefix(&format!("{provider_id}-"))
                    .unwrap_or(model_id)
            });
            join_display_prefix(provider_label, trimmed)
        }
    }
}

fn join_display_prefix(prefix: &str, model_name: &str) -> String {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        model_name.to_string()
    } else {
        format!("{prefix} {model_name}")
    }
}

fn native_openai_display_name(slug: &str) -> Option<&'static str> {
    Some(match slug {
        "gpt-5.6-sol" => "GPT-5.6-Sol",
        "gpt-5.6-terra" => "GPT-5.6-Terra",
        "gpt-5.6-luna" => "GPT-5.6-Luna",
        "gpt-5.5" => "GPT-5.5",
        "gpt-5.4" => "GPT-5.4",
        "gpt-5.4-mini" => "GPT-5.4-Mini",
        "gpt-5.3-codex-spark" => "gpt-5.3-codex-spark",
        _ => return None,
    })
}

fn is_native_openai_catalog_slug(slug: &str) -> bool {
    !slug.contains('/')
}

fn provider_advertises_image_detail_original(provider_id: &str) -> bool {
    matches!(provider_id, "openai" | "openai-api" | "openai-apikey")
}

fn native_openai_supports_fast(slug: &str) -> bool {
    is_native_openai_catalog_slug(slug)
        && slug.starts_with("gpt-")
        && !slug.ends_with("-mini")
        && !slug.ends_with("-spark")
        && slug != "gpt-5.3-codex-spark"
}

fn model_in_list(configured: &[String], model: &str) -> bool {
    configured.iter().any(|candidate| {
        model == candidate
            || model
                .strip_prefix(candidate)
                .is_some_and(|suffix| suffix.starts_with(':'))
    })
}

fn catalog_compatibility_hash(
    slug: &str,
    context_window: u64,
    auto_compact_token_limit: u64,
    modalities: &[String],
    efforts: &[String],
    capabilities: &ProviderCapabilities,
    multi_agent_version: &str,
    display_name: &str,
    description: &str,
    base_instructions: &str,
    instructions_template: Option<&str>,
) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    let mut feed = |value: &[u8]| {
        for byte in value {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    };
    feed(b"codetas-catalog-v6");
    feed(slug.as_bytes());
    feed(display_name.as_bytes());
    feed(description.as_bytes());
    feed(base_instructions.as_bytes());
    if let Some(template) = instructions_template {
        feed(template.as_bytes());
    }
    feed(&context_window.to_le_bytes());
    feed(&auto_compact_token_limit.to_le_bytes());
    for value in modalities.iter().chain(efforts.iter()) {
        feed(value.as_bytes());
    }
    for value in [
        capabilities.streaming,
        capabilities.tools,
        capabilities.parallel_tools,
        capabilities.vision,
        capabilities.audio,
        capabilities.reasoning,
        capabilities.web_search,
        capabilities.image_generation,
        capabilities.video_generation,
        capabilities.realtime,
        capabilities.websockets,
        capabilities.stateful_responses,
        capabilities.structured_output,
        capabilities.service_tier,
        capabilities.custom_tools,
        capabilities.tool_search,
        capabilities.mcp_namespaces,
        capabilities.provider_metadata,
    ] {
        feed(&[u8::from(value)]);
    }
    feed(multi_agent_version.as_bytes());
    format!("codetas-{hash:016x}")
}

fn catalog_route(settings: &GatewaySettings, route: &RouteDefinition, priority: usize) -> Value {
    let target_models = route
        .targets
        .iter()
        .filter(|target| route_target_is_normal(settings, target))
        .filter_map(|target| {
            settings
                .model_catalog
                .iter()
                .find(|model| format!("{}/{}", model.provider_id, model.model_id) == target.model)
        })
        .collect::<Vec<&ModelMetadata>>();
    let target_limits = route
        .targets
        .iter()
        .filter(|target| route_target_is_normal(settings, target))
        .filter_map(|target| {
            let (provider_id, model_id) = target.model.split_once('/')?;
            let metadata = settings.model_catalog.iter().find(|model| {
                model.enabled
                    && model.provider_id == provider_id
                    && model.model_id == model_id
            });
            let provider = settings
                .providers
                .iter()
                .find(|provider| provider.enabled && provider.id == provider_id);
            let context = metadata
                .and_then(|model| model.context_window)
                .or_else(|| {
                    provider.and_then(|item| {
                        crate::registry::resolve_model_context_window(
                            &item.model_context_windows,
                            model_id,
                        )
                    })
                })
                .unwrap_or(128_000)
                .max(8_192);
            let max_input = metadata
                .and_then(|model| model.max_input_tokens)
                .or_else(|| {
                    provider.and_then(|item| item.model_max_input_tokens.get(model_id).copied())
                });
            let max_output = metadata
                .and_then(|model| model.max_output_tokens)
                .or_else(|| {
                    provider.and_then(|item| item.model_max_output_tokens.get(model_id).copied())
                });
            let context_input_budget = context.saturating_sub(max_output.unwrap_or(0));
            let usable_input_budget = max_input
                .map(|limit| limit.min(context_input_budget))
                .unwrap_or(context_input_budget);
            Some((context, usable_input_budget))
        })
        .collect::<Vec<_>>();
    let context = target_limits.iter().map(|limits| limits.0).min();
    // Compute each target's usable input budget before taking the route minimum.
    // Combining the smallest context with the largest output reserve from a
    // different target would invent an overly restrictive budget and compact
    // heterogeneous failover routes much earlier than necessary.
    let usable_input_budget = target_limits.iter().map(|limits| limits.1).min();
    let modalities = common_modalities(&target_models);
    let capabilities = common_capabilities(settings, route);
    let allow_app_plugin_tools = route_allows_app_plugin_tools(settings, route);
    let public_id = route.alias.as_deref().unwrap_or(&route.id);
    catalog_model(
        public_id,
        &route.name,
        route
            .description
            .as_deref()
            .filter(|description| !description.trim().is_empty())
            .unwrap_or("Virtual route managed by CODETAS."),
        context,
        usable_input_budget,
        // The route budget above already includes every target's output reserve.
        None,
        Some(&modalities),
        None,
        route.default_reasoning_effort.as_deref(),
        &capabilities,
        allow_app_plugin_tools,
        multi_agent_version(settings, public_id),
        priority,
        None,
        false,
    )
}

fn route_allows_app_plugin_tools(settings: &GatewaySettings, route: &RouteDefinition) -> bool {
    route_has_normal_target(settings, route)
        && route
            .targets
            .iter()
            .filter(|target| route_target_is_normal(settings, target))
            .all(|target| {
                let Some((provider_id, model_id)) = target.model.split_once('/') else {
                    return false;
                };
                let Some(provider) = settings
                    .providers
                    .iter()
                    .find(|provider| provider.id == provider_id)
                else {
                    return false;
                };
                if provider.transport == ProviderTransport::Kiro {
                    return false;
                }
                let metadata = settings.model_catalog.iter().find(|model| {
                    model.provider_id == provider_id && model.model_id == model_id
                });
                effective_model_capabilities(provider, metadata, model_id).tools
            })
}

fn common_modalities(models: &[&ModelMetadata]) -> Vec<String> {
    let Some(first) = models.first() else {
        return vec!["text".into()];
    };
    first
        .input_modalities
        .iter()
        .filter(|modality| {
            models
                .iter()
                .all(|model| model.input_modalities.contains(modality))
        })
        .cloned()
        .collect()
}

fn common_capabilities(
    settings: &GatewaySettings,
    route: &RouteDefinition,
) -> ProviderCapabilities {
    let mut capabilities: Option<ProviderCapabilities> = None;
    for target in route
        .targets
        .iter()
        .filter(|target| route_target_is_normal(settings, target))
    {
        let Some((provider_id, model_id)) = target.model.split_once('/') else {
            continue;
        };
        let Some(provider) = settings
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
        else {
            continue;
        };
        let metadata = settings
            .model_catalog
            .iter()
            .find(|model| model.provider_id == provider_id && model.model_id == model_id);
        let target_capabilities =
            effective_model_capabilities(provider, metadata, model_id);
        capabilities = Some(match capabilities {
            None => target_capabilities.clone(),
            Some(current) => ProviderCapabilities {
                streaming: current.streaming && target_capabilities.streaming,
                tools: current.tools && target_capabilities.tools,
                parallel_tools: current.parallel_tools && target_capabilities.parallel_tools,
                vision: current.vision && target_capabilities.vision,
                audio: current.audio && target_capabilities.audio,
                reasoning: current.reasoning && target_capabilities.reasoning,
                web_search: current.web_search && target_capabilities.web_search,
                image_generation: current.image_generation
                    && target_capabilities.image_generation,
                video_generation: current.video_generation
                    && target_capabilities.video_generation,
                realtime: current.realtime && target_capabilities.realtime,
                websockets: current.websockets && target_capabilities.websockets,
                stateful_responses: current.stateful_responses
                    && target_capabilities.stateful_responses,
                structured_output: current.structured_output
                    && target_capabilities.structured_output,
                service_tier: current.service_tier && target_capabilities.service_tier,
                custom_tools: current.custom_tools && target_capabilities.custom_tools,
                tool_search: current.tool_search && target_capabilities.tool_search,
                mcp_namespaces: current.mcp_namespaces && target_capabilities.mcp_namespaces,
                provider_metadata: current.provider_metadata
                    && target_capabilities.provider_metadata,
            },
        });
    }
    capabilities.unwrap_or_default()
}

fn route_has_normal_target(settings: &GatewaySettings, route: &RouteDefinition) -> bool {
    route
        .targets
        .iter()
        .any(|target| route_target_is_normal(settings, target))
}

fn route_target_is_normal(settings: &GatewaySettings, target: &RouteTarget) -> bool {
    let Some((provider_id, model_id)) = target.model.split_once('/') else {
        return false;
    };
    settings
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .is_some_and(|provider| !model_has_image_generation_identity(settings, provider, model_id))
}

fn reasoning_description(effort: &str) -> &'static str {
    match effort {
        "none" => "No additional reasoning",
        "minimal" => "Minimal reasoning",
        "low" => "Faster responses with lighter reasoning",
        "medium" => "Balanced speed and reasoning depth",
        "high" => "More reasoning for complex work",
        "xhigh" => "Extra reasoning for difficult work",
        "max" | "ultra" => "Maximum reasoning depth",
        _ => "Provider-defined reasoning level",
    }
}

fn multi_agent_version(settings: &GatewaySettings, model: &str) -> &'static str {
    if !settings.agents.multi_agent_v2 {
        return "v1";
    }
    match settings.agents.surface_mode {
        AgentSurfaceMode::V1 => "v1",
        AgentSurfaceMode::V2 => "v2",
        AgentSurfaceMode::Default => {
            let bare = model.rsplit('/').next().unwrap_or(model);
            if bare.starts_with("gpt-5.6-luna") {
                "v1"
            } else {
                "v2"
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
            .filter_map(|model| model.get("slug").and_then(Value::as_str).map(str::to_string))
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
            .filter_map(|model| model.get("slug").and_then(Value::as_str).map(str::to_string))
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
        assert_eq!(routed["display_name"], "Test provider model-a");
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
            catalog_auto_compact_token_limit(
                200_000,
                Some(160_000),
                Some(20_000),
            ),
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
        assert_eq!(kimi["display_name"], "Kimi k2.7-code");
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
}
