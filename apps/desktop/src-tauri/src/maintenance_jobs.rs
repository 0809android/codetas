use crate::provider_gateway::{atomic_write, codex_home};
use command_group::CommandGroup;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashSet},
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager};
use toml_edit::{value, DocumentMut, Item, Value as TomlValue};

const PLAN_TTL_MS: u64 = 30 * 60 * 1_000;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const VACUUM_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const MAX_COMMAND_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_LOG_CANDIDATES: usize = 50_000;
const MAX_SESSION_FILES: usize = 250_000;
const MAX_STATE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
const SQLITE_SAFETY_MARGIN_BYTES: u64 = 256 * 1024 * 1024;
const STALE_LOCK_MS: u64 = 15 * 60 * 1_000;
const IDLE_WORKER_INTERVAL: Duration = Duration::from_secs(5);

static JOB_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenancePreviewRequest {
    /// 7, 30, 90 のいずれか。null/省略時はログを削除しない。
    pub log_retention_days: Option<u16>,
    #[serde(default)]
    pub compact_sqlite: bool,
    #[serde(default)]
    pub repair_orphan_pins: bool,
    #[serde(default)]
    pub disable_mcp_servers: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceExecuteRequest {
    pub plan_id: String,
    #[serde(default)]
    pub action_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceRollbackRequest {
    pub job_id: String,
    #[serde(default)]
    pub cancel_only: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenancePlan {
    pub schema_version: u8,
    pub id: String,
    pub generated_at_ms: u64,
    pub expires_at_ms: u64,
    pub codex_running: bool,
    pub disk_free_bytes: Option<u64>,
    pub backup_root: PathBuf,
    pub actions: Vec<MaintenanceActionPreview>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceActionPreview {
    pub id: String,
    pub kind: MaintenanceActionKind,
    pub title: String,
    pub summary: String,
    pub requires_codex_shutdown: bool,
    pub reversible: bool,
    pub estimated_reclaimable_bytes: u64,
    pub affected_item_count: usize,
    pub blocked_reason: Option<String>,
    pub details: MaintenanceActionDetails,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MaintenanceActionKind {
    CleanupTextLogs,
    CompactSqlite,
    RepairOrphanPins,
    DisableMcpServers,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum MaintenanceActionDetails {
    CleanupTextLogs {
        retention_days: u16,
        log_root: PathBuf,
        candidates: Vec<FileCandidate>,
    },
    CompactSqlite {
        database: PathBuf,
        physical_bytes: u64,
        estimated_live_bytes: u64,
        required_free_bytes: u64,
    },
    RepairOrphanPins {
        state_path: PathBuf,
        orphan_ids: Vec<String>,
        session_scan_complete: bool,
    },
    DisableMcpServers {
        config_path: PathBuf,
        server_names: Vec<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileCandidate {
    relative_path: PathBuf,
    bytes: u64,
    modified_ms: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MaintenanceJobStatus {
    Running,
    WaitingForIdle,
    Completed,
    Failed,
    Cancelled,
    RolledBack,
    RollbackFailed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceJobSummary {
    pub id: String,
    pub plan_id: String,
    pub status: MaintenanceJobStatus,
    pub created_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub action_ids: Vec<String>,
    pub reclaimed_bytes: u64,
    pub error: Option<String>,
    pub rollback_available: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MaintenanceJobJournal {
    schema_version: u8,
    id: String,
    plan_id: String,
    status: MaintenanceJobStatus,
    created_at_ms: u64,
    finished_at_ms: Option<u64>,
    action_ids: Vec<String>,
    completed_action_ids: Vec<String>,
    #[serde(default)]
    pending_actions: Vec<MaintenanceActionPreview>,
    #[serde(default)]
    action_errors: Vec<String>,
    reclaimed_bytes: u64,
    error: Option<String>,
    operations: Vec<JournalOperation>,
    #[serde(default)]
    rolled_back_operation_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum JournalOperation {
    MovedToTrash {
        original: PathBuf,
        trash: PathBuf,
    },
    ReplacedFile {
        original: PathBuf,
        backup: PathBuf,
        after_sha256: Option<String>,
    },
    ReplacedSqlite {
        database: PathBuf,
        backup_database: PathBuf,
        backup_sha256: String,
        before_size: u64,
        before_modified_ms: u64,
        #[serde(default)]
        before_sha256: Option<String>,
        after_size: u64,
        after_sha256: String,
        #[serde(default)]
        saved_wal: Option<PathBuf>,
        #[serde(default)]
        saved_shm: Option<PathBuf>,
    },
}

struct MaintenanceLock {
    path: PathBuf,
}

impl Drop for MaintenanceLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Default)]
struct CommandResult {
    stdout: String,
    stderr: String,
    success: bool,
}

#[tauri::command]
pub async fn preview_codex_maintenance(
    app: AppHandle,
    request: MaintenancePreviewRequest,
) -> Result<MaintenancePlan, String> {
    tauri::async_runtime::spawn_blocking(move || preview_blocking(&app, request))
        .await
        .map_err(|error| format!("保守プレビューを完了できませんでした: {error}"))?
}

#[tauri::command]
pub async fn execute_codex_maintenance(
    app: AppHandle,
    request: MaintenanceExecuteRequest,
) -> Result<MaintenanceJobSummary, String> {
    tauri::async_runtime::spawn_blocking(move || execute_blocking(&app, request))
        .await
        .map_err(|error| format!("保守ジョブを完了できませんでした: {error}"))?
}

#[tauri::command]
pub async fn list_codex_maintenance_jobs(
    app: AppHandle,
) -> Result<Vec<MaintenanceJobSummary>, String> {
    tauri::async_runtime::spawn_blocking(move || list_jobs_blocking(&app))
        .await
        .map_err(|error| format!("保守履歴を読み取れませんでした: {error}"))?
}

#[tauri::command]
pub async fn rollback_codex_maintenance_job(
    app: AppHandle,
    request: MaintenanceRollbackRequest,
) -> Result<MaintenanceJobSummary, String> {
    tauri::async_runtime::spawn_blocking(move || rollback_blocking(&app, request))
        .await
        .map_err(|error| format!("ロールバックを完了できませんでした: {error}"))?
}

pub(crate) fn start_idle_maintenance_worker(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            let handle = app.clone();
            let _ = tauri::async_runtime::spawn_blocking(move || {
                if let Err(error) = resume_waiting_jobs_blocking(&handle) {
                    eprintln!("CODETAS: deferred maintenance check failed: {error}");
                }
            })
            .await;
            tokio::time::sleep(IDLE_WORKER_INTERVAL).await;
        }
    });
}

fn preview_blocking(app: &AppHandle, request: MaintenancePreviewRequest) -> Result<MaintenancePlan, String> {
    if !cfg!(target_os = "macos") {
        return Err("Codexメンテナンスの変更操作は現在macOS専用です。".into());
    }
    if request
        .log_retention_days
        .is_some_and(|days| !matches!(days, 7 | 30 | 90))
    {
        return Err("ログ保存期間は7日、30日、90日、または削除しないを選択してください。".into());
    }
    validate_mcp_names(&request.disable_mcp_servers)?;

    let codex_root = canonical_or_original(&codex_home()?);
    let maintenance_root = maintenance_root(app)?;
    fs::create_dir_all(maintenance_root.join("plans"))
        .map_err(|error| format!("保守計画フォルダを作れません: {error}"))?;
    let now = now_ms();
    let plan_id = new_id("plan");
    let codex_running = !codex_process_ids()?.is_empty();
    let disk_free_bytes = available_bytes(&codex_root).ok();
    let mut warnings = Vec::new();
    let mut actions = Vec::new();

    if let Some(retention_days) = request.log_retention_days {
        let log_root = dirs::home_dir()
            .ok_or("ホームフォルダを特定できません")?
            .join("Library/Logs/com.openai.codex");
        let candidates = collect_old_logs(&log_root, retention_days, &mut warnings)?;
        let bytes = candidates.iter().map(|item| item.bytes).sum();
        actions.push(MaintenanceActionPreview {
            id: "cleanup-text-logs".into(),
            kind: MaintenanceActionKind::CleanupTextLogs,
            title: format!("{retention_days}日より古いCodexテキストログを退避"),
            summary: "Codexを終了せず、使用中のファイルだけをスキップして専用ごみ箱へ移動します。".into(),
            requires_codex_shutdown: false,
            reversible: true,
            estimated_reclaimable_bytes: bytes,
            affected_item_count: candidates.len(),
            blocked_reason: None,
            details: MaintenanceActionDetails::CleanupTextLogs {
                retention_days,
                log_root,
                candidates,
            },
        });
    }

    if request.compact_sqlite {
        let database = codex_root.join("logs_2.sqlite");
        let (physical_bytes, live_bytes) = sqlite_size_estimate(&database)?;
        let required_free_bytes = physical_bytes
            .saturating_add(sqlite_sidecar_bytes(&database))
            .saturating_add(live_bytes)
            .saturating_add(live_bytes / 4)
            .saturating_add(SQLITE_SAFETY_MARGIN_BYTES);
        let blocked_reason = sqlite_plan_block_reason(&database, disk_free_bytes, required_free_bytes);
        actions.push(MaintenanceActionPreview {
            id: "compact-sqlite".into(),
            kind: MaintenanceActionKind::CompactSqlite,
            title: "CodexログDBを自動圧縮".into(),
            summary: "Codexが稼働中なら待機し、次に自然終了したタイミングでバックアップ・検証・置換を自動実行します。".into(),
            requires_codex_shutdown: true,
            reversible: true,
            estimated_reclaimable_bytes: physical_bytes.saturating_sub(live_bytes),
            affected_item_count: usize::from(physical_bytes > 0),
            blocked_reason,
            details: MaintenanceActionDetails::CompactSqlite {
                database,
                physical_bytes,
                estimated_live_bytes: live_bytes,
                required_free_bytes,
            },
        });
    }

    if request.repair_orphan_pins {
        let state_path = codex_root.join(".codex-global-state.json");
        let (session_ids, complete) = collect_session_ids(&codex_root, &mut warnings);
        let orphan_ids = if complete {
            read_orphan_pins(&state_path, &session_ids)?
        } else {
            Vec::new()
        };
        actions.push(MaintenanceActionPreview {
            id: "repair-orphan-pins".into(),
            kind: MaintenanceActionKind::RepairOrphanPins,
            title: "孤立したピン留めを修復".into(),
            summary: "全セッション走査が完了した場合だけ待機し、Codexの自然終了後に存在しないUUID形式のピンを除去します。".into(),
            requires_codex_shutdown: true,
            reversible: true,
            estimated_reclaimable_bytes: 0,
            affected_item_count: orphan_ids.len(),
            blocked_reason: if !complete {
                Some("セッション一覧を完全に走査できなかったため修復しません。".into())
            } else {
                None
            },
            details: MaintenanceActionDetails::RepairOrphanPins {
                state_path,
                orphan_ids,
                session_scan_complete: complete,
            },
        });
    }

    if !request.disable_mcp_servers.is_empty() {
        let config_path = codex_root.join("config.toml");
        let names = configured_mcp_names(&config_path, &request.disable_mcp_servers)?;
        actions.push(MaintenanceActionPreview {
            id: "disable-mcp-servers".into(),
            kind: MaintenanceActionKind::DisableMcpServers,
            title: "選択したMCPサーバーを無効化".into(),
            summary: "Codexを終了せず、最新のconfig.tomlを再読込して選択したサーバーだけを無効化します。".into(),
            requires_codex_shutdown: false,
            reversible: true,
            estimated_reclaimable_bytes: 0,
            affected_item_count: names.len(),
            blocked_reason: None,
            details: MaintenanceActionDetails::DisableMcpServers {
                config_path,
                server_names: names,
            },
        });
    }

    let plan = MaintenancePlan {
        schema_version: 1,
        id: plan_id.clone(),
        generated_at_ms: now,
        expires_at_ms: now.saturating_add(PLAN_TTL_MS),
        codex_running,
        disk_free_bytes,
        backup_root: maintenance_root.join("jobs"),
        actions,
        warnings,
    };
    write_json(&maintenance_root.join("plans").join(format!("{plan_id}.json")), &plan)?;
    Ok(plan)
}

fn execute_blocking(
    app: &AppHandle,
    request: MaintenanceExecuteRequest,
) -> Result<MaintenanceJobSummary, String> {
    validate_id(&request.plan_id)?;
    let root = maintenance_root(app)?;
    let _lock = acquire_lock(&root)?;
    let plan: MaintenancePlan = read_json(&root.join("plans").join(format!("{}.json", request.plan_id)))?;
    if plan.schema_version != 1 || plan.id != request.plan_id {
        return Err("保守計画の識別情報が一致しません。".into());
    }
    if plan.expires_at_ms < now_ms() {
        return Err("保守プレビューの有効期限が切れています。もう一度プレビューしてください。".into());
    }
    let selected = select_actions(&plan, &request.action_ids)?;
    if selected.is_empty() {
        return Err("実行する保守操作が選択されていません。".into());
    }
    if let Some(action) = selected.iter().find(|action| action.blocked_reason.is_some()) {
        return Err(format!(
            "{}はプレビュー時点で実行できません: {}",
            action.title,
            action.blocked_reason.as_deref().unwrap_or("安全条件を満たしていません")
        ));
    }
    for action in &selected {
        validate_action_target(action)?;
    }
    let selected = selected.into_iter().cloned().collect::<Vec<_>>();
    let requested_offline_action_ids = selected
        .iter()
        .filter(|action| action.requires_codex_shutdown)
        .map(|action| action.id.clone())
        .collect::<HashSet<_>>();
    let (queued_offline_action_ids, existing_waiting_job) =
        queued_offline_actions(&root, &requested_offline_action_ids)?;
    let selected = selected
        .into_iter()
        .filter(|action| {
            !action.requires_codex_shutdown
                || !queued_offline_action_ids.contains(&action.id)
        })
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return existing_waiting_job
            .as_ref()
            .map(job_summary)
            .ok_or_else(|| "選択したオフライン処理は既に自然終了待ちです。".to_string());
    }
    let job_id = new_id("job");
    let job_dir = root.join("jobs").join(&job_id);
    fs::create_dir_all(job_dir.join("backup"))
        .and_then(|_| fs::create_dir_all(job_dir.join("trash")))
        .map_err(|error| format!("保守ジョブフォルダを作れません: {error}"))?;
    let journal_path = job_dir.join("journal.json");
    let mut journal = MaintenanceJobJournal {
        schema_version: 1,
        id: job_id,
        plan_id: plan.id.clone(),
        status: MaintenanceJobStatus::Running,
        created_at_ms: now_ms(),
        finished_at_ms: None,
        action_ids: selected.iter().map(|action| action.id.clone()).collect(),
        completed_action_ids: Vec::new(),
        pending_actions: selected
            .iter()
            .filter(|action| action.requires_codex_shutdown)
            .cloned()
            .collect(),
        action_errors: Vec::new(),
        reclaimed_bytes: 0,
        error: None,
        operations: Vec::new(),
        rolled_back_operation_count: 0,
    };
    write_json(&journal_path, &journal)?;

    for action in selected.iter().filter(|action| !action.requires_codex_shutdown) {
        let result = execute_action(action, &job_dir, &journal_path, &mut journal);
        match result {
            Ok(reclaimed) => {
                journal.reclaimed_bytes = journal.reclaimed_bytes.saturating_add(reclaimed);
                journal.completed_action_ids.push(action.id.clone());
                write_json(&journal_path, &journal)?;
            }
            Err(error) => {
                record_action_error(&mut journal, action, &error);
                write_json(&journal_path, &journal)?;
            }
        }
    }
    run_pending_actions(&job_dir, &journal_path, &mut journal)?;
    if journal.status == MaintenanceJobStatus::Completed
        && !queued_offline_action_ids.is_empty()
        && let Some(existing) = existing_waiting_job.as_ref()
    {
        return Ok(job_summary(existing));
    }
    Ok(job_summary(&journal))
}

fn queued_offline_actions(
    root: &Path,
    requested_action_ids: &HashSet<String>,
) -> Result<(HashSet<String>, Option<MaintenanceJobJournal>), String> {
    if requested_action_ids.is_empty() {
        return Ok((HashSet::new(), None));
    }
    let jobs_root = root.join("jobs");
    let entries = match fs::read_dir(&jobs_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((HashSet::new(), None));
        }
        Err(error) => return Err(format!("保守履歴フォルダを読めません: {error}")),
    };
    let mut action_ids = HashSet::new();
    let mut representative: Option<MaintenanceJobJournal> = None;
    for entry in entries.flatten() {
        let journal_path = entry.path().join("journal.json");
        let metadata = match fs::symlink_metadata(&journal_path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => metadata,
            _ => continue,
        };
        if metadata.len() == 0 {
            continue;
        }
        let journal: MaintenanceJobJournal = match read_json(&journal_path) {
            Ok(journal) => journal,
            Err(_) => continue,
        };
        if journal.schema_version != 1 || journal.status != MaintenanceJobStatus::WaitingForIdle {
            continue;
        }
        let directory_id = journal_path
            .parent()
            .and_then(Path::file_name)
            .and_then(OsStr::to_str);
        if directory_id != Some(journal.id.as_str()) || validate_id(&journal.id).is_err() {
            continue;
        }
        let mut has_valid_pending_action = false;
        for action in &journal.pending_actions {
            if action.requires_codex_shutdown
                && requested_action_ids.contains(&action.id)
                && pending_offline_action_identity_is_valid(action)
                && validate_action_target(action).is_ok()
            {
                action_ids.insert(action.id.clone());
                has_valid_pending_action = true;
            }
        }
        if has_valid_pending_action
            && representative
                .as_ref()
                .is_none_or(|current| journal.created_at_ms > current.created_at_ms)
        {
            representative = Some(journal);
        }
    }
    Ok((action_ids, representative))
}

fn pending_offline_action_identity_is_valid(action: &MaintenanceActionPreview) -> bool {
    matches!(
        (action.id.as_str(), action.kind, &action.details),
        (
            "compact-sqlite",
            MaintenanceActionKind::CompactSqlite,
            MaintenanceActionDetails::CompactSqlite { .. }
        ) | (
            "repair-orphan-pins",
            MaintenanceActionKind::RepairOrphanPins,
            MaintenanceActionDetails::RepairOrphanPins { .. }
        )
    )
}

fn record_action_error(
    journal: &mut MaintenanceJobJournal,
    action: &MaintenanceActionPreview,
    error: &str,
) {
    journal.action_errors.push(format!("{}: {error}", action.title));
    journal.error = Some(format!(
        "{}。完了済みの変更は保持されています。必要なら履歴からロールバックできます。",
        journal.action_errors.join(" / ")
    ));
}

fn finish_journal(journal: &mut MaintenanceJobJournal) {
    journal.finished_at_ms = Some(now_ms());
    if journal.action_errors.is_empty() {
        journal.status = MaintenanceJobStatus::Completed;
        journal.error = None;
    } else {
        journal.status = MaintenanceJobStatus::Failed;
    }
}

fn run_pending_actions(
    job_dir: &Path,
    journal_path: &Path,
    journal: &mut MaintenanceJobJournal,
) -> Result<(), String> {
    while let Some(action) = journal.pending_actions.first().cloned() {
        if !codex_process_ids()?.is_empty() {
            journal.status = MaintenanceJobStatus::WaitingForIdle;
            journal.finished_at_ms = None;
            write_json(journal_path, journal)?;
            return Ok(());
        }
        journal.status = MaintenanceJobStatus::Running;
        write_json(journal_path, journal)?;
        let operation_count_before = journal.operations.len();
        let preexisting_attempt_artifacts = uncommitted_attempt_artifacts(&action, job_dir)
            .into_iter()
            .filter(|path| path.exists())
            .collect::<HashSet<_>>();
        match execute_action(&action, job_dir, journal_path, journal) {
            Ok(reclaimed) => {
                journal.reclaimed_bytes = journal.reclaimed_bytes.saturating_add(reclaimed);
                journal.completed_action_ids.push(action.id.clone());
                journal.pending_actions.remove(0);
                write_json(journal_path, journal)?;
            }
            Err(error) => {
                if journal.operations.len() == operation_count_before {
                    if let Err(cleanup_error) = cleanup_uncommitted_attempt_artifacts(
                        &action,
                        job_dir,
                        &preexisting_attempt_artifacts,
                    ) {
                        record_action_error(
                            journal,
                            &action,
                            &format!(
                                "{error}; 再試行用ファイルを安全に片付けられませんでした: {cleanup_error}"
                            ),
                        );
                        journal.pending_actions.remove(0);
                        write_json(journal_path, journal)?;
                        continue;
                    }
                    match codex_process_ids() {
                        Ok(pids) if !pids.is_empty() => {
                            journal.status = MaintenanceJobStatus::WaitingForIdle;
                            journal.finished_at_ms = None;
                            write_json(journal_path, journal)?;
                            return Ok(());
                        }
                        Ok(_) => {}
                        Err(check_error) => {
                            record_action_error(
                                journal,
                                &action,
                                &format!(
                                    "{error}; Codexの稼働状態を再確認できませんでした: {check_error}"
                                ),
                            );
                            journal.pending_actions.remove(0);
                            write_json(journal_path, journal)?;
                            continue;
                        }
                    }
                }
                record_action_error(journal, &action, &error);
                journal.pending_actions.remove(0);
                write_json(journal_path, journal)?;
            }
        }
    }
    finish_journal(journal);
    write_json(journal_path, journal)
}

fn cleanup_uncommitted_attempt_artifacts(
    action: &MaintenanceActionPreview,
    job_dir: &Path,
    preexisting: &HashSet<PathBuf>,
) -> Result<(), String> {
    for path in uncommitted_attempt_artifacts(action, job_dir) {
        if !preexisting.contains(&path) {
            remove_regular_if_exists(&path)?;
        }
    }
    Ok(())
}

fn uncommitted_attempt_artifacts(
    action: &MaintenanceActionPreview,
    job_dir: &Path,
) -> Vec<PathBuf> {
    let MaintenanceActionDetails::CompactSqlite { database, .. } = &action.details else {
        return match &action.details {
            MaintenanceActionDetails::RepairOrphanPins { .. } => {
                vec![job_dir.join("backup/codex-global-state.json")]
            }
            _ => Vec::new(),
        };
    };
    let bases = [
        job_dir.join("backup/sqlite/logs_2.sqlite.original"),
        job_dir.join("backup/logs_2.sqlite.compacted"),
        database.with_extension(format!("codetas-{}.tmp", std::process::id())),
    ];
    let mut paths = Vec::with_capacity(bases.len() * 4);
    for base in bases {
        paths.push(base.clone());
        for suffix in ["-journal", "-wal", "-shm"] {
            paths.push(PathBuf::from(format!(
                "{}{suffix}",
                base.to_string_lossy()
            )));
        }
    }
    paths
}

fn resume_waiting_jobs_blocking(app: &AppHandle) -> Result<(), String> {
    if !codex_process_ids()?.is_empty() {
        return Ok(());
    }
    let root = maintenance_root(app)?;
    let jobs_root = root.join("jobs");
    if !jobs_root.exists() {
        return Ok(());
    }
    let lock_path = root.join("maintenance.lock");
    if lock_path.exists() {
        if stale_lock(&lock_path)? {
            remove_regular_if_exists(&lock_path)?;
        } else {
            return Ok(());
        }
    }
    let _lock = acquire_lock(&root)?;
    let mut journal_paths = fs::read_dir(&jobs_root)
        .map_err(|error| format!("保守履歴フォルダを読めません: {error}"))?
        .flatten()
        .map(|entry| entry.path().join("journal.json"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    journal_paths.sort();

    for journal_path in journal_paths {
        if !codex_process_ids()?.is_empty() {
            break;
        }
        let mut journal: MaintenanceJobJournal = match read_json(&journal_path) {
            Ok(journal) => journal,
            Err(_) => continue,
        };
        if journal.status != MaintenanceJobStatus::WaitingForIdle {
            continue;
        }
        let job_dir = journal_path
            .parent()
            .ok_or("ジョブフォルダを特定できません")?;
        let target_validation = validate_journal_targets(&journal, job_dir).and_then(|_| {
            for action in &journal.pending_actions {
                validate_action_target(action)?;
            }
            Ok(())
        });
        if let Err(error) = target_validation {
            journal.status = MaintenanceJobStatus::Failed;
            journal.finished_at_ms = Some(now_ms());
            journal.error = Some(format!("待機中ジョブの対象を再検証できませんでした: {error}"));
            journal.pending_actions.clear();
            write_json(&journal_path, &journal)?;
            continue;
        }
        run_pending_actions(job_dir, &journal_path, &mut journal)?;
    }
    Ok(())
}

fn execute_action(
    action: &MaintenanceActionPreview,
    job_dir: &Path,
    journal_path: &Path,
    journal: &mut MaintenanceJobJournal,
) -> Result<u64, String> {
    match &action.details {
        MaintenanceActionDetails::CleanupTextLogs {
            retention_days,
            log_root,
            candidates,
        } => cleanup_logs(*retention_days, log_root, candidates, job_dir, journal_path, journal),
        MaintenanceActionDetails::CompactSqlite {
            database,
            physical_bytes,
            estimated_live_bytes,
            required_free_bytes,
        } => compact_sqlite(
            database,
            *physical_bytes,
            *estimated_live_bytes,
            *required_free_bytes,
            job_dir,
            journal_path,
            journal,
        ),
        MaintenanceActionDetails::RepairOrphanPins {
            state_path,
            orphan_ids,
            session_scan_complete,
        } => repair_orphan_pins(
            state_path,
            orphan_ids,
            *session_scan_complete,
            job_dir,
            journal_path,
            journal,
        ),
        MaintenanceActionDetails::DisableMcpServers {
            config_path,
            server_names,
        } => disable_mcp_servers(config_path, server_names, job_dir, journal_path, journal),
    }
}

fn cleanup_logs(
    retention_days: u16,
    log_root: &Path,
    planned: &[FileCandidate],
    job_dir: &Path,
    journal_path: &Path,
    journal: &mut MaintenanceJobJournal,
) -> Result<u64, String> {
    if !matches!(retention_days, 7 | 30 | 90) {
        return Err("保存期間が許可リスト外です。".into());
    }
    let root = fs::canonicalize(log_root)
        .map_err(|error| format!("ログフォルダを確認できません: {error}"))?;
    let open_files = open_files_in_directory(&root);
    let cutoff = now_ms().saturating_sub(u64::from(retention_days) * 24 * 60 * 60 * 1_000);
    let mut reclaimed = 0_u64;
    for candidate in planned {
        validate_relative(&candidate.relative_path)?;
        let source = root.join(&candidate.relative_path);
        let source_metadata = match fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("ログ対象を再確認できません: {error}")),
        };
        if source_metadata.file_type().is_symlink() || !source_metadata.is_file() {
            continue;
        }
        let canonical = fs::canonicalize(&source)
            .map_err(|error| format!("ログ対象を再確認できません: {error}"))?;
        if !canonical.starts_with(&root) {
            return Err("ログ対象が許可されたフォルダ外を指しています。".into());
        }
        if open_files.contains(&canonical) {
            continue;
        }
        let metadata = fs::symlink_metadata(&canonical)
            .map_err(|error| format!("ログ対象の状態を確認できません: {error}"))?;
        let modified_ms = system_time_ms(metadata.modified().unwrap_or(UNIX_EPOCH));
        if metadata.len() != candidate.bytes || modified_ms != candidate.modified_ms || modified_ms >= cutoff {
            continue;
        }
        let trash = job_dir.join("trash/logs").join(&candidate.relative_path);
        if let Some(parent) = trash.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("専用ごみ箱を作れません: {error}"))?;
        }
        journal.operations.push(JournalOperation::MovedToTrash {
            original: canonical.clone(),
            trash: trash.clone(),
        });
        write_json(journal_path, journal)?;
        fs::rename(&canonical, &trash)
            .map_err(|error| format!("ログを専用ごみ箱へ移動できません: {error}"))?;
        reclaimed = reclaimed.saturating_add(metadata.len());
    }
    Ok(reclaimed)
}

fn compact_sqlite(
    database: &Path,
    _planned_physical_bytes: u64,
    _planned_live_bytes: u64,
    _planned_required_free_bytes: u64,
    job_dir: &Path,
    journal_path: &Path,
    journal: &mut MaintenanceJobJournal,
) -> Result<u64, String> {
    require_codex_offline(database)?;
    let database = fs::canonicalize(database)
        .map_err(|error| format!("SQLite DBを確認できません: {error}"))?;
    let original_size = regular_file_size(&database)?;
    let (_, current_live_bytes) = sqlite_size_estimate(&database)?;
    let required_free_bytes = original_size
        .saturating_add(sqlite_sidecar_bytes(&database))
        .saturating_add(current_live_bytes)
        .saturating_add(current_live_bytes / 4)
        .saturating_add(SQLITE_SAFETY_MARGIN_BYTES);
    let free = available_bytes(&database)?;
    if free < required_free_bytes {
        return Err(format!(
            "SQLite圧縮の空き容量が不足しています（必要 {required_free_bytes} bytes / 空き {free} bytes）。"
        ));
    }
    let sqlite3 = system_sqlite3()?;
    let backup_dir = job_dir.join("backup/sqlite");
    fs::create_dir_all(&backup_dir)
        .map_err(|error| format!("SQLiteバックアップフォルダを作れません: {error}"))?;
    let database_os = database.as_os_str().to_owned();
    let wal = PathBuf::from(format!("{}-wal", database_os.to_string_lossy()));
    let shm = PathBuf::from(format!("{}-shm", database_os.to_string_lossy()));
    let before_database = file_fingerprint(&database)?;
    let before_sha256 = sha256_file(&database)?;
    let before_wal = optional_file_fingerprint(&wal)?;
    let before_shm = optional_file_fingerprint(&shm)?;
    let backup_database = backup_dir.join("logs_2.sqlite.original");
    sqlite_backup(&sqlite3, &database, &backup_database)?;
    let backup_integrity = run_sqlite_with_timeout(
        &sqlite3,
        &backup_database,
        "PRAGMA integrity_check;",
        VACUUM_TIMEOUT,
    )?;
    if backup_integrity.trim() != "ok" {
        return Err(format!(
            "SQLiteバックアップのintegrity_checkに失敗しました: {}",
            backup_integrity.trim()
        ));
    }
    let backup_sha256 = sha256_file(&backup_database)?;
    require_codex_offline(&database)?;
    if file_fingerprint(&database)? != before_database
        || sha256_file(&database)? != before_sha256
        || optional_file_fingerprint(&wal)? != before_wal
        || optional_file_fingerprint(&shm)? != before_shm
    {
        return Err("SQLite DBまたはWALがバックアップ中に変化したため処理を停止しました。".into());
    }

    let source_tables = sqlite_schema(&sqlite3, &database)?;
    let compacted = job_dir.join("backup/logs_2.sqlite.compacted");
    let sql = format!(
        "PRAGMA busy_timeout=5000; VACUUM INTO '{}';",
        sqlite_quote(&compacted)
    );
    let vacuum = run_command(
        &sqlite3,
        &[OsStr::new("-readonly"), database.as_os_str(), OsStr::new(&sql)],
        VACUUM_TIMEOUT,
    )?;
    if !vacuum.success {
        return Err(command_error("VACUUM INTO", &vacuum));
    }
    let integrity = run_sqlite_with_timeout(
        &sqlite3,
        &compacted,
        "PRAGMA integrity_check;",
        VACUUM_TIMEOUT,
    )?;
    if integrity.trim() != "ok" {
        return Err(format!("圧縮後DBのintegrity_checkに失敗しました: {}", integrity.trim()));
    }
    let compacted_tables = sqlite_schema(&sqlite3, &compacted)?;
    if source_tables != compacted_tables {
        return Err("圧縮前後でSQLiteのテーブル/インデックス一覧が一致しません。".into());
    }
    let new_size = regular_file_size(&compacted)?;
    if new_size == 0 {
        return Err("圧縮後DBが空です。".into());
    }
    let original_permissions = fs::metadata(&database)
        .map_err(|error| format!("SQLite DBの権限を確認できません: {error}"))?
        .permissions();
    fs::set_permissions(&compacted, original_permissions)
        .map_err(|error| format!("圧縮後DBへ原本の権限を適用できません: {error}"))?;
    sync_regular(&compacted, "圧縮後DB")?;
    let after_sha256 = sha256_file(&compacted)?;

    let replacement = database.with_extension(format!("codetas-{}.tmp", std::process::id()));
    if replacement.exists() {
        return Err("SQLite置換用一時ファイルが既に存在します。".into());
    }
    fs::rename(&compacted, &replacement)
        .map_err(|error| format!("圧縮後DBを置換位置へ移動できません: {error}"))?;
    require_codex_offline(&database)?;
    if file_fingerprint(&database)? != before_database
        || sha256_file(&database)? != before_sha256
        || optional_file_fingerprint(&wal)? != before_wal
        || optional_file_fingerprint(&shm)? != before_shm
    {
        let _ = fs::rename(&replacement, &compacted);
        return Err("SQLite DBまたはWALが圧縮中に変化したため置換を中止しました。".into());
    }
    let saved_wal_path = before_wal
        .is_some()
        .then(|| backup_dir.join("logs_2.sqlite.live-wal"));
    let saved_shm_path = before_shm
        .is_some()
        .then(|| backup_dir.join("logs_2.sqlite.live-shm"));
    journal.operations.push(JournalOperation::ReplacedSqlite {
        database: database.clone(),
        backup_database: backup_database.clone(),
        backup_sha256,
        before_size: before_database.0,
        before_modified_ms: before_database.1,
        before_sha256: Some(before_sha256),
        after_size: new_size,
        after_sha256,
        saved_wal: saved_wal_path.clone(),
        saved_shm: saved_shm_path.clone(),
    });
    write_json(journal_path, journal)?;
    let saved_wal = match move_live_sidecar(&wal, &backup_dir.join("logs_2.sqlite.live-wal")) {
        Ok(saved) => saved,
        Err(error) => {
            let _ = fs::rename(&replacement, &compacted);
            return Err(error);
        }
    };
    let saved_shm = match move_live_sidecar(&shm, &backup_dir.join("logs_2.sqlite.live-shm")) {
        Ok(saved) => saved,
        Err(error) => {
            restore_sidecar(&saved_wal, &wal);
            let _ = fs::rename(&replacement, &compacted);
            return Err(error);
        }
    };
    if let Err(error) = fs::rename(&replacement, &database) {
        let _ = fs::rename(&replacement, &compacted);
        restore_sidecar(&saved_wal, &wal);
        restore_sidecar(&saved_shm, &shm);
        return Err(format!("SQLite DBを原子的に置き換えられません: {error}"));
    }
    Ok(original_size.saturating_sub(new_size))
}

fn repair_orphan_pins(
    state_path: &Path,
    _planned_orphans: &[String],
    session_scan_complete: bool,
    job_dir: &Path,
    journal_path: &Path,
    journal: &mut MaintenanceJobJournal,
) -> Result<u64, String> {
    if !session_scan_complete {
        return Err("不完全なセッション走査結果ではピン留めを修復しません。".into());
    }
    require_codex_offline(state_path)?;
    let codex_root = codex_home()?;
    let mut warnings = Vec::new();
    let (session_ids, complete) = collect_session_ids(&codex_root, &mut warnings);
    if !complete {
        return Err("実行時のセッション走査が不完全なためピン留めを修復しません。".into());
    }
    let current_orphans = read_orphan_pins(state_path, &session_ids)?;
    if current_orphans.is_empty() {
        return Ok(0);
    }
    let bytes = read_small_file(state_path, MAX_STATE_BYTES, "Codexグローバル状態")?;
    let before_sha256 = sha256_bytes(&bytes);
    let mut state: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Codexグローバル状態のJSONを解析できません: {error}"))?;
    let orphan_set = current_orphans.iter().map(String::as_str).collect::<HashSet<_>>();
    let pins = state
        .get_mut("pinned-thread-ids")
        .and_then(Value::as_array_mut)
        .ok_or("pinned-thread-idsが配列ではありません")?;
    pins.retain(|item| item.as_str().map_or(true, |id| !orphan_set.contains(id)));
    let output = serde_json::to_vec_pretty(&state)
        .map_err(|error| format!("Codexグローバル状態をシリアライズできません: {error}"))?;
    let backup = job_dir.join("backup/codex-global-state.json");
    atomic_write(&backup, &bytes)?;
    require_codex_offline(state_path)?;
    if sha256_file(state_path)? != before_sha256 {
        remove_regular_if_exists(&backup)?;
        return Err("Codexグローバル状態が修復準備中に変化したため処理を見送りました。".into());
    }
    journal.operations.push(JournalOperation::ReplacedFile {
        original: state_path.to_path_buf(),
        backup,
        after_sha256: Some(sha256_bytes(&output)),
    });
    write_json(journal_path, journal)?;
    atomic_write(state_path, &output)?;
    Ok(0)
}

fn disable_mcp_servers(
    config_path: &Path,
    server_names: &[String],
    job_dir: &Path,
    journal_path: &Path,
    journal: &mut MaintenanceJobJournal,
) -> Result<u64, String> {
    validate_mcp_names(server_names)?;
    let backup = job_dir.join("backup/config.toml");
    for _ in 0..3 {
        let bytes = read_small_file(config_path, MAX_CONFIG_BYTES, "Codex設定")?;
        let before_sha256 = sha256_bytes(&bytes);
        let raw = String::from_utf8(bytes.clone())
            .map_err(|_| "Codex設定がUTF-8ではありません".to_string())?;
        let mut document = raw
            .parse::<DocumentMut>()
            .map_err(|_| "Codex設定TOMLを解析できません。設定形式を確認してください。".to_string())?;
        let servers = document
            .get_mut("mcp_servers")
            .and_then(|item| item.as_table_mut())
            .ok_or("mcp_servers設定が見つかりません")?;
        for name in server_names {
            let item = servers
                .get_mut(name)
                .ok_or_else(|| format!("MCPサーバー {name} は現在の設定にありません"))?;
            match item {
                Item::Table(table) => table["enabled"] = value(false),
                Item::Value(TomlValue::InlineTable(table)) => {
                    table.insert("enabled", TomlValue::from(false));
                }
                _ => return Err(format!("MCPサーバー {name} の設定形式を安全に変更できません")),
            }
        }
        if sha256_file(config_path)? != before_sha256 {
            continue;
        }
        remove_regular_if_exists(&backup)?;
        atomic_write(&backup, &bytes)?;
        if sha256_file(config_path)? != before_sha256 {
            remove_regular_if_exists(&backup)?;
            continue;
        }
        let output = document.to_string().into_bytes();
        journal.operations.push(JournalOperation::ReplacedFile {
            original: config_path.to_path_buf(),
            backup,
            after_sha256: Some(sha256_bytes(&output)),
        });
        write_json(journal_path, journal)?;
        atomic_write(config_path, &output)?;
        return Ok(0);
    }
    Err("Codex設定が同時に更新され続けたため、MCP設定の変更を見送りました。".into())
}

fn list_jobs_blocking(app: &AppHandle) -> Result<Vec<MaintenanceJobSummary>, String> {
    let root = maintenance_root(app)?;
    let lock_exists = root.join("maintenance.lock").exists();
    let jobs = root.join("jobs");
    let entries = match fs::read_dir(&jobs) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("保守履歴フォルダを読めません: {error}")),
    };
    let mut summaries = entries
        .flatten()
        .filter_map(|entry| read_json::<MaintenanceJobJournal>(&entry.path().join("journal.json")).ok())
        .map(|mut journal| {
            if journal.status == MaintenanceJobStatus::Running && !lock_exists {
                journal.status = MaintenanceJobStatus::Failed;
                journal.error = Some(
                    "前回のCODETASプロセスが中断した可能性があります。ロールバックできます。"
                        .into(),
                );
            }
            job_summary(&journal)
        })
        .collect::<Vec<_>>();
    summaries.sort_by_key(|item| std::cmp::Reverse(item.created_at_ms));
    summaries.truncate(100);
    Ok(summaries)
}

fn rollback_blocking(
    app: &AppHandle,
    request: MaintenanceRollbackRequest,
) -> Result<MaintenanceJobSummary, String> {
    validate_id(&request.job_id)?;
    let root = maintenance_root(app)?;
    let _lock = acquire_lock(&root)?;
    let journal_path = root.join("jobs").join(&request.job_id).join("journal.json");
    let mut journal: MaintenanceJobJournal = read_json(&journal_path)?;
    if request.cancel_only && journal.status != MaintenanceJobStatus::WaitingForIdle {
        return Err(
            "待機ジョブは既に開始または完了しています。履歴を更新して状態を確認してください。"
                .into(),
        );
    }
    if journal.status == MaintenanceJobStatus::RolledBack {
        return Ok(job_summary(&journal));
    }
    if journal.status == MaintenanceJobStatus::WaitingForIdle {
        journal.pending_actions.clear();
        journal.status = MaintenanceJobStatus::Cancelled;
        journal.finished_at_ms = Some(now_ms());
        write_json(&journal_path, &journal)?;
        return Ok(job_summary(&journal));
    }
    validate_journal_targets(
        &journal,
        journal_path.parent().ok_or("ジョブフォルダを特定できません")?,
    )?;
    if journal.status == MaintenanceJobStatus::Running {
        journal.status = MaintenanceJobStatus::Failed;
        journal.finished_at_ms = Some(now_ms());
        journal.error = Some(
            "前回のCODETASプロセス中断を検出したため、保存済みジャーナルからロールバックします。"
                .into(),
        );
        write_json(&journal_path, &journal)?;
    }
    if journal_requires_codex_offline(&journal) {
        let codex_root = codex_home()?;
        require_codex_offline(&codex_root.join("logs_2.sqlite"))?;
    }
    journal.pending_actions.clear();
    let result = rollback_journal(&mut journal, &journal_path);
    journal.finished_at_ms = Some(now_ms());
    match result {
        Ok(()) => {
            journal.status = MaintenanceJobStatus::RolledBack;
            journal.reclaimed_bytes = 0;
            journal.completed_action_ids.clear();
            journal.error = None;
            write_json(&journal_path, &journal)?;
            Ok(job_summary(&journal))
        }
        Err(error) => {
            journal.status = MaintenanceJobStatus::RollbackFailed;
            journal.error = Some(error.clone());
            let _ = write_json(&journal_path, &journal);
            Err(format!("ロールバックを安全に停止しました: {error}"))
        }
    }
}

fn rollback_journal(
    journal: &mut MaintenanceJobJournal,
    journal_path: &Path,
) -> Result<(), String> {
    while journal.rolled_back_operation_count < journal.operations.len() {
        let index = journal
            .operations
            .len()
            .saturating_sub(1 + journal.rolled_back_operation_count);
        rollback_operation(&journal.operations[index])?;
        journal.rolled_back_operation_count += 1;
        write_json(journal_path, journal)?;
    }
    Ok(())
}

fn rollback_operation(operation: &JournalOperation) -> Result<(), String> {
    match operation {
            JournalOperation::MovedToTrash { original, trash } => {
                match (original.exists(), trash.exists()) {
                    (true, false) => return Ok(()),
                    (false, true) => {
                        if let Some(parent) = original.parent() {
                            fs::create_dir_all(parent)
                                .map_err(|error| format!("復元先フォルダを作れません: {error}"))?;
                        }
                        validate_log_restore_destination(original)?;
                        regular_file_size(trash)?;
                        fs::rename(trash, original)
                            .map_err(|error| format!("専用ごみ箱から復元できません: {error}"))?;
                    }
                    (true, true) => {
                        return Err(format!(
                            "{} と専用ごみ箱の両方にファイルがあるため上書きしません。",
                            original.display()
                        ));
                    }
                    (false, false) => {
                        return Err(format!(
                            "{} の原本と専用ごみ箱がどちらも見つかりません。",
                            original.display()
                        ));
                    }
                }
            }
            JournalOperation::ReplacedFile {
                original,
                backup,
                after_sha256,
            } => {
                if let Some(expected) = after_sha256 {
                    let current = sha256_file(original)?;
                    if &current != expected {
                        if current == sha256_file(backup)? {
                            return Ok(());
                        }
                        return Err(format!("{} はジョブ後に変更されたため上書きしません。", original.display()));
                    }
                }
                restore_file_atomically(backup, original)?;
            }
            JournalOperation::ReplacedSqlite {
                database,
                backup_database,
                backup_sha256,
                before_size,
                before_sha256,
                after_size,
                after_sha256,
                saved_wal,
                saved_shm,
                ..
            } => {
                require_codex_offline(database)?;
                let wal = PathBuf::from(format!("{}-wal", database.to_string_lossy()));
                let shm = PathBuf::from(format!("{}-shm", database.to_string_lossy()));
                let recovery_temporary = sqlite_restore_temporary(database);
                if recovery_temporary.exists() {
                    if sha256_file(&recovery_temporary)? != *backup_sha256 {
                        return Err("SQLite復元用一時ファイルの検証に失敗しました。".into());
                    }
                    if wal.exists() || shm.exists() {
                        return Err("ジョブ後のSQLite WAL/SHMが存在するため、データを失わないようロールバックを拒否しました。".into());
                    }
                    fs::rename(&recovery_temporary, database).map_err(|error| {
                        format!("中断されたSQLiteロールバックを再開できません: {error}")
                    })?;
                    return Ok(());
                }
                let current_sha256 = sha256_file(database)?;
                if before_sha256.as_deref() == Some(current_sha256.as_str())
                    && regular_file_size(database)? == *before_size
                {
                    restore_unapplied_sidecar(saved_wal, &wal)?;
                    restore_unapplied_sidecar(saved_shm, &shm)?;
                    return Ok(());
                }
                if current_sha256 == *backup_sha256 {
                    return Ok(());
                }
                if regular_file_size(database)? != *after_size || current_sha256 != *after_sha256 {
                    return Err("SQLite DBはジョブ後に変更されたため上書きしません。".into());
                }
                if wal.exists() || shm.exists() {
                    return Err("ジョブ後のSQLite WAL/SHMが存在するため、データを失わないようロールバックを拒否しました。".into());
                }
                if sha256_file(backup_database)? != *backup_sha256 {
                    return Err("SQLiteバックアップの検証に失敗しました。".into());
                }
                restore_sqlite_atomically(backup_database, database)?;
            }
    }
    Ok(())
}

fn collect_old_logs(
    log_root: &Path,
    retention_days: u16,
    warnings: &mut Vec<String>,
) -> Result<Vec<FileCandidate>, String> {
    if !log_root.exists() {
        return Ok(Vec::new());
    }
    let root = fs::canonicalize(log_root)
        .map_err(|error| format!("ログフォルダを開けません: {error}"))?;
    let cutoff = now_ms().saturating_sub(u64::from(retention_days) * 24 * 60 * 60 * 1_000);
    let mut pending = vec![root.clone()];
    let mut candidates = Vec::new();
    while let Some(directory) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                warnings.push(format!("ログフォルダの一部を走査できません: {error}"));
                continue;
            }
        };
        for entry in entries.flatten() {
            if candidates.len() >= MAX_LOG_CANDIDATES {
                return Err(
                    "ログ候補が安全上限を超えました。保存期間を短くするか、範囲を分けてください。"
                        .into(),
                );
            }
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                let modified_ms = system_time_ms(metadata.modified().unwrap_or(UNIX_EPOCH));
                if modified_ms < cutoff {
                    if let Ok(relative_path) = path.strip_prefix(&root) {
                        candidates.push(FileCandidate {
                            relative_path: relative_path.to_path_buf(),
                            bytes: metadata.len(),
                            modified_ms,
                        });
                    }
                }
            }
        }
    }
    candidates.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(candidates)
}

