#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod clients;
mod mcp;
mod provider_gateway;
mod service;

use serde::Serialize;
use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
};
use tauri::Manager;

const MAX_SKILL_SCAN_DEPTH: usize = 4;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectInspection {
    id: String,
    name: String,
    path: String,
    context_file: Option<String>,
    agents_file: Option<String>,
    skills_directory: Option<String>,
    skills_count: usize,
    mcp_file: Option<String>,
    codex_config_file: Option<String>,
    warnings: Vec<String>,
    inspected_at: String,
}

#[tauri::command]
fn pick_project() -> Option<String> {
    rfd::FileDialog::new()
        .set_title("CODETASに追加するプロジェクトを選択")
        .pick_folder()
        .map(|path| path.to_string_lossy().into_owned())
}

#[tauri::command]
fn inspect_project(path: String) -> Result<ProjectInspection, String> {
    let root =
        fs::canonicalize(&path).map_err(|error| format!("プロジェクトを開けません: {error}"))?;

    if !root.is_dir() {
        return Err("選択したパスはフォルダではありません。".into());
    }

    let context_file = first_file(&root, &[".hermes.md", "HERMES.md"]);
    let agents_file = first_file(&root, &["AGENTS.md"]);
    let skills_directory = first_directory(&root, &[".hermes/skills", "skills", ".agents/skills"]);
    let mcp_file = first_file(&root, &[".hermes/mcp.json", "mcp.json", ".mcp.json"]);
    let codex_config_file = first_file(&root, &[".codex/config.toml"]);
    let skills_count = skills_directory
        .as_deref()
        .map(|directory| count_skill_files(directory, 0))
        .unwrap_or(0);

    let mut warnings = Vec::new();
    if context_file.is_none() && agents_file.is_none() {
        warnings.push("HermesまたはCodexのプロジェクト指示が見つかりません。".into());
    }
    if mcp_file.is_some() {
        warnings.push("MCP設定は適用前に変換内容の確認が必要です。".into());
    }
    if root.join(".env").exists() || root.join("auth.json").exists() {
        warnings.push("資格情報ファイルは検出対象から除外されています。".into());
    }

    let canonical = root.to_string_lossy().into_owned();
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);

    Ok(ProjectInspection {
        id: format!("project-{:x}", hasher.finish()),
        name: root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("project")
            .to_string(),
        path: canonical,
        context_file: display_path(context_file),
        agents_file: display_path(agents_file),
        skills_directory: display_path(skills_directory),
        skills_count,
        mcp_file: display_path(mcp_file),
        codex_config_file: display_path(codex_config_file),
        warnings,
        inspected_at: unix_timestamp_iso(),
    })
}

fn first_file(root: &Path, candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(|candidate| root.join(candidate))
        .find_map(|candidate| {
            let metadata = fs::symlink_metadata(&candidate).ok()?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return None;
            }
            let canonical = fs::canonicalize(candidate).ok()?;
            canonical.starts_with(root).then_some(canonical)
        })
}

fn first_directory(root: &Path, candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(|candidate| root.join(candidate))
        .find_map(|candidate| {
            let metadata = fs::symlink_metadata(&candidate).ok()?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return None;
            }
            let canonical = fs::canonicalize(candidate).ok()?;
            canonical.starts_with(root).then_some(canonical)
        })
}

fn display_path(path: Option<PathBuf>) -> Option<String> {
    path.map(|value| value.to_string_lossy().into_owned())
}

fn count_skill_files(directory: &Path, depth: usize) -> usize {
    if depth > MAX_SKILL_SCAN_DEPTH {
        return 0;
    }

    let Ok(entries) = fs::read_dir(directory) else {
        return 0;
    };

    entries
        .filter_map(Result::ok)
        .map(|entry| (entry.path(), entry.file_type().ok()))
        .map(|(path, file_type)| {
            let Some(file_type) = file_type else { return 0 };
            if file_type.is_symlink() {
                0
            } else if file_type.is_dir() {
                count_skill_files(&path, depth + 1)
            } else if file_type.is_file()
                && path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md")
            {
                1
            } else {
                0
            }
        })
        .sum()
}

