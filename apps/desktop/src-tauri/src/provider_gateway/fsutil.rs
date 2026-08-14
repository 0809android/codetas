use super::*;

pub(crate) fn gateway_url(settings: &GatewaySettings) -> String {
    let host = match settings.runtime.host.as_str() {
        "localhost" | "0.0.0.0" => "127.0.0.1".to_string(),
        "::" => "[::1]".to_string(),
        host if host.contains(':') => format!("[{host}]"),
        host => host.to_string(),
    };
    format!("http://{host}:{}/v1", settings.runtime.port)
}

pub(crate) fn atomic_write(path: &Path, content: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("設定フォルダを作れません: {error}"))?;
    }
    let temporary = path.with_extension(format!(
        "codetas-{}-{}.tmp",
        std::process::id(),
        ATOMIC_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
    ));
    let _ = fs::remove_file(&temporary);
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("一時設定ファイルを作れません: {error}"))?;
    if let Err(error) = file.write_all(content).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("設定を書き込めません: {error}"));
    }
    if let Err(error) = replace_file(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("設定を置き換えられません: {error}"));
    }
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn replace_file(temporary: &Path, path: &Path) -> std::io::Result<()> {
    fs::rename(temporary, path)
}

#[cfg(windows)]
pub(crate) fn replace_file(temporary: &Path, path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return fs::rename(temporary, path);
    }
    let backup = path.with_extension(format!(
        "codetas-replace-{}-{}.bak",
        std::process::id(),
        WINDOWS_REPLACE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
    ));
    let _ = fs::remove_file(&backup);
    fs::rename(path, &backup)?;
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

pub(crate) fn remove_owned_file(path: &Path) -> Result<bool, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("CODETAS所有ファイルを確認できません: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("CODETAS所有ファイルは通常ファイルではないため削除しません".into());
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("CODETAS所有ファイルを削除できません: {error}")),
    }
}
