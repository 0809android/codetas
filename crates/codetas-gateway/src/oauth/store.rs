use super::*;
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use uuid::Uuid;

use std::io::Write;
pub(crate) struct AuthStoreLock {
    _process: std::sync::MutexGuard<'static, ()>,
    _file: File,
}

pub(crate) fn lock_auth_store(path: &Path) -> Result<AuthStoreLock, String> {
    let process = AUTH_FILE_LOCK
        .get_or_init(|| StdMutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let lock_path = auth_lock_path(path);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|_| "CODETAS auth directory could not be created")?;
    }
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&lock_path)
        .map_err(|_| "CODETAS auth store lock could not be opened".to_string())?;
    file.lock_exclusive()
        .map_err(|_| "CODETAS auth store is busy".to_string())?;
    Ok(AuthStoreLock {
        _process: process,
        _file: file,
    })
}

pub(crate) fn auth_lock_path(store_path: &Path) -> PathBuf {
    let name = store_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("auth.json");
    store_path.with_file_name(format!("{name}.lock"))
}

pub(crate) fn auth_refresh_lock() -> &'static tokio::sync::Mutex<()> {
    AUTH_REFRESH_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub(crate) fn read_store(path: &Path) -> Result<AuthStoreFile, String> {
    let _guard = lock_auth_store(path)?;
    load_store(path)
}

pub(crate) fn mutate_store(
    path: &Path,
    update: impl FnOnce(&mut AuthStoreFile) -> Result<(), String>,
) -> Result<(), String> {
    let _guard = lock_auth_store(path)?;
    let mut store = load_store(path)?;
    update(&mut store)?;
    save_store(path, &store)
}

pub(crate) fn stored_session(
    path: &Path,
    provider_id: &str,
) -> Result<Option<OAuthSession>, String> {
    Ok(read_store(path)?.providers.get(provider_id).cloned())
}

pub(crate) fn current_access_token(
    path: &Path,
    provider_id: &str,
) -> Result<Option<String>, String> {
    let _guard = lock_auth_store(path)?;
    let mut store = load_store(path)?;
    if !store.providers.contains_key(provider_id) {
        if let Some(session) = detect_local_cli_session(provider_id, &user_home()) {
            if cli_session_is_importable(&session) {
                let access = (!session_needs_refresh(&session)).then(|| session.access.clone());
                store.providers.insert(provider_id.to_string(), session);
                save_store(path, &store)?;
                return Ok(access);
            }
        }
    }
    let mut changed = false;
    let access = store.providers.get_mut(provider_id).and_then(|session| {
        if provider_id == "kimi" {
            let (account_id, email) = kimi_identity_from_tokens(
                &session.access,
                (!session.refresh.trim().is_empty()).then_some(session.refresh.as_str()),
            );
            if session.account_id.is_none() && account_id.is_some() {
                session.account_id = account_id;
                changed = true;
            }
            if session.email.is_none() && email.is_some() {
                session.email = email;
                changed = true;
            }
        }
        (!session_needs_refresh(session)).then(|| session.access.clone())
    });
    if changed {
        save_store(path, &store)?;
    }
    Ok(access)
}

pub(crate) fn cli_session_is_importable(session: &OAuthSession) -> bool {
    !session.access.trim().is_empty()
        && (!session_needs_refresh(session) || !session.refresh.trim().is_empty())
}

pub(crate) fn is_unwired_cli_target(provider: &ProviderDefinition) -> bool {
    matches!(
        provider.credential.source,
        CredentialSource::None | CredentialSource::Environment
    ) && !has_non_store_credential(provider)
}

pub(crate) fn has_non_store_credential(provider: &ProviderDefinition) -> bool {
    match provider.credential.source {
        CredentialSource::Environment => provider
            .credential
            .reference
            .as_deref()
            .or(provider.api_key_env.as_deref())
            .is_some_and(|name| env::var_os(name).is_some_and(|value| !value.is_empty())),
        CredentialSource::Keychain => provider
            .credential
            .reference
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty()),
        CredentialSource::Command => provider.credential.command.is_some(),
        CredentialSource::Forward => true,
        CredentialSource::OAuth => provider.credential.command.is_some(),
        CredentialSource::None => false,
    }
}

pub(crate) fn canonical_provider_id(provider_id: &str) -> &str {
    match provider_id {
        "kimi-code" => "kimi",
        other => other,
    }
}

pub(crate) fn user_home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub(crate) fn load_store(path: &Path) -> Result<AuthStoreFile, String> {
    match fs::read(path) {
        Ok(bytes) => {
            let store: AuthStoreFile = serde_json::from_slice(&bytes)
                .map_err(|_| "CODETAS auth store could not be parsed".to_string())?;
            if store.version != AUTH_STORE_VERSION {
                return Err("CODETAS auth store version is unsupported".into());
            }
            Ok(store)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(AuthStoreFile {
            version: AUTH_STORE_VERSION,
            providers: BTreeMap::new(),
        }),
        Err(_) => Err("CODETAS auth store could not be read".into()),
    }
}

pub(crate) fn save_store(path: &Path, store: &AuthStoreFile) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| "CODETAS auth directory could not be created")?;
    }
    let mut file = AuthStoreFile {
        version: AUTH_STORE_VERSION,
        providers: store.providers.clone(),
    };
    file.providers.retain(|_, session| {
        !session.access.trim().is_empty() || !session.refresh.trim().is_empty()
    });
    let bytes = serde_json::to_vec_pretty(&file)
        .map_err(|_| "CODETAS auth store could not be encoded".to_string())?;
    atomic_write_private(path, &bytes)
}

pub(crate) fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let directory = path
        .parent()
        .ok_or_else(|| "CODETAS auth store path is invalid".to_string())?;
    let sequence = AUTH_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = directory.join(format!(
        ".auth.{}.{sequence}.{}.tmp",
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    let _ = fs::remove_file(&temp);
    {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp)
            .map_err(|_| "CODETAS auth store could not be written")?;
        if let Err(_) = file.write_all(bytes).and_then(|_| file.sync_all()) {
            let _ = fs::remove_file(&temp);
            return Err("CODETAS auth store could not be written".into());
        }
    }
    if let Err(_) = replace_auth_file(&temp, path) {
        if path.exists() {
            let _ = fs::remove_file(&temp);
        }
        return Err("CODETAS auth store could not be replaced".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| "CODETAS auth store could not be protected".to_string())?;
        if fs::metadata(path)
            .map(|meta| meta.permissions().mode() & 0o777)
            .unwrap_or(0)
            != 0o600
        {
            return Err("CODETAS auth store could not be protected".into());
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn replace_auth_file(temporary: &Path, path: &Path) -> std::io::Result<()> {
    fs::rename(temporary, path)
}

#[cfg(windows)]
pub(crate) fn replace_auth_file(temporary: &Path, path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return fs::rename(temporary, path);
    }
    let backup = path.with_file_name(format!(
        ".auth-replace-{}-{}.bak",
        std::process::id(),
        AUTH_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_file(&backup);
    match fs::rename(path, &backup) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return fs::rename(temporary, path);
        }
        Err(error) => return Err(error),
    }
    match fs::rename(temporary, path) {
        Ok(()) => {
            let _ = fs::remove_file(backup);
            Ok(())
        }
        Err(error) => {
            let _ = fs::rename(&backup, path);
            Err(error)
        }
    }
}
