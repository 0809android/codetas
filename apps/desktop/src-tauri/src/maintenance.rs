use command_group::CommandGroup;
use serde::Serialize;
use serde_json::Value;
use toml_edit::DocumentMut;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::OsStr,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const SLOW_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_COMMAND_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_LOG_FILES: usize = 12;
const MAX_LOG_TAIL_BYTES: u64 = 2 * 1024 * 1024;
const MAX_GIT_REPOSITORIES: usize = 8;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum MaintenanceSeverity {
    Healthy,
    Unknown,
    Attention,
    Critical,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceFinding {
    id: String,
    category: String,
    severity: MaintenanceSeverity,
    title: String,
    summary: String,
    technical_details: Vec<String>,
    affected_paths: Vec<String>,
    affected_thread_ids: Vec<String>,
    detected_at_ms: u64,
    estimated_reclaimable_bytes: Option<u64>,
    requires_codex_shutdown: bool,
    repair_action_id: Option<String>,
    reversible: bool,
    confidence: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceStorageEntry {
    id: String,
    label: String,
    path: String,
    bytes: u64,
    file_count: Option<u64>,
    directory_count: Option<u64>,
    top_level_directory_count: Option<u64>,
    recent_24h_modified_bytes: Option<u64>,
    status: MaintenanceSeverity,
    scan_truncated: bool,
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceProcessInfo {
    pid: u32,
    parent_pid: Option<u32>,
    parent_name: Option<String>,
    name: String,
    started_at: Option<String>,
    terminal: Option<String>,
    cpu_percent: Option<f64>,
    memory_bytes: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceFileLock {
    path: String,
    thread_id: Option<String>,
    process: MaintenanceProcessInfo,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceSqliteHealth {
    path: String,
    available: bool,
    physical_bytes: u64,
    page_size: Option<u64>,
    page_count: Option<u64>,
    freelist_count: Option<u64>,
    reclaimable_bytes: Option<u64>,
    estimated_live_bytes: Option<u64>,
    journal_mode: Option<String>,
    query_duration_ms: Option<u64>,
    open_by: Vec<MaintenanceProcessInfo>,
    error: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceMcpStatus {
    name: String,
    configured: bool,
    enabled: bool,
    startup_ms: Option<u64>,
    error_count: u64,
    auth_error_count: u64,
    disable_candidate: bool,
    status: MaintenanceSeverity,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceGitStatus {
    path: String,
    status: MaintenanceSeverity,
    changed_files: Option<u64>,
    untracked_files: Option<u64>,
    estimated_diff_bytes: Option<u64>,
    upstream_configured: Option<bool>,
    origin_configured: Option<bool>,
    generated_file_candidates: Vec<String>,
    gitignore_candidates: Vec<String>,
    note: String,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceSystemHealth {
    disk_total_bytes: Option<u64>,
    disk_free_bytes: Option<u64>,
    disk_used_percent: Option<f64>,
    swap_total_bytes: Option<u64>,
    swap_used_bytes: Option<u64>,
    memory_free_percent: Option<f64>,
    codex_process_count: usize,
    codex_cpu_percent: f64,
    codex_memory_bytes: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceReport {
    schema_version: u8,
    generated_at_ms: u64,
    duration_ms: u64,
    platform: String,
    overall_status: MaintenanceSeverity,
    read_only: bool,
    privacy_note: String,
    findings: Vec<MaintenanceFinding>,
    storage: Vec<MaintenanceStorageEntry>,
    sqlite: MaintenanceSqliteHealth,
    file_locks: Vec<MaintenanceFileLock>,
    processes: Vec<MaintenanceProcessInfo>,
    mcp: Vec<MaintenanceMcpStatus>,
    mcp_max_startup_ms: Option<u64>,
    git: Vec<MaintenanceGitStatus>,
    system: MaintenanceSystemHealth,
    partial_failures: Vec<String>,
}

#[derive(Default)]
struct LogScan {
    counts: BTreeMap<&'static str, u64>,
    mcp_errors: BTreeMap<String, u64>,
    mcp_auth_errors: BTreeMap<String, u64>,
    mcp_startup_ms: BTreeMap<String, u64>,
    mcp_max_startup_ms: Option<u64>,
}

#[derive(Default)]
struct CommandResult {
    stdout: String,
    stderr: String,
    success: bool,
}

#[tauri::command]
pub async fn analyze_codex_maintenance() -> Result<MaintenanceReport, String> {
    tauri::async_runtime::spawn_blocking(analyze_codex_maintenance_blocking)
        .await
        .map_err(|error| format!("Codex診断タスクを完了できませんでした: {error}"))
}

fn analyze_codex_maintenance_blocking() -> MaintenanceReport {
    let started = Instant::now();
    let detected_at_ms = now_ms();
    let mut findings = Vec::new();
    let mut partial_failures = Vec::new();
    if !cfg!(target_os = "macos") {
        return MaintenanceReport {
            schema_version: 1,
            generated_at_ms: detected_at_ms,
            duration_ms: started.elapsed().as_millis() as u64,
            platform: std::env::consts::OS.into(),
            overall_status: MaintenanceSeverity::Unknown,
            read_only: true,
            privacy_note: privacy_note(),
            findings: vec![finding(
                "platform-unsupported",
                "system",
                MaintenanceSeverity::Unknown,
                "この診断は現在macOS専用です",
                "Codexの保存場所とプロセス確認方法が異なるため、この環境では診断を実行していません。",
                vec![],
                vec![],
                vec![],
                None,
                false,
                "high",
                detected_at_ms,
            )],
            storage: Vec::new(),
            sqlite: MaintenanceSqliteHealth::default(),
            file_locks: Vec::new(),
            processes: Vec::new(),
            mcp: Vec::new(),
            mcp_max_startup_ms: None,
            git: Vec::new(),
            system: MaintenanceSystemHealth::default(),
            partial_failures: vec!["CodexメンテナンスPhase 1はmacOS専用です。".into()],
        };
    }
    let Some(home) = dirs::home_dir() else {
        return MaintenanceReport {
            schema_version: 1,
            generated_at_ms: detected_at_ms,
            duration_ms: started.elapsed().as_millis() as u64,
            platform: std::env::consts::OS.into(),
            overall_status: MaintenanceSeverity::Unknown,
            read_only: true,
            privacy_note: privacy_note(),
            findings: vec![finding(
                "home-unavailable",
                "system",
                MaintenanceSeverity::Unknown,
                "Codexの保存場所を確認できません",
                "ホームディレクトリを解決できなかったため、診断を開始できませんでした。",
                vec![],
                vec![],
                vec![],
                None,
                false,
                "high",
                detected_at_ms,
            )],
            storage: Vec::new(),
            sqlite: MaintenanceSqliteHealth::default(),
            file_locks: Vec::new(),
            processes: Vec::new(),
            mcp: Vec::new(),
            mcp_max_startup_ms: None,
            git: Vec::new(),
            system: MaintenanceSystemHealth::default(),
            partial_failures: vec!["ホームディレクトリを解決できませんでした。".into()],
        };
    };

    let codex_root = home.join(".codex");
    let sessions = codex_root.join("sessions");
    let archived = codex_root.join("archived_sessions");
    let recovery = codex_root.join("recovery");
    let worktrees = codex_root.join("worktrees");
    let logs = home.join("Library/Logs/com.openai.codex");
    let app_support = home.join("Library/Application Support/Codex");
    let database = codex_root.join("logs_2.sqlite");
    let global_state = codex_root.join(".codex-global-state.json");
    let config = codex_root.join("config.toml");

    let process_map = process_snapshot(&mut partial_failures);
    let mut processes = process_map
        .values()
        .filter(|process| is_codex_process_name(&process.name))
        .cloned()
        .collect::<Vec<_>>();
    processes.sort_by_key(|process| process.pid);

    let system = system_health(&home, &processes, &mut partial_failures);
    if let Some(free) = system.disk_free_bytes {
        let severity = if free < 10 * 1024 * 1024 * 1024 {
            MaintenanceSeverity::Critical
        } else if free < 25 * 1024 * 1024 * 1024 {
            MaintenanceSeverity::Attention
        } else {
            MaintenanceSeverity::Healthy
        };
        if severity != MaintenanceSeverity::Healthy {
            findings.push(finding(
                "disk-space",
                "system",
                severity,
                "Macの空き容量が少なくなっています",
                "Codexのログや一時データが増えると、表示速度とmacOSのスワップ動作へ影響します。",
                vec![format!("空き容量: {} bytes", free)],
                vec![display_path(&home, &home)],
                vec![],
                None,
                false,
                "high",
                detected_at_ms,
            ));
        }
    }
    if let (Some(total), Some(used)) = (system.swap_total_bytes, system.swap_used_bytes) {
        if total > 0 && used.saturating_mul(100) / total >= 80 {
            findings.push(finding(
                "swap-pressure",
                "system",
                MaintenanceSeverity::Critical,
                "メモリの退避領域が高い水準です",
                "アプリの切り替えやタスク表示が遅くなる可能性があります。Codex関連プロセスのメモリ使用量も確認してください。",
                vec![format!("swap used: {used} / {total} bytes")],
                vec![],
                vec![],
                None,
                false,
                "high",
                detected_at_ms,
            ));
        }
    }
    if system.memory_free_percent.is_some_and(|free| free <= 10.0) {
        findings.push(finding(
            "memory-pressure",
            "system",
            MaintenanceSeverity::Attention,
            "Macのメモリ余力が少なくなっています",
            "Codexのタスク表示やアプリ切り替えが遅くなる可能性があります。スワップ使用量とCodex関連プロセスをあわせて確認してください。",
            vec![format!(
                "system-wide memory free: {:.1}%",
                system.memory_free_percent.unwrap_or_default()
            )],
            vec![],
            vec![],
            None,
            false,
            "high",
            detected_at_ms,
        ));
    }

    let storage_specs = [
        ("codex-root", "Codex全体", &codex_root, 10_u64, false),
        ("sessions", "使用中タスク", &sessions, 2_u64, false),
        ("archives", "アーカイブ済みタスク", &archived, 2_u64, false),
        ("recovery", "リカバリーデータ", &recovery, 2_u64, false),
        ("worktrees", "Codex Worktree", &worktrees, 2_u64, false),
        ("text-logs", "Codexテキストログ", &logs, 1_u64, true),
        ("app-support", "Codexアプリデータ", &app_support, 3_u64, false),
    ];
    let mut storage = Vec::new();
    for (id, label, path, attention_gib, measure_recent) in storage_specs {
        match storage_entry(id, label, path, &home, attention_gib, measure_recent) {
            Ok(entry) => storage.push(entry),
            Err(error) => partial_failures.push(format!("{label}: {error}")),
        }
    }

    let database_open_by = lsof_exact(&database, &process_map, &mut partial_failures);
    let sqlite = sqlite_health(&database, &home, database_open_by);
    if let Some(error) = &sqlite.error {
        partial_failures.push(format!("SQLiteログDB: {error}"));
    }
    if let Some(reclaimable) = sqlite.reclaimable_bytes {
        if reclaimable >= 512 * 1024 * 1024 {
            let severity = if reclaimable >= 2 * 1024 * 1024 * 1024 {
                MaintenanceSeverity::Critical
            } else {
                MaintenanceSeverity::Attention
            };
            findings.push(finding(
                "sqlite-reclaimable",
                "database",
                severity,
                "ログデータベースに大きな未使用領域があります",
                "会話の圧縮ではなく、Mac上のSQLiteファイルから回収できる領域です。圧縮はCodexを完全終了した後にだけ実行できます。",
                vec![
                    format!("page_size={:?}", sqlite.page_size),
                    format!("page_count={:?}", sqlite.page_count),
                    format!("freelist_count={:?}", sqlite.freelist_count),
                    format!("estimated_live_bytes={:?}", sqlite.estimated_live_bytes),
                    format!("journal_mode={:?}", sqlite.journal_mode),
                ],
                vec![sqlite.path.clone()],
                vec![],
                Some(reclaimable),
                true,
                "high",
                detected_at_ms,
            ));
        }
    }
    if sqlite.query_duration_ms.is_some_and(|duration| duration >= 1_000) {
        let duration = sqlite.query_duration_ms.unwrap_or_default();
        findings.push(finding(
            "sqlite-read-latency",
            "database",
            if duration >= 5_000 {
                MaintenanceSeverity::Critical
            } else {
                MaintenanceSeverity::Attention
            },
            "ログデータベースの読み取りに時間がかかっています",
            "読み取り専用の基本情報取得だけでも待ち時間が発生しました。DBの未使用領域とディスク空き容量をあわせて確認してください。",
            vec![format!("read-only PRAGMA duration: {duration}ms")],
            vec![sqlite.path.clone()],
            vec![],
            None,
            true,
            "medium",
            detected_at_ms,
        ));
    }
    if !sqlite.open_by.is_empty() {
        findings.push(finding(
            "sqlite-in-use",
            "database",
            MaintenanceSeverity::Attention,
            "ログデータベースは現在使用中です",
            "Codex起動中の圧縮や入れ替えは安全ではありません。Phase 1は状態表示だけを行います。",
            sqlite
                .open_by
                .iter()
                .map(|process| format!("PID {}: {}", process.pid, process.name))
                .collect(),
            vec![sqlite.path.clone()],
            vec![],
            None,
            true,
            "high",
            detected_at_ms,
        ));
    }

    let (session_ids, session_scan_complete) =
        collect_session_ids(&[&sessions, &archived], &mut partial_failures);
    let orphan_pins = orphan_pin_candidates(
        &global_state,
        &session_ids,
        session_scan_complete,
        &mut partial_failures,
    );
    if !orphan_pins.is_empty() {
        findings.push(finding(
            "orphan-pins",
            "tasks",
            MaintenanceSeverity::Attention,
            "保存先が見つからないピン留めがあります",
            "ピン留め先のセッションファイルが使用中・アーカイブ済みのどちらにも見つかりません。今回は候補表示のみで、解除は行いません。",
            vec![format!("候補: {}件", orphan_pins.len())],
            vec![display_path(&global_state, &home)],
            orphan_pins,
            None,
            true,
            "medium",
            detected_at_ms,
        ));
    }

    let file_locks = lsof_directory(&sessions, &home, &process_map, &mut partial_failures);
    if !file_locks.is_empty() {
        findings.push(finding(
            "session-writers",
            "processes",
            MaintenanceSeverity::Attention,
            "別のプロセスがタスクファイルを使用しています",
            "使用中のタスクは移動・削除できません。PIDと親プロセスを確認し、まず通常終了してください。",
            file_locks
                .iter()
                .map(|lock| format!("PID {} {}", lock.process.pid, lock.process.name))
                .collect(),
            file_locks.iter().map(|lock| lock.path.clone()).collect(),
            file_locks
                .iter()
                .filter_map(|lock| lock.thread_id.clone())
                .collect(),
            None,
            false,
            "high",
            detected_at_ms,
        ));
    }

    let log_scan = scan_logs(&logs, &mut partial_failures);
    add_log_findings(&log_scan, detected_at_ms, &logs, &home, &mut findings);
    let mcp = mcp_statuses(&config, &log_scan, &mut partial_failures);
    if log_scan.mcp_max_startup_ms.is_some_and(|value| value >= 10_000) {
        let ms = log_scan.mcp_max_startup_ms.unwrap_or_default();
        findings.push(finding(
            "mcp-startup",
            "mcp",
            if ms >= 30_000 {
                MaintenanceSeverity::Critical
            } else {
                MaintenanceSeverity::Attention
            },
            "MCPの起動確認に時間がかかっています",
            "タスク一覧の表示前にMCP初期化待ちが発生している可能性があります。認証エラーや不要な接続を詳細で確認してください。",
            vec![format!("mcpServerStatus/list 最大 {ms}ms")],
            vec![display_path(&config, &home)],
            vec![],
            None,
            false,
            "high",
            detected_at_ms,
        ));
    }

    let git_roots = project_roots(&global_state, &mut partial_failures);
    let git = git_diagnostics(&git_roots, &home, &mut partial_failures);
    for (index, repository) in git.iter().enumerate() {
        if repository.status >= MaintenanceSeverity::Attention {
            findings.push(finding(
                &format!("git-{index}"),
                "git",
                repository.status,
                "Gitの状態確認に時間がかかる可能性があります",
                &repository.note,
                vec![
                    format!("changed_files={:?}", repository.changed_files),
                    format!("estimated_diff_bytes={:?}", repository.estimated_diff_bytes),
                    format!("upstream={:?}", repository.upstream_configured),
                ],
                vec![repository.path.clone()],
                vec![],
                None,
                false,
                "medium",
                detected_at_ms,
            ));
        }
    }

    for failure in &mut partial_failures {
        *failure = sanitize_diagnostic_text(failure, &home);
    }
    for failure in &partial_failures {
        findings.push(finding(
            &format!("partial-{}", findings.len()),
            "system",
            MaintenanceSeverity::Unknown,
            "一部の診断を完了できませんでした",
            failure,
            vec![],
            vec![],
            vec![],
            None,
            false,
            "high",
            detected_at_ms,
        ));
    }

    let overall_status = findings
        .iter()
        .map(|finding| finding.severity)
        .chain(storage.iter().map(|entry| entry.status))
        .chain(mcp.iter().map(|entry| entry.status))
        .chain(git.iter().map(|entry| entry.status))
        .max()
        .unwrap_or(MaintenanceSeverity::Healthy);
    MaintenanceReport {
        schema_version: 1,
        generated_at_ms: detected_at_ms,
        duration_ms: started.elapsed().as_millis() as u64,
        platform: std::env::consts::OS.into(),
        overall_status,
        read_only: true,
        privacy_note: privacy_note(),
        findings,
        storage,
        sqlite,
        file_locks,
        processes,
        mcp,
        mcp_max_startup_ms: log_scan.mcp_max_startup_ms,
        git,
        system,
        partial_failures,
    }
}

fn privacy_note() -> String {
    "診断レポートは容量、件数、設定名、プロセス情報、集計済みエラーだけを含み、会話本文・ログ本文・認証情報・メールアドレスを収集しません。ホームパスは ~ として表示します。".into()
}

#[allow(clippy::too_many_arguments)]
fn finding(
    id: &str,
    category: &str,
    severity: MaintenanceSeverity,
    title: &str,
    summary: &str,
    technical_details: Vec<String>,
    affected_paths: Vec<String>,
    affected_thread_ids: Vec<String>,
    estimated_reclaimable_bytes: Option<u64>,
    requires_codex_shutdown: bool,
    confidence: &str,
    detected_at_ms: u64,
) -> MaintenanceFinding {
    let repair_action_id = match id {
        "sqlite-reclaimable" | "sqlite-read-latency" => Some("compact-sqlite".into()),
        "orphan-pins" | "log-thread-not-loaded" => Some("remove-orphan-pins".into()),
        "session-writers" | "log-active-writer" => Some("retry-archive".into()),
        "mcp-startup" | "log-auth-required" | "log-method-not-found" => {
            Some("review-mcp".into())
        }
        _ => None,
    };
    MaintenanceFinding {
        id: id.into(),
        category: category.into(),
        severity,
        title: title.into(),
        summary: summary.into(),
        technical_details,
        affected_paths,
        affected_thread_ids,
        detected_at_ms,
        estimated_reclaimable_bytes,
        requires_codex_shutdown,
        repair_action_id,
        reversible: true,
        confidence: confidence.into(),
    }
}

fn storage_entry(
    id: &str,
    label: &str,
    path: &Path,
    home: &Path,
    attention_gib: u64,
    measure_recent: bool,
) -> Result<MaintenanceStorageEntry, String> {
    if !path.exists() {
        return Ok(MaintenanceStorageEntry {
            id: id.into(),
            label: label.into(),
            path: display_path(path, home),
            bytes: 0,
            file_count: Some(0),
            directory_count: Some(0),
            top_level_directory_count: Some(0),
            recent_24h_modified_bytes: Some(0).filter(|_| measure_recent),
            status: MaintenanceSeverity::Healthy,
            scan_truncated: false,
        });
    }
    let bytes = directory_bytes(path)?;
    let (file_count, directory_count, truncated) = count_entries(path, Duration::from_secs(3), 200_000);
    let (top_level_directory_count, top_level_truncated) =
        count_top_level_directories(path, Duration::from_secs(1), 20_000);
    let (recent_24h_modified_bytes, recent_truncated) = if measure_recent {
        let (bytes, truncated) = recent_file_bytes(path, Duration::from_secs(3), 200_000);
        (Some(bytes), truncated)
    } else {
        (None, false)
    };
    let status = if bytes >= attention_gib * 1024 * 1024 * 1024 {
        MaintenanceSeverity::Attention
    } else {
        MaintenanceSeverity::Healthy
    };
    Ok(MaintenanceStorageEntry {
        id: id.into(),
        label: label.into(),
        path: display_path(path, home),
        bytes,
        file_count: Some(file_count),
        directory_count: Some(directory_count),
        top_level_directory_count,
        recent_24h_modified_bytes,
        status,
        scan_truncated: truncated || recent_truncated || top_level_truncated,
    })
}

fn directory_bytes(path: &Path) -> Result<u64, String> {
    let result = run_command("/usr/bin/du", &[OsStr::new("-sk"), path.as_os_str()], None, SLOW_COMMAND_TIMEOUT)?;
    if !result.success {
        return Err(command_error("du", &result));
    }
    let kib = result
        .stdout
        .split_whitespace()
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or("容量の出力を読み取れませんでした")?;
    Ok(kib.saturating_mul(1024))
}

fn count_entries(path: &Path, timeout: Duration, max_entries: u64) -> (u64, u64, bool) {
    let deadline = Instant::now() + timeout;
    let mut stack = vec![path.to_path_buf()];
    let mut files = 0_u64;
    let mut directories = 0_u64;
    let mut visited = 0_u64;
    let mut incomplete = false;
    while let Some(directory) = stack.pop() {
        if Instant::now() >= deadline || visited >= max_entries {
            return (files, directories, true);
        }
        let Ok(entries) = fs::read_dir(directory) else {
            incomplete = true;
            continue;
        };
        for entry in entries.flatten() {
            if Instant::now() >= deadline || visited >= max_entries {
                return (files, directories, true);
            }
            visited = visited.saturating_add(1);
            let Ok(file_type) = entry.file_type() else {
                incomplete = true;
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                directories = directories.saturating_add(1);
                stack.push(entry.path());
            } else if file_type.is_file() {
                files = files.saturating_add(1);
            }
        }
    }
    (files, directories, incomplete)
}

fn count_top_level_directories(
    path: &Path,
    timeout: Duration,
    max_entries: u64,
) -> (Option<u64>, bool) {
    let Ok(entries) = fs::read_dir(path) else {
        return (None, true);
    };
    let deadline = Instant::now() + timeout;
    let mut directories = 0_u64;
    let mut visited = 0_u64;
    for entry in entries.flatten() {
        if Instant::now() >= deadline || visited >= max_entries {
            return (Some(directories), true);
        }
        visited = visited.saturating_add(1);
        if entry
            .file_type()
            .is_ok_and(|kind| kind.is_dir() && !kind.is_symlink())
        {
            directories = directories.saturating_add(1);
        }
    }
    (Some(directories), false)
}

fn recent_file_bytes(path: &Path, timeout: Duration, max_entries: u64) -> (u64, bool) {
    let deadline = Instant::now() + timeout;
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(24 * 60 * 60))
        .unwrap_or(UNIX_EPOCH);
    let mut stack = vec![path.to_path_buf()];
    let mut bytes = 0_u64;
    let mut visited = 0_u64;
    let mut incomplete = false;
    while let Some(directory) = stack.pop() {
        if Instant::now() >= deadline || visited >= max_entries {
            return (bytes, true);
        }
        let Ok(entries) = fs::read_dir(directory) else {
            incomplete = true;
            continue;
        };
        for entry in entries.flatten() {
            if Instant::now() >= deadline || visited >= max_entries {
                return (bytes, true);
            }
            visited = visited.saturating_add(1);
            let Ok(file_type) = entry.file_type() else {
                incomplete = true;
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                if let Ok(metadata) = entry.metadata() {
                    if metadata.modified().is_ok_and(|modified| modified >= cutoff) {
                        bytes = bytes.saturating_add(metadata.len());
                    }
                }
            }
        }
    }
    (bytes, incomplete)
}

fn sqlite_health(
    path: &Path,
    home: &Path,
    open_by: Vec<MaintenanceProcessInfo>,
) -> MaintenanceSqliteHealth {
    let physical_bytes = fs::metadata(path).map(|metadata| metadata.len()).unwrap_or(0);
    let mut health = MaintenanceSqliteHealth {
        path: display_path(path, home),
        available: path.is_file(),
        physical_bytes,
        open_by,
        ..MaintenanceSqliteHealth::default()
    };
    if !path.is_file() {
        health.error = Some("ログデータベースが見つかりません。".into());
        return health;
    }
    let started = Instant::now();
    let sql = "PRAGMA query_only=ON; PRAGMA page_size; PRAGMA page_count; PRAGMA freelist_count; PRAGMA journal_mode;";
    let immutable_uri = sqlite_immutable_uri(path);
    match run_command(
        "/usr/bin/sqlite3",
        &[
            OsStr::new("-readonly"),
            OsStr::new(&immutable_uri),
            OsStr::new(sql),
        ],
        None,
        COMMAND_TIMEOUT,
    ) {
        Ok(result) if result.success => {
            let values = result.stdout.lines().map(str::trim).collect::<Vec<_>>();
            health.page_size = values.first().and_then(|value| value.parse().ok());
            health.page_count = values.get(1).and_then(|value| value.parse().ok());
            health.freelist_count = values.get(2).and_then(|value| value.parse().ok());
            health.journal_mode = values.get(3).map(|value| value.to_string());
            health.reclaimable_bytes = health
                .page_size
                .zip(health.freelist_count)
                .map(|(page, free)| page.saturating_mul(free));
            health.estimated_live_bytes = health
                .page_size
                .zip(health.page_count)
                .zip(health.freelist_count)
                .map(|((page, count), free)| page.saturating_mul(count.saturating_sub(free)));
            health.query_duration_ms = Some(started.elapsed().as_millis() as u64);
        }
        Ok(result) => health.error = Some(command_error("sqlite3", &result)),
        Err(error) => health.error = Some(error),
    }
    health
}

fn sqlite_immutable_uri(path: &Path) -> String {
    let encoded = path
        .to_string_lossy()
        .replace('%', "%25")
        .replace(' ', "%20")
        .replace('?', "%3F")
        .replace('#', "%23");
    format!("file:{encoded}?mode=ro&immutable=1")
}

fn collect_session_ids(
    paths: &[&Path],
    failures: &mut Vec<String>,
) -> (BTreeSet<String>, bool) {
    let mut ids = BTreeSet::new();
    let mut complete = true;
    for (index, path) in paths.iter().enumerate() {
        if !path.exists() {
            if index == 0 {
                complete = false;
                failures.push(format!("{} が見つからないため、孤立ピン判定を省略します。", path.display()));
            }
            continue;
        }
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut stack = vec![path.to_path_buf()];
        'directories: while let Some(directory) = stack.pop() {
            if Instant::now() >= deadline {
                complete = false;
                failures.push(format!("{} のセッション一覧がタイムアウトしました。", path.display()));
                break;
            }
            let Ok(entries) = fs::read_dir(&directory) else {
                complete = false;
                failures.push(format!("{} を読み取れませんでした。", directory.display()));
                continue;
            };
            for entry in entries.flatten() {
                if Instant::now() >= deadline {
                    complete = false;
                    failures.push(format!("{} のセッション一覧がタイムアウトしました。", path.display()));
                    break 'directories;
                }
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_symlink() {
                    continue;
                }
                if file_type.is_dir() {
                    stack.push(entry.path());
                } else if file_type.is_file() {
                    if let Some(id) = session_id_from_path(&entry.path()) {
                        ids.insert(id);
                    }
                }
            }
        }
    }
    (ids, complete)
}

fn session_id_from_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".jsonl")?;
    stem.get(stem.len().checked_sub(36)?..)
        .map(str::to_string)
        .filter(|value| looks_like_uuid(value))
}

fn looks_like_uuid(value: &str) -> bool {
    value.len() == 36
        && value
            .chars()
            .enumerate()
            .all(|(index, character)| match index {
                8 | 13 | 18 | 23 => character == '-',
                _ => character.is_ascii_hexdigit(),
            })
}

fn orphan_pin_candidates(
    state_path: &Path,
    session_ids: &BTreeSet<String>,
    session_scan_complete: bool,
    failures: &mut Vec<String>,
) -> Vec<String> {
    if !session_scan_complete {
        failures.push("セッション一覧が完全ではないため、孤立ピン留めの候補判定を安全側で省略しました。".into());
        return Vec::new();
    }
    let Ok(metadata) = fs::metadata(state_path) else {
        return Vec::new();
    };
    if metadata.len() > 8 * 1024 * 1024 {
        failures.push("Codexグローバル状態が8MiBを超えるため、ピン留め診断を省略しました。".into());
        return Vec::new();
    }
    let Ok(bytes) = fs::read(state_path) else {
        failures.push("Codexグローバル状態を読み取れませんでした。".into());
        return Vec::new();
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        failures.push("Codexグローバル状態が認識できるJSONではありません。".into());
        return Vec::new();
    };
    value
        .get("pinned-thread-ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|id| looks_like_uuid(id) && !session_ids.contains(*id))
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn process_snapshot(failures: &mut Vec<String>) -> HashMap<u32, MaintenanceProcessInfo> {
    let format = OsStr::new("pid=,ppid=,%cpu=,rss=,lstart=,tty=,comm=");
    let result = match run_command(
        "/bin/ps",
        &[OsStr::new("-axo"), format],
        None,
        COMMAND_TIMEOUT,
    ) {
        Ok(result) if result.success => result,
        Ok(result) => {
            failures.push(command_error("ps", &result));
            return HashMap::new();
        }
        Err(error) => {
            failures.push(error);
            return HashMap::new();
        }
    };
    let mut processes = result
        .stdout
        .lines()
        .filter_map(parse_process_line)
        .map(|process| (process.pid, process))
        .collect::<HashMap<_, _>>();
    let names = processes
        .iter()
        .map(|(pid, process)| (*pid, process.name.clone()))
        .collect::<HashMap<_, _>>();
    for process in processes.values_mut() {
        process.parent_name = process.parent_pid.and_then(|pid| names.get(&pid).cloned());
    }
    processes
}

fn parse_process_line(line: &str) -> Option<MaintenanceProcessInfo> {
    let parts = line.split_whitespace().collect::<Vec<_>>();
    if parts.len() < 11 {
        return None;
    }
    let pid = parts[0].parse().ok()?;
    let parent_pid = parts[1].parse().ok();
    let cpu_percent = parts[2].parse().ok();
    let memory_bytes = parts[3]
        .parse::<u64>()
        .ok()
        .map(|kib| kib.saturating_mul(1024));
    let started_at = Some(parts[4..9].join(" "));
    let terminal = (parts[9] != "??" && parts[9] != "-").then(|| parts[9].to_string());
    let command = parts[10..].join(" ");
    let name = Path::new(&command)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(&command)
        .to_string();
    Some(MaintenanceProcessInfo {
        pid,
        parent_pid,
        parent_name: None,
        name: redact_email_components(&name),
        started_at,
        terminal,
        cpu_percent,
        memory_bytes,
    })
}

fn is_codex_process_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.contains("codetas") || lower.contains("opencodex") {
        return false;
    }
    let executable = Path::new(name)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(name)
        .to_ascii_lowercase();
    executable == "codex"
        || executable.starts_with("codex (")
        || lower.contains("/chatgpt.app/contents/") && executable.contains("codex")
}

fn lsof_exact(
    path: &Path,
    processes: &HashMap<u32, MaintenanceProcessInfo>,
    failures: &mut Vec<String>,
) -> Vec<MaintenanceProcessInfo> {
    if !path.exists() {
        return Vec::new();
    }
    let result = match run_command(
        "/usr/sbin/lsof",
        &[OsStr::new("-nP"), OsStr::new("-Fpcn"), OsStr::new("--"), path.as_os_str()],
        None,
        COMMAND_TIMEOUT,
    ) {
        Ok(result) => result,
        Err(error) => {
            failures.push(format!("logs_2.sqliteの使用プロセス確認: {error}"));
            return Vec::new();
        }
    };
    if !result.success && !result.stderr.trim().is_empty() {
        failures.push(format!("logs_2.sqliteの使用プロセス確認: {}", command_error("lsof", &result)));
    }
    let pids = parse_lsof(&result.stdout)
        .into_iter()
        .map(|(pid, _, _)| pid)
        .collect::<BTreeSet<_>>();
    pids.into_iter()
        .map(|pid| processes.get(&pid).cloned().unwrap_or_else(|| MaintenanceProcessInfo {
            pid,
            name: "不明なプロセス".into(),
            ..MaintenanceProcessInfo::default()
        }))
        .collect()
}

fn lsof_directory(
    path: &Path,
    home: &Path,
    processes: &HashMap<u32, MaintenanceProcessInfo>,
    failures: &mut Vec<String>,
) -> Vec<MaintenanceFileLock> {
    if !path.is_dir() {
        return Vec::new();
    }
    let result = match run_command(
        "/usr/sbin/lsof",
        &[OsStr::new("-nP"), OsStr::new("-Fpcn"), OsStr::new("+D"), path.as_os_str()],
        None,
        SLOW_COMMAND_TIMEOUT,
    ) {
        Ok(result) => result,
        Err(error) => {
            failures.push(format!("セッションファイルの使用プロセス確認: {error}"));
            return Vec::new();
        }
    };
    if !result.success && !result.stderr.trim().is_empty() {
        failures.push(format!("セッションファイルの使用プロセス確認: {}", command_error("lsof", &result)));
    }
    let mut seen = BTreeSet::new();
    parse_lsof(&result.stdout)
        .into_iter()
        .filter(|(_, _, file)| file.ends_with(".jsonl"))
        .filter_map(|(pid, name, file)| {
            if !seen.insert((pid, file.clone())) {
                return None;
            }
            let process = processes.get(&pid).cloned().unwrap_or_else(|| MaintenanceProcessInfo {
                pid,
                name: redact_email_components(&name),
                ..MaintenanceProcessInfo::default()
            });
            let path = PathBuf::from(file);
            Some(MaintenanceFileLock {
                path: display_path(&path, home),
                thread_id: session_id_from_path(&path),
                process,
            })
        })
        .collect()
}

fn parse_lsof(output: &str) -> Vec<(u32, String, String)> {
    let mut pid = None;
    let mut command = String::new();
    let mut values = Vec::new();
    for line in output.lines() {
        if let Some(value) = line.strip_prefix('p') {
            pid = value.parse().ok();
        } else if let Some(value) = line.strip_prefix('c') {
            command = value.to_string();
        } else if let (Some(current), Some(value)) = (pid, line.strip_prefix('n')) {
            values.push((current, command.clone(), value.to_string()));
        }
    }
    values
}

fn system_health(
    home: &Path,
    codex_processes: &[MaintenanceProcessInfo],
    failures: &mut Vec<String>,
) -> MaintenanceSystemHealth {
    let mut health = MaintenanceSystemHealth {
        codex_process_count: codex_processes.len(),
        codex_cpu_percent: codex_processes
            .iter()
            .filter_map(|process| process.cpu_percent)
            .sum(),
        codex_memory_bytes: codex_processes
            .iter()
            .filter_map(|process| process.memory_bytes)
            .sum(),
        ..MaintenanceSystemHealth::default()
    };
    match run_command(
        "/bin/df",
        &[OsStr::new("-k"), home.as_os_str()],
        None,
        COMMAND_TIMEOUT,
    ) {
        Ok(result) if result.success => {
            let mut parsed = false;
            if let Some(parts) = result.stdout.lines().last().map(|line| line.split_whitespace().collect::<Vec<_>>()) {
                if parts.len() >= 5 {
                    health.disk_total_bytes = parts.get(1).and_then(|value| value.parse::<u64>().ok()).map(|value| value.saturating_mul(1024));
                    health.disk_free_bytes = parts.get(3).and_then(|value| value.parse::<u64>().ok()).map(|value| value.saturating_mul(1024));
                    health.disk_used_percent = parts.get(4).and_then(|value| value.trim_end_matches('%').parse::<f64>().ok());
                    parsed = health.disk_total_bytes.is_some() && health.disk_free_bytes.is_some();
                }
            }
            if !parsed {
                failures.push("Macのディスク容量出力を解析できませんでした。".into());
            }
        }
        Ok(result) => failures.push(command_error("df", &result)),
        Err(error) => failures.push(error),
    }
    match run_command(
        "/usr/sbin/sysctl",
        &[OsStr::new("vm.swapusage")],
        None,
        COMMAND_TIMEOUT,
    ) {
        Ok(result) if result.success => {
            health.swap_total_bytes = parse_megabytes_after(&result.stdout, "total =");
            health.swap_used_bytes = parse_megabytes_after(&result.stdout, "used =");
            if health.swap_total_bytes.is_none() || health.swap_used_bytes.is_none() {
                failures.push("Macのスワップ使用量を解析できませんでした。".into());
            }
        }
        Ok(result) => failures.push(command_error("sysctl vm.swapusage", &result)),
        Err(error) => failures.push(error),
    }
    match run_command(
        "/usr/bin/memory_pressure",
        &[OsStr::new("-Q")],
        None,
        COMMAND_TIMEOUT,
    ) {
        Ok(result) if result.success => {
            health.memory_free_percent = parse_memory_free_percent(&result.stdout);
            if health.memory_free_percent.is_none() {
                failures.push("Macのメモリプレッシャー出力を解析できませんでした。".into());
            }
        }
        Ok(result) => failures.push(command_error("memory_pressure", &result)),
        Err(error) => failures.push(error),
    }
    health
}

fn parse_memory_free_percent(value: &str) -> Option<f64> {
    value.lines().find_map(|line| {
        let suffix = line.split_once("System-wide memory free percentage:")?.1;
        suffix.trim().trim_end_matches('%').parse().ok()
    })
}

fn parse_megabytes_after(value: &str, marker: &str) -> Option<u64> {
    let suffix = value.split_once(marker)?.1.trim_start();
    let number = suffix.split_whitespace().next()?.trim_end_matches('M');
    number
        .parse::<f64>()
        .ok()
        .map(|megabytes| (megabytes * 1024.0 * 1024.0) as u64)
}

fn scan_logs(path: &Path, failures: &mut Vec<String>) -> LogScan {
    let mut scan = LogScan::default();
    let (mut files, scan_incomplete) = newest_log_files(path);
    if scan_incomplete {
        failures.push("Codexテキストログの一覧を制限時間内に完全走査できませんでした。".into());
    }
    files.truncate(MAX_LOG_FILES);
    for file in files {
        let Ok(mut handle) = File::open(&file) else {
            failures.push(format!("{} を開けないため、ログ集計から除外しました。", file.display()));
            continue;
        };
        let size = handle.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        if size > MAX_LOG_TAIL_BYTES {
            let _ = handle.seek(SeekFrom::End(-(MAX_LOG_TAIL_BYTES as i64)));
        }
        let mut bytes = Vec::new();
        if handle.read_to_end(&mut bytes).is_err() {
            failures.push(format!("{} の末尾を読み取れませんでした。", file.display()));
            continue;
        }
        let contents = String::from_utf8_lossy(&bytes);
        for line in contents.lines() {
            let lower = line.to_ascii_lowercase();
            let diagnostic_line = lower.contains(" error ")
                || lower.contains(" warning ")
                || lower.contains("error=")
                || lower.contains("method=mcpserverstatus/list")
                || lower.contains("thread not loaded")
                || lower.contains("thread already has an active writer")
                || lower.contains("authrequired")
                || lower.contains("method not found")
                || lower.contains("unknown feature key")
                || lower.contains("invalid experimental feature")
                || lower.contains("git command failed");
            if !diagnostic_line {
                continue;
            }
            for (key, pattern) in [
                ("thread-not-loaded", "thread not loaded"),
                ("active-writer", "thread already has an active writer"),
                ("auth-required", "authrequired"),
                ("method-not-found", "method not found"),
                ("unknown-feature", "unknown feature key"),
                ("invalid-feature", "invalid experimental feature"),
                ("git-failure", "git command failed"),
            ] {
                if lower.contains(pattern) {
                    *scan.counts.entry(key).or_default() += 1;
                }
            }
            if lower.contains("method=mcpserverstatus/list") {
                if let Some(duration) = parse_numeric_field(line, "durationMs=") {
                    scan.mcp_max_startup_ms = Some(scan.mcp_max_startup_ms.unwrap_or(0).max(duration));
                    for name in extract_quoted_mcp_names(line) {
                        let maximum = scan.mcp_startup_ms.entry(name).or_default();
                        *maximum = (*maximum).max(duration);
                    }
                }
            }
            if lower.contains("mcp") || lower.contains("authrequired") {
                for name in extract_quoted_mcp_names(line) {
                    *scan.mcp_errors.entry(name.clone()).or_default() += 1;
                    if lower.contains("authrequired") {
                        *scan.mcp_auth_errors.entry(name).or_default() += 1;
                    }
                }
            }
        }
    }
    if !path.exists() {
        failures.push("Codexテキストログの保存先が見つかりません。".into());
    }
    scan
}

fn newest_log_files(path: &Path) -> (Vec<PathBuf>, bool) {
    if !path.exists() {
        return (Vec::new(), false);
    }
    let mut files = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut stack = vec![path.to_path_buf()];
    let mut incomplete = false;
    while let Some(directory) = stack.pop() {
        if Instant::now() >= deadline {
            incomplete = true;
            break;
        }
        let Ok(entries) = fs::read_dir(directory) else {
            incomplete = true;
            continue;
        };
        for entry in entries.flatten() {
            if Instant::now() >= deadline {
                incomplete = true;
                break;
            }
            let Ok(file_type) = entry.file_type() else {
                incomplete = true;
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() && entry.path().extension().and_then(OsStr::to_str) == Some("log") {
                files.push(entry.path());
            }
        }
    }
    files.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH)
    });
    files.reverse();
    (files, incomplete)
}

fn parse_numeric_field(line: &str, marker: &str) -> Option<u64> {
    let suffix = line.split_once(marker)?.1;
    let digits = suffix.chars().take_while(char::is_ascii_digit).collect::<String>();
    digits.parse().ok()
}

fn extract_quoted_mcp_names(line: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = line;
    while let Some((_, suffix)) = rest.split_once("MCP server '") {
        let Some((name, tail)) = suffix.split_once('\'') else {
            break;
        };
        if !name.is_empty() && name.len() <= 120 {
            names.push(name.to_string());
        }
        rest = tail;
    }
    names
}

fn add_log_findings(
    scan: &LogScan,
    detected_at_ms: u64,
    logs: &Path,
    home: &Path,
    findings: &mut Vec<MaintenanceFinding>,
) {
    for (id, category, severity, title, summary) in [
        ("thread-not-loaded", "tasks", MaintenanceSeverity::Attention, "存在しないタスク参照が記録されています", "孤立したピン留めや古いUI状態が残っている可能性があります。"),
        ("active-writer", "tasks", MaintenanceSeverity::Attention, "別プロセスがタスクを書き込み中です", "アーカイブ操作の前に、対象プロセスを通常終了する必要があります。"),
        ("auth-required", "mcp", MaintenanceSeverity::Attention, "MCPの認証が必要です", "認証待ちのMCPが起動処理を遅らせている可能性があります。"),
        ("method-not-found", "mcp", MaintenanceSeverity::Attention, "MCPとの機能互換性に問題があります", "接続先が現在のCodexから要求されたメソッドへ対応していません。"),
        ("unknown-feature", "configuration", MaintenanceSeverity::Attention, "古いCodex設定キーが使われています", "現在のCodexが認識しないfeature keyがログへ記録されています。"),
        ("invalid-feature", "configuration", MaintenanceSeverity::Attention, "無効な実験設定があります", "現在のCodexが無視したexperimental feature keyがあります。"),
        ("git-failure", "git", MaintenanceSeverity::Attention, "Git状態確認の失敗が記録されています", "remote、upstream、または巨大差分が原因の可能性があります。"),
    ] {
        let count = scan.counts.get(id).copied().unwrap_or(0);
        if count == 0 {
            continue;
        }
        findings.push(finding(
            &format!("log-{id}"),
            category,
            severity,
            title,
            summary,
            vec![format!("直近ログの集計件数: {count}")],
            vec![display_path(logs, home)],
            vec![],
            None,
            false,
            "medium",
            detected_at_ms,
        ));
    }
}

fn mcp_statuses(config: &Path, scan: &LogScan, failures: &mut Vec<String>) -> Vec<MaintenanceMcpStatus> {
    let mut configured = BTreeMap::new();
    if fs::metadata(config).is_ok_and(|metadata| metadata.len() > 4 * 1024 * 1024) {
        failures.push("Codex設定が4MiBを超えるため、MCP一覧の解析を省略しました。".into());
        return Vec::new();
    }
    if let Ok(raw) = fs::read_to_string(config) {
        match raw.parse::<DocumentMut>() {
            Ok(document) => {
                if let Some(servers) = document.get("mcp_servers").and_then(|item| item.as_table()) {
                    for (name, item) in servers.iter() {
                        let enabled = item
                            .as_table_like()
                            .and_then(|table| table.get("enabled"))
                            .and_then(|value| value.as_bool())
                            .unwrap_or(true);
                        configured.insert(name.to_string(), enabled);
                    }
                }
            }
            Err(_) => failures.push("Codex設定が認識できるTOMLではないため、MCP一覧を取得できませんでした。".into()),
        }
    } else if config.exists() {
        failures.push("Codex設定からMCP一覧を読み取れませんでした。".into());
    }
    configured
        .into_iter()
        .map(|(name, enabled)| {
            let error_count = scan.mcp_errors.get(&name).copied().unwrap_or(0);
            let auth_error_count = scan.mcp_auth_errors.get(&name).copied().unwrap_or(0);
            let startup_ms = scan.mcp_startup_ms.get(&name).copied();
            MaintenanceMcpStatus {
                name: redact_email_components(&name),
                configured: true,
                enabled,
                startup_ms,
                error_count,
                auth_error_count,
                disable_candidate: enabled
                    && (auth_error_count > 0
                        || error_count >= 3
                        || startup_ms.is_some_and(|value| value >= 10_000)),
                status: if !enabled {
                    MaintenanceSeverity::Unknown
                } else if auth_error_count > 0 || error_count > 0 {
                    MaintenanceSeverity::Attention
                } else {
                    MaintenanceSeverity::Healthy
                },
            }
        })
        .collect()
}

fn project_roots(state_path: &Path, failures: &mut Vec<String>) -> Vec<PathBuf> {
    if fs::metadata(state_path).is_ok_and(|metadata| metadata.len() > 8 * 1024 * 1024) {
        failures.push("Codexグローバル状態が8MiBを超えるため、Git診断を省略しました。".into());
        return Vec::new();
    }
    let Ok(bytes) = fs::read(state_path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        failures.push("Git診断用のプロジェクト一覧を読み取れませんでした。".into());
        return Vec::new();
    };
    let mut roots = BTreeSet::new();
    if let Some(values) = value.get("active-workspace-roots").and_then(Value::as_array) {
        for path in values.iter().filter_map(Value::as_str) {
            roots.insert(PathBuf::from(path));
        }
    }
    if let Some(projects) = value.get("local-projects").and_then(Value::as_object) {
        for project in projects.values() {
            if let Some(values) = project.get("rootPaths").and_then(Value::as_array) {
                for path in values.iter().filter_map(Value::as_str) {
                    roots.insert(PathBuf::from(path));
                }
            }
        }
    }
    let mut normalized = BTreeSet::new();
    for path in roots {
        if !path.is_absolute() {
            continue;
        }
        match fs::canonicalize(&path) {
            Ok(path) if path.is_dir() && path.join(".git").exists() => {
                normalized.insert(path);
            }
            Ok(_) => {}
            Err(_) => failures.push(format!("{} を正規化できないため、Git診断から除外しました。", path.display())),
        }
    }
    normalized.into_iter().take(MAX_GIT_REPOSITORIES).collect()
}

fn git_diagnostics(
    roots: &[PathBuf],
    home: &Path,
    failures: &mut Vec<String>,
) -> Vec<MaintenanceGitStatus> {
    roots
        .iter()
        .map(|root| git_status(root, home))
        .inspect(|status| {
            if status.status == MaintenanceSeverity::Unknown {
                failures.push(format!("{} のGit診断を完了できませんでした。", status.path));
            }
        })
        .collect()
}

fn git_status(root: &Path, home: &Path) -> MaintenanceGitStatus {
    let status_output = run_git(root, &["status", "--porcelain=v1", "--untracked-files=normal"]);
    let Ok(status_output) = status_output else {
        return MaintenanceGitStatus {
            path: display_path(root, home),
            status: MaintenanceSeverity::Unknown,
            changed_files: None,
            untracked_files: None,
            estimated_diff_bytes: None,
            upstream_configured: None,
            origin_configured: None,
            generated_file_candidates: Vec::new(),
            gitignore_candidates: Vec::new(),
            note: "Git statusがタイムアウトしたか、実行できませんでした。".into(),
        };
    };
    if !status_output.success {
        return MaintenanceGitStatus {
            path: display_path(root, home),
            status: MaintenanceSeverity::Unknown,
            changed_files: None,
            untracked_files: None,
            estimated_diff_bytes: None,
            upstream_configured: None,
            origin_configured: None,
            generated_file_candidates: Vec::new(),
            gitignore_candidates: Vec::new(),
            note: "Git statusが失敗しました。リポジトリ設定を確認してください。".into(),
        };
    }
    let rows = status_output.stdout.lines().take(10_000).collect::<Vec<_>>();
    let changed_files = rows.len() as u64;
    let untracked_files = rows.iter().filter(|line| line.starts_with("??")).count() as u64;
    let changed_paths = rows
        .iter()
        .filter_map(|line| line.get(3..))
        .map(|value| value.split(" -> ").last().unwrap_or(value).trim())
        .collect::<Vec<_>>();
    let generated_file_candidates = changed_paths
        .iter()
        .filter(|path| looks_generated(path))
        .take(12)
        .map(|path| redact_email_components(path))
        .collect::<Vec<_>>();
    let gitignore_candidates = gitignore_candidates(&changed_paths);
    let estimated_diff_bytes = run_git(
        root,
        &["diff", "--numstat", "--no-ext-diff", "--no-textconv"],
    )
        .ok()
        .filter(|result| result.success)
        .map(|result| estimate_numstat_bytes(&result.stdout));
    let upstream_configured = run_git(
        root,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .ok()
    .map(|result| result.success);
    let origin_configured = run_git(root, &["remote", "get-url", "origin"])
        .ok()
        .map(|result| result.success && !result.stdout.trim().is_empty());
    let large = changed_files >= 100
        || estimated_diff_bytes.is_some_and(|bytes| bytes >= 1024 * 1024)
        || generated_file_candidates.len() >= 8;
    let configuration_problem = upstream_configured == Some(false) || origin_configured == Some(false);
    let status = if large || configuration_problem {
        MaintenanceSeverity::Attention
    } else {
        MaintenanceSeverity::Healthy
    };
    let note = if large {
        "未コミット差分または生成ファイル候補が多く、CodexのGit状態取得が遅くなる可能性があります。自動変更は行いません。"
    } else if configuration_problem {
        "upstreamまたはorigin remoteが設定されていません。自動修正は行いません。"
    } else {
        "Git状態に大きな遅延候補は見つかりませんでした。"
    };
    MaintenanceGitStatus {
        path: display_path(root, home),
        status,
        changed_files: Some(changed_files),
        untracked_files: Some(untracked_files),
        estimated_diff_bytes,
        upstream_configured,
        origin_configured,
        generated_file_candidates,
        gitignore_candidates,
        note: note.into(),
    }
}

fn run_git(root: &Path, arguments: &[&str]) -> Result<CommandResult, String> {
    let mut args = vec![
        OsStr::new("--no-optional-locks"),
        OsStr::new("-c"),
        OsStr::new("core.fsmonitor=false"),
        OsStr::new("-c"),
        OsStr::new("core.untrackedCache=false"),
        OsStr::new("-C"),
        root.as_os_str(),
    ];
    args.extend(arguments.iter().map(|argument| OsStr::new(*argument)));
    run_command("/usr/bin/git", &args, None, COMMAND_TIMEOUT)
}

fn estimate_numstat_bytes(output: &str) -> u64 {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let added = fields.next()?.parse::<u64>().ok()?;
            let removed = fields.next()?.parse::<u64>().ok()?;
            Some(added.saturating_add(removed).saturating_mul(80))
        })
        .sum()
}

fn looks_generated(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [
        "/target/", "/dist/", "/build/", "/coverage/", "/renders/", "/reports/",
        ".png", ".jpg", ".jpeg", ".webp", ".gif", ".glb", ".gltf", ".pdf", ".zip",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
}

fn gitignore_candidates(paths: &[&str]) -> Vec<String> {
    let mut candidates = BTreeSet::new();
    for path in paths {
        let normalized = path.trim_start_matches("./").replace('\\', "/");
        let lower = normalized.to_ascii_lowercase();
        for directory in ["target", "dist", "build", "coverage", "renders", "reports"] {
            let marker = format!("/{directory}/");
            if lower.starts_with(&format!("{directory}/")) || lower.contains(&marker) {
                candidates.insert(format!("{directory}/ — 生成物ディレクトリ候補"));
            }
        }
    }
    candidates.into_iter().take(8).collect()
}

fn run_command(
    program: &str,
    arguments: &[&OsStr],
    current_dir: Option<&Path>,
    timeout: Duration,
) -> Result<CommandResult, String> {
    let mut command = Command::new(program);
    command.args(arguments).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(directory) = current_dir {
        command.current_dir(directory);
    }
    let mut child = command
        .group_spawn()
        .map_err(|error| format!("{}を開始できませんでした: {error}", program_name(program)))?;
    let stdout = child
        .inner()
        .stdout
        .take()
        .ok_or("標準出力を取得できませんでした")?;
    let stderr = child
        .inner()
        .stderr
        .take()
        .ok_or("標準エラーを取得できませんでした")?;
    let stdout_reader = thread::spawn(move || read_bounded(stdout));
    let stderr_reader = thread::spawn(move || read_bounded(stderr));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = child.kill();
                let _ = child.wait();
                break status;
            }
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("{}が{}秒でタイムアウトしました", program_name(program), timeout.as_secs()));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("{}の状態確認に失敗しました: {error}", program_name(program)));
            }
        }
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    Ok(CommandResult {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        success: status.success(),
    })
}

