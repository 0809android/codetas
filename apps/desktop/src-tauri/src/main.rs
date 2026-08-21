#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod clients;
mod codex_app_server;
mod hermes_sync;
mod maintenance;
mod maintenance_jobs;
mod maintenance_process;
mod mcp;
mod provider_gateway;
mod service;

use serde::{Deserialize, Serialize};
use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
};
use tauri::Manager;

fn focus_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

const MAX_SKILL_SCAN_DEPTH: usize = 4;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectInspection {
    id: String,
    name: String,
    path: String,
    context_file: Option<String>,
    agents_file: Option<String>,
    skills_directory: Option<String>,
    skills_count: usize,
    mcp_file: Option<String>,
    codex_config_file: Option<String>,
    warnings: Vec<String>,
    inspected_at: String,
}

#[tauri::command]
fn pick_project() -> Option<String> {
    rfd::FileDialog::new()
        .set_title("CODETASに追加するプロジェクトを選択")
        .pick_folder()
        .map(|path| path.to_string_lossy().into_owned())
}

#[tauri::command]
fn inspect_project(path: String) -> Result<ProjectInspection, String> {
    let root =
        fs::canonicalize(&path).map_err(|error| format!("プロジェクトを開けません: {error}"))?;

    if !root.is_dir() {
        return Err("選択したパスはフォルダではありません。".into());
    }

    let context_file = first_file(&root, &[".hermes.md", "HERMES.md"]);
    let agents_file = first_file(&root, &["AGENTS.md"]);
    let skills_directory = first_directory(&root, &[".hermes/skills", "skills", ".agents/skills"]);
    let mcp_file = first_file(&root, &[".hermes/mcp.json", "mcp.json", ".mcp.json"]);
    let codex_config_file = first_file(&root, &[".codex/config.toml"]);
    let skills_count = skills_directory
        .as_deref()
        .map(|directory| count_skill_files(directory, 0))
        .unwrap_or(0);

    let mut warnings = Vec::new();
    if context_file.is_none() && agents_file.is_none() {
        warnings.push("HermesまたはCodexのプロジェクト指示が見つかりません。".into());
    }
    if mcp_file.is_some() {
        warnings.push("MCP設定は適用前に変換内容の確認が必要です。".into());
    }
    if root.join(".env").exists() || root.join("auth.json").exists() {
        warnings.push("資格情報ファイルは検出対象から除外されています。".into());
    }

    let canonical = root.to_string_lossy().into_owned();
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);

    Ok(ProjectInspection {
        id: format!("project-{:x}", hasher.finish()),
        name: root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("project")
            .to_string(),
        path: canonical,
        context_file: display_path(context_file),
        agents_file: display_path(agents_file),
        skills_directory: display_path(skills_directory),
        skills_count,
        mcp_file: display_path(mcp_file),
        codex_config_file: display_path(codex_config_file),
        warnings,
        inspected_at: unix_timestamp_iso(),
    })
}

fn first_file(root: &Path, candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(|candidate| root.join(candidate))
        .find_map(|candidate| {
            let metadata = fs::symlink_metadata(&candidate).ok()?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return None;
            }
            let canonical = fs::canonicalize(candidate).ok()?;
            canonical.starts_with(root).then_some(canonical)
        })
}

fn first_directory(root: &Path, candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(|candidate| root.join(candidate))
        .find_map(|candidate| {
            let metadata = fs::symlink_metadata(&candidate).ok()?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return None;
            }
            let canonical = fs::canonicalize(candidate).ok()?;
            canonical.starts_with(root).then_some(canonical)
        })
}

fn display_path(path: Option<PathBuf>) -> Option<String> {
    path.map(|value| value.to_string_lossy().into_owned())
}

fn count_skill_files(directory: &Path, depth: usize) -> usize {
    if depth > MAX_SKILL_SCAN_DEPTH {
        return 0;
    }

    let Ok(entries) = fs::read_dir(directory) else {
        return 0;
    };

    entries
        .filter_map(Result::ok)
        .map(|entry| (entry.path(), entry.file_type().ok()))
        .map(|(path, file_type)| {
            let Some(file_type) = file_type else { return 0 };
            if file_type.is_symlink() {
                0
            } else if file_type.is_dir() {
                count_skill_files(&path, depth + 1)
            } else if file_type.is_file()
                && path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md")
            {
                1
            } else {
                0
            }
        })
        .sum()
}

