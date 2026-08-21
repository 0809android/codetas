//! Sidecar learning runtime for live Codex session transcripts.
//!
//! Codex remains the primary conversation. CODETAS Desktop watches
//! `~/.codex/sessions` for live rollout JSONL files and starts one Python
//! sidecar per live thread. The sidecar reads the transcript and writes the
//! bound Hermes profile. When the Codex session is no longer live, the
//! sidecar is asked to stop and then dropped.

use crate::provider_gateway::{self, GatewayManager};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager};
use tokio::{process::Command, time::sleep};

const POLL: Duration = Duration::from_secs(3);
const LIVE_WINDOW: Duration = Duration::from_secs(45 * 60);
const MAX_WALK_ENTRIES: usize = 2_000;
const MAX_LIVE_SESSIONS: usize = 4;
const STOP_GRACE_POLLS: u8 = 3;

#[derive(Default)]
pub struct LearningSupervisor {
    inner: tokio::sync::Mutex<SupervisorState>,
}

#[derive(Default)]
struct SupervisorState {
    children: HashMap<String, TrackedSidecar>,
}

struct SidecarLease {
    session_id: String,
    pid: u32,
    nonce: String,
    started_at: u64,
}

struct TrackedSidecar {
    child: tokio::process::Child,
    stop_polls: u8,
    lease: SidecarLease,
}

impl Drop for TrackedSidecar {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        // Leave a live matching lease in place so plugin Stop fallback
        // cannot race a sidecar that is still exiting.
        unclaim_sidecar(&self.lease);
    }
}

pub(crate) fn start_learning_runtime(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            if let Err(error) = tick(&app).await {
                eprintln!("CODETAS learning runtime: {error}");
            }
            sleep(POLL).await;
        }
    });
}

async fn tick(app: &AppHandle) -> Result<(), String> {
    let Some(script) = learning_script_path() else {
        return Ok(());
    };
    let live = discover_live_sessions()?;
    let supervisor = app.state::<LearningSupervisor>();
    let mut state = supervisor.inner.lock().await;
    let live_ids = live
        .iter()
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();

    let running_ids = state.children.keys().cloned().collect::<Vec<_>>();
    for id in running_ids {
        if live_ids.iter().any(|live_id| live_id == &id) {
            if let Some(tracked) = state.children.get_mut(&id) {
                tracked.stop_polls = 0;
                if let Ok(Some(status)) = tracked.child.try_wait() {
                    if !status.success() {
                        eprintln!(
                            "CODETAS learning sidecar {id} exited with {status}"
                        );
                    }
                    state.children.remove(&id);
                }
            }
            continue;
        }
        let Some(tracked) = state.children.get_mut(&id) else {
            continue;
        };
        write_stop_file(&id);
        tracked.stop_polls = tracked.stop_polls.saturating_add(1);
        if tracked.stop_polls >= STOP_GRACE_POLLS {
            let _ = tracked.child.start_kill();
            state.children.remove(&id);
        }
    }

    for (id, jsonl) in live {
        if sidecar_is_finished(&id) {
            continue;
        }
        if state.children.contains_key(&id) {
            continue;
        }
        if state.children.len() >= MAX_LIVE_SESSIONS {
            break;
        }
        match spawn_sidecar(app, &script, &id, &jsonl).await {
            Ok((child, lease)) => {
                state.children.insert(
                    id,
                    TrackedSidecar {
                        child,
                        stop_polls: 0,
                        lease,
                    },
                );
            }
            Err(error) => eprintln!("CODETAS learning sidecar {id}: {error}"),
        }
    }
    Ok(())
}

async fn spawn_sidecar(
    app: &AppHandle,
    script: &Path,
    session_id: &str,
    jsonl: &Path,
) -> Result<(tokio::process::Child, SidecarLease), String> {
    let log_path = sidecar_log_path(session_id)?;
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("学習ログフォルダを作れません: {error}"))?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|error| format!("学習ログを開けません: {error}"))?;
    let err = log
        .try_clone()
        .map_err(|error| format!("学習ログを複製できません: {error}"))?;
    let mut command = Command::new(python_bin());
    command
        .arg(script)
        .arg(session_id)
        .arg(jsonl)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(err);
    let dir = learning_state_dir()?;
    command.env("CODETAS_LEARNING_STATE_DIR", &dir);
    let settings = provider_gateway::presets::gateway_configuration(app.clone())?;
    let observed = provider_gateway::observe_gateway_runtime(
        app,
        &app.state::<GatewayManager>(),
        &settings,
    )
    .await?;
    if !observed.running {
        return Err("Gatewayが停止中のため学習sidecarを起動しません".into());
    }
    let Some(runtime_url) = provider_gateway::runtime_gateway_url(app) else {
        return Err("Gateway URLを確認できないため学習sidecarを起動しません".into());
    };
    if runtime_url.is_empty() {
        return Err("Gateway URLを確認できないため学習sidecarを起動しません".into());
    }
    command.env(
        "CODETAS_LEARNING_GATEWAY_URL",
        &runtime_url,
    );
    command.env_remove("CODETAS_CLIENT_TOKEN");
    command.env_remove("CODETAS_GATEWAY_TOKEN");
    command.env_remove("CODETAS_LEARNING_GATEWAY_TOKEN");
    match command.spawn() {
        Ok(mut child) => {
            let Some(pid) = child.id() else {
                let _ = child.start_kill();
                return Err("学習sidecarのPIDを取得できません".into());
            };
            match claim_sidecar(session_id, pid) {
                Ok(lease) => Ok((child, lease)),
                Err(error) => {
                    let _ = child.start_kill();
                    Err(error)
                }
            }
        }
        Err(error) => Err(format!("python を起動できません: {error}")),
    }
}

