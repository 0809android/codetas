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
    let cli_import = oauth_provider_registry().iter().any(|descriptor| {
        descriptor.cli_import
            && (descriptor.id == provider_id || descriptor.aliases.contains(&provider_id.as_str()))
    });
    let uses_cli_login = provider_id == "google-antigravity"
        || provider_supports_native_oauth(&provider_id)
        || cli_import
        || broker == "claude-cli";
    // `gcloud auth print-access-token` is Google's documented way to call Vertex, so
    // for Google's own providers that broker is not reuse of another app's login and
    // needs no consent. Any other provider would send the token elsewhere.
    let official_gcloud =
        broker == "gcloud" && matches!(provider_id.as_str(), "google" | "google-vertex");
    if uses_cli_login || !official_gcloud {
        super::cli_consent::require_cli_login_reuse(&app).await?;
    }
    if uses_cli_login {
        refuse_cli_login_for_connection(&settings.providers[provider_index])?;
        let login = login_provider_oauth(&provider_id).await?;
        restore_cli_import(&mut settings.account_pool, &provider_id);
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
    // The broker's token command replaces the provider's credential, so the provider's
    // endpoint would receive that token. Settings saved from the window confirm such a
    // change natively; a renderer-supplied provider and broker must not skip it.
    let mut proposed = settings.clone();
    proposed.providers[provider_index].api_key_env = None;
    proposed.providers[provider_index].credential = credential.clone();
    proposed.validate()?;
    super::sensitive_changes::confirm_sensitive_setting_changes(&previous, &proposed).await?;
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

fn refuse_cli_login_for_connection(provider: &ProviderDefinition) -> Result<(), String> {
    if codetas_gateway::provider_may_take_cli_login(provider) {
        return Ok(());
    }
    Err(format!(
        "{} は公式の接続先以外を指しているので、CLI のログインは使えません。この接続のキーを Keychain または環境変数で設定してください。",
        provider.id
    ))
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
    if let Some(index) = implied_main_account(
        &views,
        provider_main.as_deref(),
        provider_uses_external_credential(provider),
    ) {
        views[index].main = true;
    }
    Ok(views)
}

/// With no account marked main, the one the provider credential names is main. A
/// lone account is main too, unless the connection pays with a credential of its
/// own: then that account is only a stored login no request uses.
fn implied_main_account(
    views: &[ProviderOAuthAccountView],
    provider_main: Option<&str>,
    provider_external: bool,
) -> Option<usize> {
    if views.iter().any(|view| view.main) {
        return None;
    }
    views
        .iter()
        .position(|view| provider_main == view.store_key.as_deref())
        .or_else(|| (views.len() == 1 && !provider_external).then_some(0))
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
    super::cli_consent::require_cli_login_reuse(&app).await?;
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
    if let Some(provider) = settings
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
    {
        refuse_cli_login_for_connection(provider)?;
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
    let removed = remove_account_from_settings(
        &mut settings,
        &provider_id,
        &account_id,
        list_stored_oauth_accounts(&provider_id),
    )?;
    settings.validate()?;
    persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
    // Delete the stored token only after the settings change is durable. A failed
    // delete leaves a session the settings no longer use, and the caller is told.
    if let Some(store_key) = removed.unused_store_key {
        remove_stored_oauth_account(&store_key)?;
    }
    Ok(())
}

struct RemovedAccount {
    /// The session to delete once the settings are saved. None if another account
    /// or connection still uses it.
    unused_store_key: Option<String>,
}

fn names_same_login(left: &ProviderCredential, right: &ProviderCredential) -> bool {
    left.source == right.source
        && match (left.reference.as_deref(), right.reference.as_deref()) {
            (Some(left), Some(right)) => codetas_gateway::same_oauth_login(left, right),
            (None, None) => true,
            _ => false,
        }
}

/// The connection that still uses this login, if any.
fn connection_using_login<'a>(
    settings: &'a GatewaySettings,
    credential: &ProviderCredential,
) -> Option<&'a str> {
    settings
        .account_pool
        .accounts
        .iter()
        .find(|account| names_same_login(&account.credential, credential))
        .map(|account| account.provider_id.as_str())
        .or_else(|| {
            settings
                .providers
                .iter()
                .find(|provider| {
                    provider.credential.command.is_none()
                        && names_same_login(&provider.credential, credential)
                })
                .map(|provider| provider.id.as_str())
        })
}