fn unix_timestamp_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    // The UI only requires a parseable instant. Avoid a time/date dependency in the shell.
    seconds.saturating_mul(1000).to_string()
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HermesProfile {
    name: String,
    display_name: Option<String>,
    description: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HermesProfileConvertReport {
    created: Vec<String>,
    skipped: Vec<String>,
}

const CODETAS_AGENT_MARKER: &str = "# Generated by CODETAS";
const HERMES_PROFILE_DIRECTORY: &str = "profiles";
const MEMORY_CHAR_LIMIT: usize = 2_200;
const USER_CHAR_LIMIT: usize = 1_375;
const SOUL_CHAR_LIMIT: usize = 12_000;

fn hermes_profiles_dir() -> PathBuf {
    dirs::home_dir()
        .map(|home| home.join(".hermes").join(HERMES_PROFILE_DIRECTORY))
        .unwrap_or_else(|| PathBuf::from(".hermes").join(HERMES_PROFILE_DIRECTORY))
}

fn codex_agents_dir() -> PathBuf {
    dirs::home_dir()
        .map(|home| home.join(".codex").join("agents"))
        .unwrap_or_else(|| PathBuf::from(".codex").join("agents"))
}

/// Minimal parser for the flat `key: value` profile.yaml Hermes writes.
/// Handles quoted values, `\uXXXX` escapes, and multi-line quoted scalars
/// (including `\` newline folding) without a YAML dependency.
fn parse_flat_yaml(raw: &str) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut current: Option<usize> = None;
    for raw_line in raw.lines() {
        let line = raw_line.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            if let Some((key, value)) = line.split_once(':') {
                fields.push((key.trim().to_string(), value.trim().to_string()));
                current = Some(fields.len() - 1);
                continue;
            }
            current = None;
            continue;
        }
        if let Some(index) = current {
            let continuation = line.trim();
            let existing = &mut fields[index].1;
            if existing.ends_with('\\') {
                existing.pop();
                existing.push_str(continuation);
            } else {
                if !existing.is_empty() {
                    existing.push(' ');
                }
                existing.push_str(continuation);
            }
        }
    }
    fields
        .into_iter()
        .map(|(key, value)| (key, unescape_yaml_scalar(&value)))
        .collect()
}

fn unescape_yaml_scalar(value: &str) -> String {
    let mut stripped = value.trim();
    let bytes = stripped.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            stripped = &stripped[1..stripped.len() - 1];
        }
    }
    let bytes = stripped.as_bytes();
    let mut result = String::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 1 < bytes.len() {
            let next = bytes[index + 1];
            if next == b'u' && index + 5 < bytes.len() {
                let hex = std::str::from_utf8(&bytes[index + 2..index + 6]).unwrap_or("");
                if let Ok(code) = u32::from_str_radix(hex, 16) {
                    if let Some(ch) = char::from_u32(code) {
                        result.push(ch);
                        index += 6;
                        continue;
                    }
                }
            }
            let escaped = match next {
                b'n' => Some('\n'),
                b't' => Some('\t'),
                b'r' => Some('\r'),
                b'"' => Some('"'),
                b'\'' => Some('\''),
                b'\\' => Some('\\'),
                _ => None,
            };
            if let Some(ch) = escaped {
                result.push(ch);
                index += 2;
                continue;
            }
        }
        let ch = stripped[index..].chars().next().unwrap_or_default();
        result.push(ch);
        index += ch.len_utf8();
    }
    result
}

fn read_hermes_profile(name: &str) -> Option<(Option<String>, String)> {
    let path = hermes_profile_root(name).join("profile.yaml");
    let raw = fs::read_to_string(&path).ok()?;
    let fields = parse_flat_yaml(&raw);
    let get = |key: &str| {
        fields
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value)
    };
    let description = get("description")?.trim().to_string();
    if description.is_empty() {
        return None;
    }
    let display_name = get("display_name")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    Some((display_name, description))
}

