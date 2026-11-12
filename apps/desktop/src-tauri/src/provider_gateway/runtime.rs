use super::*;

pub(crate) async fn update_running_gateway(
    manager: &State<'_, GatewayManager>,
    settings: &GatewaySettings,
) -> Result<(), String> {
    let guard = manager.handle.lock().await;
    if settings.runtime.standalone_service {
        drop(guard);
        if !service::restart()? {
            return Err("常駐サービスを再起動できませんでした".into());
        }
        return Ok(());
    }
    if let Some(handle) = guard.as_ref() {
        handle.set_settings(settings.clone()).await?;
    }
    Ok(())
}

async fn apply_running_gateway_transition(
    app: &AppHandle,
    manager: &State<'_, GatewayManager>,
    previous: &GatewaySettings,
    next: &GatewaySettings,
) -> Result<(), String> {
    if !requires_embedded_gateway_restart(previous, next) {
        return update_running_gateway(manager, next).await;
    }
    let prior_handle = manager.handle.lock().await.take();
    let Some(handle) = prior_handle else {
        return Ok(());
    };
    handle.shutdown().await;
    match start_managed_gateway(app, next.clone()).await {
        Ok(handle) => {
            *manager.handle.lock().await = Some(handle);
            Ok(())
        }
        Err(error) => match start_managed_gateway(app, previous.clone()).await {
            Ok(handle) => {
                *manager.handle.lock().await = Some(handle);
                Err(format!(
                    "memory budget変更後のGateway再起動に失敗したため旧runtimeへ復元しました: {error}"
                ))
            }
            Err(restore) => Err(format!(
                "memory budget変更後のGateway再起動に失敗し、旧runtimeの再起動にも失敗しました: {error}; {restore}"
            )),
        },
    }
}

pub(crate) fn requires_embedded_gateway_restart(
    previous: &GatewaySettings,
    next: &GatewaySettings,
) -> bool {
    !next.runtime.standalone_service
        && previous.runtime.memory_budget_bytes != next.runtime.memory_budget_bytes
}

pub(crate) async fn persist_and_apply_settings(
    app: &AppHandle,
    manager: &State<'_, GatewayManager>,
    previous: &GatewaySettings,
    next: &GatewaySettings,
) -> Result<(), String> {
    let catalog_snapshot = snapshot_codex_catalog_transition(app, previous, next)?;
    save_settings(app, next)?;
    if let Err(error) = apply_running_gateway_transition(app, manager, previous, next).await {
        let mut restore_errors =
            restore_settings_and_catalog(app, previous, catalog_snapshot.as_ref());
        if let Err(runtime) = update_running_gateway(manager, previous).await {
            restore_errors.push(format!("旧runtime設定: {runtime}"));
        }
        let restore = if restore_errors.is_empty() {
            "旧設定へ復元しました".to_string()
        } else {
            format!("旧状態の復元に失敗しました: {}", restore_errors.join("; "))
        };
        return Err(format!(
            "設定をruntimeへ反映できませんでした: {error}; {restore}"
        ));
    }
    if next.codex.fallback_to_official_when_unavailable {
        if let Err(error) = crate::service::ensure_codex_fallback_watchdog() {
            eprintln!("CODETAS: official Codex fallback watchdog was not started: {error}");
        }
    } else if previous.codex.fallback_to_official_when_unavailable {
        if let Err(error) = crate::service::uninstall_codex_fallback_watchdog() {
            eprintln!("CODETAS: official Codex fallback watchdog was not removed: {error}");
        }
    }
    Ok(())
}

pub(crate) async fn runtime_is_running(
    app: &AppHandle,
    manager: &State<'_, GatewayManager>,
    settings: &GatewaySettings,
) -> Result<bool, String> {
    Ok(observe_gateway_runtime(app, manager, settings)
        .await?
        .running)
}

pub(crate) struct ObservedGatewayRuntime {
    pub running: bool,
    pub locally_owned: bool,
}

pub(crate) async fn observe_gateway_runtime(
    app: &AppHandle,
    manager: &State<'_, GatewayManager>,
    settings: &GatewaySettings,
) -> Result<ObservedGatewayRuntime, String> {
    if manager.handle.lock().await.is_some() {
        return Ok(ObservedGatewayRuntime {
            running: true,
            locally_owned: true,
        });
    }
    if settings.runtime.standalone_service && service::status()?.running {
        return Ok(ObservedGatewayRuntime {
            running: true,
            locally_owned: true,
        });
    }
    if let Some(shared) = shared_gateway_runtime(app) {
        return Ok(shared);
    }
    Ok(ObservedGatewayRuntime {
        running: false,
        locally_owned: false,
    })
}

