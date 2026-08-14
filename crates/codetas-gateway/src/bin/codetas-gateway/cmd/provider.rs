use super::*;
use codetas_gateway::discover_provider_models;
use codetas_gateway::provider_presets;
use codetas_gateway::test_provider_connection;
use codetas_gateway::CredentialCommand;
use codetas_gateway::CredentialSource;
use codetas_gateway::CredentialTransport;
use codetas_gateway::ProviderCredential;
use std::path::Path;
use std::process::{Command, Stdio};
pub(crate) async fn provider(arguments: &[String], config: &Path) -> Result<(), String> {
    let mut args = arguments.to_vec();
    if help_requested(&args) {
        println!("{PROVIDER_USAGE}");
        return Ok(());
    }
    let action = positional(&mut args).unwrap_or_else(|| "list".into());
    let json_output = take_flag(&mut args, "--json")?;
    match action.as_str() {
        "list" => {
            finish_args(&args, PROVIDER_USAGE)?;
            let settings = read_valid_settings(config)?;
            if json_output {
                print_json(&settings.providers)?;
            } else if settings.providers.is_empty() {
                println!("No providers configured.");
            } else {
                for item in settings.providers {
                    let flags = [
                        item.enabled.then_some("enabled"),
                        (settings.default_provider.as_deref() == Some(item.id.as_str()))
                            .then_some("default"),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(", ");
                    println!(
                        "{}  {:?}  {}  [{}]",
                        item.id, item.protocol, item.base_url, flags
                    );
                }
            }
            Ok(())
        }
        "presets" => {
            finish_args(&args, PROVIDER_USAGE)?;
            let presets = provider_presets();
            if json_output {
                print_json(&presets)
            } else {
                for item in presets {
                    let url = if item.requires_custom_url {
                        "<custom URL>"
                    } else {
                        item.base_url
                    };
                    println!("{}  {:?}  {}", item.id, item.protocol, url);
                }
                Ok(())
            }
        }
        "show" => {
            let id = required_positional(&mut args, "provider id", PROVIDER_USAGE)?;
            finish_args(&args, PROVIDER_USAGE)?;
            let settings = read_valid_settings(config)?;
            let item = provider_ref(&settings, &id)?;
            if json_output {
                print_json(item)
            } else {
                print_json(item)
            }
        }
        "add" => {
            reject_json_for_mutation(json_output)?;
            let preset_id = required_positional(&mut args, "preset id", PROVIDER_USAGE)?;
            let id = take_option(&mut args, "--id")?;
            let name = take_option(&mut args, "--name")?;
            let base_url = take_option(&mut args, "--base-url")?;
            let google_mode = take_option(&mut args, "--google-mode")?
                .map(|value| parse_google_mode(&value))
                .transpose()?;
            let project = take_option(&mut args, "--project")?;
            let location = take_option(&mut args, "--location")?;
            let azure_deployment = take_option(&mut args, "--azure-deployment")?;
            let azure_api_version = take_option(&mut args, "--azure-api-version")?;
            let transport = take_option(&mut args, "--transport")?
                .map(|value| parse_provider_transport(&value))
                .transpose()?;
            let kiro_profile_arn = take_option(&mut args, "--kiro-profile-arn")?;
            let set_default = take_flag(&mut args, "--set-default")?;
            let disabled = take_flag(&mut args, "--disabled")?;
            finish_args(&args, PROVIDER_USAGE)?;
            let preset = provider_presets()
                .into_iter()
                .find(|item| item.id == preset_id)
                .ok_or_else(|| format!("unknown provider preset: {preset_id}"))?;
            let mut item = preset.instantiate(base_url.as_deref())?;
            if let Some(id) = id {
                item.id = id;
            }
            if let Some(name) = name {
                item.name = name;
            }
            if let Some(mode) = google_mode {
                item.google_mode = mode;
            }
            if let Some(project) = project {
                item.project = Some(project);
            }
            if let Some(location) = location {
                item.location = Some(location);
            }
            if let Some(deployment) = azure_deployment {
                item.azure_deployment = Some(deployment);
            }
            if let Some(version) = azure_api_version {
                item.azure_api_version = Some(version);
            }
            if let Some(transport) = transport {
                item.transport = transport;
            }
            if let Some(profile_arn) = kiro_profile_arn {
                item.kiro_profile_arn = Some(profile_arn);
            }
            item.enabled = !disabled;
            let mut settings = read_valid_settings(config)?;
            if settings
                .providers
                .iter()
                .any(|provider| provider.id == item.id)
            {
                return Err(format!("provider already exists: {}", item.id));
            }
            let added_id = item.id.clone();
            settings.providers.push(item);
            if set_default {
                if disabled {
                    return Err("a disabled provider cannot be the default".into());
                }
                settings.default_provider = Some(added_id.clone());
            }
            save_settings(config, &settings)?;
            println!("Added provider {added_id}.");
            Ok(())
        }
        "edit" => {
            reject_json_for_mutation(json_output)?;
            let id = required_positional(&mut args, "provider id", PROVIDER_USAGE)?;
            let name = take_option(&mut args, "--name")?;
            let base_url = take_option(&mut args, "--base-url")?;
            let default_model = take_option(&mut args, "--default-model")?;
            let google_mode = take_option(&mut args, "--google-mode")?
                .map(|value| parse_google_mode(&value))
                .transpose()?;
            let project = take_option(&mut args, "--project")?;
            let location = take_option(&mut args, "--location")?;
            let azure_deployment = take_option(&mut args, "--azure-deployment")?;
            let azure_api_version = take_option(&mut args, "--azure-api-version")?;
            let transport = take_option(&mut args, "--transport")?
                .map(|value| parse_provider_transport(&value))
                .transpose()?;
            let kiro_profile_arn = take_option(&mut args, "--kiro-profile-arn")?;
            let responses_path = take_option(&mut args, "--responses-path")?;
            let realtime_ws_base_url = take_option(&mut args, "--realtime-ws-base-url")?;
            let stateless_responses = take_option(&mut args, "--stateless-responses")?
                .map(|value| parse_bool(&value, "--stateless-responses"))
                .transpose()?;
            let strip_model_bracket_suffix =
                take_option(&mut args, "--strip-model-bracket-suffix")?
                    .map(|value| parse_bool(&value, "--strip-model-bracket-suffix"))
                    .transpose()?;
            let private = take_option(&mut args, "--allow-private-network")?
                .map(|value| parse_bool(&value, "--allow-private-network"))
                .transpose()?;
            let discovery = take_option(&mut args, "--discovery")?
                .map(|value| parse_bool(&value, "--discovery"))
                .transpose()?;
            let mut settings = read_valid_settings(config)?;
            let item = provider_mut(&mut settings, &id)?;
            let credential = parse_credential_patch(&mut args, Some(&item.credential))?;
            finish_args(&args, PROVIDER_USAGE)?;
            if let Some(name) = name {
                item.name = name;
            }
            if let Some(base_url) = base_url {
                item.base_url = base_url;
            }
            if let Some(model) = default_model {
                item.default_model = clearable(model);
            }
            if let Some(mode) = google_mode {
                item.google_mode = mode;
            }
            if let Some(project) = project {
                item.project = clearable(project);
            }
            if let Some(location) = location {
                item.location = clearable(location);
            }
            if let Some(deployment) = azure_deployment {
                item.azure_deployment = clearable(deployment);
            }
            if let Some(version) = azure_api_version {
                item.azure_api_version = clearable(version);
            }
            if let Some(transport) = transport {
                item.transport = transport;
            }
            if let Some(profile_arn) = kiro_profile_arn {
                item.kiro_profile_arn = clearable(profile_arn);
            }
            if let Some(path) = responses_path {
                item.responses_path = clearable(path);
            }
            if let Some(url) = realtime_ws_base_url {
                item.realtime_ws_base_url = clearable(url);
            }
            if let Some(value) = stateless_responses {
                item.stateless_responses = value;
            }
            if let Some(value) = strip_model_bracket_suffix {
                item.strip_model_bracket_suffix = value;
            }
            if let Some(private) = private {
                item.allow_private_network = private;
            }
            if let Some(discovery) = discovery {
                item.discovery.enabled = discovery;
            }
            if let Some(credential) = credential {
                item.credential = credential;
            }
            save_settings(config, &settings)?;
            println!("Updated provider {id}.");
            Ok(())
        }
        "login" => {
            reject_json_for_mutation(json_output)?;
            let id = required_positional(&mut args, "provider id", PROVIDER_USAGE)?;
            let broker = take_option(&mut args, "--broker")?;
            finish_args(&args, PROVIDER_USAGE)?;
            interactive_provider_login(config, &id, broker.as_deref())
        }
        "enable" | "disable" => {
            reject_json_for_mutation(json_output)?;
            let id = required_positional(&mut args, "provider id", PROVIDER_USAGE)?;
            finish_args(&args, PROVIDER_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            provider_mut(&mut settings, &id)?.enabled = action == "enable";
            save_settings(config, &settings)?;
            println!(
                "{} provider {id}.",
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
            let id = required_positional(&mut args, "provider id", PROVIDER_USAGE)?;
            finish_args(&args, PROVIDER_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            let original = settings.providers.len();
            settings.providers.retain(|item| item.id != id);
            if settings.providers.len() == original {
                return Err(format!("unknown provider: {id}"));
            }
            if settings.default_provider.as_deref() == Some(&id) {
                settings.default_provider = None;
            }
            save_settings(config, &settings)?;
            println!("Removed provider {id}.");
            Ok(())
        }
        "set-default" => {
            reject_json_for_mutation(json_output)?;
            let id = required_positional(&mut args, "provider id", PROVIDER_USAGE)?;
            finish_args(&args, PROVIDER_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            let item = provider_ref(&settings, &id)?;
            if !item.enabled {
                return Err("default provider must be enabled".into());
            }
            settings.default_provider = Some(id.clone());
            save_settings(config, &settings)?;
            println!("Default provider set to {id}.");
            Ok(())
        }
        "test" => {
            let id = required_positional(&mut args, "provider id", PROVIDER_USAGE)?;
            finish_args(&args, PROVIDER_USAGE)?;
            let settings = read_valid_settings(config)?;
            let report = test_provider_connection(provider_ref(&settings, &id)?).await;
            if json_output {
                print_json(&report)
            } else {
                print_json(&report)
            }
        }
        "discover" => {
            let id = required_positional(&mut args, "provider id", PROVIDER_USAGE)?;
            let apply = take_flag(&mut args, "--apply")?;
            finish_args(&args, PROVIDER_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            let mut probe = provider_ref(&settings, &id)?.clone();
            probe.discovery.enabled = true;
            let discovered = discover_provider_models(&probe)
                .await
                .map_err(|error| error.to_string())?;
            if apply {
                let ids = discovered
                    .iter()
                    .map(|item| item.model_id.clone())
                    .collect::<Vec<_>>();
                provider_mut(&mut settings, &id)?.models = ids;
                settings.model_catalog.retain(|item| item.provider_id != id);
                settings.model_catalog.extend(discovered.clone());
                save_settings(config, &settings)?;
            }
            if json_output {
                print_json(&discovered)
            } else {
                for model in &discovered {
                    println!("{}/{}", model.provider_id, model.model_id);
                }
                if apply {
                    println!("Applied {} discovered models.", discovered.len());
                }
                Ok(())
            }
        }
        _ => Err(format!(
            "unknown provider action: {action}\n{PROVIDER_USAGE}"
        )),
    }
}

pub(crate) fn interactive_provider_login(
    config: &Path,
    provider_id: &str,
    requested_broker: Option<&str>,
) -> Result<(), String> {
    let mut settings = read_valid_settings(config)?;
    let item = provider_mut(&mut settings, provider_id)?;
    let broker = requested_broker.unwrap_or_else(|| match provider_id {
        "github-copilot" => "github-cli",
        "google-vertex" | "google-antigravity" => "gcloud",
        _ => "",
    });
    let (program_name, login_args, token_args, refresh_interval_ms): (
        &str,
        Vec<String>,
        Vec<String>,
        u64,
    ) = match broker {
        "github-cli" => (
            "gh",
            vec![
                "auth".into(),
                "login".into(),
                "--hostname".into(),
                "github.com".into(),
                "--web".into(),
                "--git-protocol".into(),
                "https".into(),
            ],
            vec!["auth".into(), "token".into(), "--hostname".into(), "github.com".into()],
            15 * 60 * 1_000,
        ),
        "gcloud" => (
            "gcloud",
            vec!["auth".into(), "login".into(), "--brief".into()],
            vec!["auth".into(), "print-access-token".into()],
            45 * 60 * 1_000,
        ),
        "" => {
            return Err(
                "このプロバイダーには既定のOAuth brokerがありません。--broker github-cli または --broker gcloud を指定してください"
                    .into(),
            )
        }
        _ => return Err("OAuth broker must be github-cli or gcloud".into()),
    };
    let program = find_executable(program_name)?;
    println!(
        "Starting {broker} login. The external CLI owns and stores the OAuth secret; CODETAS will not read the interactive output."
    );
    let status = Command::new(&program)
        .args(&login_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| format!("failed to start {program_name}: {error}"))?;
    if !status.success() {
        return Err(format!("{broker} login did not complete successfully"));
    }
    let verified = Command::new(&program)
        .args(&token_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("failed to verify {broker} login: {error}"))?;
    if !verified.success() {
        return Err(format!(
            "{broker} completed but no access token is available"
        ));
    }
    item.api_key_env = None;
    item.credential = ProviderCredential {
        source: CredentialSource::OAuth,
        reference: Some(broker.into()),
        transport: CredentialTransport::Bearer,
        header_name: None,
        command: Some(CredentialCommand {
            program: program.to_string_lossy().into_owned(),
            args: token_args,
            cwd: None,
            timeout_ms: 15_000,
            refresh_interval_ms,
        }),
    };
    save_settings(config, &settings)?;
    println!(
        "Configured provider {provider_id} to retrieve an access token from {broker} at runtime."
    );
    Ok(())
}
