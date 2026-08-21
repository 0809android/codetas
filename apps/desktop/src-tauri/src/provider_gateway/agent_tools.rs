use super::*;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::json;
use std::{ffi::OsString, process::Output};
use tokio::io::{AsyncWriteExt, BufWriter};

const MAX_TEST_IMAGE_BYTES: usize = 12 * 1024 * 1024;
const MAX_TEST_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentMediaTestKind {
    Image,
    Video,
    Document,
    ImageGeneration,
}

impl AgentMediaTestKind {
    fn label(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Document => "document",
            Self::ImageGeneration => "imageGeneration",
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMediaTestResult {
    kind: String,
    model: String,
    summary: String,
    preview_data_url: Option<String>,
    source_path: Option<String>,
    duration_ms: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexPluginStatus {
    installed: bool,
    enabled: bool,
    gateway_connected: bool,
    gateway_reachable: bool,
    mcp_healthy: bool,
    health_detail: Option<String>,
    plugin_id: Option<String>,
    plugin_path: Option<String>,
    config_path: String,
}

#[tauri::command]
pub async fn codex_plugin_status(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
) -> Result<CodexPluginStatus, String> {
    let settings = load_settings(&app)?;
    let gateway_connected = codex_gateway_is_configured(&app, &settings)?;
    let config_path = codex_config_path()?;
    if fs::metadata(&config_path).is_ok_and(|metadata| metadata.len() > 4 * 1024 * 1024) {
        return Err("Codex設定が4MiBを超えるためプラグイン状態を確認できません".into());
    }
    let document = match fs::read_to_string(&config_path) {
        Ok(content) => Some(
            content
                .parse::<DocumentMut>()
                .map_err(|error| format!("Codex設定を解析できません: {error}"))?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("Codex設定を確認できません: {error}")),
    };
    let mut plugin_id = None;
    let mut enabled = false;
    if let Some(plugins) = document
        .as_ref()
        .and_then(|document| document.get("plugins"))
        .and_then(Item::as_table_like)
    {
        for (name, item) in plugins.iter() {
            if name == "codetas" || name.starts_with("codetas@") {
                plugin_id = Some(name.to_string());
                enabled = item
                    .as_table_like()
                    .and_then(|table| table.get("enabled"))
                    .and_then(|value| value.as_bool())
                    .unwrap_or(true);
                break;
            }
        }
    }
    let plugin_root = plugin_id
        .as_deref()
        .and_then(find_codetas_plugin_root);
    let (mcp_healthy, mut health_detail) = if enabled {
        match plugin_root.as_deref() {
            Some(root) => match probe_codetas_mcp(root).await {
                Ok(()) => (true, None),
                Err(error) => (false, Some(error)),
            },
            None if plugin_id.is_some() => (false, Some("CodexのプラグインキャッシュにCODETAS本体が見つかりません".into())),
            None => (false, None),
        }
    } else {
        (false, None)
    };
    let gateway_running = runtime_is_running(&app, &manager, &settings).await.unwrap_or(false);
    let gateway_reachable = if enabled && mcp_healthy && gateway_connected && gateway_running {
        let gateway = runtime_gateway_url(&app).unwrap_or_else(|| gateway_url(&settings));
        match probe_media_gateway(&gateway, &settings).await {
            Ok(()) => true,
            Err(error) => {
                health_detail = Some(error);
                false
            }
        }
    } else {
        false
    };
    Ok(CodexPluginStatus {
        installed: plugin_id.is_some() && plugin_root.is_some(),
        enabled,
        gateway_connected,
        gateway_reachable,
        mcp_healthy,
        health_detail,
        plugin_id,
        plugin_path: plugin_root.map(|path| path.to_string_lossy().into_owned()),
        config_path: config_path.to_string_lossy().into_owned(),
    })
}

#[tauri::command]
pub async fn run_agent_media_test(
    app: AppHandle,
    manager: State<'_, GatewayManager>,
    kind: AgentMediaTestKind,
    prompt: Option<String>,
) -> Result<Option<AgentMediaTestResult>, String> {
    let settings = load_settings(&app)?;
    if !runtime_is_running(&app, &manager, &settings).await? {
        return Err("Gatewayを起動してからテストしてください".into());
    }
    let admission_token = media_test_admission_token(&settings)?;
    let selected_model = match kind {
        AgentMediaTestKind::Image => settings.sidecars.vision_model.as_deref(),
        AgentMediaTestKind::Video => settings
            .sidecars
            .video_input_model
            .as_deref()
            .or(settings.sidecars.vision_model.as_deref()),
        AgentMediaTestKind::Document => settings
            .sidecars
            .document_model
            .as_deref()
            .or(settings.sidecars.vision_model.as_deref()),
        AgentMediaTestKind::ImageGeneration => settings.sidecars.image_model.as_deref(),
    }
    .ok_or_else(|| format!("{}用の補助モデルを先に選択してください", kind.label()))?
    .to_string();

    let source = match kind {
        AgentMediaTestKind::Image => pick_test_file(
            "画像認識をテストする画像を選択",
            "Images",
            &["png", "jpg", "jpeg", "webp", "gif"],
        ),
        AgentMediaTestKind::Video => pick_test_file(
            "解析をテストする動画を選択",
            "Videos",
            &["mp4", "mov", "m4v", "webm", "mkv"],
        ),
        AgentMediaTestKind::Document => {
            pick_test_file("PDF・OCRをテストするファイルを選択", "Documents", &["pdf", "png", "jpg", "jpeg", "webp"])
        }
        AgentMediaTestKind::ImageGeneration => None,
    };
    if !matches!(kind, AgentMediaTestKind::ImageGeneration) && source.is_none() {
        return Ok(None);
    }

    let started = Instant::now();
    let user_prompt = prompt
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let (endpoint, payload, source_path) = match kind {
        AgentMediaTestKind::Image => {
            let path = source.as_deref().ok_or("画像が選択されていません")?;
            (
                "/sidecars/vision",
                json!({
                    "image_url": image_data_url(path)?,
                    "prompt": user_prompt.unwrap_or("この画像の内容と見える文字を簡潔に説明してください。"),
                }),
                Some(path.to_string_lossy().into_owned()),
            )
        }
        AgentMediaTestKind::Video => {
            let path = source.as_deref().ok_or("動画が選択されていません")?;
            let frames = video_test_frames(path, settings.agents.video_sample_frames.min(4)).await?;
            (
                "/sidecars/video-analysis",
                json!({
                    "frames": frames,
                    "forceSidecar": true,
                    "prompt": user_prompt.unwrap_or("この動画の流れ、動作、場面変化、見える文字を簡潔に説明してください。"),
                }),
                Some(path.to_string_lossy().into_owned()),
            )
        }
        AgentMediaTestKind::Document => {
            let path = source.as_deref().ok_or("文書が選択されていません")?;
            let pages = document_test_pages(path, settings.agents.document_max_pages.min(3)).await?;
            (
                "/sidecars/document",
                json!({
                    "pages": pages,
                    "forceSidecar": true,
                    "ocr": settings.agents.ocr_enabled,
                    "prompt": user_prompt.unwrap_or("この文書の要点と重要な文字・数値を簡潔に説明してください。"),
                }),
                Some(path.to_string_lossy().into_owned()),
            )
        }
        AgentMediaTestKind::ImageGeneration => (
            "/sidecars/image",
            json!({
                "prompt": user_prompt.unwrap_or("白い背景に紫色の小さな折り紙の鳥。シンプルなプロダクトアイコン。"),
                "size": "1024x1024",
                "quality": "auto",
                "n": 1,
            }),
            None,
        ),
    };
    let gateway = runtime_gateway_url(&app).unwrap_or_else(|| gateway_url(&settings));
    let analysis_items = payload
        .get("frames")
        .or_else(|| payload.get("pages"))
        .and_then(JsonValue::as_array)
        .map(|items| items.len())
        .unwrap_or(1);
    let request_timeout_ms = settings
        .agents
        .auxiliary_timeout_ms
        .saturating_mul(analysis_items as u64)
        .saturating_add(15_000)
        .min(600_000);
    let value = post_sidecar_json(
        &gateway,
        endpoint,
        payload,
        Duration::from_millis(request_timeout_ms),
        admission_token.as_deref(),
    )
    .await?;
    let (summary, preview_data_url) = summarize_test_response(kind, &value);
    Ok(Some(AgentMediaTestResult {
        kind: kind.label().into(),
        model: selected_model,
        summary,
        preview_data_url,
        source_path,
        duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
    }))
}

fn pick_test_file(title: &str, label: &str, extensions: &[&str]) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title(title)
        .add_filter(label, extensions)
        .pick_file()
}

fn image_data_url(path: &Path) -> Result<String, String> {
    let metadata = fs::metadata(path).map_err(|error| format!("テストファイルを確認できません: {error}"))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_TEST_IMAGE_BYTES as u64 {
        return Err("テスト画像は1バイト以上12MiB以下にしてください".into());
    }
    let mime = match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("png") => "image/png",
        _ => return Err("PNG、JPEG、WebP、GIF画像を選択してください".into()),
    };
    let bytes = fs::read(path).map_err(|error| format!("テスト画像を読めません: {error}"))?;
    Ok(format!("data:{mime};base64,{}", STANDARD.encode(bytes)))
}

async fn video_test_frames(path: &Path, requested: u16) -> Result<Vec<String>, String> {
    let ffprobe = find_cli_executable("ffprobe").ok_or("動画テストにはffprobeが必要です")?;
    let ffmpeg = find_cli_executable("ffmpeg").ok_or("動画テストにはffmpegが必要です")?;
    let probe = run_media_command(
        &ffprobe,
        &[
            "-v".into(),
            "error".into(),
            "-show_entries".into(),
            "format=duration".into(),
            "-of".into(),
            "default=noprint_wrappers=1:nokey=1".into(),
            path.as_os_str().to_owned(),
        ],
        Duration::from_secs(30),
    )
    .await?;
    let duration = String::from_utf8_lossy(&probe.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|_| "動画の長さを取得できませんでした")?;
    if !duration.is_finite() || duration <= 0.0 {
        return Err("動画の長さを取得できませんでした".into());
    }
    let temporary = TemporaryMediaDirectory::new()?;
    let count = requested.clamp(1, 4);
    let mut frames = Vec::with_capacity(usize::from(count));
    for index in 0..count {
        let timestamp = duration * (f64::from(index) + 0.5) / f64::from(count);
        let output = temporary.path.join(format!("frame-{:02}.jpg", index + 1));
        run_media_command(
            &ffmpeg,
            &[
                "-v".into(),
                "error".into(),
                "-ss".into(),
                format!("{timestamp:.3}").into(),
                "-i".into(),
                path.as_os_str().to_owned(),
                "-frames:v".into(),
                "1".into(),
                "-q:v".into(),
                "4".into(),
                output.as_os_str().to_owned(),
            ],
            Duration::from_secs(60),
        )
        .await?;
        frames.push(image_data_url(&output)?);
    }
    Ok(frames)
}

async fn document_test_pages(path: &Path, requested: u16) -> Result<Vec<String>, String> {
    if path.extension().and_then(|extension| extension.to_str()).is_some_and(|extension| !extension.eq_ignore_ascii_case("pdf")) {
        return Ok(vec![image_data_url(path)?]);
    }
    let pdftoppm = find_cli_executable("pdftoppm").ok_or("PDFテストにはpdftoppmが必要です")?;
    let temporary = TemporaryMediaDirectory::new()?;
    let prefix = temporary.path.join("page");
    run_media_command(
        &pdftoppm,
        &[
            "-f".into(),
            "1".into(),
            "-l".into(),
            requested.clamp(1, 3).to_string().into(),
            "-jpeg".into(),
            "-r".into(),
            "120".into(),
            path.as_os_str().to_owned(),
            prefix.as_os_str().to_owned(),
        ],
        Duration::from_secs(120),
    )
    .await?;
    let mut pages = fs::read_dir(&temporary.path)
        .map_err(|error| format!("PDFテスト結果を確認できません: {error}"))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("jpg"))
        .collect::<Vec<_>>();
    pages.sort_by_key(|path| {
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.rsplit('-').next())
            .and_then(|page| page.parse::<u32>().ok())
            .unwrap_or(u32::MAX)
    });
    if pages.is_empty() {
        return Err("PDFからテストページを生成できませんでした".into());
    }
    pages.iter().map(|page| image_data_url(page)).collect()
}

