use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

const PROCESS_WAIT: Duration = Duration::from_secs(10);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexShutdownResult {
    requested: bool,
    stopped: bool,
    remaining_pids: Vec<u32>,
    message: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexRestartResult {
    requested: bool,
    started: bool,
    process_ids: Vec<u32>,
    message: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexWriterActionInput {
    pid: u32,
    expected_started_at: Option<String>,
    thread_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexWriterActionResult {
    pid: u32,
    requested: bool,
    stopped: bool,
    message: String,
}

#[derive(Clone)]
struct ProcessIdentity {
    pid: u32,
    started_at: String,
    terminal: String,
    command: String,
}

#[tauri::command]
pub async fn request_codex_shutdown() -> Result<CodexShutdownResult, String> {
    tauri::async_runtime::spawn_blocking(request_codex_shutdown_blocking)
        .await
        .map_err(|error| format!("Codex終了要求を完了できませんでした: {error}"))?
}

#[tauri::command]
pub async fn restart_codex() -> Result<CodexRestartResult, String> {
    tauri::async_runtime::spawn_blocking(restart_codex_blocking)
        .await
        .map_err(|error| format!("Codex再起動を完了できませんでした: {error}"))?
}

fn restart_codex_blocking() -> Result<CodexRestartResult, String> {
    if !cfg!(target_os = "macos") {
        return Err("Codexの再起動支援は現在macOS専用です。".into());
    }
    let current = codex_processes()?
        .into_iter()
        .filter(|process| is_codex_gui_main(&process.command))
        .collect::<Vec<_>>();
    if !current.is_empty() {
        return Ok(CodexRestartResult {
            requested: false,
            started: true,
            process_ids: current.into_iter().map(|process| process.pid).collect(),
            message: "Codexは既に起動しています。".into(),
        });
    }
    let output = crate::provider_gateway::cli_scan::command_output(
        Path::new("/usr/bin/open"),
        &["-b", "com.openai.codex"],
        Duration::from_secs(5),
    )
    .ok_or("Codexの起動要求がタイムアウトしました。")?;
    if !output.success {
        return Err("Codexの起動要求が失敗しました。".into());
    }
    let deadline = Instant::now() + PROCESS_WAIT;
    loop {
        let processes = codex_processes()?
            .into_iter()
            .filter(|process| is_codex_gui_main(&process.command))
            .collect::<Vec<_>>();
        if !processes.is_empty() {
            return Ok(CodexRestartResult {
                requested: true,
                started: true,
                process_ids: processes.into_iter().map(|process| process.pid).collect(),
                message: "Codexの起動を確認しました。タスク一覧はCodex側で確認してください。".into(),
            });
        }
        if Instant::now() >= deadline {
            return Ok(CodexRestartResult {
                requested: true,
                started: false,
                process_ids: Vec::new(),
                message: "Codexへ起動を要求しましたが、制限時間内に関連プロセスを確認できませんでした。".into(),
            });
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn request_codex_shutdown_blocking() -> Result<CodexShutdownResult, String> {
    if !cfg!(target_os = "macos") {
        return Err("Codexの通常終了支援は現在macOS専用です。".into());
    }
    let before = codex_processes()?;
    if before.is_empty() {
        return Ok(CodexShutdownResult {
            requested: false,
            stopped: true,
            remaining_pids: Vec::new(),
            message: "Codex関連プロセスは既に停止しています。".into(),
        });
    }
    if !before.iter().any(|process| is_codex_gui_main(&process.command)) {
        return Ok(CodexShutdownResult {
            requested: false,
            stopped: false,
            remaining_pids: before.into_iter().map(|process| process.pid).collect(),
            message: "Codex Desktopは起動していません。残っているCLIプロセスを誤って終了しないため、AppleScriptは実行しませんでした。".into(),
        });
    }

    let output = crate::provider_gateway::cli_scan::command_output(
        Path::new("/usr/bin/osascript"),
        &[
            "-e",
            "tell application id \"com.openai.codex\" to quit",
        ],
        Duration::from_secs(5),
    )
    .ok_or("Codexへの通常終了要求がタイムアウトしました。強制終了は行っていません。")?;
    if !output.success {
        return Err("Codexへの通常終了要求が失敗しました。強制終了は行っていません。".into());
    }

    let deadline = Instant::now() + PROCESS_WAIT;
    loop {
        let remaining = codex_processes()?;
        if remaining.is_empty() {
            return Ok(CodexShutdownResult {
                requested: true,
                stopped: true,
                remaining_pids: Vec::new(),
                message: "Codexへ通常終了を要求し、関連プロセスの停止を確認しました。".into(),
            });
        }
        if Instant::now() >= deadline {
            return Ok(CodexShutdownResult {
                requested: true,
                stopped: false,
                remaining_pids: remaining.into_iter().map(|process| process.pid).collect(),
                message: "Codexへ通常終了を要求しましたが、一部の関連プロセスが残っています。強制終了は行っていません。".into(),
            });
        }
        thread::sleep(Duration::from_millis(250));
    }
}

#[tauri::command]
pub async fn terminate_codex_writer(
    input: CodexWriterActionInput,
) -> Result<CodexWriterActionResult, String> {
    tauri::async_runtime::spawn_blocking(move || terminate_codex_writer_blocking(input))
        .await
        .map_err(|error| format!("書き込みプロセスの終了支援を完了できませんでした: {error}"))?
}

fn terminate_codex_writer_blocking(
    input: CodexWriterActionInput,
) -> Result<CodexWriterActionResult, String> {
    if input.pid == 0 || input.pid == std::process::id() {
        return Err("終了対象のPIDが安全条件を満たしていません。".into());
    }
    if !looks_like_thread_id(&input.thread_id) {
        return Err("タスクIDの形式が正しくありません。".into());
    }
    let identity = process_identity(input.pid)?
        .ok_or_else(|| "対象プロセスは既に終了しています。".to_string())?;
    if !is_codex_cli_writer(&identity) {
        return Err("対象PIDはターミナルで動作するCodex CLIではないため終了しません。Codex DesktopやRendererはこの操作の対象外です。".into());
    }
    let expected = input
        .expected_started_at
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or("診断時の起動時刻がないため、PIDの再利用を安全に判定できません。再診断してください。")?;
    if identity.started_at != expected {
        return Err("PIDが再利用された可能性があるため終了しません。起動時刻が診断時と一致しません。".into());
    }

    let session = find_session_file(&input.thread_id)?
        .ok_or_else(|| "対象タスクのセッションファイルを確認できません。".to_string())?;
    if !pid_holds_path(input.pid, &session)? {
        return Err("対象PIDがこのタスクを現在も使用していることを確認できないため終了しません。".into());
    }

    let pid_text = input.pid.to_string();
    let output = crate::provider_gateway::cli_scan::command_output(
        Path::new("/bin/kill"),
        &["-TERM", &pid_text],
        Duration::from_secs(5),
    )
    .ok_or("通常終了シグナルの送信がタイムアウトしました。強制終了は行っていません。")?;
    if !output.success {
        return Err("通常終了シグナルの送信に失敗しました。強制終了は行っていません。".into());
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if process_identity(input.pid)?.is_none() {
            return Ok(CodexWriterActionResult {
                pid: input.pid,
                requested: true,
                stopped: true,
                message: "対象のCodexプロセスへ通常終了を要求し、停止を確認しました。".into(),
            });
        }
        if Instant::now() >= deadline {
            return Ok(CodexWriterActionResult {
                pid: input.pid,
                requested: true,
                stopped: false,
                message: "通常終了を要求しましたが、プロセスはまだ動作中です。強制終了は行っていません。".into(),
            });
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn codex_processes() -> Result<Vec<ProcessIdentity>, String> {
    let output = crate::provider_gateway::cli_scan::command_output(
        Path::new("/bin/ps"),
        &["-axo", "pid=,lstart=,tty=,comm="],
        Duration::from_secs(5),
    )
    .ok_or("プロセス一覧の取得がタイムアウトしました。")?;
    if !output.success {
        return Err("プロセス一覧の取得に失敗しました。".into());
    }
    Ok(output
        .text
        .lines()
        .filter_map(parse_process_identity)
        .filter(|process| is_codex_command(&process.command))
        .collect())
}

fn process_identity(pid: u32) -> Result<Option<ProcessIdentity>, String> {
    Ok(codex_processes()?.into_iter().find(|process| process.pid == pid))
}

fn parse_process_identity(line: &str) -> Option<ProcessIdentity> {
    let parts = line.split_whitespace().collect::<Vec<_>>();
    if parts.len() < 8 {
        return None;
    }
    Some(ProcessIdentity {
        pid: parts[0].parse().ok()?,
        started_at: parts[1..6].join(" "),
        terminal: parts[6].to_string(),
        command: parts[7..].join(" "),
    })
}

fn is_codex_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    if lower.contains("codetas") || lower.contains("opencodex") {
        return false;
    }
    let executable = Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command)
        .to_ascii_lowercase();
    executable == "codex" || lower.contains("/applications/chatgpt.app/contents/")
}

fn is_codex_gui_main(command: &str) -> bool {
    command.eq_ignore_ascii_case("/Applications/ChatGPT.app/Contents/MacOS/ChatGPT")
}

fn is_codex_cli_writer(process: &ProcessIdentity) -> bool {
    Path::new(&process.command)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("codex"))
        && !matches!(process.terminal.as_str(), "??" | "-")
}

fn find_session_file(thread_id: &str) -> Result<Option<PathBuf>, String> {
    let root = dirs::home_dir()
        .ok_or_else(|| "ホームディレクトリを解決できません。".to_string())?
        .join(".codex/sessions");
    let mut pending = vec![root.clone()];
    let suffix = format!("{thread_id}.jsonl");
    let mut visited = 0usize;
    let deadline = Instant::now() + Duration::from_secs(5);
    while let Some(directory) = pending.pop() {
        if Instant::now() >= deadline {
            return Err("セッション一覧の確認が5秒でタイムアウトしました。再診断してください。".into());
        }
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("セッション一覧を確認できません: {error}")),
        };
        for entry in entries {
            if Instant::now() >= deadline {
                return Err("セッション一覧の確認が5秒でタイムアウトしました。再診断してください。".into());
            }
            let entry = entry.map_err(|error| format!("セッション項目を確認できません: {error}"))?;
            visited += 1;
            if visited > 100_000 {
                return Err("セッション一覧が安全な走査上限を超えました。".into());
            }
            let file_type = entry
                .file_type()
                .map_err(|error| format!("セッション項目の種類を確認できません: {error}"))?;
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file()
                && path.file_name().and_then(|name| name.to_str()) == Some(suffix.as_str())
            {
                let canonical = std::fs::canonicalize(&path)
                    .map_err(|error| format!("セッションパスを正規化できません: {error}"))?;
                let canonical_root = std::fs::canonicalize(&root)
                    .map_err(|error| format!("セッション保存先を正規化できません: {error}"))?;
                if canonical.starts_with(canonical_root) {
                    return Ok(Some(canonical));
                }
            }
        }
    }
    Ok(None)
}

fn pid_holds_path(pid: u32, path: &Path) -> Result<bool, String> {
    let pid_text = pid.to_string();
    let path_text = path.to_string_lossy().into_owned();
    let output = crate::provider_gateway::cli_scan::command_output(
        Path::new("/usr/sbin/lsof"),
        &["-Fpn", "-a", "-p", &pid_text, "--", &path_text],
        Duration::from_secs(5),
    )
    .ok_or("使用中ファイルの確認がタイムアウトしました。")?;
    if !output.success {
        return if output.text.trim().is_empty() {
            Ok(false)
        } else {
            Err("使用中ファイルの確認に失敗しました。対象プロセスは終了しません。".into())
        };
    }
    let expected = path.to_string_lossy();
    let mut saw_pid = false;
    let mut saw_path_for_pid = false;
    for line in output.text.lines() {
        if let Some(value) = line.strip_prefix('p') {
            saw_pid = value.parse::<u32>().ok() == Some(pid);
        } else if saw_pid && line.strip_prefix('n') == Some(expected.as_ref()) {
            saw_path_for_pid = true;
        }
    }
    Ok(saw_path_for_pid)
}

fn looks_like_thread_id(value: &str) -> bool {
    value.len() == 36
        && value
            .chars()
            .enumerate()
            .all(|(index, character)| match index {
                8 | 13 | 18 | 23 => character == '-',
                _ => character.is_ascii_hexdigit(),
            })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_filter_excludes_codetas_and_opencodex() {
        assert!(is_codex_command("/Applications/ChatGPT.app/Contents/Resources/codex"));
        assert!(is_codex_command("/Applications/ChatGPT.app/Contents/MacOS/ChatGPT"));
        assert!(is_codex_gui_main("/Applications/ChatGPT.app/Contents/MacOS/ChatGPT"));
        assert!(!is_codex_gui_main("/Applications/ChatGPT.app/Contents/Resources/codex"));
        assert!(!is_codex_command("/Applications/CODETAS.app/Contents/MacOS/codetas-desktop"));
        assert!(!is_codex_command("/tmp/opencodex/node_modules/bun"));
    }

    #[test]
    fn thread_id_validation_is_strict() {
        assert!(looks_like_thread_id("019ffb9c-7595-7022-a9de-81be2d15578c"));
        assert!(!looks_like_thread_id("../../sessions/example"));
        assert!(!looks_like_thread_id("019ffb9c75957022a9de81be2d15578c"));
    }
}
