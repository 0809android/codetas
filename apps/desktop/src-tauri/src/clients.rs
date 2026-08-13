use codetas_gateway::{build_codex_catalog, ClientIntegrationSettings, GatewaySettings};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

const CLIENT_MARKER: &str = "CODETAS-CLIENT-LAUNCHER-V1";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientIntegrationReport {
    pub clients: Vec<ClientArtifact>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientArtifact {
    pub client: String,
    pub enabled: bool,
    pub path: Option<String>,
    pub instructions: String,
}

pub fn sync(
    data_directory: &Path,
    executable: &Path,
    settings: &GatewaySettings,
    runtime_gateway_v1: Option<&str>,
) -> Result<ClientIntegrationReport, String> {
    let route = default_route(settings);
    let gateway_v1 = runtime_gateway_v1
        .map(str::to_string)
        .unwrap_or_else(|| format!("{}/v1", gateway_root(settings)));
    let gateway_root = gateway_v1
        .strip_suffix("/v1")
        .unwrap_or(&gateway_v1)
        .to_string();
    let launchers = data_directory.join("client-launchers");
    let mut clients = Vec::new();

    for (client, enabled, file, content, instructions) in [
        (
            "claudeCode",
            settings.integrations.claude_code,
            launcher_name("codetas-claude"),
            route
                .as_deref()
                .map(|route| claude_launcher(&gateway_root, route)),
            "Run the generated codetas-claude launcher instead of replacing the original claude command.",
        ),
        (
            "opencode",
            settings.integrations.opencode,
            launcher_name("codetas-opencode"),
            route
                .as_deref()
                .map(|route| opencode_launcher(&gateway_v1, route)),
            "Run the generated codetas-opencode launcher; it supplies an isolated in-memory provider config.",
        ),
        (
            "grok",
            settings.integrations.grok,
            launcher_name("codetas-grok"),
            route
                .as_deref()
                .map(|route| grok_launcher(&gateway_v1, route)),
            "Run the generated codetas-grok launcher; the original grok config is not modified.",
        ),
    ] {
        let path = launchers.join(file);
        if enabled {
            let content = content.ok_or_else(|| {
                format!("{client} integration requires a default provider model")
            })?;
            write_owned(&path, content.as_bytes(), 0o700)?;
            clients.push(ClientArtifact {
                client: client.into(),
                enabled: true,
                path: Some(path.to_string_lossy().into_owned()),
                instructions: instructions.into(),
            });
        } else {
            remove_owned(&path)?;
            clients.push(ClientArtifact {
                client: client.into(),
                enabled: false,
                path: None,
                instructions: "Disabled; no client-owned config was changed.".into(),
            });
        }
    }

    let desktop_fragment = launchers.join("claude-desktop-mcp.json");
    let desktop_inference = launchers.join("claude-desktop-inference.json");
    if settings.integrations.claude_desktop {
        let fragment = json!({
            "_codetasOwner": CLIENT_MARKER,
            "mcpServers": {
                "codetas-project": {
                    "command": executable,
                    "args": ["--mcp-server"]
                }
            }
        });
        let content = format!(
            "{}\n",
            serde_json::to_string_pretty(&fragment)
                .map_err(|error| format!("failed to encode Claude Desktop fragment: {error}"))?
        );
        write_owned(&desktop_fragment, content.as_bytes(), 0o600)?;
        clients.push(ClientArtifact {
            client: "claudeDesktop".into(),
            enabled: true,
            path: Some(desktop_fragment.to_string_lossy().into_owned()),
            instructions:
                "Merge the generated mcpServers.codetas-project entry in Claude Desktop after review; CODETAS does not read or rewrite the existing config."
                    .into(),
        });
        let inference = claude_desktop_inference_config(settings, &gateway_root);
        let content = format!(
            "{}\n",
            serde_json::to_string_pretty(&inference).map_err(|error| format!(
                "failed to encode Claude Desktop inference profile: {error}"
            ))?
        );
        write_owned(&desktop_inference, content.as_bytes(), 0o600)?;
        clients.push(ClientArtifact {
            client: "claudeDesktopInference".into(),
            enabled: true,
            path: Some(desktop_inference.to_string_lossy().into_owned()),
            instructions:
                "Import the reviewed inference profile into Claude Desktop's third-party config library. It contains Opus/Fable/Sonnet/Haiku assignments and no OAuth secret."
                    .into(),
        });
    } else {
        remove_owned(&desktop_fragment)?;
        remove_owned(&desktop_inference)?;
        clients.push(ClientArtifact {
            client: "claudeDesktop".into(),
            enabled: false,
            path: None,
            instructions: "Disabled; the Claude Desktop config was not modified.".into(),
        });
    }

    let pi_fragment = launchers.join("pi-models.json");
    if settings.integrations.pi {
        let fragment = pi_config(settings, &gateway_v1);
        let content = format!(
            "{}\n",
            serde_json::to_string_pretty(&fragment)
                .map_err(|error| format!("failed to encode Pi provider fragment: {error}"))?
        );
        write_owned(&pi_fragment, content.as_bytes(), 0o600)?;
        clients.push(ClientArtifact {
            client: "pi".into(),
            enabled: true,
            path: Some(pi_fragment.to_string_lossy().into_owned()),
            instructions:
                "Merge only providers.codetas into ~/.pi/agent/models.json after review; CODETAS never reads or replaces the real Pi config."
                    .into(),
        });
    } else {
        remove_owned(&pi_fragment)?;
        clients.push(ClientArtifact {
            client: "pi".into(),
            enabled: false,
            path: None,
            instructions: "Disabled; the Pi config was not modified.".into(),
        });
    }
    Ok(ClientIntegrationReport { clients })
}

pub fn remove_all(data_directory: &Path) -> Result<bool, String> {
    let launchers = data_directory.join("client-launchers");
    let mut removed = false;
    for file in [
        launcher_name("codetas-claude"),
        launcher_name("codetas-opencode"),
        launcher_name("codetas-grok"),
        "claude-desktop-mcp.json".into(),
        "claude-desktop-inference.json".into(),
        "pi-models.json".into(),
    ] {
        removed |= remove_owned(&launchers.join(file))?;
    }
    if fs::read_dir(&launchers)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
    {
        let _ = fs::remove_dir(launchers);
    }
    Ok(removed)
}

pub fn reconcile_claude_desktop_profile(settings: &mut GatewaySettings) -> Result<(), String> {
    if !settings.integrations.claude_desktop {
        settings.integrations.claude_desktop_aliases.clear();
        settings.integrations.claude_desktop_families.clear();
        settings.integrations.claude_desktop_defaults.clear();
        return Ok(());
    }
    let routes = build_codex_catalog(settings)
        .models
        .into_iter()
        .filter_map(|model| model.get("slug")?.as_str().map(str::to_string))
        .filter(|route| !route.starts_with("codetas-sidecar/"))
        .take(365)
        .collect::<Vec<_>>();
    let previous_families = settings.integrations.claude_desktop_families.clone();
    let previous_defaults = settings.integrations.claude_desktop_defaults.clone();
    let mut aliases = std::collections::BTreeMap::new();
    let mut families = std::collections::BTreeMap::new();
    let mut used = std::collections::BTreeSet::new();
    for route in &routes {
        let family = previous_families
            .get(route)
            .filter(|family| matches!(family.as_str(), "opus" | "fable" | "sonnet" | "haiku"))
            .cloned()
            .unwrap_or_else(|| inferred_claude_family(route).into());
        let alias = desktop_alias(route, &used)?;
        used.insert(alias.clone());
        aliases.insert(alias, route.clone());
        families.insert(route.clone(), family);
    }
    let mut defaults = std::collections::BTreeMap::new();
    for family in ["opus", "fable", "sonnet", "haiku"] {
        let members = routes
            .iter()
            .filter(|route| families.get(*route).is_some_and(|value| value == family))
            .collect::<Vec<_>>();
        if members.is_empty() {
            continue;
        }
        let selected = previous_defaults
            .get(family)
            .filter(|route| {
                members
                    .iter()
                    .any(|candidate| candidate.as_str() == route.as_str())
            })
            .cloned()
            .unwrap_or_else(|| (*members[0]).clone());
        defaults.insert(family.into(), selected);
    }
    settings.integrations.claude_desktop_aliases = aliases;
    settings.integrations.claude_desktop_families = families;
    settings.integrations.claude_desktop_defaults = defaults;
    Ok(())
}

fn inferred_claude_family(route: &str) -> &'static str {
    let lower = route.to_ascii_lowercase();
    if lower.contains("fable") {
        "fable"
    } else if lower.contains("sonnet") {
        "sonnet"
    } else if lower.contains("haiku") {
        "haiku"
    } else {
        "opus"
    }
}

