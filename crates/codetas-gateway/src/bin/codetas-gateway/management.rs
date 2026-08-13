use super::{atomic_write_new_or_owned, backup_path, read_valid_settings, secure_existing_file};
use codetas_gateway::{
    build_codex_catalog, discover_provider_models, provider_presets, read_observability_breakdown,
    read_observability_summary, read_recent_observability_events, test_provider_connection,
    AccountPoolStrategy, AccountReference, AgentSurfaceMode, CredentialCommand, CredentialSource,
    CredentialTransport, ExternalAccessKey, GatewaySettings, GoogleMode, ModelMetadata,
    ProviderCredential, ProviderTransport, RouteDefinition, RouteStrategy, RouteTarget,
};
use serde::Serialize;
use serde_json::json;
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

const PROVIDER_USAGE: &str = "Usage:
  codetas-gateway provider <list|presets> [--json]
  codetas-gateway provider show <id> [--json]
  codetas-gateway provider add <preset> [--id <id>] [--name <name>] [--base-url <url>] [--set-default] [--disabled]
      [--google-mode <ai-studio|vertex|cloud-code-assist>] [--project <id>] [--location <id>] [--azure-deployment <id>] [--azure-api-version <version>]
      [--transport <standard|kiro|github-copilot>] [--kiro-profile-arn <arn>]
  codetas-gateway provider edit <id> [--name <name>] [--base-url <url>] [--default-model <id|->]
      [--google-mode <ai-studio|vertex|cloud-code-assist>] [--project <id|->] [--location <id|->] [--azure-deployment <id|->] [--azure-api-version <version|->]
      [--transport <standard|kiro|github-copilot>] [--kiro-profile-arn <arn|->]
      [--responses-path <path|->] [--realtime-ws-base-url <url|->]
      [--stateless-responses <on|off>] [--strip-model-bracket-suffix <on|off>]
      [--credential-env <VAR>|--credential-keychain <ref>|--credential-oauth <ref>|--credential-command <program>|--credential-forward|--credential-none]
      [--credential-arg <value> ...] [--credential-transport <bearer|x-api-key|custom-header>] [--credential-header <name>]
      [--allow-private-network <on|off>] [--discovery <on|off>]
  codetas-gateway provider <enable|disable|remove|set-default|test> <id> [--json]
  codetas-gateway provider discover <id> [--apply] [--json]
  codetas-gateway provider login <id> [--broker <github-cli|gcloud>]
      # OAuthの秘密は外部CLIが所有し、CODETASはtoken取得コマンドだけを保存";

const ACCOUNT_USAGE: &str = "Usage:
  codetas-gateway account list [provider] [--json]
  codetas-gateway account show <provider> <id> [--json]
  codetas-gateway account add <provider> <id> --label <label>
      [--credential-env <VAR>|--credential-keychain <ref>|--credential-oauth <ref>|--credential-command <program>|--credential-forward|--credential-none]
      [--credential-arg <value> ...] [--credential-transport <bearer|x-api-key|custom-header>] [--credential-header <name>]
  codetas-gateway account use <provider> <id|auto>
  codetas-gateway account <enable|disable|remove> <provider> <id>
  codetas-gateway account strategy <quota|round-robin|fill-first>";

const MODELS_USAGE: &str = "Usage:
  codetas-gateway models list [provider] [--json]
  codetas-gateway models catalog [--json]
  codetas-gateway models add <provider> <model> [--display-name <name>] [--context-window <tokens>] [--max-output-tokens <tokens>] [--modalities <csv>]
  codetas-gateway models edit <provider> <model> [--display-name <name|->] [--context-window <tokens|0>] [--max-output-tokens <tokens|0>] [--modalities <csv|->]
  codetas-gateway models <enable|disable|remove> <provider> <model>
  codetas-gateway models selected <provider> [--set <csv>|--clear]
  codetas-gateway models sync <provider|all> [--json]";

const ROUTE_USAGE: &str = "Usage:
  codetas-gateway route list [--json]
  codetas-gateway route show <id> [--json]
  codetas-gateway route add <id> --name <name> --target <provider/model[@weight]> ... [--alias <name>] [--strategy <failover|weighted-round-robin|least-usage>] [--default-effort <level>]
  codetas-gateway route edit <id> [--name <name>] [--alias <name|->] [--target <provider/model[@weight]> ...] [--strategy <...>] [--default-effort <level|->]
  codetas-gateway route <enable|disable|remove> <id>";