fn unix_timestamp_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    // The UI only requires a parseable instant. Avoid a time/date dependency in the shell.
    seconds.saturating_mul(1000).to_string()
}


fn main() {
    match early_runtime_command() {
        Ok(Some(EarlyRuntimeCommand::GatewayService {
            settings,
            observability,
        })) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap_or_else(|error| {
                    eprintln!("CODETAS Gateway: runtime initialization failed: {error}");
                    std::process::exit(1);
                });
            if let Err(error) =
                runtime.block_on(service::run_gateway_service(settings, observability))
            {
                eprintln!("CODETAS Gateway: {error}");
                std::process::exit(1);
            }
            return;
        }
        Ok(Some(EarlyRuntimeCommand::StartGatewayService)) => match service::start() {
            Ok(true) => return,
            Ok(false) => {
                eprintln!("CODETAS Gateway service did not reach a running state");
                std::process::exit(1);
            }
            Err(error) => {
                eprintln!("CODETAS Gateway: {error}");
                std::process::exit(1);
            }
        },
        Ok(Some(EarlyRuntimeCommand::McpServer)) => {
            if let Err(error) = mcp::run() {
                eprintln!("CODETAS MCP: {error}");
                std::process::exit(1);
            }
            return;
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("CODETAS Gateway: {error}");
            std::process::exit(2);
        }
    }
    let exit_state = Arc::new(AtomicU8::new(0));
    let builder = tauri::Builder::default();
    #[cfg(not(feature = "validation-build"))]
    let builder = builder.plugin(tauri_plugin_updater::Builder::new().build());
    let app = builder
        .manage(provider_gateway::GatewayManager::default())
        .manage(provider_gateway::DebugScopeManager::default())
        .setup(move |app| {
            let open = tauri::menu::MenuItem::with_id(
                app,
                "codetas-open",
                "CODETASを開く",
                true,
                None::<&str>,
            )?;
            let start = tauri::menu::MenuItem::with_id(
                app,
                "codetas-start-gateway",
                "Gatewayを起動",
                true,
                None::<&str>,
            )?;
            let stop = tauri::menu::MenuItem::with_id(
                app,
                "codetas-stop-gateway",
                "Gatewayを停止",
                true,
                None::<&str>,
            )?;
            let quit =
                tauri::menu::MenuItem::with_id(app, "codetas-quit", "終了", true, None::<&str>)?;
            let menu = tauri::menu::Menu::with_items(app, &[&open, &start, &stop, &quit])?;
            let mut tray = tauri::tray::TrayIconBuilder::with_id("codetas-tray")
                .menu(&menu)
                .tooltip("CODETAS — Codexに、できることを足す。")
                .show_menu_on_left_click(true)
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "codetas-open" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.unminimize();
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "codetas-start-gateway" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let manager = app.state::<provider_gateway::GatewayManager>();
                            let _ = provider_gateway::start_provider_gateway(app.clone(), manager)
                                .await;
                        });
                    }
                    "codetas-stop-gateway" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let manager = app.state::<provider_gateway::GatewayManager>();
                            let _ =
                                provider_gateway::stop_provider_gateway(app.clone(), manager).await;
                        });
                    }
                    "codetas-quit" => {
                        app.exit(0);
                    }
                    _ => {}
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;
            let app_handle = app.handle().clone();
            if let Err(error) = provider_gateway::converge_codex_integration(app_handle.clone()) {
                eprintln!("CODETAS: Codex startup integration was skipped: {error}");
            }
            tauri::async_runtime::spawn(async move {
                let Ok(settings) = provider_gateway::gateway_configuration(app_handle.clone())
                else {
                    return;
                };
                if settings.runtime.auto_start {
                    let manager = app_handle.state::<provider_gateway::GatewayManager>();
                    let _ =
                        provider_gateway::start_provider_gateway(app_handle.clone(), manager).await;
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            pick_project,
            inspect_project,
            provider_gateway::start_provider_gateway,
            provider_gateway::stop_provider_gateway,
            provider_gateway::provider_gateway_status,
            provider_gateway::gateway_configuration,
            provider_gateway::check_for_codetas_update,
            provider_gateway::install_codetas_update,
            provider_gateway::save_gateway_configuration,
            provider_gateway::list_provider_presets,
            provider_gateway::scan_local_cli_clients,
            provider_gateway::register_local_cli_in_codetas,
            provider_gateway::register_codetas_provider,
            provider_gateway::list_direct_api_targets,
            provider_gateway::test_gateway_provider,
            provider_gateway::launch_provider_oauth_broker,
            provider_gateway::gateway_diagnostics,
            provider_gateway::gateway_observability_summary,
            provider_gateway::gateway_observability_breakdown,
            provider_gateway::preview_gateway_observability_cleanup,
            provider_gateway::trash_gateway_observability_cleanup,
            provider_gateway::list_gateway_observability_trash,
            provider_gateway::restore_gateway_observability_trash,
            provider_gateway::start_gateway_debug_scope,
            provider_gateway::gateway_debug_events,
            provider_gateway::gateway_service_status,
            provider_gateway::install_gateway_service,
            provider_gateway::start_gateway_service,
            provider_gateway::restart_gateway_service,
            provider_gateway::stop_gateway_service,
            provider_gateway::uninstall_gateway_service,
            provider_gateway::sync_client_integrations,
            provider_gateway::install_provider_preset,
            provider_gateway::refresh_gateway_provider_models,
            provider_gateway::sync_codex_model_catalog,
            provider_gateway::upsert_gateway_provider,
            provider_gateway::remove_gateway_provider,
            provider_gateway::set_default_gateway_provider,
            provider_gateway::install_codex_gateway_config,
            provider_gateway::restore_codex_gateway_config,
            provider_gateway::uninstall_codetas_integration
        ])
        .build(tauri::generate_context!())
        .expect("failed to build CODETAS");
    app.run(move |app, event| {
        if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
            if code == Some(tauri::RESTART_EXIT_CODE) {
                return;
            }
            match exit_state.load(Ordering::Acquire) {
                2 => return,
                1 => {
                    api.prevent_exit();
                    return;
                }
                _ => {}
            }
            api.prevent_exit();
            if exit_state
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return;
            }
            let exit_state = Arc::clone(&exit_state);
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let manager = app.state::<provider_gateway::GatewayManager>();
                provider_gateway::shutdown_embedded_gateway(&manager).await;
                exit_state.store(2, Ordering::Release);
                app.exit(0);
            });
        }
    });
}

