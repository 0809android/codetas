use codetas_gateway::{
    start_gateway_with_options, CredentialSource, GatewayRuntimeOptions, GatewaySettings,
};
use serde::Serialize;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

const SERVICE_MARKER: &str = "CODETAS-GATEWAY-SERVICE-V1";
const SERVICE_LABEL: &str = "jp.kinocode.codetas.gateway";
const WINDOWS_TASK_NAME: &str = "CODETAS Gateway";
static FILE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    pub supported: bool,
    pub installed: bool,
    pub running: bool,
    pub definition_path: Option<String>,
    pub shim_installed: bool,
    pub shim_path: Option<String>,
    pub supervisor: String,
    pub restart_policy: String,
    pub message: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceInstallReport {
    pub installed: bool,
    pub started: bool,
    pub definition_path: String,
    pub shim_path: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceUninstallReport {
    pub stopped: bool,
    pub removed_definition: bool,
    pub removed_shim: bool,
}

pub fn validate_service_settings(settings: &GatewaySettings) -> Result<Vec<String>, String> {
    if settings.security.require_local_token {
        return Err(
            "常駐サービスはCODETAS_GATEWAY_TOKENを永続化しません。ローカルトークンを無効にするか、外部サービス管理設定を使用してください"
                .into(),
        );
    }
    let mut environment_users = settings
        .providers
        .iter()
        .filter(|provider| provider.enabled)
        .filter(|provider| {
            provider.api_key_env.is_some()
                || provider.credential.source == CredentialSource::Environment
                || !provider.env_headers.is_empty()
        })
        .map(|provider| provider.id.clone())
        .collect::<Vec<_>>();
    environment_users.extend(
        settings
            .account_pool
            .accounts
            .iter()
            .filter(|account| {
                account.enabled && account.credential.source == CredentialSource::Environment
            })
            .map(|account| format!("account:{}", account.id)),
    );
    environment_users.extend(
        settings
            .security
            .external_access_keys
            .iter()
            .filter(|key| key.enabled)
            .map(|key| format!("access-key:{}", key.id)),
    );
    environment_users.sort();
    environment_users.dedup();
    if !environment_users.is_empty() {
        return Err(format!(
            "秘密値をサービス定義へ保存しないため、環境変数認証は常駐化できません: {}。OSキーチェーンまたは認証コマンドへ移行してください",
            environment_users.join(", ")
        ));
    }
    let mut warnings = Vec::new();
    if settings.providers.iter().any(|provider| {
        provider.enabled
            && (provider.credential.source == CredentialSource::Command
                || (provider.credential.source == CredentialSource::OAuth
                    && provider.credential.command.is_some()))
    }) {
        warnings
            .push("認証コマンドは最小限のサービス環境でも解決できる絶対パスを推奨します".into());
    }
    Ok(warnings)
}

pub async fn run_gateway_service(
    settings_path: PathBuf,
    observability_directory: PathBuf,
) -> Result<(), String> {
    if cfg!(feature = "validation-build") {
        return Err("検証用ビルドではOSサービスを起動しません".into());
    }
    let bytes = fs::read(&settings_path)
        .map_err(|error| format!("cannot read service settings: {error}"))?;
    let settings: GatewaySettings = serde_json::from_slice(&bytes)
        .map_err(|error| format!("cannot parse service settings: {error}"))?;
    settings.validate()?;
    let runtime_host = settings.runtime.host.clone();
    let runtime_port = settings.runtime.port;
    let mut last_modified = fs::metadata(&settings_path)
        .and_then(|metadata| metadata.modified())
        .ok();
    let mut handle = start_gateway_with_options(
        settings.runtime.port,
        settings,
        GatewayRuntimeOptions {
            observability_directory: Some(observability_directory),
            runtime_state_path: settings_path
                .parent()
                .map(|directory| directory.join("gateway-runtime.json")),
            auth_store_path: settings_path
                .parent()
                .map(|directory| directory.join("auth.json")),
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    if let Some(directory) = settings_path.parent() {
        if let Err(error) = crate::provider_gateway::reconcile_owned_codex_runtime_url_path(
            &directory.join("codex-install-journal.json"),
            &handle.url(),
        ) {
            eprintln!("CODETAS Gateway: Codex dynamic-port reconciliation skipped: {error}");
        }
    }
    let mut reload_tick = tokio::time::interval(std::time::Duration::from_secs(2));
    let shutdown = service_shutdown_signal();
    tokio::pin!(shutdown);
    enum ServiceExit {
        Signal(Result<(), String>),
        Server(Result<(), String>),
    }
    let exit = loop {
        tokio::select! {
            signal = &mut shutdown => {
                break ServiceExit::Signal(signal);
            }
            result = handle.wait_for_exit() => {
                break ServiceExit::Server(
                    result.map_err(|error| format!("gateway server exited unexpectedly: {error}"))
                );
            }
            _ = reload_tick.tick() => {
                let modified = fs::metadata(&settings_path)
                    .and_then(|metadata| metadata.modified())
                    .ok();
                if modified.is_none() || modified == last_modified {
                    continue;
                }
                last_modified = modified;
                let reloaded = fs::read(&settings_path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<GatewaySettings>(&bytes).ok());
                let Some(reloaded) = reloaded else {
                    eprintln!("CODETAS Gateway: settings reload skipped because the file is invalid");
                    continue;
                };
                if reloaded.validate().is_err() {
                    eprintln!("CODETAS Gateway: settings reload skipped because validation failed");
                    continue;
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
    match exit {
        ServiceExit::Signal(result) => {
            // Await server-task cancellation before flushing so no request can enqueue a
            // usage write after the final flush observed zero pending writers.
            handle.shutdown().await;
            result
        }
        ServiceExit::Server(result) => {
            // wait_for_exit already consumed the task result; do not poll the JoinHandle twice.
            handle.flush_observability().await;
            result
        }
    }
}

#[cfg(unix)]
async fn service_shutdown_signal() -> Result<(), String> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut terminate = signal(SignalKind::terminate())
        .map_err(|error| format!("service SIGTERM handler failed: {error}"))?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            result.map_err(|error| format!("service interrupt handler failed: {error}"))
        }
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn service_shutdown_signal() -> Result<(), String> {
    tokio::signal::ctrl_c()
        .await
        .map_err(|error| format!("service signal handler failed: {error}"))
}

pub fn install(
    executable: &Path,
    settings_path: &Path,
    observability_directory: &Path,
    install_shim: bool,
    mut warnings: Vec<String>,
) -> Result<ServiceInstallReport, String> {
    if cfg!(feature = "validation-build") {
        return Err("検証用ビルドではOSサービスを登録しません".into());
    }
    for (label, path) in [
        ("executable", executable),
        ("settings", settings_path),
        ("observability", observability_directory),
    ] {
        validate_service_path(label, path)?;
    }
    if !executable.is_file() || !settings_path.is_file() {
        return Err("常駐サービスの実行ファイルまたは設定ファイルが見つかりません".into());
    }
    let definition = definition_path()?;
    refuse_foreign_definition(&definition)?;
    if !definition.exists() && service_registration_exists()? {
        return Err("同名の実行中サービスはCODETAS所有と確認できないため変更しません".into());
    }
    let previous_definition = if definition.exists() {
        Some(
            fs::read(&definition)
                .map_err(|error| format!("旧サービス定義を退避できません: {error}"))?,
        )
    } else {
        None
    };
    let content = service_definition(executable, settings_path, observability_directory)?;
    atomic_write(&definition, content.as_bytes(), 0o600)?;
    let started = match reload_service(&definition) {
        Ok(started) => started,
        Err(error) => {
            return Err(rollback_service_install(
                &definition,
                previous_definition.as_deref(),
                error,
            ));
        }
    };
    if !started {
        return Err(rollback_service_install(
            &definition,
            previous_definition.as_deref(),
            "サービスを登録しましたが、監督プロセスが起動状態を確認できません".into(),
        ));
    }
    let shim_path = if install_shim {
        match install_codex_shim(executable) {
            Ok(path) => Some(path.to_string_lossy().into_owned()),
            Err(error) => {
                return Err(rollback_service_install(
                    &definition,
                    previous_definition.as_deref(),
                    error,
                ));
            }
        }
    } else {
        None
    };
    Ok(ServiceInstallReport {
        installed: true,
        started,
        definition_path: definition.to_string_lossy().into_owned(),
        shim_path,
        warnings,
    })
}

fn rollback_service_install(
    definition: &Path,
    previous_definition: Option<&[u8]>,
    cause: String,
) -> String {
    let mut errors = Vec::new();
    match service_registration_exists() {
        Ok(true) => match stop_service(definition) {
            Ok(true) => {}
            Ok(false) => errors.push("新サービスの停止状態を確認できません".into()),
            Err(error) => errors.push(format!("新サービス停止: {error}")),
        },
        Ok(false) => {}
        Err(error) => errors.push(format!("新サービス登録確認: {error}")),
    }
    let restored = match previous_definition {
        Some(content) => atomic_write(definition, content, 0o600),
        None => match fs::remove_file(definition) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("新サービス定義を削除できません: {error}")),
        },
    };
    if let Err(error) = restored {
        errors.push(format!("旧サービス定義: {error}"));
    } else if previous_definition.is_some() {
        match reload_service(definition) {
            Ok(true) => {}
            Ok(false) => errors.push("旧サービスの再起動状態を確認できません".into()),
            Err(error) => errors.push(format!("旧サービス再起動: {error}")),
        }
    } else if let Err(error) = refresh_service_manager() {
        errors.push(format!("サービス管理器更新: {error}"));
    }
    if errors.is_empty() {
        format!("サービス登録を復元しました: {cause}")
    } else {
        format!(
            "サービス登録と復元に失敗しました: {cause}; {}",
            errors.join("; ")
        )
    }
}

pub fn status() -> Result<ServiceStatus, String> {
    if cfg!(feature = "validation-build") {
        return Ok(ServiceStatus {
            supported: false,
            installed: false,
            running: false,
            definition_path: None,
            shim_installed: false,
            shim_path: None,
            supervisor: "validation sandbox".into(),
            restart_policy: "disabled".into(),
            message: "検証用ビルドではOSサービス操作を無効化しています".into(),
        });
    }
    let definition = definition_path()?;
    let installed = is_owned_definition(&definition)?;
    let running = installed && service_is_running()?;
    let shim = shim_path()?;
    let shim_installed = is_owned_shim(&shim)?;
    Ok(ServiceStatus {
        supported: true,
        installed,
        running,
        definition_path: Some(definition.to_string_lossy().into_owned()),
        shim_installed,
        shim_path: Some(shim.to_string_lossy().into_owned()),
        supervisor: supervisor_name().into(),
        restart_policy: restart_policy().into(),
        message: if running {
            "CODETAS Gatewayサービスは起動中です".into()
        } else if installed {
            "CODETAS Gatewayサービスは登録済みですが停止中です".into()
        } else {
            "CODETAS Gatewayサービスは未登録です".into()
        },
    })
}

#[cfg(target_os = "macos")]
fn supervisor_name() -> &'static str {
    "launchd"
}
#[cfg(target_os = "linux")]
fn supervisor_name() -> &'static str {
    "systemd --user"
}
#[cfg(target_os = "windows")]
fn supervisor_name() -> &'static str {
    "Task Scheduler"
}

#[cfg(target_os = "macos")]
fn restart_policy() -> &'static str {
    "KeepAlive / 5秒スロットル"
}
#[cfg(target_os = "linux")]
fn restart_policy() -> &'static str {
    "on-failure / 5秒 / 60秒に6回まで"
}
#[cfg(target_os = "windows")]
fn restart_policy() -> &'static str {
    "失敗時5秒後 / 最大10回"
}