fn hermes_profile_root(name: &str) -> PathBuf {
    if name == "default" {
        dirs::home_dir()
            .map(|home| home.join(".hermes"))
            .unwrap_or_else(|| PathBuf::from(".hermes"))
    } else {
        hermes_profiles_dir().join(name)
    }
}

fn read_regular_text_capped(path: &Path, max_chars: usize) -> Option<String> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    let raw = fs::read_to_string(path).ok()?;
    let mut chars = raw.chars();
    let mut text: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        text.push_str("\n…");
    }
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

fn self_learning_instructions(profile_name: &str) -> String {
    let memory_dir = if profile_name == "default" {
        "~/.hermes/memories/".to_string()
    } else {
        format!("~/.hermes/profiles/{profile_name}/memories/")
    };
    let skills_dir = if profile_name == "default" {
        "~/.hermes/skills/user/".to_string()
    } else {
        format!("~/.hermes/profiles/{profile_name}/skills/user/")
    };
    format!(
        "## 自己学習（Hermes 互換ループ）\n\
CODETAS プラグインが閉じた学習ループを動かす。MEMORY.md / USER.md 本文はこの toml に埋め込まない。SessionStart の凍結スナップショットだけが注入される。\n\
\n\
ループ:\n\
- SessionStart: MEMORY.md / USER.md と skills/user 索引を凍結注入する。compact では同じスナップショットを再利用する\n\
- ユーザーターン {memory_nudge} 回ごと、および {flush} ターン目のチェックポイント: Stop continuation で memory レビュー\n\
- 観測できたツール完了 {skill_nudge}、またはユーザーターン {skill_nudge}: Stop continuation で skill_manage レビュー\n\
- Codex には Hermes の裏 fork と終了直前ターンがない。SessionEnd は保存できない。明確な永続事実は nudge を待たず保存してよい\n\
\n\
ツール:\n\
- 毎回 SessionStart の `scopeToken` を付ける。`profileName` は表示用で書き込み先を変えられない\n\
- `memory` — target=memory|user, action=add|replace|remove。上限 memory {memory_limit} 文字 / user {user_limit} 文字。§ 区切り。溢れたら統合してから再試行\n\
- `skill_manage` — action=view|list|create|edit|patch|write_file。delete は使わない。書き込み先は `{skills_dir}` のみ\n\
\n\
対象ファイル:\n\
- `{memory_dir}MEMORY.md`\n\
- `{memory_dir}USER.md`\n\
- `{skills_dir}<skill-name>/SKILL.md`\n\
\n\
bundled / hub / 外部所有スキルは編集しない。秘密、資格情報、会話全文、一時失敗、未検証のデッドエンドは残さない。明示的な利用者指示、AGENTS.md、より優先度の高い指示が衝突したらそちらを勝たせる。プラグイン未承認ならこのループは動かない。\n",
        memory_dir = memory_dir,
        skills_dir = skills_dir,
        memory_nudge = 10,
        skill_nudge = 15,
        flush = 6,
        memory_limit = MEMORY_CHAR_LIMIT,
        user_limit = USER_CHAR_LIMIT,
    )
}

fn build_codex_agent_instructions_from(
    root: &Path,
    profile_name: &str,
    description: &str,
) -> String {
    let soul = read_regular_text_capped(&root.join("SOUL.md"), SOUL_CHAR_LIMIT);
    let identity = soul.unwrap_or_else(|| description.trim().to_string());
    format!(
        "{identity}\n\
\n\
## 担当の要約\n\
{description}\n\
\n\
{learning}",
        identity = identity.trim(),
        description = description.trim(),
        learning = self_learning_instructions(profile_name).trim_end(),
    )
}

fn build_codex_agent_instructions(profile_name: &str, description: &str) -> String {
    build_codex_agent_instructions_from(
        &hermes_profile_root(profile_name),
        profile_name,
        description,
    )
}

