use crate::provider_gateway::{atomic_write, codex_home};
use command_group::CommandGroup;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use toml_edit::{value, DocumentMut};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::OsStr,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
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
const MAX_CONTEXT_SKILLS: usize = 80;
const MAX_SKILL_SCAN_DEPTH: usize = 4;
const MAX_SKILL_DESCRIPTION_CHARS: usize = 160;
const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CONTEXT_FILE_BYTES: usize = 256 * 1024;
const OVERSIZED_SESSION_BYTES: u64 = 32 * 1024 * 1024;
const CRITICAL_SESSION_BYTES: u64 = 128 * 1024 * 1024;
const MAX_OVERSIZED_SESSIONS: usize = 8;
const OVERSIZED_SQLITE_LOG_ROW_BYTES: u64 = 1024 * 1024;
const SQLITE_LOG_BLOAT_BYTES: u64 = 256 * 1024 * 1024;
const SQLITE_LOG_BLOAT_CRITICAL_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum MaintenanceSeverity {
    #[default]
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
    thread_name: Option<String>,
    preview: Option<String>,
    thread_status: Option<String>,
    updated_at_ms: Option<u64>,
    cwd: Option<String>,
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceSkillEntry {
    id: String,
    name: String,
    source: String,
    enabled: bool,
    path: String,
    description_preview: Option<String>,
    content: String,
    truncated: bool,
    writable: bool,
    read_failed: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceInstructionSource {
    id: String,
    label: String,
    path: Option<String>,
    present: bool,
    bytes: Option<u64>,
    note: String,
    content: String,
    truncated: bool,
    writable: bool,
    read_failed: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveContextDocumentInput {
    id: String,
    content: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSkillEnabledInput {
    id: String,
    enabled: bool,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceContextLoad {
    skill_count: usize,
    enabled_skill_count: usize,
    disabled_skill_count: usize,
    scan_truncated: bool,
    skills: Vec<MaintenanceSkillEntry>,
    instruction_sources: Vec<MaintenanceInstructionSource>,
    status: MaintenanceSeverity,
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
    orphan_processes: Vec<MaintenanceProcessInfo>,
    processes: Vec<MaintenanceProcessInfo>,
    mcp: Vec<MaintenanceMcpStatus>,
    mcp_max_startup_ms: Option<u64>,
    git: Vec<MaintenanceGitStatus>,
    context_load: MaintenanceContextLoad,
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
            orphan_processes: Vec::new(),
            processes: Vec::new(),
            mcp: Vec::new(),
            mcp_max_startup_ms: None,
            git: Vec::new(),
            context_load: MaintenanceContextLoad::default(),
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
            orphan_processes: Vec::new(),
            processes: Vec::new(),
            mcp: Vec::new(),
            mcp_max_startup_ms: None,
            git: Vec::new(),
            context_load: MaintenanceContextLoad::default(),
            system: MaintenanceSystemHealth::default(),
            partial_failures: vec!["ホームディレクトリを解決できませんでした。".into()],
        };
    };

    let Ok(codex_root) = codex_home() else {
        return MaintenanceReport {
            schema_version: 1,
            generated_at_ms: detected_at_ms,
            duration_ms: started.elapsed().as_millis() as u64,
            platform: std::env::consts::OS.into(),
            overall_status: MaintenanceSeverity::Unknown,
            read_only: true,
            privacy_note: privacy_note(),
            findings: vec![finding(
                "codex-home-unavailable",
                "system",
                MaintenanceSeverity::Unknown,
                "Codexの保存場所を確認できません",
                "CODEX_HOME を解決できなかったため、診断を開始できませんでした。",
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
            orphan_processes: Vec::new(),
            processes: Vec::new(),
            mcp: Vec::new(),
            mcp_max_startup_ms: None,
            git: Vec::new(),
            context_load: MaintenanceContextLoad::default(),
            system: MaintenanceSystemHealth::default(),
            partial_failures: vec!["CODEX_HOME を解決できませんでした。".into()],
        };
    };
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
    let orphan_processes = orphan_codex_processes(&process_map, &processes);

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
    match sqlite_log_content_stats(&database) {
        Ok(Some(stats)) => add_sqlite_log_bloat_finding(
            &stats,
            &sqlite.path,
            detected_at_ms,
            &mut findings,
        ),
        Ok(None) => {}
        Err(error) => partial_failures.push(format!("SQLiteログ本文の集計: {error}")),
    }

    let (session_ids, session_scan_complete) =
        collect_session_ids(&[&sessions, &archived], &mut partial_failures);
    let (oversized_sessions, _oversized_scan_complete) =
        collect_oversized_sessions(&[&sessions, &archived], &mut partial_failures);
    add_oversized_session_findings(
        &oversized_sessions,
        detected_at_ms,
        &home,
        &mut findings,
    );
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

    let mut file_locks = lsof_directory(&sessions, &home, &process_map, &mut partial_failures);
    let locked_thread_ids = file_locks
        .iter()
        .filter_map(|lock| lock.thread_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    match crate::codex_app_server::read_codex_thread_summaries(&locked_thread_ids) {
        Ok(summaries) => {
            for lock in &mut file_locks {
                let Some(thread_id) = lock.thread_id.as_ref() else {
                    continue;
                };
                let Some(summary) = summaries.get(thread_id) else {
                    continue;
                };
                lock.thread_name = summary.name.clone();
                lock.preview = summary.preview.clone();
                lock.thread_status = summary.status.clone();
                lock.updated_at_ms = summary.updated_at_ms;
                lock.cwd = summary.cwd.clone();
            }
        }
        Err(error) if !locked_thread_ids.is_empty() => {
            partial_failures.push(format!("タスク名とプレビューの取得: {error}"));
        }
        Err(_) => {}
    }
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
    if !orphan_processes.is_empty() {
        findings.push(finding(
            "orphan-codex-processes",
            "processes",
            MaintenanceSeverity::Attention,
            "親プロセスが見つからないCodexプロセスがあります",
            "通常のアプリやターミナルから切り離されたCodex CLIの可能性があります。PIDと起動時刻を確認してください。",
            orphan_processes
                .iter()
                .map(|process| format!("PID {} {}", process.pid, process.name))
                .collect(),
            vec![],
            vec![],
            None,
            false,
            "medium",
            detected_at_ms,
        ));
    }

    let log_scan = scan_logs(&logs, &mut partial_failures);
    add_log_findings(&log_scan, detected_at_ms, &logs, &home, &mut findings);
    let mcp = mcp_statuses(&config, &log_scan, &mut partial_failures);
    let context_load = context_load_diagnostics(&codex_root, &config, &home, &mut partial_failures);
    add_context_load_findings(&context_load, detected_at_ms, &mut findings);
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
        .chain(std::iter::once(context_load.status))
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
        orphan_processes,
        processes,
        mcp,
        mcp_max_startup_ms: log_scan.mcp_max_startup_ms,
        git,
        context_load,
        system,
        partial_failures,
    }
}

fn privacy_note() -> String {
    "診断レポートは容量、件数、設定名、プロセス情報、集計済みエラー、グローバルスキルと指示の本文に加え、要対応セッションのスレッド名と短いプレビューを含みます。会話本文全体・ログ本文・認証情報・メールアドレスは収集しません。ホームパスは ~ として表示します。".into()
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
        "sqlite-reclaimable" | "sqlite-read-latency" | "sqlite-log-bloat" => {
            Some("compact-sqlite".into())
        }
        "oversized-sessions" => Some("trash-oversized-sessions".into()),
        "orphan-pins" | "log-thread-not-loaded" => Some("remove-orphan-pins".into()),
        "session-writers" | "log-active-writer" => Some("retry-archive".into()),
        "mcp-startup" | "log-auth-required" | "log-method-not-found" => {
            Some("review-mcp".into())
        }
        "global-skills-loaded" | "global-instructions-loaded" => {
            Some("review-global-context".into())
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct OversizedSession {
    id: String,
    bytes: u64,
    path: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SqliteLogContentStats {
    row_count: u64,
    estimated_body_bytes: u64,
    max_row_bytes: u64,
    prunable_bytes: u64,
    top_targets: Vec<String>,
}

fn oversized_session_severity(max_bytes: u64) -> Option<MaintenanceSeverity> {
    if max_bytes >= CRITICAL_SESSION_BYTES {
        Some(MaintenanceSeverity::Critical)
    } else if max_bytes >= OVERSIZED_SESSION_BYTES {
        Some(MaintenanceSeverity::Attention)
    } else {
        None
    }
}

fn sqlite_log_bloat_severity(stats: &SqliteLogContentStats) -> Option<MaintenanceSeverity> {
    if stats.max_row_bytes >= 8 * 1024 * 1024
        || stats.estimated_body_bytes >= SQLITE_LOG_BLOAT_CRITICAL_BYTES
    {
        Some(MaintenanceSeverity::Critical)
    } else if stats.max_row_bytes >= OVERSIZED_SQLITE_LOG_ROW_BYTES
        || stats.estimated_body_bytes >= SQLITE_LOG_BLOAT_BYTES
        || stats.prunable_bytes >= OVERSIZED_SQLITE_LOG_ROW_BYTES
    {
        Some(MaintenanceSeverity::Attention)
    } else {
        None
    }
}

fn collect_oversized_sessions(
    paths: &[&Path],
    failures: &mut Vec<String>,
) -> (Vec<OversizedSession>, bool) {
    let mut oversized = Vec::new();
    let mut complete = true;
    for path in paths {
        if !path.exists() {
            continue;
        }
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut stack = vec![path.to_path_buf()];
        'directories: while let Some(directory) = stack.pop() {
            if Instant::now() >= deadline {
                complete = false;
                failures.push(format!("{} の巨大セッション走査がタイムアウトしました。", path.display()));
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
                    failures.push(format!("{} の巨大セッション走査がタイムアウトしました。", path.display()));
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
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                let entry_path = entry.path();
                let Some(id) = session_id_from_path(&entry_path) else {
                    continue;
                };
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                if metadata.len() < OVERSIZED_SESSION_BYTES {
                    continue;
                }
                oversized.push(OversizedSession {
                    id,
                    bytes: metadata.len(),
                    path: entry_path,
                });
            }
        }
    }
    oversized.sort_by(|left, right| right.bytes.cmp(&left.bytes).then(left.id.cmp(&right.id)));
    (oversized, complete)
}

fn add_oversized_session_findings(
    oversized: &[OversizedSession],
    detected_at_ms: u64,
    home: &Path,
    findings: &mut Vec<MaintenanceFinding>,
) {
    let Some(max_bytes) = oversized.iter().map(|item| item.bytes).max() else {
        return;
    };
    let Some(severity) = oversized_session_severity(max_bytes) else {
        return;
    };
    let reclaimable = oversized.iter().map(|item| item.bytes).sum();
    let reported = oversized.iter().take(MAX_OVERSIZED_SESSIONS);
    findings.push(finding(
        "oversized-sessions",
        "tasks",
        severity,
        "個別のタスクファイルが異常に大きくなっています",
        "ディレクトリ合計ではなく、巨大な rollout JSONL 自体がタスクの再開・描画・同一App Server上の他スレッドを遅くします。会話本文は読んでいません。今すぐ整理で32MiB超のファイルだけを専用ごみ箱へ退避できます。",
        reported
            .clone()
            .map(|item| {
                format!(
                    "{}  {}  {} bytes",
                    item.id,
                    display_path(&item.path, home),
                    item.bytes
                )
            })
            .chain(
                (oversized.len() > MAX_OVERSIZED_SESSIONS).then(|| {
                    format!(
                        "他 {} 件の巨大セッションは省略しました",
                        oversized.len() - MAX_OVERSIZED_SESSIONS
                    )
                }),
            )
            .collect(),
        oversized
            .iter()
            .take(MAX_OVERSIZED_SESSIONS)
            .map(|item| display_path(&item.path, home))
            .collect(),
        oversized
            .iter()
            .take(MAX_OVERSIZED_SESSIONS)
            .map(|item| item.id.clone())
            .collect(),
        Some(reclaimable),
        false,
        "high",
        detected_at_ms,
    ));
}

fn sqlite_log_content_stats(path: &Path) -> Result<Option<SqliteLogContentStats>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let immutable_uri = sqlite_immutable_uri(path);
    let sql = format!(
        "SELECT count(*) FROM logs; \
         SELECT ifnull(sum(estimated_bytes),0) FROM logs; \
         SELECT ifnull(max(estimated_bytes),0) FROM logs; \
         SELECT ifnull(sum(CASE WHEN estimated_bytes >= {OVERSIZED_SQLITE_LOG_ROW_BYTES} THEN estimated_bytes ELSE 0 END),0) FROM logs; \
         SELECT printf('%s %s %s', target, count(*), ifnull(sum(estimated_bytes),0)) FROM logs GROUP BY target ORDER BY sum(estimated_bytes) DESC LIMIT 5;"
    );
    let result = run_command(
        "/usr/bin/sqlite3",
        &[
            OsStr::new("-readonly"),
            OsStr::new(&immutable_uri),
            OsStr::new(&sql),
        ],
        None,
        SLOW_COMMAND_TIMEOUT,
    )?;
    if !result.success {
        if result.stderr.contains("no such table: logs") || result.stdout.contains("no such table: logs") {
            return Ok(None);
        }
        return Err(command_error("sqlite3 log aggregates", &result));
    }
    let mut lines = result
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let row_count = lines
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or("ログ行数を読み取れませんでした")?;
    let estimated_body_bytes = lines
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or("ログ本文サイズを読み取れませんでした")?;
    let max_row_bytes = lines
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or("最大ログ行サイズを読み取れませんでした")?;
    let prunable_bytes = lines
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or("巨大ログ行サイズを読み取れませんでした")?;
    Ok(Some(SqliteLogContentStats {
        row_count,
        estimated_body_bytes,
        max_row_bytes,
        prunable_bytes,
        top_targets: lines.map(str::to_string).collect(),
    }))
}

fn add_sqlite_log_bloat_finding(
    stats: &SqliteLogContentStats,
    sqlite_path: &str,
    detected_at_ms: u64,
    findings: &mut Vec<MaintenanceFinding>,
) {
    let Some(severity) = sqlite_log_bloat_severity(stats) else {
        return;
    };
    findings.push(finding(
        "sqlite-log-bloat",
        "database",
        severity,
        "ログデータベースの本文が肥大化しています",
        "未使用ページではなく、巨大な診断ログ行が live データの大半です。VACUUM だけでは回収できません。圧縮ジョブは1MiB以上の行をバックアップコピー上で削除してから圧縮します。会話履歴は対象外です。",
        vec![
            format!("log_rows={}", stats.row_count),
            format!("estimated_body_bytes={}", stats.estimated_body_bytes),
            format!("max_row_bytes={}", stats.max_row_bytes),
            format!("prunable_bytes={}", stats.prunable_bytes),
        ]
        .into_iter()
        .chain(
            stats
                .top_targets
                .iter()
                .map(|target| format!("target {target}")),
        )
        .collect(),
        vec![sqlite_path.to_string()],
        vec![],
        Some(stats.prunable_bytes).filter(|bytes| *bytes > 0),
        true,
        "high",
        detected_at_ms,
    ));
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

fn orphan_codex_processes(
    process_map: &HashMap<u32, MaintenanceProcessInfo>,
    codex_processes: &[MaintenanceProcessInfo],
) -> Vec<MaintenanceProcessInfo> {
    let mut candidates = codex_processes
        .iter()
        .filter(|process| {
            let executable = Path::new(&process.name)
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or(&process.name);
            if !executable.eq_ignore_ascii_case("codex") {
                return false;
            }
            match process.parent_pid {
                Some(1) | None => true,
                Some(parent_pid) => !process_map.contains_key(&parent_pid),
            }
        })
        .cloned()
        .collect::<Vec<_>>();
    candidates.sort_by_key(|process| process.pid);
    candidates
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
                thread_name: None,
                preview: None,
                thread_status: None,
                updated_at_ms: None,
                cwd: None,
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

fn context_load_diagnostics(
    codex_root: &Path,
    config: &Path,
    home: &Path,
    failures: &mut Vec<String>,
) -> MaintenanceContextLoad {
    let mut disabled_skill_paths = BTreeSet::new();
    let mut model_instructions_path: Option<PathBuf> = None;
    if fs::metadata(config).is_ok_and(|metadata| metadata.len() > MAX_CONFIG_BYTES) {
        failures.push("Codex設定が4MiBを超えるため、グローバルスキルと追加指示の解析を省略しました。".into());
    } else if let Ok(raw) = fs::read_to_string(config) {
        match raw.parse::<DocumentMut>() {
            Ok(document) => {
                model_instructions_path = document
                    .get("model_instructions_file")
                    .and_then(toml_item_string)
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .map(|value| {
                        let path = PathBuf::from(value);
                        if path.is_absolute() {
                            path
                        } else {
                            codex_root.join(path)
                        }
                    });
                if let Some(config) = document.get("skills").and_then(|item| item.get("config")) {
                    collect_disabled_skill_paths(config, home, codex_root, &mut disabled_skill_paths);
                }
            }
            Err(_) => failures.push(
                "Codex設定が認識できるTOMLではないため、グローバルスキルと追加指示を取得できませんでした。"
                    .into(),
            ),
        }
    } else if config.exists() {
        failures.push("Codex設定からグローバルスキルと追加指示を読み取れませんでした。".into());
    }

    let mut skills = Vec::new();
    let mut seen = BTreeSet::new();
    let mut truncated = false;
    let roots = [
        (codex_root.join("skills"), "user-skills"),
        (home.join(".agents/skills"), "user-agents-skills"),
    ];
    for (directory, source) in roots {
        if scan_user_skills(
            &directory,
            source,
            home,
            codex_root,
            &disabled_skill_paths,
            &mut skills,
            &mut seen,
            &mut truncated,
        ) {
            break;
        }
    }
    skills.sort_by(|left, right| left.path.cmp(&right.path).then(left.name.cmp(&right.name)));
    let enabled_skill_count = skills.iter().filter(|skill| skill.enabled).count();
    let disabled_skill_count = skills.len().saturating_sub(enabled_skill_count);

    let agents_home = home.join("AGENTS.md");
    let agents_codex = codex_root.join("AGENTS.md");
    let mut instruction_sources = vec![
        instruction_source(
            "user-agents",
            "ホームの AGENTS.md",
            Some(&agents_home),
            home,
            codex_root,
            "ユーザー全体の作業指示。プロジェクトなしの定期実行にも乗ります。",
        ),
        instruction_source(
            "codex-home-agents",
            "Codexホームの AGENTS.md",
            Some(&agents_codex),
            home,
            codex_root,
            "CODEX_HOME 配下の作業指示です。",
        ),
        instruction_source(
            "developer-instructions",
            "config.toml の developer_instructions",
            Some(config),
            home,
            codex_root,
            "ユーザー設定の追加開発者指示です。この画面から本文を保存できます。",
        ),
    ];
    if let Some(path) = model_instructions_path {
        instruction_sources.push(instruction_source(
            "model-instructions-file",
            "model_instructions_file",
            Some(&path),
            home,
            codex_root,
            "組み込み指示の代わりに読み込むファイルです。ホームまたは Codex ホーム内ならこの画面から保存できます。",
        ));
    }

    let present_instructions = instruction_sources
        .iter()
        .filter(|source| source.present)
        .count();
    let status = if truncated {
        MaintenanceSeverity::Unknown
    } else if skills.len() >= 12 || present_instructions >= 2 {
        MaintenanceSeverity::Attention
    } else if !skills.is_empty() || present_instructions > 0 {
        MaintenanceSeverity::Attention
    } else {
        MaintenanceSeverity::Healthy
    };

    MaintenanceContextLoad {
        skill_count: skills.len(),
        enabled_skill_count,
        disabled_skill_count,
        scan_truncated: truncated,
        skills,
        instruction_sources,
        status,
    }
}

fn add_context_load_findings(
    context: &MaintenanceContextLoad,
    detected_at_ms: u64,
    findings: &mut Vec<MaintenanceFinding>,
) {
    if context.skill_count > 0 {
        let enabled_names = context
            .skills
            .iter()
            .filter(|skill| skill.enabled)
            .map(|skill| skill.name.clone())
            .take(8)
            .collect::<Vec<_>>();
        findings.push(finding(
            "global-skills-loaded",
            "configuration",
            if context.scan_truncated {
                MaintenanceSeverity::Unknown
            } else {
                MaintenanceSeverity::Attention
            },
            "グローバルスキルが読み込まれます",
            "プロジェクトを外しても、ユーザーホームのスキルは定期実行や新規チャットに残ります。下の編集欄で本文を直して保存できます。",
            vec![
                format!("検出 {}件", context.skill_count),
                format!("有効 {}件", context.enabled_skill_count),
                format!("設定で無効 {}件", context.disabled_skill_count),
                if enabled_names.is_empty() {
                    "有効スキル名なし".into()
                } else {
                    format!("有効スキル: {}", enabled_names.join(", "))
                },
            ],
            context
                .skills
                .iter()
                .map(|skill| skill.path.clone())
                .take(12)
                .collect(),
            vec![],
            None,
            false,
            if context.scan_truncated { "medium" } else { "high" },
            detected_at_ms,
        ));
    }
    let present = context
        .instruction_sources
        .iter()
        .filter(|source| source.present)
        .collect::<Vec<_>>();
    if !present.is_empty() {
        findings.push(finding(
            "global-instructions-loaded",
            "configuration",
            MaintenanceSeverity::Attention,
            "ユーザー全体の追加指示があります",
            "空プロジェクトやプロジェクトなしチャットでも、ホームの AGENTS.md や config.toml の追加指示は読み込まれます。下の編集欄で本文を直して保存できます。",
            present
                .iter()
                .map(|source| format!("{}: {}", source.label, source.note))
                .collect(),
            present
                .iter()
                .filter_map(|source| source.path.clone())
                .collect(),
            vec![],
            None,
            false,
            "high",
            detected_at_ms,
        ));
    }
}

fn instruction_source(
    id: &str,
    label: &str,
    path: Option<&Path>,
    home: &Path,
    codex_root: &Path,
    note: &str,
) -> MaintenanceInstructionSource {
    if id == "developer-instructions" {
        let (content, writable, read_failed) = match path {
            Some(path) if path.exists() => match read_config_string_value(path, "developer_instructions") {
                Ok(value) => (value.unwrap_or_default(), !is_symlink(path), false),
                Err(_) => (String::new(), false, true),
            },
            _ => (String::new(), path.is_some_and(|path| !is_symlink(path)), false),
        };
        let present = !content.trim().is_empty();
        return MaintenanceInstructionSource {
            id: id.into(),
            label: label.into(),
            path: path.map(|path| display_context_path(path, home)),
            present,
            bytes: present.then_some(content.len() as u64),
            note: note.into(),
            content,
            truncated: false,
            writable,
            read_failed,
        };
    }
    let displayed = path.map(|path| display_context_path(path, home));
    let Some(path) = path else {
        return MaintenanceInstructionSource {
            id: id.into(),
            label: label.into(),
            path: displayed,
            present: false,
            bytes: None,
            note: note.into(),
            content: String::new(),
            truncated: false,
            writable: false,
            read_failed: false,
        };
    };
    let path_writable = !is_symlink(path)
        && allowed_context_path("instruction", path, home, codex_root, &[path.to_path_buf()]).is_ok();
    if !fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return MaintenanceInstructionSource {
            id: id.into(),
            label: label.into(),
            path: displayed,
            present: false,
            bytes: None,
            note: note.into(),
            content: String::new(),
            truncated: false,
            writable: path_writable,
            read_failed: false,
        };
    }
    match read_context_file(path) {
        Ok((content, truncated, bytes)) => MaintenanceInstructionSource {
            id: id.into(),
            label: label.into(),
            path: displayed,
            present: true,
            bytes: Some(bytes),
            note: note.into(),
            content,
            truncated,
            writable: path_writable && !truncated,
            read_failed: false,
        },
        Err(_) => MaintenanceInstructionSource {
            id: id.into(),
            label: label.into(),
            path: displayed,
            present: true,
            bytes: fs::metadata(path).ok().map(|metadata| metadata.len()),
            note: note.into(),
            content: String::new(),
            truncated: false,
            writable: false,
            read_failed: true,
        },
    }
}

fn expand_config_path(value: &str, home: &Path, codex_root: &Path) -> PathBuf {
    let path = expand_display_path(value.trim(), home);
    if path.is_absolute() {
        path
    } else {
        codex_root.join(path)
    }
}

fn collect_disabled_skill_paths(
    config: &toml_edit::Item,
    home: &Path,
    codex_root: &Path,
    disabled_skill_paths: &mut BTreeSet<String>,
) {
    let mut push_disabled = |enabled: Option<bool>, path: Option<&str>| {
        if enabled.unwrap_or(true) {
            return;
        }
        if let Some(path) = path.filter(|path| !path.is_empty()) {
            let expanded = expand_config_path(path, home, codex_root);
            disabled_skill_paths.insert(normalize_lookup_path(&expanded));
            disabled_skill_paths.insert(normalize_lookup_path(&skill_package_path(&expanded)));
        }
    };
    if let Some(array) = config.as_array() {
        for item in array.iter() {
            if let Some(table) = item.as_inline_table() {
                push_disabled(
                    table.get("enabled").and_then(|value| value.as_bool()),
                    table.get("path").and_then(|value| value.as_str()),
                );
            }
        }
        return;
    }
    if let Some(tables) = config.as_array_of_tables() {
        for table in tables.iter() {
            push_disabled(
                table.get("enabled").and_then(|value| value.as_bool()),
                table.get("path").and_then(|value| value.as_str()),
            );
        }
    }
}

fn toml_item_string(item: &toml_edit::Item) -> Option<String> {
    item.as_str()
        .map(str::to_string)
        .or_else(|| item.as_value().and_then(toml_edit::Value::as_str).map(str::to_string))
}

fn read_config_string_value(path: &Path, key: &str) -> Result<Option<String>, String> {
    let raw = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let document = raw.parse::<DocumentMut>().map_err(|error| error.to_string())?;
    Ok(document.get(key).and_then(toml_item_string))
}

fn read_context_file(path: &Path) -> Result<(String, bool, u64), String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    let bytes = metadata.len();
    let truncated = bytes as usize > MAX_CONTEXT_FILE_BYTES;
    if truncated {
        let content = read_bounded_text(path, MAX_CONTEXT_FILE_BYTES)?;
        return Ok((content, true, bytes));
    }
    let raw = fs::read(path).map_err(|error| error.to_string())?;
    let content = String::from_utf8(raw).map_err(|_| "UTF-8ではありません。".to_string())?;
    Ok((content, false, bytes))
}

fn sensitive_instruction_file(file_name: Option<&str>) -> bool {
    matches!(
        file_name,
        Some("config.toml")
            | Some("auth.json")
            | Some(".env")
            | Some("credentials.json")
            | Some("id_rsa")
            | Some("id_ed25519")
    )
}

fn context_kind_from_id(id: &str) -> Option<&'static str> {
    if id.starts_with("skill:") {
        Some("skill")
    } else if matches!(
        id,
        "user-agents" | "codex-home-agents" | "developer-instructions" | "model-instructions-file"
    ) {
        Some("instruction")
    } else {
        None
    }
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

fn path_contains_parent_dir(path: &Path) -> bool {
    path.components().any(|component| matches!(component, Component::ParentDir))
}

fn allowed_context_path(
    kind: &str,
    requested: &Path,
    home: &Path,
    codex_root: &Path,
    known_instruction_paths: &[PathBuf],
) -> Result<PathBuf, String> {
    if is_symlink(requested) {
        return Err("シンボリックリンクは保存できません。".into());
    }
    if path_contains_parent_dir(requested) {
        return Err("このファイルは診断画面から編集できません。".into());
    }
    let resolved = if requested.exists() {
        fs::canonicalize(requested).map_err(|error| format!("パスを解決できません: {error}"))?
    } else if let Some(parent) = requested.parent().filter(|parent| parent.exists()) {
        let canonical_parent = fs::canonicalize(parent)
            .map_err(|error| format!("フォルダを解決できません: {error}"))?;
        canonical_parent.join(requested.file_name().unwrap_or_default())
    } else {
        requested.to_path_buf()
    };
    if is_symlink(&resolved) {
        return Err("シンボリックリンクは保存できません。".into());
    }
    let home_canon = fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let codex_canon = fs::canonicalize(codex_root).unwrap_or_else(|_| codex_root.to_path_buf());
    let agents_skills = fs::canonicalize(home.join(".agents/skills"))
        .unwrap_or_else(|_| home_canon.join(".agents/skills"));
    let codex_skills = fs::canonicalize(codex_root.join("skills"))
        .unwrap_or_else(|_| codex_canon.join("skills"));
    let file_name = resolved.file_name().and_then(|name| name.to_str());
    let under_home_or_codex = resolved.starts_with(&home_canon) || resolved.starts_with(&codex_canon);
    let logical_skill = requested.starts_with(codex_root.join("skills"))
        || requested.starts_with(home.join(".agents/skills"));
    let allowed = match kind {
        "skill" => {
            file_name == Some("SKILL.md")
                && under_home_or_codex
                && (resolved.starts_with(&codex_skills)
                    || resolved.starts_with(&agents_skills)
                    || logical_skill)
        }
        "instruction" => {
            let known = known_instruction_paths.iter().any(|path| {
                fs::canonicalize(path).ok().as_ref() == Some(&resolved) || path == &resolved
            });
            under_home_or_codex
                && !sensitive_instruction_file(file_name)
                && (resolved == home_canon.join("AGENTS.md")
                    || resolved == codex_canon.join("AGENTS.md")
                    || known)
        }
        _ => false,
    };
    if allowed {
        Ok(resolved)
    } else {
        Err("このファイルは診断画面から編集できません。".into())
    }
}

fn atomic_write_text(path: &Path, content: &str) -> Result<(), String> {
    if is_symlink(path) {
        return Err("シンボリックリンクは保存できません。".into());
    }
    atomic_write(path, content.as_bytes())
}

#[tauri::command]
pub async fn save_codex_context_document(
    input: SaveContextDocumentInput,
) -> Result<MaintenanceContextLoad, String> {
    tauri::async_runtime::spawn_blocking(move || save_codex_context_document_blocking(input))
        .await
        .map_err(|error| format!("保存タスクを完了できませんでした: {error}"))?
}

fn save_codex_context_document_blocking(
    input: SaveContextDocumentInput,
) -> Result<MaintenanceContextLoad, String> {
    if input.content.len() > MAX_CONTEXT_FILE_BYTES {
        return Err("内容が256KiBを超えるため保存できません。".into());
    }
    let home = dirs::home_dir().ok_or_else(|| "ホームディレクトリを解決できません。".to_string())?;
    let codex_root = codex_home()?;
    let kind = context_kind_from_id(&input.id).ok_or_else(|| "未知の対象です。".to_string())?;
    let mut failures = Vec::new();
    let current = context_load_diagnostics(&codex_root, &codex_root.join("config.toml"), &home, &mut failures);
    if kind == "skill" {
        let skill = current
            .skills
            .iter()
            .find(|item| item.id == input.id)
            .ok_or_else(|| "対象スキルが見つかりません。再診断してください。".to_string())?;
        if skill.truncated || skill.read_failed || !skill.writable {
            return Err("先頭だけ表示しているか読めなかったため、上書き保存できません。".into());
        }
        let requested = expand_display_path(&skill.path, &home);
        let path = allowed_context_path("skill", &requested, &home, &codex_root, &[])?;
        atomic_write_text(&path, &input.content)?;
    } else {
        let source = current
            .instruction_sources
            .iter()
            .find(|item| item.id == input.id)
            .ok_or_else(|| "対象の指示ファイルが見つかりません。再診断してください。".to_string())?;
        if source.truncated || source.read_failed || !source.writable {
            return Err("先頭だけ表示しているか読めなかったため、上書き保存できません。".into());
        }
        if input.id == "developer-instructions" {
            let config = codex_root.join("config.toml");
            with_codex_config_lock(&codex_root, || {
                let raw = if config.exists() {
                    fs::read_to_string(&config).map_err(|error| format!("設定を読めません: {error}"))?
                } else {
                    String::new()
                };
                let mut document = if raw.trim().is_empty() {
                    DocumentMut::new()
                } else {
                    raw.parse::<DocumentMut>()
                        .map_err(|_| "Codex設定が認識できるTOMLではないため保存できません。".to_string())?
                };
                if input.content.trim().is_empty() {
                    document.as_table_mut().remove("developer_instructions");
                } else {
                    document["developer_instructions"] = value(input.content.clone());
                }
                atomic_write_text(&config, &document.to_string())
            })?;
        } else {
            let requested = source
                .path
                .as_ref()
                .map(|path| expand_display_path(path, &home))
                .ok_or_else(|| "保存先がありません。".to_string())?;
            let known = current
                .instruction_sources
                .iter()
                .filter_map(|item| item.path.as_ref())
                .map(|path| expand_display_path(path, &home))
                .filter_map(|path| fs::canonicalize(&path).ok().or(Some(path)))
                .collect::<Vec<_>>();
            let path = allowed_context_path("instruction", &requested, &home, &codex_root, &known)?;
            atomic_write_text(&path, &input.content)?;
        }
    }
    let mut failures = Vec::new();
    Ok(context_load_diagnostics(
        &codex_root,
        &codex_root.join("config.toml"),
        &home,
        &mut failures,
    ))
}

#[tauri::command]
pub async fn set_codex_skill_enabled(
    input: SetSkillEnabledInput,
) -> Result<MaintenanceContextLoad, String> {
    tauri::async_runtime::spawn_blocking(move || set_codex_skill_enabled_blocking(input))
        .await
        .map_err(|error| format!("スキル設定を更新できませんでした: {error}"))?
}

fn set_codex_skill_enabled_blocking(
    input: SetSkillEnabledInput,
) -> Result<MaintenanceContextLoad, String> {
    let home = dirs::home_dir().ok_or_else(|| "ホームディレクトリを解決できません。".to_string())?;
    let codex_root = codex_home()?;
    let mut failures = Vec::new();
    let current = context_load_diagnostics(&codex_root, &codex_root.join("config.toml"), &home, &mut failures);
    let skill = current
        .skills
        .iter()
        .find(|item| item.id == input.id)
        .ok_or_else(|| "対象スキルが見つかりません。再診断してください。".to_string())?;
    let requested = expand_display_path(&skill.path, &home);
    let canonical = fs::canonicalize(&requested).unwrap_or_else(|_| requested.clone());
    let package = skill_package_path(&requested);
    let canonical_package = skill_package_path(&canonical);
    let config_path = codex_root.join("config.toml");
    with_codex_config_lock(&codex_root, || {
        let raw = if config_path.exists() {
            fs::read_to_string(&config_path).map_err(|error| format!("設定を読めません: {error}"))?
        } else {
            String::new()
        };
        let mut document = if raw.trim().is_empty() {
            DocumentMut::new()
        } else {
            raw.parse::<DocumentMut>()
                .map_err(|_| "Codex設定が認識できるTOMLではないため保存できません。".to_string())?
        };
        set_skill_enabled_in_document(
            &mut document,
            &package,
            &canonical_package,
            &home,
            &codex_root,
            input.enabled,
        )?;
        atomic_write_text(&config_path, &document.to_string())
    })?;
    let mut failures = Vec::new();
    Ok(context_load_diagnostics(
        &codex_root,
        &config_path,
        &home,
        &mut failures,
    ))
}

struct CodexConfigLock {
    path: PathBuf,
}

impl Drop for CodexConfigLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn with_codex_config_lock<T>(
    codex_root: &Path,
    action: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let path = codex_root.join(".codetas-config.lock");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("設定フォルダを作れません: {error}"))?;
    }
    for _ in 0..50 {
        match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                let _ = fs::write(&path, std::process::id().to_string());
                drop(file);
                let _lock = CodexConfigLock { path };
                return action();
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if lock_file_is_stale(&path) {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(format!("設定ロックを取れません: {error}")),
        }
    }
    Err("設定が使用中のため、変更を見送りました。".into())
}

fn lock_file_is_stale(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return true;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|elapsed| elapsed > Duration::from_secs(5))
        .unwrap_or(false)
}

fn skill_package_path(path: &Path) -> PathBuf {
    if path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md") {
        path.parent().map(Path::to_path_buf).unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    }
}

fn skill_config_path_matches(
    configured: &str,
    requested: &Path,
    canonical: &Path,
    home: &Path,
    codex_root: &Path,
) -> bool {
    let expanded = expand_config_path(configured, home, codex_root);
    let expanded_package = skill_package_path(&expanded);
    let requested_package = skill_package_path(requested);
    let canonical_package = skill_package_path(canonical);
    let expanded_canon = fs::canonicalize(&expanded_package).unwrap_or_else(|_| expanded_package.clone());
    let requested_canon = fs::canonicalize(&requested_package).unwrap_or_else(|_| requested_package.clone());
    let canonical_canon = fs::canonicalize(&canonical_package).unwrap_or_else(|_| canonical_package.clone());
    expanded_package == requested_package
        || expanded_package == canonical_package
        || expanded_canon == requested_canon
        || expanded_canon == canonical_canon
        || normalize_lookup_path(&expanded_package) == normalize_lookup_path(&requested_package)
        || normalize_lookup_path(&expanded_package) == normalize_lookup_path(&canonical_package)
}

fn set_skill_enabled_in_document(
    document: &mut DocumentMut,
    requested: &Path,
    canonical: &Path,
    home: &Path,
    codex_root: &Path,
    enabled: bool,
) -> Result<(), String> {
    use toml_edit::{ArrayOfTables, InlineTable, Item, Table, Value as TomlValue};
    let stored_path = display_context_path(&skill_package_path(requested), home);
    if document.get("skills").is_none() {
        document["skills"] = Item::Table(Table::new());
    }
    let skills = document
        .get_mut("skills")
        .and_then(|item| item.as_table_mut())
        .ok_or_else(|| "skills 設定を作れません。".to_string())?;
    if skills.get("config").is_none() {
        skills["config"] = Item::ArrayOfTables(ArrayOfTables::new());
    }
    let config = skills
        .get_mut("config")
        .ok_or_else(|| "skills.config を作れません。".to_string())?;
    if let Some(array) = config.as_array_mut() {
        for item in array.iter_mut() {
            let Some(table) = item.as_inline_table_mut() else {
                continue;
            };
            let Some(path) = table.get("path").and_then(|value| value.as_str()).map(str::to_string) else {
                continue;
            };
            if !skill_config_path_matches(&path, requested, canonical, home, codex_root) {
                continue;
            }
            table.insert("enabled", TomlValue::from(enabled));
            return Ok(());
        }
        let mut table = InlineTable::new();
        table.insert("path", stored_path.into());
        table.insert("enabled", enabled.into());
        array.push(table);
        return Ok(());
    }
    if let Some(tables) = config.as_array_of_tables_mut() {
        for table in tables.iter_mut() {
            let Some(path) = table.get("path").and_then(|value| value.as_str()).map(str::to_string) else {
                continue;
            };
            if !skill_config_path_matches(&path, requested, canonical, home, codex_root) {
                continue;
            }
            table["enabled"] = value(enabled);
            return Ok(());
        }
        let mut table = Table::new();
        table["path"] = value(stored_path);
        table["enabled"] = value(enabled);
        tables.push(table);
        return Ok(());
    }
    Err("skills.config の形式を安全に変更できません。".into())
}

fn is_skill_package_dir(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
        && fs::metadata(&path.join("SKILL.md")).is_ok_and(|metadata| metadata.is_file())
}

fn push_skill_entry(
    path: &Path,
    source: &str,
    home: &Path,
    codex_root: &Path,
    disabled_skill_paths: &BTreeSet<String>,
    output: &mut Vec<MaintenanceSkillEntry>,
    seen: &mut BTreeSet<String>,
) {
    let lookup = normalize_lookup_path(path);
    if !seen.insert(lookup.clone()) {
        return;
    }
    let parent_lookup = path
        .parent()
        .map(normalize_lookup_path)
        .unwrap_or_default();
    let enabled = !disabled_skill_paths.contains(&lookup)
        && !disabled_skill_paths.contains(&parent_lookup);
    let (name, description_preview) = skill_frontmatter(path);
    let displayed = display_context_path(path, home);
    let (content, file_truncated, writable, read_failed) = match read_context_file(path) {
        Ok((content, file_truncated, _)) => (
            content,
            file_truncated,
            !file_truncated
                && !is_symlink(path)
                && allowed_context_path("skill", path, home, codex_root, &[]).is_ok(),
            false,
        ),
        Err(_) => (String::new(), false, false, true),
    };
    output.push(MaintenanceSkillEntry {
        id: format!("skill:{displayed}"),
        name,
        source: source.into(),
        enabled,
        path: displayed,
        description_preview,
        content,
        truncated: file_truncated,
        writable,
        read_failed,
    });
}

fn scan_user_skills(
    directory: &Path,
    source: &str,
    home: &Path,
    codex_root: &Path,
    disabled_skill_paths: &BTreeSet<String>,
    output: &mut Vec<MaintenanceSkillEntry>,
    seen: &mut BTreeSet<String>,
    truncated: &mut bool,
) -> bool {
    let mut visited_dirs = BTreeSet::new();
    scan_user_skills_at(
        directory,
        source,
        home,
        codex_root,
        disabled_skill_paths,
        output,
        seen,
        truncated,
        &mut visited_dirs,
        0,
    )
}

fn scan_user_skills_at(
    directory: &Path,
    source: &str,
    home: &Path,
    codex_root: &Path,
    disabled_skill_paths: &BTreeSet<String>,
    output: &mut Vec<MaintenanceSkillEntry>,
    seen: &mut BTreeSet<String>,
    truncated: &mut bool,
    visited_dirs: &mut BTreeSet<PathBuf>,
    depth: usize,
) -> bool {
    if output.len() >= MAX_CONTEXT_SKILLS {
        *truncated = true;
        return true;
    }
    if depth > MAX_SKILL_SCAN_DEPTH {
        return false;
    }
    let Ok(canonical) = fs::canonicalize(directory) else {
        return false;
    };
    if !fs::metadata(&canonical).is_ok_and(|metadata| metadata.is_dir()) || !visited_dirs.insert(canonical) {
        return false;
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return false;
    };
    let mut children = entries.flatten().collect::<Vec<_>>();
    children.sort_by_key(|entry| entry.file_name());
    for entry in children {
        if output.len() >= MAX_CONTEXT_SKILLS {
            *truncated = true;
            return true;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            if is_skill_package_dir(&path) {
                push_skill_entry(
                    &path.join("SKILL.md"),
                    source,
                    home,
                    codex_root,
                    disabled_skill_paths,
                    output,
                    seen,
                );
            } else if path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md") {
                push_skill_entry(
                    &path,
                    source,
                    home,
                    codex_root,
                    disabled_skill_paths,
                    output,
                    seen,
                );
            }
            continue;
        }
        if file_type.is_dir() {
            if is_skill_package_dir(&path) {
                push_skill_entry(
                    &path.join("SKILL.md"),
                    source,
                    home,
                    codex_root,
                    disabled_skill_paths,
                    output,
                    seen,
                );
                continue;
            }
            if scan_user_skills_at(
                &path,
                source,
                home,
                codex_root,
                disabled_skill_paths,
                output,
                seen,
                truncated,
                visited_dirs,
                depth + 1,
            ) {
                return true;
            }
            continue;
        }
        if !file_type.is_file()
            || path.file_name().and_then(|name| name.to_str()) != Some("SKILL.md")
        {
            continue;
        }
        push_skill_entry(
            &path,
            source,
            home,
            codex_root,
            disabled_skill_paths,
            output,
            seen,
        );
    }
    false
}

fn skill_frontmatter(path: &Path) -> (String, Option<String>) {
    let fallback = path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("skill")
        .to_string();
    let Ok(raw) = read_bounded_text(path, 8 * 1024) else {
        return (fallback, None);
    };
    let Some(header) = frontmatter_header(&raw) else {
        return (fallback, None);
    };
    let name = yaml_scalar(&header, "name").unwrap_or(fallback);
    let description = yaml_scalar(&header, "description").map(truncate_skill_description);
    (name, description)
}

fn frontmatter_header(text: &str) -> Option<String> {
    let trimmed = text.strip_prefix("---")?;
    let closing = trimmed.find("\n---")?;
    Some(trimmed[..closing].to_string())
}

fn yaml_scalar(header: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    header.lines().find_map(|line| {
        let line = line.trim();
        line.strip_prefix(&prefix).map(|value| {
            value
                .trim()
                .trim_matches(|ch| ch == '"' || ch == '\'')
                .to_string()
        })
        .filter(|value| !value.is_empty())
    })
}

fn truncate_skill_description(value: String) -> String {
    let mut output = String::new();
    for ch in value.chars() {
        if output.chars().count() >= MAX_SKILL_DESCRIPTION_CHARS {
            output.push('…');
            break;
        }
        output.push(ch);
    }
    output
}

fn normalize_lookup_path(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn read_bounded_text(path: &Path, max_bytes: usize) -> Result<String, String> {
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let mut buffer = vec![0_u8; max_bytes];
    let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
    buffer.truncate(read);
    Ok(String::from_utf8_lossy(&buffer).into_owned())
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

fn display_context_path(path: &Path, home: &Path) -> String {
    path.strip_prefix(home)
        .map(|relative| format!("~/{}", relative.to_string_lossy()))
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

fn expand_display_path(value: &str, home: &Path) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/") {
        home.join(rest)
    } else if value == "~" {
        home.to_path_buf()
    } else {
        PathBuf::from(value)
    }
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
    fn oversized_session_severity_uses_file_size_not_directory_total() {
        assert_eq!(oversized_session_severity(OVERSIZED_SESSION_BYTES - 1), None);
        assert_eq!(
            oversized_session_severity(OVERSIZED_SESSION_BYTES),
            Some(MaintenanceSeverity::Attention)
        );
        assert_eq!(
            oversized_session_severity(CRITICAL_SESSION_BYTES),
            Some(MaintenanceSeverity::Critical)
        );
    }

    #[test]
    fn collect_oversized_sessions_uses_metadata_only() {
        let root = std::env::temp_dir().join(format!(
            "codetas-oversized-sessions-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let nested = root.join("2026/08/20");
        fs::create_dir_all(&nested).expect("create session dir");
        let huge = nested.join(format!("rollout-2026-08-20T00-00-00-{ACTIVE_ID}.jsonl"));
        let small = nested.join(format!("rollout-2026-08-20T00-00-01-{MISSING_ID}.jsonl"));
        fs::write(&huge, vec![0_u8; (OVERSIZED_SESSION_BYTES as usize) + 8]).expect("write huge");
        fs::write(&small, b"{}\n").expect("write small");
        let mut failures = Vec::new();
        let (found, complete) = collect_oversized_sessions(&[&root], &mut failures);
        let _ = fs::remove_dir_all(&root);
        assert!(failures.is_empty(), "{failures:?}");
        assert!(complete);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, ACTIVE_ID);
        assert!(found[0].bytes >= OVERSIZED_SESSION_BYTES);
    }

    #[test]
    fn sqlite_log_bloat_severity_ignores_small_live_data() {
        assert_eq!(
            sqlite_log_bloat_severity(&SqliteLogContentStats {
                row_count: 10,
                estimated_body_bytes: 1024,
                max_row_bytes: 512,
                prunable_bytes: 0,
                top_targets: Vec::new(),
            }),
            None
        );
        assert_eq!(
            sqlite_log_bloat_severity(&SqliteLogContentStats {
                row_count: 100,
                estimated_body_bytes: SQLITE_LOG_BLOAT_BYTES,
                max_row_bytes: OVERSIZED_SQLITE_LOG_ROW_BYTES,
                prunable_bytes: OVERSIZED_SQLITE_LOG_ROW_BYTES,
                top_targets: vec!["codex_http_client::transport 1 1048576".into()],
            }),
            Some(MaintenanceSeverity::Attention)
        );
        assert_eq!(
            sqlite_log_bloat_severity(&SqliteLogContentStats {
                row_count: 100,
                estimated_body_bytes: SQLITE_LOG_BLOAT_CRITICAL_BYTES,
                max_row_bytes: 8 * 1024 * 1024,
                prunable_bytes: 8 * 1024 * 1024,
                top_targets: Vec::new(),
            }),
            Some(MaintenanceSeverity::Critical)
        );
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
    fn context_paths_keep_the_real_home_relative_path() {
        let home = Path::new("/Users/example");
        assert_eq!(
            display_context_path(Path::new("/Users/example/.codex/skills/inbox/SKILL.md"), home),
            "~/.codex/skills/inbox/SKILL.md"
        );
        assert_eq!(
            expand_display_path("~/.codex/skills/inbox/SKILL.md", home),
            home.join(".codex/skills/inbox/SKILL.md")
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

    #[test]
    fn yaml_scalar_reads_skill_frontmatter_name() {
        let header = "name: inbox-triage\ndescription: Check Gmail\n";
        assert_eq!(yaml_scalar(header, "name").as_deref(), Some("inbox-triage"));
        assert_eq!(
            yaml_scalar(header, "description").as_deref(),
            Some("Check Gmail")
        );
    }

    #[test]
    fn context_load_finds_user_skills_and_home_agents() {
        let root = std::env::temp_dir().join(format!(
            "codetas-context-load-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let skills = root.join(".codex/skills/inbox-triage");
        fs::create_dir_all(&skills).expect("create skill dir");
        fs::write(
            skills.join("SKILL.md"),
            "---\nname: inbox-triage\ndescription: Check Gmail quietly\n---\n",
        )
        .expect("write skill");
        fs::write(root.join("AGENTS.md"), "# home agents\n").expect("write agents");
        fs::write(
            root.join(".codex/config.toml"),
            "developer_instructions = \"Keep answers short.\"\n",
        )
        .expect("write config");
        let mut failures = Vec::new();
        let report = context_load_diagnostics(
            &root.join(".codex"),
            &root.join(".codex/config.toml"),
            &root,
            &mut failures,
        );
        let _ = fs::remove_dir_all(&root);
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(report.skill_count, 1);
        assert_eq!(report.enabled_skill_count, 1);
        assert_eq!(report.skills[0].name, "inbox-triage");
        assert!(report.skills[0].content.contains("Check Gmail quietly"));
        assert!(report.skills[0].writable);
        assert!(report
            .instruction_sources
            .iter()
            .any(|source| source.id == "user-agents" && source.present));
        assert!(report
            .instruction_sources
            .iter()
            .any(|source| source.id == "developer-instructions" && source.present && source.content.contains("Keep answers short.")));
        assert_eq!(report.status, MaintenanceSeverity::Attention);
    }

    #[test]
    fn allowed_context_path_rejects_files_outside_home_and_codex() {
        let root = std::env::temp_dir().join(format!(
            "codetas-context-allow-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(root.join(".codex/skills/inbox")).expect("create dirs");
        let outside_root = std::env::temp_dir().join(format!(
            "codetas-context-outside-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&outside_root).expect("create outside dir");
        let outside = outside_root.join("outside.md");
        fs::write(&outside, "nope\n").expect("write outside");
        let agents = root.join("AGENTS.md");
        fs::write(&agents, "# agents\n").expect("write agents");
        let home = &root;
        let codex = root.join(".codex");
        let err = allowed_context_path("instruction", &outside, home, &codex, &[outside.clone()])
            .expect_err("outside path must be rejected");
        assert!(err.contains("編集できません"), "{err}");
        let allowed = allowed_context_path("instruction", &agents, home, &codex, &[])
            .expect("home AGENTS.md must be allowed");
        assert_eq!(allowed, fs::canonicalize(&agents).expect("canonicalize agents"));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside_root);
    }

    #[cfg(unix)]
    #[test]
    fn context_load_follows_skills_directory_symlink() {
        let root = std::env::temp_dir().join(format!(
            "codetas-context-skill-link-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let real = root.join("real-skills/inbox");
        fs::create_dir_all(&real).expect("create real skill dir");
        fs::write(
            real.join("SKILL.md"),
            "---\nname: linked-inbox\ndescription: Linked skill\n---\n",
        )
        .expect("write skill");
        fs::create_dir_all(root.join(".codex")).expect("create codex");
        std::os::unix::fs::symlink(root.join("real-skills"), root.join(".codex/skills"))
            .expect("link skills");
        let mut failures = Vec::new();
        let report = context_load_diagnostics(
            &root.join(".codex"),
            &root.join(".codex/config.toml"),
            &root,
            &mut failures,
        );
        let skill_path = expand_display_path(&report.skills[0].path, &root);
        let allowed = allowed_context_path(
            "skill",
            &skill_path,
            &root,
            &root.join(".codex"),
            &[],
        );
        let _ = fs::remove_dir_all(&root);
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(report.skill_count, 1);
        assert_eq!(report.skills[0].name, "linked-inbox");
        assert!(report.skills[0].writable);
        assert!(
            report.skills[0].path.ends_with(".codex/skills/inbox/SKILL.md"),
            "{}",
            report.skills[0].path
        );
        assert!(allowed.is_ok(), "{allowed:?}");
    }

    #[cfg(unix)]
    #[test]
    fn context_load_shows_linked_skill_packages_without_saving_outside() {
        let root = std::env::temp_dir().join(format!(
            "codetas-context-skill-pkg-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let outside = std::env::temp_dir().join(format!(
            "codetas-context-skill-pkg-out-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(outside.join("inbox")).expect("create outside skill");
        fs::write(
            outside.join("inbox/SKILL.md"),
            "---\nname: outside-inbox\ndescription: Linked package\n---\n",
        )
        .expect("write skill");
        fs::create_dir_all(root.join(".codex/skills")).expect("create skills");
        std::os::unix::fs::symlink(outside.join("inbox"), root.join(".codex/skills/inbox"))
            .expect("link package");
        let mut failures = Vec::new();
        let report = context_load_diagnostics(
            &root.join(".codex"),
            &root.join(".codex/config.toml"),
            &root,
            &mut failures,
        );
        let skill = &report.skills[0];
        let allowed = allowed_context_path(
            "skill",
            &expand_display_path(&skill.path, &root),
            &root,
            &root.join(".codex"),
            &[],
        );
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(report.skill_count, 1);
        assert_eq!(skill.name, "outside-inbox");
        assert!(skill.content.contains("Linked package"));
        assert!(!skill.writable);
        assert!(!skill.read_failed);
        assert!(allowed.is_err(), "{allowed:?}");
    }

    #[cfg(unix)]
    #[test]
    fn context_load_allows_skill_package_linked_inside_home() {
        let root = std::env::temp_dir().join(format!(
            "codetas-context-skill-home-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(root.join("library/inbox")).expect("create home skill");
        fs::write(
            root.join("library/inbox/SKILL.md"),
            "---\nname: home-inbox\ndescription: Home linked package\n---\n",
        )
        .expect("write skill");
        fs::create_dir_all(root.join(".codex/skills")).expect("create skills");
        std::os::unix::fs::symlink(root.join("library/inbox"), root.join(".codex/skills/inbox"))
            .expect("link package");
        let mut failures = Vec::new();
        let report = context_load_diagnostics(
            &root.join(".codex"),
            &root.join(".codex/config.toml"),
            &root,
            &mut failures,
        );
        let skill = &report.skills[0];
        let allowed = allowed_context_path(
            "skill",
            &expand_display_path(&skill.path, &root),
            &root,
            &root.join(".codex"),
            &[],
        );
        let _ = fs::remove_dir_all(&root);
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(report.skill_count, 1);
        assert_eq!(skill.name, "home-inbox");
        assert!(skill.writable);
        assert!(allowed.is_ok(), "{allowed:?}");
    }

    #[test]
    fn expand_config_path_resolves_relative_to_codex_home() {
        let home = Path::new("/Users/example");
        let codex = home.join(".codex");
        assert_eq!(
            expand_config_path("skills/inbox", home, &codex),
            codex.join("skills/inbox")
        );
        assert_eq!(
            expand_config_path("~/library/inbox", home, &codex),
            home.join("library/inbox")
        );
    }

    #[test]
    fn skill_config_matches_package_dir_and_skill_file() {
        let home = Path::new("/Users/example");
        let codex = home.join(".codex");
        let package = home.join(".codex/skills/inbox");
        let skill = package.join("SKILL.md");
        assert!(skill_config_path_matches(
            "~/.codex/skills/inbox",
            &skill,
            &skill,
            home,
            &codex
        ));
        assert!(skill_config_path_matches(
            "skills/inbox/SKILL.md",
            &package,
            &package,
            home,
            &codex
        ));
        assert!(!skill_config_path_matches(
            "skills/other",
            &skill,
            &skill,
            home,
            &codex
        ));
    }
}
