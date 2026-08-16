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
                ..AccountReference::default()
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
            apply_account_control(&mut settings, &provider, &id, &action, None)?;
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
        "priority" => {
            reject_json_for_mutation(json_output)?;
            let provider = required_positional(&mut args, "provider id", ACCOUNT_USAGE)?;
            let id = required_positional(&mut args, "account id", ACCOUNT_USAGE)?;
            let value = required_positional(&mut args, "priority", ACCOUNT_USAGE)?
                .parse::<i16>().map_err(|_| "priority must be an i16")?;
            finish_args(&args, ACCOUNT_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            account_mut(&mut settings, &provider, &id)?.priority = value;
            save_settings(config, &settings)?;
            println!("Account priority for {provider}/{id}: {value}.");
            Ok(())
        }
        "pause" | "resume" | "pin" | "unpin" => {
            reject_json_for_mutation(json_output)?;
            let provider = required_positional(&mut args, "provider id", ACCOUNT_USAGE)?;
            let id = required_positional(&mut args, "account id", ACCOUNT_USAGE)?;
            let until = optional_u64(&mut args, "--until")?;
            if action != "pause" && until.is_some() { return Err("--until is only valid with pause".into()); }
            finish_args(&args, ACCOUNT_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            apply_account_control(&mut settings, &provider, &id, &action, until)?;
            save_settings(config, &settings)?;
            println!("Updated account {provider}/{id}: {action}.");
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

fn apply_account_control(
    settings: &mut codetas_gateway::GatewaySettings,
    provider: &str,
    id: &str,
    action: &str,
    until: Option<u64>,
) -> Result<(), String> {
    match action {
        "pin" => {
            let target = account_ref(settings, provider, id)?;
            if !target.enabled {
                return Err("cannot pin a disabled account".into());
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            if target.paused && target.pause_until_unix.is_none_or(|deadline| deadline > now) {
                return Err("cannot pin a paused account".into());
            }
            for account in settings
                .account_pool
                .accounts
                .iter_mut()
                .filter(|account| account.provider_id == provider)
            {
                account.pinned = account.id == id;
            }
        }
        "enable" => account_mut(settings, provider, id)?.enabled = true,
        "disable" => {
            {
                let account = account_mut(settings, provider, id)?;
                account.enabled = false;
                account.pinned = false;
            }
            if settings.account_pool.active_accounts.get(provider).is_some_and(|active| active == id) {
                settings.account_pool.active_accounts.remove(provider);
            }
        }
        "pause" => {
            {
                let account = account_mut(settings, provider, id)?;
                account.paused = true;
                account.pause_until_unix = until;
                account.pinned = false;
            }
            if settings.account_pool.active_accounts.get(provider).is_some_and(|active| active == id) {
                settings.account_pool.active_accounts.remove(provider);
            }
        }
        "resume" => {
            let account = account_mut(settings, provider, id)?;
            account.paused = false;
            account.pause_until_unix = None;
        }
        "unpin" => account_mut(settings, provider, id)?.pinned = false,
        _ => return Err(format!("unsupported account control action: {action}")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codetas_gateway::GatewaySettings;

    fn settings_with_pinned_account() -> GatewaySettings {
        let mut settings = GatewaySettings::default();
        settings.account_pool.accounts = vec![
            AccountReference {
                id: "primary".into(), provider_id: "provider".into(), label: "Primary".into(),
                pinned: true, ..AccountReference::default()
            },
            AccountReference {
                id: "backup".into(), provider_id: "provider".into(), label: "Backup".into(),
                ..AccountReference::default()
            },
        ];
        settings.account_pool.active_accounts.insert("provider".into(), "primary".into());
        settings
    }

    #[test]
    fn missing_pin_target_does_not_clear_existing_pin() {
        let mut settings = settings_with_pinned_account();
        assert!(apply_account_control(&mut settings, "provider", "missing", "pin", None).is_err());
        assert!(settings.account_pool.accounts[0].pinned);
        assert!(!settings.account_pool.accounts[1].pinned);
    }

    #[test]
    fn pause_and_disable_clear_pin_and_active_selection() {
        let mut paused = settings_with_pinned_account();
        apply_account_control(&mut paused, "provider", "primary", "pause", None).expect("pause");
        assert!(!paused.account_pool.accounts[0].pinned);
        assert!(!paused.account_pool.active_accounts.contains_key("provider"));

        let mut disabled = settings_with_pinned_account();
        apply_account_control(&mut disabled, "provider", "primary", "disable", None).expect("disable");
        assert!(!disabled.account_pool.accounts[0].pinned);
        assert!(!disabled.account_pool.accounts[0].enabled);
        assert!(!disabled.account_pool.active_accounts.contains_key("provider"));
    }
}