const AGENT_USAGE: &str = "Usage:
  codetas-gateway agent [status] [--json]
  codetas-gateway agent set [--surface <v1|default|v2>] [--threads <n>] [--multi-agent-v2 <on|off>]
      [--main-effort <level|->] [--subagent-effort <level|->] [--subagents <csv|->] [--fallback <csv|->]
      [--web-search-model <id|->] [--vision-model <id|->] [--image-model <id|->] [--video-model <id|->] [--live-model <id|->]
      [--helper-intercept <on|off>] [--helper-target <id|->] [--helper-sources <csv>]";

const ACCESS_USAGE: &str = "Usage:
  codetas-gateway access key list [--json]
  codetas-gateway access key add <id> --label <label> --env <VAR> --scopes <csv> [--expires-at <unix-seconds>]
  codetas-gateway access key <enable|disable|remove> <id>
  codetas-gateway access endpoints [--json]";

const OBSERVE_USAGE: &str = "Usage:
  codetas-gateway observe <summary|usage> [--json]
  codetas-gateway observe breakdown [--since-ms <timestamp>] [--limit <1-50000>] [--json]
  codetas-gateway observe events [--since-ms <timestamp>] [--limit <1-500>] [--provider <id>] [--model <id>] [--status <code>] [--json|--jsonl]
  codetas-gateway observe follow [--since-ms <timestamp>] [--interval-ms <250-60000>] [--provider <id>] [--model <id>] [--status <code>] [--jsonl]";

const SYSTEM_USAGE: &str = "Usage:
  codetas-gateway system <status|validate|catalog> [--json]
  codetas-gateway system settings [--host <host>] [--port <port>] [--auto-start <on|off>] [--standalone-service <on|off>]
      [--allow-remote <on|off>] [--require-local-token <on|off>] [--dns-pinning <on|off>]
      [--request-log <on|off>] [--usage-log <on|off>] [--retention-days <days>] [--max-storage-bytes <bytes>]";

pub async fn run(group: &str, arguments: &[String], config: &Path) -> Result<(), String> {
    match group {
        "provider" => provider(arguments, config).await,
        "account" => account(arguments, config),
        "models" | "model" => models(arguments, config).await,
        "route" => route(arguments, config),
        "agent" => agent(arguments, config),
        "access" => access(arguments, config),
        "observe" => observe(arguments, config),
        "system" => system(arguments, config),
        _ => Err(format!("unknown management group: {group}")),
    }
}

async fn provider(arguments: &[String], config: &Path) -> Result<(), String> {
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

fn interactive_provider_login(
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

fn find_executable(name: &str) -> Result<PathBuf, String> {
    let path = Path::new(name);
    if path.is_absolute() && path.is_file() {
        return fs::canonicalize(path).map_err(|error| format!("cannot resolve {name}: {error}"));
    }
    let search = std::env::var_os("PATH").ok_or_else(|| "PATH is unavailable".to_string())?;
    for directory in std::env::split_paths(&search) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return fs::canonicalize(&candidate)
                .map_err(|error| format!("cannot resolve {}: {error}", candidate.display()));
        }
        #[cfg(windows)]
        for extension in ["exe", "cmd", "bat"] {
            let candidate = directory.join(format!("{name}.{extension}"));
            if candidate.is_file() {
                return fs::canonicalize(&candidate)
                    .map_err(|error| format!("cannot resolve {}: {error}", candidate.display()));
            }
        }
    }
    Err(format!("{name} was not found on PATH"))
}

fn parse_google_mode(value: &str) -> Result<GoogleMode, String> {
    match value {
        "ai-studio" | "aiStudio" => Ok(GoogleMode::AiStudio),
        "vertex" => Ok(GoogleMode::Vertex),
        "cloud-code-assist" | "cloudCodeAssist" => Ok(GoogleMode::CloudCodeAssist),
        _ => Err("--google-mode must be ai-studio, vertex, or cloud-code-assist".into()),
    }
}

fn parse_provider_transport(value: &str) -> Result<ProviderTransport, String> {
    match value {
        "standard" => Ok(ProviderTransport::Standard),
        "kiro" => Ok(ProviderTransport::Kiro),
        "github-copilot" => Ok(ProviderTransport::GithubCopilot),
        _ => Err("provider transport must be standard, kiro, or github-copilot".into()),
    }
}