pub(crate) async fn status_from_runtime(
    app: &AppHandle,
    manager: &State<'_, GatewayManager>,
    settings: GatewaySettings,
) -> Result<GatewayStatus, String> {
    let observed = observe_gateway_runtime(app, manager, &settings).await?;
    status(app, observed.running, observed.locally_owned, settings)
}

pub(crate) fn status(
    app: &AppHandle,
    running: bool,
    locally_owned: bool,
    settings: GatewaySettings,
) -> Result<GatewayStatus, String> {
    let gateway_url = if running {
        runtime_gateway_url(app).unwrap_or_else(|| gateway_url(&settings))
    } else {
        gateway_url(&settings)
    };
    let official_fallback_active = official_fallback_is_active(app);
    let fallback_reconnect_error = super::gateway_ops::codex_connection_problem(app);
    let codex_configured =
        !official_fallback_active && codex_gateway_is_configured(app, &settings).unwrap_or(false);
    Ok(GatewayStatus {
        running,
        url: gateway_url,
        providers: settings.providers,
        default_provider: settings.default_provider,
        codex_configured,
        settings_path: Some(settings_path(app)?.to_string_lossy().into_owned()),
        locally_owned: running && locally_owned,
        official_fallback_active,
        fallback_reconnect_error,
    })
}

fn shared_gateway_runtime(app: &AppHandle) -> Option<ObservedGatewayRuntime> {
    let path = gateway_runtime_state_path(app).ok()?;
    let metadata = fs::symlink_metadata(&path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 64 * 1024 {
        return None;
    }
    if !published_gateway_is_live_at(&path) {
        return None;
    }
    parse_published_runtime_state(&fs::read(path).ok()?)?;
    Some(ObservedGatewayRuntime {
        running: true,
        locally_owned: false,
    })
}

struct PublishedRuntimeState {
    pid: u32,
    url: String,
    host_port: String,
}

pub(crate) fn published_gateway_is_live_at(path: &Path) -> bool {
    published_gateway_url_if_live(path).is_some()
}

pub(crate) fn published_gateway_url_if_live(path: &Path) -> Option<String> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 64 * 1024 {
        return None;
    }
    let published = parse_published_runtime_state(&fs::read(path).ok()?)?;
    (process_exists(published.pid) && loopback_port_is_listening(&published.host_port))
        .then_some(published.url)
}

fn parse_published_runtime_state(bytes: &[u8]) -> Option<PublishedRuntimeState> {
    if bytes.len() > 64 * 1024 {
        return None;
    }
    let value = serde_json::from_slice::<JsonValue>(bytes).ok()?;
    if value.get("version").and_then(JsonValue::as_u64) != Some(1) {
        return None;
    }
    let pid = value.get("pid").and_then(JsonValue::as_u64)?;
    if pid == 0 || pid > u64::from(u32::MAX) {
        return None;
    }
    let url = value.get("url").and_then(JsonValue::as_str)?.to_string();
    let host_port = published_loopback_host_port(&url)?;
    Some(PublishedRuntimeState {
        pid: pid as u32,
        url,
        host_port,
    })
}

fn published_loopback_host_port(url: &str) -> Option<String> {
    let safe_loopback = url.starts_with("http://127.0.0.1:")
        || url.starts_with("http://[::1]:")
        || url.starts_with("http://localhost:");
    if !(safe_loopback && url.ends_with("/v1") && url.len() <= 512) {
        return None;
    }
    Some(
        url.trim_start_matches("http://")
            .trim_end_matches("/v1")
            .to_string(),
    )
}

pub(crate) fn process_exists(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        let output = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        output.map(|status| status.success()).unwrap_or(false)
    }
    #[cfg(windows)]
    {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output();
        output
            .ok()
            .filter(|result| result.status.success())
            .is_some_and(|result| {
                let text = String::from_utf8_lossy(&result.stdout);
                let needle = format!("\"{pid}\"");
                text.lines().any(|line| line.contains(&needle))
            })
    }
}