pub fn start() -> Result<bool, String> {
    if cfg!(feature = "validation-build") {
        return Err("検証用ビルドではOSサービスを起動しません".into());
    }
    let definition = definition_path()?;
    if !is_owned_definition(&definition)? {
        return Err("CODETAS所有のサービス定義がありません".into());
    }
    match start_service(&definition) {
        Ok(()) => service_is_running(),
        Err(_) => reload_service(&definition),
    }
}

pub fn restart() -> Result<bool, String> {
    if cfg!(feature = "validation-build") {
        return Err("検証用ビルドではOSサービスを再起動しません".into());
    }
    let definition = definition_path()?;
    if !is_owned_definition(&definition)? {
        return Err("CODETAS所有のサービス定義がありません".into());
    }
    reload_service(&definition)
}

pub fn stop() -> Result<bool, String> {
    if cfg!(feature = "validation-build") {
        return Err("検証用ビルドではOSサービスを停止しません".into());
    }
    let definition = definition_path()?;
    if !is_owned_definition(&definition)? {
        return Err("CODETAS所有のサービス定義がありません".into());
    }
    let running = service_is_running()?;
    if !running && !service_registration_exists()? {
        return Ok(true);
    }
    let stopped = stop_service(&definition)?;
    Ok(stopped && !service_is_running()?)
}

