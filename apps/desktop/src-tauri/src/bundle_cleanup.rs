//! `/Applications` に残った旧 CODETAS アプリの自動掃除。
//!
//! 旧ビルドは、新しいビルドが保存した `providers.json`（registry revision が
//! 上がった設定）を読めず「プロバイダ設定を移行できません: unsupported registry
//! revision」で停止する。旧ビルド自身はその場で自分を消せないため、新しいビルドの
//! 起動時に、使われていない旧バンドルをゴミ箱へ移す。直近の 1 個だけはロールバック用に
//! 残す。
//!
//! 対象はインストーラが作る `CODETAS.app.backup-YYYYMMDD-HHMMSS` だけで、それ以外の
//! 名前は手で付けたものとして残す。インストール途中の `.CODETAS.install-*` も対象外。
//! 実行できるのは `/Applications/CODETAS.app` で動く本番ビルドだけで、検証用ビルドや
//! 開発ビルドからは移動しない。削除はせず `~/.Trash` への rename だけを行うので、
//! 失敗してもバンドルは元の場所に残る。

#![cfg_attr(not(target_os = "macos"), allow(dead_code, unused_imports))]

use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    time::SystemTime,
};

#[cfg(target_os = "macos")]
const APPLICATIONS_DIR: &str = "/Applications";
const INSTALLED_BUNDLE: &str = "/Applications/CODETAS.app";
const BACKUP_PREFIX: &str = "CODETAS.app.backup-";
const MAX_KEEP_PREVIOUS: usize = 1;
const MAX_LOG_BYTES: u64 = 512 * 1024;

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppBundleSweepReport {
    pub dry_run: bool,
    /// Why nothing was moved. The command line prints it and the log records it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
    pub removed: Vec<String>,
    pub kept: Vec<String>,
    pub skipped: Vec<AppBundleSkip>,
    /// Size of what moved to the Trash. It is freed when the Trash is emptied.
    pub reclaimed_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppBundleSkip {
    pub path: String,
    pub reason: String,
}

struct SweepCandidate {
    path: PathBuf,
    stamp: String,
    bytes: u64,
}

/// `YYYYMMDD-HHMMSS` from a name an installer gives the bundle it replaces.
///
/// Every other name stays out of the sweep, even a similar one: nothing creates
/// `CODETAS.backup-opencode-*.app` or `.CODETAS-*-previous-*.app`, so someone named
/// those on purpose. `.CODETAS.install-*.app` is a build still being installed, and
/// `CODETAS.app.failed-*` is kept by the installer for inspection.
///
/// The stamp also orders the candidates. It records when the bundle was replaced,
/// where a directory's mtime is the build time, which `rename` does not change: a
/// bad build rolled back from would outrank the good one replaced after it.
fn backup_stamp(name: &str) -> Option<&str> {
    let stamp = name.strip_prefix(BACKUP_PREFIX)?;
    let (date, time) = stamp.split_once('-')?;
    let digits = |part: &str, length: usize| {
        part.len() == length && part.bytes().all(|byte| byte.is_ascii_digit())
    };
    (digits(date, 8) && digits(time, 6)).then_some(stamp)
}

fn directory_bytes(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let entry_path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&entry_path) else {
            continue;
        };
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            total += directory_bytes(&entry_path);
        } else if metadata.is_file() {
            total += metadata.len();
        }
    }
    total
}

fn looks_like_codetas_bundle(path: &Path) -> bool {
    let Ok(entries) = fs::read_dir(path.join("Contents/MacOS")) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        name.starts_with("codetas")
            && fs::symlink_metadata(entry.path())
                .map(|metadata| metadata.is_file())
                .unwrap_or(false)
    })
}

