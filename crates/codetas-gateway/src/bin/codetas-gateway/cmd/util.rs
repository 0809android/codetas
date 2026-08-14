use super::*;
use codetas_gateway::AccountReference;
use codetas_gateway::AgentSurfaceMode;
use codetas_gateway::ExternalAccessKey;
use codetas_gateway::GatewaySettings;
use codetas_gateway::ModelMetadata;
use codetas_gateway::RouteDefinition;
use codetas_gateway::RouteStrategy;
use codetas_gateway::RouteTarget;
use serde::Serialize;
use serde_json::json;
use std::fs;
use std::path::Path;
pub(crate) fn model_rows(
    settings: &GatewaySettings,
    provider_filter: Option<&str>,
) -> Vec<serde_json::Value> {
    let mut rows = Vec::new();
    for provider in &settings.providers {
        if provider_filter.is_some_and(|filter| provider.id != filter) {
            continue;
        }
        let mut ids = provider.models.clone();
        if let Some(default) = provider.default_model.as_ref() {
            if !ids.contains(default) {
                ids.push(default.clone());
            }
        }
        for metadata in settings
            .model_catalog
            .iter()
            .filter(|item| item.provider_id == provider.id)
        {
            if !ids.contains(&metadata.model_id) {
                ids.push(metadata.model_id.clone());
            }
        }
        ids.sort();
        ids.dedup();
        for id in ids {
            let metadata = settings
                .model_catalog
                .iter()
                .find(|item| item.provider_id == provider.id && item.model_id == id);
            rows.push(json!({
                "id": format!("{}/{}", provider.id, id),
                "providerId": provider.id,
                "modelId": id,
                "enabled": provider.enabled && metadata.is_none_or(|item| item.enabled),
                "metadata": metadata,
            }));
        }
    }
    rows
}

pub(crate) fn save_settings(path: &Path, settings: &GatewaySettings) -> Result<(), String> {
    let mut settings = settings.clone();
    settings.prune_stale_client_integrations();
    settings.validate()?;
    let bytes = serde_json::to_vec_pretty(&settings)
        .map_err(|error| format!("cannot encode settings: {error}"))?;
    if path.exists() {
        let backup = backup_path(path);
        fs::copy(path, &backup)
            .map_err(|error| format!("cannot create settings backup: {error}"))?;
        secure_existing_file(&backup)?;
    }
    atomic_write_new_or_owned(path, &bytes, true)
}

pub(crate) fn provider_ref<'a>(
    settings: &'a GatewaySettings,
    id: &str,
) -> Result<&'a codetas_gateway::ProviderDefinition, String> {
    settings
        .providers
        .iter()
        .find(|item| item.id == id)
        .ok_or_else(|| format!("unknown provider: {id}"))
}

pub(crate) fn provider_mut<'a>(
    settings: &'a mut GatewaySettings,
    id: &str,
) -> Result<&'a mut codetas_gateway::ProviderDefinition, String> {
    settings
        .providers
        .iter_mut()
        .find(|item| item.id == id)
        .ok_or_else(|| format!("unknown provider: {id}"))
}

pub(crate) fn account_ref<'a>(
    settings: &'a GatewaySettings,
    provider: &str,
    id: &str,
) -> Result<&'a AccountReference, String> {
    settings
        .account_pool
        .accounts
        .iter()
        .find(|item| item.provider_id == provider && item.id == id)
        .ok_or_else(|| format!("unknown account: {provider}/{id}"))
}

pub(crate) fn account_mut<'a>(
    settings: &'a mut GatewaySettings,
    provider: &str,
    id: &str,
) -> Result<&'a mut AccountReference, String> {
    settings
        .account_pool
        .accounts
        .iter_mut()
        .find(|item| item.provider_id == provider && item.id == id)
        .ok_or_else(|| format!("unknown account: {provider}/{id}"))
}

pub(crate) fn model_mut<'a>(
    settings: &'a mut GatewaySettings,
    provider: &str,
    model: &str,
) -> Result<&'a mut ModelMetadata, String> {
    settings
        .model_catalog
        .iter_mut()
        .find(|item| item.provider_id == provider && item.model_id == model)
        .ok_or_else(|| format!("unknown model metadata: {provider}/{model}"))
}

pub(crate) fn route_ref<'a>(
    settings: &'a GatewaySettings,
    id: &str,
) -> Result<&'a RouteDefinition, String> {
    settings
        .routes
        .iter()
        .find(|item| item.id == id)
        .ok_or_else(|| format!("unknown route: {id}"))
}

pub(crate) fn route_mut<'a>(
    settings: &'a mut GatewaySettings,
    id: &str,
) -> Result<&'a mut RouteDefinition, String> {
    settings
        .routes
        .iter_mut()
        .find(|item| item.id == id)
        .ok_or_else(|| format!("unknown route: {id}"))
}

pub(crate) fn access_key_mut<'a>(
    settings: &'a mut GatewaySettings,
    id: &str,
) -> Result<&'a mut ExternalAccessKey, String> {
    settings
        .security
        .external_access_keys
        .iter_mut()
        .find(|item| item.id == id)
        .ok_or_else(|| format!("unknown access key: {id}"))
}

pub(crate) fn parse_route_target(value: &str) -> Result<RouteTarget, String> {
    let (model, weight) = value
        .rsplit_once('@')
        .map_or((value, 1), |(model, weight)| {
            (
                model,
                weight
                    .parse::<u16>()
                    .ok()
                    .filter(|value| *value > 0)
                    .unwrap_or(0),
            )
        });
    if weight == 0 {
        return Err(format!("route target has invalid weight: {value}"));
    }
    Ok(RouteTarget {
        model: model.into(),
        weight,
    })
}