pub fn uninstall() -> Result<ServiceUninstallReport, String> {
    if cfg!(feature = "validation-build") {
        return Ok(ServiceUninstallReport {
            stopped: false,
            removed_definition: false,
            removed_shim: false,
        });
    }
    let definition = definition_path()?;
    match fs::symlink_metadata(&definition) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ServiceUninstallReport {
                stopped: false,
                removed_definition: false,
                removed_shim: remove_codex_shim()?,
            });
        }
        Err(error) => return Err(format!("サービス定義を確認できません: {error}")),
        Ok(_) => {}
    }
    if !is_owned_definition(&definition)? {
        return Err("同名のサービス定義はCODETAS所有ではないため削除しません".into());
    }
    let running = service_is_running()?;
    let registered = service_registration_exists()?;
    let stopped = if running || registered {
        let stopped = stop_service(&definition)?;
        if !stopped || service_is_running()? {
            return Err(
                "常駐サービスが停止していないため、所有定義を保持したまま解除を中止しました".into(),
            );
        }
        true
    } else {
        true
    };
    let removed_definition = match fs::remove_file(&definition) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(format!("サービス定義を削除できません: {error}")),
    };
    refresh_service_manager()?;
    let removed_shim = remove_codex_shim()?;
    Ok(ServiceUninstallReport {
        stopped,
        removed_definition,
        removed_shim,
    })
}

