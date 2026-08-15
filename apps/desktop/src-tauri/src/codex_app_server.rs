use command_group::CommandGroup;
use serde::Serialize;
use serde_json::{json, Value as JsonValue};
use std::{
    io::{Read, Write},
    path::PathBuf,
    process::{ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    thread,
    time::{Duration, Instant},
};

const APP_SERVER_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_STDOUT_LINE_BYTES: usize = 512 * 1024;
const MAX_STDOUT_TOTAL_BYTES: usize = 2 * 1024 * 1024;
const MAX_STDOUT_LINES: usize = 256;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_SERVER_MESSAGE_CHARS: usize = 512;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CodexArchiveRetryReport {
    thread_id: String,
    archived: bool,
    transport: String,
    message: String,
}

enum StdoutEvent {
    Line(Vec<u8>),
    LimitExceeded(&'static str),
    ReadFailed,
    Eof,
}

/// Retries a Codex task archive through the official App Server protocol.
///
/// The child is placed in its own process group so every exit path terminates
/// helpers spawned by the CLI as well as the group leader.
#[tauri::command]
pub(crate) async fn retry_codex_archive(
    thread_id: String,
) -> Result<CodexArchiveRetryReport, String> {
    tauri::async_runtime::spawn_blocking(move || retry_codex_archive_blocking(thread_id))
        .await
        .map_err(|error| format!("Codex App Server操作を完了できませんでした: {error}"))?
}

fn retry_codex_archive_blocking(
    thread_id: String,
) -> Result<CodexArchiveRetryReport, String> {
    if !is_uuid(&thread_id) {
        return Err("threadId must be a UUID in 8-4-4-4-12 form".into());
    }

    let Some(codex) = find_codex_cli() else {
        return Ok(failed_report(
            thread_id,
            "Codex CLIが見つからないため、App Serverを起動できませんでした",
        ));
    };

    let mut command = Command::new(codex);
    command
        .args(["app-server", "--listen", "stdio://"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match command.group_spawn() {
        Ok(child) => child,
        Err(error) => {
            return Ok(failed_report(
                thread_id,
                format!("Codex App Serverを起動できませんでした: {error}"),
            ));
        }
    };

    let outcome = (|| {
        let mut stdin = child
            .inner()
            .stdin
            .take()
            .ok_or_else(|| "Codex App Serverのstdinを開けませんでした".to_string())?;
        let stdout = child
            .inner()
            .stdout
            .take()
            .ok_or_else(|| "Codex App Serverのstdoutを開けませんでした".to_string())?;
        let stderr = child
            .inner()
            .stderr
            .take()
            .ok_or_else(|| "Codex App Serverのstderrを開けませんでした".to_string())?;

        let (stdout_tx, stdout_rx) = mpsc::sync_channel(32);
        thread::spawn(move || read_stdout_jsonl(stdout, stdout_tx));
        let (stderr_tx, stderr_rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ = stderr_tx.send(read_bounded(stderr, MAX_STDERR_BYTES));
        });

        let deadline = Instant::now() + APP_SERVER_TIMEOUT;
        let result = run_archive_protocol(&mut stdin, &stdout_rx, deadline, &thread_id);
        drop(stdin);

        match result {
            Ok(()) => Ok(CodexArchiveRetryReport {
                thread_id,
                archived: true,
                transport: "appServer".into(),
                message: "Codex App Serverでアーカイブを再実行しました".into(),
            }),
            Err(message) => {
                let stderr_bytes = stderr_rx
                    .recv_timeout(Duration::from_millis(25))
                    .unwrap_or_default();
                let suffix = if stderr_bytes.is_empty() {
                    String::new()
                } else {
                    format!(" (App Server stderr: {} bytes captured)", stderr_bytes.len())
                };
                Ok(failed_report(thread_id, format!("{message}{suffix}")))
            }
        }
    })();

    // command-group maps this to a POSIX process group on macOS. Kill even
    // after the leader exits so inherited stdio holders cannot survive.
    let _ = child.kill();
    let _ = child.wait();
    outcome
}

fn run_archive_protocol(
    stdin: &mut ChildStdin,
    stdout: &Receiver<StdoutEvent>,
    deadline: Instant,
    thread_id: &str,
) -> Result<(), String> {
    send_json(
        stdin,
        &json!({
            "method": "initialize",
            "id": 1,
            "params": {
                "clientInfo": {
                    "name": "codetas-desktop",
                    "title": "CODETAS Desktop",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        }),
    )?;
    wait_for_response(stdout, 1, deadline)
        .map_err(|message| format!("App Server初期化に失敗しました: {message}"))?;

    send_json(
        stdin,
        &json!({
            "method": "initialized",
            "params": {}
        }),
    )?;

    send_json(
        stdin,
        &json!({
            "method": "thread/archive",
            "id": 2,
            "params": { "threadId": thread_id }
        }),
    )?;
    wait_for_response(stdout, 2, deadline)
        .map(|_| ())
        .map_err(|message| format!("thread/archiveに失敗しました: {message}"))
}

/// Updates pin metadata only after `thread/read` has confirmed that the task
/// exists. This is intentionally not part of archive retry; callers must opt in
/// to changing pin state separately.
#[allow(dead_code)]
fn unpin_existing_thread(
    stdin: &mut ChildStdin,
    stdout: &Receiver<StdoutEvent>,
    deadline: Instant,
    thread_id: &str,
) -> Result<(), String> {
    if !is_uuid(thread_id) {
        return Err("threadId must be a UUID in 8-4-4-4-12 form".into());
    }
    send_json(
        stdin,
        &json!({
            "method": "thread/read",
            "id": 10,
            "params": { "threadId": thread_id, "includeTurns": false }
        }),
    )?;
    wait_for_response(stdout, 10, deadline)
        .map_err(|message| format!("thread/readに失敗しました: {message}"))?;

    send_json(
        stdin,
        &json!({
            "method": "thread/metadata/update",
            "id": 11,
            "params": { "threadId": thread_id, "isPinned": false }
        }),
    )?;
    wait_for_response(stdout, 11, deadline)
        .map(|_| ())
        .map_err(|message| format!("thread/metadata/updateに失敗しました: {message}"))
}

fn send_json(stdin: &mut ChildStdin, value: &JsonValue) -> Result<(), String> {
    serde_json::to_writer(&mut *stdin, value)
        .map_err(|error| format!("App Server要求をJSON化できませんでした: {error}"))?;
    stdin
        .write_all(b"\n")
        .and_then(|_| stdin.flush())
        .map_err(|error| format!("App Server要求を送信できませんでした: {error}"))
}

fn wait_for_response(
    stdout: &Receiver<StdoutEvent>,
    request_id: u64,
    deadline: Instant,
) -> Result<JsonValue, String> {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err("応答がタイムアウトしました".into());
        }
        let remaining = deadline.saturating_duration_since(now);
        let wait = remaining.min(RESPONSE_POLL_INTERVAL);
        match stdout.recv_timeout(wait) {
            Ok(StdoutEvent::Line(line)) => {
                let message: JsonValue = serde_json::from_slice(&line)
                    .map_err(|_| "stdoutに不正なJSONL応答がありました".to_string())?;
                if message.get("id").and_then(JsonValue::as_u64) != Some(request_id) {
                    continue;
                }
                if let Some(error) = message.get("error") {
                    return Err(server_error_message(error));
                }
                return message
                    .get("result")
                    .cloned()
                    .ok_or_else(|| "応答にresultがありません".to_string());
            }
            Ok(StdoutEvent::LimitExceeded(reason)) => return Err(reason.into()),
            Ok(StdoutEvent::ReadFailed) => return Err("stdoutの読み取りに失敗しました".into()),
            Ok(StdoutEvent::Eof) => return Err("応答前にApp Serverが終了しました".into()),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                return Err("App Serverのstdoutが切断されました".into());
            }
        }
    }
}

fn server_error_message(error: &JsonValue) -> String {
    let message = error
        .get("message")
        .and_then(JsonValue::as_str)
        .unwrap_or("App Serverがエラーを返しました");
    truncate_chars(message, MAX_SERVER_MESSAGE_CHARS)
}

fn read_stdout_jsonl<R: Read>(mut reader: R, sender: SyncSender<StdoutEvent>) {
    let mut buffer = [0_u8; 4096];
    let mut line = Vec::new();
    let mut total_bytes = 0_usize;
    let mut line_count = 0_usize;
    let mut line_overflow = false;

    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => {
                if !line.is_empty() || line_overflow {
                    if line_overflow {
                        let _ = sender.send(StdoutEvent::LimitExceeded(
                            "App ServerのJSONL応答行が上限を超えました",
                        ));
                    } else {
                        let _ = sender.send(StdoutEvent::Line(line));
                    }
                }
                let _ = sender.send(StdoutEvent::Eof);
                return;
            }
            Ok(read) => read,
            Err(_) => {
                let _ = sender.send(StdoutEvent::ReadFailed);
                return;
            }
        };
        total_bytes = total_bytes.saturating_add(read);
        if total_bytes > MAX_STDOUT_TOTAL_BYTES {
            let _ = sender.send(StdoutEvent::LimitExceeded(
                "App Serverのstdout合計が上限を超えました",
            ));
            return;
        }

        for &byte in &buffer[..read] {
            if byte == b'\n' {
                line_count = line_count.saturating_add(1);
                if line_count > MAX_STDOUT_LINES {
                    let _ = sender.send(StdoutEvent::LimitExceeded(
                        "App ServerのJSONL応答数が上限を超えました",
                    ));
                    return;
                }
                if line_overflow {
                    let _ = sender.send(StdoutEvent::LimitExceeded(
                        "App ServerのJSONL応答行が上限を超えました",
                    ));
                    return;
                }
                if !line.is_empty() {
                    if sender.send(StdoutEvent::Line(std::mem::take(&mut line))).is_err() {
                        return;
                    }
                }
                line_overflow = false;
            } else if !line_overflow {
                if line.len() < MAX_STDOUT_LINE_BYTES {
                    line.push(byte);
                } else {
                    line_overflow = true;
                }
            }
        }
    }
}

