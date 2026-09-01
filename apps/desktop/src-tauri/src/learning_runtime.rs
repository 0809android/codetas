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
use tokio::{io::AsyncWriteExt, process::Command, time::sleep};

const POLL: Duration = Duration::from_secs(3);
const LIVE_WINDOW: Duration = Duration::from_secs(45 * 60);
const MAX_WALK_ENTRIES: usize = 2_000;
const MAX_LIVE_SESSIONS: usize = 4;
const STOP_GRACE_POLLS: u8 = 3;
const START_GATE_PROTOCOL: &str = "stdin-v1";
const START_GATE_RELEASE: &[u8] = b"start\n";

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
        // Keep the matching lease until the process is observed dead so
        // plugin Stop fallback cannot inject a Codex turn mid-exit.
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

fn self_improvement_mode_enabled(app: &AppHandle) -> bool {
    provider_gateway::presets::gateway_configuration(app.clone())
        .ok()
        .is_some_and(|settings| settings.codex.self_improvement_mode)
}

async fn gateway_runtime_available(app: &AppHandle) -> bool {
    let Ok(settings) = provider_gateway::presets::gateway_configuration(app.clone()) else {
        return false;
    };
    let Ok(observed) = provider_gateway::observe_gateway_runtime(
        app,
        &app.state::<GatewayManager>(),
        &settings,
    )
    .await
    else {
        return false;
    };
    if !observed.running {
        return false;
    }
    provider_gateway::runtime_gateway_url(app).is_some_and(|url| !url.is_empty())
}

fn claimed_session_ids() -> Vec<String> {
    let Ok(dir) = learning_state_dir() else {
        return Vec::new();
    };
    let sidecars = dir.join("sidecars");
    let Ok(entries) = fs::read_dir(&sidecars) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let Some(session_id) = name.strip_suffix(".claimed") else {
            continue;
        };
        if looks_like_session_id(session_id) {
            ids.push(session_id.to_string());
        }
    }
    ids
}

fn stop_untracked_sidecars(tracked_ids: &[String]) {
    for session_id in claimed_session_ids() {
        if tracked_ids.iter().any(|tracked| tracked == &session_id) {
            continue;
        }
        write_stop_file(&session_id);
    }
}

fn stop_untracked_finished_sidecars(live_ids: &[String]) {
    for session_id in claimed_session_ids() {
        if live_ids.iter().any(|live_id| live_id == &session_id) {
            continue;
        }
        write_stop_file(&session_id);
    }
}

async fn stop_tracked_sidecars(app: &AppHandle) {
    let supervisor = app.state::<LearningSupervisor>();
    let mut state = supervisor.inner.lock().await;
    let running_ids = state.children.keys().cloned().collect::<Vec<_>>();
    stop_untracked_sidecars(&running_ids);
    for id in running_ids {
        write_pause_file(&id);
        if let Some(tracked) = state.children.get_mut(&id) {
            tracked.stop_polls = tracked.stop_polls.saturating_add(1);
            if tracked.stop_polls >= STOP_GRACE_POLLS {
                write_missed_flush_marker(&id);
                if let Some(mut child) = state.children.remove(&id) {
                    let _ = child.child.start_kill();
                    let _ = child.child.wait().await;
                    unclaim_sidecar(&child.lease);
                }
            }
        }
    }
}