fn python_bin() -> &'static str {
    if cfg!(windows) {
        "python"
    } else {
        "python3"
    }
}

pub(crate) fn learning_script_path() -> Option<PathBuf> {
    if let Ok(root) = std::env::var("CODETAS_PLUGIN_ROOT") {
        let script = PathBuf::from(root)
            .join("scripts")
            .join("session_learning_runtime.py");
        if script.is_file() {
            return Some(script);
        }
    }
    if let Some(root) = provider_gateway::find_codetas_plugin_root("codetas") {
        let script = root.join("scripts").join("session_learning_runtime.py");
        if script.is_file() {
            return Some(script);
        }
    }
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../plugins/codetas/scripts/session_learning_runtime.py");
    dev.is_file().then_some(dev)
}

fn learning_state_dir() -> Result<PathBuf, String> {
    if let Ok(value) = std::env::var("CODETAS_LEARNING_STATE_DIR") {
        if !value.is_empty() {
            return Ok(PathBuf::from(value));
        }
    }
    crate::provider_gateway::codex_home().map(|home| home.join("codetas-learning"))
}

fn sidecar_log_path(session_id: &str) -> Result<PathBuf, String> {
    Ok(learning_state_dir()?.join("sidecars").join(format!("{session_id}.log")))
}

fn sidecar_is_finished(session_id: &str) -> bool {
    learning_state_dir()
        .map(|dir| dir.join("sidecars").join(format!("{session_id}.finished")).is_file())
        .unwrap_or(false)
}

fn process_is_live(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        let output = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "pid="])
            .output();
        match output {
            Ok(output) => String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .any(|value| value == pid.to_string()),
            Err(_) => true,
        }
    }
    #[cfg(windows)]
    {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output();
        match output {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout.lines().any(|line| {
                    line.split(',').nth(1).map(|value| value.trim_matches('"') == pid.to_string()).unwrap_or(false)
                })
            }
            Err(_) => true,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        true
    }
}

fn read_lease_file(path: &Path) -> Option<SidecarLease> {
    let raw = fs::read_to_string(path).ok()?;
    let pid = raw
        .split("\"pid\":")
        .nth(1)?
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()?;
    let nonce = raw.split("\"nonce\":\"").nth(1)?.split('"').next()?.to_string();
    let started_at = raw
        .split("\"started_at\":")
        .nth(1)?
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()?;
    if nonce.is_empty() {
        return None;
    }
    Some(SidecarLease {
        session_id: path
            .file_name()?
            .to_str()?
            .strip_suffix(".claimed")?
            .to_string(),
        pid,
        nonce,
        started_at,
    })
}