#[tauri::command]
fn list_hermes_profiles() -> Vec<HermesProfile> {
    let mut profiles = Vec::new();
    let default_root = hermes_profile_root("default");
    if default_root.join("profile.yaml").is_file() || default_root.join("SOUL.md").is_file() {
        let description = read_hermes_profile("default")
            .map(|(_, description)| description)
            .unwrap_or_else(|| "Default Hermes profile".into());
        profiles.push(HermesProfile {
            name: "default".into(),
            display_name: Some("default".into()),
            description,
        });
    }
    let Ok(entries) = fs::read_dir(hermes_profiles_dir()) else {
        return profiles;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "default" {
            continue;
        }
        let Some((display_name, description)) = read_hermes_profile(&name) else {
            continue;
        };
        profiles.push(HermesProfile {
            name,
            display_name,
            description,
        });
    }
    profiles.sort_by(|left, right| left.name.cmp(&right.name));
    profiles
}

fn compact_description(description: &str) -> String {
    let single_line = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut result: String = single_line.chars().take(160).collect();
    if single_line.len() > 160 {
        result.push('…');
    }
    result
}

fn render_codex_agent(name: &str, description: &str, instructions: &str) -> String {
    let escape_quoted = |value: &str| value.replace('\\', "\\\\").replace('"', "\\\"");
    let multiline = instructions.replace("\"\"\"", "\"\"\\\"");
    format!(
        "{marker}\nname = \"{name}\"\ndescription = \"{description}\"\ndeveloper_instructions = \"\"\"\n{instructions}\n\"\"\"\n",
        marker = CODETAS_AGENT_MARKER,
        name = escape_quoted(name),
        description = escape_quoted(description),
        instructions = multiline,
    )
}

fn atomic_write_text(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("フォルダを作れません: {error}"))?;
    }
    let temp = path.with_extension("tmp");
    fs::write(&temp, content).map_err(|error| format!("書き込めません: {error}"))?;
    fs::rename(&temp, path).map_err(|error| format!("保存できません: {error}"))
}

fn record_converted_profile(name: &str, kind: &str) -> Result<(), String> {
    let home = dirs::home_dir().ok_or_else(|| "ホームディレクトリを特定できません".to_string())?;
    let path = home.join(".codex").join("codetas-learning").join("agent-map.json");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("フォルダを作れません: {error}"))?;
    }
    let mut map = serde_json::Map::new();
    if path.exists() {
        if let Ok(raw) = fs::read_to_string(&path) {
            if let Ok(serde_json::Value::Object(existing)) = serde_json::from_str(&raw) {
                map = existing;
            }
        }
    }
    let mut entry = serde_json::Map::new();
    entry.insert("kind".into(), serde_json::Value::String(kind.into()));
    entry.insert(
        "name".into(),
        serde_json::Value::String(if kind == "default" {
            "default".into()
        } else {
            name.into()
        }),
    );
    map.insert(name.into(), serde_json::Value::Object(entry));
    atomic_write_text(&path, &serde_json::Value::Object(map).to_string())
}