/// `/Applications` 直下の候補を新しく置き換えられた順に返す。通常ディレクトリ以外は除外する。
fn stale_bundles(root: &Path, running: Option<&Path>) -> Vec<SweepCandidate> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
        else {
            continue;
        };
        let Some(stamp) = backup_stamp(&name).map(str::to_string) else {
            continue;
        };
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        if running.is_some_and(|running| same_path(running, &path)) {
            continue;
        }
        if !looks_like_codetas_bundle(&path) {
            continue;
        }
        candidates.push(SweepCandidate {
            bytes: directory_bytes(&path),
            path,
            stamp,
        });
    }
    candidates.sort_by(|left, right| {
        right
            .stamp
            .cmp(&left.stamp)
            .then_with(|| right.path.cmp(&left.path))
    });
    candidates
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// A unique name, so a bundle from an earlier sweep in the Trash is never replaced.
fn unused_trash_path(trash: &Path, name: &str, moment: u128) -> PathBuf {
    let process = std::process::id();
    let mut attempt = 0_u32;
    loop {
        let candidate = trash.join(format!("{name}-{moment}-{process}-{attempt}"));
        if fs::symlink_metadata(&candidate).is_err() {
            return candidate;
        }
        attempt += 1;
    }
}

/// A rename, so a failure part way cannot leave a half-deleted bundle. It fails
/// across volumes and without permission, and the caller records that and moves on.
fn move_to_trash(bundle: &Path, trash: &Path) -> std::io::Result<PathBuf> {
    let name = bundle
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .ok_or_else(|| std::io::Error::other("bundle path has no name"))?;
    fs::create_dir_all(trash)?;
    let moment = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    let target = unused_trash_path(trash, &name, moment);
    fs::rename(bundle, &target)?;
    Ok(target)
}

/// 純粋な掃除処理。`is_running` を差し替えられるのでテストから実プロセスを見ずに検証できる。
fn sweep_with(
    root: &Path,
    running: Option<&Path>,
    keep_previous: usize,
    dry_run: bool,
    trash: &Path,
    is_running: &dyn Fn(&Path) -> bool,
) -> AppBundleSweepReport {
    let mut report = AppBundleSweepReport {
        dry_run,
        ..AppBundleSweepReport::default()
    };
    let mut kept_previous = 0;
    for candidate in stale_bundles(root, running) {
        let display = candidate.path.to_string_lossy().to_string();
        if is_running(&candidate.path) {
            report.skipped.push(AppBundleSkip {
                path: display,
                reason: "in_use".into(),
            });
            continue;
        }
        if kept_previous < keep_previous_max(keep_previous) {
            kept_previous += 1;
            report.kept.push(display);
            continue;
        }
        if dry_run {
            report.removed.push(display);
            report.reclaimed_bytes += candidate.bytes;
            continue;
        }
        match move_to_trash(&candidate.path, trash) {
            Ok(_) => {
                report.removed.push(display);
                report.reclaimed_bytes += candidate.bytes;
            }
            Err(error) => report.skipped.push(AppBundleSkip {
                path: display,
                reason: format!("trash_failed: {error}"),
            }),
        }
    }
    report
}

fn keep_previous_max(requested: usize) -> usize {
    requested.min(3)
}

/// 0 is a match and 1 is no match. Anything else means pgrep could not answer, which
/// must not read as nothing having been started from this bundle.
fn pgrep_status_means_in_use(code: Option<i32>) -> bool {
    code != Some(1)
}

/// Whether a process was started under this bundle's backup name, which is a build
/// someone launched for a rollback. It does not see a process started before the
/// installer renamed the bundle: its argv still says `/Applications/CODETAS.app`. That
/// is safe to miss, because a rename keeps the inode and the process keeps running.
#[cfg(target_os = "macos")]
fn process_uses_bundle(path: &Path) -> bool {
    let marker = format!("{}/Contents/MacOS/", path.to_string_lossy());
    Command::new("pgrep")
        .arg("-f")
        .arg(&marker)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| pgrep_status_means_in_use(status.code()))
        .unwrap_or(true)
}

#[cfg(target_os = "macos")]
fn current_bundle() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    executable
        .ancestors()
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
        })
        .map(Path::to_path_buf)
}

/// A validation build is a sandbox and a debug build is someone's checkout. Neither
/// may touch the rollback copies of the app actually installed on this machine.
fn production_build() -> bool {
    is_production_build(cfg!(debug_assertions), cfg!(feature = "validation-build"))
}

fn is_production_build(debug_assertions: bool, validation_build: bool) -> bool {
    !debug_assertions && !validation_build
}

fn sweep_refusal(running: Option<&Path>, production: bool) -> Option<String> {
    if !production {
        return Some("not_a_production_build".into());
    }
    if running != Some(Path::new(INSTALLED_BUNDLE)) {
        return Some("not_running_from_installed_bundle".into());
    }
    None
}

