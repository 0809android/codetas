use crate::config::{
    AgentSurfaceMode, GatewaySettings, ModelMetadata, ProviderCapabilities, RouteDefinition,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

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
         your internal knowledge of your version may be outdated."
    )
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

    for provider in settings
        .providers
        .iter()
        .filter(|provider| provider.enabled)
    {
        let mut ids = provider.models.clone();
        if let Some(default_model) = provider.default_model.as_deref() {
            if !ids.iter().any(|model| model == default_model) {
                ids.insert(0, default_model.to_string());
            }
        }
        for model in settings
            .model_catalog
            .iter()
            .filter(|model| model.enabled && model.provider_id == provider.id)
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
            let display_name = details
                .and_then(|model| model.display_name.as_deref().map(str::to_string))
                .unwrap_or_else(|| catalog_display_name(&provider.id, &provider.name, &model_id));
            models.insert(
                slug.clone(),
                catalog_model(
                    &slug,
                    &display_name,
                    &format!("Routed by CODETAS through {}.", provider.name),
                    details
                        .and_then(|model| model.context_window)
                        .or_else(|| provider.model_context_windows.get(&model_id).copied()),
                    details
                        .filter(|model| !model.input_modalities.is_empty())
                        .map(|model| model.input_modalities.as_slice())
                        .or(provider_modalities),
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
                    details
                        .map(|model| &model.capabilities)
                        .unwrap_or(&provider.capabilities),
                    multi_agent_version(settings, &slug),
                    models.len() + 1,
                    details.and_then(|model| model.instructions_template.as_deref()),
                ),
            );
        }
    }

    for route in settings.routes.iter().filter(|route| route.enabled) {
        let route_model = catalog_route(settings, route, models.len() + 1);
        models.insert(
            route.alias.clone().unwrap_or_else(|| route.id.clone()),
            route_model,
        );
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
            "codetas-sidecar/image",
            "CODETAS Image",
            "Capability-routed image generation sidecar.",
            settings.sidecars.image_model.as_deref(),
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
                Some(&modalities),
                None,
                None,
                &capabilities,
                multi_agent_version(settings, slug),
                models.len() + 1,
                None,
            ),
        );
    }

    CodexCatalog {
        models: models.into_values().collect(),
    }
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
    (
        metadata.and_then(|model| model.context_window).or_else(|| {
            provider.and_then(|provider| provider.model_context_windows.get(model_id).copied())
        }),
        metadata
            .filter(|model| !model.input_modalities.is_empty())
            .map(|model| model.input_modalities.clone())
            .or_else(|| {
                provider.and_then(|provider| provider.model_input_modalities.get(model_id).cloned())
            })
            .unwrap_or_else(|| vec!["text".into()]),
        metadata
            .map(|model| model.capabilities.clone())
            .or_else(|| provider.map(|provider| provider.capabilities.clone()))
            .unwrap_or_default(),
    )
}