fn validate_service_path(label: &str, path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("{label} path must be absolute"));
    }
    if path
        .to_string_lossy()
        .chars()
        .any(|character| character.is_control())
    {
        return Err(format!("{label} path contains control characters"));
    }
    Ok(())
}

fn refuse_foreign_definition(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("サービス定義を確認できません: {error}")),
        Ok(_) => {}
    }
    if is_owned_definition(path)? {
        Ok(())
    } else {
        Err("同名のサービス定義が既にあり、CODETAS所有ではないため上書きしません".into())
    }
}

fn is_owned_definition(path: &Path) -> Result<bool, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("サービス定義を確認できません: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("サービス定義は通常ファイルではないため変更しません".into());
    }
    fs::read_to_string(path)
        .map(|content| content.contains(SERVICE_MARKER))
        .map_err(|error| format!("サービス定義を読めません: {error}"))
}

fn install_codex_shim(executable: &Path) -> Result<PathBuf, String> {
    let path = shim_path()?;
    if !is_owned_shim(&path)? && fs::symlink_metadata(&path).is_ok() {
        return Err("既存のCodex起動シムはCODETAS所有ではないため上書きしません".into());
    }
    atomic_write(&path, shim_definition(executable)?.as_bytes(), 0o700)?;
    Ok(path)
}

fn remove_codex_shim() -> Result<bool, String> {
    let path = shim_path()?;
    if !is_owned_shim(&path)? {
        return match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!("Codex起動シムを確認できません: {error}")),
            Ok(_) => Err("Codex起動シムはCODETAS所有ではないため削除しません".into()),
        };
    }
    fs::remove_file(path)
        .map(|_| true)
        .map_err(|error| format!("Codex起動シムを削除できません: {error}"))
}

fn is_owned_shim(path: &Path) -> Result<bool, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("Codex起動シムを確認できません: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Codex起動シムは通常ファイルではないため変更しません".into());
    }
    fs::read_to_string(path)
        .map(|content| content.contains(SERVICE_MARKER))
        .map_err(|error| format!("Codex起動シムを読めません: {error}"))
}

fn atomic_write(path: &Path, content: &[u8], mode: u32) -> Result<(), String> {
    let parent = path.parent().ok_or("service path has no parent")?;
    fs::create_dir_all(parent).map_err(|error| format!("サービスフォルダを作れません: {error}"))?;
    let sequence = FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".codetas-service-{}-{sequence}.tmp",
        std::process::id()
    ));
    let _ = fs::remove_file(&temporary);
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    secure_mode(&mut options, mode);
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("一時サービス定義を作れません: {error}"))?;
    if let Err(error) = file.write_all(content).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("サービス定義を書けません: {error}"));
    }
    replace_owned_file(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("サービス定義を置換できません: {error}")
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
    let backup = path.with_extension(format!(
        "codetas-replace-{}-{}.bak",
        std::process::id(),
        FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
    ));
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

#[cfg(unix)]
fn secure_mode(options: &mut OpenOptions, mode: u32) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(mode);
}

