use super::*;

/// Gives a newly discovered model the settings of its closest older sibling.
///
/// A provider's `/models` usually lists only ids, so a new version would otherwise
/// start with none of the limits and wire quirks its family needs. Call this once,
/// when the model first appears: a value the user later changes or removes must
/// stay that way, and existing values are never replaced.
///
/// Everything comes from one sibling. Choosing per setting would mix versions:
/// `glm-5.4` would take the thinking toggle of `glm-5.1` even though `glm-5.2` and
/// `glm-5.3` dropped it.
pub fn inherit_settings_from_older_sibling(provider: &mut ProviderDefinition, model: &str) {
    inherit_settings_with_reported_window(provider, model, None);
}

/// `inherit_settings_from_older_sibling` for a model whose `/models` row states a
/// window. That window outranks the provider's own map in the catalog and in routing,
/// so limits taken from a larger sibling must fit it too.
pub fn inherit_settings_with_reported_window(
    provider: &mut ProviderDefinition,
    model: &str,
    reported_window: Option<u64>,
) {
    fn copy_entry<V: Clone>(map: &mut BTreeMap<String, V>, sibling: &str, model: &str) {
        if !map.contains_key(model) {
            if let Some(value) = map.get(sibling).cloned() {
                map.insert(model.to_string(), value);
            }
        }
    }
    fn copy_membership(list: &mut Vec<String>, sibling: &str, model: &str) {
        if list.iter().any(|entry| entry == sibling) && !list.iter().any(|entry| entry == model) {
            list.push(model.to_string());
        }
    }

    // The load-time backfill stops after one revision so a removal sticks. DeepSeek
    // and Kimi can reject a history without this, even with no sibling to copy from.
    let mut defaults = ProviderDefinition {
        id: provider.id.clone(),
        protocol: provider.protocol,
        ..ProviderDefinition::default()
    };
    apply_registry_defaults(&mut defaults);
    if defaults
        .preserve_reasoning_content_models
        .iter()
        .any(|entry| entry == model)
        && !provider
            .preserve_reasoning_content_models
            .iter()
            .any(|entry| entry == model)
    {
        provider
            .preserve_reasoning_content_models
            .push(model.to_string());
    }

    let known_models = provider
        .models
        .iter()
        .chain(provider.model_protocols.keys())
        .chain(provider.model_input_modalities.keys())
        .chain(provider.model_reasoning_effort_map.keys())
        .chain(provider.model_context_windows.keys())
        .chain(provider.model_max_input_tokens.keys())
        .chain(provider.model_max_output_tokens.keys())
        .chain(provider.model_reasoning_efforts.keys())
        .chain(provider.model_default_reasoning_efforts.keys())
        .map(String::as_str)
        .filter(|known| *known != model);
    let Some(sibling) = closest_older_sibling(known_models, model).map(str::to_string) else {
        return;
    };
    let sibling = sibling.as_str();

    copy_entry(&mut provider.model_protocols, sibling, model);
    copy_entry(&mut provider.model_input_modalities, sibling, model);
    copy_entry(&mut provider.model_reasoning_effort_map, sibling, model);
    if let Some(window) = provider.model_context_windows.get(sibling).copied() {
        if !provider.model_context_windows.contains_key(model)
            && window_fits_saved_limits(provider, model, window)
        {
            provider
                .model_context_windows
                .insert(model.to_string(), window);
        }
    }

    // Validation rejects a limit above the window, so the smallest window the model
    // is known to have caps what it inherits.
    let window = provider
        .model_context_windows
        .get(model)
        .copied()
        .into_iter()
        .chain(reported_window)
        .min();
    if !provider.model_max_input_tokens.contains_key(model) {
        if let Some(limit) = provider.model_max_input_tokens.get(sibling).copied() {
            provider.model_max_input_tokens.insert(
                model.to_string(),
                window.map_or(limit, |window| limit.min(window)),
            );
        }
    }
    // An output reserve comes off the compaction budget. Capping one at the window
    // would leave no budget at all, and a sibling's reserve that takes over half of
    // this model's window was sized for a bigger model, so it is not inherited.
    if !provider.model_max_output_tokens.contains_key(model) {
        if let Some(limit) = provider
            .model_max_output_tokens
            .get(sibling)
            .copied()
            .filter(|limit| window.is_none_or(|window| limit.saturating_mul(2) <= window))
        {
            provider
                .model_max_output_tokens
                .insert(model.to_string(), limit);
        }
    }

    // Validation requires the default effort to be in the model's own list, so
    // take them together, and only when the model has neither.
    if !provider.model_reasoning_efforts.contains_key(model)
        && !provider.model_default_reasoning_efforts.contains_key(model)
    {
        copy_entry(&mut provider.model_reasoning_efforts, sibling, model);
        copy_entry(
            &mut provider.model_default_reasoning_efforts,
            sibling,
            model,
        );
    }

    for list in [
        &mut provider.preserve_reasoning_content_models,
        &mut provider.no_reasoning_models,
        &mut provider.no_temperature_models,
        &mut provider.no_top_p_models,
        &mut provider.no_penalty_models,
        &mut provider.no_structured_output_models,
        &mut provider.auto_tool_choice_only_models,
        &mut provider.reasoning_split_models,
        &mut provider.thinking_toggle_models,
        &mut provider.thinking_budget_models,
        &mut provider.empty_completion_retry_models,
        &mut provider.terminal_continuation_guard_models,
    ] {
        copy_membership(list, sibling, model);
    }
    if let Some(list) = provider.requires_reasoning_placeholder_models.as_mut() {
        copy_membership(list, sibling, model);
    }
}