async fn tick(app: &AppHandle) -> Result<(), String> {
    if !self_improvement_mode_enabled(app) {
        clear_self_improvement_enabled_marker();
        stop_tracked_sidecars(app).await;
        return Ok(());
    }
    ensure_self_improvement_enabled()?;
    let live = discover_live_sessions()?;
    if !gateway_runtime_available(app).await {
        stop_tracked_sidecars(app).await;
        return Ok(());
    }
    let Some(script) = learning_script_path() else {
        return Ok(());
    };
    let supervisor = app.state::<LearningSupervisor>();
    let mut state = supervisor.inner.lock().await;
    let live_ids = live
        .iter()
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();

    stop_untracked_finished_sidecars(&live_ids);
    let running_ids = state.children.keys().cloned().collect::<Vec<_>>();
    for id in running_ids {
        if live_ids.iter().any(|live_id| live_id == &id) {
            if let Some(tracked) = state.children.get_mut(&id) {
                tracked.stop_polls = 0;
                clear_pause_file(&id);
                if let Ok(Some(status)) = tracked.child.try_wait() {
                    if !status.success() {
                        eprintln!(
                            "CODETAS learning sidecar {id} exited with {status}"
                        );
                    }
                    if let Some(dead) = state.children.remove(&id) {
                        unclaim_sidecar(&dead.lease);
                    }
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
            write_missed_flush_marker(&id);
            if let Some(mut child) = state.children.remove(&id) {
                let _ = child.child.start_kill();
                let _ = child.child.wait().await;
                unclaim_sidecar(&child.lease);
            }
        }
    }

    for (id, jsonl) in live.into_iter().take(MAX_LIVE_SESSIONS) {
        if sidecar_is_finished(&id) {
            // Only a readable, confirmed-stale marker may be cleared before
            // reclaim. Read failures stay fail-closed even though the broader
            // stale diagnostic treats them as unsafe to trust.
            if !sidecar_finished_is_stale(&id, &jsonl)
                || !sidecar_finished_marker_is_readable(&id)
            {
                continue;
            }
            // Clear before spawn/lease publish so plugin ownership checks do
            // not see .finished while a new live lease already exists.
            clear_finished_file(&id);
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
    let python = python_bin()?;
    let mut command = Command::new(&python.program);
    command
        .args(&python.prefix_args)
        .arg(script)
        .arg(session_id)
        .arg(jsonl)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
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
    if let Ok(token) = std::env::var("CODETAS_GATEWAY_TOKEN") {
        if !token.is_empty() {
            command.env("CODETAS_LEARNING_GATEWAY_TOKEN", token);
        } else {
            command.env_remove("CODETAS_LEARNING_GATEWAY_TOKEN");
        }
    } else {
        command.env_remove("CODETAS_LEARNING_GATEWAY_TOKEN");
    }
    command.env_remove("CODETAS_GATEWAY_TOKEN");
    command.env("CODETAS_LEARNING_START_GATE", START_GATE_PROTOCOL);
    clear_stop_file(session_id);
    clear_pause_file(session_id);
    match command.spawn() {
        Ok(mut child) => {
            let Some(pid) = child.id() else {
                let _ = child.start_kill();
                return Err("学習sidecarのPIDを取得できません".into());
            };
            match claim_sidecar(session_id, pid) {
                Ok(lease) => {
                    if !lease_is_published(&lease) {
                        stop_unstarted_sidecar(&mut child, &lease).await;
                        return Err("学習sidecarの所有記録を検証できません".into());
                    }
                    let Some(mut gate) = child.stdin.take() else {
                        stop_unstarted_sidecar(&mut child, &lease).await;
                        return Err("学習sidecarの開始ゲートを取得できません".into());
                    };
                    if let Err(error) = gate.write_all(START_GATE_RELEASE).await {
                        stop_unstarted_sidecar(&mut child, &lease).await;
                        return Err(format!("学習sidecarの開始ゲートを解除できません: {error}"));
                    }
                    drop(gate);
                    Ok((child, lease))
                }
                Err(error) => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    Err(error)
                }
            }
        }
        Err(error) => Err(format!("python を起動できません: {error}")),
    }
}

async fn stop_unstarted_sidecar(child: &mut tokio::process::Child, lease: &SidecarLease) {
    let _ = child.start_kill();
    let _ = child.wait().await;
    unclaim_sidecar(lease);
}

struct PythonLaunch {
    program: String,
    prefix_args: Vec<String>,
}

fn python_bin() -> Result<PythonLaunch, String> {
    let candidates: Vec<PythonLaunch> = if cfg!(windows) {
        vec![
            PythonLaunch { program: "python".into(), prefix_args: vec![] },
            PythonLaunch { program: "python3".into(), prefix_args: vec![] },
            PythonLaunch { program: "py".into(), prefix_args: vec!["-3".into()] },
        ]
    } else {
        vec![
            PythonLaunch { program: "python3".into(), prefix_args: vec![] },
            PythonLaunch { program: "python".into(), prefix_args: vec![] },
        ]
    };
    for candidate in candidates {
        let mut probe = std::process::Command::new(&candidate.program);
        probe.args(&candidate.prefix_args).args([
            "-c",
            "import sys; raise SystemExit(0 if sys.version_info >= (3, 10) else 1)",
        ]);
        if probe.status().map(|status| status.success()).unwrap_or(false) {
            return Ok(candidate);
        }
    }
    Err("python / python3 が見つかりません".into())
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

fn clear_finished_file(session_id: &str) {
    clear_control_file(session_id, "finished");
}

fn sidecar_finished_marker_is_readable(session_id: &str) -> bool {
    learning_state_dir()
        .and_then(|dir| {
            fs::read_to_string(
                dir.join("sidecars")
                    .join(format!("{session_id}.finished")),
            )
            .map_err(|error| error.to_string())
        })
        .is_ok()
}

fn json_nonneg_u64(value: Option<&serde_json::Value>) -> Option<u64> {
    value.and_then(|item| item.as_u64())
}

fn identity_hex(value: Option<&serde_json::Value>) -> Option<String> {
    let raw = value.and_then(|item| item.as_str())?;
    if raw.len() < 2 || raw.len() > 32 || raw.len() % 2 != 0 {
        return None;
    }
    if !raw.bytes().all(|ch| matches!(ch, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    Some(raw.to_string())
}

fn even_hex(value: u64) -> String {
    let text = format!("{value:x}");
    if text.len() % 2 == 1 {
        format!("0{text}")
    } else {
        text
    }
}

fn sidecar_finished_is_stale(session_id: &str, jsonl: &Path) -> bool {
    let Ok(dir) = learning_state_dir() else {
        return false;
    };
    let finished = dir.join("sidecars").join(format!("{session_id}.finished"));
    let raw = match fs::read_to_string(&finished) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    let jsonl_meta = match fs::metadata(jsonl) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    let Ok(marker) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return true;
    };
    if marker.get("kind").and_then(|value| value.as_str()) != Some("finished") {
        return true;
    }
    if json_nonneg_u64(marker.get("revision")).is_none() {
        return true;
    }
    let Some(boundary) = marker.get("workBoundary") else {
        return true;
    };
    let Some(offset) = json_nonneg_u64(boundary.get("offset")) else {
        return true;
    };
    let Some(top_size) = json_nonneg_u64(marker.get("jsonlSize")) else {
        return true;
    };
    let Some(top_dev) = identity_hex(marker.get("jsonlDev")) else {
        return true;
    };
    let Some(top_ino) = identity_hex(marker.get("jsonlIno")) else {
        return true;
    };
    if jsonl_meta.len() != offset || jsonl_meta.len() != top_size {
        return true;
    }
    if boundary.get("jsonlSize").is_some() {
        match json_nonneg_u64(boundary.get("jsonlSize")) {
            Some(nested_size) if nested_size == top_size => {}
            _ => return true,
        }
    }
    if boundary.get("jsonlDev").is_some() {
        match identity_hex(boundary.get("jsonlDev")) {
            Some(nested_dev) if nested_dev == top_dev => {}
            _ => return true,
        }
    }
    if boundary.get("jsonlIno").is_some() {
        match identity_hex(boundary.get("jsonlIno")) {
            Some(nested_ino) if nested_ino == top_ino => {}
            _ => return true,
        }
    }
    let Some((live_dev, live_ino)) = jsonl_path_identity(jsonl) else {
        return true;
    };
    top_dev != live_dev || top_ino != live_ino
}

fn jsonl_path_identity(path: &Path) -> Option<(String, String)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(path).ok()?;
        return Some((even_hex(meta.dev()), even_hex(meta.ino())));
    }
    #[cfg(windows)]
    {
        return windows_file_identity(path);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}

#[cfg(windows)]
fn windows_file_identity(path: &Path) -> Option<(String, String)> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut info = unsafe { std::mem::zeroed::<FILE_ID_INFO>() };
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            &mut info as *mut FILE_ID_INFO as *mut _,
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return None;
    }
    Some((
        format!("{:016x}", info.VolumeSerialNumber),
        info.FileId.Identifier.iter().map(|byte| format!("{byte:02x}")).collect(),
    ))
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
    let session_id = session_id_from_lease_path(path)?;
    read_lease_contents(path, &session_id)
}

fn session_id_from_lease_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let session_id = name
        .strip_suffix(".claimed.stale")
        .or_else(|| name.strip_suffix(".claimed"))?;
    Some(session_id.to_string())
}

fn read_lease_contents(path: &Path, session_id: &str) -> Option<SidecarLease> {
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
    if nonce.is_empty() || session_id.is_empty() {
        return None;
    }
    Some(SidecarLease {
        session_id: session_id.to_string(),
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
                return Err("学習sidecarの所有記録が競合しました".into());
            }
        }
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let started_at = now.as_secs();
    let nonce = format!("{pid:x}-{:x}", now.as_nanos());
    let payload = format!("{{\"pid\":{pid},\"nonce\":\"{nonce}\",\"started_at\":{started_at}}}\n");
    let lease = SidecarLease {
        session_id: session_id.to_string(),
        pid,
        nonce: nonce.clone(),
        started_at,
    };
    let staged = dir.join(format!(".{session_id}.{pid}.{nonce}.claimed.tmp"));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    match options.open(&staged) {
        Ok(mut file) => {
            if let Err(error) = file.write_all(payload.as_bytes()) {
                let _ = fs::remove_file(&staged);
                return Err(format!("学習sidecarの所有を記録できません: {error}"));
            }
            if let Err(error) = file.sync_all() {
                let _ = fs::remove_file(&staged);
                return Err(format!("学習sidecarの所有を記録できません: {error}"));
            }
        }
        Err(error) => {
            return Err(format!("学習sidecarの所有を記録できません: {error}"));
        }
    }
    match fs::hard_link(&staged, &claimed) {
        Ok(()) => {
            let _ = fs::remove_file(&staged);
            Ok(lease)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&staged);
            if let Some(existing) = read_lease_file(&claimed) {
                if process_is_live(existing.pid) {
                    return Err("学習sidecarはすでに所有されています".into());
                }
            }
            Err("学習sidecarの所有記録が競合しました".into())
        }
        Err(error) => {
            let _ = fs::remove_file(&staged);
            Err(format!("学習sidecarの所有を公開できません: {error}"))
        }
    }
}