pub(crate) fn parse_route_strategy(value: &str) -> Result<RouteStrategy, String> {
    match value {
        "failover" => Ok(RouteStrategy::Failover),
        "weighted-round-robin" => Ok(RouteStrategy::WeightedRoundRobin),
        "least-usage" => Ok(RouteStrategy::LeastUsage),
        _ => Err("route strategy must be failover, weighted-round-robin, or least-usage".into()),
    }
}

pub(crate) fn parse_surface(value: &str) -> Result<AgentSurfaceMode, String> {
    match value {
        "v1" => Ok(AgentSurfaceMode::V1),
        "default" => Ok(AgentSurfaceMode::Default),
        "v2" => Ok(AgentSurfaceMode::V2),
        _ => Err("agent surface must be v1, default, or v2".into()),
    }
}

pub(crate) fn parse_bool(value: &str, option: &str) -> Result<bool, String> {
    match value {
        "on" | "true" | "1" => Ok(true),
        "off" | "false" | "0" => Ok(false),
        _ => Err(format!("{option} must be on or off")),
    }
}

pub(crate) fn clearable(value: String) -> Option<String> {
    (value != "-").then_some(value)
}
pub(crate) fn clearable_csv(value: String) -> Vec<String> {
    if value == "-" {
        Vec::new()
    } else {
        csv(&value)
    }
}
pub(crate) fn csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

pub(crate) fn optional_bool(args: &mut Vec<String>, option: &str) -> Result<Option<bool>, String> {
    take_option(args, option)?
        .map(|value| parse_bool(&value, option))
        .transpose()
}

pub(crate) fn optional_u16(args: &mut Vec<String>, option: &str) -> Result<Option<u16>, String> {
    take_option(args, option)?
        .map(|value| {
            value
                .parse::<u16>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| format!("{option} must be an integer between 1 and 65535"))
        })
        .transpose()
}

pub(crate) fn optional_u64(args: &mut Vec<String>, option: &str) -> Result<Option<u64>, String> {
    take_option(args, option)?
        .map(|value| {
            value
                .parse::<u64>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| format!("{option} must be a positive integer"))
        })
        .transpose()
}

pub(crate) fn optional_usize(
    args: &mut Vec<String>,
    option: &str,
) -> Result<Option<usize>, String> {
    take_option(args, option)?
        .map(|value| {
            value
                .parse::<usize>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| format!("{option} must be a positive integer"))
        })
        .transpose()
}

pub(crate) fn parse_clearable_u64(value: &str, option: &str) -> Result<Option<u64>, String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| format!("{option} must be a non-negative integer"))?;
    Ok((parsed > 0).then_some(parsed))
}

pub(crate) fn help_requested(args: &[String]) -> bool {
    args.iter()
        .any(|value| value == "--help" || value == "-h" || value == "help")
}

pub(crate) fn positional(args: &mut Vec<String>) -> Option<String> {
    args.first()
        .filter(|value| !value.starts_with('-'))
        .cloned()
        .map(|value| {
            args.remove(0);
            value
        })
}

pub(crate) fn required_positional(
    args: &mut Vec<String>,
    label: &str,
    usage: &str,
) -> Result<String, String> {
    positional(args).ok_or_else(|| format!("{label} is required\n{usage}"))
}

pub(crate) fn take_flag(args: &mut Vec<String>, name: &str) -> Result<bool, String> {
    let positions = args
        .iter()
        .enumerate()
        .filter_map(|(index, value)| (value == name).then_some(index))
        .collect::<Vec<_>>();
    if positions.len() > 1 {
        return Err(format!("{name} may be specified only once"));
    }
    if let Some(index) = positions.first().copied() {
        args.remove(index);
        Ok(true)
    } else {
        Ok(false)
    }
}

pub(crate) fn take_option(args: &mut Vec<String>, name: &str) -> Result<Option<String>, String> {
    let positions = args
        .iter()
        .enumerate()
        .filter_map(|(index, value)| (value == name).then_some(index))
        .collect::<Vec<_>>();
    if positions.len() > 1 {
        return Err(format!("{name} may be specified only once"));
    }
    let Some(index) = positions.first().copied() else {
        return Ok(None);
    };
    if index + 1 >= args.len() || args[index + 1].starts_with("--") {
        return Err(format!("{name} requires a value"));
    }
    args.remove(index);
    Ok(Some(args.remove(index)))
}

pub(crate) fn take_all_options(args: &mut Vec<String>, name: &str) -> Result<Vec<String>, String> {
    let mut values = Vec::new();
    while let Some(index) = args.iter().position(|value| value == name) {
        if index + 1 >= args.len() || args[index + 1].starts_with("--") {
            return Err(format!("{name} requires a value"));
        }
        args.remove(index);
        values.push(args.remove(index));
    }
    Ok(values)
}

pub(crate) fn finish_args(args: &[String], usage: &str) -> Result<(), String> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "unknown or unexpected argument(s): {}\n{usage}",
            args.join(" ")
        ))
    }
}

pub(crate) fn reject_json_for_mutation(json: bool) -> Result<(), String> {
    if json {
        Err("--json is available for read-only commands only".into())
    } else {
        Ok(())
    }
}

pub(crate) fn print_json(value: &impl Serialize) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value)
            .map_err(|error| format!("cannot encode output: {error}"))?
    );
    Ok(())
}