fn desktop_alias(route: &str, used: &std::collections::BTreeSet<String>) -> Result<String, String> {
    if let Some((provider, model)) = route.split_once('/') {
        if provider == "anthropic" && model.starts_with("claude-") && !used.contains(model) {
            return Ok(model.to_string());
        }
    }
    for salt in 0..=365_u16 {
        let source = if salt == 0 {
            route.to_string()
        } else {
            format!("{route}#{salt}")
        };
        let mut hash = 0xcbf29ce484222325_u64;
        for byte in source.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        let first = char::from(b'a' + (hash % 26) as u8);
        let rest = base36((hash / 26) % 1_296, 2);
        let alias = format!("codetas-claude-opus-4-8-{first}{rest}");
        if !used.contains(&alias) {
            return Ok(alias);
        }
    }
    Err("Claude Desktop alias space is exhausted".into())
}

fn base36(mut value: u64, width: usize) -> String {
    let mut output = vec!['0'; width];
    for index in (0..width).rev() {
        let digit = (value % 36) as u8;
        output[index] = if digit < 10 {
            char::from(b'0' + digit)
        } else {
            char::from(b'a' + digit - 10)
        };
        value /= 36;
    }
    output.into_iter().collect()
}

fn claude_desktop_inference_config(settings: &GatewaySettings, gateway_root: &str) -> Value {
    let models = settings
        .integrations
        .claude_desktop_aliases
        .iter()
        .map(|(alias, route)| {
            let family = settings
                .integrations
                .claude_desktop_families
                .get(route)
                .map(String::as_str)
                .unwrap_or("opus");
            let mut model = serde_json::Map::new();
            model.insert("name".into(), Value::String(alias.clone()));
            model.insert(
                "labelOverride".into(),
                Value::String(format!("CODETAS · {route}")),
            );
            model.insert("anthropicFamilyTier".into(), Value::String(family.into()));
            if settings
                .integrations
                .claude_desktop_defaults
                .get(family)
                .is_some_and(|default| default == route)
            {
                model.insert("isFamilyDefault".into(), Value::Bool(true));
            }
            if authoritative_context(settings, route).is_some_and(|context| context >= 1_000_000) {
                model.insert("supports1m".into(), Value::Bool(true));
                model.insert("prefer1m".into(), Value::Bool(true));
            }
            Value::Object(model)
        })
        .collect::<Vec<_>>();
    json!({
        "_codetasOwner": CLIENT_MARKER,
        "inferenceProvider": "gateway",
        "inferenceCredentialKind": "static",
        "inferenceGatewayBaseUrl": gateway_root,
        "inferenceGatewayApiKey": if settings.security.require_local_token {
            "REPLACE_WITH_CODETAS_CLIENT_TOKEN"
        } else {
            "codetas-local"
        },
        "inferenceGatewayHeaders": {"x-codetas-client": "claude-desktop"},
        "modelDiscoveryEnabled": false,
        "inferenceModels": models,
    })
}