enum EarlyRuntimeCommand {
    GatewayService {
        settings: PathBuf,
        observability: PathBuf,
    },
    StartGatewayService,
    McpServer,
}

fn early_runtime_command() -> Result<Option<EarlyRuntimeCommand>, String> {
    let mut arguments = std::env::args_os().skip(1);
    let Some(command) = arguments.next() else {
        return Ok(None);
    };
    if command == "--start-gateway-service" {
        if arguments.next().is_some() {
            return Err("--start-gateway-service does not accept additional arguments".into());
        }
        return Ok(Some(EarlyRuntimeCommand::StartGatewayService));
    }
    if command == "--mcp-server" {
        if arguments.next().is_some() {
            return Err("--mcp-server does not accept additional arguments".into());
        }
        return Ok(Some(EarlyRuntimeCommand::McpServer));
    }
    if command != "--gateway-service" {
        return Ok(None);
    }
    let mut settings = None;
    let mut observability = None;
    while let Some(argument) = arguments.next() {
        if argument == "--config" {
            settings = Some(PathBuf::from(
                arguments.next().ok_or("--config requires a path")?,
            ));
        } else if argument == "--observability" {
            observability = Some(PathBuf::from(
                arguments.next().ok_or("--observability requires a path")?,
            ));
        } else {
            return Err(format!(
                "unknown gateway service argument: {}",
                argument.to_string_lossy()
            ));
        }
    }
    Ok(Some(EarlyRuntimeCommand::GatewayService {
        settings: settings.ok_or("--gateway-service requires --config")?,
        observability: observability.ok_or("--gateway-service requires --observability")?,
    }))
}