#[cfg(not(unix))]
fn secure_mode(_options: &mut OpenOptions, _mode: u32) {}

fn run(program: &str, args: &[String]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("{program}を実行できません: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(1_000)
                .collect::<String>()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[cfg(target_os = "windows")]
fn windows_quote(path: &Path) -> String {
    format!("\"{}\"", path.to_string_lossy().replace('"', "\\\""))
}

#[cfg(target_os = "macos")]
fn definition_path() -> Result<PathBuf, String> {
    dirs::home_dir()
        .map(|home| {
            home.join("Library/LaunchAgents")
                .join(format!("{SERVICE_LABEL}.plist"))
        })
        .ok_or_else(|| "ホームフォルダを特定できません".into())
}

#[cfg(target_os = "macos")]
fn shim_path() -> Result<PathBuf, String> {
    dirs::data_local_dir()
        .map(|data| data.join("codetas/bin/codetas-codex"))
        .ok_or_else(|| "CODETASデータフォルダを特定できません".into())
}

#[cfg(target_os = "macos")]
fn service_definition(
    executable: &Path,
    settings: &Path,
    observability: &Path,
) -> Result<String, String> {
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- {SERVICE_MARKER} -->
<plist version="1.0"><dict>
<key>Label</key><string>{SERVICE_LABEL}</string>
<key>ProgramArguments</key><array>
<string>{}</string><string>--gateway-service</string>
<string>--config</string><string>{}</string>
<string>--observability</string><string>{}</string>
</array>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><true/>
<key>ThrottleInterval</key><integer>5</integer>
<key>ProcessType</key><string>Background</string>
<key>StandardOutPath</key><string>/dev/null</string>
<key>StandardErrorPath</key><string>/dev/null</string>
</dict></plist>
"#,
        xml_escape(&executable.to_string_lossy()),
        xml_escape(&settings.to_string_lossy()),
        xml_escape(&observability.to_string_lossy()),
    ))
}

#[cfg(target_os = "macos")]
fn shim_definition(executable: &Path) -> Result<String, String> {
    Ok(format!(
        "#!/bin/sh\n# {SERVICE_MARKER}\n{} --start-gateway-service >/dev/null 2>&1 || exit $?\nexec codex \"$@\"\n",
        shell_quote(executable)
    ))
}

#[cfg(target_os = "macos")]
fn service_domain() -> Result<String, String> {
    let uid = run("id", &["-u".into()])?;
    if !uid.chars().all(|character| character.is_ascii_digit()) {
        return Err("現在のユーザーIDを検証できません".into());
    }
    Ok(format!("gui/{uid}"))
}

#[cfg(target_os = "macos")]
fn reload_service(definition: &Path) -> Result<bool, String> {
    let domain = service_domain()?;
    if service_registration_exists()? && !stop_service(definition)? {
        return Err("launchdの旧Gatewayを安全に停止・解除できません".into());
    }
    run(
        "launchctl",
        &[
            "bootstrap".into(),
            domain.clone(),
            definition.to_string_lossy().into_owned(),
        ],
    )?;
    run(
        "launchctl",
        &[
            "kickstart".into(),
            "-k".into(),
            format!("{domain}/{SERVICE_LABEL}"),
        ],
    )?;
    service_is_running()
}

#[cfg(target_os = "macos")]
fn start_service(_definition: &Path) -> Result<(), String> {
    let domain = service_domain()?;
    run(
        "launchctl",
        &[
            "kickstart".into(),
            "-k".into(),
            format!("{domain}/{SERVICE_LABEL}"),
        ],
    )
    .map(|_| ())
}