fn collect_session_ids(codex_root: &Path, warnings: &mut Vec<String>) -> (BTreeSet<String>, bool) {
    let mut ids = BTreeSet::new();
    let mut complete = true;
    let mut seen = 0_usize;
    let sessions_root = codex_root.join("sessions");
    if !sessions_root.is_dir() {
        warnings.push("使用中セッションの保存先が見つからないため孤立ピンを判定しません。".into());
        complete = false;
    }
    for root in [sessions_root, codex_root.join("archived_sessions")] {
        if !root.exists() {
            continue;
        }
        let mut pending = vec![root];
        while let Some(directory) = pending.pop() {
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) => {
                    complete = false;
                    warnings.push(format!("セッションフォルダの一部を走査できません: {error}"));
                    continue;
                }
            };
            for entry_result in entries {
                let entry = match entry_result {
                    Ok(entry) => entry,
                    Err(error) => {
                        complete = false;
                        warnings.push(format!("セッション項目の一部を読み取れません: {error}"));
                        continue;
                    }
                };
                seen += 1;
                if seen > MAX_SESSION_FILES {
                    warnings.push("セッション走査が安全上限を超えました。".into());
                    return (ids, false);
                }
                let path = entry.path();
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(_) => {
                        complete = false;
                        continue;
                    }
                };
                if metadata.file_type().is_symlink() {
                    continue;
                }
                if metadata.is_dir() {
                    pending.push(path);
                } else if metadata.is_file() {
                    if let Some(id) = session_id_from_path(&path) {
                        ids.insert(id);
                    }
                }
            }
        }
    }
    (ids, complete)
}