fn read_bounded(mut reader: impl Read) -> Vec<u8> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let Ok(read) = reader.read(&mut buffer) else {
            break;
        };
        if read == 0 {
            break;
        }
        let remaining = MAX_COMMAND_OUTPUT.saturating_sub(output.len());
        if remaining > 0 {
            output.extend_from_slice(&buffer[..read.min(remaining)]);
        }
    }
    output
}

fn command_error(label: &str, result: &CommandResult) -> String {
    let detail = result.stderr.lines().next().unwrap_or("詳細なし");
    format!("{label}が失敗しました: {}", redact(detail))
}

fn redact(value: &str) -> String {
    let mut output = value.to_string();
    for marker in ["Bearer ", "token=", "api_key=", "apikey="] {
        if let Some(index) = output.to_ascii_lowercase().find(&marker.to_ascii_lowercase()) {
            output.truncate(index);
            output.push_str("<redacted>");
        }
    }
    output.chars().take(300).collect()
}

fn program_name(program: &str) -> &str {
    Path::new(program)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(program)
}

fn display_path(path: &Path, home: &Path) -> String {
    let displayed = path.strip_prefix(home)
        .map(|relative| format!("~/{}", relative.to_string_lossy()))
        .unwrap_or_else(|_| path.to_string_lossy().into_owned());
    redact_email_components(&displayed)
}