async fn run_media_command(program: &Path, args: &[OsString], timeout: Duration) -> Result<Output, String> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| format!("{}が{}秒でタイムアウトしました", program.display(), timeout.as_secs()))?
        .map_err(|error| format!("{}を実行できません: {error}", program.display()))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{}に失敗しました: {}", program.display(), detail.trim()));
    }
    Ok(output)
}

async fn post_sidecar_json(
    gateway_v1: &str,
    endpoint: &str,
    payload: JsonValue,
    timeout: Duration,
    admission_token: Option<&str>,
) -> Result<JsonValue, String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|error| format!("テスト用HTTPクライアントを作成できません: {error}"))?;
    let mut request = client
        .post(format!("{}{}", gateway_v1.trim_end_matches('/'), endpoint))
        .json(&payload);
    if let Some(token) = admission_token {
        request = request.header("x-codetas-token", token);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("Gatewayへテストを送信できません: {error}"))?;
    let status = response.status();
    if response.content_length().is_some_and(|length| length > MAX_TEST_RESPONSE_BYTES as u64) {
        return Err("テスト応答が大きすぎます".into());
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("テスト応答を読めません: {error}"))?;
    if bytes.len() > MAX_TEST_RESPONSE_BYTES {
        return Err("テスト応答が大きすぎます".into());
    }
    if !status.is_success() {
        return Err(format!(
            "GatewayテストがHTTP {}で失敗しました: {}",
            status.as_u16(),
            String::from_utf8_lossy(&bytes).chars().take(2_000).collect::<String>()
        ));
    }
    serde_json::from_slice(&bytes).map_err(|error| format!("テスト応答がJSONではありません: {error}"))
}