fn loopback_port_is_listening(host_port: &str) -> bool {
    let Some((host, port)) = host_port.rsplit_once(':') else {
        return false;
    };
    let Ok(port) = port.parse::<u16>() else {
        return false;
    };
    let host = host.trim_matches(['[', ']']);
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::new(
            match host {
                "127.0.0.1" | "localhost" => std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                "::1" => std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
                _ => return false,
            },
            port,
        ),
        std::time::Duration::from_millis(200),
    )
    .is_ok()
}

pub(crate) async fn start_managed_gateway(
    app: &AppHandle,
    settings: GatewaySettings,
) -> Result<GatewayHandle, String> {
    let handle = start_gateway_with_options(
        settings.runtime.port,
        settings,
        GatewayRuntimeOptions {
            observability_directory: Some(observability_path(app)?),
            runtime_state_path: Some(settings_path(app)?.with_file_name("gateway-runtime.json")),
            auth_store_path: Some(auth_store_file_path(app)?),
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    // Compared with the journal, not with the configured port: the config follows
    // whatever this gateway bound last, including after the port setting changes.
    match codex_journal_path(app) {
        Ok(journal_path) => reconcile_started_gateway_at(&journal_path, &handle.url()),
        Err(error) => super::gateway_ops::set_codex_connection_error(Some(error)),
    }
    Ok(handle)
}

/// The gateway is already serving. A Codex config that cannot follow it is a problem
/// to show in the connection panel, not a reason to drop the gateway.
pub(crate) fn reconcile_started_gateway_at(journal_path: &Path, actual_url: &str) {
    if let Err(error) = reconcile_owned_codex_runtime_url_path(journal_path, actual_url) {
        eprintln!("CODETAS: Codex config could not follow the gateway: {error}");
        super::gateway_ops::set_codex_connection_error(Some(error));
    }
}

pub(crate) fn load_settings(app: &AppHandle) -> Result<GatewaySettings, String> {
    load_settings_with_migration_state(app).map(|(settings, _)| settings)
}

pub(crate) fn load_settings_with_migration_state(
    app: &AppHandle,
) -> Result<(GatewaySettings, bool), String> {
    // Keep reads side-effect free. Settings/catalog writers persist migrations
    // only inside the shared command mutation boundary, so a read cannot race
    // a load-modify-save transaction with a stale whole-file rewrite.
    load_settings_file(&settings_path(app)?)
}

pub(crate) fn load_settings_file(path: &Path) -> Result<(GatewaySettings, bool), String> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((default_desktop_gateway_settings(), false));
        }
        Err(error) => return Err(format!("プロバイダ設定を読めません: {error}")),
    };
    let (settings, migrated) = parse_gateway_settings_json(content.as_bytes())
        .map_err(|error| format!("プロバイダ設定を移行できません: {error}"))?;
    Ok((settings, migrated))
}

fn default_desktop_gateway_settings() -> GatewaySettings {
    let mut settings = GatewaySettings::default();
    if cfg!(feature = "validation-build") {
        settings.runtime.port = 43_431;
    }
    settings
}

pub(crate) fn import_local_cli_sessions_for_start(
    app: &AppHandle,
    settings: &mut GatewaySettings,
) -> Result<bool, String> {
    configure_desktop_auth_store(app)?;
    if !super::cli_consent::cli_login_reuse_allowed(app) {
        return Ok(false);
    }
    let adopted = adopt_local_cli_sessions(settings)?;
    // DeepAI Code's loopback port and token change every time that app starts,
    // so its connection is re-pointed from the running app at each CODETAS start.
    let deepai = dirs::home_dir()
        .map(|home| apply_deepai_runtime(settings, &home))
        .transpose()?
        .unwrap_or(false);
    Ok(adopted || deepai)
}

pub(crate) fn startup_settings_need_persist(migrated: bool, adopted: bool) -> bool {
    migrated || adopted
}

pub(crate) fn save_settings(app: &AppHandle, settings: &GatewaySettings) -> Result<(), String> {
    let path = settings_path(app)?;
    let content = serde_json::to_vec_pretty(settings)
        .map_err(|error| format!("プロバイダ設定を生成できません: {error}"))?;
    let catalog_snapshot = sync_owned_codex_catalog(app, settings)?;
    if let Err(error) = atomic_write(&path, &content) {
        if let Some((catalog_path, previous_catalog)) = catalog_snapshot {
            if let Err(restore) = restore_optional_file(&catalog_path, previous_catalog.as_deref())
            {
                return Err(format!(
                    "プロバイダ設定を保存できず、Codexカタログの復元にも失敗しました: {error}; {restore}"
                ));
            }
        }
        return Err(error);
    }
    Ok(())
}