fn default_route(settings: &GatewaySettings) -> Option<String> {
    let provider_id = settings.default_provider.as_deref()?;
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.enabled && provider.id == provider_id)?;
    let model = provider
        .default_model
        .as_deref()
        .or_else(|| provider.models.first().map(String::as_str))?;
    Some(format!("{provider_id}/{model}"))
}

fn gateway_root(settings: &GatewaySettings) -> String {
    let host = match settings.runtime.host.as_str() {
        "localhost" | "0.0.0.0" => "127.0.0.1".to_string(),
        "::" => "[::1]".to_string(),
        host if host.contains(':') => format!("[{host}]"),
        host => host.to_string(),
    };
    format!("http://{host}:{}", settings.runtime.port)
}

#[cfg(unix)]
fn launcher_name(base: &str) -> String {
    base.into()
}

#[cfg(windows)]
fn launcher_name(base: &str) -> String {
    format!("{base}.cmd")
}

#[cfg(unix)]
fn claude_launcher(gateway_root: &str, route: &str) -> String {
    format!(
        "#!/bin/sh\n# {CLIENT_MARKER}\n: \"${{CODETAS_CLIENT_TOKEN:=${{CODETAS_GATEWAY_TOKEN:-codetas-local}}}}\"\nexport CODETAS_CLIENT_TOKEN\nexport ANTHROPIC_BASE_URL={}\nexport ANTHROPIC_AUTH_TOKEN=\"$CODETAS_CLIENT_TOKEN\"\nexport ANTHROPIC_MODEL={}\nexec claude \"$@\"\n",
        shell_quote(gateway_root),
        shell_quote(route)
    )
}

