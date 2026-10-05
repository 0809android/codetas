//! Fetch the official CLI into CODETAS's cache, never replacing the user's CLI.
use super::*;
use base64::Engine;
use sha2::{Digest, Sha512};
use std::io::Read;

const LIMIT: usize = 512 * 1024 * 1024;
const REGISTRY: &str = "https://registry.npmjs.org";
static UPDATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn platform() -> Result<(&'static str, &'static str, &'static str), String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Ok(("win32-x64", "x86_64-pc-windows-msvc", "codex.exe")),
        ("windows", "aarch64") => Ok(("win32-arm64", "aarch64-pc-windows-msvc", "codex.exe")),
        ("macos", "x86_64") => Ok(("darwin-x64", "x86_64-apple-darwin", "codex")),
        ("macos", "aarch64") => Ok(("darwin-arm64", "aarch64-apple-darwin", "codex")),
        ("linux", "x86_64") => Ok(("linux-x64", "x86_64-unknown-linux-musl", "codex")),
        ("linux", "aarch64") => Ok(("linux-arm64", "aarch64-unknown-linux-musl", "codex")),
        _ => Err("この環境のCodex CLI自動更新は未対応です".into()),
    }
}

async fn bounded(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    let mut response = response.error_for_status()
        .map_err(|_| "公式Codex CLIの取得先がエラーを返しました".to_string())?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await
        .map_err(|_| "公式Codex CLIのダウンロードに失敗しました".to_string())? {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err("公式Codex CLIのダウンロードがサイズ上限を超えました".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn verified_binary(bytes: &[u8], integrity: &str, target: &str) -> Result<Vec<u8>, String> {
    let digest = base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes));
    if integrity != format!("sha512-{digest}") {
        return Err("公式Codex CLIの整合性検証に失敗しました".into());
    }
    let gzip = flate2::read::GzDecoder::new(bytes);
    // Bound decompression too, and extract only the exact executable path.
    let mut archive = tar::Archive::new(gzip.take((LIMIT + 1) as u64));
    for entry in archive.entries().map_err(|_| "Codex CLIのアーカイブが不正です")? {
        let mut entry = entry.map_err(|_| "Codex CLIのアーカイブを読めません")?;
        if entry.path().map_err(|_| "Codex CLIのパスが不正です")?.as_ref() != Path::new(target) {
            continue;
        }
        if !entry.header().entry_type().is_file() || entry.size() > LIMIT as u64 {
            return Err("Codex CLIの実行ファイルが不正です".into());
        }
        let mut binary = Vec::new();
        entry.read_to_end(&mut binary).map_err(|_| "Codex CLIを展開できません")?;
        if binary.is_empty() {
            return Err("Codex CLIの実行ファイルが空です".into());
        }
        return Ok(binary);
    }
    Err("公式配布にCodex CLI実行ファイルがありません".into())
}

pub(super) async fn latest_executable(app: &AppHandle) -> Result<PathBuf, String> {
    let cache = app.path().app_cache_dir().map_err(|_| "CODETASキャッシュを特定できません")?;
    latest_executable_in(&cache).await
}

async fn latest_executable_in(cache: &Path) -> Result<PathBuf, String> {
    let _guard = UPDATE_LOCK.lock().await;
    let (platform, triple, filename) = platform()?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build().map_err(|_| "Codex CLI更新用の接続を作れません")?;
    let latest = client.get(format!("{REGISTRY}/@openai%2fcodex/latest"))
        .send().await.map_err(|_| "公式Codex CLIの最新版を確認できません")?;
    let latest: JsonValue = serde_json::from_slice(&bounded(latest, 1024 * 1024).await?)
        .map_err(|_| "公式Codex CLIのバージョン情報が不正です")?;
    let version = latest.get("version").and_then(JsonValue::as_str)
        .filter(|v| valid_version(v)).ok_or("公式Codex CLIのバージョンが不正です")?;
    let metadata = client.get(format!("{REGISTRY}/@openai%2fcodex/{version}-{platform}"))
        .send().await.map_err(|_| "公式Codex CLIの配布情報を取得できません")?;
    let metadata: JsonValue = serde_json::from_slice(&bounded(metadata, 1024 * 1024).await?)
        .map_err(|_| "公式Codex CLIの配布情報が不正です")?;
    let dist = metadata.get("dist").ok_or("公式Codex CLIの配布情報がありません")?;
    let url = dist.get("tarball").and_then(JsonValue::as_str).ok_or("Codex CLIのURLがありません")?;
    let parsed = reqwest::Url::parse(url).map_err(|_| "Codex CLIのURLが不正です")?;
    if parsed.scheme() != "https" || parsed.host_str() != Some("registry.npmjs.org")
        || !parsed.username().is_empty() || parsed.password().is_some() || parsed.port().is_some() {
        return Err("Codex CLIのダウンロード先が公式npmレジストリではありません".into());
    }
    let integrity = dist.get("integrity").and_then(JsonValue::as_str)
        .ok_or("Codex CLIの整合性情報がありません")?.to_string();
    let root = cache.join("codex-cli").join(version);
    let archive_path = root.join("package.tgz");
    let bytes = match fs::read(&archive_path) {
        Ok(bytes) if bytes.len() <= LIMIT
            && integrity == format!("sha512-{}", base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&bytes))) => bytes,
        _ => {
            let response = client.get(parsed).send().await.map_err(|_| "最新版Codex CLIをダウンロードできません")?;
            bounded(response, LIMIT).await?
        }
    };
    let target = format!("package/vendor/{triple}/bin/{filename}");
    let (cached, binary) = tauri::async_runtime::spawn_blocking(move || {
        verified_binary(&bytes, &integrity, &target).map(|binary| (bytes, binary))
    }).await.map_err(|_| "Codex CLIの展開処理に失敗しました")??;
    fs::create_dir_all(&root).map_err(|_| "Codex CLIの保存先を作れません")?;
    let executable = root.join(filename);
    // Always restore the verified binary rather than trusting a cached executable.
    atomic_write(&archive_path, &cached).map_err(|_| "Codex CLIアーカイブを保存できません")?;
    atomic_write(&executable, &binary).map_err(|_| "最新版Codex CLIを保存できません")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .map_err(|_| "Codex CLIの実行権限を設定できません")?;
    }
    Ok(executable)
}