pub(crate) fn sync_owned_codex_catalog(
    app: &AppHandle,
    settings: &GatewaySettings,
) -> Result<Option<(PathBuf, Option<Vec<u8>>)>, String> {
    let snapshot = snapshot_owned_codex_catalog(app, settings)?;
    let Some((catalog_path, previous)) = snapshot else {
        return Ok(None);
    };
    let content = serialize_codex_catalog(settings, None)?;
    atomic_write(&catalog_path, &content)?;
    Ok(Some((catalog_path, previous)))
}

pub(crate) fn snapshot_owned_codex_catalog(
    app: &AppHandle,
    settings: &GatewaySettings,
) -> Result<Option<(PathBuf, Option<Vec<u8>>)>, String> {
    if !settings.codex.auto_sync_catalog {
        return Ok(None);
    }
    let journal_path = codex_journal_path(app)?;
    let Some(journal) = read_codex_journal(&journal_path)? else {
        return Ok(None);
    };
    let (_, catalog_path, _, _) = validate_codex_journal_paths(app, &journal)?;
    let previous = read_optional_file(&catalog_path)?;
    Ok(Some((catalog_path, previous)))
}

pub(crate) fn snapshot_codex_catalog_transition(
    app: &AppHandle,
    previous: &GatewaySettings,
    next: &GatewaySettings,
) -> Result<Option<(PathBuf, Option<Vec<u8>>)>, String> {
    if !previous.codex.auto_sync_catalog && !next.codex.auto_sync_catalog {
        return Ok(None);
    }
    let mut ownership_settings = next.clone();
    ownership_settings.codex.auto_sync_catalog = true;
    snapshot_owned_codex_catalog(app, &ownership_settings)
}

pub(crate) fn restore_file_snapshot(
    snapshot: Option<&(PathBuf, Option<Vec<u8>>)>,
) -> Result<(), String> {
    match snapshot {
        Some((path, content)) => restore_optional_file(path, content.as_deref()),
        None => Ok(()),
    }
}

pub(crate) fn restore_settings_and_catalog(
    app: &AppHandle,
    previous: &GatewaySettings,
    catalog_snapshot: Option<&(PathBuf, Option<Vec<u8>>)>,
) -> Vec<String> {
    let mut errors = Vec::new();
    if let Err(error) = write_settings_only(app, previous) {
        errors.push(format!("旧設定ファイル: {error}"));
    }
    if let Err(error) = restore_file_snapshot(catalog_snapshot) {
        errors.push(format!("旧Codexカタログ: {error}"));
    }
    errors
}

pub(crate) fn write_settings_only(
    app: &AppHandle,
    settings: &GatewaySettings,
) -> Result<(), String> {
    let content = serde_json::to_vec_pretty(settings)
        .map_err(|error| format!("プロバイダ設定を生成できません: {error}"))?;
    atomic_write(&settings_path(app)?, &content)
}

pub(crate) fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|directory| directory.join("providers.json"))
        .map_err(|error| format!("CODETAS設定フォルダを特定できません: {error}"))
}

pub(crate) fn observability_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|directory| directory.join("observability"))
        .map_err(|error| format!("CODETAS観測フォルダを特定できません: {error}"))
}

pub(crate) fn gateway_runtime_state_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(settings_path(app)?.with_file_name("gateway-runtime.json"))
}

pub(crate) fn runtime_gateway_url(app: &AppHandle) -> Option<String> {
    let path = gateway_runtime_state_path(app).ok()?;
    let metadata = fs::symlink_metadata(&path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 64 * 1024 {
        return None;
    }
    let value = serde_json::from_slice::<JsonValue>(&fs::read(path).ok()?).ok()?;
    if value.get("version").and_then(JsonValue::as_u64) != Some(1) {
        return None;
    }
    let url = value.get("url").and_then(JsonValue::as_str)?;
    let safe_loopback = url.starts_with("http://127.0.0.1:")
        || url.starts_with("http://[::1]:")
        || url.starts_with("http://localhost:");
    (safe_loopback && url.ends_with("/v1") && url.len() <= 512).then(|| url.to_string())
}

pub(crate) fn remove_owned_directory(path: &Path) -> Result<bool, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("CODETAS所有フォルダを確認できません: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("CODETAS所有フォルダは通常ディレクトリではないため削除しません".into());
    }
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("CODETAS所有フォルダを削除できません: {error}")),
    }
}

