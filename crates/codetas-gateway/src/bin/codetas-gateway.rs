use codetas_gateway::{
    parse_gateway_settings_json, start_gateway_with_options, GatewayRuntimeOptions, GatewaySettings,
};
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
};

#[path = "codetas-gateway/management.rs"]
mod management;

enum Command {
    Serve {
        config: PathBuf,
        port: Option<u16>,
    },
    Show {
        config: PathBuf,
    },
    Validate {
        config: PathBuf,
    },
    Export {
        config: PathBuf,
        output: PathBuf,
    },
    Import {
        config: PathBuf,
        input: PathBuf,
    },
    Manage {
        config: PathBuf,
        group: String,
        arguments: Vec<String>,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("CODETAS Gateway: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    match parse_arguments()? {
        Command::Serve { config, port } => serve(config, port).await,
        Command::Show { config } => {
            let settings = read_valid_settings(&config)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&settings)
                    .map_err(|error| format!("cannot encode settings: {error}"))?
            );
            Ok(())
        }
        Command::Validate { config } => {
            read_valid_settings(&config)?;
            println!("CODETAS settings are valid: {}", config.display());
            Ok(())
        }
        Command::Export { config, output } => {
            let settings = read_valid_settings(&config)?;
            let content = serde_json::to_vec_pretty(&settings)
                .map_err(|error| format!("cannot encode settings: {error}"))?;
            atomic_write_new_or_owned(&output, &content, false)?;
            println!("CODETAS settings exported to {}", output.display());
            Ok(())
        }
        Command::Import { config, input } => {
            let settings = read_valid_settings(&input)?;
            let content = serde_json::to_vec_pretty(&settings)
                .map_err(|error| format!("cannot encode settings: {error}"))?;
            if config.exists() {
                let backup = backup_path(&config);
                fs::copy(&config, &backup)
                    .map_err(|error| format!("cannot create import backup: {error}"))?;
                secure_existing_file(&backup)?;
                println!("Previous settings backed up to {}", backup.display());
            }
            atomic_write_new_or_owned(&config, &content, true)?;
            println!("CODETAS settings imported from {}", input.display());
            Ok(())
        }
        Command::Manage {
            config,
            group,
            arguments,
        } => management::run(&group, &arguments, &config).await,
    }
}

