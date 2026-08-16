use super::*;

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayManagementPatch {
    memory_budget_bytes: Option<u64>,
    max_inflight_requests: Option<u32>,
    selected_models: Option<Vec<String>>,
    model_picker_order: Option<Vec<String>>,
    account_controls: Option<Vec<AccountControlPatch>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountControlPatch {
    provider_id: String,
    account_id: String,
    priority: Option<i16>,
    enabled: Option<bool>,
    paused: Option<bool>,
    #[serde(default)]
    pause_until_unix: PatchField<Option<u64>>,
    pinned: Option<bool>,
}

#[derive(Default)]
enum PatchField<T> {
    #[default]
    Missing,
    Present(T),
}

impl<'de, T> Deserialize<'de> for PatchField<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        T::deserialize(deserializer).map(Self::Present)
    }
}

#[tauri::command]
pub async fn patch_gateway_management(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    patch: GatewayManagementPatch,
) -> Result<GatewaySettings, String> {
    let _mutation = manager.settings_mutation.lock().await;
    let previous = load_settings(&app)?;
    let mut next = previous.clone();
    apply_gateway_management_patch(&mut next, patch)?;
    clients::reconcile_claude_desktop_profile(&mut next)?;
    next.validate()?;
    persist_and_apply_settings(&app, &manager, &previous, &next).await?;
    Ok(next)
}

