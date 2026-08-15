#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod clients;
mod codex_app_server;
mod maintenance;
mod maintenance_jobs;
mod maintenance_process;
mod mcp;
mod provider_gateway;
mod service;

use serde::{Deserialize, Serialize};
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

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HermesProfile {
    name: String,
    display_name: Option<String>,
    description: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HermesProfileConvertReport {
    created: Vec<String>,
    skipped: Vec<String>,
}

const CODETAS_AGENT_MARKER: &str = "# Generated by CODETAS";
const HERMES_PROFILE_DIRECTORY: &str = "profiles";

fn hermes_profiles_dir() -> PathBuf {
    dirs::home_dir()
        .map(|home| home.join(".hermes").join(HERMES_PROFILE_DIRECTORY))
        .unwrap_or_else(|| PathBuf::from(".hermes").join(HERMES_PROFILE_DIRECTORY))
}

fn codex_agents_dir() -> PathBuf {
    dirs::home_dir()
        .map(|home| home.join(".codex").join("agents"))
        .unwrap_or_else(|| PathBuf::from(".codex").join("agents"))
}

/// Minimal parser for the flat `key: value` profile.yaml Hermes writes.
/// Handles quoted values, `\uXXXX` escapes, and multi-line quoted scalars
/// (including `\` newline folding) without a YAML dependency.
fn parse_flat_yaml(raw: &str) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut current: Option<usize> = None;
    for raw_line in raw.lines() {
        let line = raw_line.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            if let Some((key, value)) = line.split_once(':') {
                fields.push((key.trim().to_string(), value.trim().to_string()));
                current = Some(fields.len() - 1);
                continue;
            }
            current = None;
            continue;
        }
        if let Some(index) = current {
            let continuation = line.trim();
            let existing = &mut fields[index].1;
            if existing.ends_with('\\') {
                existing.pop();
                existing.push_str(continuation);
            } else {
                if !existing.is_empty() {
                    existing.push(' ');
                }
                existing.push_str(continuation);
            }
        }
    }
    fields
        .into_iter()
        .map(|(key, value)| (key, unescape_yaml_scalar(&value)))
        .collect()
}

fn unescape_yaml_scalar(value: &str) -> String {
    let mut stripped = value.trim();
    let bytes = stripped.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            stripped = &stripped[1..stripped.len() - 1];
        }
    }
    let bytes = stripped.as_bytes();
    let mut result = String::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 1 < bytes.len() {
            let next = bytes[index + 1];
            if next == b'u' && index + 5 < bytes.len() {
                let hex = std::str::from_utf8(&bytes[index + 2..index + 6]).unwrap_or("");
                if let Ok(code) = u32::from_str_radix(hex, 16) {
                    if let Some(ch) = char::from_u32(code) {
                        result.push(ch);
                        index += 6;
                        continue;
                    }
                }
            }
            let escaped = match next {
                b'n' => Some('\n'),
                b't' => Some('\t'),
                b'r' => Some('\r'),
                b'"' => Some('"'),
                b'\'' => Some('\''),
                b'\\' => Some('\\'),
                _ => None,
            };
            if let Some(ch) = escaped {
                result.push(ch);
                index += 2;
                continue;
            }
        }
        let ch = stripped[index..].chars().next().unwrap_or_default();
        result.push(ch);
        index += ch.len_utf8();
    }
    result
}

fn read_hermes_profile(name: &str) -> Option<(Option<String>, String)> {
    let path = hermes_profiles_dir().join(name).join("profile.yaml");
    let raw = fs::read_to_string(&path).ok()?;
    let fields = parse_flat_yaml(&raw);
    let get = |key: &str| {
        fields
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value)
    };
    let description = get("description")?.trim().to_string();
    if description.is_empty() {
        return None;
    }
    let display_name = get("display_name")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    Some((display_name, description))
}

#[tauri::command]
fn list_hermes_profiles() -> Vec<HermesProfile> {
    let mut profiles = Vec::new();
    let Ok(entries) = fs::read_dir(hermes_profiles_dir()) else {
        return profiles;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((display_name, description)) = read_hermes_profile(&name) else {
            continue;
        };
        profiles.push(HermesProfile {
            name,
            display_name,
            description,
        });
    }
    profiles.sort_by(|left, right| left.name.cmp(&right.name));
    profiles
}

fn compact_description(description: &str) -> String {
    let single_line = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut result: String = single_line.chars().take(160).collect();
    if single_line.len() > 160 {
        result.push('…');
    }
    result
}

fn render_codex_agent(name: &str, description: &str, instructions: &str) -> String {
    let escape_quoted = |value: &str| value.replace('\\', "\\\\").replace('"', "\\\"");
    let multiline = instructions.replace("\"\"\"", "\"\"\\\"");
    format!(
        "{marker}\nname = \"{name}\"\ndescription = \"{description}\"\ndeveloper_instructions = \"\"\"\n{instructions}\n\"\"\"\n",
        marker = CODETAS_AGENT_MARKER,
        name = escape_quoted(name),
        description = escape_quoted(description),
        instructions = multiline,
    )
}

fn atomic_write_text(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("フォルダを作れません: {error}"))?;
    }
    let temp = path.with_extension("tmp");
    fs::write(&temp, content).map_err(|error| format!("書き込めません: {error}"))?;
    fs::rename(&temp, path).map_err(|error| format!("保存できません: {error}"))
}