async fn serve(config: PathBuf, port: Option<u16>) -> Result<(), String> {
    let port_override = port;
    let mut settings = read_valid_settings(&config)?;
    if let Some(port) = port_override {
        settings.runtime.port = port;
    }
    settings.validate()?;
    let runtime_port = settings.runtime.port;
    let runtime_host = settings.runtime.host.clone();
    let mut last_modified = fs::metadata(&config)
        .and_then(|metadata| metadata.modified())
        .ok();
    let observability_directory = config
        .parent()
        .map(|directory| directory.join("observability"));
    let runtime_state_path = config
        .parent()
        .map(|directory| directory.join("gateway-runtime.json"));
    let mut handle = start_gateway_with_options(
        runtime_port,
        settings,
        GatewayRuntimeOptions {
            observability_directory,
            runtime_state_path,
            auth_store_path: config.parent().map(|directory| directory.join("auth.json")),
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    println!("CODETAS Gateway listening at {}", handle.url());
    let mut reload_tick = tokio::time::interval(std::time::Duration::from_secs(2));
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let mut server_exited = false;
    let exit_result = loop {
        tokio::select! {
            signal = &mut shutdown => {
                break signal;
            }
            result = handle.wait_for_exit() => {
                server_exited = true;
                break result.map_err(|error| format!("gateway server exited unexpectedly: {error}"));
            }
            _ = reload_tick.tick() => {
                let modified = fs::metadata(&config)
                    .and_then(|metadata| metadata.modified())
                    .ok();
                if modified.is_none() || modified == last_modified {
                    continue;
                }
                last_modified = modified;
                let mut reloaded = match read_valid_settings(&config) {
                    Ok(settings) => settings,
                    Err(_) => {
                        eprintln!("CODETAS Gateway: settings reload skipped because validation failed");
                        continue;
                    }
                };
                if let Some(port_override) = port_override {
                    reloaded.runtime.port = port_override;
                }
                if reloaded.runtime.host != runtime_host || reloaded.runtime.port != runtime_port {
                    eprintln!("CODETAS Gateway: host or port changed; restart is required");
                    continue;
                }
                if handle.set_settings(reloaded).await.is_err() {
                    eprintln!("CODETAS Gateway: settings reload failed");
                }
            }
        }
    };
    if server_exited {
        handle.flush_observability().await;
    } else {
        handle.shutdown().await;
    }
    exit_result
}

#[cfg(unix)]
async fn shutdown_signal() -> Result<(), String> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut terminate = signal(SignalKind::terminate())
        .map_err(|error| format!("SIGTERM handler failed: {error}"))?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            result.map_err(|error| format!("interrupt handler failed: {error}"))
        }
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> Result<(), String> {
    tokio::signal::ctrl_c()
        .await
        .map_err(|error| format!("signal handler failed: {error}"))
}

fn read_valid_settings(path: &Path) -> Result<GatewaySettings, String> {
    let bytes = fs::read(path).map_err(|error| format!("cannot read settings: {error}"))?;
    parse_gateway_settings_json(&bytes)
        .map(|(settings, _)| settings)
        .map_err(|error| format!("cannot parse settings: {error}"))
}

fn parse_arguments() -> Result<Command, String> {
    let mut values = env::args().skip(1).collect::<Vec<_>>();
    if values
        .first()
        .is_some_and(|value| value == "--help" || value == "-h")
    {
        print_help();
        std::process::exit(0);
    }
    if values
        .first()
        .is_some_and(|value| value == "--version" || value == "-V")
    {
        println!("codetas-gateway {}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }

    let group = values
        .first()
        .filter(|value| {
            matches!(
                value.as_str(),
                "provider"
                    | "account"
                    | "models"
                    | "model"
                    | "route"
                    | "agent"
                    | "access"
                    | "observe"
                    | "system"
            )
        })
        .cloned();
    if group.is_some() {
        values.remove(0);
    }
    let explicit_serve = values.first().is_some_and(|value| value == "serve");
    if explicit_serve {
        values.remove(0);
    }
    let config_action = if group.is_none() && values.first().is_some_and(|value| value == "config")
    {
        values.remove(0);
        Some(if values.is_empty() {
            return Err("config requires show, validate, export, or import".into());
        } else {
            values.remove(0)
        })
    } else {
        None
    };
    let mut config = env::var_os("CODETAS_SETTINGS").map(PathBuf::from);
    let mut port = None;
    let mut input = None;
    let mut output = None;
    let mut remaining = Vec::new();
    let mut index = 0;
    while index < values.len() {
        let argument = &values[index];
        match argument.as_str() {
            "--config" => {
                index += 1;
                config = Some(PathBuf::from(
                    values.get(index).ok_or("--config requires a path")?,
                ));
            }
            "--port" if group.is_none() => {
                index += 1;
                let value = values.get(index).ok_or("--port requires a number")?;
                port = Some(
                    value
                        .parse::<u16>()
                        .ok()
                        .filter(|port| *port > 0)
                        .ok_or("--port must be between 1 and 65535")?,
                );
            }
            "--input" if group.is_none() => {
                index += 1;
                input = Some(PathBuf::from(
                    values.get(index).ok_or("--input requires a path")?,
                ));
            }
            "--output" if group.is_none() => {
                index += 1;
                output = Some(PathBuf::from(
                    values.get(index).ok_or("--output requires a path")?,
                ));
            }
            _ if group.is_some() => remaining.push(argument.clone()),
            other => return Err(format!("unknown argument: {other}")),
        }
        index += 1;
    }
    let config = config.ok_or("--config or CODETAS_SETTINGS is required")?;
    if let Some(group) = group {
        if port.is_some() || input.is_some() || output.is_some() || explicit_serve {
            return Err(format!("invalid arguments for {group}"));
        }
        return Ok(Command::Manage {
            config,
            group,
            arguments: remaining,
        });
    }
    match config_action.as_deref() {
        None => Ok(Command::Serve { config, port }),
        Some("show") if port.is_none() && input.is_none() && output.is_none() => {
            Ok(Command::Show { config })
        }
        Some("validate") if port.is_none() && input.is_none() && output.is_none() => {
            Ok(Command::Validate { config })
        }
        Some("export") if port.is_none() && input.is_none() => Ok(Command::Export {
            config,
            output: output.ok_or("config export requires --output")?,
        }),
        Some("import") if port.is_none() && output.is_none() => Ok(Command::Import {
            config,
            input: input.ok_or("config import requires --input")?,
        }),
        Some(action) => Err(format!("invalid arguments for config {action}")),
    }
}

fn print_help() {
    println!(
        "Usage:\n  codetas-gateway [serve] --config <settings.json> [--port <port>]\n  codetas-gateway config <show|validate|export|import> --config <settings.json> [...]\n  codetas-gateway <provider|account|models|route|agent|access|observe|system> <subcommand> --config <settings.json>\n\nRun a management command with --help for its complete usage. Credentials are configured only by environment, keychain, OAuth broker, command reference, or caller forwarding; plaintext secrets are not accepted."
    );
}

fn backup_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "codetas-backup-{}-{}.json",
        std::process::id(),
        unique_suffix()
    ))
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn atomic_write_new_or_owned(path: &Path, content: &[u8], replace: bool) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create settings directory: {error}"))?;
    if path.exists() && !replace {
        return Err(format!("output already exists: {}", path.display()));
    }
    let temporary = parent.join(format!(
        ".codetas-settings-{}-{}.tmp",
        std::process::id(),
        unique_suffix()
    ));
    let _ = fs::remove_file(&temporary);
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    secure_mode(&mut options);
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("cannot create settings temporary file: {error}"))?;
    if let Err(error) = file
        .write_all(content)
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.sync_all())
    {
        let _ = fs::remove_file(&temporary);
        return Err(format!("cannot write settings: {error}"));
    }
    replace_file(&temporary, path)
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, path: &Path) -> Result<(), String> {
    fs::rename(temporary, path).map_err(|error| format!("cannot publish settings: {error}"))
}

#[cfg(windows)]
fn replace_file(temporary: &Path, path: &Path) -> Result<(), String> {
    if !path.exists() {
        return fs::rename(temporary, path)
            .map_err(|error| format!("cannot publish settings: {error}"));
    }
    let backup = path.with_extension(format!(
        "codetas-replace-{}-{}.bak",
        std::process::id(),
        unique_suffix()
    ));
    let _ = fs::remove_file(&backup);
    fs::rename(path, &backup)
        .map_err(|error| format!("cannot stage previous settings: {error}"))?;
    match fs::rename(temporary, path) {
        Ok(()) => {
            let _ = fs::remove_file(backup);
            Ok(())
        }
        Err(error) => {
            let _ = fs::rename(&backup, path);
            Err(format!("cannot publish settings: {error}"))
        }
    }
}

#[cfg(unix)]
fn secure_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(windows)]
fn secure_mode(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn secure_existing_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("cannot secure backup: {error}"))
}

#[cfg(windows)]
fn secure_existing_file(_path: &Path) -> Result<(), String> {
    Ok(())
}