#[cfg(windows)]
fn claude_launcher(gateway_root: &str, route: &str) -> String {
    format!(
        "@echo off\r\nrem {CLIENT_MARKER}\r\nif not defined CODETAS_CLIENT_TOKEN if defined CODETAS_GATEWAY_TOKEN set \"CODETAS_CLIENT_TOKEN=%CODETAS_GATEWAY_TOKEN%\"\r\nif not defined CODETAS_CLIENT_TOKEN set \"CODETAS_CLIENT_TOKEN=codetas-local\"\r\nset \"ANTHROPIC_BASE_URL={}\"\r\nset \"ANTHROPIC_AUTH_TOKEN=%CODETAS_CLIENT_TOKEN%\"\r\nset \"ANTHROPIC_MODEL={}\"\r\nclaude %*\r\n",
        cmd_value(gateway_root),
        cmd_value(route)
    )
}

#[cfg(unix)]
fn opencode_launcher(gateway_v1: &str, route: &str) -> String {
    let config = opencode_config(gateway_v1, route);
    format!(
        "#!/bin/sh\n# {CLIENT_MARKER}\n: \"${{CODETAS_CLIENT_TOKEN:=${{CODETAS_GATEWAY_TOKEN:-codetas-local}}}}\"\nexport CODETAS_CLIENT_TOKEN\nexport OPENCODE_CONFIG_CONTENT={}\nexec opencode \"$@\"\n",
        shell_quote(&config.to_string())
    )
}

#[cfg(windows)]
fn opencode_launcher(gateway_v1: &str, route: &str) -> String {
    format!(
        "@echo off\r\nrem {CLIENT_MARKER}\r\nif not defined CODETAS_CLIENT_TOKEN if defined CODETAS_GATEWAY_TOKEN set \"CODETAS_CLIENT_TOKEN=%CODETAS_GATEWAY_TOKEN%\"\r\nif not defined CODETAS_CLIENT_TOKEN set \"CODETAS_CLIENT_TOKEN=codetas-local\"\r\nset \"OPENCODE_CONFIG_CONTENT={}\"\r\nopencode %*\r\n",
        cmd_value(&opencode_config(gateway_v1, route).to_string())
    )
}

fn opencode_config(gateway_v1: &str, route: &str) -> Value {
    let mut models = serde_json::Map::new();
    models.insert(route.into(), json!({"name": format!("CODETAS {route}")}));
    json!({
        "model": format!("codetas/{route}"),
        "provider": {
            "codetas": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "CODETAS",
                "options": {
                    "baseURL": gateway_v1,
                    "apiKey": "{env:CODETAS_CLIENT_TOKEN}"
                },
                "models": models
            }
        }
    })
}

fn pi_config(settings: &GatewaySettings, gateway_v1: &str) -> Value {
    let models = build_codex_catalog(settings)
        .models
        .into_iter()
        .filter_map(|model| {
            let id = model.get("slug")?.as_str()?.to_string();
            let name = model
                .get("display_name")
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_string();
            let input = model
                .get("input_modalities")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .filter(|values| !values.is_empty())
                .unwrap_or_else(|| vec!["text".into()]);
            let mut entry = serde_json::Map::new();
            entry.insert("id".into(), Value::String(id.clone()));
            entry.insert("name".into(), Value::String(name));
            entry.insert("input".into(), json!(input));
            if let Some(context) = authoritative_context(settings, &id) {
                entry.insert("contextWindow".into(), json!(context));
                entry.insert("maxTokens".into(), json!(context.min(32_000)));
            }
            Some(Value::Object(entry))
        })
        .collect::<Vec<_>>();
    json!({
        "_codetasOwner": CLIENT_MARKER,
        "providers": {
            "codetas": {
                "baseUrl": gateway_v1,
                "api": "openai-completions",
                "apiKey": "$CODETAS_CLIENT_TOKEN",
                "models": models
            }
        }
    })
}

fn authoritative_context(settings: &GatewaySettings, public_model: &str) -> Option<u64> {
    if let Some(route) = settings.routes.iter().find(|route| {
        route.enabled && (route.id == public_model || route.alias.as_deref() == Some(public_model))
    }) {
        let contexts = route
            .targets
            .iter()
            .map(|target| {
                let (provider_id, model_id) = target.model.split_once('/')?;
                settings
                    .model_catalog
                    .iter()
                    .find(|model| {
                        model.enabled
                            && model.provider_id == provider_id
                            && model.model_id == model_id
                    })?
                    .context_window
            })
            .collect::<Option<Vec<_>>>()?;
        return contexts.into_iter().min();
    }
    let (provider_id, model_id) = public_model.split_once('/')?;
    settings
        .model_catalog
        .iter()
        .find(|model| {
            model.enabled && model.provider_id == provider_id && model.model_id == model_id
        })?
        .context_window
}

