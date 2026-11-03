use super::*;

#[tauri::command]
pub fn gateway_service_status() -> Result<ServiceStatus, String> {
    service::status()
}

#[tauri::command]
pub async fn install_gateway_service(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    input: ServiceInstallInput,
) -> Result<ServiceInstallReport, String> {
    let _mutation = manager.settings_mutation.lock().await;
    if service::status()?.installed {
        return Err("常駐サービスは登録済みです。設定保存時に安全に再起動されます".into());
    }
    let previous = load_settings(&app)?;
    let executable = env::current_exe()
        .map_err(|error| format!("CODETAS実行ファイルを特定できません: {error}"))?;
    let settings_file = settings_path(&app)?;
    let observability_directory = observability_path(&app)?;
    let warnings = service::validate_service_settings(&previous)?;
    let mut settings = previous.clone();
    settings.runtime.standalone_service = true;
    settings.validate()?;
    let catalog_snapshot = snapshot_codex_catalog_transition(&app, &previous, &settings)?;
    save_settings(&app, &settings)?;

    let prior_handle = manager.handle.lock().await.take();
    let was_running = prior_handle.is_some();
    if let Some(handle) = prior_handle {
        handle.shutdown().await;
    }
    let result = service::install(
        &executable,
        &settings_file,
        &observability_directory,
        input.install_shim,
        warnings,
    );
    match result {
        Ok(report) => Ok(report),
        Err(error) => {
            let mut restore_errors =
                restore_settings_and_catalog(&app, &previous, catalog_snapshot.as_ref());
            if was_running {
                match start_managed_gateway(&app, previous).await {
                    Ok(handle) => *manager.handle.lock().await = Some(handle),
                    Err(restore) => {
                        restore_errors.push(format!("埋め込みGateway: {restore}"));
                    }
                }
            }
            if restore_errors.is_empty() {
                Err(format!(
                    "常駐サービスを登録できないため旧状態へ復元しました: {error}"
                ))
            } else {
                Err(format!(
                    "常駐サービスを登録できず、旧状態の復元にも問題がありました: {error}; {}",
                    restore_errors.join("; ")
                ))
            }
        }
    }
}

#[tauri::command]
pub async fn launch_provider_oauth_broker(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    input: ProviderOAuthInput,
) -> Result<ProviderOAuthLaunchReport, String> {
    let _mutation = manager.settings_mutation.lock().await;
    let provider_id = input.provider_id.clone();
    let previous = load_settings(&app)?;
    let mut settings = previous.clone();
    let provider_index = settings
        .providers
        .iter()
        .position(|provider| provider.id == provider_id)
        .ok_or_else(|| format!("プロバイダーが見つかりません: {provider_id}"))?;
    let broker = input.broker.as_deref().unwrap_or_else(|| {
        match settings.providers[provider_index].id.as_str() {
            "github-copilot" => "github-cli",
            "google" | "google-vertex" => "gcloud",
            "anthropic" => "claude-cli",
            _ => "",
        }
    });
    configure_desktop_auth_store(&app)?;
    if provider_id == "google-antigravity"
        || provider_supports_native_oauth(&provider_id)
        || broker == "claude-cli"
    {
        let login = login_provider_oauth(&provider_id).await?;
        settings.providers[provider_index].enabled = true;
        settings.providers[provider_index].api_key_env = None;
        settings.providers[provider_index].credential = oauth_session_credential(&provider_id);
        settings.validate()?;
        persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
        return Ok(ProviderOAuthLaunchReport {
            provider_id,
            broker: if login.imported {
                "local-cli".into()
            } else {
                "codetas-oauth".into()
            },
            launched: true,
            token_command: String::new(),
            instructions: login.message,
        });
    }
    let (program_name, login_args, token_args, refresh_interval_ms) = oauth_broker_spec(broker)?;
    let program = find_program_on_path(program_name)?;
    let credential = ProviderCredential {
        source: CredentialSource::OAuth,
        reference: Some(broker.to_string()),
        transport: CredentialTransport::Bearer,
        header_name: None,
        command: Some(CredentialCommand {
            program: program.to_string_lossy().into_owned(),
            args: token_args.clone(),
            cwd: None,
            timeout_ms: 15_000,
            refresh_interval_ms,
        }),
    };
    // Do not alter the active provider until the detached terminal reports a successful
    // login and a second, output-discarding token command confirms the broker session.
    // Cancelling the browser or closing the terminal therefore leaves the old credential intact.
    run_oauth_terminal_transaction(&app, &program, &login_args, &token_args, broker).await?;
    settings.providers[provider_index].api_key_env = None;
    settings.providers[provider_index].credential = credential;
    settings.validate()?;
    persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
    Ok(ProviderOAuthLaunchReport {
        provider_id,
        broker: broker.to_string(),
        launched: true,
        token_command: format!(
            "{} {}",
            program.display(),
            token_args.join(" ")
        ),
        instructions: format!(
            "{broker}のログインとtoken取得確認が完了しました。秘密は外部CLIが所有し、CODETASには保存していません。"
        ),
    })
}

