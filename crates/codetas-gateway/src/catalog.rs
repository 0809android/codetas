use crate::config::{
    effective_model_capabilities, model_has_image_generation_identity, model_rejects_images,
    AgentSurfaceMode, CatalogDisplayNameFormat, CredentialSource, GatewaySettings, ModelMetadata,
    ProviderCapabilities, ProviderDefinition, ProviderTransport, RouteDefinition, RouteTarget,
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
        for model in settings.model_catalog.iter().filter(|model| {
            model.enabled
                && model.provider_id == provider.id
                && !model_has_image_generation_identity(settings, provider, &model.model_id)
        }) {
            if !ids.iter().any(|id| id == &model.model_id) {
                ids.push(model.model_id.clone());
            }
        }
        for model_id in ids
            .into_iter()
            .filter(|id| !provider.model_catalog_aliases.contains_key(id))
        {
            let target = format!("{}/{}", provider.id, model_id);
            let slug = codex_model_slug(&provider.id, &model_id);
            let details = metadata.get(&target).copied();
            if details.is_some_and(|model| !model.enabled) {
                continue;
            }
            // Vision is the default capability for provider models. A stale
            // `text`-only row from an older settings file must not hide the
            // provider-wide default from the published Codex catalog.
            let text_only = ["text".to_string()];
            let provider_modalities = if model_rejects_images(&provider.id, &model_id) {
                Some(&text_only[..])
            } else {
                provider
                    .model_input_modalities
                    .get(&model_id)
                    .filter(|modalities| {
                        !provider.capabilities.vision
                            || modalities.iter().any(|modality| modality == "image")
                    })
                    .map(Vec::as_slice)
            };
            let provider_efforts = if model_in_list(&provider.no_reasoning_models, &model_id) {
                Some(&[][..])
            } else {
                provider
                    .model_reasoning_efforts
                    .get(&model_id)
                    .map(Vec::as_slice)
                    .or_else(|| {
                        provider
                            .requires_listed_reasoning_levels()
                            .then_some(&[][..])
                    })
            };
            let display_name = catalog_display_name(
                &settings.catalog.display_name_format,
                &provider.id,
                &provider.name,
                provider.display_prefix.as_deref(),
                &model_id,
                details.and_then(|model| model.display_name.as_deref()),
            );
            let display_name = match provider_connection_label(provider) {
                Some(label) => format!("{display_name} ({label})"),
                None => display_name,
            };
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
                        })
                        .or_else(|| Some(default_context_window(Some(provider), &model_id))),
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
        let right_slug = right
            .get("slug")
            .and_then(Value::as_str)
            .unwrap_or_default();
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

/// The `auto_compact_token_limit` Codex reads for a published model, which is the size
/// at which it compacts. `None` when the catalog does not list that model.
pub(crate) fn published_auto_compact_limit(settings: &GatewaySettings, slug: &str) -> Option<u64> {
    build_codex_catalog(settings)
        .models
        .iter()
        .find(|model| model.get("slug").and_then(Value::as_str) == Some(slug))
        .and_then(|model| model.get("auto_compact_token_limit"))
        .and_then(Value::as_u64)
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
                route.enabled && route.alias.as_deref().unwrap_or(route.id.as_str()) == published
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
    }) || settings
        .model_catalog
        .iter()
        .any(|model| model.enabled && model.provider_id == "openai" && model.model_id == bare)
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
            route
                .targets
                .iter()
                .filter_map(|route_target| route_target_window(settings, route_target))
                .min(),
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
        metadata
            .and_then(|model| model.context_window)
            .or_else(|| {
                provider.and_then(|provider| {
                    crate::registry::resolve_model_context_window(
                        &provider.model_context_windows,
                        model_id,
                    )
                })
            })
            .or_else(|| provider.map(|provider| default_context_window(Some(provider), model_id))),
        modalities,
        capabilities,
    )
}

/// The window a route target publishes: its catalog row, then its provider's map, then
/// the same default as a directly published model.
fn route_target_window(settings: &GatewaySettings, target: &RouteTarget) -> Option<u64> {
    let (provider_id, model_id) = target.model.split_once('/')?;
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.id == provider_id);
    settings
        .model_catalog
        .iter()
        .find(|model| model.provider_id == provider_id && model.model_id == model_id)
        .and_then(|model| model.context_window)
        .or_else(|| {
            provider.and_then(|provider| {
                crate::registry::resolve_model_context_window(
                    &provider.model_context_windows,
                    model_id,
                )
            })
        })
        .or_else(|| Some(default_context_window(provider, model_id)))
}

/// Assumed window for a hosted model when nothing states its limits.
///
/// The part of a request that compaction cannot shrink reached about 250k tokens
/// with 400+ tools, so a trigger below that compacts after every tool call. 272k
/// (trigger 244.8k) is too low. Almost no recent model has a window between 272k
/// and 400k, so 400k clears it.
///
/// A model whose real window is smaller overflows before it reaches the 360k trigger.
/// When the upstream's error is recognized it becomes `context_length_exceeded`: that
/// turn fails, and the thread compacts only after it. The cost recurs each time the
/// thread fills up again. An error that is not recognized stays a generic provider
/// error, and nothing compacts until the user does it.
const DEFAULT_CONTEXT_WINDOW: u64 = 400_000;
/// For providers where overflowing is worse than compacting early. Local servers are
/// often configured with small windows, and some truncate silently instead of failing.
/// GitHub Models has no discovery and no per-model data, and its models vary widely.
const DEFAULT_SMALL_CONTEXT_WINDOW: u64 = 128_000;