fn session_id_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let candidate = stem.rsplit('-').take(5).collect::<Vec<_>>();
    if candidate.len() != 5 {
        return None;
    }
    let id = candidate.into_iter().rev().collect::<Vec<_>>().join("-");
    looks_like_uuid(&id).then_some(id)
}

fn read_orphan_pins(state_path: &Path, sessions: &BTreeSet<String>) -> Result<Vec<String>, String> {
    if !state_path.exists() {
        return Ok(Vec::new());
    }
    let bytes = read_small_file(state_path, MAX_STATE_BYTES, "Codexグローバル状態")?;
    let state: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Codexグローバル状態のJSONを解析できません: {error}"))?;
    let mut ids = state
        .get("pinned-thread-ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|id| looks_like_uuid(id) && !sessions.contains(*id))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

fn configured_mcp_names(config: &Path, requested: &[String]) -> Result<Vec<String>, String> {
    let raw = String::from_utf8(read_small_file(config, MAX_CONFIG_BYTES, "Codex設定")?)
        .map_err(|_| "Codex設定がUTF-8ではありません".to_string())?;
    let document = raw
        .parse::<DocumentMut>()
        .map_err(|_| "Codex設定TOMLを解析できません。設定形式を確認してください。".to_string())?;
    let servers = document
        .get("mcp_servers")
        .and_then(|item| item.as_table())
        .ok_or("mcp_servers設定が見つかりません")?;
    let mut names = requested.to_vec();
    names.sort();
    names.dedup();
    for name in &names {
        if !servers.contains_key(name) {
            return Err(format!("MCPサーバー {name} は現在の設定にありません"));
        }
    }
    Ok(names)
}

fn sqlite_size_estimate(database: &Path) -> Result<(u64, u64), String> {
    if !database.exists() {
        return Ok((0, 0));
    }
    let physical = regular_file_size(database)?;
    let sqlite3 = system_sqlite3()?;
    let output = run_sqlite(
        &sqlite3,
        database,
        "PRAGMA page_size; PRAGMA page_count; PRAGMA freelist_count;",
    )?;
    let values = output
        .lines()
        .filter_map(|line| line.trim().parse::<u64>().ok())
        .collect::<Vec<_>>();
    if values.len() < 3 {
        return Err("SQLiteのページ情報を取得できませんでした。".into());
    }
    let live = values[0].saturating_mul(values[1].saturating_sub(values[2]));
    Ok((physical, live.min(physical)))
}

fn sqlite_sidecar_bytes(database: &Path) -> u64 {
    ["-wal", "-shm"]
        .iter()
        .filter_map(|suffix| {
            let path = PathBuf::from(format!("{}{suffix}", database.to_string_lossy()));
            fs::symlink_metadata(path)
                .ok()
                .filter(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
                .map(|metadata| metadata.len())
        })
        .sum()
}

fn sqlite_schema(sqlite3: &Path, database: &Path) -> Result<Vec<String>, String> {
    let output = run_sqlite(
        sqlite3,
        database,
        "SELECT type || char(31) || name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name;",
    )?;
    Ok(output.lines().map(str::to_owned).collect())
}

fn sqlite_backup(sqlite3: &Path, source: &Path, destination: &Path) -> Result<(), String> {
    if destination.exists() {
        return Err("SQLiteバックアップ先が既に存在します。".into());
    }
    let command = format!(".backup {}", sqlite_dot_quote(destination)?);
    let result = run_command(
        sqlite3,
        &[OsStr::new("-readonly"), source.as_os_str(), OsStr::new(&command)],
        VACUUM_TIMEOUT,
    )?;
    if !result.success {
        return Err(command_error("SQLite backup", &result));
    }
    regular_file_size(destination)?;
    sync_regular(destination, "SQLiteバックアップ")
}

fn run_sqlite(sqlite3: &Path, database: &Path, sql: &str) -> Result<String, String> {
    run_sqlite_with_timeout(sqlite3, database, sql, COMMAND_TIMEOUT)
}

fn run_sqlite_with_timeout(
    sqlite3: &Path,
    database: &Path,
    sql: &str,
    timeout: Duration,
) -> Result<String, String> {
    let result = run_command(
        sqlite3,
        &[OsStr::new("-readonly"), database.as_os_str(), OsStr::new(sql)],
        timeout,
    )?;
    if !result.success {
        return Err(command_error("sqlite3", &result));
    }
    Ok(result.stdout)
}

fn sqlite_plan_block_reason(
    database: &Path,
    free: Option<u64>,
    required_free: u64,
) -> Option<String> {
    if !database.exists() {
        return Some("SQLite DBが見つかりません。".into());
    }
    if free.is_some_and(|bytes| bytes < required_free) {
        return Some("VACUUM INTOに必要な空き容量が不足しています。".into());
    }
    None
}

fn require_codex_offline(database_or_state: &Path) -> Result<(), String> {
    let pids = codex_process_ids()?;
    if !pids.is_empty() {
        return Err(format!("Codexが実行中です（PID: {}）。完全終了してから再実行してください。", join_pids(&pids)));
    }
    let mut paths = vec![database_or_state.to_path_buf()];
    if database_or_state
        .file_name()
        .and_then(OsStr::to_str)
        == Some("logs_2.sqlite")
    {
        paths.push(PathBuf::from(format!("{}-wal", database_or_state.to_string_lossy())));
        paths.push(PathBuf::from(format!("{}-shm", database_or_state.to_string_lossy())));
    }
    for path in paths {
        if lsof_has_holders(&path)? {
            return Err(format!("{} を使用中のプロセスがあります。", path.display()));
        }
    }
    Ok(())
}

fn codex_process_ids() -> Result<Vec<u32>, String> {
    let result = run_command(
        Path::new("/bin/ps"),
        &[OsStr::new("-axo"), OsStr::new("pid=,comm=")],
        COMMAND_TIMEOUT,
    )?;
    if !result.success {
        return Err(command_error("ps", &result));
    }
    let mut pids = result
        .stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.trim().splitn(2, char::is_whitespace);
            let pid = parts.next()?.parse::<u32>().ok()?;
            let command = parts.next()?.trim().to_ascii_lowercase();
            let name = Path::new(&command).file_name()?.to_str()?;
            (name == "codex"
                || name.starts_with("codex (")
                || name == "codex helper"
                || name.starts_with("codex helper ")
                || (name == "chatgpt" && command.contains("/chatgpt.app/")))
                .then_some(pid)
        })
        .collect::<Vec<_>>();
    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

fn lsof_has_holders(path: &Path) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    let result = run_command(
        Path::new("/usr/sbin/lsof"),
        &[OsStr::new("-nP"), OsStr::new("--"), path.as_os_str()],
        COMMAND_TIMEOUT,
    )?;
    if result.success {
        Ok(result.stdout.lines().skip(1).any(|line| !line.trim().is_empty()))
    } else if result.stdout.trim().is_empty() && result.stderr.trim().is_empty() {
        Ok(false)
    } else {
        Err(command_error("lsof", &result))
    }
}

fn open_files_in_directory(root: &Path) -> HashSet<PathBuf> {
    let result = run_command(
        Path::new("/usr/sbin/lsof"),
        &[
            OsStr::new("-nP"),
            OsStr::new("-Fn"),
            OsStr::new("+D"),
            root.as_os_str(),
        ],
        COMMAND_TIMEOUT,
    );
    let Ok(result) = result else {
        return HashSet::new();
    };
    result
        .stdout
        .lines()
        .filter_map(|line| line.strip_prefix('n'))
        .map(PathBuf::from)
        .map(|path| canonical_or_original(&path))
        .collect()
}

fn available_bytes(path: &Path) -> Result<u64, String> {
    let probe = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    let result = run_command(
        Path::new("/bin/df"),
        &[OsStr::new("-Pk"), probe.as_os_str()],
        COMMAND_TIMEOUT,
    )?;
    if !result.success {
        return Err(command_error("df", &result));
    }
    let line = result.stdout.lines().last().ok_or("dfの結果が空です")?;
    let fields = line.split_whitespace().collect::<Vec<_>>();
    let blocks = fields
        .get(3)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or("dfの空き容量を解析できません")?;
    Ok(blocks.saturating_mul(1024))
}

fn acquire_lock(root: &Path) -> Result<MaintenanceLock, String> {
    fs::create_dir_all(root).map_err(|error| format!("保守データフォルダを作れません: {error}"))?;
    let path = root.join("maintenance.lock");
    let content = format!("{}\n{}\n", std::process::id(), now_ms());
    for attempt in 0..2 {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(content.as_bytes())
                    .and_then(|_| file.sync_all())
                    .map_err(|error| format!("排他ロックを書き込めません: {error}"))?;
                return Ok(MaintenanceLock { path });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempt == 0 => {
                if stale_lock(&path)? {
                    remove_regular_if_exists(&path)?;
                    continue;
                }
                return Err("別のCodexメンテナンスジョブが実行中です。".into());
            }
            Err(error) => return Err(format!("排他ロックを取得できません: {error}")),
        }
    }
    Err("排他ロックを取得できません。".into())
}