fn account(arguments: &[String], config: &Path) -> Result<(), String> {
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

async fn models(arguments: &[String], config: &Path) -> Result<(), String> {
    let mut args = arguments.to_vec();
    if help_requested(&args) {
        println!("{MODELS_USAGE}");
        return Ok(());
    }
    let action = positional(&mut args).unwrap_or_else(|| "list".into());
    let json_output = take_flag(&mut args, "--json")?;
    match action.as_str() {
        "list" => {
            let provider = positional(&mut args);
            finish_args(&args, MODELS_USAGE)?;
            let settings = read_valid_settings(config)?;
            let items = model_rows(&settings, provider.as_deref());
            if json_output {
                print_json(&items)
            } else {
                for item in items {
                    println!(
                        "{}  {}",
                        item["id"].as_str().unwrap_or_default(),
                        if item["enabled"].as_bool().unwrap_or(false) {
                            "enabled"
                        } else {
                            "disabled"
                        }
                    );
                }
                Ok(())
            }
        }
        "catalog" => {
            finish_args(&args, MODELS_USAGE)?;
            let settings = read_valid_settings(config)?;
            let catalog = build_codex_catalog(&settings);
            if json_output {
                print_json(&catalog)
            } else {
                for item in catalog.models {
                    println!(
                        "{}",
                        item.get("slug")
                            .and_then(|value| value.as_str())
                            .unwrap_or_default()
                    );
                }
                Ok(())
            }
        }
        "add" => {
            reject_json_for_mutation(json_output)?;
            let provider = required_positional(&mut args, "provider id", MODELS_USAGE)?;
            let model = required_positional(&mut args, "model id", MODELS_USAGE)?;
            let display_name = take_option(&mut args, "--display-name")?;
            let context_window = optional_u64(&mut args, "--context-window")?;
            let max_output_tokens = optional_u64(&mut args, "--max-output-tokens")?;
            let modalities = take_option(&mut args, "--modalities")?.map(|value| csv(&value));
            finish_args(&args, MODELS_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            let capabilities = provider_ref(&settings, &provider)?.capabilities.clone();
            if settings
                .model_catalog
                .iter()
                .any(|item| item.provider_id == provider && item.model_id == model)
            {
                return Err(format!("model metadata already exists: {provider}/{model}"));
            }
            let provider_item = provider_mut(&mut settings, &provider)?;
            if !provider_item.models.contains(&model) {
                provider_item.models.push(model.clone());
            }
            settings.model_catalog.push(ModelMetadata {
                provider_id: provider.clone(),
                model_id: model.clone(),
                display_name,
                enabled: true,
                context_window,
                max_input_tokens: None,
                max_output_tokens,
                input_modalities: modalities.unwrap_or_else(|| vec!["text".into()]),
                reasoning_efforts: Vec::new(),
                default_reasoning_effort: None,
                capabilities,
                input_price_per_million: None,
                output_price_per_million: None,
            });
            save_settings(config, &settings)?;
            println!("Added model {provider}/{model}.");
            Ok(())
        }
        "edit" => {
            reject_json_for_mutation(json_output)?;
            let provider = required_positional(&mut args, "provider id", MODELS_USAGE)?;
            let model = required_positional(&mut args, "model id", MODELS_USAGE)?;
            let display_name = take_option(&mut args, "--display-name")?;
            let context = take_option(&mut args, "--context-window")?
                .map(|value| parse_clearable_u64(&value, "--context-window"))
                .transpose()?;
            let output = take_option(&mut args, "--max-output-tokens")?
                .map(|value| parse_clearable_u64(&value, "--max-output-tokens"))
                .transpose()?;
            let modalities = take_option(&mut args, "--modalities")?;
            finish_args(&args, MODELS_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            let item = model_mut(&mut settings, &provider, &model)?;
            if let Some(name) = display_name {
                item.display_name = clearable(name);
            }
            if let Some(value) = context {
                item.context_window = value;
            }
            if let Some(value) = output {
                item.max_output_tokens = value;
            }
            if let Some(value) = modalities {
                item.input_modalities = if value == "-" {
                    Vec::new()
                } else {
                    csv(&value)
                };
            }
            save_settings(config, &settings)?;
            println!("Updated model {provider}/{model}.");
            Ok(())
        }
        "enable" | "disable" => {
            reject_json_for_mutation(json_output)?;
            let provider = required_positional(&mut args, "provider id", MODELS_USAGE)?;
            let model = required_positional(&mut args, "model id", MODELS_USAGE)?;
            finish_args(&args, MODELS_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            model_mut(&mut settings, &provider, &model)?.enabled = action == "enable";
            save_settings(config, &settings)?;
            println!(
                "{} model {provider}/{model}.",
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
            let provider = required_positional(&mut args, "provider id", MODELS_USAGE)?;
            let model = required_positional(&mut args, "model id", MODELS_USAGE)?;
            finish_args(&args, MODELS_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            let selected = provider_ref(&settings, &provider)?.models.contains(&model);
            let original = settings.model_catalog.len();
            settings
                .model_catalog
                .retain(|item| !(item.provider_id == provider && item.model_id == model));
            if let Ok(item) = provider_mut(&mut settings, &provider) {
                item.models.retain(|value| value != &model);
                if item.default_model.as_deref() == Some(&model) {
                    item.default_model = None;
                }
            }
            if settings.model_catalog.len() == original && !selected {
                return Err(format!("unknown model metadata: {provider}/{model}"));
            }
            save_settings(config, &settings)?;
            println!("Removed model {provider}/{model}.");
            Ok(())
        }
        "selected" => {
            reject_json_for_mutation(json_output)?;
            let provider = required_positional(&mut args, "provider id", MODELS_USAGE)?;
            let selected = take_option(&mut args, "--set")?;
            let clear = take_flag(&mut args, "--clear")?;
            if selected.is_some() == clear {
                return Err("selected requires exactly one of --set or --clear".into());
            }
            finish_args(&args, MODELS_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            provider_mut(&mut settings, &provider)?.models =
                selected.map(|value| csv(&value)).unwrap_or_default();
            save_settings(config, &settings)?;
            println!("Updated selected models for {provider}.");
            Ok(())
        }
        "sync" => {
            let target = required_positional(&mut args, "provider id or all", MODELS_USAGE)?;
            finish_args(&args, MODELS_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            let providers = settings
                .providers
                .iter()
                .filter(|item| target == "all" || item.id == target)
                .cloned()
                .collect::<Vec<_>>();
            if providers.is_empty() {
                return Err(format!("unknown provider: {target}"));
            }
            let mut result = Vec::new();
            for mut provider in providers {
                provider.discovery.enabled = true;
                let discovered = discover_provider_models(&provider)
                    .await
                    .map_err(|error| format!("{}: {error}", provider.id))?;
                provider_mut(&mut settings, &provider.id)?.models = discovered
                    .iter()
                    .map(|item| item.model_id.clone())
                    .collect();
                settings
                    .model_catalog
                    .retain(|item| item.provider_id != provider.id);
                settings.model_catalog.extend(discovered.clone());
                result.extend(discovered);
            }
            save_settings(config, &settings)?;
            if json_output {
                print_json(&result)
            } else {
                println!("Synchronized {} models.", result.len());
                Ok(())
            }
        }
        _ => Err(format!("unknown models action: {action}\n{MODELS_USAGE}")),
    }
}

fn route(arguments: &[String], config: &Path) -> Result<(), String> {
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

fn agent(arguments: &[String], config: &Path) -> Result<(), String> {
    let mut args = arguments.to_vec();
    if help_requested(&args) {
        println!("{AGENT_USAGE}");
        return Ok(());
    }
    let action = positional(&mut args).unwrap_or_else(|| "status".into());
    let json_output = take_flag(&mut args, "--json")?;
    if action == "status" || action == "show" {
        finish_args(&args, AGENT_USAGE)?;
        let settings = read_valid_settings(config)?;
        let value = json!({
            "agents": settings.agents,
            "sidecars": settings.sidecars,
            "helperIntercept": settings.helper_intercept,
        });
        return if json_output {
            print_json(&value)
        } else {
            print_json(&value)
        };
    }
    if action != "set" {
        return Err(format!("unknown agent action: {action}\n{AGENT_USAGE}"));
    }
    reject_json_for_mutation(json_output)?;
    let surface = take_option(&mut args, "--surface")?
        .map(|value| parse_surface(&value))
        .transpose()?;
    let threads = optional_u16(&mut args, "--threads")?;
    let multi_agent = take_option(&mut args, "--multi-agent-v2")?
        .map(|value| parse_bool(&value, "--multi-agent-v2"))
        .transpose()?;
    let main_effort = take_option(&mut args, "--main-effort")?;
    let subagent_effort = take_option(&mut args, "--subagent-effort")?;
    let subagents = take_option(&mut args, "--subagents")?;
    let fallback = take_option(&mut args, "--fallback")?;
    let web = take_option(&mut args, "--web-search-model")?;
    let vision = take_option(&mut args, "--vision-model")?;
    let image = take_option(&mut args, "--image-model")?;
    let video = take_option(&mut args, "--video-model")?;
    let live = take_option(&mut args, "--live-model")?;
    let helper_enabled = take_option(&mut args, "--helper-intercept")?
        .map(|value| parse_bool(&value, "--helper-intercept"))
        .transpose()?;
    let helper_target = take_option(&mut args, "--helper-target")?;
    let helper_sources = take_option(&mut args, "--helper-sources")?;
    finish_args(&args, AGENT_USAGE)?;
    let mut settings = read_valid_settings(config)?;
    if let Some(value) = surface {
        settings.agents.surface_mode = value;
    }
    if let Some(value) = threads {
        settings.agents.max_threads = value;
    }
    if let Some(value) = multi_agent {
        settings.agents.multi_agent_v2 = value;
    }
    if let Some(value) = main_effort {
        settings.agents.effort_cap = clearable(value);
    }
    if let Some(value) = subagent_effort {
        settings.agents.subagent_effort_cap = clearable(value);
    }
    if let Some(value) = subagents {
        settings.agents.subagent_models = clearable_csv(value);
    }
    if let Some(value) = fallback {
        settings.agents.subagent_fallback = clearable_csv(value);
    }
    if let Some(value) = web {
        settings.sidecars.web_search_model = clearable(value);
    }
    if let Some(value) = vision {
        settings.sidecars.vision_model = clearable(value);
    }
    if let Some(value) = image {
        settings.sidecars.image_model = clearable(value);
    }
    if let Some(value) = video {
        settings.sidecars.video_model = clearable(value);
    }
    if let Some(value) = live {
        settings.sidecars.live_model = clearable(value);
    }
    if let Some(value) = helper_enabled {
        settings.helper_intercept.enabled = value;
    }
    if let Some(value) = helper_target {
        settings.helper_intercept.target_model = clearable(value);
    }
    if let Some(value) = helper_sources {
        settings.helper_intercept.source_models = csv(&value);
    }
    save_settings(config, &settings)?;
    println!("Updated agent settings.");
    Ok(())
}

fn access(arguments: &[String], config: &Path) -> Result<(), String> {
    let mut args = arguments.to_vec();
    if help_requested(&args) {
        println!("{ACCESS_USAGE}");
        return Ok(());
    }
    let area = positional(&mut args).unwrap_or_else(|| "key".into());
    let json_output = take_flag(&mut args, "--json")?;
    if area == "endpoints" {
        finish_args(&args, ACCESS_USAGE)?;
        let settings = read_valid_settings(config)?;
        let base = format!("http://{}:{}", settings.runtime.host, settings.runtime.port);
        let value = json!({
            "baseUrl": base.clone(),
            "responsesEndpoint": format!("{base}/v1/responses"),
            "chatEndpoint": format!("{base}/v1/chat/completions"),
            "messagesEndpoint": format!("{base}/v1/messages"),
            "geminiEndpointTemplate": format!("{base}/v1beta/models/{{model}}:generateContent"),
            "geminiStreamEndpointTemplate": format!("{base}/v1beta/models/{{model}}:streamGenerateContent"),
            "geminiModelsEndpoint": format!("{base}/v1beta/models"),
            "modelsEndpoint": format!("{base}/v1/models"),
            "websocketEndpoint": format!("ws://{}:{}/v1/responses", settings.runtime.host, settings.runtime.port),
            "sidecarEndpoints": {
                "webSearch": format!("{base}/v1/sidecars/web-search"),
                "vision": format!("{base}/v1/sidecars/vision"),
                "image": format!("{base}/v1/sidecars/image"),
                "video": format!("{base}/v1/sidecars/video"),
                "videoStatusTemplate": format!("{base}/v1/sidecars/video/{{requestId}}"),
            },
        });
        return if json_output {
            print_json(&value)
        } else {
            print_json(&value)
        };
    }
    if area != "key" && area != "keys" {
        return Err(format!("unknown access area: {area}\n{ACCESS_USAGE}"));
    }
    let action = positional(&mut args).unwrap_or_else(|| "list".into());
    match action.as_str() {
        "list" => {
            finish_args(&args, ACCESS_USAGE)?;
            let settings = read_valid_settings(config)?;
            if json_output {
                print_json(&settings.security.external_access_keys)
            } else {
                for item in settings.security.external_access_keys {
                    println!(
                        "{}  {}  {}  {}",
                        item.id,
                        item.label,
                        item.env_var,
                        item.scopes.join(",")
                    );
                }
                Ok(())
            }
        }
        "add" => {
            reject_json_for_mutation(json_output)?;
            let id = required_positional(&mut args, "key id", ACCESS_USAGE)?;
            let label = take_option(&mut args, "--label")?.ok_or("key add requires --label")?;
            let env_var = take_option(&mut args, "--env")?.ok_or("key add requires --env")?;
            let scopes =
                csv(&take_option(&mut args, "--scopes")?.ok_or("key add requires --scopes")?);
            let expires_at_unix = optional_u64(&mut args, "--expires-at")?;
            finish_args(&args, ACCESS_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            if settings
                .security
                .external_access_keys
                .iter()
                .any(|item| item.id == id)
            {
                return Err(format!("access key id already exists: {id}"));
            }
            settings
                .security
                .external_access_keys
                .push(ExternalAccessKey {
                    id: id.clone(),
                    label,
                    env_var,
                    scopes,
                    enabled: true,
                    expires_at_unix,
                });
            save_settings(config, &settings)?;
            println!("Added environment-backed access key {id}.");
            Ok(())
        }
        "enable" | "disable" => {
            reject_json_for_mutation(json_output)?;
            let id = required_positional(&mut args, "key id", ACCESS_USAGE)?;
            finish_args(&args, ACCESS_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            access_key_mut(&mut settings, &id)?.enabled = action == "enable";
            save_settings(config, &settings)?;
            println!(
                "{} access key {id}.",
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
            let id = required_positional(&mut args, "key id", ACCESS_USAGE)?;
            finish_args(&args, ACCESS_USAGE)?;
            let mut settings = read_valid_settings(config)?;
            let original = settings.security.external_access_keys.len();
            settings
                .security
                .external_access_keys
                .retain(|item| item.id != id);
            if settings.security.external_access_keys.len() == original {
                return Err(format!("unknown access key: {id}"));
            }
            save_settings(config, &settings)?;
            println!("Removed access key {id}.");
            Ok(())
        }
        _ => Err(format!(
            "unknown access key action: {action}\n{ACCESS_USAGE}"
        )),
    }
}

fn observe(arguments: &[String], config: &Path) -> Result<(), String> {
    let mut args = arguments.to_vec();
    if help_requested(&args) {
        println!("{OBSERVE_USAGE}");
        return Ok(());
    }
    let action = positional(&mut args).unwrap_or_else(|| "summary".into());
    let json_output = take_flag(&mut args, "--json")?;
    let directory = config
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("observability");
    match action.as_str() {
        "summary" | "usage" => {
            finish_args(&args, OBSERVE_USAGE)?;
            let value = read_observability_summary(directory);
            if json_output {
                print_json(&value)
            } else {
                print_json(&value)
            }
        }
        "events" | "logs" => {
            let since = optional_u64(&mut args, "--since-ms")?.unwrap_or(0);
            let limit = optional_usize(&mut args, "--limit")?
                .unwrap_or(200)
                .clamp(1, 500);
            let provider = take_option(&mut args, "--provider")?;
            let model = take_option(&mut args, "--model")?;
            let status = take_option(&mut args, "--status")?
                .map(|value| {
                    value
                        .parse::<u16>()
                        .map_err(|_| "--status must be an HTTP status code".to_string())
                })
                .transpose()?;
            let jsonl = take_flag(&mut args, "--jsonl")?;
            if json_output && jsonl {
                return Err("--json and --jsonl cannot be combined".into());
            }
            finish_args(&args, OBSERVE_USAGE)?;
            let events = read_recent_observability_events(directory, since, 500)
                .into_iter()
                .filter(|item| {
                    provider
                        .as_deref()
                        .is_none_or(|value| item.provider_id.as_deref() == Some(value))
                })
                .filter(|item| {
                    model.as_deref().is_none_or(|value| {
                        item.exposed_model == value || item.upstream_model.as_deref() == Some(value)
                    })
                })
                .filter(|item| status.is_none_or(|value| item.status_code == value))
                .take(limit)
                .collect::<Vec<_>>();
            if jsonl {
                for item in events {
                    println!(
                        "{}",
                        serde_json::to_string(&item).map_err(|error| error.to_string())?
                    );
                }
                Ok(())
            } else if json_output {
                print_json(&events)
            } else {
                for item in events {
                    println!(
                        "{}  {}  {}  {}ms  {}",
                        item.timestamp_ms,
                        item.status_code,
                        item.exposed_model,
                        item.latency_ms,
                        item.outcome
                    );
                }
                Ok(())
            }
        }
        "breakdown" => {
            let since = optional_u64(&mut args, "--since-ms")?.unwrap_or(0);
            let limit = optional_usize(&mut args, "--limit")?
                .unwrap_or(50_000)
                .clamp(1, 50_000);
            finish_args(&args, OBSERVE_USAGE)?;
            let value = read_observability_breakdown(directory, since, limit);
            if json_output {
                print_json(&value)
            } else {
                print_json(&value)
            }
        }
        "follow" => {
            if json_output {
                return Err(
                    "observe follow is an unbounded stream; use --jsonl instead of --json".into(),
                );
            }
            let since = optional_u64(&mut args, "--since-ms")?.unwrap_or_else(now_millis);
            let interval = optional_u64(&mut args, "--interval-ms")?
                .unwrap_or(1_000)
                .clamp(250, 60_000);
            let provider = take_option(&mut args, "--provider")?;
            let model = take_option(&mut args, "--model")?;
            let status = take_option(&mut args, "--status")?
                .map(|value| {
                    value
                        .parse::<u16>()
                        .map_err(|_| "--status must be an HTTP status code".to_string())
                })
                .transpose()?;
            let jsonl = take_flag(&mut args, "--jsonl")?;
            finish_args(&args, OBSERVE_USAGE)?;
            follow_observability(
                &directory,
                since,
                interval,
                provider.as_deref(),
                model.as_deref(),
                status,
                jsonl,
            )
        }
        _ => Err(format!("unknown observe action: {action}\n{OBSERVE_USAGE}")),
    }
}

fn follow_observability(
    directory: &Path,
    mut cursor_ms: u64,
    interval_ms: u64,
    provider: Option<&str>,
    model: Option<&str>,
    status: Option<u16>,
    jsonl: bool,
) -> Result<(), String> {
    let mut seen_at_cursor = BTreeSet::<String>::new();
    loop {
        let mut events = read_recent_observability_events(directory, cursor_ms, 500);
        events.reverse();
        for item in events {
            if item.timestamp_ms < cursor_ms
                || provider.is_some_and(|value| item.provider_id.as_deref() != Some(value))
                || model.is_some_and(|value| {
                    item.exposed_model != value && item.upstream_model.as_deref() != Some(value)
                })
                || status.is_some_and(|value| item.status_code != value)
            {
                continue;
            }
            if item.timestamp_ms > cursor_ms {
                cursor_ms = item.timestamp_ms;
                seen_at_cursor.clear();
            }
            let key = format!(
                "{}\0{}\0{}",
                item.request_id,
                item.shadow_rule_id.as_deref().unwrap_or_default(),
                item.parent_request_id.as_deref().unwrap_or_default()
            );
            if !seen_at_cursor.insert(key) {
                continue;
            }
            if jsonl {
                println!(
                    "{}",
                    serde_json::to_string(&item).map_err(|error| error.to_string())?
                );
            } else {
                println!(
                    "{}  {}  {}  {}ms  {}",
                    item.timestamp_ms,
                    item.status_code,
                    item.exposed_model,
                    item.latency_ms,
                    item.outcome
                );
            }
        }
        io::stdout()
            .flush()
            .map_err(|error| format!("failed to flush observation stream: {error}"))?;
        thread::sleep(Duration::from_millis(interval_ms));
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn system(arguments: &[String], config: &Path) -> Result<(), String> {
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

fn parse_credential_patch(
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

fn model_rows(settings: &GatewaySettings, provider_filter: Option<&str>) -> Vec<serde_json::Value> {
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

fn save_settings(path: &Path, settings: &GatewaySettings) -> Result<(), String> {
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

fn provider_ref<'a>(
    settings: &'a GatewaySettings,
    id: &str,
) -> Result<&'a codetas_gateway::ProviderDefinition, String> {
    settings
        .providers
        .iter()
        .find(|item| item.id == id)
        .ok_or_else(|| format!("unknown provider: {id}"))
}

fn provider_mut<'a>(
    settings: &'a mut GatewaySettings,
    id: &str,
) -> Result<&'a mut codetas_gateway::ProviderDefinition, String> {
    settings
        .providers
        .iter_mut()
        .find(|item| item.id == id)
        .ok_or_else(|| format!("unknown provider: {id}"))
}

fn account_ref<'a>(
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

fn account_mut<'a>(
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

fn model_mut<'a>(
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

fn route_ref<'a>(settings: &'a GatewaySettings, id: &str) -> Result<&'a RouteDefinition, String> {
    settings
        .routes
        .iter()
        .find(|item| item.id == id)
        .ok_or_else(|| format!("unknown route: {id}"))
}

fn route_mut<'a>(
    settings: &'a mut GatewaySettings,
    id: &str,
) -> Result<&'a mut RouteDefinition, String> {
    settings
        .routes
        .iter_mut()
        .find(|item| item.id == id)
        .ok_or_else(|| format!("unknown route: {id}"))
}

fn access_key_mut<'a>(
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

fn parse_route_target(value: &str) -> Result<RouteTarget, String> {
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

fn parse_route_strategy(value: &str) -> Result<RouteStrategy, String> {
    match value {
        "failover" => Ok(RouteStrategy::Failover),
        "weighted-round-robin" => Ok(RouteStrategy::WeightedRoundRobin),
        "least-usage" => Ok(RouteStrategy::LeastUsage),
        _ => Err("route strategy must be failover, weighted-round-robin, or least-usage".into()),
    }
}

fn parse_surface(value: &str) -> Result<AgentSurfaceMode, String> {
    match value {
        "v1" => Ok(AgentSurfaceMode::V1),
        "default" => Ok(AgentSurfaceMode::Default),
        "v2" => Ok(AgentSurfaceMode::V2),
        _ => Err("agent surface must be v1, default, or v2".into()),
    }
}

fn parse_bool(value: &str, option: &str) -> Result<bool, String> {
    match value {
        "on" | "true" | "1" => Ok(true),
        "off" | "false" | "0" => Ok(false),
        _ => Err(format!("{option} must be on or off")),
    }
}

fn clearable(value: String) -> Option<String> {
    (value != "-").then_some(value)
}
fn clearable_csv(value: String) -> Vec<String> {
    if value == "-" {
        Vec::new()
    } else {
        csv(&value)
    }
}
fn csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn optional_bool(args: &mut Vec<String>, option: &str) -> Result<Option<bool>, String> {
    take_option(args, option)?
        .map(|value| parse_bool(&value, option))
        .transpose()
}

fn optional_u16(args: &mut Vec<String>, option: &str) -> Result<Option<u16>, String> {
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

fn optional_u64(args: &mut Vec<String>, option: &str) -> Result<Option<u64>, String> {
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

fn optional_usize(args: &mut Vec<String>, option: &str) -> Result<Option<usize>, String> {
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

fn parse_clearable_u64(value: &str, option: &str) -> Result<Option<u64>, String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| format!("{option} must be a non-negative integer"))?;
    Ok((parsed > 0).then_some(parsed))
}

fn help_requested(args: &[String]) -> bool {
    args.iter()
        .any(|value| value == "--help" || value == "-h" || value == "help")
}

fn positional(args: &mut Vec<String>) -> Option<String> {
    args.first()
        .filter(|value| !value.starts_with('-'))
        .cloned()
        .map(|value| {
            args.remove(0);
            value
        })
}

fn required_positional(args: &mut Vec<String>, label: &str, usage: &str) -> Result<String, String> {
    positional(args).ok_or_else(|| format!("{label} is required\n{usage}"))
}

fn take_flag(args: &mut Vec<String>, name: &str) -> Result<bool, String> {
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

fn take_option(args: &mut Vec<String>, name: &str) -> Result<Option<String>, String> {
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

fn take_all_options(args: &mut Vec<String>, name: &str) -> Result<Vec<String>, String> {
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

fn finish_args(args: &[String], usage: &str) -> Result<(), String> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "unknown or unexpected argument(s): {}\n{usage}",
            args.join(" ")
        ))
    }
}

fn reject_json_for_mutation(json: bool) -> Result<(), String> {
    if json {
        Err("--json is available for read-only commands only".into())
    } else {
        Ok(())
    }
}

fn print_json(value: &impl Serialize) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value)
            .map_err(|error| format!("cannot encode output: {error}"))?
    );
    Ok(())
}
