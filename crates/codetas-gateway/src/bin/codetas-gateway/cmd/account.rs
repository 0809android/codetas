use super::*;
use codetas_gateway::AccountPoolStrategy;
use codetas_gateway::AccountReference;

use std::path::Path;
pub(crate) fn account(arguments: &[String], config: &Path) -> Result<(), String> {
    let mut args = arguments.to_vec();
    if help_requested(&args) {
        println!("{ACCOUNT_USAGE}");
        return Ok(());
    }
    let action = positional(&mut args).unwrap_or_else(|| "list".into());
    let json_output = take_flag(&mut args, "--json")?;
    match action.as_str() {
        "list" => {
            let provider = positional(&mut args);
            finish_args(&args, ACCOUNT_USAGE)?;
            let settings = read_valid_settings(config)?;
            let items = settings
                .account_pool
                .accounts
                .iter()
                .cloned()
                .filter(|item| provider.as_deref().is_none_or(|id| item.provider_id == id))
                .collect::<Vec<_>>();
            if json_output {
                print_json(&items)
            } else {
                if items.is_empty() {
                    println!("No account references configured.");
                }
                for item in items {
                    let active = settings.account_pool.active_accounts.get(&item.provider_id)
                        == Some(&item.id);
                    println!(
                        "{}  {}  {}  {:?}{}",
                        item.provider_id,
                        item.id,
                        item.label,
                        item.credential.source,
                        if active { "  [active]" } else { "" }
                    );
                }
                Ok(())
            }
        }
        "show" => {
            let provider = required_positional(&mut args, "provider id", ACCOUNT_USAGE)?;
            let id = required_positional(&mut args, "account id", ACCOUNT_USAGE)?;
            finish_args(&args, ACCOUNT_USAGE)?;
            let settings = read_valid_settings(config)?;
            let item = account_ref(&settings, &provider, &id)?;
            if json_output {
                print_json(item)
            } else {
                print_json(item)
            }
        }
        "add" => {
            reject_json_for_mutation(json_output)?;
            let provider = required_positional(&mut args, "provider id", ACCOUNT_USAGE)?;
            let id = required_positional(&mut args, "account id", ACCOUNT_USAGE)?;
            let label = take_option(&mut args, "--label")?.ok_or("account add requires --label")?;
            let credential = parse_credential_patch(&mut args, None)?.unwrap_or_default();
            finish_args(&args, ACCOUNT_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            provider_ref(&settings, &provider)?;
            if settings
                .account_pool
                .accounts
                .iter()
                .any(|item| item.id == id)
            {
                return Err(format!("account id already exists: {id}"));
            }
            settings.account_pool.accounts.push(AccountReference {
                id: id.clone(),
                provider_id: provider.clone(),
                label,
                credential,
                enabled: true,
            });
            save_settings(config, &settings)?;
            println!("Added account reference {provider}/{id}.");
            Ok(())
        }
        "use" => {
            reject_json_for_mutation(json_output)?;
            let provider = required_positional(&mut args, "provider id", ACCOUNT_USAGE)?;
            let id = required_positional(&mut args, "account id or auto", ACCOUNT_USAGE)?;
            finish_args(&args, ACCOUNT_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            provider_ref(&settings, &provider)?;
            if id == "auto" {
                settings.account_pool.active_accounts.remove(&provider);
            } else {
                let item = account_ref(&settings, &provider, &id)?;
                if !item.enabled {
                    return Err("cannot select a disabled account".into());
                }
                settings
                    .account_pool
                    .active_accounts
                    .insert(provider.clone(), id.clone());
            }
            save_settings(config, &settings)?;
            println!("Account selection for {provider}: {id}.");
            Ok(())
        }
        "enable" | "disable" => {
            reject_json_for_mutation(json_output)?;
            let provider = required_positional(&mut args, "provider id", ACCOUNT_USAGE)?;
            let id = required_positional(&mut args, "account id", ACCOUNT_USAGE)?;
            finish_args(&args, ACCOUNT_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            account_mut(&mut settings, &provider, &id)?.enabled = action == "enable";
            save_settings(config, &settings)?;
            println!(
                "{} account {provider}/{id}.",
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
            let provider = required_positional(&mut args, "provider id", ACCOUNT_USAGE)?;
            let id = required_positional(&mut args, "account id", ACCOUNT_USAGE)?;
            finish_args(&args, ACCOUNT_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            let original = settings.account_pool.accounts.len();
            settings
                .account_pool
                .accounts
                .retain(|item| !(item.provider_id == provider && item.id == id));
            if settings.account_pool.accounts.len() == original {
                return Err(format!("unknown account: {provider}/{id}"));
            }
            if settings.account_pool.active_accounts.get(&provider) == Some(&id) {
                settings.account_pool.active_accounts.remove(&provider);
            }
            save_settings(config, &settings)?;
            println!("Removed account {provider}/{id}.");
            Ok(())
        }
        "strategy" => {
            reject_json_for_mutation(json_output)?;
            let strategy = required_positional(&mut args, "strategy", ACCOUNT_USAGE)?;
            finish_args(&args, ACCOUNT_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            settings.account_pool.strategy = match strategy.as_str() {
                "quota" => AccountPoolStrategy::Quota,
                "round-robin" => AccountPoolStrategy::RoundRobin,
                "fill-first" => AccountPoolStrategy::FillFirst,
                _ => return Err("strategy must be quota, round-robin, or fill-first".into()),
            };
            save_settings(config, &settings)?;
            println!("Account strategy set to {strategy}.");
            Ok(())
        }
        _ => Err(format!("unknown account action: {action}\n{ACCOUNT_USAGE}")),
    }
}