fn stale_lock(path: &Path) -> Result<bool, String> {
    let raw = fs::read_to_string(path).map_err(|error| format!("排他ロックを読めません: {error}"))?;
    let mut lines = raw.lines();
    let pid = lines.next().and_then(|line| line.parse::<u32>().ok());
    let created = lines.next().and_then(|line| line.parse::<u64>().ok()).unwrap_or(0);
    if now_ms().saturating_sub(created) < STALE_LOCK_MS {
        return Ok(false);
    }
    let Some(pid) = pid else {
        return Ok(true);
    };
    let result = Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("排他ロックのプロセスを確認できません: {error}"))?;
    Ok(!result.success())
}

fn maintenance_root(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|path| path.join("maintenance"))
        .map_err(|error| format!("CODETASデータフォルダを特定できません: {error}"))
}

fn select_actions<'a>(
    plan: &'a MaintenancePlan,
    requested: &[String],
) -> Result<Vec<&'a MaintenanceActionPreview>, String> {
    let ids = requested.to_vec();
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(id.clone()) {
            continue;
        }
        let action = plan
            .actions
            .iter()
            .find(|action| action.id == id)
            .ok_or_else(|| format!("保守計画にない操作が指定されました: {id}"))?;
        selected.push(action);
    }
    Ok(selected)
}

