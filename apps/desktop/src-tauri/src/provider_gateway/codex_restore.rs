use super::*;

pub(crate) fn command_succeeds(program: &Path, args: &[&str], timeout: Duration) -> bool {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = match command.group_spawn() {
        Ok(child) => child,
        Err(_) => return false,
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Probe commands never own long-lived descendants. Clean up the
                // process group / Job Object even when the leader exited first.
                let _ = child.kill();
                let _ = child.wait();
                return status.success();
            }
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(25)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

pub(crate) fn create_codex_backup(
    journal_path: &Path,
    source: &Path,
    prefix: &str,
    extension: &str,
    created: &mut Vec<PathBuf>,
) -> Result<Option<String>, String> {
    let Some(original) = read_optional_file(source)? else {
        return Ok(None);
    };
    let directory = journal_path
        .parent()
        .ok_or("CODETAS復元フォルダを特定できません")?;
    fs::create_dir_all(directory)
        .map_err(|error| format!("CODETAS復元フォルダを作れません: {error}"))?;
    let unique = format!(
        "{}-{}-{}",
        now_ms(),
        std::process::id(),
        ATOMIC_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let backup = directory.join(format!("{prefix}-{unique}.{extension}"));
    atomic_write(&backup, &original)
        .map_err(|error| format!("Codexバックアップを作れません: {error}"))?;
    created.push(backup.clone());
    Ok(Some(backup.to_string_lossy().into_owned()))
}

pub(crate) fn restore_agents_before_disable(
    app: &AppHandle,
    document: &mut DocumentMut,
) -> Result<(), String> {
    let journal_path = codex_journal_path(app)?;
    let Some(journal) = read_codex_journal(&journal_path)? else {
        return Ok(());
    };
    let (_, _, backup_path, _) = validate_codex_journal_paths(app, &journal)?;
    let Some(installed_agents) = journal.installed_agents.as_ref() else {
        return Ok(());
    };
    let backup = match backup_path.as_deref() {
        Some(path) => fs::read_to_string(path)
            .map_err(|error| format!("Codex設定バックアップを読めません: {error}"))?
            .parse::<DocumentMut>()
            .map_err(|error| format!("Codex設定バックアップを解析できません: {error}"))?,
        None => DocumentMut::new(),
    };
    let mut conflicts = Vec::new();
    restore_owned_agent_settings(document, &backup, installed_agents, &mut conflicts)?;
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Codexのagents設定がCODETAS接続後に変更されたため無効化を停止しました: {}",
            conflicts.join(" / ")
        ))
    }
}

pub(crate) fn restore_owned_agent_settings(
    document: &mut DocumentMut,
    backup: &DocumentMut,
    installed: &CodexAgentInstall,
    conflicts: &mut Vec<String>,
) -> Result<(), String> {
    restore_owned_agent_bool(document, backup, "enabled", installed.enabled, conflicts)?;
    restore_owned_agent_integer(
        document,
        backup,
        "max_concurrent_threads_per_session",
        installed.max_concurrent_threads_per_session,
        conflicts,
    )?;
    let should_remove = document
        .get("agents")
        .and_then(Item::as_table)
        .is_some_and(Table::is_empty)
        && backup.get("agents").is_none();
    if should_remove {
        document.remove("agents");
    }
    Ok(())
}

pub(crate) fn restore_owned_agent_bool(
    document: &mut DocumentMut,
    backup: &DocumentMut,
    key: &str,
    installed: bool,
    conflicts: &mut Vec<String>,
) -> Result<(), String> {
    let current = document
        .get("agents")
        .and_then(Item::as_table)
        .and_then(|agents| agents.get(key))
        .and_then(Item::as_bool);
    let previous = backup
        .get("agents")
        .and_then(Item::as_table)
        .and_then(|agents| agents.get(key))
        .and_then(Item::as_bool);
    if current != Some(installed) {
        if current == previous {
            return Ok(());
        }
        conflicts.push(format!(
            "agents.{key} was changed or removed after installation"
        ));
        return Ok(());
    }
    restore_agent_item(document, backup, key)
}

