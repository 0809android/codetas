use super::*;
use codetas_gateway::RouteDefinition;

use std::path::Path;
pub(crate) fn route(arguments: &[String], config: &Path) -> Result<(), String> {
    let mut args = arguments.to_vec();
    if help_requested(&args) {
        println!("{ROUTE_USAGE}");
        return Ok(());
    }
    let action = positional(&mut args).unwrap_or_else(|| "list".into());
    let json_output = take_flag(&mut args, "--json")?;
    match action.as_str() {
        "list" => {
            finish_args(&args, ROUTE_USAGE)?;
            let settings = read_valid_settings(config)?;
            if json_output {
                print_json(&settings.routes)
            } else {
                for item in settings.routes {
                    println!(
                        "{}  {}  {:?}  {} target(s)",
                        item.id,
                        item.alias.unwrap_or_else(|| "-".into()),
                        item.strategy,
                        item.targets.len()
                    );
                }
                Ok(())
            }
        }
        "show" => {
            let id = required_positional(&mut args, "route id", ROUTE_USAGE)?;
            finish_args(&args, ROUTE_USAGE)?;
            let settings = read_valid_settings(config)?;
            let item = route_ref(&settings, &id)?;
            if json_output {
                print_json(item)
            } else {
                print_json(item)
            }
        }
        "add" | "edit" => {
            reject_json_for_mutation(json_output)?;
            let id = required_positional(&mut args, "route id", ROUTE_USAGE)?;
            let name = take_option(&mut args, "--name")?;
            let alias = take_option(&mut args, "--alias")?;
            let strategy = take_option(&mut args, "--strategy")?
                .map(|value| parse_route_strategy(&value))
                .transpose()?;
            let effort = take_option(&mut args, "--default-effort")?;
            let targets = take_all_options(&mut args, "--target")?
                .into_iter()
                .map(|value| parse_route_target(&value))
                .collect::<Result<Vec<_>, _>>()?;
            finish_args(&args, ROUTE_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            if action == "add" {
                if settings.routes.iter().any(|item| item.id == id) {
                    return Err(format!("route already exists: {id}"));
                }
                settings.routes.push(RouteDefinition {
                    id: id.clone(),
                    name: name.ok_or("route add requires --name")?,
                    alias: alias.and_then(clearable),
                    strategy: strategy.unwrap_or_default(),
                    targets,
                    default_reasoning_effort: effort.and_then(clearable),
                    ..RouteDefinition::default()
                });
            } else {
                let item = route_mut(&mut settings, &id)?;
                if let Some(name) = name {
                    item.name = name;
                }
                if let Some(alias) = alias {
                    item.alias = clearable(alias);
                }
                if let Some(strategy) = strategy {
                    item.strategy = strategy;
                }
                if let Some(effort) = effort {
                    item.default_reasoning_effort = clearable(effort);
                }
                if !targets.is_empty() {
                    item.targets = targets;
                }
            }
            save_settings(config, &settings)?;
            println!(
                "{} route {id}.",
                if action == "add" { "Added" } else { "Updated" }
            );
            Ok(())
        }
        "enable" | "disable" => {
            reject_json_for_mutation(json_output)?;
            let id = required_positional(&mut args, "route id", ROUTE_USAGE)?;
            finish_args(&args, ROUTE_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            route_mut(&mut settings, &id)?.enabled = action == "enable";
            save_settings(config, &settings)?;
            println!(
                "{} route {id}.",
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
            let id = required_positional(&mut args, "route id", ROUTE_USAGE)?;
            finish_args(&args, ROUTE_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            let original = settings.routes.len();
            settings.routes.retain(|item| item.id != id);
            if settings.routes.len() == original {
                return Err(format!("unknown route: {id}"));
            }
            save_settings(config, &settings)?;
            println!("Removed route {id}.");
            Ok(())
        }
        _ => Err(format!("unknown route action: {action}\n{ROUTE_USAGE}")),
    }
}