/// A row this connection has no pool entry for is a stored login, listed under every
/// connection that shares its canonical provider. Err means nothing changed.
fn check_stored_login_row(
    settings: &GatewaySettings,
    provider_id: &str,
    account_id: &str,
    sessions: &[codetas_gateway::OAuthStoredAccount],
) -> Result<String, String> {
    let store_key = sessions
        .iter()
        .map(|session| session.store_key.as_str())
        .find(|key| oauth_account_id_for_store_key(settings, provider_id, key) == account_id)
        .ok_or_else(|| {
            "このアカウントは見つからないので、何も削除していません。すでに削除された可能性があります。"
                .to_string()
        })?;
    let login = oauth_session_credential(store_key);
    let this_connection_uses_it = settings
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .is_some_and(|provider| names_same_login(&provider.credential, &login));
    if !this_connection_uses_it {
        if let Some(connection) = connection_using_login(settings, &login) {
            return Err(format!(
                "{store_key} のログインは {connection} がまだ使っているので、削除していません。"
            ));
        }
    }
    Ok(store_key.to_string())
}

/// Err means nothing was removed, and says why.
fn remove_account_from_settings(
    settings: &mut GatewaySettings,
    provider_id: &str,
    account_id: &str,
    sessions: Result<Vec<codetas_gateway::OAuthStoredAccount>, String>,
) -> Result<RemovedAccount, String> {
    let sessions = sessions?;
    // Never materialize new pool accounts for a provider that is currently on
    // an API key or another non-OAuth credential: pool accounts take routing
    // precedence over the provider credential.
    let provider_external = settings
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .is_some_and(provider_uses_external_credential);
    let has_pool_entry = settings
        .account_pool
        .accounts
        .iter()
        .any(|account| account.provider_id == provider_id && account.id == account_id);
    if !has_pool_entry {
        let store_key = check_stored_login_row(settings, provider_id, account_id, &sessions)?;
        if provider_external {
            if session_key_provider(&store_key) == store_key {
                dismiss_cli_import(&mut settings.account_pool, provider_id);
            }
            return Ok(RemovedAccount {
                unused_store_key: Some(store_key),
            });
        }
    }
    if !provider_external {
        materialize_oauth_accounts(settings, provider_id, &sessions);
    }
    let index = settings
        .account_pool
        .accounts
        .iter()
        .position(|account| account.provider_id == provider_id && account.id == account_id)
        .ok_or_else(|| format!("アカウントが見つかりません: {account_id}"))?;
    let removed = settings.account_pool.accounts.remove(index);
    let was_main = settings
        .account_pool
        .active_accounts
        .get(provider_id)
        .is_some_and(|active| active == account_id);
    if was_main {
        settings.account_pool.active_accounts.remove(provider_id);
    }
    // Compared canonically: a connection saved under an alias such as `kimi-code`
    // holds that alias, while its account holds the store key `kimi`.
    let provider_points_at_removed = settings
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .is_some_and(|provider| names_same_login(&provider.credential, &removed.credential));
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
    let mut connection_cleared = false;
    if let Some(replacement) = replacement {
        if adopt_replacement {
            set_primary_account(settings, provider_id, &replacement)?;
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
        connection_cleared = true;
    }
    let in_use = connection_using_login(settings, &removed.credential).is_some();
    // The CLI is still signed in, so startup adoption would undo this: it would turn
    // the cleared connection back on, or bring back the base login being deleted.
    let base_login = removed
        .credential
        .reference
        .as_deref()
        .filter(|reference| session_key_provider(reference) == *reference)
        .filter(|_| removed.credential.source == CredentialSource::OAuth);
    if !in_use {
        if connection_cleared {
            dismiss_cli_import(&mut settings.account_pool, provider_id);
        }
        if let Some(base_login) = base_login {
            dismiss_cli_import(&mut settings.account_pool, base_login);
        }
    }
    let unused_store_key = removed.credential.reference.clone().filter(|_| !in_use);
    Ok(RemovedAccount { unused_store_key })
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

    fn stored_login(store_key: &str) -> OAuthStoredAccount {
        OAuthStoredAccount {
            store_key: store_key.into(),
            account_id: None,
            email: None,
            expires_at_ms: 1,
            source: "local-cli".into(),
        }
    }

    fn connection(id: &str, credential: ProviderCredential) -> ProviderDefinition {
        let mut provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == id)
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.enabled = true;
        provider.api_key_env = None;
        provider.credential = credential;
        provider
    }

    #[test]
    fn removing_an_alias_connections_login_dismisses_it_and_clears_the_credential() {
        let mut settings = GatewaySettings {
            providers: vec![connection(
                "kimi-code",
                oauth_session_credential("kimi-code"),
            )],
            ..GatewaySettings::default()
        };
        let sessions = vec![stored_login("kimi")];
        let account_id = oauth_account_id_for_store_key(&settings, "kimi-code", "kimi");
        let removed = removal_ok(&mut settings, "kimi-code", &account_id, sessions);
        assert_eq!(removed.unused_store_key.as_deref(), Some("kimi"));
        assert_eq!(settings.account_pool.dismissed_cli_imports, ["kimi"]);
        let provider = &settings.providers[0];
        assert_eq!(provider.credential.source, CredentialSource::None);
        assert!(!provider.enabled);
    }

    fn removal_ok(
        settings: &mut GatewaySettings,
        provider_id: &str,
        account_id: &str,
        sessions: Vec<OAuthStoredAccount>,
    ) -> RemovedAccount {
        remove_account_from_settings(settings, provider_id, account_id, Ok(sessions))
            .unwrap_or_else(|error| panic!("removal failed: {error}"))
    }

    /// The error, after checking that a refused removal changed nothing.
    fn removal_refused(
        settings: &mut GatewaySettings,
        provider_id: &str,
        account_id: &str,
        sessions: Result<Vec<OAuthStoredAccount>, String>,
    ) -> String {
        let before = serde_json::to_value(&*settings).unwrap();
        let error = remove_account_from_settings(settings, provider_id, account_id, sessions)
            .err()
            .expect("the removal should have been refused");
        assert_eq!(serde_json::to_value(&*settings).unwrap(), before);
        error
    }

    fn keychain_credential() -> ProviderCredential {
        ProviderCredential {
            source: CredentialSource::Keychain,
            reference: Some("api-key/own".into()),
            ..ProviderCredential::default()
        }
    }

    fn pool_account(provider_id: &str, store_key: &str) -> AccountReference {
        AccountReference {
            id: format!("pool-{store_key}"),
            provider_id: provider_id.into(),
            label: store_key.into(),
            credential: oauth_session_credential(store_key),
            enabled: true,
            priority: 0,
            paused: false,
            pause_until_unix: None,
            pinned: false,
        }
    }

    #[test]
    fn a_stored_login_another_connection_uses_is_not_deleted_from_this_card() {
        // `kimi` signs in. `kimi-code` has its own key, yet lists the same login.
        let mut settings = GatewaySettings {
            providers: vec![
                connection("kimi", oauth_session_credential("kimi")),
                connection("kimi-code", keychain_credential()),
            ],
            ..GatewaySettings::default()
        };
        let account_id = oauth_account_id_for_store_key(&settings, "kimi-code", "kimi");
        let error = removal_refused(
            &mut settings,
            "kimi-code",
            &account_id,
            Ok(vec![stored_login("kimi")]),
        );
        assert!(error.contains("kimi がまだ使っている"), "{error}");
    }

    #[test]
    fn a_stored_account_another_connections_pool_uses_is_not_deleted_from_this_card() {
        let mut settings = GatewaySettings {
            providers: vec![
                connection("kimi", oauth_session_credential("kimi#b")),
                connection("kimi-code", keychain_credential()),
            ],
            ..GatewaySettings::default()
        };
        settings
            .account_pool
            .accounts
            .push(pool_account("kimi", "kimi#b"));
        let account_id = oauth_account_id_for_store_key(&settings, "kimi-code", "kimi#b");
        let error = removal_refused(
            &mut settings,
            "kimi-code",
            &account_id,
            Ok(vec![stored_login("kimi#b")]),
        );
        assert!(error.contains("kimi がまだ使っている"), "{error}");
    }

    #[test]
    fn a_second_removal_from_a_cleared_connection_does_not_report_a_removal() {
        // After the first removal `kimi-code` has no credential and the row is still
        // listed, because `kimi` keeps the login.
        let mut settings = GatewaySettings {
            providers: vec![
                connection("kimi-code", ProviderCredential::default()),
                connection("kimi", oauth_session_credential("kimi")),
            ],
            ..GatewaySettings::default()
        };
        let account_id = oauth_account_id_for_store_key(&settings, "kimi-code", "kimi");
        let error = removal_refused(
            &mut settings,
            "kimi-code",
            &account_id,
            Ok(vec![stored_login("kimi")]),
        );
        assert!(error.contains("kimi がまだ使っている"), "{error}");
    }

    #[test]
    fn removing_a_base_login_from_an_unwired_connection_keeps_it_removed() {
        let unset = ProviderCredential {
            source: CredentialSource::Environment,
            reference: Some("CODETAS_TEST_UNSET_MISTRAL_KEY".into()),
            ..ProviderCredential::default()
        };
        let mut settings = GatewaySettings {
            providers: vec![connection("mistral", unset)],
            ..GatewaySettings::default()
        };
        let account_id = oauth_account_id_for_store_key(&settings, "mistral", "mistral");
        let removed = removal_ok(
            &mut settings,
            "mistral",
            &account_id,
            vec![stored_login("mistral")],
        );
        assert_eq!(removed.unused_store_key.as_deref(), Some("mistral"));
        assert_eq!(settings.account_pool.dismissed_cli_imports, ["mistral"]);
        assert_eq!(
            settings.providers[0].credential.source,
            CredentialSource::Environment
        );
    }

    #[test]
    fn removing_the_last_pool_login_of_a_connection_on_its_own_key_keeps_it_removed() {
        let mut settings = GatewaySettings {
            providers: vec![connection("anthropic", keychain_credential())],
            ..GatewaySettings::default()
        };
        settings
            .account_pool
            .accounts
            .push(pool_account("anthropic", "anthropic"));
        let removed = removal_ok(
            &mut settings,
            "anthropic",
            "pool-anthropic",
            vec![stored_login("anthropic")],
        );
        assert_eq!(removed.unused_store_key.as_deref(), Some("anthropic"));
        assert_eq!(settings.account_pool.dismissed_cli_imports, ["anthropic"]);
        assert_eq!(
            settings.providers[0].credential.source,
            CredentialSource::Keychain
        );
    }

    #[test]
    fn removing_a_base_login_with_accounts_left_still_keeps_it_removed() {
        let mut settings = GatewaySettings {
            providers: vec![connection(
                "anthropic",
                oauth_session_credential("anthropic#b"),
            )],
            ..GatewaySettings::default()
        };
        settings.account_pool.accounts = vec![
            pool_account("anthropic", "anthropic"),
            pool_account("anthropic", "anthropic#b"),
        ];
        let removed = removal_ok(
            &mut settings,
            "anthropic",
            "pool-anthropic",
            vec![stored_login("anthropic"), stored_login("anthropic#b")],
        );
        assert_eq!(removed.unused_store_key.as_deref(), Some("anthropic"));
        assert_eq!(settings.account_pool.dismissed_cli_imports, ["anthropic"]);
    }

    #[test]
    fn removing_an_account_key_dismisses_only_when_it_clears_the_connection() {
        // Adoption cannot bring this key back, and the connection keeps its own key.
        let mut on_own_key = GatewaySettings {
            providers: vec![connection("anthropic", keychain_credential())],
            ..GatewaySettings::default()
        };
        on_own_key
            .account_pool
            .accounts
            .push(pool_account("anthropic", "anthropic#b"));
        let sessions = vec![stored_login("anthropic#b")];
        let removed = removal_ok(
            &mut on_own_key,
            "anthropic",
            "pool-anthropic#b",
            sessions.clone(),
        );
        assert_eq!(removed.unused_store_key.as_deref(), Some("anthropic#b"));
        assert!(on_own_key.account_pool.dismissed_cli_imports.is_empty());

        // Cleared, it would be turned back on by whatever the CLI is signed in to.
        let mut wired = GatewaySettings {
            providers: vec![connection(
                "anthropic",
                oauth_session_credential("anthropic#b"),
            )],
            ..GatewaySettings::default()
        };
        wired
            .account_pool
            .accounts
            .push(pool_account("anthropic", "anthropic#b"));
        removal_ok(&mut wired, "anthropic", "pool-anthropic#b", sessions);
        assert_eq!(wired.account_pool.dismissed_cli_imports, ["anthropic"]);
    }

    #[test]
    fn an_unreadable_auth_store_is_an_error_and_not_an_empty_list() {
        let mut settings = GatewaySettings {
            providers: vec![connection("kimi", oauth_session_credential("kimi"))],
            ..GatewaySettings::default()
        };
        let error = removal_refused(
            &mut settings,
            "kimi",
            "kimi-default",
            Err("CODETAS auth store could not be parsed".into()),
        );
        assert!(error.contains("could not be parsed"), "{error}");
    }

    #[test]
    fn removal_keeps_a_login_another_connection_still_uses() {
        let mut settings = GatewaySettings {
            providers: vec![
                connection("kimi-code", oauth_session_credential("kimi-code")),
                connection("kimi", oauth_session_credential("kimi")),
            ],
            ..GatewaySettings::default()
        };
        let sessions = vec![stored_login("kimi")];
        let account_id = oauth_account_id_for_store_key(&settings, "kimi-code", "kimi");
        let removed = removal_ok(&mut settings, "kimi-code", &account_id, sessions);
        assert_eq!(removed.unused_store_key, None);
        // Nothing was deleted, so there is nothing for adoption to bring back.
        assert!(settings.account_pool.dismissed_cli_imports.is_empty());
        assert_eq!(
            settings.providers[0].credential.source,
            CredentialSource::None
        );
        assert_eq!(
            settings.providers[1].credential.source,
            CredentialSource::OAuth
        );
    }

    #[test]
    fn removal_really_removes_a_stored_login_under_a_connections_own_key() {
        let keychain = ProviderCredential {
            source: CredentialSource::Keychain,
            reference: Some("api-key/mistral".into()),
            ..ProviderCredential::default()
        };
        let mut settings = GatewaySettings {
            providers: vec![connection("mistral", keychain)],
            ..GatewaySettings::default()
        };
        let sessions = vec![stored_login("mistral")];
        let account_id = oauth_account_id_for_store_key(&settings, "mistral", "mistral");
        let removed = removal_ok(&mut settings, "mistral", &account_id, sessions.clone());
        assert_eq!(removed.unused_store_key.as_deref(), Some("mistral"));
        assert_eq!(settings.account_pool.dismissed_cli_imports, ["mistral"]);
        // Its own key stays, and it stays on.
        let provider = &settings.providers[0];
        assert_eq!(provider.credential.source, CredentialSource::Keychain);
        assert!(provider.enabled);

        // A row that no longer exists is not reported as removed.
        let error = removal_refused(&mut settings, "mistral", "no-such-account", Ok(sessions));
        assert!(error.contains("何も削除していません"), "{error}");
    }

    #[test]
    fn a_login_no_request_uses_is_not_shown_as_main() {
        let view = ProviderOAuthAccountView {
            account_id: "mistral-default".into(),
            label: "mistral".into(),
            email: None,
            store_key: Some("mistral".into()),
            main: false,
            enabled: true,
            expires_at_ms: 0,
            source: "local-cli".into(),
        };
        assert_eq!(
            implied_main_account(std::slice::from_ref(&view), Some("api-key/mistral"), true),
            None
        );
        assert_eq!(
            implied_main_account(std::slice::from_ref(&view), Some("mistral"), false),
            Some(0)
        );
        assert_eq!(implied_main_account(&[view], None, false), Some(0));
    }

    #[test]
    fn a_proxy_connection_refuses_a_cli_login() {
        let mut provider = connection("mistral", ProviderCredential::default());
        assert!(refuse_cli_login_for_connection(&provider).is_ok());
        provider.base_url = "https://proxy.example/v1".into();
        assert!(refuse_cli_login_for_connection(&provider).is_err());
        provider.id = "kimi".into();
        assert!(refuse_cli_login_for_connection(&provider).is_ok());
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

/// Saves an API key typed into the connection editor to the macOS Keychain and
/// returns the reference the provider credential should use. The key itself is
/// never written to the settings file or returned to the web view.
#[tauri::command]
pub async fn store_provider_api_key(
    provider_id: String,
    api_key: String,
) -> Result<String, String> {
    let reference = format!("api-key/{}", provider_id.trim());
    let stored_reference = reference.clone();
    tokio::task::spawn_blocking(move || {
        codetas_gateway::store_keychain_credential(&stored_reference, &api_key)
    })
    .await
    .map_err(|_| "API key could not be saved".to_string())??;
    Ok(reference)
}