pub(crate) fn restore_owned_agent_integer(
    document: &mut DocumentMut,
    backup: &DocumentMut,
    key: &str,
    installed: i64,
    conflicts: &mut Vec<String>,
) -> Result<(), String> {
    let current = document
        .get("agents")
        .and_then(Item::as_table)
        .and_then(|agents| agents.get(key))
        .and_then(Item::as_integer);
    let previous = backup
        .get("agents")
        .and_then(Item::as_table)
        .and_then(|agents| agents.get(key))
        .and_then(Item::as_integer);
    if current != Some(installed) {
        if current == previous {
            return Ok(());
        }
        conflicts.push(format!(
            "agents.{key} was changed or removed after installation"
        ));
        return Ok(());
    }
    restore_agent_item(document, backup, key)
}

pub(crate) fn restore_agent_item(
    document: &mut DocumentMut,
    backup: &DocumentMut,
    key: &str,
) -> Result<(), String> {
    let previous = backup
        .get("agents")
        .and_then(Item::as_table)
        .and_then(|agents| agents.get(key))
        .cloned();
    let agents = document
        .get_mut("agents")
        .and_then(Item::as_table_mut)
        .ok_or("agents must be a TOML table")?;
    if let Some(previous) = previous {
        agents[key] = previous;
    } else {
        agents.remove(key);
    }
    Ok(())
}

pub(crate) fn restore_owned_string(
    document: &mut DocumentMut,
    backup: &DocumentMut,
    key: &str,
    installed_value: &str,
    conflicts: &mut Vec<String>,
) {
    let current = document.get(key).and_then(Item::as_str);
    let previous = backup.get(key).and_then(Item::as_str);
    if current != Some(installed_value) {
        if current != previous {
            conflicts.push(format!("{key} was changed after CODETAS installation"));
        }
        return;
    }
    if let Some(previous) = backup.get(key) {
        document[key] = previous.clone();
    } else {
        document.remove(key);
    }
}

pub(crate) fn restore_owned_provider(
    document: &mut DocumentMut,
    backup: &DocumentMut,
    journal: &CodexInstallJournal,
    conflicts: &mut Vec<String>,
) -> Result<(), String> {
    let current_item = document
        .get("model_providers")
        .and_then(Item::as_table)
        .and_then(|providers| providers.get(GATEWAY_PROVIDER_ID));
    let previous_item = backup
        .get("model_providers")
        .and_then(Item::as_table)
        .and_then(|providers| providers.get(GATEWAY_PROVIDER_ID));
    if current_item.map(|item| item.to_string()) == previous_item.map(|item| item.to_string()) {
        return Ok(());
    }
    let current = current_item.and_then(Item::as_table);
    let owned = current.is_some_and(|provider| {
        let expected_length = if journal.installed_local_token { 5 } else { 4 };
        let token_header_matches = if journal.installed_local_token {
            provider
                .get("env_http_headers")
                .and_then(Item::as_table)
                .and_then(|headers| headers.get("x-codetas-token"))
                .and_then(Item::as_str)
                == Some("CODETAS_GATEWAY_TOKEN")
        } else {
            provider.get("env_http_headers").is_none()
        };
        provider.len() == expected_length
            && provider.get("name").and_then(Item::as_str) == Some("CODETAS Gateway")
            && provider.get("base_url").and_then(Item::as_str)
                == Some(journal.installed_base_url.as_str())
            && provider.get("wire_api").and_then(Item::as_str) == Some("responses")
            && provider.get("supports_websockets").and_then(Item::as_bool) == Some(true)
            && token_header_matches
    });
    if current.is_none() {
        conflicts.push("model_providers.codetas_gateway was removed after installation".into());
        return Ok(());
    }
    if !owned {
        conflicts.push("model_providers.codetas_gateway was changed after installation".into());
        return Ok(());
    }
    let previous = backup
        .get("model_providers")
        .and_then(Item::as_table)
        .and_then(|providers| providers.get(GATEWAY_PROVIDER_ID))
        .cloned();
    let providers = document
        .get_mut("model_providers")
        .and_then(Item::as_table_mut)
        .ok_or("model_providers must be a TOML table")?;
    if let Some(previous) = previous {
        providers[GATEWAY_PROVIDER_ID] = previous;
    } else {
        providers.remove(GATEWAY_PROVIDER_ID);
    }
    Ok(())
}