fn validate_action_target(action: &MaintenanceActionPreview) -> Result<(), String> {
    let codex_root = canonical_or_original(&codex_home()?);
    let expected_logs = canonical_or_original(
        &dirs::home_dir()
            .ok_or("ホームフォルダを特定できません")?
            .join("Library/Logs/com.openai.codex"),
    );
    let valid = match &action.details {
        MaintenanceActionDetails::CleanupTextLogs { log_root, .. } => {
            canonical_or_original(log_root) == expected_logs
        }
        MaintenanceActionDetails::CompactSqlite { database, .. } => {
            canonical_or_original(database) == canonical_or_original(&codex_root.join("logs_2.sqlite"))
        }
        MaintenanceActionDetails::RepairOrphanPins { state_path, .. } => {
            canonical_or_original(state_path)
                == canonical_or_original(&codex_root.join(".codex-global-state.json"))
        }
        MaintenanceActionDetails::DisableMcpServers { config_path, .. } => {
            canonical_or_original(config_path)
                == canonical_or_original(&codex_root.join("config.toml"))
        }
    };
    if valid {
        Ok(())
    } else {
        Err("保守計画の対象パスが許可範囲外です。もう一度プレビューしてください。".into())
    }
}

fn validate_journal_targets(journal: &MaintenanceJobJournal, job_dir: &Path) -> Result<(), String> {
    let codex_root = canonical_or_original(&codex_home()?);
    let expected_logs = canonical_or_original(
        &dirs::home_dir()
            .ok_or("ホームフォルダを特定できません")?
            .join("Library/Logs/com.openai.codex"),
    );
    let backup_root = job_dir.join("backup");
    let trash_root = job_dir.join("trash");
    for action in &journal.pending_actions {
        validate_action_target(action)?;
    }
    for operation in &journal.operations {
        let valid = match operation {
            JournalOperation::MovedToTrash { original, trash } => {
                path_within(original, &expected_logs)
                    && original != &expected_logs
                    && path_within(trash, &trash_root)
            }
            JournalOperation::ReplacedFile { original, backup, .. } => {
                (canonical_or_original(original)
                    == canonical_or_original(&codex_root.join(".codex-global-state.json"))
                    || canonical_or_original(original)
                        == canonical_or_original(&codex_root.join("config.toml")))
                    && path_within(backup, &backup_root)
            }
            JournalOperation::ReplacedSqlite {
                database,
                backup_database,
                saved_wal,
                saved_shm,
                ..
            } => {
                canonical_or_original(database)
                    == canonical_or_original(&codex_root.join("logs_2.sqlite"))
                    && path_within(backup_database, &backup_root)
                    && saved_wal
                        .as_ref()
                        .is_none_or(|path| path_within(path, &backup_root))
                    && saved_shm
                        .as_ref()
                        .is_none_or(|path| path_within(path, &backup_root))
            }
        };
        if !valid {
            return Err("保守ジャーナルの対象パスが許可範囲外です。".into());
        }
    }
    Ok(())
}