#[cfg(target_os = "macos")]
fn stop_service(_definition: &Path) -> Result<bool, String> {
    let domain = service_domain()?;
    let target = format!("{domain}/{SERVICE_LABEL}");
    let status = Command::new("launchctl")
        .args(["bootout", target.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("launchctlを実行できません: {error}"))?;
    Ok(status.success())
}

#[cfg(target_os = "macos")]
fn service_is_running() -> Result<bool, String> {
    let domain = service_domain()?;
    let target = format!("{domain}/{SERVICE_LABEL}");
    Ok(Command::new("launchctl")
        .args(["print", target.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("launchctlを実行できません: {error}"))?
        .success())
}

#[cfg(target_os = "macos")]
fn refresh_service_manager() -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn service_registration_exists() -> Result<bool, String> {
    service_is_running()
}

#[cfg(target_os = "linux")]
fn definition_path() -> Result<PathBuf, String> {
    dirs::config_dir()
        .map(|config| config.join("systemd/user/codetas-gateway.service"))
        .ok_or_else(|| "設定フォルダを特定できません".into())
}

#[cfg(target_os = "linux")]
fn shim_path() -> Result<PathBuf, String> {
    dirs::data_local_dir()
        .map(|data| data.join("codetas/bin/codetas-codex"))
        .ok_or_else(|| "CODETASデータフォルダを特定できません".into())
}

#[cfg(target_os = "linux")]
fn systemd_quote(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('$', "\\$")
    )
}

#[cfg(target_os = "linux")]
fn service_definition(
    executable: &Path,
    settings: &Path,
    observability: &Path,
) -> Result<String, String> {
    Ok(format!(
        "# {SERVICE_MARKER}\n[Unit]\nDescription=CODETAS Gateway\nAfter=network-online.target\nWants=network-online.target\nStartLimitIntervalSec=60s\nStartLimitBurst=6\n\n[Service]\nType=simple\nExecStart={} --gateway-service --config {} --observability {}\nRestart=on-failure\nRestartSec=5s\nNoNewPrivileges=true\nPrivateTmp=true\n\n[Install]\nWantedBy=default.target\n",
        systemd_quote(executable),
        systemd_quote(settings),
        systemd_quote(observability),
    ))
}

#[cfg(target_os = "linux")]
fn shim_definition(executable: &Path) -> Result<String, String> {
    Ok(format!(
        "#!/bin/sh\n# {SERVICE_MARKER}\n{} --start-gateway-service >/dev/null 2>&1 || exit $?\nexec codex \"$@\"\n",
        shell_quote(executable)
    ))
}

#[cfg(target_os = "linux")]
fn reload_service(_definition: &Path) -> Result<bool, String> {
    refresh_service_manager()?;
    run(
        "systemctl",
        &[
            "--user".into(),
            "enable".into(),
            "--now".into(),
            "codetas-gateway.service".into(),
        ],
    )?;
    run(
        "systemctl",
        &[
            "--user".into(),
            "restart".into(),
            "codetas-gateway.service".into(),
        ],
    )?;
    service_is_running()
}

#[cfg(target_os = "linux")]
fn start_service(_definition: &Path) -> Result<(), String> {
    run(
        "systemctl",
        &[
            "--user".into(),
            "enable".into(),
            "--now".into(),
            "codetas-gateway.service".into(),
        ],
    )
    .map(|_| ())
}

#[cfg(target_os = "linux")]
fn stop_service(_definition: &Path) -> Result<bool, String> {
    run(
        "systemctl",
        &[
            "--user".into(),
            "disable".into(),
            "--now".into(),
            "codetas-gateway.service".into(),
        ],
    )?;
    Ok(!service_is_running()?)
}

#[cfg(target_os = "linux")]
fn service_is_running() -> Result<bool, String> {
    Ok(Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "codetas-gateway.service"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("systemctlを実行できません: {error}"))?
        .success())
}

#[cfg(target_os = "linux")]
fn refresh_service_manager() -> Result<(), String> {
    run("systemctl", &["--user".into(), "daemon-reload".into()]).map(|_| ())
}