#[allow(clippy::too_many_arguments)]
fn catalog_model(
    slug: &str,
    display_name: &str,
    description: &str,
    context_window: Option<u64>,
    input_modalities: Option<&[String]>,
    reasoning_efforts: Option<&[String]>,
    default_reasoning_effort: Option<&str>,
    capabilities: &ProviderCapabilities,
    multi_agent_version: &str,
    priority: usize,
    instructions_template: Option<&str>,
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
        .unwrap_or_else(|| vec!["text".into()]);
    let compatibility_hash = catalog_compatibility_hash(
        slug,
        context_window,
        &modalities,
        &efforts,
        capabilities,
        multi_agent_version,
    );
    let mut entry = serde_json::Map::from_iter([
        ("slug".into(), json!(slug)),
        ("display_name".into(), json!(display_name)),
        ("description".into(), json!(description)),
        ("base_instructions".into(), json!(base_instructions(slug, context_window, &efforts, default_effort))),
        ("supported_reasoning_levels".into(), json!(levels)),
        ("shell_type".into(), json!("shell_command")),
        ("visibility".into(), json!("list")),
        ("supported_in_api".into(), json!(true)),
        ("priority".into(), json!(priority)),
        ("include_skills_usage_instructions".into(), json!(false)),
        (
            "include_apps_usage_instructions".into(),
            json!(native_openai_supports_fast(slug)),
        ),
        (
            "include_plugin_usage_instructions".into(),
            json!(native_openai_supports_fast(slug)),
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
            json!(capabilities.vision),
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
        (
            "use_responses_lite".into(),
            json!(native_openai_supports_fast(slug)),
        ),
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
            json!(context_window.saturating_mul(9) / 10),
        ),
    ]);
    if let Some(default_effort) = default_effort {
        entry.insert("default_reasoning_level".into(), json!(default_effort));
    }
    if let Some(template) = instructions_template {
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

fn catalog_display_name(provider_id: &str, provider_name: &str, model_id: &str) -> String {
    if provider_id == "openai" {
        return native_openai_display_name(model_id)
            .unwrap_or(model_id)
            .to_string();
    }
    let trimmed = model_id
        .strip_prefix(&format!("{provider_id}-"))
        .unwrap_or(model_id);
    format!("{provider_name} {trimmed}")
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

fn native_openai_supports_fast(slug: &str) -> bool {
    !slug.contains('/')
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
    modalities: &[String],
    efforts: &[String],
    capabilities: &ProviderCapabilities,
    multi_agent_version: &str,
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
    feed(b"codetas-catalog-v3");
    feed(slug.as_bytes());
    feed(&context_window.to_le_bytes());
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
        .filter_map(|target| {
            settings
                .model_catalog
                .iter()
                .find(|model| format!("{}/{}", model.provider_id, model.model_id) == target.model)
        })
        .collect::<Vec<&ModelMetadata>>();
    let context = target_models
        .iter()
        .filter_map(|model| model.context_window)
        .min();
    let modalities = common_modalities(&target_models);
    let capabilities = common_capabilities(settings, route);
    let public_id = route.alias.as_deref().unwrap_or(&route.id);
    catalog_model(
        public_id,
        &route.name,
        "Virtual route managed by CODETAS.",
        context,
        Some(&modalities),
        None,
        route.default_reasoning_effort.as_deref(),
        &capabilities,
        multi_agent_version(settings, public_id),
        priority,
        None,
    )
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
    for target in &route.targets {
        let Some((provider_id, _)) = target.model.split_once('/') else {
            continue;
        };
        let Some(provider) = settings
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
        else {
            continue;
        };
        capabilities = Some(match capabilities {
            None => provider.capabilities.clone(),
            Some(current) => ProviderCapabilities {
                streaming: current.streaming && provider.capabilities.streaming,
                tools: current.tools && provider.capabilities.tools,
                parallel_tools: current.parallel_tools && provider.capabilities.parallel_tools,
                vision: current.vision && provider.capabilities.vision,
                audio: current.audio && provider.capabilities.audio,
                reasoning: current.reasoning && provider.capabilities.reasoning,
                web_search: current.web_search && provider.capabilities.web_search,
                image_generation: current.image_generation
                    && provider.capabilities.image_generation,
                video_generation: current.video_generation
                    && provider.capabilities.video_generation,
                realtime: current.realtime && provider.capabilities.realtime,
                websockets: current.websockets && provider.capabilities.websockets,
                stateful_responses: current.stateful_responses
                    && provider.capabilities.stateful_responses,
            },
        });
    }
    capabilities.unwrap_or_default()
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
        assert!(routed.get("additional_speed_tiers").is_none());
        assert!(routed.get("service_tiers").is_none());
    }

    #[test]
    fn native_openai_catalog_exposes_fast_toggle() {
        let provider = ProviderDefinition {
            id: "openai".into(),
            name: "OpenAI (Codex login)".into(),
            models: vec!["gpt-5.6-sol".into(), "gpt-5.6-terra".into()],
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
}
