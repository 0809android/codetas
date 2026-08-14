use super::*;
use codetas_gateway::build_codex_catalog;
use codetas_gateway::read_observability_summary;
use codetas_gateway::CredentialCommand;
use codetas_gateway::CredentialSource;
use codetas_gateway::CredentialTransport;
use codetas_gateway::ProviderCredential;
use serde_json::json;
use std::path::Path;

pub(crate) fn system(arguments: &[String], config: &Path) -> Result<(), String> {
    let mut args = arguments.to_vec();
    if help_requested(&args) {
        println!("{SYSTEM_USAGE}");
        return Ok(());
    }
    let action = positional(&mut args).unwrap_or_else(|| "status".into());
    let json_output = take_flag(&mut args, "--json")?;
    match action.as_str() {
        "status" => {
            finish_args(&args, SYSTEM_USAGE)?;
            let settings = read_valid_settings(config)?;
            let value = json!({
                "valid": true,
                "ready": settings.default_provider.as_deref().is_some_and(|id| settings.providers.iter().any(|item| item.id == id && item.enabled)),
                "host": settings.runtime.host,
                "port": settings.runtime.port,
                "providers": settings.providers.len(),
                "models": settings.model_catalog.len(),
                "routes": settings.routes.len(),
                "accounts": settings.account_pool.accounts.len(),
                "observability": read_observability_summary(config.parent().unwrap_or_else(|| Path::new(".")).join("observability")),
            });
            if json_output {
                print_json(&value)
            } else {
                print_json(&value)
            }
        }
        "validate" => {
            finish_args(&args, SYSTEM_USAGE)?;
            read_valid_settings(config)?;
            if json_output {
                print_json(&json!({"valid": true, "config": config}))
            } else {
                println!("CODETAS settings are valid: {}", config.display());
                Ok(())
            }
        }
        "catalog" => {
            finish_args(&args, SYSTEM_USAGE)?;
            let settings = read_valid_settings(config)?;
            let catalog = build_codex_catalog(&settings);
            if json_output {
                print_json(&catalog)
            } else {
                println!("{} catalog models.", catalog.models.len());
                Ok(())
            }
        }
        "settings" => {
            let host = take_option(&mut args, "--host")?;
            let port = optional_u16(&mut args, "--port")?;
            let auto_start = optional_bool(&mut args, "--auto-start")?;
            let standalone = optional_bool(&mut args, "--standalone-service")?;
            let allow_remote = optional_bool(&mut args, "--allow-remote")?;
            let require_token = optional_bool(&mut args, "--require-local-token")?;
            let dns = optional_bool(&mut args, "--dns-pinning")?;
            let request_log = optional_bool(&mut args, "--request-log")?;
            let usage_log = optional_bool(&mut args, "--usage-log")?;
            let retention = optional_u16(&mut args, "--retention-days")?;
            let budget = optional_u64(&mut args, "--max-storage-bytes")?;
            finish_args(&args, SYSTEM_USAGE)?;
            if [
                host.is_some(),
                port.is_some(),
                auto_start.is_some(),
                standalone.is_some(),
                allow_remote.is_some(),
                require_token.is_some(),
                dns.is_some(),
                request_log.is_some(),
                usage_log.is_some(),
                retention.is_some(),
                budget.is_some(),
            ]
            .into_iter()
            .all(|value| !value)
            {
                let settings = read_valid_settings(config)?;
                return if json_output {
                    print_json(
                        &json!({"runtime": settings.runtime, "security": settings.security, "observability": settings.observability}),
                    )
                } else {
                    print_json(
                        &json!({"runtime": settings.runtime, "security": settings.security, "observability": settings.observability}),
                    )
                };
            }
            reject_json_for_mutation(json_output)?;
            let mut settings = read_valid_settings(config)?;
            if let Some(value) = host {
                settings.runtime.host = value;
            }
            if let Some(value) = port {
                settings.runtime.port = value;
            }
            if let Some(value) = auto_start {
                settings.runtime.auto_start = value;
            }
            if let Some(value) = standalone {
                settings.runtime.standalone_service = value;
            }
            if let Some(value) = allow_remote {
                settings.security.allow_remote = value;
            }
            if let Some(value) = require_token {
                settings.security.require_local_token = value;
            }
            if let Some(value) = dns {
                settings.security.dns_pinning = value;
            }
            if let Some(value) = request_log {
                settings.observability.request_log = value;
            }
            if let Some(value) = usage_log {
                settings.observability.usage_log = value;
            }
            if let Some(value) = retention {
                settings.observability.retention_days = value;
            }
            if let Some(value) = budget {
                settings.observability.max_storage_bytes = value;
            }
            save_settings(config, &settings)?;
            println!("Updated system settings.");
            Ok(())
        }
        _ => Err(format!("unknown system action: {action}\n{SYSTEM_USAGE}")),
    }
}

pub(crate) fn parse_credential_patch(
    args: &mut Vec<String>,
    current: Option<&ProviderCredential>,
) -> Result<Option<ProviderCredential>, String> {
    let env = take_option(args, "--credential-env")?;
    let keychain = take_option(args, "--credential-keychain")?;
    let oauth = take_option(args, "--credential-oauth")?;
    let command = take_option(args, "--credential-command")?;
    let forward = take_flag(args, "--credential-forward")?;
    let none = take_flag(args, "--credential-none")?;
    let command_args = take_all_options(args, "--credential-arg")?;
    let transport = take_option(args, "--credential-transport")?;
    let header_name = take_option(args, "--credential-header")?;
    let selected = [
        env.is_some(),
        keychain.is_some(),
        oauth.is_some(),
        command.is_some(),
        forward,
        none,
    ]
    .into_iter()
    .filter(|value| *value)
    .count();
    let has_patch =
        selected > 0 || transport.is_some() || header_name.is_some() || !command_args.is_empty();
    if !has_patch {
        return Ok(None);
    }
    if selected > 1 {
        return Err("select only one credential source".into());
    }
    let mut value = current.cloned().unwrap_or_default();
    if let Some(reference) = env {
        value.source = CredentialSource::Environment;
        value.reference = Some(reference);
        value.command = None;
    } else if let Some(reference) = keychain {
        value.source = CredentialSource::Keychain;
        value.reference = Some(reference);
        value.command = None;
    } else if let Some(reference) = oauth {
        value.source = CredentialSource::OAuth;
        value.reference = Some(reference);
        value.command = None;
    } else if let Some(program) = command {
        value.source = CredentialSource::Command;
        value.reference = None;
        value.command = Some(CredentialCommand {
            program,
            args: command_args.clone(),
            ..CredentialCommand::default()
        });
    } else if forward {
        value = ProviderCredential {
            source: CredentialSource::Forward,
            ..ProviderCredential::default()
        };
    } else if none {
        value = ProviderCredential::default();
    } else if !command_args.is_empty() {
        let command = value.command.as_mut().ok_or(
            "--credential-arg requires --credential-command or an existing command credential",
        )?;
        command.args = command_args;
    }
    if let Some(transport) = transport {
        value.transport = match transport.as_str() {
            "bearer" => CredentialTransport::Bearer,
            "x-api-key" => CredentialTransport::XApiKey,
            "custom-header" => CredentialTransport::CustomHeader,
            _ => {
                return Err(
                    "credential transport must be bearer, x-api-key, or custom-header".into(),
                )
            }
        };
    }
    if header_name.is_some() {
        value.header_name = header_name;
    }
    Ok(Some(value))
}