fn journal_requires_codex_offline(journal: &MaintenanceJobJournal) -> bool {
    journal.operations.iter().any(|operation| match operation {
        JournalOperation::ReplacedSqlite { .. } => true,
        JournalOperation::ReplacedFile { original, .. } => original
            .file_name()
            .and_then(OsStr::to_str)
            == Some(".codex-global-state.json"),
        JournalOperation::MovedToTrash { .. } => false,
    })
}

fn job_summary(journal: &MaintenanceJobJournal) -> MaintenanceJobSummary {
    MaintenanceJobSummary {
        id: journal.id.clone(),
        plan_id: journal.plan_id.clone(),
        status: journal.status,
        created_at_ms: journal.created_at_ms,
        finished_at_ms: journal.finished_at_ms,
        action_ids: journal.action_ids.clone(),
        reclaimed_bytes: journal.reclaimed_bytes,
        error: journal.error.clone(),
        rollback_available: (!journal.operations.is_empty() || !journal.pending_actions.is_empty())
            && !matches!(
                journal.status,
                MaintenanceJobStatus::RolledBack | MaintenanceJobStatus::Running
            ),
    }
}

fn move_live_sidecar(source: &Path, destination: &Path) -> Result<Option<PathBuf>, String> {
    if !source.exists() {
        return Ok(None);
    }
    fs::rename(source, destination)
        .map_err(|error| format!("SQLite sidecarを退避できません: {error}"))?;
    Ok(Some(destination.to_path_buf()))
}