pub(crate) fn is_codetas_loopback_url(value: &str) -> bool {
    let Some(authority_and_path) = value.strip_prefix("http://") else {
        return false;
    };
    if authority_and_path.contains(['@', '?', '#']) {
        return false;
    }
    let Some((authority, path)) = authority_and_path.split_once('/') else {
        return false;
    };
    let host = authority
        .split_once(':')
        .map(|(host, port)| {
            (!port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())).then_some(host)
        })
        .unwrap_or(Some(authority));
    host.is_some_and(|host| matches!(host, "127.0.0.1" | "localhost"))
        && path.trim_end_matches('/') == "v1"
}

pub(crate) fn remove_legacy_codetas_provider_if_owned(
    document: &mut DocumentMut,
    gateway_base_url: &str,
) -> Result<(), String> {
    let Some(item) = document
        .get("model_providers")
        .and_then(Item::as_table)
        .and_then(|providers| providers.get(GATEWAY_PROVIDER_ID))
    else {
        return Ok(());
    };
    let Some(provider) = item.as_table() else {
        return Err("既存のmodel_providers.codetas_gatewayがtableではありません".into());
    };
    let current_keys = provider.iter().map(|(key, _)| key).collect::<BTreeSet<_>>();
    let mut expected_current_keys = ["name", "base_url", "wire_api", "supports_websockets"]
        .into_iter()
        .collect::<BTreeSet<_>>();
    if provider.contains_key("env_http_headers") {
        expected_current_keys.insert("env_http_headers");
    }
    let current_shape_owned = document.get("model_provider").and_then(Item::as_str)
        == Some(GATEWAY_PROVIDER_ID)
        && current_keys == expected_current_keys
        && provider.get("name").and_then(Item::as_str) == Some("CODETAS Gateway")
        && provider.get("base_url").and_then(Item::as_str) == Some(gateway_base_url)
        && provider.get("wire_api").and_then(Item::as_str) == Some("responses")
        && provider.get("supports_websockets").and_then(Item::as_bool) == Some(true)
        && provider.get("env_http_headers").is_none_or(|headers| {
            let Some(headers) = headers.as_table() else {
                return false;
            };
            headers.len() == 1
                && headers.get("x-codetas-token").and_then(Item::as_str)
                    == Some("CODETAS_GATEWAY_TOKEN")
        });
    let legacy_url_key = if provider.contains_key("base_url") {
        Some("base_url")
    } else if provider.contains_key("chatgpt_base_url") {
        Some("chatgpt_base_url")
    } else {
        None
    };
    let legacy_keys = provider.iter().map(|(key, _)| key).collect::<BTreeSet<_>>();
    let mut expected_legacy_keys = [
        "name",
        "wire_api",
        "requires_openai_auth",
        "supports_websockets",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    if let Some(key) = legacy_url_key {
        expected_legacy_keys.insert(key);
    }
    let legacy_url = legacy_url_key
        .and_then(|key| provider.get(key))
        .and_then(Item::as_str);
    let legacy_shape_owned = document.get("model_provider").and_then(Item::as_str)
        == Some(GATEWAY_PROVIDER_ID)
        && legacy_keys == expected_legacy_keys
        && provider.get("name").and_then(Item::as_str) == Some("OpenAI")
        && legacy_url.is_some_and(is_codetas_loopback_url)
        && provider.get("wire_api").and_then(Item::as_str) == Some("responses")
        && provider.get("requires_openai_auth").and_then(Item::as_bool) == Some(false)
        && provider.get("supports_websockets").and_then(Item::as_bool) == Some(true);
    if !current_shape_owned && !legacy_shape_owned {
        return Err(
            "既存のmodel_providers.codetas_gatewayはCODETASの既知の設定と一致しないため変更しません"
                .into(),
        );
    }
    let providers = document
        .get_mut("model_providers")
        .and_then(Item::as_table_mut)
        .ok_or("model_providers must be a TOML table")?;
    providers.remove(GATEWAY_PROVIDER_ID);
    Ok(())
}

pub(crate) fn configure_native_codex_gateway(
    document: &mut DocumentMut,
    gateway_base_url: &str,
    model: &str,
    catalog_path: &Path,
) -> Result<(), String> {
    if let Some(existing_base_url) = document.get("openai_base_url").and_then(Item::as_str) {
        if existing_base_url != gateway_base_url {
            return Err(
                "既存のopenai_base_urlはユーザー所有のため上書きしません。内容を確認してから再実行してください"
                    .into(),
            );
        }
    }
    if let Some(legacy_base_url) = document.get("chatgpt_base_url").and_then(Item::as_str) {
        if legacy_base_url != gateway_base_url {
            return Err(
                "既存のchatgpt_base_urlはユーザー所有のため上書きしません。内容を確認してから再実行してください"
                    .into(),
            );
        }
        document.remove("chatgpt_base_url");
    }
    remove_legacy_codetas_provider_if_owned(document, gateway_base_url)?;
    // Preserve Codex's native provider identity while overriding the built-in
    // OpenAI transport. Namespaced catalog entries then remain selectable as
    // routed models without moving or rewriting existing `openai` sessions.
    document["model_provider"] = value("openai");
    document["model"] = value(model);
    document["model_catalog_json"] = value(catalog_path.to_string_lossy().into_owned());
    document["openai_base_url"] = value(gateway_base_url);
    Ok(())
}

#[cfg(feature = "validation-build")]
pub(crate) fn codex_home() -> Result<PathBuf, String> {
    dirs::home_dir()
        .map(|home| home.join(".codex-codetas-validation"))
        .ok_or_else(|| "検証用Codexホームを特定できません".into())
}

#[cfg(not(feature = "validation-build"))]
pub(crate) fn codex_home() -> Result<PathBuf, String> {
    if let Some(home) = env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    dirs::home_dir()
        .map(|home| home.join(".codex"))
        .ok_or_else(|| "ホームフォルダを特定できません".into())
}

pub(crate) fn write_codex_catalog(settings: &GatewaySettings) -> Result<PathBuf, String> {
    let path = codex_home()?.join("codetas-model-catalog.json");
    let content = serialize_codex_catalog(settings, None)?;
    atomic_write(&path, &content)?;
    Ok(path)
}

pub(crate) fn serialize_codex_catalog(
    settings: &GatewaySettings,
    selected_model: Option<&str>,
) -> Result<Vec<u8>, String> {
    let catalog = build_codex_catalog(settings);
    catalog
        .validate_for_codex(selected_model)
        .map_err(|error| format!("Codexモデルカタログの事前検証に失敗しました: {error}"))?;
    serde_json::to_vec_pretty(&catalog)
        .map_err(|error| format!("Codexモデルカタログを生成できません: {error}"))
}

pub(crate) fn read_optional_file(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("既存ファイルを確認できません: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("対象は通常ファイルではないため変更しません".into());
    }
    fs::read(path)
        .map(Some)
        .map_err(|error| format!("既存ファイルを退避できません: {error}"))
}

pub(crate) fn restore_optional_file(path: &Path, content: Option<&[u8]>) -> Result<(), String> {
    match content {
        Some(content) => atomic_write(path, content),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("新規作成ファイルを取り除けません: {error}")),
        },
    }
}