fn media_test_admission_token(settings: &GatewaySettings) -> Result<Option<String>, String> {
    if settings.security.require_local_token {
        if let Ok(token) = env::var("CODETAS_GATEWAY_TOKEN") {
            if !token.is_empty() {
                return Ok(Some(token));
            }
        }
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let enabled_external = settings
        .security
        .external_access_keys
        .iter()
        .filter(|key| key.enabled)
        .collect::<Vec<_>>();
    for key in &enabled_external {
        if key.expires_at_unix.is_some_and(|expires| expires <= now)
            || !key
                .scopes
                .iter()
                .any(|scope| scope == "gateway:*" || scope == "sidecars:write")
        {
            continue;
        }
        if let Ok(token) = env::var(&key.env_var) {
            if !token.is_empty() {
                return Ok(Some(token));
            }
        }
    }
    if settings.security.require_local_token || !enabled_external.is_empty() {
        return Err(
            "メディアテストに使えるGatewayトークンがありません。CODETAS_GATEWAY_TOKEN、またはsidecars:write権限を持つ外部アクセスキーを設定してください"
                .into(),
        );
    }
    Ok(None)
}

fn find_codetas_plugin_root(plugin_id: &str) -> Option<PathBuf> {
    let (plugin_name, marketplace) = plugin_id.split_once('@').unwrap_or((plugin_id, ""));
    if plugin_name != "codetas"
        || marketplace
            .chars()
            .any(|character| !(character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')))
    {
        return None;
    }
    if let Ok(root) = env::var("CODETAS_PLUGIN_ROOT") {
        let root = PathBuf::from(root);
        if codetas_plugin_manifest_is_valid(&root) {
            return Some(root);
        }
    }
    let cache = codex_home().ok()?.join("plugins").join("cache");
    let mut bases = Vec::new();
    if !marketplace.is_empty() {
        bases.push(cache.join(marketplace).join(plugin_name));
    } else {
        let sources = fs::read_dir(&cache).ok()?;
        for source in sources.filter_map(Result::ok).take(128) {
            if source.file_type().ok().is_some_and(|kind| kind.is_dir()) {
                bases.push(source.path().join(plugin_name));
            }
        }
    }
    for base in bases {
        if codetas_plugin_manifest_is_valid(&base) {
            return Some(base);
        }
        let mut versions = fs::read_dir(&base)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().ok().is_some_and(|kind| kind.is_dir()))
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        versions.sort();
        if let Some(root) = versions
            .into_iter()
            .rev()
            .find(|path| codetas_plugin_manifest_is_valid(path))
        {
            return Some(root);
        }
    }
    None
}

fn codetas_plugin_manifest_is_valid(root: &Path) -> bool {
    let manifest = root.join(".codex-plugin").join("plugin.json");
    if !manifest.is_file() {
        return false;
    }
    let Ok(content) = fs::read_to_string(manifest) else {
        return false;
    };
    serde_json::from_str::<JsonValue>(&content)
        .ok()
        .and_then(|value| value.get("name").and_then(JsonValue::as_str).map(str::to_string))
        .as_deref()
        == Some("codetas")
}

async fn probe_media_gateway(gateway_v1: &str, settings: &GatewaySettings) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|error| format!("Gateway疎通確認を準備できません: {error}"))?;
    let mut request = client.get(format!(
        "{}/sidecars/config",
        gateway_v1.trim_end_matches('/')
    ));
    if let Some(token) = media_test_admission_token(settings)? {
        request = request.header("x-codetas-token", token);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("CODETAS Gatewayへ接続できません: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "CODETAS GatewayのメディアAPIがHTTP {}を返しました",
            response.status().as_u16()
        ));
    }
    Ok(())
}