fn lease_is_published(lease: &SidecarLease) -> bool {
    let Ok(dir) = learning_state_dir() else {
        return false;
    };
    let path = dir
        .join("sidecars")
        .join(format!("{}.claimed", lease.session_id));
    read_lease_file(&path)
        .map(|published| leases_match(&published, lease))
        .unwrap_or(false)
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
            if let Some(moved) = read_lease_contents(&staging, &expected.session_id) {
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

fn write_control_file(session_id: &str, name: &str) {
    if !looks_like_session_id(session_id) {
        return;
    }
    let Ok(dir) = learning_state_dir() else {
        return;
    };
    let path = dir.join("sidecars").join(format!("{session_id}.{name}"));
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, format!("{name}\n").as_bytes());
}

fn write_stop_file(session_id: &str) {
    write_control_file(session_id, "stop");
}

fn write_pause_file(session_id: &str) {
    write_control_file(session_id, "pause");
}

fn write_missed_flush_marker(session_id: &str) {
    if !looks_like_session_id(session_id) {
        return;
    }
    let Ok(dir) = learning_state_dir() else {
        return;
    };
    let sidecar = dir.join("sidecars").join(format!("{session_id}.json"));
    let path = dir.join("sidecars").join(format!("{session_id}.missed"));
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let payload = match fs::read_to_string(&sidecar)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|value| value.get("work_boundary").cloned())
        .and_then(|boundary| {
            Some((
                boundary.get("user_turns")?.as_u64()?,
                boundary.get("tool_units")?.as_u64()?,
                boundary.get("offset")?.as_u64()?,
                boundary.get("id")?.as_u64()?,
            ))
        }) {
        Some((user_turns, tool_units, offset, boundary_id)) => format!(
            "{{\"kind\":\"missed\",\"userTurns\":{user_turns},\"toolUnits\":{tool_units},\"offset\":{offset},\"id\":{boundary_id}}}\n"
        ),
        None => "{\"kind\":\"missed\",\"unknown\":true}\n".to_string(),
    };
    let tmp = dir.join("sidecars").join(format!(".{session_id}.missed.{}.tmp", std::process::id()));
    if fs::write(&tmp, payload.as_bytes()).is_err() {
        eprintln!("CODETAS learning sidecar {session_id}: failed to write missed marker");
        return;
    }
    if replace_file(&tmp, &path).is_err() {
        let _ = fs::remove_file(&tmp);
        eprintln!("CODETAS learning sidecar {session_id}: failed to replace missed marker");
    }
}

fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        fn wide(path: &Path) -> Vec<u16> {
            path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
        }
        extern "system" {
            fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
        }
        const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
        let from_w = wide(from);
        let to_w = wide(to);
        let ok = unsafe { MoveFileExW(from_w.as_ptr(), to_w.as_ptr(), MOVEFILE_REPLACE_EXISTING) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::rename(from, to)
    }
}

fn clear_control_file(session_id: &str, name: &str) {
    if !looks_like_session_id(session_id) {
        return;
    }
    let Ok(dir) = learning_state_dir() else {
        return;
    };
    let _ = fs::remove_file(dir.join("sidecars").join(format!("{session_id}.{name}")));
}

fn clear_stop_file(session_id: &str) {
    clear_control_file(session_id, "stop");
}

fn clear_pause_file(session_id: &str) {
    clear_control_file(session_id, "pause");
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
    discover_session_jsonls(Some(LIVE_WINDOW), MAX_WALK_ENTRIES, false)
}

fn discover_existing_sessions() -> Result<Vec<(String, PathBuf)>, String> {
    discover_existing_sessions_with_limit(MAX_WALK_ENTRIES)
}

fn discover_existing_sessions_with_limit(
    max_walk_entries: usize,
) -> Result<Vec<(String, PathBuf)>, String> {
    discover_session_jsonls(None, max_walk_entries, true)
}

