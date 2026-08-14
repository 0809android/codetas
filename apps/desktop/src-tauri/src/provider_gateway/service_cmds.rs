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
            "google" | "google-vertex" | "google-antigravity" => "gcloud",
            "anthropic" => "claude-cli",
            _ => "",
        }
    });
    configure_desktop_auth_store(&app)?;
    if provider_supports_native_oauth(&provider_id) || broker == "claude-cli" {
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

pub(crate) fn launch_claude_setup_token_terminal(app: &AppHandle, program: &Path) -> Result<(), String> {
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
    fs::write(&script, body).map_err(|error| format!("OAuth起動スクリプトを書けません: {error}"))?;
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
pub fn start_gateway_service() -> Result<ServiceStatus, String> {
    if !service::start()? {
        return Err("常駐Gatewayの起動状態を確認できません".into());
    }
    service::status()
}

#[tauri::command]
pub fn restart_gateway_service() -> Result<ServiceStatus, String> {
    if !service::restart()? {
        return Err("常駐Gatewayの再起動状態を確認できません".into());
    }
    service::status()
}

#[tauri::command]
pub fn stop_gateway_service() -> Result<ServiceStatus, String> {
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