fn valid_version(version: &str) -> bool {
    !version.is_empty() && version.len() <= 80
        && version.bytes().all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "Downloads the latest official Codex CLI into the local cache"]
    async fn latest_cli_download_live_smoke() {
        let cache = dirs::cache_dir().unwrap().join("jp.kinocode.codetas");
        let executable = latest_executable_in(&cache).await.unwrap();
        let output = tokio::process::Command::new(&executable).arg("--version").output().await.unwrap();
        assert!(output.status.success());
        let version = String::from_utf8(output.stdout).unwrap();
        let expected = executable.parent().unwrap().file_name().unwrap().to_string_lossy();
        assert_eq!(version.trim(), format!("codex-cli {expected}"));
        println!("Verified latest official CLI: {}", version.trim());
    }

    #[test]
    fn rejects_unsafe_version_paths() {
        assert!(valid_version("0.160.0"));
        assert!(!valid_version("../escape"));
        assert!(!valid_version(""));
    }

    #[test]
    fn rejects_bad_integrity_before_extracting() {
        assert!(verified_binary(b"not an archive", "sha512-wrong", "codex.exe")
            .unwrap_err().contains("整合性"));
    }

    #[test]
    fn extracts_only_the_expected_binary() {
        let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(gzip);
        let mut header = tar::Header::new_gnu();
        header.set_size(6);
        header.set_mode(0o755);
        header.set_cksum();
        archive.append_data(&mut header, "package/vendor/test/bin/codex", &b"binary"[..]).unwrap();
        let bytes = archive.into_inner().unwrap().finish().unwrap();
        let integrity = format!("sha512-{}", base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&bytes)));
        assert_eq!(verified_binary(&bytes, &integrity, "package/vendor/test/bin/codex").unwrap(), b"binary");
        assert!(verified_binary(&bytes, &integrity, "other").is_err());
    }
}