/// DeepAI Code binds a new loopback port every time it starts, and a request
/// refuses to send the capability anywhere but the endpoint it was read from.
/// A CODETAS process that outlives that restart would answer 502 until someone
/// reopened the app, so this re-imports the running app's endpoint whenever it
/// differs from the stored one. The model list is re-read on every check too, so
/// models DeepAI Code adds later reach the Codex picker without a restart.
///
/// Returns whether settings were changed. Nothing is written when the app has
/// no runtime config, when the endpoint is unchanged, or when the user removed
/// the connection; `apply_deepai_runtime` owns those rules.
pub(crate) async fn refresh_deepai_runtime(
    app: &AppHandle,
    manager: &State<'_, GatewayManager>,
) -> Result<bool, String> {
    let Some(home) = dirs::home_dir() else {
        return Ok(false);
    };
    let Some(live) = codetas_gateway::deepai_runtime_endpoint(&home) else {
        return Ok(false);
    };
    let endpoint_changed =
        codetas_gateway::stored_deepai_endpoint().as_deref() != Some(live.as_str());
    let _mutation = manager.settings_mutation.lock().await;
    let previous = load_settings(app)?;
    // Keeping an existing connection pointed at the running app is not a new import;
    // adding one is, and waits for the user's consent.
    if !super::cli_consent::cli_login_reuse_allowed(app)
        && previous
            .providers
            .iter()
            .all(|provider| provider.id != "deepai")
    {
        return Ok(false);
    }
    let mut next = previous.clone();
    if !apply_deepai_runtime(&mut next, &home)? {
        return Ok(false);
    }
    next.validate()?;
    persist_and_apply_settings(app, manager, &previous, &next).await?;
    eprintln!(
        "CODETAS: re-imported DeepAI Code at {live} ({})",
        if endpoint_changed {
            "endpoint changed"
        } else {
            "models changed"
        }
    );
    Ok(true)
}

/// How often the running DeepAI Code app is re-checked. The app writes its new
/// port on startup, so a short interval keeps the gap to one or two requests.
const DEEPAI_RUNTIME_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Watches the DeepAI Code loopback endpoint for the life of the process.
///
/// This runs independently of the gateway state so a restart of DeepAI Code is
/// picked up whether or not the gateway is currently serving. Failures are
/// logged and retried on the next tick; a transient read must not stop the
/// watcher.
pub(crate) fn spawn_deepai_runtime_watcher(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(DEEPAI_RUNTIME_POLL_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let manager = app.state::<GatewayManager>();
            if let Err(error) = refresh_deepai_runtime(&app, &manager).await {
                eprintln!("CODETAS: DeepAI Code endpoint refresh failed: {error}");
            }
        }
    });
}

#[cfg(test)]
mod runtime_transition_tests {
    use super::*;