fn discover_session_jsonls(
    live_window: Option<Duration>,
    max_walk_entries: usize,
    fail_closed_on_limit: bool,
) -> Result<Vec<(String, PathBuf)>, String> {
    let root = crate::provider_gateway::codex_home()?.join("sessions");
    discover_session_jsonls_from_root(
        &root,
        SystemTime::now(),
        live_window,
        max_walk_entries,
        fail_closed_on_limit,
    )
}

fn discover_session_jsonls_from_root(
    root: &Path,
    now: SystemTime,
    live_window: Option<Duration>,
    max_walk_entries: usize,
    fail_closed_on_limit: bool,
) -> Result<Vec<(String, PathBuf)>, String> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) if fail_closed_on_limit => {
            return Err("session root is not a regular directory".into());
        }
        Ok(_) => return Ok(Vec::new()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) if fail_closed_on_limit => {
            return Err(format!("session root is unreadable: {error}"));
        }
        Err(_) => return Ok(Vec::new()),
    }
    let mut pending = vec![root.to_path_buf()];
    let mut seen = 0usize;
    let mut sessions: Vec<(String, PathBuf, SystemTime)> = Vec::new();
    let mut truncated = false;
    while let Some(directory) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if fail_closed_on_limit => {
                return Err(format!(
                    "session directory is unreadable ({}): {error}",
                    directory.display()
                ));
            }
            Err(_) => continue,
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if fail_closed_on_limit => {
                    return Err(format!("session directory entry is unreadable: {error}"));
                }
                Err(_) => continue,
            };
            seen += 1;
            if seen > max_walk_entries {
                truncated = true;
                break;
            }
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if fail_closed_on_limit => {
                    return Err(format!(
                        "session path is unreadable ({}): {error}",
                        path.display()
                    ));
                }
                Err(_) => continue,
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
            let mtime = match metadata.modified() {
                Ok(mtime) => mtime,
                Err(_) if live_window.is_some() => continue,
                Err(_) => UNIX_EPOCH,
            };
            if metadata.len() == 0 {
                continue;
            }
            if let Some(window) = live_window {
                let Ok(age) = now.duration_since(mtime) else {
                    continue;
                };
                if age > window {
                    continue;
                }
            }
            sessions.push((id, path, mtime));
        }
        if truncated {
            break;
        }
    }
    if truncated && fail_closed_on_limit {
        return Err(
            "session walk exceeded bound; refusing partial enable-boundary snapshot".into(),
        );
    }
    sessions.sort_by(|left, right| right.2.cmp(&left.2));
    Ok(sessions
        .into_iter()
        .map(|(id, path, _)| (id, path))
        .collect())
}