fn claim_sidecar(session_id: &str, pid: u32) -> Result<SidecarLease, String> {
    if !looks_like_session_id(session_id) {
        return Err("session id が不正です".into());
    }
    if pid == 0 {
        return Err("学習sidecarのPIDが不正です".into());
    }
    let dir = learning_state_dir()?.join("sidecars");
    fs::create_dir_all(&dir).map_err(|error| format!("学習状態フォルダを作れません: {error}"))?;
    let claimed = dir.join(format!("{session_id}.claimed"));
    if claimed.is_file() {
        match read_lease_file(&claimed) {
            Some(existing) if process_is_live(existing.pid) => {
                return Err("学習sidecarはすでに所有されています".into());
            }
            Some(existing) => {
                if !remove_matching_lease(&claimed, &existing) {
                    return Err("学習sidecarの所有記録が競合しました".into());
                }
            }
            None => {
                if claimed.is_file() && !remove_stale_unreadable_lease(&claimed) {
                    return Err("学習sidecarの所有記録が競合しました".into());
                }
            }
        }
    }
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let nonce = format!("{pid:x}-{started_at:x}");
    let payload = format!("{{\"pid\":{pid},\"nonce\":\"{nonce}\",\"started_at\":{started_at}}}\n");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    match options.open(&claimed) {
        Ok(mut file) => {
            if let Err(error) = file.write_all(payload.as_bytes()) {
                let _ = fs::remove_file(&claimed);
                return Err(format!("学習sidecarの所有を記録できません: {error}"));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if let Some(existing) = read_lease_file(&claimed) {
                if process_is_live(existing.pid) {
                    return Err("学習sidecarはすでに所有されています".into());
                }
            }
            return Err("学習sidecarの所有記録が競合しました".into());
        }
        Err(error) => {
            return Err(format!("学習sidecarの所有を記録できません: {error}"));
        }
    }
    Ok(SidecarLease {
        session_id: session_id.to_string(),
        pid,
        nonce,
        started_at,
    })
}

fn leases_match(left: &SidecarLease, right: &SidecarLease) -> bool {
    left.session_id == right.session_id
        && left.pid == right.pid
        && left.nonce == right.nonce
        && left.started_at == right.started_at
}

fn remove_matching_lease(path: &Path, expected: &SidecarLease) -> bool {
    let Some(current) = read_lease_file(path) else {
        return !path.is_file();
    };
    if !leases_match(&current, expected) || process_is_live(current.pid) {
        return false;
    }
    let staging = path.with_extension("claimed.stale");
    match fs::rename(path, &staging) {
        Ok(()) => {
            if let Some(moved) = read_lease_file(&staging) {
                if leases_match(&moved, expected) && !process_is_live(moved.pid) {
                    let _ = fs::remove_file(&staging);
                    return true;
                }
            }
            let _ = fs::rename(&staging, path);
            false
        }
        Err(_) => false,
    }
}

fn remove_stale_unreadable_lease(path: &Path) -> bool {
    if read_lease_file(path).is_some() {
        return false;
    }
    let staging = path.with_extension("claimed.stale");
    match fs::rename(path, &staging) {
        Ok(()) => {
            if read_lease_file(&staging).is_some() {
                let _ = fs::rename(&staging, path);
                return false;
            }
            let _ = fs::remove_file(&staging);
            true
        }
        Err(_) => false,
    }
}

fn unclaim_sidecar(lease: &SidecarLease) {
    if !looks_like_session_id(&lease.session_id) {
        return;
    }
    let Ok(dir) = learning_state_dir() else {
        return;
    };
    let path = dir.join("sidecars").join(format!("{}.claimed", lease.session_id));
    let _ = remove_matching_lease(&path, lease);
}

fn write_stop_file(session_id: &str) {
    if !looks_like_session_id(session_id) {
        return;
    }
    let Ok(dir) = learning_state_dir() else {
        return;
    };
    let path = dir.join("sidecars").join(format!("{session_id}.stop"));
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, b"stop\n");
}

fn looks_like_session_id(value: &str) -> bool {
    value.len() == 36
        && value.as_bytes().iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn session_id_from_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".jsonl")?;
    let candidate = stem.get(stem.len().checked_sub(36)?..)?;
    looks_like_session_id(candidate).then(|| candidate.to_ascii_lowercase())
}

fn discover_live_sessions() -> Result<Vec<(String, PathBuf)>, String> {
    let root = crate::provider_gateway::codex_home()?.join("sessions");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let now = SystemTime::now();
    let mut pending = vec![root];
    let mut seen = 0usize;
    let mut live: Vec<(String, PathBuf, SystemTime)> = Vec::new();
    while let Some(directory) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > MAX_WALK_ENTRIES {
                live.sort_by(|left, right| right.2.cmp(&left.2));
                live.truncate(MAX_LIVE_SESSIONS);
                return Ok(live
                    .into_iter()
                    .map(|(id, path, _)| (id, path))
                    .collect());
            }
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let Some(id) = session_id_from_path(&path) else {
                continue;
            };
            let Ok(mtime) = metadata.modified() else {
                continue;
            };
            let Ok(age) = now.duration_since(mtime) else {
                continue;
            };
            if age <= LIVE_WINDOW && metadata.len() > 0 {
                live.push((id, path, mtime));
            }
        }
    }
    live.sort_by(|left, right| right.2.cmp(&left.2));
    live.truncate(MAX_LIVE_SESSIONS);
    Ok(live.into_iter().map(|(id, path, _)| (id, path)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn session_id_is_taken_from_rollout_filename() {
        let path = Path::new(
            "sessions/2026/08/20/rollout-2026-08-20T00-00-00-01a002ab-772a-7553-b882-d2675d3d6ee6.jsonl",
        );
        assert_eq!(
            session_id_from_path(path).as_deref(),
            Some("01a002ab-772a-7553-b882-d2675d3d6ee6")
        );
        assert!(session_id_from_path(Path::new("notes.jsonl")).is_none());
        assert!(looks_like_session_id("01a002ab-772a-7553-b882-d2675d3d6ee6"));
        assert!(!looks_like_session_id("../escape"));
    }

    #[test]
    fn stop_file_name_requires_a_uuid_session_id() {
        assert!(looks_like_session_id("01a002ab-772a-7553-b882-d2675d3d6ee6"));
        assert!(!looks_like_session_id("../escape"));
        assert!(!looks_like_session_id("01a002ab-772a-7553-b882-d2675d3d6ee6/../x"));
    }
}