    /// The watcher must not rewrite settings for an unchanged endpoint, and it
    /// must not invent one when DeepAI Code has never written its config. Both
    /// paths return before the mutation lock, so they are safe to call without
    /// a running gateway.
    #[test]
    fn deepai_endpoint_comparison_ignores_an_unchanged_port() {
        let home = std::env::temp_dir().join(format!(
            "codetas-deepai-watch-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        if home.exists() {
            fs::remove_dir_all(&home).unwrap();
        }
        let directory = home.join(".deepai-code/userdata/deepai/pi/deepai");
        fs::create_dir_all(&directory).unwrap();
        assert_eq!(codetas_gateway::deepai_runtime_endpoint(&home), None);
        fs::write(
            directory.join("models.json"),
            r#"{"providers":{"deepai":{"baseUrl":"http://127.0.0.1:52003","apiKey":"deepai-key","models":[{"id":"kimi-k3"}]}}}"#,
        )
        .unwrap();
        let live = codetas_gateway::deepai_runtime_endpoint(&home).unwrap();
        assert_eq!(live, "http://127.0.0.1:52003");
        // The same value twice is what the poll sees while the app is idle.
        assert_eq!(
            codetas_gateway::deepai_runtime_endpoint(&home).as_deref(),
            Some(live.as_str())
        );
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn memory_budget_change_restarts_only_the_embedded_gateway() {
        let previous = GatewaySettings::default();
        let mut next = previous.clone();
        next.runtime.memory_budget_bytes *= 2;
        assert!(requires_embedded_gateway_restart(&previous, &next));

        next.runtime.standalone_service = true;
        assert!(!requires_embedded_gateway_restart(&previous, &next));

        let unchanged = previous.clone();
        assert!(!requires_embedded_gateway_restart(&previous, &unchanged));
    }

    #[test]
    fn default_settings_load_path_does_not_adopt_cli_providers() {
        let settings = default_desktop_gateway_settings();
        let expected = GatewaySettings::default();

        assert_eq!(settings.providers.len(), expected.providers.len());
        assert_eq!(
            settings
                .providers
                .iter()
                .map(|provider| provider.id.as_str())
                .collect::<Vec<_>>(),
            expected
                .providers
                .iter()
                .map(|provider| provider.id.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(settings.default_provider, expected.default_provider);
    }

    #[test]
    fn pure_parse_reports_pending_v1_migration_without_persisting_it() {
        let (settings, migrated) =
            parse_gateway_settings_json(br#"{"version":1,"defaultProvider":null,"providers":[]}"#)
                .expect("v1 settings migration");

        assert!(migrated);
        assert_ne!(settings.version, 1);
    }

    #[test]
    fn a_settings_file_that_does_not_parse_is_an_error_and_a_missing_one_is_not() {
        let unique = ATOMIC_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "codetas-settings-file-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("providers.json");

        assert!(load_settings_file(&path).is_ok());

        fs::write(&path, "{ not json").unwrap();
        assert!(load_settings_file(&path).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_live_published_gateway_reports_the_url_it_bound() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let unique = ATOMIC_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "codetas-published-url-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("gateway-runtime.json");
        let publish = |pid: u32, port: u16| {
            fs::write(
                &path,
                format!(r#"{{"version":1,"pid":{pid},"url":"http://127.0.0.1:{port}/v1"}}"#),
            )
            .unwrap();
        };
        publish(std::process::id(), port);
        assert_eq!(
            published_gateway_url_if_live(&path),
            Some(format!("http://127.0.0.1:{port}/v1"))
        );

        // The port still answers, but the process that published it is gone.
        let mut child = Command::new("true").spawn().unwrap();
        let exited = child.id();
        child.wait().unwrap();
        publish(exited, port);
        assert_eq!(published_gateway_url_if_live(&path), None);

        // Tests running alongside can take a freed port at once, so try a few.
        drop(listener);
        let closed_port_is_not_live = (0..20).any(|_| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            publish(std::process::id(), port);
            published_gateway_url_if_live(&path).is_none()
        });
        assert!(closed_port_is_not_live);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn startup_persists_migrations_even_without_cli_adoption() {
        assert!(startup_settings_need_persist(true, false));
        assert!(startup_settings_need_persist(false, true));
        assert!(!startup_settings_need_persist(false, false));
    }

    #[test]
    fn published_runtime_state_accepts_loopback_gateway() {
        let parsed = parse_published_runtime_state(
            br#"{
              "version": 1,
              "instanceId": "aeb8e671-d6da-428b-8a12-8d70a135f08f",
              "pid": 55418,
              "configuredPort": 42421,
              "actualPort": 42421,
              "url": "http://127.0.0.1:42421/v1",
              "startedAtUnixMs": 1
            }"#,
        )
        .expect("loopback runtime state");
        assert_eq!(parsed.pid, 55418);
        assert_eq!(parsed.url, "http://127.0.0.1:42421/v1");
        assert_eq!(parsed.host_port, "127.0.0.1:42421");
    }

    #[test]
    fn published_runtime_state_rejects_non_loopback_and_stale_pid() {
        assert!(parse_published_runtime_state(
            br#"{"version":1,"pid":12,"url":"http://example.invalid:42421/v1"}"#
        )
        .is_none());
        assert!(parse_published_runtime_state(
            br#"{"version":1,"pid":0,"url":"http://127.0.0.1:42421/v1"}"#
        )
        .is_none());
        assert!(process_exists(std::process::id()));
        assert!(!process_exists(4_294_967_294));
    }
}