#[tauri::command]
fn convert_hermes_profiles(
    profile_names: Vec<String>,
) -> Result<HermesProfileConvertReport, String> {
    let mut created = Vec::new();
    let mut skipped = Vec::new();
    let agents_dir = codex_agents_dir();
    fs::create_dir_all(&agents_dir)
        .map_err(|error| format!("Codex agentsフォルダを作れません: {error}"))?;
    for requested in profile_names {
        if requested.is_empty()
            || requested.contains('/')
            || requested.contains('\\')
            || requested == "."
            || requested == ".."
        {
            skipped.push(format!("{requested}: 不正な名前"));
            continue;
        }
        let Some((display_name, description)) = read_hermes_profile(&requested) else {
            skipped.push(format!("{requested}: プロファイルが見つかりません"));
            continue;
        };
        let target = agents_dir.join(format!("{requested}.toml"));
        if target.exists() {
            let existing = fs::read_to_string(&target).unwrap_or_default();
            if !existing.contains(CODETAS_AGENT_MARKER) {
                skipped.push(format!(
                    "{requested}: Codexプロファイルが既に存在します（上書きしません）"
                ));
                continue;
            }
        }
        let routing_description = display_name.unwrap_or_else(|| compact_description(&description));
        let content = render_codex_agent(&requested, &routing_description, &description);
        atomic_write_text(&target, &content)?;
        created.push(requested);
    }
    Ok(HermesProfileConvertReport { created, skipped })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_yaml_with_quotes_and_unicode_escapes() {
        let raw = "description: コード差分を検査する\n\
                   display_name: \"\\u30CD\\u30A4\\u30C6\\u30A3\\u30AA\"\n\
                   description_auto: false\n";
        let fields = parse_flat_yaml(raw);
        assert_eq!(
            fields
                .iter()
                .find(|(key, _)| key == "description")
                .map(|(_, value)| value.as_str()),
            Some("コード差分を検査する")
        );
        assert_eq!(
            fields
                .iter()
                .find(|(key, _)| key == "display_name")
                .map(|(_, value)| value.as_str()),
            Some("ネイティオ")
        );
        assert_eq!(
            fields
                .iter()
                .find(|(key, _)| key == "description_auto")
                .map(|(_, value)| value.as_str()),
            Some("false")
        );
    }

    #[test]
    fn folds_multiline_quoted_scalars() {
        let raw = "description: \"\\u4E2D\\u56FD\\u5927\\u9646\\\n  SNS\\u3001\\u82F1\\u8A9E\"\n\
                   description_auto: false\n";
        let fields = parse_flat_yaml(raw);
        let description = fields
            .iter()
            .find(|(key, _)| key == "description")
            .map(|(_, value)| value.as_str());
        assert_eq!(description, Some("中国大陆SNS、英語"));
        assert_eq!(
            fields
                .iter()
                .find(|(key, _)| key == "description_auto")
                .map(|(_, value)| value.as_str()),
            Some("false")
        );
    }

    #[test]
    fn renders_valid_codex_agent_toml() {
        let content = render_codex_agent(
            "scyther",
            "コード差分を検査する専任レビュアー",
            "コード差分を読み取り専用で検査する。\nセキュリティを優先する。",
        );
        assert!(content.starts_with("# Generated by CODETAS\n"));
        assert!(content.contains("name = \"scyther\"\n"));
        assert!(content.contains("description = \"コード差分を検査する専任レビュアー\"\n"));
        assert!(content.contains("developer_instructions = \"\"\"\n"));
        assert!(content.ends_with("\"\"\"\n"));
    }

    #[test]
    fn compacts_long_descriptions() {
        let long = format!("word{}", "x".repeat(300));
        let compact = compact_description(&long);
        assert!(compact.chars().count() <= 161);
        assert!(compact.ends_with('…'));
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
                            let _ = provider_gateway::gateway_ops::start_provider_gateway(
                                app.clone(),
                                manager,
                            )
                            .await;
                        });
                    }
                    "codetas-stop-gateway" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
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
            if let Err(error) =
                provider_gateway::gateway_ops::converge_codex_integration(app_handle.clone())
            {
                eprintln!("CODETAS: Codex startup integration was skipped: {error}");
            }
            tauri::async_runtime::spawn(async move {
                let Ok(settings) =
                    provider_gateway::presets::gateway_configuration(app_handle.clone())
                else {
                    return;
                };
                if settings.runtime.auto_start {
                    let manager = app_handle.state::<provider_gateway::GatewayManager>();
                    let _ = provider_gateway::gateway_ops::start_provider_gateway(
                        app_handle.clone(),
                        manager,
                    )
                    .await;
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            pick_project,
            inspect_project,
            list_hermes_profiles,
            convert_hermes_profiles,
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
            provider_gateway::presets::check_for_codetas_update,
            provider_gateway::presets::install_codetas_update,
            provider_gateway::gateway_ops::save_gateway_configuration,
            provider_gateway::cli_scan::list_provider_presets,
            provider_gateway::cli_scan::scan_local_cli_clients,
            provider_gateway::cli_scan::register_local_cli_in_codetas,
            provider_gateway::cli_scan::register_codetas_provider,
            provider_gateway::cli_scan::list_direct_api_targets,
            provider_gateway::diagnostics::test_gateway_provider,
            provider_gateway::service_cmds::launch_provider_oauth_broker,
            provider_gateway::diagnostics::gateway_diagnostics,
            provider_gateway::diagnostics::gateway_observability_summary,
            provider_gateway::diagnostics::gateway_observability_breakdown,
            provider_gateway::diagnostics::preview_gateway_observability_cleanup,
            provider_gateway::diagnostics::trash_gateway_observability_cleanup,
            provider_gateway::diagnostics::list_gateway_observability_trash,
            provider_gateway::diagnostics::restore_gateway_observability_trash,
            provider_gateway::diagnostics::start_gateway_debug_scope,
            provider_gateway::diagnostics::gateway_debug_events,
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