pub(crate) fn set_codex_catalog_path(path: &Path) -> Result<(), String> {
    let config_path = codex_config_path()?;
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
    document["model_catalog_json"] = value(path.to_string_lossy().into_owned());
    atomic_write(&config_path, document.to_string().as_bytes())
}

pub(crate) fn codex_gateway_is_configured(
    app: &AppHandle,
    settings: &GatewaySettings,
) -> Result<bool, String> {
    let path = codex_config_path()?;
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.to_string()),
    };
    let document = match content.parse::<DocumentMut>() {
        Ok(document) => document,
        Err(_) => return Ok(false),
    };
    let expected_url = gateway_url(settings);
    let runtime_url = runtime_gateway_url(app);
    let selected = document.get("model_provider").and_then(Item::as_str);
    let native_base_url = document
        .get("openai_base_url")
        .and_then(Item::as_str)
        .is_some_and(|value| {
            value == expected_url.as_str()
                || runtime_url
                    .as_deref()
                    .is_some_and(|runtime| value == runtime)
        });
    if selected.is_none_or(|provider| provider == "openai") && native_base_url {
        return Ok(true);
    }

    let registered = document
        .get("model_providers")
        .and_then(Item::as_table)
        .and_then(|providers| providers.get(GATEWAY_PROVIDER_ID))
        .and_then(Item::as_table)
        .and_then(|gateway| gateway.get("base_url"))
        .and_then(Item::as_str)
        .is_some_and(|value| {
            value == expected_url.as_str()
                || runtime_url
                    .as_deref()
                    .is_some_and(|runtime| value == runtime)
        });
    Ok(selected == Some(GATEWAY_PROVIDER_ID) && registered)
}

