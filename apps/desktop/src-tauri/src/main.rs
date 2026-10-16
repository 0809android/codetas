#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod clients;
mod codex_app_server;
mod maintenance;
mod maintenance_jobs;
mod maintenance_process;
mod mcp;
mod provider_gateway;
mod service;

use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
};
use tauri::Manager;

fn focus_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
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
        Ok(Some(EarlyRuntimeCommand::CodexFallbackWatchdog)) => {
            if let Err(error) = service::run_codex_fallback_watchdog() {
                eprintln!("CODETAS Codex fallback: {error}");
                std::process::exit(1);
            }
            return;
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
    let builder = builder.plugin(tauri_plugin_single_instance::init(
        |app, _argv, _cwd| {
            focus_main_window(app);
        },
    ));
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
                "接続する",
                true,
                None::<&str>,
            )?;
            let stop = tauri::menu::MenuItem::with_id(
                app,
                "codetas-stop-gateway",
                "解除する",
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
                        focus_main_window(app);
                    }
                    "codetas-start-gateway" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let manager = app.state::<provider_gateway::GatewayManager>();
                            if provider_gateway::gateway_ops::start_provider_gateway(
                                app.clone(),
                                manager,
                            )
                            .await
                            .is_ok()
                            {
                                let manager = app.state::<provider_gateway::GatewayManager>();
                                let _ = provider_gateway::gateway_ops::install_codex_gateway_config(
                                    app.clone(),
                                    manager,
                                    provider_gateway::CodexGatewayInstallInput { model: None },
                                )
                                .await;
                            }
                        });
                    }
                    "codetas-stop-gateway" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let manager = app.state::<provider_gateway::GatewayManager>();
                            let _ = provider_gateway::gateway_ops::restore_codex_gateway_config(
                                app.clone(),
                                manager,
                            )
                            .await;
                            let manager = app.state::<provider_gateway::GatewayManager>();
                            let _ = provider_gateway::gateway_ops::stop_provider_gateway(
                                app.clone(),
                                manager,
                            )
                            .await;
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
            maintenance_jobs::start_idle_maintenance_worker(app_handle.clone());
            tauri::async_runtime::spawn(async move {
                let Ok(settings) = provider_gateway::load_gateway_settings(&app_handle) else {
                    return;
                };
                if settings.codex.fallback_to_official_when_unavailable {
                    if let Err(error) = service::ensure_codex_fallback_watchdog() {
                        eprintln!(
                            "CODETAS: official Codex fallback watchdog was not started: {error}"
                        );
                    }
                }
                if settings.runtime.auto_start {
                    let manager = app_handle.state::<provider_gateway::GatewayManager>();
                    if let Err(error) = provider_gateway::gateway_ops::start_provider_gateway(
                        app_handle.clone(),
                        manager,
                    )
                    .await
                    {
                        eprintln!("CODETAS: Gateway auto-start failed: {error}");
                    }
                    return;
                }
                if settings.codex.auto_connect
                    && !settings.codex.fallback_to_official_when_unavailable
                {
                    let manager = app_handle.state::<provider_gateway::GatewayManager>();
                    if let Err(error) = provider_gateway::gateway_ops::converge_codex_integration(
                        app_handle.clone(),
                        manager,
                    )
                    .await
                    {
                        eprintln!("CODETAS: Codex startup integration was skipped: {error}");
                    }
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            provider_gateway::gateway_ops::start_provider_gateway,
            provider_gateway::gateway_ops::stop_provider_gateway,
            provider_gateway::gateway_ops::provider_gateway_status,
            maintenance::analyze_codex_maintenance,
            maintenance_jobs::preview_codex_maintenance,
            maintenance_jobs::execute_codex_maintenance,
            maintenance_jobs::list_codex_maintenance_jobs,
            maintenance_jobs::rollback_codex_maintenance_job,
            maintenance_process::request_codex_shutdown,
            maintenance_process::restart_codex,
            maintenance_process::terminate_codex_writer,
            codex_app_server::retry_codex_archive,
            provider_gateway::presets::gateway_configuration,
            provider_gateway::presets::oauth_provider_registry,
            provider_gateway::presets::gateway_compatibility_lab,
            provider_gateway::presets::gateway_route_dry_runs,
            provider_gateway::presets::check_for_codetas_update,
            provider_gateway::presets::install_codetas_update,
            provider_gateway::gateway_ops::save_gateway_configuration,
            provider_gateway::gateway_ops::patch_gateway_management,
            provider_gateway::gateway_ops::apply_agent_preset_configuration,
            provider_gateway::cli_scan::list_provider_presets,
            provider_gateway::cli_scan::scan_local_cli_clients,
            provider_gateway::cli_scan::register_local_cli_in_codetas,
            provider_gateway::cli_scan::register_codetas_provider,
            provider_gateway::cli_scan::list_direct_api_targets,
            provider_gateway::diagnostics::test_gateway_provider,
            provider_gateway::agent_tools::codex_plugin_status,
            provider_gateway::agent_tools::run_agent_media_test,
            provider_gateway::service_cmds::launch_provider_oauth_broker,
            provider_gateway::diagnostics::gateway_diagnostics,
            provider_gateway::diagnostics::gateway_recent_events,
            provider_gateway::diagnostics::gateway_observability_summary,
            provider_gateway::diagnostics::gateway_cache_health,
            provider_gateway::diagnostics::gateway_observability_breakdown,
            provider_gateway::diagnostics::preview_gateway_observability_cleanup,
            provider_gateway::diagnostics::trash_gateway_observability_cleanup,
            provider_gateway::diagnostics::list_gateway_observability_trash,
            provider_gateway::diagnostics::restore_gateway_observability_trash,
            provider_gateway::diagnostics::start_gateway_debug_scope,
            provider_gateway::diagnostics::gateway_debug_events,
            provider_gateway::diagnostics::stop_gateway_debug_scope,
            provider_gateway::service_cmds::gateway_service_status,
            provider_gateway::service_cmds::install_gateway_service,
            provider_gateway::service_cmds::start_gateway_service,
            provider_gateway::service_cmds::restart_gateway_service,
            provider_gateway::service_cmds::stop_gateway_service,
            provider_gateway::service_cmds::uninstall_gateway_service,
            provider_gateway::integration::sync_client_integrations,
            provider_gateway::presets::install_provider_preset,
            provider_gateway::presets::refresh_gateway_provider_models,
            provider_gateway::presets::sync_codex_model_catalog,
            provider_gateway::gateway_ops::upsert_gateway_provider,
            provider_gateway::gateway_ops::remove_gateway_provider,
            provider_gateway::gateway_ops::set_default_gateway_provider,
            provider_gateway::gateway_ops::install_codex_gateway_config,
            provider_gateway::gateway_ops::restore_codex_gateway_config,
            provider_gateway::gateway_ops::uninstall_codetas_integration
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
                provider_gateway::gateway_ops::shutdown_embedded_gateway(&manager).await;
                if let Ok(settings) = provider_gateway::load_gateway_settings(&app) {
                    if settings.codex.fallback_to_official_when_unavailable {
                        if let Err(error) = provider_gateway::apply_official_codex_fallback(&app) {
                            eprintln!("CODETAS: official Codex fallback on exit failed: {error}");
                        }
                    }
                }
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
    CodexFallbackWatchdog,
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
    if command == "--codex-fallback-watchdog" {
        if arguments.next().is_some() {
            return Err("--codex-fallback-watchdog does not accept additional arguments".into());
        }
        return Ok(Some(EarlyRuntimeCommand::CodexFallbackWatchdog));
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