#[derive(Deserialize)]
pub(super) struct PublishedModelLimit {
    context: u64,
    input: Option<u64>,
}

/// Limits for providers whose `/models` reports none, keyed by provider id. This
/// is generated by `scripts/update_published_limits.py`, so refresh it there rather
/// than editing it by hand.
pub(super) static PUBLISHED_MODEL_LIMITS: LazyLock<
    BTreeMap<String, BTreeMap<String, PublishedModelLimit>>,
> = LazyLock::new(|| {
    serde_json::from_str(include_str!("published_limits.json")).unwrap_or_default()
});

static SMALLEST_KNOWN_WINDOWS: LazyLock<BTreeMap<String, u64>> = LazyLock::new(|| {
    let mut smallest = BTreeMap::<String, u64>::new();
    let mut note = |model: &str, window: u64| {
        smallest
            .entry(model.to_ascii_lowercase())
            .and_modify(|known| *known = window.min(*known))
            .or_insert(window);
    };
    for preset in provider_presets() {
        let mut provider = ProviderDefinition {
            id: preset.id.into(),
            protocol: preset.protocol,
            ..ProviderDefinition::default()
        };
        apply_registry_defaults(&mut provider);
        for (model, window) in &provider.model_context_windows {
            note(model, *window);
        }
    }
    // Some tables belong to a provider that has no preset.
    for limits in PUBLISHED_MODEL_LIMITS.values() {
        for (model, limit) in limits {
            note(model, limit.context);
        }
    }
    smallest
});

/// The smallest window any preset or published table states for this exact model id,
/// ignoring case.
pub(crate) fn smallest_known_context_window(model_id: &str) -> Option<u64> {
    SMALLEST_KNOWN_WINDOWS
        .get(&model_id.to_ascii_lowercase())
        .copied()
}

/// Fills only what the registry left open, so its own rows always win.
/// `backfill_registry_input_limits` carries these into saved settings for the
/// models a provider actually lists.
pub(super) fn insert_published_limits(provider: &mut ProviderDefinition) {
    // `opencode-free` is Zen's endpoint without a key, so it serves Zen's `-free`
    // models. `account_usage` accepts both spellings of Command Code.
    let table = match provider.id.as_str() {
        "opencode-free" => "opencode-zen",
        "commandcode" => "command-code",
        id => id,
    };
    let Some(limits) = PUBLISHED_MODEL_LIMITS.get(table) else {
        return;
    };
    for (model, limit) in limits {
        let window = *provider
            .model_context_windows
            .entry(model.clone())
            .or_insert(limit.context);
        if let Some(input) = limit.input {
            provider
                .model_max_input_tokens
                .entry(model.clone())
                .or_insert(input.min(window));
        }
    }
}