fn self_improvement_enabled_marker_path() -> Result<PathBuf, String> {
    Ok(learning_state_dir()?.join("self-improvement.enabled"))
}

fn self_improvement_enabled_marker_exists() -> Result<bool, String> {
    let path = self_improvement_enabled_marker_path()?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err("self-improvement enabled marker is not a regular file".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("self-improvement enabled marker is unreadable: {error}")),
    }
}

fn clear_self_improvement_enabled_marker() {
    if let Ok(path) = self_improvement_enabled_marker_path() {
        let _ = fs::remove_file(path);
    }
}

fn ensure_self_improvement_enabled() -> Result<bool, String> {
    ensure_self_improvement_enabled_with_limit(MAX_WALK_ENTRIES)
}

fn ensure_self_improvement_enabled_with_limit(max_walk_entries: usize) -> Result<bool, String> {
    if self_improvement_enabled_marker_exists()? {
        return Ok(false);
    }
    let existing = discover_existing_sessions_with_limit(max_walk_entries)?;
    enable_self_improvement_for_sessions(&existing)
}

fn enable_self_improvement_for_sessions(
    sessions: &[(String, PathBuf)],
) -> Result<bool, String> {
    if self_improvement_enabled_marker_exists()? {
        return Ok(false);
    }
    write_enable_boundaries(sessions)?;
    mark_self_improvement_enabled()
}