fn sanitize_account_slug(raw: &str) -> String {
    raw.chars()
        .map(|character| {
            if character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '_')
            {
                character
            } else if character.is_ascii_uppercase() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(48)
        .collect()
}

fn oauth_account_id_for_store_key(
    settings: &GatewaySettings,
    provider_id: &str,
    store_key: &str,
) -> String {
    let base = session_key_provider(store_key);
    let suffix = store_key
        .strip_prefix(&format!("{base}#"))
        .unwrap_or("default");
    let slug = sanitize_account_slug(suffix);
    let slug = if slug.is_empty() {
        "account".to_string()
    } else {
        slug
    };
    let prefix = sanitize_account_slug(provider_id);
    let base_id = format!("{prefix}-{slug}")
        .chars()
        .take(56)
        .collect::<String>();
    let base_id = base_id.trim_end_matches('-').to_string();
    if !settings
        .account_pool
        .accounts
        .iter()
        .any(|account| account.id == base_id)
    {
        return base_id;
    }
    for index in 2..1_000 {
        let candidate = format!("{base_id}-{index}");
        if !settings
            .account_pool
            .accounts
            .iter()
            .any(|account| account.id == candidate)
        {
            return candidate;
        }
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("{}-{}", base_id.chars().take(50).collect::<String>(), stamp)
}

/// Adds account-pool entries for OAuth sessions that are not represented yet.
/// This is how a legacy single-session login (`anthropic`) becomes the first
/// routable account before a second one is added.
fn materialize_oauth_accounts(
    settings: &mut GatewaySettings,
    provider_id: &str,
    sessions: &[codetas_gateway::OAuthStoredAccount],
) -> bool {
    let mut changed = false;
    for session in sessions {
        let already = settings.account_pool.accounts.iter().any(|account| {
            account.provider_id == provider_id
                && account.credential.reference.as_deref() == Some(session.store_key.as_str())
        });
        if already {
            continue;
        }
        let account_id = oauth_account_id_for_store_key(settings, provider_id, &session.store_key);
        let label = session
            .email
            .clone()
            .or_else(|| session.account_id.clone())
            .unwrap_or_else(|| session.store_key.clone());
        settings.account_pool.accounts.push(AccountReference {
            id: account_id,
            provider_id: provider_id.to_string(),
            label,
            credential: oauth_session_credential(&session.store_key),
            enabled: true,
            priority: 0,
            paused: false,
            pause_until_unix: None,
            pinned: false,
        });
        changed = true;
    }
    changed
}

fn adopt_provider_credential_main(settings: &mut GatewaySettings, provider_id: &str) {
    if settings
        .account_pool
        .active_accounts
        .contains_key(provider_id)
    {
        return;
    }
    let reference = settings
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .and_then(|provider| provider.credential.reference.clone());
    let Some(reference) = reference else {
        return;
    };
    if let Some(account_id) = settings
        .account_pool
        .accounts
        .iter()
        .find(|account| {
            account.provider_id == provider_id
                && account.credential.reference.as_deref() == Some(reference.as_str())
        })
        .map(|account| account.id.clone())
    {
        settings
            .account_pool
            .active_accounts
            .insert(provider_id.to_string(), account_id);
    }
}

/// Accounts are OAuth-only. Adding one to a provider that currently uses an
/// environment key, Keychain entry, command, forward auth, or a CLI broker
/// would silently change which credential pays for requests, so those
/// providers must switch to OAuth deliberately first.
fn provider_uses_external_credential(provider: &ProviderDefinition) -> bool {
    let environment_name = provider
        .credential
        .reference
        .as_deref()
        .or(provider.api_key_env.as_deref());
    let environment_is_exported = environment_name
        .is_some_and(|name| env::var_os(name).is_some_and(|value| !value.is_empty()));
    match provider.credential.source {
        // A preset's `ANTHROPIC_API_KEY` placeholder with no exported value is
        // not an active credential; adopting OAuth is how the provider becomes
        // usable at all.
        CredentialSource::Environment => environment_is_exported,
        CredentialSource::Keychain | CredentialSource::Command | CredentialSource::Forward => true,
        CredentialSource::OAuth => provider.credential.command.is_some(),
        CredentialSource::None => environment_is_exported,
    }
}

fn require_account_managed_provider(provider: &ProviderDefinition) -> Result<(), String> {
    if provider_uses_external_credential(provider) {
        return Err(format!(
            "{} はAPIキーなどOAuth以外の認証で設定されています。アカウント管理を使うには、先に認証方法をOAuthログインへ変更してください。",
            provider.id
        ));
    }
    Ok(())
}

/// Selects the account that CODETAS prefers. The routing pool still sorts by
/// observed quota, so the main account is used first only while it is under
/// `autoSwitchThresholdPercent`; otherwise a free account takes over.
pub(crate) fn set_primary_account(
    settings: &mut GatewaySettings,
    provider_id: &str,
    account_id: &str,
) -> Result<(), String> {
    let provider_index = settings
        .providers
        .iter()
        .position(|provider| provider.id == provider_id)
        .ok_or_else(|| format!("プロバイダーが見つかりません: {provider_id}"))?;
    let highest_priority = settings
        .account_pool
        .accounts
        .iter()
        .filter(|account| account.provider_id == provider_id && account.enabled)
        .map(|account| account.priority)
        .max()
        .unwrap_or(0);
    let account = settings
        .account_pool
        .accounts
        .iter_mut()
        .find(|account| account.provider_id == provider_id && account.id == account_id)
        .ok_or_else(|| format!("アカウントが見つかりません: {account_id}"))?;
    if !account.enabled {
        return Err("無効なアカウントはメインにできません".into());
    }
    account.paused = false;
    account.pause_until_unix = None;
    if account.priority < highest_priority {
        account.priority = highest_priority;
    }
    let credential = account.credential.clone();
    for other in settings
        .account_pool
        .accounts
        .iter_mut()
        .filter(|other| other.provider_id == provider_id)
    {
        other.pinned = false;
    }
    settings
        .account_pool
        .active_accounts
        .insert(provider_id.to_string(), account_id.to_string());
    let provider = &mut settings.providers[provider_index];
    provider.credential = credential;
    provider.api_key_env = None;
    provider.enabled = true;
    Ok(())
}

fn account_view(
    account_id: String,
    label: String,
    session: Option<&codetas_gateway::OAuthStoredAccount>,
    store_key: Option<String>,
    main: bool,
    enabled: bool,
) -> ProviderOAuthAccountView {
    ProviderOAuthAccountView {
        account_id,
        label,
        email: session.and_then(|session| session.email.clone()),
        store_key,
        main,
        enabled,
        expires_at_ms: session.map(|session| session.expires_at_ms).unwrap_or(0),
        source: session
            .map(|session| session.source.clone())
            .unwrap_or_else(|| "oauth".to_string()),
    }
}

#[tauri::command]
pub fn list_provider_oauth_accounts(
    app: AppHandle,
    provider_id: String,
) -> Result<Vec<ProviderOAuthAccountView>, String> {
    configure_desktop_auth_store(&app)?;
    let settings = load_settings(&app)?;
    let provider = settings
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .ok_or_else(|| format!("プロバイダーが見つかりません: {provider_id}"))?;
    let sessions = list_stored_oauth_accounts(&provider_id).unwrap_or_default();
    let active = settings
        .account_pool
        .active_accounts
        .get(&provider_id)
        .cloned();
    let provider_main = provider.credential.reference.clone();
    let mut views = Vec::new();
    let mut covered = std::collections::BTreeSet::new();
    for account in settings
        .account_pool
        .accounts
        .iter()
        .filter(|account| account.provider_id == provider_id)
    {
        if account.credential.source != CredentialSource::OAuth
            || account.credential.command.is_some()
        {
            continue;
        }
        let Some(reference) = account.credential.reference.as_deref() else {
            continue;
        };
        let session = sessions
            .iter()
            .find(|session| session.store_key == reference);
        covered.insert(reference.to_string());
        views.push(account_view(
            account.id.clone(),
            account.label.clone(),
            session,
            Some(reference.to_string()),
            active.as_deref() == Some(account.id.as_str()),
            account.enabled,
        ));
    }
    for session in &sessions {
        if covered.contains(&session.store_key) {
            continue;
        }
        let account_id =
            oauth_account_id_for_store_key(&settings, &provider_id, &session.store_key);
        let label = session
            .email
            .clone()
            .or_else(|| session.account_id.clone())
            .unwrap_or_else(|| session.store_key.clone());
        views.push(account_view(
            account_id,
            label,
            Some(session),
            Some(session.store_key.clone()),
            active.is_none() && provider_main.as_deref() == Some(session.store_key.as_str()),
            true,
        ));
    }
    if !views.iter().any(|view| view.main) {
        if let Some(index) = views
            .iter()
            .position(|view| provider_main.as_deref() == view.store_key.as_deref())
        {
            views[index].main = true;
        } else if views.len() == 1 {
            views[0].main = true;
        }
    }
    Ok(views)
}

#[tauri::command]
pub async fn add_provider_oauth_account(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    provider_id: String,
) -> Result<ProviderOAuthAccountView, String> {
    let _mutation = manager.settings_mutation.lock().await;
    configure_desktop_auth_store(&app)?;
    if !provider_supports_native_oauth(&provider_id) {
        return Err(format!("{provider_id} はアプリ内OAuthに未対応です"));
    }
    let previous = load_settings(&app)?;
    let mut settings = previous.clone();
    let provider_index = settings
        .providers
        .iter()
        .position(|provider| provider.id == provider_id)
        .ok_or_else(|| format!("プロバイダーが見つかりません: {provider_id}"))?;
    require_account_managed_provider(&settings.providers[provider_index])?;
    let sessions = list_stored_oauth_accounts(&provider_id).unwrap_or_default();
    materialize_oauth_accounts(&mut settings, &provider_id, &sessions);
    adopt_provider_credential_main(&mut settings, &provider_id);
    let session = login_provider_oauth_new_account(&provider_id).await?;
    let label = session
        .email
        .clone()
        .or_else(|| session.account_id.clone())
        .unwrap_or_else(|| session.store_key.clone());
    // Signing into an account that is already configured must update it, not
    // create a second pool entry that points at the same token.
    let existing_account_id = settings
        .account_pool
        .accounts
        .iter()
        .find(|account| {
            account.provider_id == provider_id
                && account.credential.reference.as_deref() == Some(session.store_key.as_str())
        })
        .map(|account| account.id.clone());
    if let Some(existing_account_id) = existing_account_id {
        if let Some(account) =
            settings.account_pool.accounts.iter_mut().find(|account| {
                account.provider_id == provider_id && account.id == existing_account_id
            })
        {
            account.label = label.clone();
            account.enabled = true;
        }
        settings.validate()?;
        let main = settings
            .account_pool
            .active_accounts
            .get(&provider_id)
            .is_some_and(|active| active == &existing_account_id);
        persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
        return Ok(account_view(
            existing_account_id,
            label,
            Some(&session),
            Some(session.store_key.clone()),
            main,
            true,
        ));
    }
    let account_id = oauth_account_id_for_store_key(&settings, &provider_id, &session.store_key);
    settings.account_pool.accounts.push(AccountReference {
        id: account_id.clone(),
        provider_id: provider_id.clone(),
        label: label.clone(),
        credential: oauth_session_credential(&session.store_key),
        enabled: true,
        priority: 0,
        paused: false,
        pause_until_unix: None,
        pinned: false,
    });
    settings.providers[provider_index].enabled = true;
    settings.providers[provider_index].api_key_env = None;
    if !settings
        .account_pool
        .active_accounts
        .contains_key(&provider_id)
    {
        set_primary_account(&mut settings, &provider_id, &account_id)?;
    }
    settings.validate()?;
    let main = settings
        .account_pool
        .active_accounts
        .get(&provider_id)
        .is_some_and(|active| active == &account_id);
    let store_key = session.store_key.clone();
    if let Err(error) = persist_and_apply_settings(&app, &manager, &previous, &settings).await {
        // The login token is already in the private store. Since this branch
        // only runs for a brand-new store key, remove it so a failed settings
        // save does not leave an unlisted account behind.
        let _ = remove_stored_oauth_account(&store_key);
        return Err(error);
    }
    Ok(account_view(
        account_id,
        label,
        Some(&session),
        Some(store_key),
        main,
        true,
    ))
}

#[tauri::command]
pub async fn set_provider_oauth_primary_account(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    provider_id: String,
    account_id: String,
) -> Result<(), String> {
    let _mutation = manager.settings_mutation.lock().await;
    configure_desktop_auth_store(&app)?;
    let previous = load_settings(&app)?;
    let mut settings = previous.clone();
    let provider_external = settings
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .map(provider_uses_external_credential)
        .unwrap_or(false);
    if provider_external {
        return Err(format!(
            "{provider_id} はAPIキーなどOAuth以外の認証で設定中です。先に認証方法をOAuthログインへ変更してください。"
        ));
    }
    let sessions = list_stored_oauth_accounts(&provider_id).unwrap_or_default();
    materialize_oauth_accounts(&mut settings, &provider_id, &sessions);
    set_primary_account(&mut settings, &provider_id, &account_id)?;
    settings.validate()?;
    persist_and_apply_settings(&app, &manager, &previous, &settings).await
}

#[tauri::command]
pub async fn remove_provider_oauth_account(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    provider_id: String,
    account_id: String,
) -> Result<(), String> {
    let _mutation = manager.settings_mutation.lock().await;
    configure_desktop_auth_store(&app)?;
    let previous = load_settings(&app)?;
    let mut settings = previous.clone();
    // Never materialize new pool accounts for a provider that is currently on
    // an API key or another non-OAuth credential: pool accounts take routing
    // precedence over the provider credential.
    let provider_external = settings
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .is_some_and(provider_uses_external_credential);
    let sessions = list_stored_oauth_accounts(&provider_id).unwrap_or_default();
    if !provider_external {
        materialize_oauth_accounts(&mut settings, &provider_id, &sessions);
    }
    let Some(index) = settings
        .account_pool
        .accounts
        .iter()
        .position(|account| account.provider_id == provider_id && account.id == account_id)
    else {
        return Ok(());
    };
    let removed = settings.account_pool.accounts.remove(index);
    let removed_reference = removed.credential.reference.clone();
    let was_main = settings
        .account_pool
        .active_accounts
        .get(&provider_id)
        .is_some_and(|active| active == &account_id);
    if was_main {
        settings.account_pool.active_accounts.remove(&provider_id);
    }
    let provider_points_at_removed = settings
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .is_some_and(|provider| provider.credential.reference == removed_reference);
    let replacement = settings
        .account_pool
        .accounts
        .iter()
        .filter(|account| account.provider_id == provider_id && account.enabled)
        .map(|account| account.id.clone())
        .next();
    // Only change the provider credential when it actually points at the
    // removed account. A provider switched to an API key must keep that key.
    let adopt_replacement = (was_main || provider_points_at_removed)
        && !settings
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .is_some_and(provider_uses_external_credential);
    if let Some(replacement) = replacement {
        if adopt_replacement {
            set_primary_account(&mut settings, &provider_id, &replacement)?;
        }
    } else if provider_points_at_removed {
        if let Some(provider) = settings
            .providers
            .iter_mut()
            .find(|provider| provider.id == provider_id)
        {
            provider.credential = ProviderCredential::default();
            provider.api_key_env = None;
            provider.enabled = false;
        }
    }
    settings.validate()?;
    persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
    // Delete the stored token only after the settings change is durable. If the
    // delete fails, the account is gone from settings and the orphan session is
    // left in the private store (harmless), never the reverse.
    if let Some(reference) = removed_reference {
        let still_used = settings
            .account_pool
            .accounts
            .iter()
            .any(|account| account.credential.reference.as_deref() == Some(reference.as_str()));
        if !still_used {
            let _ = remove_stored_oauth_account(&reference);
        }
    }
    Ok(())
}

pub(crate) fn oauth_broker_spec(
    broker: &str,
) -> Result<(&'static str, Vec<String>, Vec<String>, u64), String> {
    match broker {
        "github-cli" => Ok((
            "gh",
            vec![
                "auth".into(),
                "login".into(),
                "--hostname".into(),
                "github.com".into(),
                "--web".into(),
                "--git-protocol".into(),
                "https".into(),
            ],
            vec![
                "auth".into(),
                "token".into(),
                "--hostname".into(),
                "github.com".into(),
            ],
            15 * 60 * 1_000,
        )),
        "gcloud" => Ok((
            "gcloud",
            vec!["auth".into(), "login".into(), "--brief".into()],
            vec!["auth".into(), "print-access-token".into()],
            45 * 60 * 1_000,
        )),
        "" => Err("このプロバイダーには既定のOAuth brokerがありません".into()),
        _ => Err("OAuth brokerはgithub-cli、gcloud、またはclaude-cliを指定してください".into()),
    }
}

pub(crate) fn launch_claude_setup_token_terminal(
    app: &AppHandle,
    program: &Path,
) -> Result<(), String> {
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("CODETASデータフォルダを特定できません: {error}"))?
        .join("oauth-broker");
    fs::create_dir_all(&directory)
        .map_err(|error| format!("OAuth起動フォルダを作れません: {error}"))?;
    let script = directory.join("claude-setup-token.sh");
    let body = format!(
        "#!/bin/sh\nset -eu\necho 'Claude Pro/Max の setup-token を起動します。'\necho '表示されたトークンは CLAUDE_CODE_OAUTH_TOKEN に設定するか、CODETAS の Keychain 参照へ保存してください。'\necho\n{} setup-token\necho\nprintf '終わったらこの窓を閉じてください。'\nread -r _\n",
        shell_quote(&program.to_string_lossy())
    );
    fs::write(&script, body)
        .map_err(|error| format!("OAuth起動スクリプトを書けません: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("OAuth起動スクリプトを保護できません: {error}"))?;
    }
    launch_oauth_terminal(&script)
}

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub(crate) fn find_program_on_path(name: &str) -> Result<PathBuf, String> {
    let path = env::var_os("PATH").ok_or("PATHを取得できません")?;
    for directory in env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return fs::canonicalize(&candidate)
                .map_err(|error| format!("{}を解決できません: {error}", candidate.display()));
        }
        #[cfg(windows)]
        for extension in ["exe", "cmd", "bat"] {
            let candidate = directory.join(format!("{name}.{extension}"));
            if candidate.is_file() {
                return fs::canonicalize(&candidate)
                    .map_err(|error| format!("{}を解決できません: {error}", candidate.display()));
            }
        }
    }
    Err(format!("{name}がPATH上に見つかりません"))
}