#[cfg(target_os = "linux")]
fn service_registration_exists() -> Result<bool, String> {
    Ok(Command::new("systemctl")
        .args(["--user", "cat", "codetas-gateway.service"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("systemctlを実行できません: {error}"))?
        .success())
}

#[cfg(target_os = "windows")]
fn definition_path() -> Result<PathBuf, String> {
    dirs::data_local_dir()
        .map(|data| data.join("CODETAS/service/codetas-gateway-task.xml"))
        .ok_or_else(|| "設定フォルダを特定できません".into())
}

#[cfg(target_os = "windows")]
fn shim_path() -> Result<PathBuf, String> {
    dirs::data_local_dir()
        .map(|data| data.join("CODETAS/bin/codetas-codex.cmd"))
        .ok_or_else(|| "CODETASデータフォルダを特定できません".into())
}

#[cfg(target_os = "windows")]
fn service_definition(
    executable: &Path,
    settings: &Path,
    observability: &Path,
) -> Result<String, String> {
    let user = std::env::var("USERNAME").map_err(|_| "Windowsユーザー名を取得できません")?;
    let arguments = format!(
        "--gateway-service --config {} --observability {}",
        windows_quote(settings),
        windows_quote(observability)
    );
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- {SERVICE_MARKER} -->
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
<Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers>
<Principals><Principal id="Author"><UserId>{}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><RestartOnFailure><Interval>PT5S</Interval><Count>10</Count></RestartOnFailure><Enabled>true</Enabled></Settings>
<Actions Context="Author"><Exec><Command>{}</Command><Arguments>{}</Arguments></Exec></Actions>
</Task>"#,
        xml_escape(&user),
        xml_escape(&executable.to_string_lossy()),
        xml_escape(&arguments),
    ))
}

#[cfg(target_os = "windows")]
fn shim_definition(executable: &Path) -> Result<String, String> {
    Ok(format!(
        "@echo off\r\nrem {SERVICE_MARKER}\r\n{} --start-gateway-service >NUL 2>&1 || exit /b %ERRORLEVEL%\r\ncodex %*\r\n",
        windows_quote(executable)
    ))
}

#[cfg(target_os = "windows")]
fn reload_service(definition: &Path) -> Result<bool, String> {
    if service_registration_exists()? && !stop_service(definition)? {
        return Err("Task Schedulerの旧Gatewayを安全に停止・解除できません".into());
    }
    run(
        "schtasks.exe",
        &[
            "/Create".into(),
            "/TN".into(),
            WINDOWS_TASK_NAME.into(),
            "/XML".into(),
            definition.to_string_lossy().into_owned(),
            "/F".into(),
        ],
    )?;
    start_service(definition)?;
    service_is_running()
}

#[cfg(target_os = "windows")]
fn start_service(_definition: &Path) -> Result<(), String> {
    run(
        "schtasks.exe",
        &["/Run".into(), "/TN".into(), WINDOWS_TASK_NAME.into()],
    )?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !service_is_running()? {
        if std::time::Instant::now() >= deadline {
            return Err("Task SchedulerがGatewayを10秒以内に起動できませんでした".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn stop_service(_definition: &Path) -> Result<bool, String> {
    if service_is_running()? {
        if let Err(error) = run(
            "schtasks.exe",
            &["/End".into(), "/TN".into(), WINDOWS_TASK_NAME.into()],
        ) {
            if service_is_running()? {
                return Err(error);
            }
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while service_is_running()? {
            if std::time::Instant::now() >= deadline {
                return Err(
                    "Task Scheduler ended the task but the Gateway process did not stop within 10 seconds"
                        .into(),
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    run(
        "schtasks.exe",
        &[
            "/Delete".into(),
            "/TN".into(),
            WINDOWS_TASK_NAME.into(),
            "/F".into(),
        ],
    )?;
    Ok(!service_registration_exists()?)
}

#[cfg(target_os = "windows")]
fn service_is_running() -> Result<bool, String> {
    if !service_registration_exists()? {
        return Ok(false);
    }
    let script = format!(
        "(Get-ScheduledTask -TaskName '{}').State -eq 'Running'",
        WINDOWS_TASK_NAME.replace('\'', "''")
    );
    run(
        "powershell.exe",
        &[
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script,
        ],
    )
    .map(|output| output.eq_ignore_ascii_case("true"))
}

#[cfg(target_os = "windows")]
fn service_registration_exists() -> Result<bool, String> {
    Ok(Command::new("schtasks.exe")
        .args(["/Query", "/TN", WINDOWS_TASK_NAME])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("schtasks.exeを実行できません: {error}"))?
        .success())
}

#[cfg(target_os = "windows")]
fn refresh_service_manager() -> Result<(), String> {
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
compile_error!("CODETAS gateway service supports macOS, Linux, and Windows only");