fn redact_email_components(value: &str) -> String {
    value
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

fn sanitize_diagnostic_text(value: &str, home: &Path) -> String {
    let home = home.to_string_lossy();
    redact_email_components(&redact(&value.replace(home.as_ref(), "~")))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTIVE_ID: &str = "01a00437-ad1c-7f00-89b1-6ff230b6f29d";
    const MISSING_ID: &str = "019ffb9c-7595-7022-a9de-81be2d15578c";

    #[test]
    fn session_id_uses_only_a_valid_uuid_suffix() {
        let path = Path::new("rollout-日本語-01a00437-ad1c-7f00-89b1-6ff230b6f29d.jsonl");
        assert_eq!(session_id_from_path(path).as_deref(), Some(ACTIVE_ID));
        assert_eq!(session_id_from_path(Path::new("not-a-session.jsonl")), None);
        assert_eq!(session_id_from_path(Path::new("01a00437-ad1c-7f00-89b1-6ff230b6f29d.txt")), None);
    }

    #[test]
    fn orphan_pins_require_a_complete_session_scan() {
        let mut failures = Vec::new();
        let candidates = orphan_pin_candidates(
            Path::new("/unused"),
            &BTreeSet::new(),
            false,
            &mut failures,
        );
        assert!(candidates.is_empty());
        assert!(!failures.is_empty());
    }

    #[test]
    fn orphan_pins_keep_existing_and_ignore_malformed_ids() {
        let path = std::env::temp_dir().join(format!(
            "codetas-maintenance-pins-{}-{}.json",
            std::process::id(),
            now_ms()
        ));
        fs::write(
            &path,
            format!(
                r#"{{"pinned-thread-ids":["{ACTIVE_ID}","{MISSING_ID}","not-a-thread"]}}"#
            ),
        )
        .expect("write test state");
        let session_ids = BTreeSet::from([ACTIVE_ID.to_string()]);
        let mut failures = Vec::new();
        let candidates = orphan_pin_candidates(&path, &session_ids, true, &mut failures);
        let _ = fs::remove_file(path);
        assert_eq!(candidates, vec![MISSING_ID.to_string()]);
        assert!(failures.is_empty());
    }

    #[test]
    fn lsof_parser_keeps_process_and_file_association() {
        let parsed = parse_lsof("p123\ncCodex\nn/tmp/a.jsonl\np456\ncnode\nn/tmp/b.jsonl\n");
        assert_eq!(
            parsed,
            vec![
                (123, "Codex".into(), "/tmp/a.jsonl".into()),
                (456, "node".into(), "/tmp/b.jsonl".into()),
            ]
        );
    }

    #[test]
    fn report_paths_hide_home_and_email_components() {
        let home = Path::new("/Users/example");
        assert_eq!(
            display_path(Path::new("/Users/example/project/person@example.com/log"), home),
            "~/project/<redacted-email>/log"
        );
    }

    #[test]
    fn gitignore_suggestions_are_directory_scoped_and_deduplicated() {
        let suggestions = gitignore_candidates(&[
            "renders/preview.png",
            "renders/final.png",
            "packages/app/dist/index.js",
            "notes/image.png",
        ]);
        assert_eq!(
            suggestions,
            vec![
                "dist/ — 生成物ディレクトリ候補".to_string(),
                "renders/ — 生成物ディレクトリ候補".to_string(),
            ]
        );
    }

    #[test]
    fn codex_process_filter_does_not_count_similarly_named_tools() {
        assert!(is_codex_process_name(
            "/Applications/ChatGPT.app/Contents/Resources/codex"
        ));
        assert!(is_codex_process_name("Codex (Renderer)"));
        assert!(!is_codex_process_name("/tmp/opencodex/node_modules/bun"));
        assert!(!is_codex_process_name("codetas-desktop"));
    }
}
