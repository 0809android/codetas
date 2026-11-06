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
    write_codex_backup(journal_path, &original, prefix, extension, created).map(Some)
}

/// The config backup is what a disconnect goes back to. A config already on the
/// gateway with no journal holds CODETAS's values, which are not official ones.
pub(crate) fn create_codex_config_backup(
    journal_path: &Path,
    config_path: &Path,
    catalog_path: &Path,
    created: &mut Vec<PathBuf>,
) -> Result<Option<String>, String> {
    let Some(original) = read_optional_file(config_path)? else {
        return Ok(None);
    };
    let official = official_config_content(&original, catalog_path);
    write_codex_backup(
        journal_path,
        official.as_deref().unwrap_or(&original),
        "codex-config-before",
        "toml",
        created,
    )
    .map(Some)
}

fn official_config_content(original: &[u8], catalog_path: &Path) -> Option<Vec<u8>> {
    let mut document = std::str::from_utf8(original)
        .ok()?
        .parse::<DocumentMut>()
        .ok()?;
    strip_codetas_connection(&mut document, catalog_path, None)
        .then(|| document.to_string().into_bytes())
}

fn write_codex_backup(
    journal_path: &Path,
    content: &[u8],
    prefix: &str,
    extension: &str,
    created: &mut Vec<PathBuf>,
) -> Result<String, String> {
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
    atomic_write(&backup, content)
        .map_err(|error| format!("Codexバックアップを作れません: {error}"))?;
    created.push(backup.clone());
    Ok(backup.to_string_lossy().into_owned())
}