fn assumes_small_window(provider: &ProviderDefinition) -> bool {
    // LiteLLM on a private address is a router to hosted models, not a local model server.
    (provider.allow_private_network && provider.id != "litellm") || provider.id == "github-models"
}

/// A registry row for the same model under another provider can only lower this, so a
/// model known to be small does not get the hosted guess, and one known to be large keeps
/// it: what a provider actually serves may be less than the largest window seen elsewhere.
fn default_context_window(provider: Option<&ProviderDefinition>, model_id: &str) -> u64 {
    let default = if provider.is_some_and(assumes_small_window) {
        DEFAULT_SMALL_CONTEXT_WINDOW
    } else {
        DEFAULT_CONTEXT_WINDOW
    };
    crate::registry::smallest_known_context_window(model_id)
        .map_or(default, |known| known.min(default))
}

/// What a window leaves for input after reserving output. A reserve that takes more
/// than half of the window is a mismatched pair, such as a window discovery reported
/// and an output limit inherited from a bigger sibling. Honouring it would put the
/// compaction trigger under what compaction can shrink, or at 1 when the reserve
/// covers the whole window.
pub(crate) fn input_budget_after_output_reserve(
    context_window: u64,
    max_output_tokens: Option<u64>,
) -> u64 {
    context_window
        .saturating_sub(max_output_tokens.unwrap_or(0))
        .max(context_window / 2)
}

/// The output limit that fits that budget, for a request to send. A reserve within half of
/// the window comes back unchanged.
pub(crate) fn output_limit_within_window(context_window: u64, max_output_tokens: u64) -> u64 {
    let budget = input_budget_after_output_reserve(context_window, Some(max_output_tokens));
    max_output_tokens.min(context_window - budget)
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
    let context_window = context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW).max(8_192);
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
    let auto_compact_token_limit =
        catalog_auto_compact_token_limit(context_window, max_input_tokens, max_output_tokens);
    let generated_base_instructions =
        base_instructions(slug, context_window, &efforts, default_effort);
    let instructions_template = instructions_template.map(with_skill_loop_guard);
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
        (
            "base_instructions".into(),
            json!(generated_base_instructions),
        ),
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
    let context_input_budget = input_budget_after_output_reserve(context_window, max_output_tokens);
    let input_budget = max_input_tokens
        .map(|limit| limit.min(context_input_budget))
        .unwrap_or(context_input_budget);
    input_budget
        .saturating_mul(AUTO_COMPACT_PERCENT)
        .checked_div(100)
        .unwrap_or(0)
        .max(1)
}

// Keep native OpenAI slugs unchanged for existing conversations. Only the
// picker label changes: subscription entries keep the bare model name and only
// metered API connections are marked "(API)"; slugs remain independent.
fn provider_connection_label(provider: &ProviderDefinition) -> Option<&'static str> {
    match provider.id.as_str() {
        "openai-api" | "openai-apikey" | "anthropic-apikey" => Some("API"),
        "openai" | "anthropic" => match provider.credential.source {
            CredentialSource::Forward | CredentialSource::OAuth => None,
            CredentialSource::Environment
            | CredentialSource::Keychain
            | CredentialSource::Command => Some("API"),
            _ => None,
        },
        _ => None,
    }
}

fn catalog_display_name(
    format: &CatalogDisplayNameFormat,
    provider_id: &str,
    provider_name: &str,
    display_prefix: Option<&str>,
    model_id: &str,
    custom_name: Option<&str>,
) -> String {
    let custom_name = custom_name.map(str::trim).filter(|name| !name.is_empty());
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
                    custom_name.unwrap_or_else(|| {
                        native_openai_display_name(model_id).unwrap_or(model_id)
                    }),
                );
            }
            // Model name only; the provider name appears only when the user set
            // an explicit display prefix (or picks a provider-qualified format).
            join_display_prefix(prefix.unwrap_or(""), model_label)
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
        "gpt-6-astra" => "GPT-6-Astra",
        "gpt-6.1-sol" => "GPT-6.1-Sol",
        "gpt-6-sol" => "GPT-6-Sol",
        "gpt-6-luna" => "GPT-6-Luna",
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
                model.enabled && model.provider_id == provider_id && model.model_id == model_id
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
                .unwrap_or_else(|| default_context_window(provider, model_id))
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
            let context_input_budget = input_budget_after_output_reserve(context, max_output);
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
                let metadata = settings
                    .model_catalog
                    .iter()
                    .find(|model| model.provider_id == provider_id && model.model_id == model_id);
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
            models.iter().all(|model| {
                model.input_modalities.contains(modality)
                    && (*modality != "image"
                        || !model_rejects_images(&model.provider_id, &model.model_id))
            })
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
        let target_capabilities = effective_model_capabilities(provider, metadata, model_id);
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
                image_generation: current.image_generation && target_capabilities.image_generation,
                video_generation: current.video_generation && target_capabilities.video_generation,
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
mod tests;
