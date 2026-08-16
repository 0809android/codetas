use super::*;

pub(crate) fn apply_provider_wire_compatibility(
    body: &mut Value,
    request: &Value,
    candidate: &RouteCandidate,
    protocol: ProviderProtocol,
) -> Result<(), String> {
    let provider = &candidate.provider;
    let model = &candidate.upstream_model;
    let supports_structured_output = provider.capabilities.structured_output
        && !model_matches_any(model, &provider.no_structured_output_models);
    if !supports_structured_output {
        strip_structured_output(body, protocol);
    }
    let supports_service_tier = provider.capabilities.service_tier
        || model_matches_any(model, &provider.service_tier_models);
    if !supports_service_tier {
        if let Some(object) = body.as_object_mut() {
            object.remove("service_tier");
            object.remove("serviceTier");
        }
    } else if protocol == ProviderProtocol::ChatCompletions {
        if let Some(tier) = provider.chat_service_tier.as_deref() {
            if let Some(object) = body.as_object_mut() {
                object
                    .entry("service_tier")
                    .or_insert_with(|| Value::String(tier.to_string()));
            }
        }
    }
    if protocol == ProviderProtocol::GeminiGenerateContent {
        if let Some(effort) = request
            .pointer("/reasoning/effort")
            .and_then(Value::as_str)
            .and_then(google_thinking_level)
        {
            let Some(object) = body.as_object_mut() else {
                return Ok(());
            };
            let generation = object
                .entry("generationConfig")
                .or_insert_with(|| json!({}));
            if !generation.is_object() {
                *generation = json!({});
            }
            if let Some(generation) = generation.as_object_mut() {
                generation.insert("thinkingConfig".into(), json!({"thinkingLevel": effort}));
            }
        }
        return Ok(());
    }
    if protocol == ProviderProtocol::AnthropicMessages {
        if candidate.provider.escape_builtin_tool_names {
            escape_anthropic_tool_names(body)?;
        }
        return Ok(());
    }
    if protocol == ProviderProtocol::Responses {
        if let Some(mode) = candidate
            .provider
            .model_reasoning_modes
            .get(&candidate.upstream_model)
        {
            let Some(object) = body.as_object_mut() else {
                return Ok(());
            };
            let reasoning = object.entry("reasoning").or_insert_with(|| json!({}));
            if !reasoning.is_object() {
                *reasoning = json!({});
            }
            if let Some(reasoning) = reasoning.as_object_mut() {
                reasoning.insert("mode".into(), Value::String(mode.clone()));
            }
        }
        return Ok(());
    }
    if protocol != ProviderProtocol::ChatCompletions {
        return Ok(());
    }
    let Some(object) = body.as_object_mut() else {
        return Ok(());
    };
    let effort = request.pointer("/reasoning/effort").and_then(Value::as_str);
    if model_matches_any(model, &provider.reasoning_split_models) {
        object.insert("reasoning_split".into(), Value::Bool(true));
    }
    if let Some(effort) = effort {
        if model_matches_any(model, &provider.thinking_budget_models) {
            if let Some(rank) = reasoning_rank(effort) {
                let max_tokens = object
                    .get("max_tokens")
                    .and_then(Value::as_u64)
                    .or(candidate.max_output_tokens)
                    .unwrap_or(32_768);
                let percent = match rank {
                    0 | 1 => 0,
                    2 => 20,
                    3 => 50,
                    4 => 75,
                    5 => 90,
                    _ => 100,
                };
                if rank == 1 {
                    object.insert("thinking_budget".into(), json!(0));
                } else if percent > 0 {
                    object.insert(
                        "thinking_budget".into(),
                        json!((max_tokens.saturating_mul(percent) / 100).max(1)),
                    );
                }
            }
        } else if model_matches_any(model, &provider.thinking_toggle_models) || is_minimax_m3(model)
        {
            let toggle = if is_minimax_m3(model) {
                match effort {
                    "enabled" | "disabled" | "adaptive" => Some(effort),
                    value if reasoning_rank(value).is_some_and(|rank| rank <= 2) => {
                        Some("disabled")
                    }
                    value if reasoning_rank(value).is_some() => Some("adaptive"),
                    _ => None,
                }
            } else {
                match effort {
                    "enabled" | "disabled" | "adaptive" => Some(effort),
                    value if reasoning_rank(value).is_some_and(|rank| rank <= 2) => {
                        Some("disabled")
                    }
                    value if reasoning_rank(value).is_some() => Some("enabled"),
                    _ => None,
                }
            };
            if let Some(toggle) = toggle {
                object.insert("thinking".into(), json!({"type": toggle}));
            }
        } else if map_value_ignore_case(&provider.model_reasoning_efforts, model)
            .is_some_and(|efforts| !efforts.is_empty())
            || map_value_ignore_case(&provider.model_reasoning_effort_map, model).is_some()
        {
            object.insert("reasoning_effort".into(), Value::String(effort.to_string()));
        }
    }
    if model_matches_any(model, &provider.auto_tool_choice_only_models) {
        if let Some(choice) = object.get_mut("tool_choice") {
            if choice.as_str() != Some("none") {
                *choice = Value::String("auto".into());
            }
        }
    }
    ensure_chat_function_parameters(object);
    if is_zen_chat_endpoint(&provider.base_url) {
        sanitize_zen_chat_tools(object);
    } else if is_kimi_chat_endpoint(&provider.base_url) {
        sanitize_kimi_chat_tools(object);
    } else if is_xai_chat_endpoint(&provider.base_url) {
        sanitize_xai_chat_tools(object);
    }
    Ok(())
}

