use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
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

fn write_debug_log(message: &str) {
    let Some(path) = debug_log_path() else {
        return;
    };
    let _guard = LOCK.lock().unwrap();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(metadata) = fs::metadata(&path) {
        if metadata.len() >= MAX_DEBUG_LOG_BYTES {
            let rotated = path.with_extension("log.old");
            let _ = fs::rename(&path, rotated);
        }
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "[{ts}] {message}");
    }
}