#[cfg(unix)]
fn grok_launcher(gateway_v1: &str, route: &str) -> String {
    format!(
        "#!/bin/sh\n# {CLIENT_MARKER}\n: \"${{CODETAS_CLIENT_TOKEN:=${{CODETAS_GATEWAY_TOKEN:-codetas-local}}}}\"\nexport CODETAS_CLIENT_TOKEN\nexport GROK_MODELS_BASE_URL={}\nexport XAI_API_KEY=\"$CODETAS_CLIENT_TOKEN\"\nexport GROK_DEFAULT_MODEL={}\nexec grok \"$@\"\n",
        shell_quote(gateway_v1),
        shell_quote(route)
    )
}

#[cfg(windows)]
fn grok_launcher(gateway_v1: &str, route: &str) -> String {
    format!(
        "@echo off\r\nrem {CLIENT_MARKER}\r\nif not defined CODETAS_CLIENT_TOKEN if defined CODETAS_GATEWAY_TOKEN set \"CODETAS_CLIENT_TOKEN=%CODETAS_GATEWAY_TOKEN%\"\r\nif not defined CODETAS_CLIENT_TOKEN set \"CODETAS_CLIENT_TOKEN=codetas-local\"\r\nset \"GROK_MODELS_BASE_URL={}\"\r\nset \"XAI_API_KEY=%CODETAS_CLIENT_TOKEN%\"\r\nset \"GROK_DEFAULT_MODEL={}\"\r\ngrok %*\r\n",
        cmd_value(gateway_v1),
        cmd_value(route)
    )
}

#[cfg(unix)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(windows)]
fn cmd_value(value: &str) -> String {
    value
        .replace('^', "^^")
        .replace('%', "%%")
        .replace('"', "^\"")
}

fn write_owned(path: &Path, content: &[u8], mode: u32) -> Result<(), String> {
    if path.exists() {
        let current = fs::read_to_string(path)
            .map_err(|error| format!("existing client artifact cannot be read: {error}"))?;
        if !current.contains(CLIENT_MARKER) {
            return Err(format!(
                "client artifact is not owned by CODETAS and will not be overwritten: {}",
                path.display()
            ));
        }
    }
    let parent = path.parent().ok_or("client artifact has no parent")?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("client artifact directory cannot be created: {error}"))?;
    let temporary = parent.join(format!(".codetas-client-{}.tmp", std::process::id()));
    let _ = fs::remove_file(&temporary);
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    secure_mode(&mut options, mode);
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("client artifact temporary file cannot be created: {error}"))?;
    if let Err(error) = file.write_all(content).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("client artifact cannot be written: {error}"));
    }
    replace_owned_file(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("client artifact cannot be replaced: {error}")
    })
}

#[cfg(not(windows))]
fn replace_owned_file(temporary: &Path, path: &Path) -> std::io::Result<()> {
    fs::rename(temporary, path)
}

#[cfg(windows)]
fn replace_owned_file(temporary: &Path, path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return fs::rename(temporary, path);
    }
    let backup = path.with_extension(format!("codetas-replace-{}.bak", std::process::id()));
    let _ = fs::remove_file(&backup);
    fs::rename(path, &backup)?;
    match fs::rename(temporary, path) {
        Ok(()) => {
            let _ = fs::remove_file(backup);
            Ok(())
        }
        Err(error) => {
            let _ = fs::rename(&backup, path);
            Err(error)
        }
    }
}

fn remove_owned(path: &Path) -> Result<bool, String> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("client artifact cannot be read: {error}")),
    };
    if !content.contains(CLIENT_MARKER) {
        return Err(format!(
            "client artifact is not owned by CODETAS and will not be removed: {}",
            path.display()
        ));
    }
    fs::remove_file(path)
        .map(|_| true)
        .map_err(|error| format!("client artifact cannot be removed: {error}"))
}

#[cfg(unix)]
fn secure_mode(options: &mut OpenOptions, mode: u32) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(mode);
}

#[cfg(windows)]
fn secure_mode(_options: &mut OpenOptions, _mode: u32) {}

pub fn apply_switches(settings: &mut GatewaySettings, integrations: ClientIntegrationSettings) {
    settings.integrations = integrations;
}
