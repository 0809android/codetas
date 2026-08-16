use super::*;

#[tauri::command]
pub async fn install_provider_preset(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    input: PresetInstallInput,
) -> Result<GatewayStatus, String> {
    let _mutation = manager.settings_mutation.lock().await;
    let preset = provider_presets()
        .into_iter()
        .find(|preset| preset.id == input.preset_id)
        .ok_or_else(|| format!("unknown provider preset: {}", input.preset_id))?;
    let provider = preset.instantiate(input.base_url.as_deref())?;
    let provider_id = provider.id.clone();
    let previous = load_settings(&app)?;
    let mut settings = previous.clone();
    if let Some(existing) = settings
        .providers
        .iter_mut()
        .find(|item| item.id == provider_id)
    {
        *existing = provider;
    } else {
        settings.providers.push(provider);
    }
    if input.make_default || settings.default_provider.is_none() {
        settings.default_provider = Some(provider_id);
    }
    clients::reconcile_claude_desktop_profile(&mut settings)?;
    settings.validate()?;
    persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
    let running = runtime_is_running(&manager, &settings).await?;
    status(&app, running, settings)
}

#[tauri::command]
pub async fn refresh_gateway_provider_models(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    provider_id: String,
) -> Result<GatewaySettings, String> {
    let _mutation = manager.settings_mutation.lock().await;
    let previous = load_settings(&app)?;
    let mut settings = previous.clone();
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.id == provider_id && provider.enabled)
        .cloned()
        .ok_or_else(|| "enabled provider was not found".to_string())?;
    let discovered = discover_provider_models(&provider)
        .await
        .map_err(|error| error.to_string())?;
    let mut merged = settings
        .model_catalog
        .iter()
        .filter(|model| model.provider_id == provider_id)
        .cloned()
        .map(|model| (model.model_id.clone(), model))
        .collect::<BTreeMap<_, _>>();
    for model in discovered {
        merged.entry(model.model_id.clone()).or_insert(model);
    }
    let discovered_ids = merged.keys().cloned().collect::<Vec<_>>();

    settings
        .model_catalog
        .retain(|model| model.provider_id != provider_id);
    settings.model_catalog.extend(merged.into_values());
    if let Some(current) = settings
        .providers
        .iter_mut()
        .find(|item| item.id == provider_id)
    {
        current.models = discovered_ids;
    }
    clients::reconcile_claude_desktop_profile(&mut settings)?;
    settings.validate()?;
    persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
    Ok(settings)
}