fn restore_sidecar(saved: &Option<PathBuf>, destination: &Path) {
    if let Some(saved) = saved {
        let _ = fs::rename(saved, destination);
    }
}

fn restore_unapplied_sidecar(saved: &Option<PathBuf>, destination: &Path) -> Result<(), String> {
    let Some(saved) = saved else {
        return Ok(());
    };
    match (saved.exists(), destination.exists()) {
        (false, true) => Ok(()),
        (true, false) => fs::rename(saved, destination)
            .map_err(|error| format!("SQLite sidecarを復元できません: {error}")),
        (true, true) => Err(format!(
            "{} と退避済みsidecarの両方が存在するため上書きしません。",
            destination.display()
        )),
        (false, false) => Err(format!(
            "置換前に存在したSQLite sidecar {} を復元できません。",
            destination.display()
        )),
    }
}

fn restore_file_atomically(backup: &Path, target: &Path) -> Result<(), String> {
    regular_file_size(backup)?;
    let temporary = target.with_extension("codetas-maintenance-restore.tmp");
    if temporary.exists() {
        if sha256_file(&temporary)? != sha256_file(backup)? {
            return Err("復元用一時ファイルの検証に失敗しました。".into());
        }
        return fs::rename(&temporary, target)
            .map_err(|error| format!("中断されたファイル復元を再開できません: {error}"));
    }
    fs::copy(backup, &temporary)
        .map_err(|error| format!("復元用コピーを作成できません: {error}"))?;
    sync_regular(&temporary, "復元ファイル")?;
    fs::rename(&temporary, target)
        .map_err(|error| format!("バックアップを原子的に復元できません: {error}"))
}