pub(crate) fn reconcile_owned_codex_runtime_url(
    app: &AppHandle,
    actual_url: &str,
) -> Result<(), String> {
    let journal_path = codex_journal_path(app)?;
    reconcile_owned_codex_runtime_url_path(&journal_path, actual_url)
}

pub(crate) fn reconcile_owned_codex_runtime_url_path(
    journal_path: &Path,
    actual_url: &str,
) -> Result<(), String> {
    let Some(mut journal) = read_codex_journal(journal_path)? else {
        return Ok(());
    };
    let (config_path, _, _, _) =
        validate_codex_journal_paths_from_journal_path(journal_path, &journal)?;
    if journal.installed_base_url == actual_url {
        return Ok(());
    }

    let original =
        fs::read(&config_path).map_err(|error| format!("Codex設定を読めません: {error}"))?;
    if original.len() > 16 * 1024 * 1024 {
        return Err("Codex設定が大きすぎます".into());
    }
    let source = std::str::from_utf8(&original).map_err(|_| "Codex設定がUTF-8ではありません")?;
    let mut document = source
        .parse::<DocumentMut>()
        .map_err(|error| format!("Codex設定を解析できません: {error}"))?;
    if journal.routing_mode == CodexRoutingMode::OpenAiBaseUrl {
        let selected = document
            .get("model_provider")
            .and_then(Item::as_str)
            .is_none_or(|provider| provider == "openai");
        let installed = document.get("openai_base_url").and_then(Item::as_str);
        if !selected || installed != Some(journal.installed_base_url.as_str()) {
            return Err(
                "Codex設定がCODETASの前回適用値から変更されているため、動的ポートへ更新しません"
                    .into(),
            );
        }
        document["openai_base_url"] = value(actual_url);
        atomic_write(&config_path, document.to_string().as_bytes())?;

        journal.installed_base_url = actual_url.to_string();
        let next_journal = serde_json::to_vec_pretty(&journal)
            .map_err(|error| format!("Codex復元情報を生成できません: {error}"))?;
        if let Err(error) = atomic_write(journal_path, &next_journal) {
            let _ = atomic_write(&config_path, &original);
            return Err(format!(
                "動的ポートの復元情報を保存できないためCodex設定を戻しました: {error}"
            ));
        }
        return Ok(());
    }

    let selected =
        document.get("model_provider").and_then(Item::as_str) == Some(GATEWAY_PROVIDER_ID);
    let gateway = document
        .get_mut("model_providers")
        .and_then(Item::as_table_mut)
        .and_then(|providers| providers.get_mut(GATEWAY_PROVIDER_ID))
        .and_then(Item::as_table_mut);
    let Some(gateway) = gateway else {
        return Ok(());
    };
    let installed = gateway.get("base_url").and_then(Item::as_str);
    if !selected || installed != Some(journal.installed_base_url.as_str()) {
        return Err(
            "Codex設定がCODETASの前回適用値から変更されているため、動的ポートへ更新しません".into(),
        );
    }
    gateway["base_url"] = value(actual_url);
    atomic_write(&config_path, document.to_string().as_bytes())?;

    journal.installed_base_url = actual_url.to_string();
    let next_journal = serde_json::to_vec_pretty(&journal)
        .map_err(|error| format!("Codex復元情報を生成できません: {error}"))?;
    if let Err(error) = atomic_write(journal_path, &next_journal) {
        let _ = atomic_write(&config_path, &original);
        return Err(format!(
            "動的ポートの復元情報を保存できないためCodex設定を戻しました: {error}"
        ));
    }
    Ok(())
}