pub(crate) async fn run_oauth_terminal_transaction(
    app: &AppHandle,
    program: &Path,
    login_args: &[String],
    token_args: &[String],
    broker: &str,
) -> Result<(), String> {
    let (script, result) = create_oauth_transaction_script(app, program, login_args, token_args)?;
    if let Err(error) = launch_oauth_terminal(&script) {
        cleanup_oauth_transaction(&script);
        return Err(error);
    }
    let deadline = Instant::now() + Duration::from_secs(20 * 60);
    loop {
        match fs::read_to_string(&result) {
            Ok(value) => {
                cleanup_oauth_transaction(&script);
                return if value.trim() == "ok" {
                    Ok(())
                } else {
                    Err(format!(
                        "{broker}のログインが完了しなかったため設定を変更しませんでした"
                    ))
                };
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                cleanup_oauth_transaction(&script);
                return Err(format!("OAuth結果を確認できません: {error}"));
            }
        }
        if Instant::now() >= deadline {
            cleanup_oauth_transaction(&script);
            return Err(
                "OAuthログインが20分以内に完了しなかったため設定を変更しませんでした".into(),
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

pub(crate) fn cleanup_oauth_transaction(script: &Path) {
    if let Some(directory) = script.parent() {
        let _ = fs::remove_dir_all(directory);
    }
}

#[cfg(unix)]
pub(crate) fn create_oauth_transaction_script(
    app: &AppHandle,
    program: &Path,
    login_args: &[String],
    token_args: &[String],
) -> Result<(PathBuf, PathBuf), String> {
    use std::os::unix::fs::PermissionsExt;
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("CODETASデータフォルダを特定できません: {error}"))?
        .join("oauth-broker");
    fs::create_dir_all(&directory)
        .map_err(|error| format!("OAuth起動フォルダを作れません: {error}"))?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("OAuth起動フォルダを保護できません: {error}"))?;
    let nonce = format!(
        "{}-{}-{}",
        now_ms(),
        std::process::id(),
        OAUTH_TRANSACTION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let transaction = directory.join(nonce);
    fs::create_dir(&transaction)
        .map_err(|error| format!("OAuth取引フォルダを作れません: {error}"))?;
    fs::set_permissions(&transaction, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("OAuth取引フォルダを保護できません: {error}"))?;
    let extension = if cfg!(target_os = "macos") {
        "command"
    } else {
        "sh"
    };
    let script = transaction.join(format!("login.{extension}"));
    let result = transaction.join("result.status");
    let mut login = shell_quote_value(&program.to_string_lossy());
    for argument in login_args {
        login.push(' ');
        login.push_str(&shell_quote_value(argument));
    }
    let mut verify = shell_quote_value(&program.to_string_lossy());
    for argument in token_args {
        verify.push(' ');
        verify.push_str(&shell_quote_value(argument));
    }
    let content = format!(
        "#!/bin/sh\n# CODETAS-OAUTH-BROKER-V2\numask 077\n{login}\nstatus=$?\nif [ \"$status\" -eq 0 ]; then\n  {verify} >/dev/null 2>&1\n  status=$?\nfi\nif [ \"$status\" -eq 0 ]; then\n  printf 'ok\\n' > {}\n  printf '\\nCODETAS: login verified. You can close this terminal.\\n'\nelse\n  printf 'failed\\n' > {}\n  printf '\\nCODETAS: login was not applied. You can close this terminal.\\n'\nfi\nexit \"$status\"\n",
        shell_quote_value(&result.to_string_lossy()),
        shell_quote_value(&result.to_string_lossy()),
    );
    atomic_write(&script, content.as_bytes())?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("OAuth起動スクリプトを保護できません: {error}"))?;
    Ok((script, result))
}

#[cfg(target_os = "windows")]
pub(crate) fn create_oauth_transaction_script(
    app: &AppHandle,
    program: &Path,
    login_args: &[String],
    token_args: &[String],
) -> Result<(PathBuf, PathBuf), String> {
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("CODETASデータフォルダを特定できません: {error}"))?
        .join("oauth-broker");
    fs::create_dir_all(&directory)
        .map_err(|error| format!("OAuth起動フォルダを作れません: {error}"))?;
    let nonce = format!(
        "{}-{}-{}",
        now_ms(),
        std::process::id(),
        OAUTH_TRANSACTION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let transaction = directory.join(nonce);
    fs::create_dir(&transaction)
        .map_err(|error| format!("OAuth取引フォルダを作れません: {error}"))?;
    let script = transaction.join("login.ps1");
    let result = transaction.join("result.status");
    let login = powershell_invocation(program, login_args);
    let verify = powershell_invocation(program, token_args);
    let content = format!(
        "# CODETAS-OAUTH-BROKER-V2\r\n{login}\r\n$status = $LASTEXITCODE\r\nif ($status -eq 0) {{\r\n  {verify} *> $null\r\n  $status = $LASTEXITCODE\r\n}}\r\nif ($status -eq 0) {{\r\n  Set-Content -LiteralPath {} -Value 'ok' -NoNewline\r\n  Write-Host '\nCODETAS: login verified. You can close this terminal.'\r\n}} else {{\r\n  Set-Content -LiteralPath {} -Value 'failed' -NoNewline\r\n  Write-Host '\nCODETAS: login was not applied. You can close this terminal.'\r\n}}\r\nexit $status\r\n",
        powershell_quote_value(&result.to_string_lossy()),
        powershell_quote_value(&result.to_string_lossy()),
    );
    atomic_write(&script, content.as_bytes())?;
    Ok((script, result))
}

#[cfg(target_os = "macos")]
pub(crate) fn launch_oauth_terminal(script: &Path) -> Result<(), String> {
    let launch = Command::new("/usr/bin/open")
        .args(["-a", "Terminal"])
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match launch {
        Ok(_) => Ok(()),
        Err(error) => Err(format!("Terminalを起動できません: {error}")),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn launch_oauth_terminal(script: &Path) -> Result<(), String> {
    let candidates: [(&str, &[&str]); 4] = [
        ("x-terminal-emulator", &["-e"]),
        ("gnome-terminal", &["--"]),
        ("konsole", &["-e"]),
        ("xterm", &["-e"]),
    ];
    let mut attempted = Vec::new();
    for (terminal, prefix) in candidates {
        let mut command = Command::new(terminal);
        command
            .args(prefix)
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match command.spawn() {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                attempted.push(terminal);
            }
            Err(error) => return Err(format!("{terminal}を起動できません: {error}")),
        }
    }
    Err(format!(
        "OAuth用ターミナルが見つかりません: {}",
        attempted.join(", ")
    ))
}

#[cfg(target_os = "windows")]
pub(crate) fn launch_oauth_terminal(script: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    Command::new("powershell.exe")
        .creation_flags(0x0000_0010)
        .args(["-NoLogo", "-NoExit", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("OAuthターミナルを起動できません: {error}"))
}

#[cfg(unix)]
pub(crate) fn shell_quote_value(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(target_os = "windows")]
pub(crate) fn powershell_invocation(program: &Path, args: &[String]) -> String {
    let mut output = format!("& {}", powershell_quote_value(&program.to_string_lossy()));
    for argument in args {
        output.push(' ');
        output.push_str(&powershell_quote_value(argument));
    }
    output
}

#[cfg(target_os = "windows")]
pub(crate) fn powershell_quote_value(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[tauri::command]
pub async fn start_gateway_service(
    manager: State<'_, GatewayManager>,
) -> Result<ServiceStatus, String> {
    let _mutation = manager.settings_mutation.lock().await;
    if !service::start()? {
        return Err("常駐Gatewayの起動状態を確認できません".into());
    }
    service::status()
}

#[tauri::command]
pub async fn restart_gateway_service(
    manager: State<'_, GatewayManager>,
) -> Result<ServiceStatus, String> {
    let _mutation = manager.settings_mutation.lock().await;
    if !service::restart()? {
        return Err("常駐Gatewayの再起動状態を確認できません".into());
    }
    service::status()
}

#[tauri::command]
pub async fn stop_gateway_service(
    manager: State<'_, GatewayManager>,
) -> Result<ServiceStatus, String> {
    let _mutation = manager.settings_mutation.lock().await;
    if !service::stop()? {
        return Err("常駐Gatewayを停止できませんでした".into());
    }
    let status = service::status()?;
    if status.running {
        return Err("常駐Gatewayが停止状態になっていません".into());
    }
    Ok(status)
}

#[tauri::command]
pub async fn uninstall_gateway_service(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<ServiceUninstallReport, String> {
    let _mutation = manager.settings_mutation.lock().await;
    let previous = load_settings(&app)?;
    let mut settings = previous.clone();
    settings.runtime.standalone_service = false;
    settings.validate()?;
    let catalog_snapshot = snapshot_codex_catalog_transition(&app, &previous, &settings)?;
    save_settings(&app, &settings)?;
    let report = match service::uninstall() {
        Ok(report) => report,
        Err(error) => {
            let restore_errors =
                restore_settings_and_catalog(&app, &previous, catalog_snapshot.as_ref());
            return Err(if restore_errors.is_empty() {
                format!("常駐サービスを解除できないため旧設定へ復元しました: {error}")
            } else {
                format!(
                    "常駐サービスを解除できず、旧状態の復元にも失敗しました: {error}; {}",
                    restore_errors.join("; ")
                )
            });
        }
    };
    if settings.runtime.auto_start {
        let handle = start_managed_gateway(&app, settings).await?;
        *manager.handle.lock().await = Some(handle);
    }
    Ok(report)
}

#[cfg(test)]
mod oauth_account_tests {
    use super::*;
    use codetas_gateway::{provider_presets, OAuthStoredAccount};

    fn anthropic_settings() -> GatewaySettings {
        let mut settings = GatewaySettings::default();
        let provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == "anthropic")
            .expect("anthropic preset")
            .instantiate(None)
            .expect("anthropic provider");
        settings.providers.push(provider);
        settings
    }

    #[test]
    fn materializes_legacy_and_scoped_sessions_without_duplicates() {
        let mut settings = anthropic_settings();
        let sessions = vec![
            OAuthStoredAccount {
                store_key: "anthropic".into(),
                account_id: Some("legacy".into()),
                email: Some("main@example.com".into()),
                expires_at_ms: 1,
                source: "oauth".into(),
            },
            OAuthStoredAccount {
                store_key: "anthropic#0f9a-account".into(),
                account_id: Some("0f9a-account".into()),
                email: Some("second@example.com".into()),
                expires_at_ms: 2,
                source: "oauth".into(),
            },
        ];

        assert!(materialize_oauth_accounts(
            &mut settings,
            "anthropic",
            &sessions
        ));
        assert!(!materialize_oauth_accounts(
            &mut settings,
            "anthropic",
            &sessions
        ));
        assert_eq!(settings.account_pool.accounts.len(), 2);
    }

    #[test]
    fn setting_main_moves_the_active_marker_and_provider_credential() {
        let mut settings = anthropic_settings();
        settings.account_pool.accounts = vec![
            AccountReference {
                id: "anthropic-main".into(),
                provider_id: "anthropic".into(),
                label: "main@example.com".into(),
                credential: oauth_session_credential("anthropic#main"),
                pinned: true,
                ..AccountReference::default()
            },
            AccountReference {
                id: "anthropic-second".into(),
                provider_id: "anthropic".into(),
                label: "second@example.com".into(),
                credential: oauth_session_credential("anthropic#second"),
                ..AccountReference::default()
            },
        ];
        settings
            .account_pool
            .active_accounts
            .insert("anthropic".into(), "anthropic-main".into());

        set_primary_account(&mut settings, "anthropic", "anthropic-second").expect("switch main");

        assert_eq!(
            settings
                .account_pool
                .active_accounts
                .get("anthropic")
                .map(String::as_str),
            Some("anthropic-second")
        );
        assert!(!settings.account_pool.accounts[0].pinned);
        assert_eq!(
            settings.providers[0].credential.reference.as_deref(),
            Some("anthropic#second")
        );
        settings.validate().expect("valid settings");
    }

    #[test]
    fn external_credentials_are_not_treated_as_account_managed() {
        let mut settings = anthropic_settings();
        settings.providers[0].credential.reference =
            Some("CODETAS_TEST_UNSET_ANTHROPIC_KEY".into());
        settings.providers[0].api_key_env = Some("CODETAS_TEST_UNSET_ANTHROPIC_KEY".into());
        assert!(!provider_uses_external_credential(&settings.providers[0]));

        settings.providers[0].credential.source = CredentialSource::OAuth;
        settings.providers[0].credential.command = Some(CredentialCommand {
            program: "claude".into(),
            args: Vec::new(),
            cwd: None,
            timeout_ms: 1_000,
            refresh_interval_ms: 1_000,
        });
        assert!(provider_uses_external_credential(&settings.providers[0]));

        settings.providers[0].credential.command = None;
        settings.providers[0].credential.source = CredentialSource::Environment;
        settings.providers[0].credential.reference =
            Some("CODETAS_TEST_UNSET_ANTHROPIC_KEY".into());
        assert!(!provider_uses_external_credential(&settings.providers[0]));
        settings.providers[0].credential.reference = Some("PATH".into());
        assert!(provider_uses_external_credential(&settings.providers[0]));
        assert!(require_account_managed_provider(&settings.providers[0]).is_err());

        // An exported preset key blocks account management even when the
        // credential source is still the default `none`.
        settings.providers[0].credential = ProviderCredential::default();
        settings.providers[0].api_key_env = Some("PATH".into());
        assert!(provider_uses_external_credential(&settings.providers[0]));
    }
}