#[tauri::command]
fn convert_hermes_profiles(
    profile_names: Vec<String>,
) -> Result<HermesProfileConvertReport, String> {
    let mut created = Vec::new();
    let mut skipped = Vec::new();
    let agents_dir = codex_agents_dir();
    fs::create_dir_all(&agents_dir)
        .map_err(|error| format!("Codex agentsフォルダを作れません: {error}"))?;
    for requested in profile_names {
        if requested.is_empty()
            || requested.contains('/')
            || requested.contains('\\')
            || requested == "."
            || requested == ".."
        {
            skipped.push(format!("{requested}: 不正な名前"));
            continue;
        }
        let (display_name, description) = match read_hermes_profile(&requested) {
            Some(value) => value,
            None if requested == "default"
                && (hermes_profile_root("default").join("SOUL.md").is_file()
                    || hermes_profile_root("default").join("profile.yaml").is_file()) =>
            {
                (Some("default".into()), "Default Hermes profile".into())
            }
            None => {
                skipped.push(format!("{requested}: プロファイルが見つかりません"));
                continue;
            }
        };
        let target = agents_dir.join(format!("{requested}.toml"));
        if target.exists() {
            let existing = fs::read_to_string(&target).unwrap_or_default();
            if !existing.contains(CODETAS_AGENT_MARKER) {
                skipped.push(format!(
                    "{requested}: Codexプロファイルが既に存在します（上書きしません）"
                ));
                continue;
            }
        }
        let routing_description = display_name.unwrap_or_else(|| compact_description(&description));
        let instructions = build_codex_agent_instructions(&requested, &description);
        let content = render_codex_agent(&requested, &routing_description, &instructions);
        atomic_write_text(&target, &content)?;
        let kind = if requested == "default" { "default" } else { "named" };
        record_converted_profile(&requested, kind)?;
        created.push(requested);
    }
    Ok(HermesProfileConvertReport { created, skipped })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_yaml_with_quotes_and_unicode_escapes() {
        let raw = "description: コード差分を検査する\n\
                   display_name: \"\\u30CD\\u30A4\\u30C6\\u30A3\\u30AA\"\n\
                   description_auto: false\n";
        let fields = parse_flat_yaml(raw);
        assert_eq!(
            fields
                .iter()
                .find(|(key, _)| key == "description")
                .map(|(_, value)| value.as_str()),
            Some("コード差分を検査する")
        );
        assert_eq!(
            fields
                .iter()
                .find(|(key, _)| key == "display_name")
                .map(|(_, value)| value.as_str()),
            Some("ネイティオ")
        );
        assert_eq!(
            fields
                .iter()
                .find(|(key, _)| key == "description_auto")
                .map(|(_, value)| value.as_str()),
            Some("false")
        );
    }

    #[test]
    fn folds_multiline_quoted_scalars() {
        let raw = "description: \"\\u4E2D\\u56FD\\u5927\\u9646\\\n  SNS\\u3001\\u82F1\\u8A9E\"\n\
                   description_auto: false\n";
        let fields = parse_flat_yaml(raw);
        let description = fields
            .iter()
            .find(|(key, _)| key == "description")
            .map(|(_, value)| value.as_str());
        assert_eq!(description, Some("中国大陆SNS、英語"));
        assert_eq!(
            fields
                .iter()
                .find(|(key, _)| key == "description_auto")
                .map(|(_, value)| value.as_str()),
            Some("false")
        );
    }

    #[test]
    fn renders_valid_codex_agent_toml() {
        let content = render_codex_agent(
            "scyther",
            "コード差分を検査する専任レビュアー",
            "コード差分を読み取り専用で検査する。\nセキュリティを優先する。",
        );
        assert!(content.starts_with("# Generated by CODETAS\n"));
        assert!(content.contains("name = \"scyther\"\n"));
        assert!(content.contains("description = \"コード差分を検査する専任レビュアー\"\n"));
        assert!(content.contains("developer_instructions = \"\"\"\n"));
        assert!(content.ends_with("\"\"\"\n"));
    }

    #[test]
    fn agent_instructions_include_soul_memory_and_self_learning() {
        let root = std::env::temp_dir().join(format!(
            "codetas-profile-learn-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("memories")).expect("temp profile dir");
        fs::write(root.join("SOUL.md"), "あなたは検査担当です。").expect("soul");
        fs::write(
            root.join("memories").join("MEMORY.md"),
            "この作業は読み取り専用。",
        )
        .expect("memory");
        fs::write(
            root.join("memories").join("USER.md"),
            "利用者は短い報告を好む。",
        )
        .expect("user");
        let instructions =
            build_codex_agent_instructions_from(&root, "scyther", "コード差分を検査する。");
        assert!(instructions.contains("あなたは検査担当です。"));
        assert!(!instructions.contains("この作業は読み取り専用。"));
        assert!(!instructions.contains("利用者は短い報告を好む。"));
        assert!(instructions.contains("## 自己学習（Hermes 互換ループ）"));
        assert!(instructions.contains("~/.hermes/profiles/scyther/memories/MEMORY.md"));
        assert!(instructions.contains("skills/user/"));
        assert!(instructions.contains("SessionStart"));
        assert!(instructions.contains("scopeToken"));
        assert!(instructions.contains("~/.hermes/profiles/scyther/memories/"));
        let converted = render_codex_agent("scyther", "検査担当", &instructions);
        assert!(converted.contains("自己学習（Hermes 互換ループ）"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn agent_instructions_fall_back_to_description_without_soul() {
        let root = std::env::temp_dir().join(format!(
            "codetas-profile-learn-empty-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp profile dir");
        let instructions =
            build_codex_agent_instructions_from(&root, "oddish", "収益化分析を担当する。");
        assert!(instructions.contains("収益化分析を担当する。"));
        assert!(!instructions.contains("（まだエントリなし）"));
        assert!(instructions.contains("## 自己学習（Hermes 互換ループ）"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn capped_profile_text_ignores_symlinks_and_empty_files() {
        let root = std::env::temp_dir().join(format!(
            "codetas-profile-learn-link-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp profile dir");
        fs::write(root.join("USER.md"), "   \n").expect("empty user");
        assert!(read_regular_text_capped(&root.join("USER.md"), 100).is_none());
        let target = root.join("outside.md");
        fs::write(&target, "outside secret").expect("outside");
        let link = root.join("SOUL.md");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &link).expect("symlink");
            assert!(read_regular_text_capped(&link, 100).is_none());
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn compacts_long_descriptions() {
        let long = format!("word{}", "x".repeat(300));
        let compact = compact_description(&long);
        assert!(compact.chars().count() <= 161);
        assert!(compact.ends_with('…'));
    }
}

fn main() {
    match early_runtime_command() {
        Ok(Some(EarlyRuntimeCommand::GatewayService {
            settings,
            observability,
        })) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap_or_else(|error| {
                    eprintln!("CODETAS Gateway: runtime initialization failed: {error}");
                    std::process::exit(1);
                });
            if let Err(error) =
                runtime.block_on(service::run_gateway_service(settings, observability))
            {
                eprintln!("CODETAS Gateway: {error}");
                std::process::exit(1);
            }
            return;
        }
        Ok(Some(EarlyRuntimeCommand::StartGatewayService)) => match service::start() {
            Ok(true) => return,
            Ok(false) => {
                eprintln!("CODETAS Gateway service did not reach a running state");
                std::process::exit(1);
            }
            Err(error) => {
                eprintln!("CODETAS Gateway: {error}");
                std::process::exit(1);
            }
        },
        Ok(Some(EarlyRuntimeCommand::McpServer)) => {
            if let Err(error) = mcp::run() {
                eprintln!("CODETAS MCP: {error}");
                std::process::exit(1);
            }
            return;
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("CODETAS Gateway: {error}");
            std::process::exit(2);
        }
    }
    let exit_state = Arc::new(AtomicU8::new(0));
    let builder = tauri::Builder::default();
    #[cfg(not(feature = "validation-build"))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(
        |app, _argv, _cwd| {
            focus_main_window(app);
        },
    ));
    #[cfg(not(feature = "validation-build"))]
    let builder = builder.plugin(tauri_plugin_updater::Builder::new().build());
    let app = builder
        .manage(provider_gateway::GatewayManager::default())
        .manage(provider_gateway::DebugScopeManager::default())
        .setup(move |app| {
            let open = tauri::menu::MenuItem::with_id(
                app,
                "codetas-open",
                "CODETASを開く",
                true,
                None::<&str>,
            )?;
            let start = tauri::menu::MenuItem::with_id(
                app,
                "codetas-start-gateway",
                "Gatewayを起動",
                true,
                None::<&str>,
            )?;
            let stop = tauri::menu::MenuItem::with_id(
                app,
                "codetas-stop-gateway",
                "Gatewayを停止",
                true,
                None::<&str>,
            )?;
            let quit =
                tauri::menu::MenuItem::with_id(app, "codetas-quit", "終了", true, None::<&str>)?;
            let menu = tauri::menu::Menu::with_items(app, &[&open, &start, &stop, &quit])?;
            let mut tray = tauri::tray::TrayIconBuilder::with_id("codetas-tray")
                .menu(&menu)
                .tooltip("CODETAS — Codexに、できることを足す。")
                .show_menu_on_left_click(true)
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "codetas-open" => {
                        focus_main_window(app);
                    }
                    "codetas-start-gateway" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let manager = app.state::<provider_gateway::GatewayManager>();
                            let _ = provider_gateway::gateway_ops::start_provider_gateway(
                                app.clone(),
                                manager,
                            )
                            .await;
                        });
                    }
                    "codetas-stop-gateway" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let manager = app.state::<provider_gateway::GatewayManager>();
                            let _ = provider_gateway::gateway_ops::stop_provider_gateway(
                                app.clone(),
                                manager,
                            )
                            .await;
                        });
                    }
                    "codetas-quit" => {
                        app.exit(0);
                    }
                    _ => {}
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;
            let app_handle = app.handle().clone();
            let convergence_app = app_handle.clone();
            tauri::async_runtime::spawn(async move {
                let manager = convergence_app.state::<provider_gateway::GatewayManager>();
                if let Err(error) = provider_gateway::gateway_ops::converge_codex_integration(
                    convergence_app.clone(),
                    manager,
                )
                .await
                {
                    eprintln!("CODETAS: Codex startup integration was skipped: {error}");
                }
            });
            maintenance_jobs::start_idle_maintenance_worker(app_handle.clone());
            tauri::async_runtime::spawn(async move {
                let Ok(settings) =
                    provider_gateway::presets::gateway_configuration(app_handle.clone())
                else {
                    return;
                };
                if settings.runtime.auto_start {
                    let manager = app_handle.state::<provider_gateway::GatewayManager>();
                    let _ = provider_gateway::gateway_ops::start_provider_gateway(
                        app_handle.clone(),
                        manager,
                    )
                    .await;
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            pick_project,
            inspect_project,
            list_hermes_profiles,
            convert_hermes_profiles,
            hermes_sync::scan_hermes_sync,
            hermes_sync::preview_hermes_sync,
            hermes_sync::apply_hermes_sync,
            hermes_sync::list_hermes_editable_files,
            hermes_sync::save_hermes_editable_file,
            provider_gateway::gateway_ops::start_provider_gateway,
            provider_gateway::gateway_ops::stop_provider_gateway,
            provider_gateway::gateway_ops::provider_gateway_status,
            maintenance::analyze_codex_maintenance,
            maintenance::save_codex_context_document,
            maintenance::set_codex_skill_enabled,
            maintenance_jobs::preview_codex_maintenance,
            maintenance_jobs::execute_codex_maintenance,
            maintenance_jobs::list_codex_maintenance_jobs,
            maintenance_jobs::rollback_codex_maintenance_job,
            maintenance_process::request_codex_shutdown,
            maintenance_process::restart_codex,
            maintenance_process::terminate_codex_writer,
            codex_app_server::retry_codex_archive,
            provider_gateway::presets::gateway_configuration,
            provider_gateway::presets::oauth_provider_registry,
            provider_gateway::presets::gateway_compatibility_lab,
            provider_gateway::presets::gateway_route_dry_runs,
            provider_gateway::presets::check_for_codetas_update,
            provider_gateway::presets::install_codetas_update,
            provider_gateway::gateway_ops::save_gateway_configuration,
            provider_gateway::gateway_ops::patch_gateway_management,
            provider_gateway::gateway_ops::apply_agent_preset_configuration,
            provider_gateway::cli_scan::list_provider_presets,
            provider_gateway::cli_scan::scan_local_cli_clients,
            provider_gateway::cli_scan::register_local_cli_in_codetas,
            provider_gateway::cli_scan::register_codetas_provider,
            provider_gateway::cli_scan::list_direct_api_targets,
            provider_gateway::diagnostics::test_gateway_provider,
            provider_gateway::agent_tools::codex_plugin_status,
            provider_gateway::agent_tools::run_agent_media_test,
            provider_gateway::service_cmds::launch_provider_oauth_broker,
            provider_gateway::diagnostics::gateway_diagnostics,
            provider_gateway::diagnostics::gateway_observability_summary,
            provider_gateway::diagnostics::gateway_observability_breakdown,
            provider_gateway::diagnostics::preview_gateway_observability_cleanup,
            provider_gateway::diagnostics::trash_gateway_observability_cleanup,
            provider_gateway::diagnostics::list_gateway_observability_trash,
            provider_gateway::diagnostics::restore_gateway_observability_trash,
            provider_gateway::diagnostics::start_gateway_debug_scope,
            provider_gateway::diagnostics::gateway_debug_events,
            provider_gateway::diagnostics::stop_gateway_debug_scope,
            provider_gateway::service_cmds::gateway_service_status,
            provider_gateway::service_cmds::install_gateway_service,
            provider_gateway::service_cmds::start_gateway_service,
            provider_gateway::service_cmds::restart_gateway_service,
            provider_gateway::service_cmds::stop_gateway_service,
            provider_gateway::service_cmds::uninstall_gateway_service,
            provider_gateway::integration::sync_client_integrations,
            provider_gateway::presets::install_provider_preset,
            provider_gateway::presets::refresh_gateway_provider_models,
            provider_gateway::presets::sync_codex_model_catalog,
            provider_gateway::gateway_ops::upsert_gateway_provider,
            provider_gateway::gateway_ops::remove_gateway_provider,
            provider_gateway::gateway_ops::set_default_gateway_provider,
            provider_gateway::gateway_ops::install_codex_gateway_config,
            provider_gateway::gateway_ops::restore_codex_gateway_config,
            provider_gateway::gateway_ops::uninstall_codetas_integration
        ])
        .build(tauri::generate_context!())
        .expect("failed to build CODETAS");
    app.run(move |app, event| {
        if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
            if code == Some(tauri::RESTART_EXIT_CODE) {
                return;
            }
            match exit_state.load(Ordering::Acquire) {
                2 => return,
                1 => {
                    api.prevent_exit();
                    return;
                }
                _ => {}
            }
            api.prevent_exit();
            if exit_state
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return;
            }
            let exit_state = Arc::clone(&exit_state);
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let manager = app.state::<provider_gateway::GatewayManager>();
                provider_gateway::gateway_ops::shutdown_embedded_gateway(&manager).await;
                exit_state.store(2, Ordering::Release);
                app.exit(0);
            });
        }
    });
}