fn strip_structured_output(body: &mut Value, protocol: ProviderProtocol) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    match protocol {
        ProviderProtocol::Responses => {
            if let Some(text) = object.get_mut("text").and_then(Value::as_object_mut) {
                text.remove("format");
            }
            object.remove("response_format");
        }
        ProviderProtocol::ChatCompletions => {
            object.remove("response_format");
        }
        ProviderProtocol::AnthropicMessages => {
            if let Some(output_config) = object
                .get_mut("output_config")
                .and_then(Value::as_object_mut)
            {
                output_config.remove("format");
            }
        }
        ProviderProtocol::GeminiGenerateContent => {
            if let Some(generation) = object
                .get_mut("generationConfig")
                .and_then(Value::as_object_mut)
            {
                generation.remove("responseMimeType");
                generation.remove("responseSchema");
                generation.remove("responseJsonSchema");
            }
        }
    }
}

pub(crate) fn model_supports_vision(
    provider: &crate::config::ProviderDefinition,
    model: &str,
) -> bool {
    map_value_ignore_case(&provider.model_input_modalities, model)
        .map(|modalities| modalities.iter().any(|value| value == "image"))
        .unwrap_or(provider.capabilities.vision)
}

pub(crate) fn model_matches_any(model: &str, configured: &[String]) -> bool {
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

pub(crate) fn model_requires_reasoning_placeholder(
    provider: &crate::config::ProviderDefinition,
    model: &str,
) -> bool {
    model_matches_any(
        model,
        provider
            .requires_reasoning_placeholder_models
            .as_deref()
            .unwrap_or(&provider.preserve_reasoning_content_models),
    )
}

pub(crate) fn map_value_ignore_case<'a, V>(
    map: &'a std::collections::BTreeMap<String, V>,
    key: &str,
) -> Option<&'a V> {
    map.get(key).or_else(|| {
        let folded = key.to_ascii_lowercase();
        map.iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(&folded))
            .map(|(_, value)| value)
    })
}

pub(crate) fn is_minimax_m3(model: &str) -> bool {
    model.eq_ignore_ascii_case("minimax-m3") || model.eq_ignore_ascii_case("MiniMax-M3")
}

pub(crate) struct AttemptFailure {
    pub(crate) response: Response<Body>,
    pub(crate) kind: AttemptFailureKind,
}

#[cfg(test)]
mod conformance_tests {
    use super::*;
    use crate::config::{ProviderCapabilities, ProviderDefinition};

    fn candidate(provider: ProviderDefinition, model: &str) -> RouteCandidate {
        RouteCandidate {
            capabilities: provider.capabilities.clone(),
            provider,
            upstream_model: model.into(),
            exposed_model: model.into(),
            credential: None,
            account_id: None,
            target_key: format!("fixture/{model}"),
            route_id: None,
            failure_threshold: 1,
            quota_threshold_percent: 0,
            input_price_per_million: None,
            output_price_per_million: None,
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
        }
    }

    #[test]
    fn model_match_wildcard_covers_every_model_without_changing_exact_suffix_matching() {
        assert!(model_matches_any("claude-future-model", &["*".into()]));
        assert!(model_matches_any("K3", &["k3".into()]));
        assert!(model_matches_any("k3:latest", &["k3".into()]));
        assert!(model_matches_any("k3", &["k3:latest".into()]));
        assert!(!model_matches_any("k30", &["k3".into()]));
        assert!(!model_matches_any("claude-future-model", &[]));
    }

    #[test]
    fn unsupported_structured_output_and_service_tier_are_removed() {
        let provider = ProviderDefinition {
            capabilities: ProviderCapabilities {
                structured_output: false,
                service_tier: false,
                ..ProviderCapabilities::default()
            },
            ..ProviderDefinition::default()
        };
        let candidate = candidate(provider, "plain");
        let request = json!({"text": {"format": {"type": "json_schema"}}, "service_tier": "priority"});
        let mut wire = request.clone();
        apply_provider_wire_compatibility(
            &mut wire,
            &request,
            &candidate,
            ProviderProtocol::Responses,
        ).expect("compatibility");
        assert!(wire.pointer("/text/format").is_none());
        assert!(wire.get("service_tier").is_none());
    }

    #[test]
    fn supported_chat_service_tier_is_injected_without_dropping_schema() {
        let provider = ProviderDefinition {
            capabilities: ProviderCapabilities {
                structured_output: true,
                service_tier: true,
                ..ProviderCapabilities::default()
            },
            chat_service_tier: Some("priority".into()),
            ..ProviderDefinition::default()
        };
        let candidate = candidate(provider, "tiered");
        let request = json!({"response_format": {"type": "json_schema"}});
        let mut wire = request.clone();
        apply_provider_wire_compatibility(
            &mut wire,
            &request,
            &candidate,
            ProviderProtocol::ChatCompletions,
        ).expect("compatibility");
        assert_eq!(wire["service_tier"], "priority");
        assert_eq!(wire["response_format"]["type"], "json_schema");
    }
}
