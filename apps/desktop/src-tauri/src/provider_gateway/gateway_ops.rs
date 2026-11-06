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
    if let Some(value) = patch.memory_budget_bytes {
        next.runtime.memory_budget_bytes = value;
    }
    if let Some(value) = patch.max_inflight_requests {
        next.runtime.max_inflight_requests = value;
    }
    if let Some(value) = patch.selected_models {
        next.catalog.selected_models = value;
    }
    if let Some(value) = patch.model_picker_order {
        next.catalog.model_picker_order = value;
    }
    for control in patch.account_controls.unwrap_or_default() {
        let account_index = next
            .account_pool
            .accounts
            .iter()
            .position(|item| {
                item.provider_id == control.provider_id && item.id == control.account_id
            })
            .ok_or_else(|| {
                format!(
                    "unknown account: {}/{}",
                    control.provider_id, control.account_id
                )
            })?;
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
        if let Some(value) = control.priority {
            account.priority = value;
        }
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
                next.account_pool
                    .active_accounts
                    .remove(&control.provider_id);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod management_patch_tests {
    use super::*;
    use codetas_gateway::AccountReference;
    use serde_json::json;
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

        apply_gateway_management_patch(
            &mut settings,
            GatewayManagementPatch {
                selected_models: Some(vec!["new/model".into()]),
                ..GatewayManagementPatch::default()
            },
        )
        .expect("focused patch");

        assert_eq!(
            settings.catalog.selected_models,
            vec!["new/model".to_string()]
        );
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
            apply_gateway_management_patch(
                &mut settings,
                GatewayManagementPatch {
                    account_controls: Some(vec![control]),
                    ..GatewayManagementPatch::default()
                },
            )
            .expect("account control");

            assert!(!settings.account_pool.accounts[0].pinned);
            assert!(!settings
                .account_pool
                .active_accounts
                .contains_key("provider"));
        }
    }

    #[test]
    fn pinning_atomically_unpins_provider_siblings() {
        let mut settings = account_settings();
        let mut control = account_patch("backup");
        control.pinned = Some(true);

        apply_gateway_management_patch(
            &mut settings,
            GatewayManagementPatch {
                account_controls: Some(vec![control]),
                ..GatewayManagementPatch::default()
            },
        )
        .expect("pin backup");

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

        apply_gateway_management_patch(
            &mut settings,
            GatewayManagementPatch {
                account_controls: Some(vec![control]),
                ..GatewayManagementPatch::default()
            },
        )
        .expect("resume account");

        assert!(!settings.account_pool.accounts[0].paused);
        assert_eq!(settings.account_pool.accounts[0].pause_until_unix, None);
    }

    #[test]
    fn pause_deadline_patch_preserves_missing_but_distinguishes_null_and_number() {
        for (raw, expected) in [
            (
                json!({"providerId": "provider", "accountId": "primary"}),
                Some(50),
            ),
            (
                json!({"providerId": "provider", "accountId": "primary", "pauseUntilUnix": null}),
                None,
            ),
            (
                json!({"providerId": "provider", "accountId": "primary", "pauseUntilUnix": 75}),
                Some(75),
            ),
        ] {
            let mut settings = account_settings();
            settings.account_pool.accounts[0].pause_until_unix = Some(50);
            let control =
                serde_json::from_value::<AccountControlPatch>(raw).expect("account control patch");

            apply_gateway_management_patch(
                &mut settings,
                GatewayManagementPatch {
                    account_controls: Some(vec![control]),
                    ..GatewayManagementPatch::default()
                },
            )
            .expect("apply deadline patch");

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
        }))
        .expect("account control patch");

        apply_gateway_management_patch(
            &mut settings,
            GatewayManagementPatch {
                account_controls: Some(vec![control]),
                ..GatewayManagementPatch::default()
            },
        )
        .expect("apply indefinite pause");

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

/// Why the last automatic connection to Codex failed. Startup has no window to show
/// this in, and stderr goes nowhere when the app is opened from Finder. It stays in
/// memory because the failure may come before any install journal exists, and every
/// start tries again anyway.
static CODEX_CONNECTION_ERROR: StdMutex<Option<String>> = StdMutex::new(None);

/// Tests that touch the process-wide connection error run one at a time.
#[cfg(test)]
pub(crate) static CONNECTION_ERROR_TEST_LOCK: StdMutex<()> = StdMutex::new(());

pub(crate) fn set_codex_connection_error(error: Option<String>) {
    *CODEX_CONNECTION_ERROR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = error;
}

pub(crate) fn codex_connection_error() -> Option<String> {
    CODEX_CONNECTION_ERROR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// What the connection panel shows: a failed connection, or else a failed fallback.
pub(crate) fn codex_connection_problem(app: &AppHandle) -> Option<String> {
    match codex_journal_path(app) {
        Ok(journal_path) => codex_connection_problem_at(&journal_path),
        Err(_) => codex_connection_error(),
    }
}

fn codex_connection_problem_at(journal_path: &Path) -> Option<String> {
    codex_connection_error().or_else(|| codex_journal_watchdog_error_at(journal_path))
}

/// For a caller with no window of its own, such as the tray or startup: the error
/// goes to the banner the window shows on every page.
pub(crate) fn report_codex_problem<T>(result: Result<T, String>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            eprintln!("CODETAS: {error}");
            set_codex_connection_error(Some(error));
            None
        }
    }
}

fn converge_codex_integration_unlocked(app: AppHandle) -> Result<String, String> {
    record_convergence_result(run_codex_convergence(app))
}

fn record_convergence_result(result: Result<String, String>) -> Result<String, String> {
    set_codex_connection_error(result.as_ref().err().cloned());
    result
}

fn run_codex_convergence(app: AppHandle) -> Result<String, String> {
    let (mut settings, migrated) = load_settings_with_migration_state(&app)?;
    let adopted = import_local_cli_sessions_for_start(&app, &mut settings)?;
    if startup_settings_need_persist(migrated, adopted) {
        settings.validate()?;
        save_settings(&app, &settings)?;
    }
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
    let settings = load_settings(&app)?;
    let observed = observe_gateway_runtime(&app, &manager, &settings).await?;
    if observed.running && !observed.locally_owned {
        if settings.codex.auto_connect {
            if let Err(error) = converge_codex_integration_unlocked(app.clone()) {
                eprintln!("CODETAS: Codex automatic connection was skipped: {error}");
            }
        }
        reconnect_codex_after_official_fallback(&app, &settings);
        return status(&app, true, false, settings);
    }
    let mut guard = manager.handle.lock().await;
    if settings.runtime.standalone_service {
        let prior_handle = guard.take();
        drop(guard);
        if let Some(handle) = prior_handle {
            handle.shutdown().await;
        }
        let service_status = service::status()?;
        let running = service_status.running || (service_status.installed && service::start()?);
        if running {
            if settings.codex.auto_connect {
                if let Err(error) = converge_codex_integration_unlocked(app.clone()) {
                    eprintln!("CODETAS: Codex automatic connection was skipped: {error}");
                }
            }
            reconnect_codex_after_official_fallback(&app, &settings);
            if settings.codex.fallback_to_official_when_unavailable {
                if let Err(error) = crate::service::ensure_codex_fallback_watchdog() {
                    eprintln!("CODETAS: official Codex fallback watchdog was not started: {error}");
                }
            }
        }
        return status(&app, running, running, settings);
    }
    if guard.is_none() {
        let handle = start_managed_gateway(&app, settings.clone()).await?;
        *guard = Some(handle);
    } else if let Some(handle) = guard.as_ref() {
        handle.set_settings(settings.clone()).await?;
    }
    drop(guard);
    if settings.codex.auto_connect {
        if let Err(error) = converge_codex_integration_unlocked(app.clone()) {
            eprintln!("CODETAS: Codex automatic connection was skipped: {error}");
        }
        apply_saved_settings(
            || load_settings(&app),
            |latest| apply_settings_to_embedded_gateway(&manager, latest),
        )
        .await;
    }
    reconnect_codex_after_official_fallback(&app, &settings);
    if settings.codex.fallback_to_official_when_unavailable {
        if let Err(error) = crate::service::ensure_codex_fallback_watchdog() {
            eprintln!("CODETAS: official Codex fallback watchdog was not started: {error}");
        }
    }
    status(&app, true, true, settings)
}

