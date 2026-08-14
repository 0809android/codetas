use super::*;

pub(crate) fn codex_config_path() -> Result<PathBuf, String> {
    Ok(codex_home()?.join("config.toml"))
}

pub(crate) fn codex_journal_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|directory| directory.join("codex-install-journal.json"))
        .map_err(|error| format!("CODETAS設定フォルダを特定できません: {error}"))
}

pub(crate) fn read_codex_journal(path: &Path) -> Result<Option<CodexInstallJournal>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Codex復元情報を確認できません: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Codex復元情報は通常ファイルではないため使用しません".into());
    }
    if metadata.len() > 1024 * 1024 {
        return Err("Codex復元情報が大きすぎます".into());
    }
    let bytes = fs::read(path).map_err(|error| format!("Codex復元情報を読めません: {error}"))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("Codex復元情報を解析できません: {error}"))
}

pub(crate) fn validate_codex_journal_paths(
    app: &AppHandle,
    journal: &CodexInstallJournal,
) -> Result<(PathBuf, PathBuf, Option<PathBuf>, Option<PathBuf>), String> {
    let journal_path = codex_journal_path(app)?;
    let validated = validate_codex_journal_paths_from_journal_path(&journal_path, journal)?;
    if validated.0 != codex_config_path()?
        || validated.1 != codex_home()?.join("codetas-model-catalog.json")
    {
        return Err("Codex復元情報が現在のCODEX_HOME外を参照しています".into());
    }
    Ok(validated)
}

pub(crate) fn validate_codex_journal_paths_from_journal_path(
    journal_path: &Path,
    journal: &CodexInstallJournal,
) -> Result<(PathBuf, PathBuf, Option<PathBuf>, Option<PathBuf>), String> {
    if journal.version != CODEX_JOURNAL_VERSION {
        return Err("unsupported Codex restore journal version".into());
    }
    let config_path = PathBuf::from(&journal.config_path);
    let catalog_path = PathBuf::from(&journal.catalog_path);
    let config_home = config_path
        .parent()
        .ok_or("Codex復元情報の設定パスに親フォルダがありません")?;
    if config_path.file_name().and_then(|name| name.to_str()) != Some("config.toml")
        || catalog_path != config_home.join("codetas-model-catalog.json")
    {
        return Err("Codex復元情報の設定・カタログパスの組み合わせが不正です".into());
    }
    let backup_path = journal.backup_path.as_deref().map(PathBuf::from);
    if let Some(backup) = backup_path.as_deref() {
        let expected_parent = journal_path
            .parent()
            .ok_or("CODETAS復元フォルダを特定できません")?;
        let valid_name = backup
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with("codex-config-before-") && name.ends_with(".toml")
            });
        if backup.parent() != Some(expected_parent) || !valid_name {
            return Err("Codex復元情報のバックアップパスが所有境界外です".into());
        }
    }
    let catalog_backup_path = journal.catalog_backup_path.as_deref().map(PathBuf::from);
    if let Some(backup) = catalog_backup_path.as_deref() {
        let expected_parent = journal_path
            .parent()
            .ok_or("CODETAS復元フォルダを特定できません")?;
        let valid_name = backup
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with("codex-catalog-before-") && name.ends_with(".json")
            });
        if backup.parent() != Some(expected_parent) || !valid_name {
            return Err("Codex復元情報のカタログバックアップが所有境界外です".into());
        }
    }
    match (journal.catalog_existed, catalog_backup_path.is_some()) {
        (Some(true), true) | (Some(false), false) | (None, _) => {}
        _ => return Err("Codex復元情報のカタログ存在状態が矛盾しています".into()),
    }
    Ok((config_path, catalog_path, backup_path, catalog_backup_path))
}

pub(crate) fn prepare_codex_journal(
    app: &AppHandle,
    config_path: &Path,
    catalog_path: &Path,
    model: &str,
    base_url: &str,
    local_token: bool,
    routing_mode: CodexRoutingMode,
    installed_agents: Option<CodexAgentInstall>,
) -> Result<Vec<PathBuf>, String> {
    let journal_path = codex_journal_path(app)?;
    let existing = match read_codex_journal(&journal_path)? {
        Some(journal) => {
            validate_codex_journal_paths(app, &journal)?;
            Some(journal)
        }
        None => None,
    };
    let mut created_backups = Vec::new();
    let backup_path = if let Some(existing) = existing.as_ref() {
        if Path::new(&existing.config_path) != config_path {
            return Err("既存のCODETAS復元情報が別のCodex設定を参照しています".into());
        }
        existing.backup_path.clone()
    } else {
        create_codex_backup(
            &journal_path,
            config_path,
            "codex-config-before",
            "toml",
            &mut created_backups,
        )?
    };
    let (catalog_backup_path, catalog_existed) = if let Some(existing) = existing.as_ref() {
        (
            existing.catalog_backup_path.clone(),
            existing.catalog_existed,
        )
    } else {
        match create_codex_backup(
            &journal_path,
            catalog_path,
            "codex-catalog-before",
            "json",
            &mut created_backups,
        ) {
            Ok(path) => {
                let existed = path.is_some();
                (path, Some(existed))
            }
            Err(error) => {
                for backup in &created_backups {
                    let _ = fs::remove_file(backup);
                }
                return Err(error);
            }
        }
    };
    let journal = CodexInstallJournal {
        version: CODEX_JOURNAL_VERSION,
        config_path: config_path.to_string_lossy().into_owned(),
        backup_path,
        catalog_path: catalog_path.to_string_lossy().into_owned(),
        catalog_backup_path,
        catalog_existed,
        installed_model: model.to_string(),
        installed_base_url: base_url.to_string(),
        routing_mode,
        installed_local_token: local_token,
        installed_agents,
    };
    let content = serde_json::to_vec_pretty(&journal)
        .map_err(|error| format!("CODETAS復元情報を生成できません: {error}"))?;
    if let Err(error) = atomic_write(&journal_path, &content) {
        for backup in &created_backups {
            let _ = fs::remove_file(backup);
        }
        return Err(error);
    }
    Ok(created_backups)
}