fn apply_gateway_management_patch(
    next: &mut GatewaySettings,
    patch: GatewayManagementPatch,
) -> Result<(), String> {
    if let Some(value) = patch.memory_budget_bytes { next.runtime.memory_budget_bytes = value; }
    if let Some(value) = patch.max_inflight_requests { next.runtime.max_inflight_requests = value; }
    if let Some(value) = patch.selected_models { next.catalog.selected_models = value; }
    if let Some(value) = patch.model_picker_order { next.catalog.model_picker_order = value; }
    for control in patch.account_controls.unwrap_or_default() {
        let account_index = next.account_pool.accounts.iter().position(|item| {
            item.provider_id == control.provider_id && item.id == control.account_id
        }).ok_or_else(|| format!("unknown account: {}/{}", control.provider_id, control.account_id))?;
        let current = &next.account_pool.accounts[account_index];
        let enabled = control.enabled.unwrap_or(current.enabled);
        let paused = control.paused.unwrap_or(current.paused);
        let pause_until_unix = if control.paused == Some(false) {
            None
        } else {
            match &control.pause_until_unix {
                PatchField::Missing => current.pause_until_unix,
                PatchField::Present(value) => *value,
            }
        };
        let requested_pinned = control.pinned.unwrap_or(current.pinned);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let currently_paused = paused && pause_until_unix.is_none_or(|until| until > now);
        let pause_requested = control.paused == Some(true);
        if control.pinned == Some(true) && (!enabled || pause_requested || currently_paused) {
            return Err(format!(
                "cannot pin disabled or paused account: {}/{}",
                control.provider_id, control.account_id
            ));
        }
        let pinned = requested_pinned && enabled && !pause_requested && !currently_paused;
        if pinned {
            for account in next
                .account_pool
                .accounts
                .iter_mut()
                .filter(|account| account.provider_id == control.provider_id)
            {
                account.pinned = false;
            }
        }
        let account = &mut next.account_pool.accounts[account_index];
        if let Some(value) = control.priority { account.priority = value; }
        account.enabled = enabled;
        account.paused = paused;
        account.pause_until_unix = pause_until_unix;
        account.pinned = pinned;
        if !enabled || pause_requested || currently_paused {
            account.pinned = false;
            if next
                .account_pool
                .active_accounts
                .get(&control.provider_id)
                .is_some_and(|active| active == &control.account_id)
            {
                next.account_pool.active_accounts.remove(&control.provider_id);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod management_patch_tests {
    use super::*;
    use codetas_gateway::AccountReference;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    #[test]
    fn focused_patch_updates_only_requested_fields_without_registry_revision_locking() {
        let mut settings = GatewaySettings::default();
        settings.registry_revision = 2;
        settings.runtime.port = 43_210;
        settings.catalog.selected_models = vec!["old/model".into()];
        settings.default_provider = Some("keep-provider".into());

        apply_gateway_management_patch(&mut settings, GatewayManagementPatch {
            selected_models: Some(vec!["new/model".into()]),
            ..GatewayManagementPatch::default()
        }).expect("focused patch");

        assert_eq!(settings.catalog.selected_models, vec!["new/model".to_string()]);
        assert_eq!(settings.registry_revision, 2);
        assert_eq!(settings.runtime.port, 43_210);
        assert_eq!(settings.default_provider.as_deref(), Some("keep-provider"));
        assert!(settings.providers.is_empty());
    }

    fn account_settings() -> GatewaySettings {
        let mut settings = GatewaySettings::default();
        settings.account_pool.accounts = vec![
            AccountReference {
                id: "primary".into(),
                provider_id: "provider".into(),
                label: "Primary".into(),
                pinned: true,
                ..AccountReference::default()
            },
            AccountReference {
                id: "backup".into(),
                provider_id: "provider".into(),
                label: "Backup".into(),
                ..AccountReference::default()
            },
        ];
        settings
            .account_pool
            .active_accounts
            .insert("provider".into(), "primary".into());
        settings
    }

    fn account_patch(account_id: &str) -> AccountControlPatch {
        AccountControlPatch {
            provider_id: "provider".into(),
            account_id: account_id.into(),
            priority: None,
            enabled: None,
            paused: None,
            pause_until_unix: PatchField::Missing,
            pinned: None,
        }
    }

    #[test]
    fn pausing_or_disabling_clears_pin_and_active_account() {
        for (enabled, paused, pause_until_unix) in [
            (None, Some(true), None),
            (None, Some(true), Some(Some(0))),
            (Some(false), None, None),
        ] {
            let mut settings = account_settings();
            let mut control = account_patch("primary");
            control.enabled = enabled;
            control.paused = paused;
            control.pause_until_unix = pause_until_unix
                .map(PatchField::Present)
                .unwrap_or(PatchField::Missing);
            apply_gateway_management_patch(&mut settings, GatewayManagementPatch {
                account_controls: Some(vec![control]),
                ..GatewayManagementPatch::default()
            }).expect("account control");

            assert!(!settings.account_pool.accounts[0].pinned);
            assert!(!settings.account_pool.active_accounts.contains_key("provider"));
        }
    }

    #[test]
    fn pinning_atomically_unpins_provider_siblings() {
        let mut settings = account_settings();
        let mut control = account_patch("backup");
        control.pinned = Some(true);

        apply_gateway_management_patch(&mut settings, GatewayManagementPatch {
            account_controls: Some(vec![control]),
            ..GatewayManagementPatch::default()
        }).expect("pin backup");

        assert!(!settings.account_pool.accounts[0].pinned);
        assert!(settings.account_pool.accounts[1].pinned);
    }

    #[test]
    fn resuming_always_clears_pause_deadline() {
        let mut settings = account_settings();
        settings.account_pool.accounts[0].paused = true;
        settings.account_pool.accounts[0].pause_until_unix = Some(u64::MAX);
        settings.account_pool.accounts[0].pinned = false;
        let mut control = account_patch("primary");
        control.paused = Some(false);

        apply_gateway_management_patch(&mut settings, GatewayManagementPatch {
            account_controls: Some(vec![control]),
            ..GatewayManagementPatch::default()
        }).expect("resume account");

        assert!(!settings.account_pool.accounts[0].paused);
        assert_eq!(settings.account_pool.accounts[0].pause_until_unix, None);
    }

    #[test]
    fn pause_deadline_patch_preserves_missing_but_distinguishes_null_and_number() {
        for (raw, expected) in [
            (json!({"providerId": "provider", "accountId": "primary"}), Some(50)),
            (json!({"providerId": "provider", "accountId": "primary", "pauseUntilUnix": null}), None),
            (json!({"providerId": "provider", "accountId": "primary", "pauseUntilUnix": 75}), Some(75)),
        ] {
            let mut settings = account_settings();
            settings.account_pool.accounts[0].pause_until_unix = Some(50);
            let control = serde_json::from_value::<AccountControlPatch>(raw)
                .expect("account control patch");

            apply_gateway_management_patch(&mut settings, GatewayManagementPatch {
                account_controls: Some(vec![control]),
                ..GatewayManagementPatch::default()
            }).expect("apply deadline patch");

            assert_eq!(settings.account_pool.accounts[0].pause_until_unix, expected);
        }
    }

    #[test]
    fn pausing_with_explicit_null_creates_an_indefinite_pause() {
        let mut settings = account_settings();
        let control = serde_json::from_value::<AccountControlPatch>(json!({
            "providerId": "provider",
            "accountId": "primary",
            "paused": true,
            "pauseUntilUnix": null
        })).expect("account control patch");

        apply_gateway_management_patch(&mut settings, GatewayManagementPatch {
            account_controls: Some(vec![control]),
            ..GatewayManagementPatch::default()
        }).expect("apply indefinite pause");

        assert!(settings.account_pool.accounts[0].paused);
        assert_eq!(settings.account_pool.accounts[0].pause_until_unix, None);
        assert!(!settings.account_pool.accounts[0].pinned);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shared_mutation_lock_preserves_disjoint_concurrent_patches() {
        let manager = Arc::new(GatewayManager::default());
        let settings = Arc::new(Mutex::new(GatewaySettings::default()));
        let apply = |manager: Arc<GatewayManager>,
                     settings: Arc<Mutex<GatewaySettings>>,
                     patch: GatewayManagementPatch| {
            tokio::spawn(async move {
                let _mutation = manager.settings_mutation.lock().await;
                let mut next = settings.lock().await.clone();
                tokio::task::yield_now().await;
                apply_gateway_management_patch(&mut next, patch).expect("management patch");
                *settings.lock().await = next;
            })
        };
        let memory = apply(
            Arc::clone(&manager),
            Arc::clone(&settings),
            GatewayManagementPatch {
                memory_budget_bytes: Some(123_456),
                ..GatewayManagementPatch::default()
            },
        );
        let inflight = apply(
            Arc::clone(&manager),
            Arc::clone(&settings),
            GatewayManagementPatch {
                max_inflight_requests: Some(17),
                ..GatewayManagementPatch::default()
            },
        );

        memory.await.expect("memory patch task");
        inflight.await.expect("inflight patch task");
        let settings = settings.lock().await;
        assert_eq!(settings.runtime.memory_budget_bytes, 123_456);
        assert_eq!(settings.runtime.max_inflight_requests, 17);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shared_mutation_lock_blocks_patch_while_runtime_handle_is_transitional() {
        let manager = Arc::new(GatewayManager::default());
        let handle_is_none = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let restart_manager = Arc::clone(&manager);
        let restart_state = Arc::clone(&handle_is_none);
        let restart = tokio::spawn(async move {
            let _mutation = restart_manager.settings_mutation.lock().await;
            restart_state.store(true, Ordering::SeqCst);
            started_tx.send(()).expect("restart started");
            release_rx.await.expect("restart release");
            restart_state.store(false, Ordering::SeqCst);
        });
        started_rx.await.expect("restart start signal");

        let patch_manager = Arc::clone(&manager);
        let patch_state = Arc::clone(&handle_is_none);
        let patch = tokio::spawn(async move {
            let _mutation = patch_manager.settings_mutation.lock().await;
            assert!(!patch_state.load(Ordering::SeqCst));
        });
        tokio::task::yield_now().await;
        assert!(handle_is_none.load(Ordering::SeqCst));
        release_tx.send(()).expect("release restart");

        restart.await.expect("restart task");
        patch.await.expect("patch task");
    }
}

#[tauri::command]
pub async fn save_gateway_configuration(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    mut configuration: GatewaySettings,
) -> Result<GatewaySettings, String> {
    let _mutation = manager.settings_mutation.lock().await;
    // Provider/route edits can invalidate generated Claude Desktop aliases. Rebuild the
    // owned profile before validation so stale aliases never make the settings impossible
    // to save or repair.
    clients::reconcile_claude_desktop_profile(&mut configuration)?;
    configuration.validate()?;
    let previous = load_settings(&app)?;
    configuration.registry_revision = configuration
        .registry_revision
        .max(previous.registry_revision);
    let catalog_snapshot = snapshot_codex_catalog_transition(&app, &previous, &configuration)?;
    if previous.runtime.standalone_service != configuration.runtime.standalone_service {
        return Err("常駐サービスの切替は専用の登録・解除操作を使用してください".into());
    }
    if previous.runtime.standalone_service {
        service::validate_service_settings(&configuration)?;
        save_settings(&app, &configuration)?;
        if let Some(handle) = manager.handle.lock().await.take() {
            handle.shutdown().await;
        }
        if let Err(error) = service::restart().and_then(|running| {
            running
                .then_some(())
                .ok_or_else(|| "再起動後の起動状態を確認できません".to_string())
        }) {
            let mut restore_errors =
                restore_settings_and_catalog(&app, &previous, catalog_snapshot.as_ref());
            let restored = service::restart().and_then(|running| {
                running
                    .then_some(())
                    .ok_or_else(|| "旧設定での再起動後も稼働状態を確認できません".to_string())
            });
            if let Err(restore) = restored {
                restore_errors.push(format!("旧設定での再起動: {restore}"));
            }
            return Err(if restore_errors.is_empty() {
                format!("常駐サービスを再起動できないため旧設定へ復元しました: {error}")
            } else {
                format!(
                    "常駐サービスを再起動できず、旧状態の復元にも失敗しました: {error}; {}",
                    restore_errors.join("; ")
                )
            });
        }
        return Ok(configuration);
    }
    let runtime_restart_required = previous.runtime.port != configuration.runtime.port
        || previous.runtime.host != configuration.runtime.host
        || requires_embedded_gateway_restart(&previous, &configuration);
    if runtime_restart_required {
        let previous_handle = manager.handle.lock().await.take();
        let was_running = previous_handle.is_some();
        if let Some(handle) = previous_handle {
            handle.shutdown().await;
        }
        if let Err(error) = save_settings(&app, &configuration) {
            if was_running {
                match start_managed_gateway(&app, previous).await {
                    Ok(handle) => *manager.handle.lock().await = Some(handle),
                    Err(restore) => {
                        return Err(format!(
                            "設定ファイルを保存できず、旧Gatewayの復元にも失敗しました: {error}; {restore}"
                        ));
                    }
                }
            }
            return Err(error);
        }
        if was_running {
            match start_managed_gateway(&app, configuration.clone()).await {
                Ok(handle) => *manager.handle.lock().await = Some(handle),
                Err(error) => {
                    let mut restore_errors =
                        restore_settings_and_catalog(&app, &previous, catalog_snapshot.as_ref());
                    let restore = start_managed_gateway(&app, previous).await;
                    return Err(match restore {
                        Ok(handle) => {
                            *manager.handle.lock().await = Some(handle);
                            if restore_errors.is_empty() {
                                format!("新しいGateway runtimeを起動できないため旧設定へ復元しました: {error}")
                            } else {
                                format!("新しいGateway runtimeを起動できず、旧永続状態の復元にも失敗しました: {error}; {}", restore_errors.join("; "))
                            }
                        }
                        Err(restore) => {
                            restore_errors.push(format!("旧Gateway: {restore}"));
                            format!("新しいGateway runtimeを起動できず、旧状態の復元にも失敗しました: {error}; {}", restore_errors.join("; "))
                        }
                    });
                }
            }
        }
        return Ok(configuration);
    }

    save_settings(&app, &configuration)?;
    let guard = manager.handle.lock().await;
    if let Some(handle) = guard.as_ref() {
        if let Err(error) = handle.set_settings(configuration.clone()).await {
            let mut restore_errors =
                restore_settings_and_catalog(&app, &previous, catalog_snapshot.as_ref());
            let restore = handle.set_settings(previous).await;
            if let Err(restore) = restore {
                restore_errors.push(format!("旧runtime設定: {restore}"));
            }
            return Err(if restore_errors.is_empty() {
                format!("設定を反映できないため旧設定へ復元しました: {error}")
            } else {
                format!(
                    "設定を反映できず、旧状態の復元にも失敗しました: {error}; {}",
                    restore_errors.join("; ")
                )
            });
        }
    }
    Ok(configuration)
}

pub async fn converge_codex_integration(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<String, String> {
    let _mutation = manager.settings_mutation.lock().await;
    converge_codex_integration_unlocked(app)
}

fn converge_codex_integration_unlocked(app: AppHandle) -> Result<String, String> {
    let settings = load_settings(&app)?;
    if !settings.codex.auto_connect {
        return Ok("Codex auto-connection is disabled".into());
    }
    install_codex_gateway_config_unlocked(app, CodexGatewayInstallInput { model: None })
}

#[tauri::command]
pub async fn apply_agent_preset_configuration(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    input: AgentPresetApplyInput,
) -> Result<GatewaySettings, String> {
    let _mutation = manager.settings_mutation.lock().await;
    let previous = load_settings(&app)?;
    let mut next = previous.clone();
    let vision_model = input.vision_model.trim();
    if vision_model.is_empty() {
        return Err("Visionモデルを選択してください".into());
    }
    let main_model = input
        .main_model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string);
    if let Some(model) = main_model.as_deref() {
        let provider_id = model
            .split_once('/')
            .map(|(provider, _)| provider)
            .ok_or("メインモデルはprovider/model形式で指定してください")?;
        if !next
            .providers
            .iter()
            .any(|provider| provider.enabled && provider.id == provider_id)
        {
            return Err("メインモデルのプロバイダが有効ではありません".into());
        }
        next.default_provider = Some(provider_id.to_string());
    }
    next.agents.image_input_mode = AuxiliaryInputMode::Auto;
    next.agents.video_input_mode = AuxiliaryInputMode::Auto;
    next.agents.document_input_mode = AuxiliaryInputMode::Auto;
    next.sidecars.vision_model = Some(vision_model.to_string());
    next.sidecars.video_input_model = Some(vision_model.to_string());
    next.sidecars.document_model = Some(vision_model.to_string());
    if let Some(image_model) = input
        .image_model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
    {
        next.sidecars.image_model = Some(image_model.to_string());
    }

    // Stage every setting mutation performed by the Codex installer before the
    // first persistence step so the running Gateway receives the final state.
    enable_codex_openai_passthrough(&mut next)?;
    configure_desktop_auth_store(&app)?;
    auto_register_authenticated_cli_providers(&mut next)?;
    next.codex.auto_sync_catalog = true;
    next.codex.auto_connect = true;
    clients::reconcile_claude_desktop_profile(&mut next)?;
    next.validate()?;
    if let Some(model) = main_model.as_deref() {
        let public_model = codex_public_model_id(model);
        if !is_codex_catalog_model(&next, &public_model) {
            return Err(format!("メインモデルがCodexカタログにありません: {model}"));
        }
    }
    persist_and_apply_settings(&app, &manager, &previous, &next).await?;

    if let Err(error) = install_codex_gateway_config_unlocked(
        app.clone(),
        CodexGatewayInstallInput {
            model: main_model.as_deref().map(codex_public_model_id),
        },
    ) {
        let rollback = persist_and_apply_settings(&app, &manager, &next, &previous).await;
        return Err(match rollback {
            Ok(()) => format!("おすすめ構成を適用できないため旧設定へ復元しました: {error}"),
            Err(rollback_error) => format!(
                "おすすめ構成を適用できず、旧設定の復元にも失敗しました: {error}; {rollback_error}"
            ),
        });
    }
    Ok(next)
}

#[tauri::command]
pub async fn start_provider_gateway(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<GatewayStatus, String> {
    let _mutation = manager.settings_mutation.lock().await;
    let mut settings = load_settings(&app)?;
    if import_local_cli_sessions_for_start(&app, &mut settings)? {
        settings.validate()?;
        save_settings(&app, &settings)?;
    }
    // Manual starts also converge Codex in case its config changed while CODETAS
    // was already running. Startup convergence uses the same mutation lock.
    if let Err(error) = converge_codex_integration_unlocked(app.clone()) {
        eprintln!("CODETAS: Codex automatic connection was skipped: {error}");
    }
    let settings = load_settings(&app)?;
    let mut guard = manager.handle.lock().await;
    if settings.runtime.standalone_service {
        let prior_handle = guard.take();
        drop(guard);
        if let Some(handle) = prior_handle {
            handle.shutdown().await;
        }
        let service_status = service::status()?;
        let running = service_status.running || (service_status.installed && service::start()?);
        return status(&app, running, settings);
    }
    if guard.is_none() {
        let handle = start_managed_gateway(&app, settings.clone()).await?;
        *guard = Some(handle);
    } else if let Some(handle) = guard.as_ref() {
        handle.set_settings(settings.clone()).await?;
    }
    status(&app, true, settings)
}

#[tauri::command]
pub async fn stop_provider_gateway(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<GatewayStatus, String> {
    let _mutation = manager.settings_mutation.lock().await;
    let settings = load_settings(&app)?;
    let running = if settings.runtime.standalone_service {
        if service::status()?.installed {
            if !service::stop()? {
                return Err("常駐Gatewayを停止できませんでした".into());
            }
            let running = service::status()?.running;
            if running {
                return Err("常駐Gatewayが停止状態になっていません".into());
            }
            false
        } else {
            false
        }
    } else {
        if let Some(handle) = manager.handle.lock().await.take() {
            handle.shutdown().await;
        }
        false
    };
    status(&app, running, settings)
}

pub async fn shutdown_embedded_gateway(manager: &GatewayManager) {
    if let Some(handle) = manager.handle.lock().await.take() {
        handle.shutdown().await;
    }
}

#[tauri::command]
pub async fn provider_gateway_status(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<GatewayStatus, String> {
    let guard = manager.handle.lock().await;
    let embedded_running = guard.is_some();
    let settings = if let Some(handle) = guard.as_ref() {
        handle.settings().await
    } else {
        load_settings(&app)?
    };
    drop(guard);
    let running =
        embedded_running || (settings.runtime.standalone_service && service::status()?.running);
    status(&app, running, settings)
}

#[tauri::command]
pub async fn upsert_gateway_provider(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    input: ProviderUpsertInput,
) -> Result<GatewayStatus, String> {
    let _mutation = manager.settings_mutation.lock().await;
    input.provider.validate()?;
    let previous = load_settings(&app)?;
    let mut settings = previous.clone();
    let provider_id = input.provider.id.clone();
    if let Some(existing) = settings
        .providers
        .iter_mut()
        .find(|provider| provider.id == provider_id)
    {
        *existing = input.provider;
    } else {
        settings.providers.push(input.provider);
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
pub async fn remove_gateway_provider(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    provider_id: String,
) -> Result<GatewayStatus, String> {
    let _mutation = manager.settings_mutation.lock().await;
    let previous = load_settings(&app)?;
    let mut settings = previous.clone();
    let before = settings.providers.len();
    settings
        .providers
        .retain(|provider| provider.id != provider_id);
    if settings.providers.len() == before {
        return Err("provider was not found".into());
    }
    if settings.default_provider.as_deref() == Some(provider_id.as_str()) {
        settings.default_provider = settings
            .providers
            .iter()
            .find(|provider| provider.enabled)
            .map(|provider| provider.id.clone());
    }
    clients::reconcile_claude_desktop_profile(&mut settings)?;
    settings.validate()?;
    persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
    let running = runtime_is_running(&manager, &settings).await?;
    status(&app, running, settings)
}

#[tauri::command]
pub async fn set_default_gateway_provider(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    provider_id: String,
) -> Result<GatewayStatus, String> {
    let _mutation = manager.settings_mutation.lock().await;
    let previous = load_settings(&app)?;
    let mut settings = previous.clone();
    if !settings
        .providers
        .iter()
        .any(|provider| provider.id == provider_id && provider.enabled)
    {
        return Err("default provider must reference an enabled provider".into());
    }
    settings.default_provider = Some(provider_id);
    settings.validate()?;
    persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
    let running = runtime_is_running(&manager, &settings).await?;
    status(&app, running, settings)
}

#[tauri::command]
pub async fn install_codex_gateway_config(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    input: CodexGatewayInstallInput,
) -> Result<String, String> {
    let _mutation = manager.settings_mutation.lock().await;
    install_codex_gateway_config_unlocked(app, input)
}

fn install_codex_gateway_config_unlocked(
    app: AppHandle,
    input: CodexGatewayInstallInput,
) -> Result<String, String> {
    configure_desktop_auth_store(&app)?;
    let mut settings = load_settings(&app)?;
    let previous_settings = settings.clone();
    if settings.security.require_local_token {
        return Err(
            "Codexの履歴を維持する接続方式ではローカルトークンを使用できません。Gatewayをloopback限定にしてrequireLocalTokenを無効にしてください"
                .into(),
        );
    }
    if !matches!(
        settings.runtime.host.as_str(),
        "localhost" | "127.0.0.1" | "::1"
    ) {
        return Err(
            "Codexの履歴を維持する接続方式はlocalhost、127.0.0.1、または::1でのみ使用できます"
                .into(),
        );
    }
    enable_codex_openai_passthrough(&mut settings)?;
    auto_register_authenticated_cli_providers(&mut settings)?;
    let gateway_base_url = runtime_gateway_url(&app).unwrap_or_else(|| gateway_url(&settings));
    let config_path = codex_config_path()?;
    let original_config = read_optional_file(&config_path)?;
    let existing = match fs::read_to_string(&config_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("Codex設定を読めません: {error}")),
    };
    let mut document = if existing.trim().is_empty() {
        DocumentMut::new()
    } else {
        existing
            .parse::<DocumentMut>()
            .map_err(|error| format!("既存のCodex設定を解析できません: {error}"))?
    };

    let requested_model = input
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(codex_public_model_id);
    let existing_model = document
        .get("model")
        .and_then(Item::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty());
    let model = select_codex_model(&settings, requested_model.as_deref(), existing_model)
        .ok_or("Codexへ接続するには有効なモデルが必要です")?;
    if model.len() > 300 || model.chars().any(char::is_control) {
        return Err("model must be a printable value up to 300 characters".into());
    }
    settings.codex.auto_sync_catalog = true;
    settings.codex.auto_connect = true;
    let installed_agents = if settings.agents.multi_agent_v2 {
        let installed = CodexAgentInstall {
            enabled: true,
            max_concurrent_threads_per_session: i64::from(settings.agents.max_threads),
        };
        if !document.contains_key("agents") {
            document["agents"] = Item::Table(Table::new());
        }
        let agents = document["agents"]
            .as_table_mut()
            .ok_or("agents must be a TOML table")?;
        agents["enabled"] = value(installed.enabled);
        agents["max_concurrent_threads_per_session"] =
            value(installed.max_concurrent_threads_per_session);
        Some(installed)
    } else {
        restore_agents_before_disable(&app, &mut document)?;
        None
    };
    let existing_provider = document.get("model_provider").and_then(Item::as_str);
    if existing_provider
        .is_some_and(|provider| provider != "openai" && provider != GATEWAY_PROVIDER_ID)
    {
        return Err(format!(
            "既存の外部model_providerを上書きしません: {}",
            existing_provider.unwrap_or_default()
        ));
    }
    let catalog_path = codex_home()?.join("codetas-model-catalog.json");
    let original_catalog = read_optional_file(&catalog_path)?;
    let catalog_content = serialize_codex_catalog(&settings, Some(&model))?;
    configure_native_codex_gateway(&mut document, &gateway_base_url, &model, &catalog_path)?;

    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Codex設定フォルダを作れません: {error}"))?;
    }
    let installed_base_url = gateway_base_url;
    let journal_path = codex_journal_path(&app)?;
    let original_journal = read_optional_file(&journal_path)?;
    let mut created_backups = Vec::new();
    let transaction = (|| {
        created_backups = prepare_codex_journal(
            &app,
            &config_path,
            &catalog_path,
            &model,
            &installed_base_url,
            false,
            CodexRoutingMode::OpenAiBaseUrl,
            installed_agents,
        )?;
        atomic_write(&catalog_path, &catalog_content)?;
        atomic_write(&config_path, document.to_string().as_bytes())?;
        save_settings(&app, &settings)
    })();
    if let Err(error) = transaction {
        let mut restore_errors = Vec::new();
        for (label, path, snapshot) in [
            ("Codex設定", &config_path, original_config.as_deref()),
            ("復元journal", &journal_path, original_journal.as_deref()),
        ] {
            if let Err(restore) = restore_optional_file(path, snapshot) {
                restore_errors.push(format!("{label}: {restore}"));
            }
        }
        if let Err(restore) = write_settings_only(&app, &previous_settings) {
            restore_errors.push(format!("CODETAS設定: {restore}"));
        }
        if let Err(restore) = restore_optional_file(&catalog_path, original_catalog.as_deref()) {
            restore_errors.push(format!("モデルカタログ: {restore}"));
        }
        for backup in created_backups {
            match fs::remove_file(backup) {
                Ok(()) => {}
                Err(remove) if remove.kind() == std::io::ErrorKind::NotFound => {}
                Err(remove) => {
                    restore_errors.push(format!("新規バックアップ: {remove}"));
                }
            }
        }
        return Err(if restore_errors.is_empty() {
            format!("Codex接続を適用できないため旧状態へ復元しました: {error}")
        } else {
            format!(
                "Codex接続を適用できず、旧状態の復元にも問題がありました: {error}; {}",
                restore_errors.join("; ")
            )
        });
    }
    Ok(config_path.to_string_lossy().into_owned())
}

pub(crate) fn select_codex_model(
    settings: &GatewaySettings,
    requested: Option<&str>,
    existing: Option<&str>,
) -> Option<String> {
    requested
        .filter(|model| is_codex_catalog_model(settings, model))
        .or_else(|| existing.filter(|model| is_codex_catalog_model(settings, model)))
        .map(str::to_string)
        .or_else(|| {
            settings
                .default_provider
                .as_deref()
                .and_then(|provider_id| {
                    settings
                        .providers
                        .iter()
                        .find(|provider| provider.id == provider_id && provider.enabled)
                })
                .and_then(|provider| {
                    provider
                        .default_model
                        .as_deref()
                        .filter(|model| provider.models.iter().any(|candidate| candidate == model))
                        .or_else(|| provider.models.first().map(String::as_str))
                        .map(|model| {
                            if provider.id == "openai" {
                                model.to_string()
                            } else {
                                format!("{}/{}", provider.id, model)
                            }
                        })
                })
                .filter(|model| is_codex_catalog_model(settings, model))
        })
        .or_else(|| {
            settings
                .providers
                .iter()
                .find(|provider| provider.id == "openai" && provider.enabled)
                .and_then(|provider| {
                    provider
                        .default_model
                        .as_deref()
                        .filter(|model| provider.models.iter().any(|candidate| candidate == model))
                        .or_else(|| provider.models.first().map(String::as_str))
                        .map(str::to_string)
                })
        })
}

pub(crate) fn is_codex_catalog_model(settings: &GatewaySettings, model: &str) -> bool {
    build_codex_catalog(settings)
        .models
        .iter()
        .any(|entry| entry.get("slug").and_then(JsonValue::as_str) == Some(model))
}

pub(crate) fn codex_public_model_id(model: &str) -> String {
    model
        .strip_prefix("openai/")
        .filter(|model| !model.is_empty())
        .unwrap_or(model)
        .to_string()
}

#[tauri::command]
pub async fn restore_codex_gateway_config(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<CodexRestoreReport, String> {
    let _mutation = manager.settings_mutation.lock().await;
    restore_codex_gateway_config_unlocked(app)
}

fn restore_codex_gateway_config_unlocked(app: AppHandle) -> Result<CodexRestoreReport, String> {
    let journal_path = codex_journal_path(&app)?;
    let journal = read_codex_journal(&journal_path)?.ok_or("CODETASのCodex復元情報がありません")?;
    let (config_path, catalog_path, backup_path, catalog_backup_path) =
        validate_codex_journal_paths(&app, &journal)?;
    let current = fs::read_to_string(&config_path)
        .map_err(|error| format!("現在のCodex設定を読めません: {error}"))?;
    let mut document = current
        .parse::<DocumentMut>()
        .map_err(|error| format!("現在のCodex設定を解析できません: {error}"))?;
    let backup = match backup_path.as_deref() {
        Some(path) => fs::read_to_string(path)
            .map_err(|error| format!("Codex設定バックアップを読めません: {error}"))?
            .parse::<DocumentMut>()
            .map_err(|error| format!("Codex設定バックアップを解析できません: {error}"))?,
        None => DocumentMut::new(),
    };
    let mut conflicts = Vec::new();

    let installed_provider = match journal.routing_mode {
        CodexRoutingMode::ProviderTable => GATEWAY_PROVIDER_ID,
        CodexRoutingMode::OpenAiBaseUrl => "openai",
    };
    restore_owned_string(
        &mut document,
        &backup,
        "model_provider",
        installed_provider,
        &mut conflicts,
    );
    restore_owned_string(
        &mut document,
        &backup,
        "model",
        &journal.installed_model,
        &mut conflicts,
    );
    restore_owned_string(
        &mut document,
        &backup,
        "model_catalog_json",
        &journal.catalog_path,
        &mut conflicts,
    );
    match journal.routing_mode {
        CodexRoutingMode::ProviderTable => {
            restore_owned_provider(&mut document, &backup, &journal, &mut conflicts)?;
        }
        CodexRoutingMode::OpenAiBaseUrl => restore_owned_string(
            &mut document,
            &backup,
            "openai_base_url",
            &journal.installed_base_url,
            &mut conflicts,
        ),
    }
    if let Some(installed_agents) = journal.installed_agents.as_ref() {
        restore_owned_agent_settings(&mut document, &backup, installed_agents, &mut conflicts)?;
    }
    let current_catalog = read_optional_file(&catalog_path)?;
    let original_catalog = match (journal.catalog_existed, catalog_backup_path.as_deref()) {
        (Some(true), Some(path)) => Some(
            read_optional_file(path)?.ok_or("Codexモデルカタログのバックアップが見つかりません")?,
        ),
        (Some(false), None) | (None, _) => None,
        _ => return Err("Codexモデルカタログの復元情報が矛盾しています".into()),
    };
    let mut settings = load_settings(&app)?;
    let expected_catalog = serde_json::to_value(build_codex_catalog(&settings))
        .map_err(|error| format!("CODETASモデルカタログを検証できません: {error}"))?;
    let current_catalog_json = current_catalog
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<JsonValue>(bytes).ok());
    let catalog_owned = current_catalog_json
        .as_ref()
        .is_some_and(|actual| actual == &expected_catalog);
    let catalog_already_restored = match journal.catalog_existed {
        Some(true) => current_catalog.as_deref() == original_catalog.as_deref(),
        Some(false) => current_catalog.is_none(),
        None => false,
    };
    if !catalog_owned && !catalog_already_restored {
        conflicts
            .push("model_catalog_json: CODETASの生成後にカタログ内容が変更されています".into());
    }
    let mut removed_catalog = false;
    if conflicts.is_empty() {
        let current_config = fs::read(&config_path)
            .map_err(|error| format!("現在のCodex設定を退避できません: {error}"))?;
        let preserve_legacy_catalog = journal.catalog_existed.is_none();
        // Disconnecting is persistent: stop CODETAS from re-applying the Codex
        // connection on the next startup or gateway start.
        let previous_settings = settings.clone();
        settings.codex.auto_connect = false;
        let restore_result = (|| {
            atomic_write(&config_path, document.to_string().as_bytes())?;
            if !preserve_legacy_catalog {
                restore_optional_file(&catalog_path, original_catalog.as_deref())?;
            }
            fs::remove_file(&journal_path)
                .map_err(|error| format!("CODETAS復元情報を削除できません: {error}"))?;
            write_settings_only(&app, &settings)
        })();
        if let Err(error) = restore_result {
            let mut rollback_errors = Vec::new();
            if let Err(rollback) = atomic_write(&config_path, &current_config) {
                rollback_errors.push(format!("Codex設定: {rollback}"));
            }
            if let Err(rollback) = restore_optional_file(&catalog_path, current_catalog.as_deref())
            {
                rollback_errors.push(format!("モデルカタログ: {rollback}"));
            }
            if let Err(rollback) = write_settings_only(&app, &previous_settings) {
                rollback_errors.push(format!("CODETAS設定: {rollback}"));
            }
            return Err(if rollback_errors.is_empty() {
                format!("Codex接続を復元できないため変更を取り消しました: {error}")
            } else {
                format!(
                    "Codex接続を復元できず、変更の取り消しにも失敗しました: {error}; {}",
                    rollback_errors.join("; ")
                )
            });
        }
        removed_catalog = journal.catalog_existed == Some(false) && current_catalog.is_some();
        for owned_backup in [backup_path.as_deref(), catalog_backup_path.as_deref()]
            .into_iter()
            .flatten()
        {
            let _ = fs::remove_file(owned_backup);
        }
    }
    Ok(CodexRestoreReport {
        restored: conflicts.is_empty(),
        config_path: config_path.to_string_lossy().into_owned(),
        conflicts,
        removed_catalog,
    })
}

#[tauri::command]
pub async fn uninstall_codetas_integration(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<CodetasUninstallReport, String> {
    let _mutation = manager.settings_mutation.lock().await;
    let journal_path = codex_journal_path(&app)?;
    let restore = if read_codex_journal(&journal_path)?.is_some() {
        Some(restore_codex_gateway_config_unlocked(app.clone())?)
    } else {
        None
    };
    if let Some(report) = restore.as_ref().filter(|report| !report.restored) {
        return Ok(CodetasUninstallReport {
            restored_codex: false,
            removed_settings: false,
            removed_catalog: report.removed_catalog,
            removed_observability: false,
            removed_service: false,
            stopped_gateway: false,
            conflicts: report.conflicts.clone(),
        });
    }

    let prior_handle = manager.handle.lock().await.take();
    let embedded_stopped = prior_handle.is_some();
    if let Some(handle) = prior_handle {
        handle.shutdown().await;
    }
    let service_status = service::status()?;
    let service_report = if service_status.installed {
        Some(service::uninstall()?)
    } else {
        None
    };
    let removed_service = service_report
        .as_ref()
        .is_some_and(|report| report.removed_definition);
    let stopped_gateway =
        embedded_stopped || service_report.as_ref().is_some_and(|report| report.stopped);
    let settings = settings_path(&app)?;
    let removed_settings = remove_owned_file(&settings)?;
    let _ = remove_owned_file(&settings.with_file_name("auth.json"));
    let _ = remove_owned_file(&settings.with_file_name("auth.json.lock"));
    // Without the ownership journal, a matching file name is not sufficient proof that
    // CODETAS created the catalog. Leave it untouched rather than deleting user state.
    let removed_catalog = restore
        .as_ref()
        .is_some_and(|report| report.removed_catalog);
    let data_directory = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("CODETASデータフォルダを特定できません: {error}"))?;
    let _ = clients::remove_all(&data_directory)?;
    let removed_observability = remove_owned_directory(&observability_path(&app)?)?;
    Ok(CodetasUninstallReport {
        restored_codex: restore.as_ref().map_or(true, |report| report.restored),
        removed_settings,
        removed_catalog,
        removed_observability,
        removed_service,
        stopped_gateway,
        conflicts: Vec::new(),
    })
}