/// Convergence can adopt a local CLI login and save it. The embedded gateway does
/// not watch the file, so until it is told, the new connection shows as registered
/// while `provider/model` falls through to the default provider. The gateway is
/// already serving and the Codex config is written, so a failure here is shown in
/// the connection panel rather than failing the command.
async fn apply_saved_settings<Apply, Applied>(
    load_saved: impl FnOnce() -> Result<GatewaySettings, String>,
    apply: Apply,
) where
    Apply: FnOnce(GatewaySettings) -> Applied,
    Applied: std::future::Future<Output = Result<(), String>>,
{
    let applied = match load_saved() {
        Ok(latest) => apply(latest).await,
        Err(error) => Err(error),
    };
    if let Err(error) = applied {
        eprintln!("CODETAS: adopted connections were not applied to the gateway: {error}");
        set_codex_connection_error(Some(format!(
            "追加された接続をGatewayへ反映できません: {error}"
        )));
    }
}

/// Not `update_running_gateway`: that restarts a standalone service, which reloads
/// the file on its own and must not drop its requests for this.
async fn apply_settings_to_embedded_gateway(
    manager: &GatewayManager,
    settings: GatewaySettings,
) -> Result<(), String> {
    let guard = manager.handle.lock().await;
    match guard.as_ref() {
        Some(handle) => handle.set_settings(settings).await,
        None => Ok(()),
    }
}

fn reconnect_codex_after_official_fallback(app: &AppHandle, settings: &GatewaySettings) {
    let Ok(journal_path) = codex_journal_path(app) else {
        return;
    };
    let live_url = runtime_gateway_url(app).unwrap_or_else(|| gateway_url(settings));
    // With the gateway up, an earlier failed fallback no longer matters, so this
    // clears it unless the config could not follow the gateway.
    let failure = match sync_codex_with_live_gateway_at(&journal_path, &live_url, settings) {
        Ok(_) => None,
        Err(error) => {
            eprintln!("CODETAS: Codex reconnect after official fallback failed: {error}");
            Some(error)
        }
    };
    let _ = record_watchdog_error(&journal_path, failure.as_deref());
}

