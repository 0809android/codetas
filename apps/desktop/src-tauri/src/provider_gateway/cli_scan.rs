use super::*;


#[tauri::command]
pub fn scan_local_cli_clients(app: AppHandle, deep: Option<bool>) -> LocalCliScanReport {
    let registered = load_settings(&app)
        .map(|settings| {
            settings
                .providers
                .iter()
                .filter(|provider| provider.enabled && provider_has_usable_credential(provider))
                .map(|provider| provider.id.clone())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    scan_local_cli_candidates(deep.unwrap_or(false), &registered)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalCliRegistrationReport {
    provider_id: String,
    enabled: bool,
    credential_needed: bool,
    message: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectApiTarget {
    provider_id: String,
    name: String,
    hint: String,
}

#[tauri::command]
pub fn list_direct_api_targets() -> Vec<DirectApiTarget> {
    vec![
        DirectApiTarget {
            provider_id: "deepseek".into(),
            name: "DeepSeek API".into(),
            hint: "DeepSeek 公式APIキーを DEEPSEEK_API_KEY または Keychain 参照で保存します。".into(),
        },
        DirectApiTarget {
            provider_id: "zai".into(),
            name: "GLM (Z.AI)".into(),
            hint: "Z.AI Coding Plan の ZAI_API_KEY を Keychain または環境変数で保存します。".into(),
        },
        DirectApiTarget {
            provider_id: "minimax".into(),
            name: "MiniMax".into(),
            hint: "MiniMax Coding Plan の MINIMAX_API_KEY を Keychain または環境変数で保存します。".into(),
        },
    ]
}

#[tauri::command]
pub async fn register_codetas_provider(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    provider_id: String,
) -> Result<LocalCliRegistrationReport, String> {
    register_provider_in_codetas(app, manager, &provider_id).await
}

#[tauri::command]
pub async fn register_local_cli_in_codetas(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    client_id: String,
) -> Result<LocalCliRegistrationReport, String> {
    let candidate = LOCAL_CLI_CANDIDATES
        .iter()
        .find(|item| item.id == client_id)
        .ok_or_else(|| format!("未対応のローカルCLIです: {client_id}"))?;
    let provider_id = candidate
        .codetas_provider_id
        .ok_or("このCLIにはCODETAS再登録先がありません")?;
    register_provider_in_codetas(app, manager, provider_id).await
}

pub(crate) async fn register_provider_in_codetas(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    provider_id: &str,
) -> Result<LocalCliRegistrationReport, String> {
    let previous = load_settings(&app)?;
    let mut settings = previous.clone();
    if settings
        .providers
        .iter()
        .all(|provider| provider.id != provider_id)
    {
        let provider = provider_presets()
            .into_iter()
            .find(|preset| preset.id == provider_id)
            .ok_or_else(|| format!("CODETAS再登録先のpresetがありません: {provider_id}"))?
            .instantiate(None)?;
        settings.providers.push(provider);
    }
    configure_desktop_auth_store(&app)?;
    let needs_native_login = {
        let provider = settings
            .providers
            .iter_mut()
            .find(|provider| provider.id == provider_id)
            .ok_or_else(|| format!("CODETAS再登録先がありません: {provider_id}"))?;
        provider.enabled = true;
        if !provider_has_usable_credential(provider)
            && (detect_local_cli_session_for(provider_id) || has_stored_session(provider_id))
        {
            provider.api_key_env = None;
            provider.credential = oauth_session_credential(provider_id);
        }
        !provider_has_usable_credential(provider) && provider_supports_native_oauth(provider_id)
    };
    if needs_native_login {
        let login = login_provider_oauth(provider_id).await?;
        let provider = settings
            .providers
            .iter_mut()
            .find(|provider| provider.id == provider_id)
            .ok_or_else(|| format!("CODETAS再登録先がありません: {provider_id}"))?;
        provider.api_key_env = None;
        provider.credential = oauth_session_credential(provider_id);
        provider.enabled = true;
        settings.validate()?;
        persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
        return Ok(LocalCliRegistrationReport {
            provider_id: provider_id.to_string(),
            enabled: true,
            credential_needed: false,
            message: login.message,
        });
    }
    let credential_needed = settings
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .is_none_or(|provider| !provider_has_usable_credential(provider));
    settings.validate()?;
    persist_and_apply_settings(&app, &manager, &previous, &settings).await?;
    Ok(LocalCliRegistrationReport {
        provider_id: provider_id.to_string(),
        enabled: true,
        credential_needed,
        message: if credential_needed {
            format!(
                "{provider_id} をCODETASに再登録しました。{}",
                provider_registration_hint(provider_id)
            )
        } else {
            format!("{provider_id} を有効にしました。既存のログインを使います。")
        },
    })
}

pub(crate) fn detect_local_cli_session_for(provider_id: &str) -> bool {
    dirs::home_dir().is_some_and(|home| detect_local_cli_session(provider_id, &home).is_some())
}

pub(crate) fn provider_has_usable_credential(provider: &ProviderDefinition) -> bool {
    match provider.credential.source {
        CredentialSource::None => false,
        CredentialSource::Environment => provider
            .credential
            .reference
            .as_deref()
            .or(provider.api_key_env.as_deref())
            .is_some_and(|name| env::var_os(name).is_some_and(|value| !value.is_empty())),
        CredentialSource::OAuth if provider.credential.command.is_none() => provider
            .credential
            .reference
            .as_deref()
            .is_some_and(|reference| {
                has_stored_session(reference) || detect_local_cli_session_for(reference)
            }),
        CredentialSource::Keychain | CredentialSource::OAuth | CredentialSource::Command => provider
            .credential
            .reference
            .as_deref()
            .or_else(|| {
                provider
                    .credential
                    .command
                    .as_ref()
                    .map(|command| command.program.as_str())
            })
            .is_some_and(|value| !value.trim().is_empty()),
        CredentialSource::Forward => true,
    }
}

#[tauri::command]
pub fn list_provider_presets() -> Vec<ProviderPreset> {
    provider_presets()
}

#[tauri::command]

pub(crate) fn scan_local_cli_candidates(deep: bool, registered: &BTreeSet<String>) -> LocalCliScanReport {
    let clients = LOCAL_CLI_CANDIDATES
        .iter()
        .map(|candidate| scan_local_cli_candidate(*candidate, deep, registered))
        .collect();
    LocalCliScanReport {
        checked_at_ms: now_ms(),
        deep,
        clients,
    }
}

pub(crate) fn scan_local_cli_candidate(
    candidate: LocalCliCandidate,
    deep: bool,
    registered: &BTreeSet<String>,
) -> LocalCliStatus {
    let already_registered = candidate
        .codetas_provider_id
        .is_some_and(|provider_id| registered.contains(provider_id));
    let Some(executable) = find_cli_executable(candidate.executable) else {
        return LocalCliStatus {
            id: candidate.id.into(),
            name: candidate.name.into(),
            installed: false,
            executable: None,
            version: None,
            probe_state: "notInstalled".into(),
            message: "CLIが見つかりません".into(),
            can_register: false,
            needs_codetas_registration: false,
            codetas_provider_id: candidate.codetas_provider_id.map(str::to_string),
            registration_hint: local_cli_registration_hint(candidate.id).into(),
        };
    };
    let version = command_output(&executable, &["--version"], Duration::from_secs(3))
        .filter(|output| output.success)
        .and_then(|output| {
            output
                .text
                .lines()
                .next()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
        });
    let (probe_state, message) = if deep {
        deep_probe_local_cli(candidate.id, &executable)
    } else {
        (
            "installed".into(),
            "インストール済み。登録確認で認証・quotaを検証できます".into(),
        )
    };
    let ready = probe_state == "ready";
    LocalCliStatus {
        id: candidate.id.into(),
        name: candidate.name.into(),
        installed: true,
        executable: Some(executable.to_string_lossy().into_owned()),
        version,
        probe_state,
        message,
        can_register: ready && candidate.supports_provider,
        needs_codetas_registration: candidate.codetas_provider_id.is_some() && !already_registered,
        codetas_provider_id: candidate.codetas_provider_id.map(str::to_string),
        registration_hint: local_cli_registration_hint(candidate.id).into(),
    }
}

pub(crate) fn provider_registration_hint(provider_id: &str) -> &'static str {
    match provider_id {
        "anthropic" | "anthropic-apikey" => {
            "Claude の既存ログインがあれば取り込みます。なければブラウザでログインします。"
        }
        "deepseek" => "DEEPSEEK_API_KEY を Keychain または環境変数で保存します。",
        "zai" => "ZAI_API_KEY を Keychain または環境変数で保存します。",
        "minimax" | "minimax-cn" => {
            "MINIMAX_API_KEY を Keychain または環境変数で保存します。"
        }
        "xai" => "Grok CLI のログインがあれば取り込みます。なければブラウザでログインします。",
        "kimi" => "Kimi CLI のログインがあれば取り込みます。なければブラウザでログインします。",
        _ => "APIキー、既存CLIログイン、またはアプリ内OAuthで接続します。",
    }
}

pub(crate) fn local_cli_registration_hint(id: &str) -> &'static str {
    match id {
        "codex" => "Codexログインはそのまま転送します。別途APIキーは不要です。",
        "claude" => "Claude CLIのログインを取り込みます。未ログインならCODETASでブラウザログインします。",
        "grok" => "Grok CLIのログインを取り込みます。未ログインならCODETASでブラウザログインします。",
        "agy" => "gcloud OAuth、または GOOGLE_ANTIGRAVITY_ACCESS_TOKEN を Keychain 参照で保存します。",
        "qwen" => "Alibaba / DashScope のAPIキーを DASHSCOPE_API_KEY または Keychain 参照で保存します。",
        "kimi" => "Kimi CLIのログインを取り込みます。未ログインならCODETASでブラウザログインします。",
        "opencode" => "OpenCode Go のAPIキーを OPENCODE_API_KEY または Keychain 参照で保存します。",
        "gemini" => "Google AI Studio のAPIキーを GEMINI_API_KEY または Keychain 参照で保存します。",
        "kiro" => "Kiro のアクセストークンを KIRO_ACCESS_TOKEN または Keychain 参照で保存します。",
        _ => "APIキーまたは既存のCLIログインで接続します。",
    }
}

pub(crate) fn deep_probe_local_cli(id: &str, executable: &Path) -> (String, String) {
    let args: &[&str] = match id {
        "codex" => &["login", "status"],
        "claude" => &["auth", "status", "--text"],
        "agy" => &["models"],
        "kimi" => &["provider", "list"],
        "opencode" => &["providers", "list"],
        "grok" | "qwen" | "gemini" | "kiro" => {
            return (
                "installed".into(),
                format!(
                    "{}を検出しました。CODETASに取り込むか、ログインしてください",
                    match id {
                        "grok" => "Grok CLI",
                        "qwen" => "Qwen Code",
                        "gemini" => "Gemini CLI",
                        "kiro" => "Kiro CLI",
                        _ => "CLI",
                    }
                ),
            );
        }
        _ => return ("unsupported".into(), "安全な非対話probeが未定義です".into()),
    };
    let timeout = Duration::from_secs(15);
    let Some(output) = command_output(executable, args, timeout) else {
        return (
            "unavailable".into(),
            "非対話probeに失敗またはタイムアウトしました".into(),
        );
    };
    classify_local_cli_probe(id, output.success, &output.text)
}

pub(crate) fn classify_local_cli_probe(id: &str, success: bool, output: &str) -> (String, String) {
    let normalized = output.to_ascii_lowercase();
    if normalized.contains("not logged in")
        || normalized.contains("unauthorized")
        || (!success && normalized.contains("login"))
    {
        return (
            "authenticationRequired".into(),
            "headless実行の認証が必要です".into(),
        );
    }
    let codex_logged_in = id == "codex"
        && success
        && output.lines().any(|line| {
            let line = line.trim().to_ascii_lowercase();
            line.starts_with("logged in") && !line.starts_with("not logged in")
        });
    if success && (normalized.contains("codetas_cli_ok") || codex_logged_in) {
        return ("ready".into(), "非対話probeに成功しました".into());
    }
    if success && id == "agy" && output.lines().any(|line| line.contains('\t')) {
        return (
            "authenticated".into(),
            "Antigravity CLIはログイン済みです。CODETASが取り込みます".into(),
        );
    }
    if success && id == "kimi" && kimi_provider_list_is_authenticated(output) {
        return (
            "authenticated".into(),
            "Kimi CLIはログイン済みです。CODETASが取り込みます".into(),
        );
    }
    if success && id == "claude" && claude_auth_status_is_authenticated(output) {
        return (
            "authenticated".into(),
            "Claude CLIはログイン済みです。CODETASが取り込みます".into(),
        );
    }
    if success && id == "opencode" && output.lines().any(|line| !line.trim().is_empty()) {
        return (
            "authenticated".into(),
            "OpenCode CLIはログイン済みです。CODETASが取り込みます".into(),
        );
    }
    if normalized.contains("quota") || normalized.contains("insufficient_quota") {
        return (
            "quotaExhausted".into(),
            "認証済みですがquotaが枯渇しています".into(),
        );
    }

    (
        "unavailable".into(),
        "非対話probeは応答しましたが利用可能性を確認できませんでした".into(),
    )
}

pub(crate) fn claude_auth_status_is_authenticated(output: &str) -> bool {
    let normalized = output.to_ascii_lowercase();
    !normalized.contains("not logged in")
        && (normalized.contains("logged in")
            || normalized.contains("oauth")
            || normalized.contains("api key")
            || normalized.contains("anthropic"))
}

pub(crate) fn kimi_provider_list_is_authenticated(output: &str) -> bool {
    output.lines().any(|line| {
        let line = line.to_ascii_lowercase();
        (line.contains("source=oauth") || line.contains("source=api_key") || line.contains("type=kimi"))
            && line.contains("models=")
            && !line.contains("models=0")
    })
}

pub(crate) struct BoundedCommandOutput {
    success: bool,
    text: String,
}

pub(crate) fn command_output(
    program: &Path,
    args: &[&str],
    timeout: Duration,
) -> Option<BoundedCommandOutput> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // command-group maps to a POSIX process group on Unix and a Job Object on
    // Windows, so timeout cleanup includes descendants started by the CLI.
    let mut child = command.group_spawn().ok()?;
    let stdout = child.inner().stdout.take()?;
    let stderr = child.inner().stderr.take()?;
    // Drain both pipes while the process is running. read_bounded keeps only a
    // bounded prefix but continues to EOF, preventing pipe-capacity deadlocks.
    let (stdout_tx, stdout_rx) = mpsc::channel();
    let (stderr_tx, stderr_rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = stdout_tx.send(read_bounded(stdout, 32 * 1024));
    });
    thread::spawn(move || {
        let _ = stderr_tx.send(read_bounded(stderr, 32 * 1024));
    });
    let started = Instant::now();
    let mut status = None;
    let mut stdout_bytes = None;
    let mut stderr_bytes = None;
    loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(current)) => {
                    status = Some(current);
                    // The group leader may exit before helpers that inherited
                    // its pipes. Probe commands never own long-lived helpers,
                    // so stop the remaining group/job immediately while
                    // preserving the leader's exit status.
                    let _ = child.kill();
                    let _ = child.wait();
                }
                Ok(None) => {}
                Err(_) => break,
            }
        }
        if stdout_bytes.is_none() {
            if let Ok(output) = stdout_rx.try_recv() {
                stdout_bytes = output;
            }
        }
        if stderr_bytes.is_none() {
            if let Ok(output) = stderr_rx.try_recv() {
                stderr_bytes = output;
            }
        }
        if status.is_some() && stdout_bytes.is_some() && stderr_bytes.is_some() {
            break;
        }
        if started.elapsed() >= timeout {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let completed = status.is_some() && stdout_bytes.is_some() && stderr_bytes.is_some();
    if !completed {
        let _ = child.kill();
        let _ = child.wait();
        // Reader threads are intentionally detached here. Group cleanup closes
        // inherited pipe writers; the readers then finish independently, while
        // this timeout path remains bounded and never waits on an unbounded join.
        return None;
    }
    let success = status?.success();
    let mut bytes = stdout_bytes?;
    let stderr = stderr_bytes?;
    const MAX_OUTPUT_BYTES: usize = 64 * 1024;
    bytes.extend_from_slice(
        &stderr[..stderr
            .len()
            .min(MAX_OUTPUT_BYTES.saturating_sub(bytes.len()))],
    );
    Some(BoundedCommandOutput {
        success,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

pub(crate) fn read_bounded(mut reader: impl Read, limit: usize) -> Option<Vec<u8>> {
    let mut kept = Vec::with_capacity(limit);
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer).ok()?;
        if read == 0 {
            return Some(kept);
        }
        let remaining = limit.saturating_sub(kept.len());
        kept.extend_from_slice(&buffer[..read.min(remaining)]);
    }
}
