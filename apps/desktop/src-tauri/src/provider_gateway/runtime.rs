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

pub(crate) async fn persist_and_apply_settings(
    app: &AppHandle,
    manager: &State<'_, GatewayManager>,
    previous: &GatewaySettings,
    next: &GatewaySettings,
) -> Result<(), String> {
    let catalog_snapshot = snapshot_codex_catalog_transition(app, previous, next)?;
    save_settings(app, next)?;
    if let Err(error) = update_running_gateway(manager, next).await {
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
    Ok(())
}

pub(crate) async fn runtime_is_running(
    manager: &State<'_, GatewayManager>,
    settings: &GatewaySettings,
) -> Result<bool, String> {
    if manager.handle.lock().await.is_some() {
        return Ok(true);
    }
    if settings.runtime.standalone_service {
        return Ok(service::status()?.running);
    }
    Ok(false)
}

pub(crate) fn status(
    app: &AppHandle,
    running: bool,
    settings: GatewaySettings,
) -> Result<GatewayStatus, String> {
    let gateway_url = if running {
        runtime_gateway_url(app).unwrap_or_else(|| gateway_url(&settings))
    } else {
        gateway_url(&settings)
    };
    let codex_configured = codex_gateway_is_configured(app, &settings).unwrap_or(false);
    Ok(GatewayStatus {
        running,
        url: gateway_url,
        providers: settings.providers,
        default_provider: settings.default_provider,
        codex_configured,
        settings_path: Some(settings_path(app)?.to_string_lossy().into_owned()),
    })
}

pub(crate) async fn start_managed_gateway(
    app: &AppHandle,
    settings: GatewaySettings,
) -> Result<GatewayHandle, String> {
    let configured_url = gateway_url(&settings);
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
    let actual_url = handle.url();
    if actual_url != configured_url {
        reconcile_owned_codex_runtime_url(app, &actual_url)?;
    }
    Ok(handle)
}

pub(crate) fn load_settings(app: &AppHandle) -> Result<GatewaySettings, String> {
    configure_desktop_auth_store(app)?;
    let path = settings_path(app)?;
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut settings = GatewaySettings::default();
            if cfg!(feature = "validation-build") {
                settings.runtime.port = 43_431;
            }
            if adopt_local_cli_sessions(&mut settings).unwrap_or(false) {
                let _ = save_settings(app, &settings);
            }
            return Ok(settings);
        }
        Err(error) => return Err(format!("プロバイダ設定を読めません: {error}")),
    };
    let (mut settings, migrated) = parse_gateway_settings_json(content.as_bytes())
        .map_err(|error| format!("プロバイダ設定を移行できません: {error}"))?;
    let adopted = adopt_local_cli_sessions(&mut settings).unwrap_or(false);
    if migrated || adopted {
        save_settings(app, &settings)?;
    }
    Ok(settings)
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