async fn probe_codetas_mcp(root: &Path) -> Result<(), String> {
    for relative in [
        ".mcp.json",
        "scripts/mcp_server.py",
        "scripts/media_tools.py",
        "skills/codetas-media/SKILL.md",
    ] {
        if !root.join(relative).is_file() {
            return Err(format!("プラグインファイルが不足しています: {relative}"));
        }
    }
    let config_path = root.join(".mcp.json");
    let metadata = fs::metadata(&config_path)
        .map_err(|error| format!("MCP設定を確認できません: {error}"))?;
    if metadata.len() > 1024 * 1024 {
        return Err("MCP設定が1MiBを超えています".into());
    }
    let config: JsonValue = serde_json::from_slice(
        &fs::read(&config_path).map_err(|error| format!("MCP設定を読めません: {error}"))?,
    )
    .map_err(|error| format!("MCP設定を解析できません: {error}"))?;
    let definition = config
        .get("mcpServers")
        .and_then(|servers| servers.get("codetas-project"))
        .and_then(JsonValue::as_object)
        .ok_or("MCP設定にcodetas-projectサーバーがありません")?;
    let command_name = definition
        .get("command")
        .and_then(JsonValue::as_str)
        .filter(|command| !command.trim().is_empty())
        .ok_or("MCPサーバーのcommandがありません")?;
    let cwd = definition
        .get("cwd")
        .and_then(JsonValue::as_str)
        .map(|cwd| root.join(cwd))
        .unwrap_or_else(|| root.to_path_buf());
    let root = root
        .canonicalize()
        .map_err(|error| format!("プラグインフォルダを解決できません: {error}"))?;
    let cwd = cwd
        .canonicalize()
        .map_err(|error| format!("MCP作業フォルダを解決できません: {error}"))?;
    if !cwd.starts_with(&root) {
        return Err("MCP作業フォルダがプラグイン外を参照しています".into());
    }
    let command_path = Path::new(command_name);
    let program = if command_path.is_absolute() || command_name.contains('/') || command_name.contains('\\') {
        let path = if command_path.is_absolute() {
            command_path.to_path_buf()
        } else {
            cwd.join(command_path)
        };
        if !path.is_file() {
            return Err(format!("MCPサーバーcommandが見つかりません: {}", path.display()));
        }
        path
    } else {
        find_cli_executable(command_name)
            .ok_or_else(|| format!("MCPサーバーcommandがPATHにありません: {command_name}"))?
    };
    let args = definition
        .get("args")
        .and_then(JsonValue::as_array)
        .map(|args| {
            args.iter()
                .map(|arg| {
                    arg.as_str()
                        .map(OsString::from)
                        .ok_or_else(|| "MCPサーバーargsは文字列で指定してください".to_string())
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(environment) = definition.get("env").and_then(JsonValue::as_object) {
        for (name, value) in environment {
            let value = value
                .as_str()
                .ok_or("MCPサーバーenvは文字列で指定してください")?;
            command.env(name, value);
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("MCPサーバーを起動できません: {error}"))?;
    let stdin = child.stdin.take().ok_or("MCPサーバーstdinを開けません")?;
    let mut stdin = BufWriter::new(stdin);
    let handshake = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"codetas-health\",\"version\":\"0.1.0\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":{}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}\n"
    );
    stdin
        .write_all(handshake.as_bytes())
        .await
        .map_err(|error| format!("MCPハンドシェイクを送れません: {error}"))?;
    stdin
        .shutdown()
        .await
        .map_err(|error| format!("MCPサーバーstdinを閉じられません: {error}"))?;
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(8), child.wait_with_output())
        .await
        .map_err(|_| "MCPハンドシェイクが8秒でタイムアウトしました".to_string())?
        .map_err(|error| format!("MCPサーバーの終了を待てません: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "MCPサーバーが失敗しました: {}",
            String::from_utf8_lossy(&output.stderr).chars().take(2_000).collect::<String>()
        ));
    }
    let responses = String::from_utf8(output.stdout)
        .map_err(|_| "MCP応答がUTF-8ではありません".to_string())?;
    let tools = responses
        .lines()
        .filter_map(|line| serde_json::from_str::<JsonValue>(line).ok())
        .find(|value| value.get("id").and_then(JsonValue::as_i64) == Some(2))
        .and_then(|value| value.get("result").cloned())
        .and_then(|result| result.get("tools").cloned())
        .and_then(|tools| tools.as_array().cloned())
        .ok_or("MCP tools/list応答を取得できません")?;
    let healthy = [
        "vision_analyze",
        "video_analyze",
        "document_analyze",
        "image_generate",
    ]
        .iter()
        .all(|required| {
            tools.iter().any(|tool| {
                tool.get("name").and_then(JsonValue::as_str) == Some(*required)
            })
        });
    if !healthy {
        return Err("MCPサーバーが必要なメディアツールを公開していません".into());
    }
    Ok(())
}

fn summarize_test_response(kind: AgentMediaTestKind, value: &JsonValue) -> (String, Option<String>) {
    if !matches!(kind, AgentMediaTestKind::ImageGeneration) {
        let result = value
            .get("result")
            .and_then(JsonValue::as_str)
            .unwrap_or("補助モデルから応答を受信しました");
        return (result.chars().take(2_000).collect(), None);
    }
    let first = value
        .get("data")
        .and_then(JsonValue::as_array)
        .and_then(|items| items.first())
        .and_then(JsonValue::as_object);
    let preview = first
        .and_then(|item| item.get("b64_json"))
        .and_then(JsonValue::as_str)
        .filter(|data| data.len() <= 12 * 1024 * 1024)
        .map(|data| format!("data:image/png;base64,{data}"));
    let summary = first
        .and_then(|item| item.get("revised_prompt"))
        .and_then(JsonValue::as_str)
        .map(|prompt| format!("画像を生成しました: {prompt}"))
        .unwrap_or_else(|| "画像生成モデルから応答を受信しました".into());
    (summary, preview)
}

struct TemporaryMediaDirectory {
    path: PathBuf,
}

impl TemporaryMediaDirectory {
    fn new() -> Result<Self, String> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "一時フォルダIDを作成できません")?
            .as_nanos();
        let path = env::temp_dir().join(format!("codetas-media-test-{}-{unique}", std::process::id()));
        fs::create_dir(&path).map_err(|error| format!("一時フォルダを作成できません: {error}"))?;
        Ok(Self { path })
    }
}

impl Drop for TemporaryMediaDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
