use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static LOCK: Mutex<()> = Mutex::new(());

/// Append a diagnostic line to the debug log. Only active when the
/// `CODETAS_DEBUG_LOG` environment variable is set to a file path; otherwise the
/// call is a no-op so production traffic pays no I/O cost.
pub fn log(message: &str) {
    let Ok(path) = std::env::var("CODETAS_DEBUG_LOG") else {
        return;
    };
    let _guard = LOCK.lock().unwrap();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "[{ts}] {message}");
    }
}