fn restore_sqlite_atomically(backup: &Path, target: &Path) -> Result<(), String> {
    regular_file_size(backup)?;
    let temporary = sqlite_restore_temporary(target);
    if temporary.exists() {
        return Err("SQLite復元用一時ファイルが既に存在します。".into());
    }
    fs::rename(backup, &temporary)
        .map_err(|error| format!("SQLiteバックアップを復元位置へ移動できません: {error}"))?;
    if let Err(error) = fs::rename(&temporary, target) {
        let _ = fs::rename(&temporary, backup);
        return Err(format!("SQLiteバックアップを原子的に復元できません: {error}"));
    }
    Ok(())
}

fn sqlite_restore_temporary(target: &Path) -> PathBuf {
    target.with_extension("codetas-maintenance-sqlite-restore.tmp")
}

fn remove_regular_if_exists(path: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{} を確認できません: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("{} は通常ファイルではないため削除しません。", path.display()));
    }
    fs::remove_file(path).map_err(|error| format!("{} を削除できません: {error}", path.display()))
}

fn regular_file_size(path: &Path) -> Result<u64, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("{} を確認できません: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("{} は通常ファイルではありません。", path.display()));
    }
    Ok(metadata.len())
}

fn sync_regular(path: &Path, label: &str) -> Result<(), String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("{label}をディスクへ同期できません: {error}"))
}

fn system_sqlite3() -> Result<PathBuf, String> {
    let configured = Path::new("/usr/bin/sqlite3");
    let metadata = fs::symlink_metadata(configured)
        .map_err(|error| format!("macOS標準sqlite3を確認できません: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("/usr/bin/sqlite3が通常ファイルではありません。".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("/usr/bin/sqlite3に実行権限がありません。".into());
        }
    }
    fs::canonicalize(configured)
        .map_err(|error| format!("macOS標準sqlite3を解決できません: {error}"))
}

fn file_fingerprint(path: &Path) -> Result<(u64, u64), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("{} を確認できません: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("{} は通常ファイルではありません。", path.display()));
    }
    Ok((
        metadata.len(),
        system_time_ms(metadata.modified().unwrap_or(UNIX_EPOCH)),
    ))
}

fn optional_file_fingerprint(path: &Path) -> Result<Option<(u64, u64)>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(format!("{} は通常ファイルではありません。", path.display()));
            }
            Ok(Some((
                metadata.len(),
                system_time_ms(metadata.modified().unwrap_or(UNIX_EPOCH)),
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{} を確認できません: {error}", path.display())),
    }
}

fn read_small_file(path: &Path, maximum: u64, label: &str) -> Result<Vec<u8>, String> {
    let size = regular_file_size(path)?;
    if size > maximum {
        return Err(format!("{label}が安全上限（{maximum} bytes）を超えています。"));
    }
    fs::read(path).map_err(|error| format!("{label}を読み取れません: {error}"))
}

fn validate_mcp_names(names: &[String]) -> Result<(), String> {
    if names.len() > 64 {
        return Err("一度に変更できるMCPサーバーは64件までです。".into());
    }
    for name in names {
        if name.is_empty()
            || name.len() > 120
            || name.chars().any(|character| character.is_control())
        {
            return Err("MCPサーバー名が安全な形式ではありません。".into());
        }
    }
    Ok(())
}

fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 100
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err("保守IDが安全な形式ではありません。".into());
    }
    Ok(())
}

fn validate_relative(path: &Path) -> Result<(), String> {
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err("相対パスが許可範囲外です。".into());
    }
    Ok(())
}

fn validate_log_restore_destination(path: &Path) -> Result<(), String> {
    let expected_root = fs::canonicalize(
        dirs::home_dir()
            .ok_or("ホームフォルダを特定できません")?
            .join("Library/Logs/com.openai.codex"),
    )
    .map_err(|error| format!("Codexログ保存先を確認できません: {error}"))?;
    let parent = path.parent().ok_or("ログ復元先の親フォルダがありません")?;
    let canonical_parent = fs::canonicalize(parent)
        .map_err(|error| format!("ログ復元先を正規化できません: {error}"))?;
    if canonical_parent.starts_with(&expected_root) {
        Ok(())
    } else {
        Err("ログ復元先が許可されたフォルダ外を指しています。".into())
    }
}

fn path_within(path: &Path, root: &Path) -> bool {
    path.is_absolute()
        && !path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
        && path.starts_with(root)
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("保守データをシリアライズできません: {error}"))?;
    atomic_write(path, &bytes)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let bytes = read_small_file(path, 16 * 1024 * 1024, "保守データ")?;
    serde_json::from_slice(&bytes).map_err(|error| format!("保守データを解析できません: {error}"))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|error| format!("ファイルを検証できません: {error}"))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("ファイルを検証できません: {error}"))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn run_command(program: &Path, arguments: &[&OsStr], timeout: Duration) -> Result<CommandResult, String> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .group_spawn()
        .map_err(|error| format!("{}を開始できませんでした: {error}", program.display()))?;
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
                return Err(format!("{}が{}秒でタイムアウトしました", program.display(), timeout.as_secs()));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("{}の状態確認に失敗しました: {error}", program.display()));
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
        if output.len() < MAX_COMMAND_OUTPUT {
            let remaining = MAX_COMMAND_OUTPUT - output.len();
            output.extend_from_slice(&buffer[..read.min(remaining)]);
        }
    }
    output
}

fn command_error(label: &str, result: &CommandResult) -> String {
    let detail = if !result.stderr.trim().is_empty() {
        result.stderr.trim()
    } else {
        result.stdout.trim()
    };
    if detail.is_empty() {
        format!("{label}が失敗しました")
    } else {
        format!("{label}が失敗しました: {}", sanitize_error_text(detail))
    }
}

fn sanitize_error_text(value: &str) -> String {
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
                "<redacted-email>".to_string()
            } else {
                component.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
        .chars()
        .take(500)
        .collect()
}

fn sqlite_quote(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "''")
}

fn sqlite_dot_quote(path: &Path) -> Result<String, String> {
    let raw = path.to_string_lossy();
    if raw.chars().any(char::is_control) {
        return Err("SQLiteバックアップパスに制御文字が含まれています。".into());
    }
    Ok(format!(
        "\"{}\"",
        raw.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

fn looks_like_uuid(value: &str) -> bool {
    value.len() == 36
        && value.chars().enumerate().all(|(index, character)| match index {
            8 | 13 | 18 | 23 => character == '-',
            _ => character.is_ascii_hexdigit(),
        })
}

fn join_pids(pids: &[u32]) -> String {
    pids.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
}

fn canonical_or_original(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn system_time_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn now_ms() -> u64 {
    system_time_ms(SystemTime::now())
}

fn new_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}-{}",
        now_ms(),
        std::process::id(),
        JOB_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_retention_means_never_delete_logs() {
        let request: MaintenancePreviewRequest = serde_json::from_str(
            r#"{"logRetentionDays":null,"compactSqlite":false}"#,
        )
        .expect("preview request");
        assert_eq!(request.log_retention_days, None);
    }

    #[test]
    fn extracts_session_uuid_from_rollout_filename() {
        let path = Path::new(
            "/tmp/rollout-2026-08-15T08-46-26-01a002ab-772a-7553-b882-d2675d3d6ee6.jsonl",
        );
        assert_eq!(
            session_id_from_path(path).as_deref(),
            Some("01a002ab-772a-7553-b882-d2675d3d6ee6")
        );
    }

    #[test]
    fn rejects_parent_directory_candidates() {
        assert!(validate_relative(Path::new("safe/log.txt")).is_ok());
        assert!(validate_relative(Path::new("../outside.txt")).is_err());
    }

    #[test]
    fn rejects_malformed_thread_ids() {
        assert!(looks_like_uuid("01a002ab-772a-7553-b882-d2675d3d6ee6"));
        assert!(!looks_like_uuid("not-a-thread"));
    }
}
