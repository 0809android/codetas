use super::*;
use sha2::{Digest, Sha256};
use std::time::Instant;

struct CachedAntigravityProject {
    token_fingerprint: String,
    project: String,
    expires_at: Instant,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AntigravityProjectError {
    #[error("{message}")]
    Authentication { status: Option<u16>, message: String },
    #[error("{0}")]
    Request(String),
    #[error("Antigravity Cloud Code Assist returned HTTP {status}: {message}")]
    Status { status: u16, message: String },
    #[error("{message}")]
    InvalidResponse { status: u16, message: String },
}

static ANTIGRAVITY_PROJECT_CACHE: OnceLock<tokio::sync::Mutex<Option<CachedAntigravityProject>>> =
    OnceLock::new();

pub(crate) fn session_needs_refresh(session: &OAuthSession) -> bool {
    session.access.trim().is_empty()
        || session.expires_at_ms <= 0
        || session.expires_at_ms <= now_ms() + REFRESH_SKEW_MS
}

pub(crate) async fn refresh_session(
    provider_id: &str,
    session: &OAuthSession,
) -> Result<OAuthSession, String> {
    match provider_id {
        "kimi" => refresh_kimi_token(&session.refresh).await,
        "anthropic" => refresh_anthropic_token(&session.refresh).await,
        "xai" => refresh_xai_token(&session.refresh).await,
        "google-antigravity" => refresh_antigravity_cli_session().await,
        _ => Err(format!("{provider_id} の自動更新に未対応です")),
    }
}

pub(crate) async fn refresh_antigravity_cli_session() -> Result<OAuthSession, String> {
    let executable = find_antigravity_cli_executable()
        .ok_or_else(|| "Antigravity CLI (agy) が見つかりません".to_string())?;
    let mut process = tokio::process::Command::new(executable);
    process
        .arg("models")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(Duration::from_secs(30), process.status())
        .await
        .map_err(|_| "Antigravity CLI の認証更新がタイムアウトしました".to_string())?
        .map_err(|_| "Antigravity CLI の認証更新を開始できませんでした".to_string())?;
    if !status.success() {
        return Err("Antigravity CLI の認証更新に失敗しました。agy で再ログインしてください".into());
    }
    let session = detect_antigravity_cli_session(&user_home())
        .ok_or_else(|| "Antigravity CLI の更新済み認証を読み取れませんでした".to_string())?;
    if session_needs_refresh(&session) {
        return Err("Antigravity CLI が有効なアクセストークンを返しませんでした".into());
    }
    Ok(session)
}

pub(crate) async fn resolve_antigravity_cloud_project(
    access_token: &str,
) -> Result<String, String> {
    let token_fingerprint = format!("{:x}", Sha256::digest(access_token.as_bytes()));
    let cache = ANTIGRAVITY_PROJECT_CACHE.get_or_init(|| tokio::sync::Mutex::new(None));
    let mut cached = cache.lock().await;
    if let Some(entry) = cached.as_ref() {
        if entry.token_fingerprint == token_fingerprint && entry.expires_at > Instant::now() {
            return Ok(entry.project.clone());
        }
    }
    let project = fetch_antigravity_cloud_project(access_token)
        .await
        .map_err(|error| error.to_string())?;
    *cached = Some(CachedAntigravityProject {
        token_fingerprint,
        project: project.clone(),
        expires_at: Instant::now() + Duration::from_secs(30 * 60),
    });
    Ok(project)
}

pub(crate) async fn probe_antigravity_cloud_project(
    access_token: &str,
) -> Result<String, AntigravityProjectError> {
    fetch_antigravity_cloud_project(access_token).await
}

async fn fetch_antigravity_cloud_project(
    access_token: &str,
) -> Result<String, AntigravityProjectError> {
    let response = oauth_http_client()
        .map_err(AntigravityProjectError::Request)?
        .post("https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist")
        .bearer_auth(access_token)
        .header(reqwest::header::USER_AGENT, "antigravity")
        .json(&serde_json::json!({
            "metadata": { "ideType": "ANTIGRAVITY" }
        }))
        .send()
        .await
        .map_err(|_| {
            AntigravityProjectError::Request(
                "Antigravity のCloud Code Assistプロジェクトを取得できませんでした".into(),
            )
        })?;
    let status = response.status().as_u16();
    if !response.status().is_success() {
        let message = "Antigravity のCloud Code Assistプロジェクト取得が拒否されました"
            .to_string();
        return Err(if matches!(status, 401 | 403) {
            AntigravityProjectError::Authentication {
                status: Some(status),
                message,
            }
        } else {
            AntigravityProjectError::Status { status, message }
        });
    }
    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|_| {
            AntigravityProjectError::InvalidResponse {
                status,
                message: "Antigravity のCloud Code Assist応答を解析できませんでした".into(),
            }
        })?;
    let project = json_string(
        &value,
        &["cloudaicompanionProject", "cloudAiCompanionProject"],
    )
    .filter(|value| is_valid_google_project_id(value))
    .ok_or_else(|| {
        AntigravityProjectError::InvalidResponse {
            status,
            message: "Antigravity のCloud Code Assistプロジェクトが応答にありません".into(),
        }
    })?;
    Ok(project)
}

fn find_antigravity_cli_executable() -> Option<PathBuf> {
    let executable_name = if cfg!(windows) { "agy.exe" } else { "agy" };
    let mut directories = env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    if let Some(home) = dirs::home_dir() {
        directories.push(home.join(".local/bin"));
    }
    directories.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
    ]);
    directories.into_iter().find_map(|directory| {
        let candidate = directory.join(executable_name);
        candidate
            .is_file()
            .then(|| fs::canonicalize(candidate).ok())
            .flatten()
    })
}