fn read_bounded<R: Read>(mut reader: R, limit: usize) -> Vec<u8> {
    let mut kept = Vec::with_capacity(limit.min(8 * 1024));
    let mut buffer = [0_u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => return kept,
            Ok(read) => {
                let remaining = limit.saturating_sub(kept.len());
                kept.extend_from_slice(&buffer[..read.min(remaining)]);
            }
        }
    }
}

fn failed_report(thread_id: String, message: impl Into<String>) -> CodexArchiveRetryReport {
    let message = sanitize_message(&message.into());
    CodexArchiveRetryReport {
        thread_id,
        archived: false,
        transport: "appServer".into(),
        message: truncate_chars(&message, MAX_SERVER_MESSAGE_CHARS),
    }
}

fn find_codex_cli() -> Option<PathBuf> {
    let bundled = PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex");
    let canonical = std::fs::canonicalize(&bundled).ok()?;
    let metadata = std::fs::metadata(&canonical).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    metadata.is_file().then_some(canonical)
}

fn sanitize_message(value: &str) -> String {
    let mut output = value.to_string();
    if let Some(home) = dirs::home_dir() {
        output = output.replace(home.to_string_lossy().as_ref(), "~");
    }
    for marker in ["Bearer ", "token=", "api_key=", "apikey="] {
        if let Some(index) = output.to_ascii_lowercase().find(&marker.to_ascii_lowercase()) {
            output.truncate(index);
            output.push_str("<redacted>");
        }
    }
    output
        .split('/')
        .map(|component| {
            if component.contains('@') && component.contains('.') {
                "<redacted-email>"
            } else {
                component
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn is_uuid(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }
    value.as_bytes().iter().enumerate().all(|(index, byte)| {
        if matches!(index, 8 | 13 | 18 | 23) {
            *byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_uuid_shape() {
        assert!(is_uuid("01a00463-b3a0-7e30-a11f-cea53284244d"));
        assert!(!is_uuid("01a00463-b3a0-7e30-a11f"));
        assert!(!is_uuid("01a00463_b3a0_7e30_a11f_cea53284244d"));
    }

    #[test]
    fn truncates_server_messages() {
        assert_eq!(truncate_chars("abc", 3), "abc");
        assert_eq!(truncate_chars("abcd", 3), "abc…");
    }
}