pub(crate) fn restore_agents_before_disable(
    journal_path: &Path,
    document: &mut DocumentMut,
) -> Result<(), String> {
    let Some(journal) = read_codex_journal(journal_path)? else {
        return Ok(());
    };
    let (_, _, backup_path, _) =
        validate_codex_journal_paths_from_journal_path(journal_path, &journal)?;
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

#[derive(Clone, Copy)]
pub(crate) enum CodexGatewayTransportClaim<'a> {
    /// First install: never overwrite a different base URL.
    RespectExisting,
    /// A journal says CODETAS installed this URL. A config still holding it holds
    /// our own value, even though the gateway has since moved to another port.
    JournalOwned(&'a str),
    /// Temporary official fallback is over. Take back a local loopback
    /// transport so Codex returns to CODETAS without waiting for a manual
    /// reconnect. Remote user-owned URLs stay untouched.
    ReclaimLocalLoopback,
}

#[cfg(test)]
pub(crate) fn configure_native_codex_gateway(
    document: &mut DocumentMut,
    gateway_base_url: &str,
    model: &str,
    catalog_path: &Path,
) -> Result<(), String> {
    configure_native_codex_gateway_with_claim(
        document,
        gateway_base_url,
        model,
        catalog_path,
        CodexGatewayTransportClaim::RespectExisting,
    )
}

fn is_codetas_backend_loopback_url(value: &str) -> bool {
    value
        .trim_end_matches('/')
        .strip_suffix("/backend-api")
        .is_some_and(|origin| is_codetas_loopback_url(&format!("{origin}/v1")))
}

/// Older builds pointed `chatgpt_base_url` at the gateway to answer Codex's plan-limit
/// probe. Codex 0.166 refuses a workspace backend that is not HTTPS: `account/read`
/// fails, and every ChatGPT backend request in the app fails with it ("Workspace
/// routing is unavailable"). So CODETAS never writes this key and removes what it
/// wrote. No one else writes a loopback `/backend-api`, whatever its port.
fn remove_stale_backend_url(document: &mut DocumentMut) -> bool {
    let stale = document
        .get("chatgpt_base_url")
        .and_then(Item::as_str)
        .is_some_and(is_codetas_backend_loopback_url);
    if stale {
        document.remove("chatgpt_base_url");
    }
    stale
}

/// For a start where the port has not moved, so nothing else would touch the config.
/// A config that cannot be read is left for the paths that report it.
fn remove_stale_backend_url_from_file(config_path: &Path) -> Result<(), String> {
    let Ok(source) = fs::read_to_string(config_path) else {
        return Ok(());
    };
    let Ok(mut document) = source.parse::<DocumentMut>() else {
        return Ok(());
    };
    if remove_stale_backend_url(&mut document) {
        atomic_write(config_path, document.to_string().as_bytes())?;
    }
    Ok(())
}

const CODETAS_URL_KEYS: [&str; 2] = ["openai_base_url", "chatgpt_base_url"];

fn is_codetas_own_url(value: &str) -> bool {
    is_codetas_loopback_url(value) || is_codetas_backend_loopback_url(value)
}

fn names_codetas_catalog(document: &DocumentMut, catalog_path: &Path) -> bool {
    document
        .get("model_catalog_json")
        .and_then(Item::as_str)
        .is_some_and(|catalog| same_codex_owned_path(Path::new(catalog), catalog_path))
}

fn strip_codetas_urls(document: &mut DocumentMut) -> bool {
    let mut found = false;
    for key in CODETAS_URL_KEYS {
        if document
            .get(key)
            .and_then(Item::as_str)
            .is_some_and(is_codetas_own_url)
        {
            document.remove(key);
            found = true;
        }
    }
    found
}

/// The catalog reference and a model only the gateway can serve. Both are useless
/// to official Codex once the URLs are gone.
fn strip_codetas_choices(
    document: &mut DocumentMut,
    catalog_path: &Path,
    settings: Option<&GatewaySettings>,
) {
    if names_codetas_catalog(document, catalog_path) {
        document.remove("model_catalog_json");
    }
    if document
        .get("model")
        .and_then(Item::as_str)
        .is_some_and(|model| model_needs_gateway(settings, model))
    {
        document.remove("model");
    }
}

/// For a config already known to be on the gateway: an install refuses any other
/// loopback URL, so whatever loopback URL it holds is CODETAS's.
fn strip_codetas_connection(
    document: &mut DocumentMut,
    catalog_path: &Path,
    settings: Option<&GatewaySettings>,
) -> bool {
    let found = strip_codetas_urls(document);
    if found {
        strip_codetas_choices(document, catalog_path, settings);
    }
    found
}

/// A loopback `/v1` URL alone can be someone's own local proxy. It is CODETAS's when
/// it is a URL CODETAS knows, or next to something only an install writes: the
/// catalog reference or a loopback `/backend-api`. That holds on any port.
fn config_is_on_codetas_gateway(
    document: &DocumentMut,
    catalog_path: &Path,
    known_urls: &[&str],
) -> bool {
    let wrote_catalog = names_codetas_catalog(document, catalog_path);
    let wrote_backend = document
        .get("chatgpt_base_url")
        .and_then(Item::as_str)
        .is_some_and(is_codetas_backend_loopback_url);
    CODETAS_URL_KEYS
        .into_iter()
        .filter_map(|key| document.get(key).and_then(Item::as_str))
        .any(|url| {
            is_codetas_own_url(url) && (wrote_catalog || wrote_backend || known_urls.contains(&url))
        })
}

/// With no journal there is no backup to go back to, so disconnecting removes what
/// CODETAS wrote and leaves the rest. Returns whether there was anything to remove.
pub(crate) fn remove_codetas_connection_without_journal(
    document: &mut DocumentMut,
    catalog_path: &Path,
    known_urls: &[&str],
    settings: &GatewaySettings,
) -> Result<bool, String> {
    if document.get("model_provider").and_then(Item::as_str) == Some(GATEWAY_PROVIDER_ID) {
        let provider_url = document
            .get("model_providers")
            .and_then(Item::as_table)
            .and_then(|providers| providers.get(GATEWAY_PROVIDER_ID))
            .and_then(Item::as_table)
            .and_then(|provider| provider.get("base_url"))
            .and_then(Item::as_str)
            .filter(|url| is_codetas_loopback_url(url))
            .unwrap_or_default()
            .to_string();
        remove_legacy_codetas_provider_if_owned(document, &provider_url)?;
        document.remove("model_provider");
        strip_codetas_urls(document);
        strip_codetas_choices(document, catalog_path, Some(settings));
        return Ok(true);
    }
    if !config_is_on_codetas_gateway(document, catalog_path, known_urls) {
        return Ok(false);
    }
    Ok(strip_codetas_connection(
        document,
        catalog_path,
        Some(settings),
    ))
}

/// Whether only the gateway can serve this model. The catalog publishes every routed
/// model as `provider/model`, official ids have no slash, and a route alias exists
/// only in CODETAS. This decides what official Codex can be left with, and has
/// nothing to do with how a request's history is continued.
pub(crate) fn model_needs_gateway(settings: Option<&GatewaySettings>, model: &str) -> bool {
    model.contains('/')
        || settings.is_some_and(|settings| {
            settings
                .routes
                .iter()
                .any(|route| route.alias.as_deref().unwrap_or(&route.id) == model)
        })
}

fn native_transport_url_is_writable(
    existing: &str,
    gateway_base_url: &str,
    claim: CodexGatewayTransportClaim,
) -> bool {
    existing == gateway_base_url
        || match claim {
            CodexGatewayTransportClaim::RespectExisting => false,
            CodexGatewayTransportClaim::JournalOwned(installed) => existing == installed,
            CodexGatewayTransportClaim::ReclaimLocalLoopback => is_codetas_loopback_url(existing),
        }
}

pub(crate) fn configure_native_codex_gateway_with_claim(
    document: &mut DocumentMut,
    gateway_base_url: &str,
    model: &str,
    catalog_path: &Path,
    claim: CodexGatewayTransportClaim,
) -> Result<(), String> {
    if let Some(existing_base_url) = document.get("openai_base_url").and_then(Item::as_str) {
        if !native_transport_url_is_writable(existing_base_url, gateway_base_url, claim) {
            return Err(
                "既存のopenai_base_urlはユーザー所有のため上書きしません。内容を確認してから再実行してください"
                    .into(),
            );
        }
    }
    if let Some(legacy_base_url) = document.get("chatgpt_base_url").and_then(Item::as_str) {
        if !is_codetas_backend_loopback_url(legacy_base_url)
            && !native_transport_url_is_writable(legacy_base_url, gateway_base_url, claim)
        {
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
        return remove_stale_backend_url_from_file(&config_path);
    }
    if journal.official_fallback_active {
        // The config holds official values for now, so there is no URL of ours in
        // it to move. Reconnecting writes this one, which must be the actual port.
        journal.installed_base_url = actual_url.to_string();
        let next_journal = serde_json::to_vec_pretty(&journal)
            .map_err(|error| format!("Codex復元情報を生成できません: {error}"))?;
        return atomic_write(journal_path, &next_journal);
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
        remove_stale_backend_url(&mut document);
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

/// Service mode has no window and its process is not the app's, so the journal is
/// the only place a failure here can reach the connection panel from.
pub(crate) fn reconcile_service_runtime_url_at(journal_path: &Path, actual_url: &str) {
    if let Err(error) = reconcile_owned_codex_runtime_url_path(journal_path, actual_url) {
        eprintln!("CODETAS Gateway: Codex dynamic-port reconciliation skipped: {error}");
        let _ = record_watchdog_error(journal_path, Some(&error));
    }
}

/// What must hold for as long as a gateway is up. The order matters: reconnecting
/// writes the journal's URL, so the journal has to follow the gateway first. With
/// `auto_connect` off a config that fell back stays official until the user connects.
pub(crate) fn sync_codex_with_live_gateway_at(
    journal_path: &Path,
    live_url: &str,
    settings: &GatewaySettings,
) -> Result<bool, String> {
    reconcile_owned_codex_runtime_url_path(journal_path, live_url)?;
    if !settings.codex.auto_connect {
        return Ok(false);
    }
    reapply_codex_gateway_after_official_fallback_at(journal_path, settings)
}

pub(crate) fn official_fallback_is_active(app: &AppHandle) -> bool {
    let Ok(path) = codex_journal_path(app) else {
        return false;
    };
    read_codex_journal(&path)
        .ok()
        .flatten()
        .is_some_and(|journal| journal.official_fallback_active)
}

pub(crate) fn apply_temporary_official_codex_fallback(app: &AppHandle) -> Result<bool, String> {
    let journal_path = codex_journal_path(app)?;
    let settings = load_settings(app).ok();
    apply_temporary_official_codex_fallback_at(&journal_path, settings.as_ref())
}

pub(crate) fn apply_temporary_official_codex_fallback_at(
    journal_path: &Path,
    settings: Option<&GatewaySettings>,
) -> Result<bool, String> {
    let Some(mut journal) = read_codex_journal(journal_path)? else {
        return Ok(false);
    };
    if journal.official_fallback_active {
        return Ok(false);
    }
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
    let picked = model_picked_in_codex(&document, settings);
    let mut conflicts = Vec::new();
    apply_owned_codex_restore(&mut document, &backup, &journal, settings, &mut conflicts)?;
    if !conflicts.is_empty() {
        return Err(format!(
            "公式Codexへ一時退避できません: {}",
            conflicts.join(" / ")
        ));
    }
    if let Some(picked) = picked {
        journal.installed_model = picked;
    }
    let current_catalog = read_optional_file(&catalog_path)?;
    let original_catalog = match (journal.catalog_existed, catalog_backup_path.as_deref()) {
        (Some(true), Some(path)) => Some(
            read_optional_file(path)?.ok_or("Codexモデルカタログのバックアップが見つかりません")?,
        ),
        (Some(false), None) | (None, _) => None,
        _ => return Err("Codexモデルカタログの復元情報が矛盾しています".into()),
    };
    let expected_catalog = settings
        .map(|settings| {
            serde_json::to_value(build_codex_catalog(settings))
                .map_err(|error| format!("CODETASモデルカタログを検証できません: {error}"))
        })
        .transpose()?;
    let current_catalog_json = current_catalog
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<JsonValue>(bytes).ok());
    let catalog_owned = expected_catalog
        .as_ref()
        .zip(current_catalog_json.as_ref())
        .is_some_and(|(expected, actual)| actual == expected);
    let catalog_already_restored = match journal.catalog_existed {
        Some(true) => current_catalog.as_deref() == original_catalog.as_deref(),
        Some(false) => current_catalog.is_none(),
        None => false,
    };
    // A catalog that differs from what CODETAS would generate may hold someone's
    // edits, so leave the file alone for the outage (reconnecting regenerates it).
    // That is no reason to skip the fallback, or Codex keeps pointing at a gateway
    // that is down, unless official Codex would go on to load this unknown file.
    let catalog_unrecognized = !catalog_owned && !catalog_already_restored;
    let restored_config_uses_catalog = document
        .get("model_catalog_json")
        .and_then(Item::as_str)
        .is_some_and(|restored| same_codex_owned_path(Path::new(restored), &catalog_path));
    if catalog_unrecognized && restored_config_uses_catalog {
        return Err(
            "model_catalog_json: CODETASの生成後にカタログ内容が変更されているため公式へ一時退避しません"
                .into(),
        );
    }
    let preserve_catalog = journal.catalog_existed.is_none() || catalog_unrecognized;
    let current_config = fs::read(&config_path)
        .map_err(|error| format!("現在のCodex設定を退避できません: {error}"))?;
    journal.official_fallback_active = true;
    journal.last_watchdog_error = None;
    journal.last_watchdog_error_at = None;
    let next_journal = serde_json::to_vec_pretty(&journal)
        .map_err(|error| format!("CODETAS復元情報を生成できません: {error}"))?;
    let restore_result = (|| {
        atomic_write(&config_path, document.to_string().as_bytes())?;
        if !preserve_catalog {
            restore_optional_file(&catalog_path, original_catalog.as_deref())?;
        }
        atomic_write(journal_path, &next_journal)
    })();
    if let Err(error) = restore_result {
        let mut rollback_errors = Vec::new();
        if let Err(rollback) = atomic_write(&config_path, &current_config) {
            rollback_errors.push(format!("Codex設定: {rollback}"));
        }
        if let Err(rollback) = restore_optional_file(&catalog_path, current_catalog.as_deref()) {
            rollback_errors.push(format!("モデルカタログ: {rollback}"));
        }
        return Err(if rollback_errors.is_empty() {
            format!("公式Codexへ一時退避できないため変更を取り消しました: {error}")
        } else {
            format!(
                "公式Codexへ一時退避できず、変更の取り消しにも失敗しました: {error}; {}",
                rollback_errors.join("; ")
            )
        });
    }
    Ok(true)
}

/// The model a reconnect should return to: what Codex last picked, while the catalog
/// can still offer it. `installed_model` is what reconnecting reapplies.
fn model_picked_in_codex(
    document: &DocumentMut,
    settings: Option<&GatewaySettings>,
) -> Option<String> {
    let model = document.get("model").and_then(Item::as_str)?;
    settings
        .filter(|settings| super::gateway_ops::is_codex_catalog_model(settings, model))
        .map(|_| model.to_string())
}

/// Codex writes the model picker's choice to `model`, and a provider or model can
/// leave the catalog after that, so no value here is ever a reason to keep Codex on
/// a gateway. The backup's model comes back when CODETAS installed the current one,
/// or when official Codex could not serve it. Anything else is the user's official
/// choice. During the fallback the config already holds official values, so a match
/// with the journal proves nothing.
fn restore_owned_model(
    document: &mut DocumentMut,
    backup: &DocumentMut,
    journal: &CodexInstallJournal,
    settings: Option<&GatewaySettings>,
) {
    let Some(current) = document.get("model").and_then(Item::as_str) else {
        return;
    };
    let installed = !journal.official_fallback_active && current == journal.installed_model;
    if !installed && !model_needs_gateway(settings, current) {
        return;
    }
    if let Some(previous) = backup.get("model") {
        document["model"] = previous.clone();
    } else {
        document.remove("model");
    }
}

/// What the backup says official Codex used. A backup taken from a config already on
/// the gateway (a legacy install, or a lost journal) holds CODETAS's own values, so
/// they count as absent. A config holding the same value got it from that backup.
fn official_values_in_backup(
    backup: &DocumentMut,
    document: &mut DocumentMut,
    catalog_path: &Path,
    settings: Option<&GatewaySettings>,
) -> DocumentMut {
    for key in CODETAS_URL_KEYS {
        let held = backup
            .get(key)
            .and_then(Item::as_str)
            .filter(|url| is_codetas_own_url(url));
        if held.is_some() && document.get(key).and_then(Item::as_str) == held {
            document.remove(key);
        }
    }
    let mut official = backup.clone();
    strip_codetas_connection(&mut official, catalog_path, settings);
    official
}

pub(crate) fn apply_owned_codex_restore(
    document: &mut DocumentMut,
    backup: &DocumentMut,
    journal: &CodexInstallJournal,
    settings: Option<&GatewaySettings>,
    conflicts: &mut Vec<String>,
) -> Result<(), String> {
    let official =
        official_values_in_backup(backup, document, Path::new(&journal.catalog_path), settings);
    let backup = &official;
    let installed_provider = match journal.routing_mode {
        CodexRoutingMode::ProviderTable => GATEWAY_PROVIDER_ID,
        CodexRoutingMode::OpenAiBaseUrl => "openai",
    };
    restore_owned_string(
        document,
        backup,
        "model_provider",
        installed_provider,
        conflicts,
    );
    restore_owned_model(document, backup, journal, settings);
    // The config and the journal can name the same catalog differently when
    // CODEX_HOME points at the target of a `~/.codex` symlink.
    let installed_catalog = document
        .get("model_catalog_json")
        .and_then(Item::as_str)
        .filter(|current| {
            same_codex_owned_path(Path::new(current), Path::new(&journal.catalog_path))
        })
        .unwrap_or(&journal.catalog_path)
        .to_string();
    restore_owned_string(
        document,
        backup,
        "model_catalog_json",
        &installed_catalog,
        conflicts,
    );
    match journal.routing_mode {
        CodexRoutingMode::ProviderTable => {
            restore_owned_provider(document, backup, journal, conflicts)?;
        }
        CodexRoutingMode::OpenAiBaseUrl => {
            restore_owned_string(
                document,
                backup,
                "openai_base_url",
                &journal.installed_base_url,
                conflicts,
            );
            // Only a config an older build wrote still holds one of ours. Any other
            // value is the user's, and CODETAS has no claim on it.
            let stale_backend = document
                .get("chatgpt_base_url")
                .and_then(Item::as_str)
                .filter(|url| is_codetas_backend_loopback_url(url))
                .map(str::to_string);
            if let Some(stale_backend) = stale_backend {
                restore_owned_string(
                    document,
                    backup,
                    "chatgpt_base_url",
                    &stale_backend,
                    conflicts,
                );
            }
        }
    }
    if let Some(installed_agents) = journal.installed_agents.as_ref() {
        restore_owned_agent_settings(document, backup, installed_agents, conflicts)?;
    }
    Ok(())
}

pub(crate) fn reapply_codex_gateway_after_official_fallback_at(
    journal_path: &Path,
    settings: &GatewaySettings,
) -> Result<bool, String> {
    let Some(mut journal) = read_codex_journal(journal_path)? else {
        return Ok(false);
    };
    if !journal.official_fallback_active {
        return Ok(false);
    }
    let (config_path, catalog_path, _, _) =
        validate_codex_journal_paths_from_journal_path(journal_path, &journal)?;
    let current = match fs::read_to_string(&config_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("現在のCodex設定を読めません: {error}")),
    };
    let mut document = if current.trim().is_empty() {
        DocumentMut::new()
    } else {
        current
            .parse::<DocumentMut>()
            .map_err(|error| format!("現在のCodex設定を解析できません: {error}"))?
    };
    configure_native_codex_gateway_with_claim(
        &mut document,
        &journal.installed_base_url,
        &journal.installed_model,
        &catalog_path,
        CodexGatewayTransportClaim::ReclaimLocalLoopback,
    )?;
    let catalog_content = serialize_codex_catalog(settings, Some(&journal.installed_model))?;
    let original_config = read_optional_file(&config_path)?;
    let original_catalog = read_optional_file(&catalog_path)?;
    journal.official_fallback_active = false;
    journal.last_watchdog_error = None;
    journal.last_watchdog_error_at = None;
    let next_journal = serde_json::to_vec_pretty(&journal)
        .map_err(|error| format!("CODETAS復元情報を生成できません: {error}"))?;
    let restore_result = (|| {
        atomic_write(&catalog_path, &catalog_content)?;
        atomic_write(&config_path, document.to_string().as_bytes())?;
        atomic_write(journal_path, &next_journal)
    })();
    if let Err(error) = restore_result {
        let mut rollback_errors = Vec::new();
        if let Err(rollback) = restore_optional_file(&config_path, original_config.as_deref()) {
            rollback_errors.push(format!("Codex設定: {rollback}"));
        }
        if let Err(rollback) = restore_optional_file(&catalog_path, original_catalog.as_deref()) {
            rollback_errors.push(format!("モデルカタログ: {rollback}"));
        }
        return Err(if rollback_errors.is_empty() {
            format!("公式退避からの再接続に失敗したため変更を取り消しました: {error}")
        } else {
            format!(
                "公式退避からの再接続に失敗し、変更の取り消しにも失敗しました: {error}; {}",
                rollback_errors.join("; ")
            )
        });
    }
    Ok(true)
}