enum EarlyRuntimeCommand {
    GatewayService {
        settings: PathBuf,
        observability: PathBuf,
    },
    StartGatewayService,
    McpServer,
}

fn early_runtime_command() -> Result<Option<EarlyRuntimeCommand>, String> {
    let mut arguments = std::env::args_os().skip(1);
    let Some(command) = arguments.next() else {
        return Ok(None);
    };
    if command == "--start-gateway-service" {
        if arguments.next().is_some() {
            return Err("--start-gateway-service does not accept additional arguments".into());
        }
        return Ok(Some(EarlyRuntimeCommand::StartGatewayService));
    }
    if command == "--mcp-server" {
        if arguments.next().is_some() {
            return Err("--mcp-server does not accept additional arguments".into());
        }
        return Ok(Some(EarlyRuntimeCommand::McpServer));
    }
    if command != "--gateway-service" {
        return Ok(None);
    }
    let mut settings = None;
    let mut observability = None;
    while let Some(argument) = arguments.next() {
        if argument == "--config" {
            settings = Some(PathBuf::from(
                arguments.next().ok_or("--config requires a path")?,
            ));
        } else if argument == "--observability" {
            observability = Some(PathBuf::from(
                arguments.next().ok_or("--observability requires a path")?,
            ));
        } else {
            return Err(format!(
                "unknown gateway service argument: {}",
                argument.to_string_lossy()
            ));
        }
    }
    Ok(Some(EarlyRuntimeCommand::GatewayService {
        settings: settings.ok_or("--gateway-service requires --config")?,
        observability: observability.ok_or("--gateway-service requires --observability")?,
    }))
}