/// The candidate that a new `model_id` most likely behaves like: the same family
/// and suffix, the same major version, and the newest minor version not above its
/// own. `deepseek-v4.1-flash` matches `deepseek-v4-flash`, but not
/// `deepseek-v4-pro` or `deepseek-v3-flash`.
pub(super) fn closest_older_sibling<'a>(
    candidates: impl Iterator<Item = &'a str>,
    model_id: &str,
) -> Option<&'a str> {
    let target = parse_model_version(model_id)?;
    candidates
        .filter_map(|candidate| {
            let other = parse_model_version(candidate)?;
            (other.family == target.family
                && other.suffix == target.suffix
                && other.major == target.major
                && other.minor <= target.minor)
                .then_some((other.minor, candidate))
        })
        .max_by_key(|(minor, _)| *minor)
        .map(|(_, candidate)| candidate)
}

struct ModelVersion<'a> {
    family: &'a str,
    major: u32,
    minor: u32,
    suffix: &'a str,
}

/// Splits `deepseek-v4.1-flash` into family `deepseek-v`, version 4.1 and suffix
/// `-flash`. A dotted segment wins over an integer one, so the `0309` in
/// `grok-4.20-0309-reasoning` stays part of the suffix.
fn parse_model_version(model_id: &str) -> Option<ModelVersion<'_>> {
    let mut first_integer = None;
    let mut start = 0;
    for segment in model_id.split('-') {
        let end = start + segment.len();
        if let Some((digits_at, major, minor)) = parse_version_segment(segment) {
            let version = ModelVersion {
                family: &model_id[..start + digits_at],
                major,
                minor: minor.unwrap_or(0),
                suffix: &model_id[end..],
            };
            if minor.is_some() {
                return Some(version);
            }
            first_integer = first_integer.or(Some(version));
        }
        start = end + 1;
    }
    first_integer
}

/// Parses a segment such as `4.1`, `v4` or `qwen3.8` into the offset of its first
/// digit and its version. `/` is allowed for ids like `qwen/qwen3.8`.
fn parse_version_segment(segment: &str) -> Option<(usize, u32, Option<u32>)> {
    let digits_at = segment.find(|character: char| character.is_ascii_digit())?;
    let (name, version) = segment.split_at(digits_at);
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphabetic() || byte == b'/')
    {
        return None;
    }
    let (major, minor) = match version.split_once('.') {
        Some((major, minor)) => (parse_digits(major)?, Some(parse_digits(minor)?)),
        None => (parse_digits(version)?, None),
    };
    Some((digits_at, major, minor))
}

fn parse_digits(value: &str) -> Option<u32> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

pub(super) fn insert_limits(target: &mut BTreeMap<String, u64>, values: &[(&str, u64)]) {
    for (model, limit) in values {
        target.insert((*model).into(), *limit);
    }
}

pub(super) fn set_model_modalities(
    provider: &mut ProviderDefinition,
    model: &str,
    values: &[&str],
) {
    provider
        .model_input_modalities
        .insert(model.into(), strings(values));
    if values.contains(&"image") {
        provider.capabilities.vision = true;
    }
}

pub(super) fn set_efforts(provider: &mut ProviderDefinition, models: &[&str], efforts: &[&str]) {
    for model in models {
        provider
            .model_reasoning_efforts
            .insert((*model).into(), strings(efforts));
    }
    if !models.is_empty() && !efforts.is_empty() {
        provider.capabilities.reasoning = true;
    }
}

pub(super) fn set_wire_map(
    provider: &mut ProviderDefinition,
    models: &[&str],
    values: &[(&str, &str)],
) {
    let mapping = values
        .iter()
        .map(|(effort, wire)| ((*effort).to_string(), (*wire).to_string()))
        .collect::<BTreeMap<_, _>>();
    for model in models {
        provider
            .model_reasoning_effort_map
            .insert((*model).into(), mapping.clone());
    }
}