pub(crate) fn enable_codex_openai_passthrough(
    settings: &mut GatewaySettings,
) -> Result<(), String> {
    if let Some(provider) = settings
        .providers
        .iter_mut()
        .find(|provider| provider.id == "openai")
    {
        provider.enabled = true;
        return settings.validate();
    }

    let mut provider = provider_presets()
        .into_iter()
        .find(|preset| preset.id == "openai")
        .ok_or("OpenAI Codexログインのpassthrough presetがありません")?
        .instantiate(None)?;
    provider.enabled = true;
    settings.providers.push(provider);
    settings.validate()
}

pub(crate) fn auto_register_authenticated_cli_providers(
    settings: &mut GatewaySettings,
) -> Result<(), String> {
    // Codex Desktop authentication is forwarded by the native `openai` provider;
    // no token is extracted or copied from the Codex CLI.
    if let Some(codex) = find_cli_executable("codex") {
        let _ = command_succeeds(&codex, &["login", "status"], Duration::from_secs(5));
    }

    // GitHub CLI has a documented token-output command. Store only the absolute
    // executable path and invoke it on demand; the token never enters settings.
    if settings
        .providers
        .iter()
        .all(|provider| provider.id != "github-models")
    {
        if let Some(gh) = find_cli_executable("gh").filter(|path| {
            command_succeeds(
                path,
                &["auth", "status", "--hostname", "github.com"],
                Duration::from_secs(5),
            )
        }) {
            register_github_cli_provider(settings, &gh)?;
        }
    }
    if auth_store_is_configured() {
        adopt_local_cli_sessions(settings)?;
    }
    settings.validate()
}

pub(crate) fn configure_desktop_auth_store(app: &AppHandle) -> Result<(), String> {
    configure_auth_store_path(Some(auth_store_file_path(app)?));
    Ok(())
}

pub(crate) fn auth_store_file_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(settings_path(app)?.with_file_name("auth.json"))
}

pub(crate) fn register_github_cli_provider(
    settings: &mut GatewaySettings,
    gh: &Path,
) -> Result<(), String> {
    if !gh.is_absolute() {
        return Err("GitHub CLI command must use an absolute path".into());
    }
    if settings
        .providers
        .iter()
        .any(|provider| provider.id == "github-models")
    {
        return Ok(());
    }
    let mut provider = provider_presets()
        .into_iter()
        .find(|preset| preset.id == "github-models")
        .ok_or("GitHub Models presetがありません")?
        .instantiate(None)?;
    provider.credential = ProviderCredential {
        source: CredentialSource::Command,
        reference: None,
        transport: CredentialTransport::Bearer,
        header_name: None,
        command: Some(CredentialCommand {
            program: gh.to_string_lossy().into_owned(),
            args: vec![
                "auth".into(),
                "token".into(),
                "--hostname".into(),
                "github.com".into(),
            ],
            cwd: None,
            timeout_ms: 5_000,
            refresh_interval_ms: 300_000,
        }),
    };
    provider.enabled = true;
    settings.providers.push(provider);
    settings.validate()
}

pub(crate) fn find_cli_executable(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        return None;
    }
    let mut candidates = Vec::new();
    if let Some(path) = env::var_os("PATH") {
        candidates.extend(env::split_paths(&path).map(|directory| directory.join(name)));
    }
    if let Some(home) = dirs::home_dir() {
        candidates.extend([
            home.join(".local/bin").join(name),
            home.join(".cargo/bin").join(name),
            home.join(".npm-global/bin").join(name),
            // Vendor CLIs install here; macOS GUI PATH does not include them.
            home.join(".kimi-code/bin").join(name),
            home.join(".opencode/bin").join(name),
            home.join(".claude/bin").join(name),
            home.join(".kiro/bin").join(name),
        ]);
    }
    candidates.extend([
        PathBuf::from("/opt/homebrew/bin").join(name),
        PathBuf::from("/usr/local/bin").join(name),
        PathBuf::from("/usr/bin").join(name),
    ]);
    candidates.into_iter().find_map(|candidate| {
        let metadata = fs::metadata(&candidate).ok()?;
        metadata
            .is_file()
            .then(|| fs::canonicalize(&candidate).unwrap_or(candidate))
    })
}