fn refused_report(dry_run: bool, reason: String) -> AppBundleSweepReport {
    AppBundleSweepReport {
        dry_run,
        refused: Some(reason),
        ..AppBundleSweepReport::default()
    }
}

/// 起動時とコマンドラインから呼ぶ本体。macOS 以外では何もしない。dry run は何も動かさない
/// ので、どのビルドからでも実行できる。
fn run(dry_run: bool) -> AppBundleSweepReport {
    #[cfg(target_os = "macos")]
    {
        let running = current_bundle();
        if !dry_run {
            if let Some(reason) = sweep_refusal(running.as_deref(), production_build()) {
                return refused_report(dry_run, reason);
            }
        }
        let Some(trash) = dirs::home_dir().map(|home| home.join(".Trash")) else {
            return refused_report(dry_run, "no_home_directory".into());
        };
        sweep_with(
            Path::new(APPLICATIONS_DIR),
            running.as_deref(),
            MAX_KEEP_PREVIOUS,
            dry_run,
            &trash,
            &process_uses_bundle,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        AppBundleSweepReport {
            dry_run,
            ..AppBundleSweepReport::default()
        }
    }
}

/// Unreadable settings can mean the running build is the wrong one, and then the
/// rollback copies are exactly what the user needs. A dry run moves nothing, so it
/// stays available for looking into that state.
fn settings_refusal(dry_run: bool, settings: &Result<(), String>) -> Option<String> {
    match settings {
        Err(error) if !dry_run => Some(format!("settings_unreadable: {error}")),
        _ => None,
    }
}

pub fn run_unless_settings_are_broken(
    dry_run: bool,
    settings: Result<(), String>,
) -> AppBundleSweepReport {
    match settings_refusal(dry_run, &settings) {
        Some(reason) => refused_report(dry_run, reason),
        None => run(dry_run),
    }
}

/// What the last startup sweep could not move, for diagnostics. It lasts as long as
/// the process because every start sweeps again.
static TRASH_FAILURES: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn remember_trash_failures(report: &AppBundleSweepReport) {
    *TRASH_FAILURES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = failed_moves(report);
}

pub fn trash_failures() -> Vec<String> {
    TRASH_FAILURES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn failed_moves(report: &AppBundleSweepReport) -> Vec<String> {
    report
        .skipped
        .iter()
        .filter(|skip| skip.reason.starts_with("trash_failed:"))
        .map(|skip| format!("{}: {}", skip.path, skip.reason))
        .collect()
}

/// 結果を JSONL で残す。UI を出せない起動直後の掃除でもあとから追えるようにする。
pub fn append_log(log_path: &Path, report: &AppBundleSweepReport) -> std::io::Result<()> {
    use std::io::Write;
    if report.refused.is_none() && report.removed.is_empty() && report.skipped.is_empty() {
        return Ok(());
    }
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::metadata(log_path)
        .map(|metadata| metadata.len() > MAX_LOG_BYTES)
        .unwrap_or(false)
    {
        let _ = fs::rename(log_path, log_path.with_extension("jsonl.1"));
    }
    let record = serde_json::json!({
        "at": SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default(),
        "dryRun": report.dry_run,
        "refused": report.refused,
        "removed": report.removed,
        "kept": report.kept,
        "skipped": report.skipped,
        "reclaimedBytes": report.reclaimed_bytes,
    });
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    writeln!(file, "{record}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const OLDEST: &str = "CODETAS.app.backup-20261101-000000";
    const MIDDLE: &str = "CODETAS.app.backup-20261103-120000";
    const NEWEST: &str = "CODETAS.app.backup-20261105-194223";

    struct TempRoot {
        path: PathBuf,
        trash: PathBuf,
    }

    impl TempRoot {
        fn new(label: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let base = std::env::temp_dir().join(format!("codetas-bundles-{label}-{unique}"));
            let path = base.join("Applications");
            fs::create_dir_all(&path).unwrap();
            Self {
                trash: base.join("Trash"),
                path,
            }
        }

        fn bundle(&self, name: &str) -> PathBuf {
            let bundle = self.path.join(name);
            fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
            fs::write(bundle.join("Contents/MacOS/codetas-desktop"), b"bin").unwrap();
            std::thread::sleep(Duration::from_millis(12));
            bundle
        }

        fn sweep(&self, running: Option<&Path>, keep: usize) -> AppBundleSweepReport {
            sweep_with(
                &self.path,
                running,
                keep,
                false,
                &self.trash,
                &never_running,
            )
        }

        fn trashed(&self) -> Vec<String> {
            let Ok(entries) = fs::read_dir(&self.trash) else {
                return Vec::new();
            };
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .collect()
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            if let Some(base) = self.path.parent() {
                let _ = fs::remove_dir_all(base);
            }
        }
    }

    fn never_running(_: &Path) -> bool {
        false
    }

    #[test]
    fn moves_old_bundles_to_the_trash_and_keeps_the_last_one_replaced() {
        let root = TempRoot::new("sweep");
        root.bundle("CODETAS.app");
        root.bundle(OLDEST);
        root.bundle(MIDDLE);
        root.bundle(NEWEST);

        let running = root.path.join("CODETAS.app");
        let report = root.sweep(Some(&running), 1);

        assert_eq!(report.removed.len(), 2, "{:?}", report.removed);
        assert_eq!(report.kept.len(), 1, "{:?}", report.kept);
        assert!(report.reclaimed_bytes > 0);
        assert!(root.path.join("CODETAS.app").is_dir());
        assert!(root.path.join(NEWEST).is_dir());
        assert!(!root.path.join(OLDEST).exists());
        assert!(!root.path.join(MIDDLE).exists());

        // Still recoverable, with its contents.
        let trashed = root.trashed();
        assert_eq!(trashed.len(), 2, "{trashed:?}");
        for name in trashed {
            assert!(root
                .trash
                .join(name)
                .join("Contents/MacOS/codetas-desktop")
                .is_file());
        }
    }

    #[test]
    fn ranks_by_when_a_bundle_was_replaced_not_when_it_was_built() {
        let root = TempRoot::new("rank");
        // The known-good build was built first but replaced last, and the bad build
        // rolled back from was built later. Its newer mtime must not win.
        root.bundle(NEWEST);
        root.bundle(MIDDLE);

        let report = root.sweep(None, 1);

        assert!(root.path.join(NEWEST).is_dir(), "{report:?}");
        assert!(!root.path.join(MIDDLE).exists(), "{report:?}");
    }

    #[test]
    fn never_touches_a_bundle_an_installer_did_not_name() {
        let root = TempRoot::new("names");
        let untouched = [
            root.bundle("CODETAS.app"),
            root.bundle(".CODETAS.install-20261104-001133.app"),
            root.bundle("CODETAS.app.failed-20261104-001133"),
            root.bundle(".CODETAS.install-20261104-001133.app.failed"),
            root.bundle("CODETAS.backup-opencode-20261105-192610.app"),
            root.bundle(".CODETAS-argon-previous-59bcb6f3.app"),
            root.bundle("CODETAS.app.backup-keep-this-one"),
            root.bundle("CODETAS Validation.app"),
        ];

        let report = root.sweep(None, 0);

        assert!(report.removed.is_empty(), "{report:?}");
        assert!(report.kept.is_empty(), "{report:?}");
        for bundle in untouched {
            assert!(bundle.is_dir(), "{}", bundle.display());
        }
        assert!(root.trashed().is_empty());
    }

    #[test]
    fn a_staged_install_does_not_use_up_the_kept_slot() {
        let root = TempRoot::new("staged");
        let backup = root.bundle(NEWEST);
        let staged = root.bundle(".CODETAS.install-20261106-010101.app");

        let report = root.sweep(None, 1);

        assert_eq!(report.kept, vec![backup.to_string_lossy().to_string()]);
        assert!(backup.is_dir());
        assert!(staged.is_dir());
    }

    #[test]
    fn never_removes_the_running_bundle_or_unrelated_apps() {
        let root = TempRoot::new("running");
        // A name the filter accepts, so only the running check protects it.
        let running = root.bundle(MIDDLE);
        let unrelated = root.bundle("Slack.app");
        let stale = root.bundle(OLDEST);

        let report = root.sweep(Some(&running), 0);

        assert!(running.is_dir());
        assert!(unrelated.is_dir());
        assert!(!stale.exists());
        assert_eq!(report.removed, vec![stale.to_string_lossy().to_string()]);
    }

    #[test]
    fn keeps_bundles_that_are_still_in_use() {
        let root = TempRoot::new("in-use");
        let in_use = root.bundle(NEWEST);
        let stale = root.bundle(OLDEST);

        let running = root.path.join("CODETAS.app");
        let report = sweep_with(
            &root.path,
            Some(&running),
            0,
            false,
            &root.trash,
            &|path: &Path| path == in_use,
        );

        assert!(in_use.is_dir());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, "in_use");
        assert_eq!(report.removed, vec![stale.to_string_lossy().to_string()]);
    }

    #[test]
    fn dry_run_reports_without_moving() {
        let root = TempRoot::new("dry-run");
        let target = root.bundle(NEWEST);

        let report = sweep_with(&root.path, None, 0, true, &root.trash, &never_running);

        assert_eq!(report.removed.len(), 1);
        assert!(report.dry_run);
        assert!(target.is_dir());
        assert!(root.trashed().is_empty());
    }

    #[test]
    fn a_bundle_that_cannot_reach_the_trash_stays_and_the_reason_is_recorded() {
        let root = TempRoot::new("trash-fails");
        let target = root.bundle(NEWEST);
        // A file where the Trash should be makes both the mkdir and the rename fail.
        fs::write(&root.trash, b"not a directory").unwrap();

        let report = root.sweep(None, 0);

        assert!(target.join("Contents/MacOS/codetas-desktop").is_file());
        assert!(report.removed.is_empty());
        assert_eq!(report.reclaimed_bytes, 0);
        assert_eq!(report.skipped.len(), 1);
        assert!(
            report.skipped[0].reason.starts_with("trash_failed:"),
            "{report:?}"
        );
    }

    #[test]
    fn a_name_already_in_the_trash_is_not_replaced() {
        let root = TempRoot::new("trash-collision");
        fs::create_dir_all(&root.trash).unwrap();
        let first = unused_trash_path(&root.trash, NEWEST, 7);
        fs::create_dir_all(&first).unwrap();

        // The same bundle name in the same millisecond.
        let second = unused_trash_path(&root.trash, NEWEST, 7);

        assert_ne!(first, second);
        assert!(!second.exists());
    }

    #[cfg(unix)]
    #[test]
    fn ignores_symlinked_entries() {
        let root = TempRoot::new("symlink");
        let real = root.bundle("real-target");
        let link = root.path.join(NEWEST);
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let report = root.sweep(None, 0);

        assert!(report.removed.is_empty());
        assert!(real.is_dir());
        assert!(fs::symlink_metadata(&link).is_ok());
    }

    #[test]
    fn a_directory_without_a_codetas_executable_is_left_alone() {
        let root = TempRoot::new("not-a-bundle");
        let lookalike = root.path.join(NEWEST);
        fs::create_dir_all(lookalike.join("Contents/MacOS")).unwrap();
        fs::write(lookalike.join("Contents/MacOS/something-else"), b"bin").unwrap();

        let report = root.sweep(None, 0);

        assert!(report.removed.is_empty());
        assert!(lookalike.is_dir());
    }

    #[test]
    fn accepts_only_the_stamp_installers_write() {
        assert_eq!(
            backup_stamp("CODETAS.app.backup-20261104-113227"),
            Some("20261104-113227")
        );
        for name in [
            "CODETAS.app",
            "CODETAS.app.backup",
            "CODETAS.app.backup-",
            "CODETAS.app.backup-20260101",
            "CODETAS.app.backup-20261104-11322",
            "CODETAS.app.backup-20261104-1132270",
            "CODETAS.app.backup-2026110a-113227",
            "CODETAS.app.backup-20261104-113227 copy",
            "codetas.app.backup-20261104-113227",
            "Google Chrome.app",
        ] {
            assert_eq!(backup_stamp(name), None, "{name}");
        }
    }

    #[test]
    fn only_a_production_build_running_from_the_installed_path_may_sweep() {
        let installed = Path::new(INSTALLED_BUNDLE);
        assert_eq!(sweep_refusal(Some(installed), true), None);

        for elsewhere in [
            None,
            Some(Path::new(
                "/Applications/CODETAS.app.backup-20261105-194223",
            )),
            Some(Path::new("/tmp/CODETAS.app")),
            Some(Path::new("/Applications/CODETAS Validation.app")),
        ] {
            assert!(sweep_refusal(elsewhere, true).is_some(), "{elsewhere:?}");
        }
        assert!(sweep_refusal(Some(installed), false).is_some());
    }

    #[test]
    fn only_a_release_build_that_is_not_a_validation_build_is_a_production_build() {
        for (debug_assertions, validation_build, production) in [
            (false, false, true),
            (false, true, false),
            (true, false, false),
            (true, true, false),
        ] {
            assert_eq!(
                is_production_build(debug_assertions, validation_build),
                production,
                "debug={debug_assertions} validation={validation_build}"
            );
        }
    }

    #[test]
    fn a_refusal_is_recorded_in_the_log() {
        let root = TempRoot::new("log-refused");
        let log = root.path.join("sweep.jsonl");

        append_log(
            &log,
            &refused_report(false, "settings_unreadable: x".into()),
        )
        .unwrap();

        let line = fs::read_to_string(&log).unwrap();
        let record = serde_json::from_str::<serde_json::Value>(line.trim()).unwrap();
        assert_eq!(record["refused"], "settings_unreadable: x");
    }

    #[test]
    fn a_failed_move_is_recorded_and_offered_to_diagnostics() {
        let root = TempRoot::new("log-trash-failed");
        let target = root.bundle(NEWEST);
        fs::write(&root.trash, b"not a directory").unwrap();
        let log = root.path.join("sweep.jsonl");

        let report = root.sweep(None, 0);
        append_log(&log, &report).unwrap();

        let line = fs::read_to_string(&log).unwrap();
        let record = serde_json::from_str::<serde_json::Value>(line.trim()).unwrap();
        assert!(record["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .starts_with("trash_failed:"));
        let failures = failed_moves(&report);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains(target.to_string_lossy().as_ref()));
    }

    #[test]
    fn a_bundle_left_in_use_is_not_a_failed_move() {
        let report = AppBundleSweepReport {
            skipped: vec![AppBundleSkip {
                path: "/Applications/CODETAS.app.backup-20261105-194223".into(),
                reason: "in_use".into(),
            }],
            ..AppBundleSweepReport::default()
        };
        assert!(failed_moves(&report).is_empty());
    }

    #[test]
    fn a_sweep_with_nothing_to_say_writes_no_log() {
        let root = TempRoot::new("log-empty");
        let log = root.path.join("sweep.jsonl");

        append_log(&log, &AppBundleSweepReport::default()).unwrap();

        assert!(!log.exists());
    }

    #[test]
    fn broken_settings_refuse_a_real_sweep_but_not_a_dry_run() {
        let broken = Err("unsupported registry revision".to_string());
        let reason = settings_refusal(false, &broken).expect("refused");
        assert!(reason.contains("unsupported registry revision"));
        assert_eq!(settings_refusal(true, &broken), None);
        assert_eq!(settings_refusal(false, &Ok(())), None);

        let report = run_unless_settings_are_broken(false, broken);
        assert!(report.refused.is_some());
        assert!(report.removed.is_empty());
    }

    #[test]
    fn a_pgrep_failure_counts_as_in_use() {
        assert!(pgrep_status_means_in_use(Some(0)));
        assert!(!pgrep_status_means_in_use(Some(1)));
        assert!(pgrep_status_means_in_use(Some(2)));
        assert!(pgrep_status_means_in_use(None));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pgrep_sees_a_process_started_under_the_backup_name() {
        let root = TempRoot::new("pgrep");
        let bundle = root.bundle(NEWEST);
        assert!(!process_uses_bundle(&bundle));

        // `sleep; :` keeps the shell alive with the marker in its arguments, the way
        // a build launched from the backup path has that path in argv[0].
        let mut shell = Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 5; :")
            .arg(bundle.join("Contents/MacOS/codetas-desktop"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let seen = process_uses_bundle(&bundle);
        let _ = shell.kill();
        let _ = shell.wait();

        assert!(seen);
        assert!(!process_uses_bundle(&bundle));
    }
}