fn mark_self_improvement_enabled() -> Result<bool, String> {
    let path = self_improvement_enabled_marker_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(file) => {
            file.sync_all().map_err(|error| error.to_string())?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

fn write_enable_boundaries(live: &[(String, PathBuf)]) -> Result<(), String> {
    for (session_id, jsonl) in live {
        write_enable_boundary(session_id, jsonl)?;
    }
    Ok(())
}

fn write_enable_boundary(session_id: &str, jsonl: &Path) -> Result<(), String> {
    if !looks_like_session_id(session_id) {
        return Ok(());
    }
    let dir = learning_state_dir()?;
    let sidecars = dir.join("sidecars");
    fs::create_dir_all(&sidecars).map_err(|error| error.to_string())?;
    let metadata = fs::metadata(jsonl).map_err(|error| error.to_string())?;
    let (dev, ino) = jsonl_path_identity(jsonl)
        .ok_or_else(|| format!("jsonl identity unavailable for {session_id}"))?;
    let size = metadata.len();
    let payload = format!(
        "{}\n",
        serde_json::json!({
            "kind": "enable-boundary",
            "offset": size,
            "jsonlSize": size,
            "jsonlDev": dev,
            "jsonlIno": ino,
        })
    );
    let path = sidecars.join(format!("{session_id}.enable-boundary"));
    let tmp = sidecars.join(format!(".{session_id}.enable-boundary.{}.tmp", std::process::id()));
    {
        use std::io::Write;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|error| error.to_string())?;
        file.write_all(payload.as_bytes())
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
    }
    if let Err(error) = replace_file(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(error.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn enabled_marker_is_published_only_after_boundaries_succeed() {
        let dir = std::env::temp_dir().join(format!(
            "codetas-enable-marker-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        std::env::set_var("CODETAS_LEARNING_STATE_DIR", &dir);
        let session = "01a002ab-772a-7553-b882-d2675d3d6ee6";
        let missing = dir.join("missing.jsonl");
        assert!(enable_self_improvement_for_sessions(&[(session.into(), missing)]).is_err());
        assert!(!self_improvement_enabled_marker_path().unwrap().exists());

        let jsonl = dir.join("rollout.jsonl");
        fs::write(&jsonl, "existing transcript\n").unwrap();
        assert_eq!(
            enable_self_improvement_for_sessions(&[(session.into(), jsonl)]).unwrap(),
            true
        );
        assert!(dir.join("sidecars").join(format!("{session}.enable-boundary")).is_file());
        assert!(self_improvement_enabled_marker_path().unwrap().is_file());
        assert_eq!(enable_self_improvement_for_sessions(&[]).unwrap(), false);
        std::env::remove_var("CODETAS_LEARNING_STATE_DIR");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_session_snapshot_includes_sessions_older_than_live_window() {
        let dir = std::env::temp_dir().join(format!(
            "codetas-enable-existing-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let root = dir.join("sessions");
        let nested = root.join("2026").join("08").join("26");
        fs::create_dir_all(&nested).unwrap();
        let session = "01a002ab-772a-7553-b882-d2675d3d6ee6";
        let jsonl = nested.join(format!("rollout-2026-08-26T00-00-00-{session}.jsonl"));
        fs::write(&jsonl, "existing transcript\n").unwrap();
        let future = SystemTime::now() + LIVE_WINDOW + Duration::from_secs(1);

        let existing = discover_session_jsonls_from_root(&root, future, None, 16, true).unwrap();
        assert_eq!(existing, vec![(session.into(), jsonl.clone())]);
        let live = discover_session_jsonls_from_root(
            &root,
            future,
            Some(LIVE_WINDOW),
            16,
            false,
        )
        .unwrap();
        assert!(live.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_session_snapshot_fails_closed_at_walk_limit() {
        let dir = std::env::temp_dir().join(format!(
            "codetas-enable-limit-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let root = dir.join("sessions");
        fs::create_dir_all(&root).unwrap();
        for session in [
            "01a002ab-772a-7553-b882-d2675d3d6ee6",
            "01a002ab-772a-7553-b882-d2675d3d6ee7",
        ] {
            fs::write(root.join(format!("rollout-{session}.jsonl")), "transcript\n").unwrap();
        }
        let error = discover_session_jsonls_from_root(
            &root,
            SystemTime::now(),
            None,
            1,
            true,
        )
        .unwrap_err();
        assert!(error.contains("refusing partial enable-boundary snapshot"));
        let _ = fs::remove_dir_all(&dir);
    }

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

    #[test]
    fn staged_claimed_lease_parses_without_claimed_suffix() {
        let dir = std::env::temp_dir().join(format!(
            "codetas-lease-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("01a002ab-772a-7553-b882-d2675d3d6ee6.claimed.stale");
        fs::write(&path, "{\"pid\":123,\"nonce\":\"abc\",\"started_at\":1}\n").unwrap();
        let lease = read_lease_file(&path).expect("staged lease should parse");
        assert_eq!(lease.session_id, "01a002ab-772a-7553-b882-d2675d3d6ee6");
        assert_eq!(lease.pid, 123);
        assert_eq!(lease.nonce, "abc");
        assert_eq!(lease.started_at, 1);
        let moved = read_lease_contents(&path, "01a002ab-772a-7553-b882-d2675d3d6ee6")
            .expect("expected session id should bind staged contents");
        assert_eq!(moved.session_id, "01a002ab-772a-7553-b882-d2675d3d6ee6");
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn finished_marker_is_stale_when_jsonl_grows_past_boundary() {
        let dir = std::env::temp_dir().join(format!(
            "codetas-finished-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(dir.join("sidecars")).unwrap();
        std::env::set_var("CODETAS_LEARNING_STATE_DIR", &dir);
        let session = "01a002ab-772a-7553-b882-d2675d3d6ee6";
        let jsonl = dir.join("rollout.jsonl");
        fs::write(&jsonl, "hello\nworld\n").unwrap();
        fs::write(
            dir.join("sidecars").join(format!("{session}.finished")),
            r#"{"kind":"finished","workBoundary":{"offset":6},"jsonlSize":6}"#,
        )
        .unwrap();
        assert!(sidecar_finished_is_stale(session, &jsonl));
        fs::write(&jsonl, "hello\n").unwrap();
        let identity = jsonl_path_identity(&jsonl).expect("file identity");
        fs::write(
            dir.join("sidecars").join(format!("{session}.finished")),
            format!(
                r#"{{"kind":"finished","revision":1,"workBoundary":{{"offset":6,"jsonlSize":6,"jsonlDev":"{}","jsonlIno":"{}"}},"jsonlSize":6,"jsonlDev":"{}","jsonlIno":"{}"}}"#,
                identity.0, identity.1, identity.0, identity.1
            ),
        )
        .unwrap();
        assert!(!sidecar_finished_is_stale(session, &jsonl));
        fs::write(
            dir.join("sidecars").join(format!("{session}.finished")),
            format!(
                r#"{{"kind":"finished","revision":"1","workBoundary":{{"offset":6}},"jsonlSize":6,"jsonlDev":"{}","jsonlIno":"{}"}}"#,
                identity.0, identity.1
            ),
        )
        .unwrap();
        assert!(sidecar_finished_is_stale(session, &jsonl));
        fs::write(&jsonl, "hi\n").unwrap();
        assert!(sidecar_finished_is_stale(session, &jsonl));
        fs::write(&jsonl, "hello\n").unwrap();
        fs::write(
            dir.join("sidecars").join(format!("{session}.finished")),
            r#"{"kind":"finished","workBoundary":{"offset":6,"jsonlSize":6}}"#,
        )
        .unwrap();
        assert!(sidecar_finished_is_stale(session, &jsonl));
        fs::write(
            dir.join("sidecars").join(format!("{session}.finished")),
            r#"{"kind":"finished","workBoundary":{"offset":6,"jsonlSize":6},"jsonlSize":99}"#,
        )
        .unwrap();
        assert!(sidecar_finished_is_stale(session, &jsonl));
        fs::write(&jsonl, "hello\n").unwrap();
        let identity = jsonl_path_identity(&jsonl).expect("file identity");
        fs::write(
            dir.join("sidecars").join(format!("{session}.finished")),
            format!(
                r#"{{"kind":"finished","revision":1,"workBoundary":{{"offset":6}},"jsonlSize":6,"jsonlDev":"{}","jsonlIno":"{}"}}"#,
                identity.0.to_ascii_uppercase(),
                identity.1
            ),
        )
        .unwrap();
        assert!(sidecar_finished_is_stale(session, &jsonl));
        assert_eq!(even_hex(1), "01");
        std::env::remove_var("CODETAS_LEARNING_STATE_DIR");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_finished_marker_is_stale() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "codetas-finished-unreadable-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(dir.join("sidecars")).unwrap();
        std::env::set_var("CODETAS_LEARNING_STATE_DIR", &dir);
        let session = "01a002ab-772a-7553-b882-d2675d3d6ee6";
        let jsonl = dir.join("rollout.jsonl");
        fs::write(&jsonl, "hello\n").unwrap();
        let finished = dir.join("sidecars").join(format!("{session}.finished"));
        fs::write(&finished, r#"{"kind":"finished","revision":1}"#).unwrap();
        let mut permissions = fs::metadata(&finished).unwrap().permissions();
        permissions.set_mode(0o000);
        fs::set_permissions(&finished, permissions).unwrap();
        assert!(sidecar_finished_is_stale(session, &jsonl));
        let mut permissions = fs::metadata(&finished).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&finished, permissions).unwrap();
        std::env::remove_var("CODETAS_LEARNING_STATE_DIR");
        let _ = fs::remove_dir_all(&dir);
    }
}