#[tauri::command]
pub async fn sync_codex_model_catalog(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<String, String> {
    let _mutation = manager.settings_mutation.lock().await;
    let previous = load_settings(&app)?;
    let settings = previous.clone();
    let path = codex_home()?.join("codetas-model-catalog.json");
    let previous_catalog = read_optional_file(&path)?;
    let configured = codex_gateway_is_configured(&app, &settings)?;
    let journal_path = codex_journal_path(&app)?;
    let journal = read_codex_journal(&journal_path)?
        .ok_or("Codex接続を先に適用し、CODETASの所有情報を作成してください")?;
    validate_codex_journal_paths(&app, &journal)?;
    let config_path = codex_config_path()?;
    let previous_config = configured
        .then(|| read_optional_file(&config_path))
        .transpose()?
        .flatten();
    let path = write_codex_catalog(&settings)?;
    let result = (|| {
        if configured {
            set_codex_catalog_path(&path)?;
        }
        save_settings(&app, &settings)
    })();
    if let Err(error) = result {
        let mut restore_errors = Vec::new();
        if let Err(restore) = save_settings(&app, &previous) {
            restore_errors.push(format!("旧設定: {restore}"));
        }
        if configured {
            if let Err(restore) = restore_optional_file(&config_path, previous_config.as_deref()) {
                restore_errors.push(format!("旧Codex設定: {restore}"));
            }
        }
        if let Err(restore) = restore_optional_file(&path, previous_catalog.as_deref()) {
            restore_errors.push(format!("旧カタログ: {restore}"));
        }
        return Err(if restore_errors.is_empty() {
            format!("Codexモデルカタログを同期できないため旧状態へ復元しました: {error}")
        } else {
            format!(
                "Codexモデルカタログを同期できず、旧状態の復元にも問題がありました: {error}; {}",
                restore_errors.join("; ")
            )
        });
    }
    Ok(path.to_string_lossy().into_owned())
}

#[tauri::command]
pub fn gateway_configuration(app: AppHandle) -> Result<GatewaySettings, String> {
    load_settings(&app)
}

#[tauri::command]
pub fn oauth_provider_registry() -> Vec<codetas_gateway::OAuthProviderDescriptor> {
    codetas_gateway::oauth_provider_registry().to_vec()
}

#[tauri::command]
pub fn gateway_compatibility_lab(app: AppHandle) -> Result<codetas_gateway::CompatibilityLabReport, String> {
    let settings = load_settings(&app)?;
    Ok(codetas_gateway::compatibility_lab_report(&settings))
}

#[tauri::command]
pub async fn gateway_route_dry_runs(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<Vec<codetas_gateway::RouteDryRunReport>, String> {
    let settings = load_settings(&app)?;
    let route_ids = settings.routes.iter().map(|route| route.id.clone()).collect::<Vec<_>>();
    let guard = manager.handle.lock().await;
    if let Some(handle) = guard.as_ref() {
        let mut reports = Vec::with_capacity(route_ids.len());
        for route_id in route_ids {
            reports.push(handle.route_dry_run(&route_id, false).await?);
        }
        return Ok(reports);
    }
    Err("live gateway route dry-run is unavailable while the gateway is disconnected".into())
}

#[tauri::command]
pub async fn check_for_codetas_update(app: AppHandle) -> Result<UpdateCheck, String> {
    let settings = load_settings(&app)?;
    check_signed_update(&settings.updates, env!("CARGO_PKG_VERSION")).await
}

#[tauri::command]
pub async fn install_codetas_update(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<(), String> {
    let _mutation = manager.settings_mutation.lock().await;
    let settings = load_settings(&app)?;
    let signed = check_signed_update(&settings.updates, env!("CARGO_PKG_VERSION")).await?;
    if !signed.update_available {
        return Err("利用できるCODETAS更新はありません".into());
    }
    let endpoint = settings
        .updates
        .installer_endpoint
        .as_deref()
        .ok_or("更新インストーラのendpointが設定されていません")?;
    let installer_public_key = settings
        .updates
        .installer_public_key
        .as_deref()
        .ok_or("更新インストーラの公開鍵が設定されていません")?;
    let endpoint = tauri::Url::parse(endpoint)
        .map_err(|_| "更新インストーラのendpointが不正です".to_string())?;
    let updater = app
        .updater_builder()
        .endpoints(vec![endpoint])
        .map_err(|error| format!("更新endpointを構成できません: {error}"))?
        .pubkey(installer_public_key)
        .build()
        .map_err(|error| format!("更新検証器を構成できません: {error}"))?;
    let update = updater
        .check()
        .await
        .map_err(|error| format!("更新インストーラを確認できません: {error}"))?
        .ok_or("署名済みmanifestと一致する更新インストーラがありません")?;
    if update.version != signed.manifest.version {
        return Err("更新manifestとインストーラのバージョンが一致しません".into());
    }
    let expected_url = tauri::Url::parse(&signed.manifest.download_url)
        .map_err(|_| "署名済み更新URLが不正です".to_string())?;
    if update.download_url != expected_url {
        return Err("更新manifestとインストーラのダウンロードURLが一致しません".into());
    }

    let bytes = update
        .download(|_, _| {}, || {})
        .await
        .map_err(|error| format!("署名済み更新をダウンロードできません: {error}"))?;
    if bytes.len() as u64 != signed.manifest.artifact_size_bytes {
        return Err("更新ファイルのサイズが署名済みmanifestと一致しません".into());
    }
    let digest = Sha256::digest(&bytes);
    let actual_sha256 = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if !actual_sha256.eq_ignore_ascii_case(&signed.manifest.sha256) {
        return Err("更新ファイルのSHA-256が署名済みmanifestと一致しません".into());
    }

    let prior_handle = manager.handle.lock().await.take();
    let embedded_was_running = prior_handle.is_some();
    if let Some(handle) = prior_handle {
        handle.shutdown().await;
    }
    let service_was_running = settings.runtime.standalone_service && service::status()?.running;
    if service_was_running && !service::stop()? {
        if embedded_was_running {
            let handle = start_managed_gateway(&app, settings.clone()).await?;
            *manager.handle.lock().await = Some(handle);
        }
        return Err("更新前に常駐Gatewayを停止できませんでした".into());
    }

    if let Err(error) = update.install(&bytes) {
        let mut restore_errors = Vec::new();
        if service_was_running {
            match service::start() {
                Ok(true) => {}
                Ok(false) => {
                    restore_errors.push("常駐Gateway: 起動操作後の稼働状態を確認できません".into())
                }
                Err(restore_error) => {
                    restore_errors.push(format!("常駐Gateway: {restore_error}"));
                }
            }
        } else if embedded_was_running {
            match start_managed_gateway(&app, settings).await {
                Ok(handle) => *manager.handle.lock().await = Some(handle),
                Err(restore_error) => {
                    restore_errors.push(format!("埋め込みGateway: {restore_error}"))
                }
            }
        }
        let restore = if restore_errors.is_empty() {
            "停止したGatewayは復元しました".to_string()
        } else {
            format!("Gateway復元エラー: {}", restore_errors.join("; "))
        };
        return Err(format!("更新を適用できません: {error}; {restore}"));
    }

    app.restart();
}
