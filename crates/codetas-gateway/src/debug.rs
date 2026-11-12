use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static LOCK: Mutex<()> = Mutex::new(());
const MAX_DEBUG_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// Append a diagnostic line to the durable debug log.
///
/// Writes to `CODETAS_DEBUG_LOG` when set, otherwise `~/.codex/codetas-debug.log`.
/// Tests stay silent unless `CODETAS_DEBUG_LOG` is set.
pub fn log(message: &str) {
    write_debug_log(message);
}

/// Always print a diagnostic line to stderr, and also append it to the durable log.
pub fn log_always(message: &str) {
    eprintln!("[codetas] {message}");
    write_debug_log(message);
}

fn debug_log_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("CODETAS_DEBUG_LOG") {
        return (!path.trim().is_empty()).then(|| PathBuf::from(path));
    }
    if cfg!(test) {
        return None;
    }
    let home = std::env::var_os("HOME").filter(|value| !value.is_empty())?;
    Some(PathBuf::from(home).join(".codex").join("codetas-debug.log"))
}

/// Where the opt-in raw upstream error bodies go: next to the debug log, inside the
/// user's own directory. A fixed name in a shared temporary directory could be
/// pre-created or linked elsewhere by another local account.
pub(crate) fn upstream_error_log_path() -> Option<PathBuf> {
    debug_log_path().map(|path| path.with_file_name("codetas-upstream-errors.log"))
}

fn write_debug_log(message: &str) {
    let Some(path) = debug_log_path() else {
        return;
    };
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    append_private_log(&path, &format!("[{ts}] {message}\n"));
}

/// Appends to an owner-only log file, rotating it to `*.log.old` at
/// `MAX_DEBUG_LOG_BYTES`. A symbolic link at the path is never followed, and a file
/// left readable by others from an earlier version is narrowed to the owner.
pub(crate) fn append_private_log(path: &Path, text: &str) {
    let _guard = LOCK.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => return,
        Ok(metadata) if metadata.len() >= MAX_DEBUG_LOG_BYTES => {
            let _ = fs::rename(path, path.with_extension("log.old"));
        }
        _ => {}
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let Ok(mut file) = options.open(path) else {
        return;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
    }
    let _ = file.write_all(text.as_bytes());
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn log_files_are_owner_only_even_when_they_already_existed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("codetas-debug.log");
        fs::write(&path, "old\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        append_private_log(&path, "new\n");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(fs::read_to_string(&path).unwrap(), "old\nnew\n");
    }

    #[test]
    fn a_symbolic_link_at_the_log_path_is_not_followed() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.txt");
        fs::write(&target, "untouched").unwrap();
        let path = directory.path().join("codetas-upstream-errors.log");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        append_private_log(&path, "secret body\n");
        assert_eq!(fs::read_to_string(&target).unwrap(), "untouched");
    }
}