#[tauri::command]
pub async fn stop_provider_gateway(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<GatewayStatus, String> {
    let _mutation = manager.settings_mutation.lock().await;
    let settings = load_settings(&app)?;
    let observed = observe_gateway_runtime(&app, &manager, &settings).await?;
    if observed.running && !observed.locally_owned {
        return Err(
            "別のCODETASプロセスがGatewayを所有しているため、この画面からは停止できません".into(),
        );
    }
    if settings.runtime.standalone_service {
        if service::status()?.installed {
            if !service::stop()? {
                return Err("常駐Gatewayを停止できませんでした".into());
            }
            let running = service::status()?.running;
            if running {
                return Err("常駐Gatewayが停止状態になっていません".into());
            }
        }
    } else if let Some(handle) = manager.handle.lock().await.take() {
        handle.shutdown().await;
    }
    if settings.codex.fallback_to_official_when_unavailable {
        if let Err(error) = apply_temporary_official_codex_fallback(&app) {
            eprintln!("CODETAS: official Codex fallback after stop failed: {error}");
        }
    }
    status_from_runtime(&app, &manager, settings).await
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
    let settings = if let Some(handle) = guard.as_ref() {
        handle.settings().await
    } else {
        load_settings(&app)?
    };
    drop(guard);
    status_from_runtime(&app, &manager, settings).await
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
    status_from_runtime(&app, &manager, settings).await
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
    remove_provider_from_settings(&mut settings, &provider_id)?;
    clients::reconcile_claude_desktop_profile(&mut settings)?;
    settings.validate()?;
    persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
    status_from_runtime(&app, &manager, settings).await
}

fn remove_provider_from_settings(
    settings: &mut GatewaySettings,
    provider_id: &str,
) -> Result<(), String> {
    let before = settings.providers.len();
    settings
        .providers
        .retain(|provider| provider.id != provider_id);
    if settings.providers.len() == before {
        return Err("provider was not found".into());
    }
    if settings.default_provider.as_deref() == Some(provider_id) {
        settings.default_provider = settings
            .providers
            .iter()
            .find(|provider| provider.enabled)
            .map(|provider| provider.id.clone());
    }
    // With the CLI still signed in, startup adoption would recreate this provider.
    // DeepAI Code is imported by `apply_deepai_runtime` rather than adoption, but
    // it holds the same "removed stays removed" rule.
    let adoptable = provider_id == "deepai"
        || oauth_provider_registry().iter().any(|descriptor| {
            descriptor.cli_import
                && (descriptor.id == provider_id || descriptor.aliases.contains(&provider_id))
        });
    if adoptable {
        dismiss_cli_import(&mut settings.account_pool, provider_id);
    }
    Ok(())
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
    status_from_runtime(&app, &manager, settings).await
}

#[tauri::command]
pub async fn install_codex_gateway_config(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    input: CodexGatewayInstallInput,
) -> Result<String, String> {
    let _mutation = manager.settings_mutation.lock().await;
    install_then_apply_saved_settings(
        || install_codex_gateway_config_unlocked(app.clone(), input),
        || load_settings(&app),
        |latest| apply_settings_to_embedded_gateway(&manager, latest),
    )
    .await
}

/// An install saves what it adopted and enabled. The gateway a user just connected
/// to has to hear about it too, including when `auto_connect` was off and the start
/// before this skipped convergence.
async fn install_then_apply_saved_settings<Apply, Applied>(
    install: impl FnOnce() -> Result<String, String>,
    load_saved: impl FnOnce() -> Result<GatewaySettings, String>,
    apply: Apply,
) -> Result<String, String>
where
    Apply: FnOnce(GatewaySettings) -> Applied,
    Applied: std::future::Future<Output = Result<(), String>>,
{
    let path = install()?;
    apply_saved_settings(load_saved, apply).await;
    Ok(path)
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
    let paths = CodexInstallPaths {
        config: codex_config_path()?,
        catalog: codex_home()?.join("codetas-model-catalog.json"),
        journal: codex_journal_path(&app)?,
    };
    if let Some(journal) = read_codex_journal(&paths.journal)? {
        validate_codex_journal_paths(&app, &journal)?;
    }
    install_codex_gateway_files_at(
        &paths,
        &mut settings,
        input.model.as_deref(),
        &gateway_base_url,
        &|settings| save_settings(&app, settings),
        &|| write_settings_only(&app, &previous_settings),
    )?;
    set_codex_connection_error(None);
    Ok(paths.config.to_string_lossy().into_owned())
}

/// The Codex files one install touches. The command derives them from the
/// AppHandle, and tests point them at a temporary Codex home.
pub(crate) struct CodexInstallPaths {
    pub(crate) config: PathBuf,
    pub(crate) catalog: PathBuf,
    pub(crate) journal: PathBuf,
}

/// While the official fallback is active, `model` in the config is the official one
/// the fallback restored. The model the user was on is in the journal, and a start
/// runs this install before it would have reconnected from that journal.
fn model_to_install(
    settings: &GatewaySettings,
    requested: Option<&str>,
    document: &DocumentMut,
    journal: Option<&CodexInstallJournal>,
) -> Option<String> {
    let remembered = journal
        .filter(|journal| journal.official_fallback_active)
        .map(|journal| journal.installed_model.as_str())
        .filter(|model| is_codex_catalog_model(settings, model));
    let configured = document
        .get("model")
        .and_then(Item::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty());
    select_codex_model(settings, requested, remembered.or(configured))
}

pub(crate) fn install_codex_gateway_files_at(
    paths: &CodexInstallPaths,
    settings: &mut GatewaySettings,
    requested_model: Option<&str>,
    gateway_base_url: &str,
    persist_settings: &dyn Fn(&GatewaySettings) -> Result<(), String>,
    restore_previous_settings: &dyn Fn() -> Result<(), String>,
) -> Result<(), String> {
    let CodexInstallPaths {
        config: config_path,
        catalog: catalog_path,
        journal: journal_path,
    } = paths;
    let original_config = read_optional_file(config_path)?;
    let existing = match fs::read_to_string(config_path) {
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
    let existing_journal = read_codex_journal(journal_path)?;

    let requested_model = requested_model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(codex_public_model_id);
    let model = model_to_install(
        settings,
        requested_model.as_deref(),
        &document,
        existing_journal.as_ref(),
    )
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
        restore_agents_before_disable(journal_path, &mut document)?;
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
    let original_catalog = read_optional_file(catalog_path)?;
    let catalog_content = serialize_codex_catalog(settings, Some(&model))?;
    let claim = existing_journal
        .as_ref()
        .map_or(CodexGatewayTransportClaim::RespectExisting, |journal| {
            CodexGatewayTransportClaim::JournalOwned(&journal.installed_base_url)
        });
    configure_native_codex_gateway_with_claim(
        &mut document,
        gateway_base_url,
        &model,
        catalog_path,
        claim,
    )?;

    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Codex設定フォルダを作れません: {error}"))?;
    }
    let original_journal = read_optional_file(journal_path)?;
    let mut created_backups = Vec::new();
    let transaction = (|| {
        created_backups = prepare_codex_journal_at(
            journal_path,
            existing_journal,
            config_path,
            catalog_path,
            &model,
            gateway_base_url,
            false,
            CodexRoutingMode::OpenAiBaseUrl,
            installed_agents,
        )?;
        atomic_write(catalog_path, &catalog_content)?;
        atomic_write(config_path, document.to_string().as_bytes())?;
        persist_settings(settings)
    })();
    if let Err(error) = transaction {
        let mut restore_errors = Vec::new();
        for (label, path, snapshot) in [
            ("Codex設定", config_path, original_config.as_deref()),
            ("復元journal", journal_path, original_journal.as_deref()),
        ] {
            if let Err(restore) = restore_optional_file(path, snapshot) {
                restore_errors.push(format!("{label}: {restore}"));
            }
        }
        if let Err(restore) = restore_previous_settings() {
            restore_errors.push(format!("CODETAS設定: {restore}"));
        }
        if let Err(restore) = restore_optional_file(catalog_path, original_catalog.as_deref()) {
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
    Ok(())
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
    let Some(journal) = read_codex_journal(&journal_path)? else {
        let settings = load_settings(&app)?;
        let configured_url = gateway_url(&settings);
        let runtime_url = runtime_gateway_url(&app);
        let known_urls = std::iter::once(configured_url.as_str())
            .chain(runtime_url.as_deref())
            .collect::<Vec<_>>();
        return restore_without_journal(
            &codex_config_path()?,
            &codex_home()?.join("codetas-model-catalog.json"),
            &known_urls,
            &settings,
            &|settings| write_settings_only(&app, settings),
        );
    };
    validate_codex_journal_paths(&app, &journal)?;
    let settings = load_settings(&app)?;
    restore_codex_gateway_files_at(&journal_path, journal, &settings, &|settings| {
        write_settings_only(&app, settings)
    })
}

/// No journal and no gateway in the config means there is truly nothing to do. A
/// config still on the gateway has no backup to go back to, and stopping the gateway
/// would strand Codex, so what CODETAS wrote comes out. A config that does not parse
/// cannot be judged either way, and saying so is the only honest answer.
fn restore_without_journal(
    config_path: &Path,
    catalog_path: &Path,
    known_urls: &[&str],
    settings: &GatewaySettings,
    write_settings: &dyn Fn(&GatewaySettings) -> Result<(), String>,
) -> Result<CodexRestoreReport, String> {
    let original = read_optional_file(config_path)?.unwrap_or_default();
    let mut document = std::str::from_utf8(&original)
        .map_err(|_| "現在のCodex設定がUTF-8ではありません".to_string())?
        .parse::<DocumentMut>()
        .map_err(|error| format!("現在のCodex設定を解析できません: {error}"))?;
    let restored = remove_codetas_connection_without_journal(
        &mut document,
        catalog_path,
        known_urls,
        settings,
    )?;
    if restored {
        atomic_write(config_path, document.to_string().as_bytes())?;
        let mut disconnected = settings.clone();
        disconnected.codex.auto_connect = false;
        if let Err(error) = write_settings(&disconnected) {
            return Err(match atomic_write(config_path, &original) {
                Ok(()) => format!("Codex接続を復元できないため変更を取り消しました: {error}"),
                Err(rollback) => format!(
                    "Codex接続を復元できず、変更の取り消しにも失敗しました: {error}; {rollback}"
                ),
            });
        }
    }
    set_codex_connection_error(None);
    Ok(CodexRestoreReport {
        restored,
        config_path: config_path.to_string_lossy().into_owned(),
        conflicts: Vec::new(),
        removed_catalog: false,
    })
}

pub(crate) fn restore_codex_gateway_files_at(
    journal_path: &Path,
    journal: CodexInstallJournal,
    settings: &GatewaySettings,
    write_settings: &dyn Fn(&GatewaySettings) -> Result<(), String>,
) -> Result<CodexRestoreReport, String> {
    let (config_path, catalog_path, backup_path, catalog_backup_path) =
        validate_codex_journal_paths_from_journal_path(journal_path, &journal)?;
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

    apply_owned_codex_restore(
        &mut document,
        &backup,
        &journal,
        Some(settings),
        &mut conflicts,
    )?;
    let current_catalog = read_optional_file(&catalog_path)?;
    let original_catalog = match (journal.catalog_existed, catalog_backup_path.as_deref()) {
        (Some(true), Some(path)) => Some(
            read_optional_file(path)?.ok_or("Codexモデルカタログのバックアップが見つかりません")?,
        ),
        (Some(false), None) | (None, _) => None,
        _ => return Err("Codexモデルカタログの復元情報が矛盾しています".into()),
    };
    let expected_catalog = serde_json::to_value(build_codex_catalog(settings))
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
    let catalog_unrecognized = !catalog_owned && !catalog_already_restored;
    // The fallback leaves an unrecognized catalog in place on purpose and official
    // Codex does not load it, so neither does a disconnect touch it or stop over it.
    let catalog_kept_by_fallback = catalog_unrecognized && journal.official_fallback_active;
    if catalog_unrecognized && !catalog_kept_by_fallback {
        conflicts
            .push("model_catalog_json: CODETASの生成後にカタログ内容が変更されています".into());
    }
    let mut removed_catalog = false;
    if conflicts.is_empty() {
        let current_config = fs::read(&config_path)
            .map_err(|error| format!("現在のCodex設定を退避できません: {error}"))?;
        let preserve_catalog = journal.catalog_existed.is_none() || catalog_kept_by_fallback;
        // Disconnecting is persistent: stop CODETAS from re-applying the Codex
        // connection on the next startup or gateway start.
        let mut disconnected = settings.clone();
        disconnected.codex.auto_connect = false;
        let restore_result = (|| {
            atomic_write(&config_path, document.to_string().as_bytes())?;
            if !preserve_catalog {
                restore_optional_file(&catalog_path, original_catalog.as_deref())?;
            }
            // Removing the journal is last because nothing can undo it. Without it
            // the backups are orphaned and the next install records the connected
            // config as the one to restore.
            write_settings(&disconnected)?;
            fs::remove_file(journal_path)
                .map_err(|error| format!("CODETAS復元情報を削除できません: {error}"))
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
            if let Err(rollback) = write_settings(settings) {
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
        removed_catalog = journal.catalog_existed == Some(false)
            && current_catalog.is_some()
            && !preserve_catalog;
        for owned_backup in [backup_path.as_deref(), catalog_backup_path.as_deref()]
            .into_iter()
            .flatten()
        {
            let _ = fs::remove_file(owned_backup);
        }
        // Disconnecting on purpose makes an earlier failure to connect beside the point.
        set_codex_connection_error(None);
    } else {
        // The tray has no toast, and a window opened later should still explain
        // why Codex is still connected.
        set_codex_connection_error(Some(format!(
            "公式Codexへ戻せませんでした: {}",
            conflicts.join(" / ")
        )));
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
        let _ = service::uninstall_codex_fallback_watchdog();
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
        restored_codex: restore.as_ref().is_none_or(|report| report.restored),
        removed_settings,
        removed_catalog,
        removed_observability,
        removed_service,
        stopped_gateway,
        conflicts: Vec::new(),
    })
}

#[cfg(test)]
mod codex_lifecycle_tests {
    use super::super::tests::{
        fallback_settings, fallback_temp_root, read_config, write_fallback_fixture, BACKEND_URL,
        GATEWAY_URL,
    };
    use super::*;
    use std::cell::RefCell;
    use std::sync::MutexGuard;

    const MOVED_URL: &str = "http://127.0.0.1:50000/v1";
    const REMOTE_URL: &str = "https://example.invalid/v1";

    fn connection_error_guard() -> MutexGuard<'static, ()> {
        let guard = CONNECTION_ERROR_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        set_codex_connection_error(None);
        guard
    }

    /// A Codex home connected through the gateway on `GATEWAY_URL`.
    struct Fixture {
        root: PathBuf,
        settings: GatewaySettings,
        installed: String,
        picked: String,
        config_path: PathBuf,
        catalog_path: PathBuf,
        journal_path: PathBuf,
    }

    impl Fixture {
        /// `config_model` is what Codex last wrote. The journal remembers `installed`.
        fn new(name: &str, config_model: impl FnOnce(&str, &str) -> String) -> Self {
            let root = fallback_temp_root(name);
            let (settings, installed, picked) = fallback_settings();
            let model = config_model(&installed, &picked);
            let (config_path, catalog_path, journal_path) =
                write_fallback_fixture(&root, &settings, &installed, &model);
            Self {
                root,
                settings,
                installed,
                picked,
                config_path,
                catalog_path,
                journal_path,
            }
        }

        fn picked_in_codex(name: &str) -> Self {
            Self::new(name, |_, picked| picked.to_string())
        }

        fn unchanged(name: &str) -> Self {
            Self::new(name, |installed, _| installed.to_string())
        }

        fn journal(&self) -> CodexInstallJournal {
            read_codex_journal(&self.journal_path).unwrap().unwrap()
        }

        fn set_in_config(&self, key: &str, url: &str) {
            let mut document = read_config(&self.config_path);
            document[key] = value(url);
            fs::write(&self.config_path, document.to_string()).unwrap();
        }

        fn remove_from_config(&self, key: &str) {
            let mut document = read_config(&self.config_path);
            document.remove(key);
            fs::write(&self.config_path, document.to_string()).unwrap();
        }

        fn restore_with_no_journal(
            &self,
            known_urls: &[&str],
            write_settings: &dyn Fn(&GatewaySettings) -> Result<(), String>,
        ) -> Result<CodexRestoreReport, String> {
            fs::remove_file(&self.journal_path).unwrap();
            restore_without_journal(
                &self.config_path,
                &self.catalog_path,
                known_urls,
                &self.settings,
                write_settings,
            )
        }

        /// What the config held before CODETAS was installed.
        fn set_official_config(&self, content: &str) {
            let backup = self
                .journal_path
                .with_file_name("codex-config-before-test.toml");
            fs::write(&backup, content).unwrap();
            let mut journal = self.journal();
            journal.backup_path = Some(backup.to_string_lossy().into_owned());
            fs::write(
                &self.journal_path,
                serde_json::to_vec_pretty(&journal).unwrap(),
            )
            .unwrap();
        }

        fn fall_back(&self) {
            assert!(apply_temporary_official_codex_fallback_at(
                &self.journal_path,
                Some(&self.settings)
            )
            .unwrap());
        }

        fn install(&self, url: &str) -> Result<(), String> {
            self.install_saving_with(url, &|_| Ok(()))
        }

        fn install_saving_with(
            &self,
            url: &str,
            persist: &dyn Fn(&GatewaySettings) -> Result<(), String>,
        ) -> Result<(), String> {
            let paths = CodexInstallPaths {
                config: self.config_path.clone(),
                catalog: self.catalog_path.clone(),
                journal: self.journal_path.clone(),
            };
            let mut settings = self.settings.clone();
            install_codex_gateway_files_at(&paths, &mut settings, None, url, persist, &|| Ok(()))
        }

        fn restore(
            &self,
            write_settings: &dyn Fn(&GatewaySettings) -> Result<(), String>,
        ) -> Result<CodexRestoreReport, String> {
            restore_codex_gateway_files_at(
                &self.journal_path,
                self.journal(),
                &self.settings,
                write_settings,
            )
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    // #1: every quit applies the fallback, so every relaunch comes through here.
    #[test]
    fn a_relaunch_keeps_the_model_picked_before_quitting() {
        let fixture = Fixture::picked_in_codex("relaunch");
        fixture.set_official_config(&format!("model = \"{}\"\n", fixture.installed));

        // Quit: the exit handler puts the official model back in the config.
        fixture.fall_back();
        assert_eq!(
            read_config(&fixture.config_path)["model"].as_str(),
            Some(fixture.installed.as_str())
        );

        // Relaunch, in start_provider_gateway's order: the gateway binds, convergence
        // installs, then the reconnect has nothing left to do.
        reconcile_started_gateway_at(&fixture.journal_path, GATEWAY_URL);
        fixture.install(GATEWAY_URL).unwrap();
        assert!(!sync_codex_with_live_gateway_at(
            &fixture.journal_path,
            GATEWAY_URL,
            &fixture.settings
        )
        .unwrap());

        let config = read_config(&fixture.config_path);
        assert_eq!(config["model"].as_str(), Some(fixture.picked.as_str()));
        assert_eq!(config["openai_base_url"].as_str(), Some(GATEWAY_URL));
        assert!(config.get("chatgpt_base_url").is_none());
        let journal = fixture.journal();
        assert_eq!(journal.installed_model, fixture.picked);
        assert!(!journal.official_fallback_active);
    }

    #[test]
    fn a_relaunch_keeps_the_model_when_the_watchdog_reconnects_first() {
        let fixture = Fixture::picked_in_codex("relaunch-watchdog");
        fixture.set_official_config(&format!("model = \"{}\"\n", fixture.installed));
        fixture.fall_back();

        assert!(sync_codex_with_live_gateway_at(
            &fixture.journal_path,
            GATEWAY_URL,
            &fixture.settings
        )
        .unwrap());
        fixture.install(GATEWAY_URL).unwrap();

        assert_eq!(
            read_config(&fixture.config_path)["model"].as_str(),
            Some(fixture.picked.as_str())
        );
    }

    #[test]
    fn the_journal_model_only_outranks_the_config_during_a_fallback() {
        let fixture = Fixture::picked_in_codex("model-rule");
        let (settings, installed, picked) = (
            &fixture.settings,
            fixture.installed.as_str(),
            fixture.picked.as_str(),
        );
        let config = read_config(&fixture.config_path);
        let mut journal = fixture.journal();

        // Connected: `model` in the config is what Codex last picked.
        assert_eq!(
            model_to_install(settings, None, &config, Some(&journal)).as_deref(),
            Some(picked)
        );

        // In the fallback: `model` in the config is the official one.
        let official = format!("model = \"{installed}\"\n")
            .parse::<DocumentMut>()
            .unwrap();
        journal.official_fallback_active = true;
        journal.installed_model = picked.to_string();
        assert_eq!(
            model_to_install(settings, None, &official, Some(&journal)).as_deref(),
            Some(picked)
        );

        // An explicit request still wins.
        assert_eq!(
            model_to_install(settings, Some(installed), &official, Some(&journal)).as_deref(),
            Some(installed)
        );

        // A remembered model that left the catalog cannot be installed.
        journal.installed_model = "gone/model".into();
        assert_eq!(
            model_to_install(settings, None, &official, Some(&journal)).as_deref(),
            Some(installed)
        );
    }

    #[test]
    fn a_failed_install_puts_everything_back() {
        let fixture = Fixture::unchanged("install-rollback");
        let config = fs::read(&fixture.config_path).unwrap();
        let catalog = fs::read(&fixture.catalog_path).unwrap();
        let journal = fs::read(&fixture.journal_path).unwrap();

        let error = fixture
            .install_saving_with(MOVED_URL, &|_| Err("disk full".into()))
            .unwrap_err();

        assert!(error.contains("disk full"), "{error}");
        assert_eq!(fs::read(&fixture.config_path).unwrap(), config);
        assert_eq!(fs::read(&fixture.catalog_path).unwrap(), catalog);
        assert_eq!(fs::read(&fixture.journal_path).unwrap(), journal);
    }

    // #2
    #[test]
    fn a_manual_restore_follows_the_model_picked_in_codex() {
        let _guard = connection_error_guard();
        set_codex_connection_error(Some("an earlier failure".into()));
        let fixture = Fixture::picked_in_codex("restore-picked");
        let written = RefCell::new(Vec::new());

        let report = fixture
            .restore(&|settings| {
                written.borrow_mut().push(settings.codex.auto_connect);
                Ok(())
            })
            .unwrap();

        assert!(report.restored);
        assert!(report.conflicts.is_empty());
        assert!(report.allows_gateway_stop());
        let config = read_config(&fixture.config_path);
        for key in [
            "model",
            "model_provider",
            "model_catalog_json",
            "openai_base_url",
            "chatgpt_base_url",
        ] {
            assert!(config.get(key).is_none(), "{key}");
        }
        assert!(!fixture.journal_path.exists());
        assert!(!fixture.catalog_path.exists());
        assert_eq!(*written.borrow(), vec![false]);
        assert_eq!(codex_connection_error(), None);
    }

    #[test]
    fn a_manual_restore_with_a_conflict_keeps_the_connection_and_says_why() {
        let _guard = connection_error_guard();
        let fixture = Fixture::unchanged("restore-conflict");
        fixture.set_in_config("openai_base_url", REMOTE_URL);
        let config = fs::read(&fixture.config_path).unwrap();

        let report = fixture
            .restore(&|_| panic!("a conflict must not change the settings"))
            .unwrap();

        assert!(!report.restored);
        assert!(!report.allows_gateway_stop());
        assert!(
            report
                .conflicts
                .iter()
                .any(|conflict| conflict.contains("openai_base_url was changed")),
            "{:?}",
            report.conflicts
        );
        assert_eq!(fs::read(&fixture.config_path).unwrap(), config);
        assert!(fixture.journal_path.exists());
        assert!(fixture.catalog_path.exists());
        let shown = codex_connection_problem_at(&fixture.journal_path).expect("shown in the panel");
        assert!(shown.contains("openai_base_url was changed"), "{shown}");
        set_codex_connection_error(None);
    }

    #[test]
    fn a_manual_restore_is_not_blocked_by_a_gateway_model_that_left_the_catalog() {
        let _guard = connection_error_guard();
        let fixture = Fixture::new("restore-gone-model", |_, _| "gone/model".into());
        fixture.set_official_config("model = \"official-before\"\n");

        let report = fixture.restore(&|_| Ok(())).unwrap();

        assert!(report.restored, "{:?}", report.conflicts);
        assert_eq!(
            read_config(&fixture.config_path)["model"].as_str(),
            Some("official-before")
        );
    }

    #[test]
    fn a_manual_restore_leaves_an_official_model_the_user_chose() {
        let _guard = connection_error_guard();
        let fixture = Fixture::new("restore-own-model", |_, _| "my-own-model".into());
        fixture.set_official_config("model = \"official-before\"\n");

        let report = fixture.restore(&|_| Ok(())).unwrap();

        assert!(report.restored, "{:?}", report.conflicts);
        assert_eq!(
            read_config(&fixture.config_path)["model"].as_str(),
            Some("my-own-model")
        );
    }

    #[test]
    fn only_a_provider_model_or_a_route_counts_as_needing_the_gateway() {
        let settings = GatewaySettings {
            routes: vec![
                codetas_gateway::RouteDefinition {
                    id: "route-id".into(),
                    alias: Some("fast".into()),
                    ..codetas_gateway::RouteDefinition::default()
                },
                codetas_gateway::RouteDefinition {
                    id: "unaliased".into(),
                    ..codetas_gateway::RouteDefinition::default()
                },
            ],
            ..GatewaySettings::default()
        };

        assert!(model_needs_gateway(None, "zai/glm-5"));
        assert!(model_needs_gateway(Some(&settings), "fast"));
        assert!(model_needs_gateway(Some(&settings), "unaliased"));
        assert!(!model_needs_gateway(Some(&settings), "route-id"));
        assert!(!model_needs_gateway(Some(&settings), "gpt-5.6-sol"));
        assert!(!model_needs_gateway(None, "fast"));
    }

    #[test]
    fn a_fallback_puts_back_the_official_model_when_a_gateway_model_left_the_catalog() {
        let fixture = Fixture::new("fallback-gone-model", |_, _| "gone/model".into());
        fixture.set_official_config("model = \"official-before\"\n");

        fixture.fall_back();

        assert_eq!(
            read_config(&fixture.config_path)["model"].as_str(),
            Some("official-before")
        );
        assert_eq!(fixture.journal().installed_model, fixture.installed);
    }

    #[test]
    fn a_fallback_leaves_an_official_pick_and_a_reconnect_returns_to_it() {
        let root_settings = fallback_settings().0;
        let official_pick = build_codex_catalog(&root_settings)
            .models
            .iter()
            .filter_map(|model| model.get("slug").and_then(JsonValue::as_str))
            .filter(|slug| !slug.contains('/'))
            .nth(1)
            .unwrap()
            .to_string();
        let pick = official_pick.clone();
        let fixture = Fixture::new("fallback-official-pick", move |_, _| pick);
        fixture.set_official_config(&format!("model = \"{}\"\n", fixture.installed));

        fixture.fall_back();

        assert_eq!(
            read_config(&fixture.config_path)["model"].as_str(),
            Some(official_pick.as_str())
        );
        assert_eq!(fixture.journal().installed_model, official_pick);
        assert!(sync_codex_with_live_gateway_at(
            &fixture.journal_path,
            GATEWAY_URL,
            &fixture.settings
        )
        .unwrap());
        assert_eq!(
            read_config(&fixture.config_path)["model"].as_str(),
            Some(official_pick.as_str())
        );
    }

    #[test]
    fn a_manual_restore_during_the_fallback_leaves_an_official_pick_alone() {
        let _guard = connection_error_guard();
        let fixture = Fixture::unchanged("restore-official-pick");
        fixture.fall_back();
        fs::write(&fixture.config_path, "model = \"official-pick\"\n").unwrap();

        let report = fixture.restore(&|_| Ok(())).unwrap();

        assert!(report.restored, "{:?}", report.conflicts);
        assert_eq!(
            read_config(&fixture.config_path)["model"].as_str(),
            Some("official-pick")
        );
        assert!(!fixture.journal_path.exists());
    }

    #[test]
    fn restoring_without_a_journal_is_nothing_to_do_unless_codex_is_on_the_gateway() {
        let _guard = connection_error_guard();
        let fixture = Fixture::unchanged("no-journal-nothing");
        fs::write(&fixture.config_path, "model = \"gpt-5.5\"\n").unwrap();

        let report = fixture
            .restore_with_no_journal(&[GATEWAY_URL], &|_| panic!("nothing to save"))
            .unwrap();

        assert!(!report.restored);
        assert!(report.conflicts.is_empty());
        assert!(report.allows_gateway_stop());
        assert_eq!(
            fs::read_to_string(&fixture.config_path).unwrap(),
            "model = \"gpt-5.5\"\n"
        );
    }

    #[test]
    fn restoring_with_no_config_is_nothing_to_do() {
        let _guard = connection_error_guard();
        let fixture = Fixture::unchanged("no-journal-no-config");
        fs::remove_file(&fixture.config_path).unwrap();

        let report = fixture
            .restore_with_no_journal(&[GATEWAY_URL], &|_| panic!("nothing to save"))
            .unwrap();

        assert!(!report.restored);
        assert!(!fixture.config_path.exists());
    }

    #[test]
    fn a_disconnect_with_no_journal_removes_what_codetas_wrote() {
        let _guard = connection_error_guard();
        let fixture = Fixture::picked_in_codex("no-journal-connected");
        let mut config = read_config(&fixture.config_path);
        config["approval_policy"] = value("never");
        fs::write(&fixture.config_path, config.to_string()).unwrap();
        let written = RefCell::new(Vec::new());

        let report = fixture
            .restore_with_no_journal(&[GATEWAY_URL], &|settings| {
                written.borrow_mut().push(settings.codex.auto_connect);
                Ok(())
            })
            .unwrap();

        assert!(report.restored);
        assert!(report.allows_gateway_stop());
        let config = read_config(&fixture.config_path);
        for key in [
            "openai_base_url",
            "chatgpt_base_url",
            "model_catalog_json",
            "model",
        ] {
            assert!(config.get(key).is_none(), "{key}");
        }
        assert_eq!(config["approval_policy"].as_str(), Some("never"));
        assert_eq!(*written.borrow(), vec![false]);
    }

    #[test]
    fn a_disconnect_with_no_journal_recognises_an_old_port_and_keeps_an_official_model() {
        let _guard = connection_error_guard();
        let fixture = Fixture::new("no-journal-old-port", |_, _| "gpt-5.5".into());
        fixture.remove_from_config("chatgpt_base_url");

        // Not a URL CODETAS knows now, but the config names CODETAS's catalog.
        let report = fixture
            .restore_with_no_journal(&[MOVED_URL], &|_| Ok(()))
            .unwrap();

        assert!(report.restored);
        let config = read_config(&fixture.config_path);
        assert!(config.get("openai_base_url").is_none());
        assert_eq!(config["model"].as_str(), Some("gpt-5.5"));
    }

    #[test]
    fn a_disconnect_with_no_journal_leaves_a_local_proxy_alone() {
        let _guard = connection_error_guard();
        let fixture = Fixture::unchanged("no-journal-proxy");
        let content = "openai_base_url = \"http://localhost:11434/v1\"\n";
        fs::write(&fixture.config_path, content).unwrap();

        let report = fixture
            .restore_with_no_journal(&[GATEWAY_URL], &|_| panic!("nothing to save"))
            .unwrap();

        assert!(!report.restored);
        assert_eq!(fs::read_to_string(&fixture.config_path).unwrap(), content);
    }

    #[test]
    fn a_config_that_does_not_parse_is_an_error_with_no_journal() {
        let _guard = connection_error_guard();
        let fixture = Fixture::unchanged("no-journal-broken");
        fs::write(&fixture.config_path, "not [ toml").unwrap();

        let result =
            fixture.restore_with_no_journal(&[GATEWAY_URL], &|_| panic!("nothing to save"));

        let error = result.err().expect("cannot tell what the config holds");
        assert!(error.contains("解析"), "{error}");
        assert_eq!(
            fs::read_to_string(&fixture.config_path).unwrap(),
            "not [ toml"
        );
    }

    #[test]
    fn a_disconnect_with_no_journal_puts_the_config_back_if_the_settings_cannot_be_saved() {
        let _guard = connection_error_guard();
        let fixture = Fixture::unchanged("no-journal-rollback");
        let config = fs::read(&fixture.config_path).unwrap();

        let result = fixture.restore_with_no_journal(&[GATEWAY_URL], &|_| Err("disk full".into()));

        let error = result.err().expect("the settings write fails");
        assert!(error.contains("disk full"), "{error}");
        assert_eq!(fs::read(&fixture.config_path).unwrap(), config);
    }

    #[test]
    fn a_disconnect_with_no_journal_removes_a_legacy_provider_table_on_any_port() {
        let _guard = connection_error_guard();
        let fixture = Fixture::unchanged("no-journal-legacy");
        fs::write(
            &fixture.config_path,
            "model_provider = \"codetas_gateway\"\nmodel = \"zai/glm-5\"\n\
             [model_providers.codetas_gateway]\nname = \"CODETAS Gateway\"\nbase_url = \"http://127.0.0.1:50000/v1\"\nwire_api = \"responses\"\nsupports_websockets = true\n",
        )
        .unwrap();

        let report = fixture
            .restore_with_no_journal(&[GATEWAY_URL], &|_| Ok(()))
            .unwrap();

        assert!(report.restored);
        let config = read_config(&fixture.config_path);
        assert!(config.get("model_provider").is_none());
        assert!(config.get("model").is_none());
        assert!(!config.to_string().contains("codetas_gateway"));
    }

    // #2
    #[test]
    fn a_connected_config_with_no_journal_is_not_recorded_as_official() {
        let _guard = connection_error_guard();
        let fixture = Fixture::picked_in_codex("no-journal-install");
        fs::remove_file(&fixture.journal_path).unwrap();

        fixture.install(GATEWAY_URL).unwrap();

        let backup = fixture.journal().backup_path.expect("config backup");
        let backup = read_config(Path::new(&backup));
        for key in [
            "openai_base_url",
            "chatgpt_base_url",
            "model_catalog_json",
            "model",
        ] {
            assert!(backup.get(key).is_none(), "{key}");
        }

        let report = fixture.restore(&|_| Ok(())).unwrap();

        assert!(report.restored, "{:?}", report.conflicts);
        let config = read_config(&fixture.config_path);
        for key in [
            "openai_base_url",
            "chatgpt_base_url",
            "model_catalog_json",
            "model",
        ] {
            assert!(config.get(key).is_none(), "{key}");
        }
    }

    #[test]
    fn a_backup_recording_a_connected_config_does_not_put_codex_back_on_the_gateway() {
        let _guard = connection_error_guard();
        let fixture = Fixture::unchanged("connected-backup");
        fixture.set_official_config(&format!(
            "model = \"routed/picked-model\"\nmodel_catalog_json = \"{}\"\nopenai_base_url = \"{GATEWAY_URL}\"\nchatgpt_base_url = \"{BACKEND_URL}\"\n",
            fixture.catalog_path.display()
        ));

        let report = fixture.restore(&|_| Ok(())).unwrap();

        assert!(report.restored, "{:?}", report.conflicts);
        let config = read_config(&fixture.config_path);
        for key in [
            "openai_base_url",
            "chatgpt_base_url",
            "model_catalog_json",
            "model",
        ] {
            assert!(config.get(key).is_none(), "{key}");
        }
    }

    // #6
    #[test]
    fn a_disconnect_during_the_fallback_is_not_blocked_by_the_catalog_it_kept() {
        let _guard = connection_error_guard();
        let fixture = Fixture::unchanged("fallback-kept-catalog");
        fs::write(&fixture.catalog_path, "{\"models\":[]}").unwrap();
        fixture.fall_back();

        let report = fixture.restore(&|_| Ok(())).unwrap();

        assert!(report.restored, "{:?}", report.conflicts);
        assert!(!report.removed_catalog);
        assert!(!fixture.journal_path.exists());
        assert_eq!(
            fs::read_to_string(&fixture.catalog_path).unwrap(),
            "{\"models\":[]}"
        );
    }

    #[test]
    fn a_changed_catalog_still_blocks_a_disconnect_outside_the_fallback() {
        let _guard = connection_error_guard();
        let fixture = Fixture::unchanged("changed-catalog");
        fs::write(&fixture.catalog_path, "{\"models\":[]}").unwrap();

        let report = fixture
            .restore(&|_| panic!("a conflict must not change the settings"))
            .unwrap();

        assert!(!report.restored);
        assert!(report.conflicts[0].contains("model_catalog_json"));
        set_codex_connection_error(None);
    }

    // #7
    #[test]
    fn a_legacy_backup_with_codetas_url_does_not_conflict_before_a_new_install() {
        let _guard = connection_error_guard();
        // What HEAD left: it took the URL out of the config, but the backup has it.
        let fixture = Fixture::unchanged("legacy-backup");
        fixture.remove_from_config("chatgpt_base_url");
        fixture.set_official_config("chatgpt_base_url = \"http://127.0.0.1:42421/v1\"\n");

        fixture.fall_back();
        let config = read_config(&fixture.config_path);
        assert!(config.get("chatgpt_base_url").is_none());
        assert!(config.get("openai_base_url").is_none());

        let report = fixture.restore(&|_| Ok(())).unwrap();
        assert!(report.restored, "{:?}", report.conflicts);
        assert!(read_config(&fixture.config_path)
            .get("chatgpt_base_url")
            .is_none());
    }

    #[test]
    fn a_config_holding_codetas_url_from_its_backup_is_not_an_edit() {
        let _guard = connection_error_guard();
        let fixture = Fixture::unchanged("legacy-backup-held");
        fixture.fall_back();
        // An older build's fallback put the backup's own URL back in the config.
        let own = "http://127.0.0.1:42421/v1";
        fixture.set_official_config(&format!("openai_base_url = \"{own}\"\n"));
        fs::write(
            &fixture.config_path,
            format!("openai_base_url = \"{own}\"\n"),
        )
        .unwrap();
        reconcile_owned_codex_runtime_url_path(&fixture.journal_path, MOVED_URL).unwrap();

        let report = fixture.restore(&|_| Ok(())).unwrap();

        assert!(report.restored, "{:?}", report.conflicts);
        assert!(read_config(&fixture.config_path)
            .get("openai_base_url")
            .is_none());
    }

    // #5
    #[test]
    fn with_auto_connect_off_nothing_reconnects_until_the_user_asks() {
        let mut fixture = Fixture::picked_in_codex("auto-connect-off");
        fixture.settings.codex.auto_connect = false;
        fixture.set_official_config(&format!("model = \"{}\"\n", fixture.installed));

        // Quit: a connected thread still gets the fallback.
        fixture.fall_back();
        let official = fs::read(&fixture.config_path).unwrap();

        // Relaunch and the watchdog: the journal follows the gateway, Codex stays.
        assert!(!sync_codex_with_live_gateway_at(
            &fixture.journal_path,
            MOVED_URL,
            &fixture.settings
        )
        .unwrap());
        assert_eq!(fs::read(&fixture.config_path).unwrap(), official);
        let journal = fixture.journal();
        assert!(journal.official_fallback_active);
        assert_eq!(journal.installed_base_url, MOVED_URL);

        // The user asks: connecting works and returns to the model they were on.
        fixture.install(MOVED_URL).unwrap();
        let config = read_config(&fixture.config_path);
        assert_eq!(config["openai_base_url"].as_str(), Some(MOVED_URL));
        assert_eq!(config["model"].as_str(), Some(fixture.picked.as_str()));
        assert!(!fixture.journal().official_fallback_active);
    }

    #[test]
    fn with_auto_connect_off_a_connected_config_still_follows_the_gateway() {
        let mut fixture = Fixture::unchanged("auto-connect-off-follow");
        fixture.settings.codex.auto_connect = false;

        assert!(!sync_codex_with_live_gateway_at(
            &fixture.journal_path,
            MOVED_URL,
            &fixture.settings
        )
        .unwrap());

        assert_eq!(
            read_config(&fixture.config_path)["openai_base_url"].as_str(),
            Some(MOVED_URL)
        );
    }

    // #3
    #[test]
    fn a_failure_with_no_window_reaches_the_banner() {
        let _guard = connection_error_guard();

        assert_eq!(report_codex_problem(Ok::<_, String>(7)), Some(7));
        assert_eq!(codex_connection_error(), None);

        assert_eq!(
            report_codex_problem(Err::<u8, _>("cannot bind".to_string())),
            None
        );
        assert_eq!(codex_connection_error().as_deref(), Some("cannot bind"));
        set_codex_connection_error(None);
    }

    // #5, #11
    #[test]
    fn a_failed_restore_leaves_the_journal_and_can_be_retried() {
        let _guard = connection_error_guard();
        let fixture = Fixture::unchanged("restore-rollback");
        set_codex_connection_error(Some("an earlier failure".into()));
        let config = fs::read(&fixture.config_path).unwrap();
        let catalog = fs::read(&fixture.catalog_path).unwrap();
        let journal = fs::read(&fixture.journal_path).unwrap();

        let error = fixture
            .restore(&|settings| {
                if settings.codex.auto_connect {
                    Ok(())
                } else {
                    Err("disk full".into())
                }
            })
            .err()
            .expect("the settings write fails");

        assert!(error.contains("disk full"), "{error}");
        assert_eq!(fs::read(&fixture.config_path).unwrap(), config);
        assert_eq!(fs::read(&fixture.catalog_path).unwrap(), catalog);
        assert_eq!(fs::read(&fixture.journal_path).unwrap(), journal);
        // The failure to connect is still true, so it is still shown.
        assert_eq!(
            codex_connection_error().as_deref(),
            Some("an earlier failure")
        );

        assert!(fixture.restore(&|_| Ok(())).unwrap().restored);
        assert!(!fixture.journal_path.exists());
        assert_eq!(codex_connection_error(), None);
    }

    // #3
    #[test]
    fn with_the_fallback_active_reconcile_moves_only_the_journal_url() {
        let fixture = Fixture::unchanged("reconcile-fallback");
        fixture.fall_back();
        let official = fs::read(&fixture.config_path).unwrap();

        reconcile_owned_codex_runtime_url_path(&fixture.journal_path, MOVED_URL).unwrap();

        assert_eq!(fs::read(&fixture.config_path).unwrap(), official);
        let journal = fixture.journal();
        assert_eq!(journal.installed_base_url, MOVED_URL);
        assert!(journal.official_fallback_active);

        // The reconnect then writes the port the gateway actually bound.
        assert!(sync_codex_with_live_gateway_at(
            &fixture.journal_path,
            MOVED_URL,
            &fixture.settings
        )
        .unwrap());
        let config = read_config(&fixture.config_path);
        assert_eq!(config["openai_base_url"].as_str(), Some(MOVED_URL));
        assert!(config.get("chatgpt_base_url").is_none());
    }

    #[test]
    fn a_started_gateway_shows_a_config_it_cannot_follow_instead_of_failing() {
        let _guard = connection_error_guard();
        let fixture = Fixture::unchanged("reconcile-started");
        fixture.set_in_config("openai_base_url", REMOTE_URL);

        // Returning nothing is the point: start_managed_gateway cannot drop the
        // handle over this.
        reconcile_started_gateway_at(&fixture.journal_path, MOVED_URL);

        let shown = codex_connection_problem_at(&fixture.journal_path).expect("shown in the panel");
        assert!(shown.contains("動的ポート"), "{shown}");
        assert_eq!(
            read_config(&fixture.config_path)["openai_base_url"].as_str(),
            Some(REMOTE_URL)
        );
        set_codex_connection_error(None);
    }

    #[test]
    fn a_service_start_records_a_config_it_cannot_follow_in_the_journal() {
        let fixture = Fixture::unchanged("reconcile-service");
        fixture.set_in_config("openai_base_url", REMOTE_URL);

        reconcile_service_runtime_url_at(&fixture.journal_path, MOVED_URL);

        let recorded = fixture.journal().last_watchdog_error.expect("recorded");
        assert!(recorded.contains("動的ポート"), "{recorded}");
    }

    #[test]
    fn a_service_start_during_the_fallback_moves_the_journal_url() {
        let fixture = Fixture::unchanged("reconcile-service-fallback");
        fixture.fall_back();

        reconcile_service_runtime_url_at(&fixture.journal_path, MOVED_URL);

        let journal = fixture.journal();
        assert_eq!(journal.installed_base_url, MOVED_URL);
        assert_eq!(journal.last_watchdog_error, None);
    }

    #[test]
    fn a_live_gateway_keeps_failing_until_the_config_can_follow_it() {
        let fixture = Fixture::unchanged("live-sync");
        fixture.set_in_config("openai_base_url", REMOTE_URL);

        // The watchdog clears its error only on success, so this keeps it shown.
        assert!(sync_codex_with_live_gateway_at(
            &fixture.journal_path,
            MOVED_URL,
            &fixture.settings
        )
        .is_err());

        fixture.set_in_config("openai_base_url", GATEWAY_URL);
        assert!(!sync_codex_with_live_gateway_at(
            &fixture.journal_path,
            MOVED_URL,
            &fixture.settings
        )
        .unwrap());
        assert_eq!(
            read_config(&fixture.config_path)["openai_base_url"].as_str(),
            Some(MOVED_URL)
        );
    }

    // #4
    #[test]
    fn reconcile_moves_the_url_and_removes_what_an_older_build_wrote() {
        let fixture = Fixture::unchanged("reconcile-port");
        assert_eq!(
            read_config(&fixture.config_path)["chatgpt_base_url"].as_str(),
            Some(BACKEND_URL)
        );

        reconcile_owned_codex_runtime_url_path(&fixture.journal_path, MOVED_URL).unwrap();

        let config = read_config(&fixture.config_path);
        assert_eq!(config["openai_base_url"].as_str(), Some(MOVED_URL));
        assert!(config.get("chatgpt_base_url").is_none());
        assert_eq!(fixture.journal().installed_base_url, MOVED_URL);
    }

    // The upgrade path: this start is the only thing that will ever clean it up when
    // the port is unchanged and automatic connection is off.
    #[test]
    fn reconcile_removes_what_an_older_build_wrote_when_the_port_has_not_moved() {
        let fixture = Fixture::unchanged("reconcile-same-port");

        reconcile_owned_codex_runtime_url_path(&fixture.journal_path, GATEWAY_URL).unwrap();

        let config = read_config(&fixture.config_path);
        assert_eq!(config["openai_base_url"].as_str(), Some(GATEWAY_URL));
        assert!(config.get("chatgpt_base_url").is_none());
    }

    #[test]
    fn reconcile_leaves_a_backend_url_the_user_set_when_the_port_has_not_moved() {
        let fixture = Fixture::unchanged("reconcile-same-port-user");
        fixture.set_in_config("chatgpt_base_url", "https://example.invalid/backend-api");

        reconcile_owned_codex_runtime_url_path(&fixture.journal_path, GATEWAY_URL).unwrap();

        assert_eq!(
            read_config(&fixture.config_path)["chatgpt_base_url"].as_str(),
            Some("https://example.invalid/backend-api")
        );
    }

    #[test]
    fn reconcile_leaves_a_backend_url_the_user_changed() {
        let fixture = Fixture::unchanged("reconcile-user-backend");
        fixture.set_in_config("chatgpt_base_url", "https://example.invalid/backend-api");

        reconcile_owned_codex_runtime_url_path(&fixture.journal_path, MOVED_URL).unwrap();

        let config = read_config(&fixture.config_path);
        assert_eq!(config["openai_base_url"].as_str(), Some(MOVED_URL));
        assert_eq!(
            config["chatgpt_base_url"].as_str(),
            Some("https://example.invalid/backend-api")
        );
    }

    #[test]
    fn an_install_after_a_port_change_takes_back_the_url_the_journal_installed() {
        let fixture = Fixture::unchanged("install-port");

        fixture.install(MOVED_URL).unwrap();

        let config = read_config(&fixture.config_path);
        assert_eq!(config["openai_base_url"].as_str(), Some(MOVED_URL));
        assert!(config.get("chatgpt_base_url").is_none());
        assert_eq!(fixture.journal().installed_base_url, MOVED_URL);
    }

    #[test]
    fn an_install_still_refuses_urls_the_journal_did_not_install() {
        let fixture = Fixture::unchanged("install-foreign-openai");
        fixture.set_in_config("openai_base_url", REMOTE_URL);
        let error = fixture.install(MOVED_URL).unwrap_err();
        assert!(error.contains("openai_base_url"), "{error}");

        let fixture = Fixture::unchanged("install-foreign-backend");
        fixture.set_in_config("chatgpt_base_url", "https://example.invalid/backend-api");
        let error = fixture.install(MOVED_URL).unwrap_err();
        assert!(error.contains("chatgpt_base_url"), "{error}");
    }

    #[test]
    fn an_install_with_no_journal_owns_no_url_but_the_one_it_would_write() {
        let fixture = Fixture::unchanged("install-no-journal");
        fs::remove_file(&fixture.journal_path).unwrap();

        let error = fixture.install(MOVED_URL).unwrap_err();

        assert!(error.contains("openai_base_url"), "{error}");
    }

    // #8
    #[tokio::test(flavor = "current_thread")]
    async fn an_install_tells_the_gateway_what_it_saved() {
        let saved = GatewaySettings {
            default_provider: Some("adopted".into()),
            ..GatewaySettings::default()
        };
        let applied = RefCell::new(None);

        let path = install_then_apply_saved_settings(
            || Ok("/Users/test/.codex/config.toml".to_string()),
            || Ok(saved.clone()),
            |latest| {
                applied.replace(Some(latest.default_provider.clone()));
                std::future::ready(Ok(()))
            },
        )
        .await
        .unwrap();

        assert_eq!(path, "/Users/test/.codex/config.toml");
        assert_eq!(applied.into_inner(), Some(Some("adopted".to_string())));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_install_tells_the_gateway_nothing() {
        let applied = RefCell::new(false);

        let result = install_then_apply_saved_settings(
            || Err("refused".to_string()),
            || Ok(GatewaySettings::default()),
            |_| {
                applied.replace(true);
                std::future::ready(Ok(()))
            },
        )
        .await;

        assert_eq!(result, Err("refused".to_string()));
        assert!(!applied.into_inner());
    }

    #[test]
    fn a_gateway_that_rejects_the_settings_is_shown_without_failing_the_install() {
        let _guard = connection_error_guard();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        let result = runtime.block_on(install_then_apply_saved_settings(
            || Ok("/Users/test/.codex/config.toml".to_string()),
            || Ok(GatewaySettings::default()),
            |_| std::future::ready(Err("invalid settings".to_string())),
        ));

        assert!(result.is_ok());
        let shown = codex_connection_error().expect("shown in the panel");
        assert!(shown.contains("invalid settings"), "{shown}");
        set_codex_connection_error(None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn no_embedded_gateway_means_nothing_to_tell() {
        let manager = GatewayManager::default();
        assert!(
            apply_settings_to_embedded_gateway(&manager, GatewaySettings::default())
                .await
                .is_ok()
        );
    }

    // #9
    #[test]
    fn removing_a_provider_cli_adoption_would_recreate_records_the_dismissal() {
        fn provider(id: &str) -> ProviderDefinition {
            ProviderDefinition {
                id: id.into(),
                name: id.into(),
                base_url: "https://example.invalid/v1".into(),
                ..ProviderDefinition::default()
            }
        }
        let mut settings = GatewaySettings {
            providers: vec![provider("zai"), provider("kimi-code"), provider("deepseek")],
            default_provider: Some("zai".into()),
            ..GatewaySettings::default()
        };

        remove_provider_from_settings(&mut settings, "zai").unwrap();
        remove_provider_from_settings(&mut settings, "kimi-code").unwrap();
        remove_provider_from_settings(&mut settings, "deepseek").unwrap();

        assert_eq!(settings.account_pool.dismissed_cli_imports, ["zai", "kimi"]);
        assert!(settings.providers.is_empty());
        assert_eq!(settings.default_provider, None);
        assert!(remove_provider_from_settings(&mut settings, "zai").is_err());
    }

    // #10
    #[test]
    fn a_backup_holding_codetas_own_chatgpt_url_is_not_restored() {
        let fixture = Fixture::unchanged("own-backend-url");
        let journal = fixture.journal();
        let restored = |backup_url: &str| {
            let backup = format!("chatgpt_base_url = \"{backup_url}\"\n")
                .parse::<DocumentMut>()
                .unwrap();
            let mut document = read_config(&fixture.config_path);
            let mut conflicts = Vec::new();
            apply_owned_codex_restore(&mut document, &backup, &journal, None, &mut conflicts)
                .unwrap();
            assert!(conflicts.is_empty(), "{conflicts:?}");
            document
                .get("chatgpt_base_url")
                .and_then(Item::as_str)
                .map(str::to_string)
        };

        assert_eq!(restored("http://127.0.0.1:42421/v1"), None);
        assert_eq!(restored("http://127.0.0.1:42421/backend-api"), None);
        assert_eq!(restored("http://localhost:43000/v1"), None);
        assert_eq!(
            restored("https://chatgpt.com/backend-api").as_deref(),
            Some("https://chatgpt.com/backend-api")
        );
    }

    #[test]
    fn a_backup_holding_codetas_own_openai_url_is_not_restored() {
        let fixture = Fixture::unchanged("own-openai-url");
        let journal = fixture.journal();
        let restored = |backup_url: &str| {
            let backup = format!("openai_base_url = \"{backup_url}\"\n")
                .parse::<DocumentMut>()
                .unwrap();
            let mut document = read_config(&fixture.config_path);
            let mut conflicts = Vec::new();
            apply_owned_codex_restore(&mut document, &backup, &journal, None, &mut conflicts)
                .unwrap();
            assert!(conflicts.is_empty(), "{conflicts:?}");
            document
                .get("openai_base_url")
                .and_then(Item::as_str)
                .map(str::to_string)
        };

        assert_eq!(restored(GATEWAY_URL), None);
        assert_eq!(restored(MOVED_URL), None);
        assert_eq!(restored(REMOTE_URL).as_deref(), Some(REMOTE_URL));
    }

    // #12
    #[test]
    fn a_connection_failure_is_shown_with_no_install_journal() {
        let _guard = connection_error_guard();
        let root = fallback_temp_root("no-journal");
        let missing = root.join("codex-install-journal.json");

        let result = record_convergence_result(Err("openai_base_url is user-owned".into()));

        assert!(result.is_err());
        assert_eq!(
            codex_connection_problem_at(&missing).as_deref(),
            Some("openai_base_url is user-owned")
        );

        assert!(record_convergence_result(Ok("connected".into())).is_ok());
        assert_eq!(codex_connection_problem_at(&missing), None);
    }

    #[test]
    fn the_panel_shows_a_failed_connection_before_a_failed_fallback() {
        let _guard = connection_error_guard();
        let fixture = Fixture::unchanged("panel-order");
        record_watchdog_error(&fixture.journal_path, Some("fallback failed")).unwrap();

        assert_eq!(
            codex_connection_problem_at(&fixture.journal_path).as_deref(),
            Some("fallback failed")
        );

        let _ = record_convergence_result(Err("connection failed".into()));
        assert_eq!(
            codex_connection_problem_at(&fixture.journal_path).as_deref(),
            Some("connection failed")
        );
        set_codex_connection_error(None);
    }
}
