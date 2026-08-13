use serde_json::{json, Value};
use std::{
    fs::{self, File},
    io::{self, BufRead, Read, Write},
    path::{Path, PathBuf},
};

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_CONTEXT_BYTES: usize = 64 * 1024;
const MAX_SCAN_DEPTH: usize = 4;

pub fn run() -> Result<(), String> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut stdout = io::stdout().lock();
    loop {
        let response = match read_bounded_line(&mut input)
            .map_err(|error| format!("failed to read MCP request: {error}"))?
        {
            BoundedLine::Eof => break,
            BoundedLine::TooLarge => {
                jsonrpc_error(Value::Null, -32600, "MCP request exceeds the size limit")
            }
            BoundedLine::Line(line) => match serde_json::from_slice::<Value>(&line) {
                Ok(message) => match handle(&message) {
                    Some(response) => response,
                    None => continue,
                },
                Err(_) => jsonrpc_error(Value::Null, -32700, "Invalid JSON"),
            },
        };
        serde_json::to_writer(&mut stdout, &response)
            .map_err(|error| format!("failed to encode MCP response: {error}"))?;
        stdout
            .write_all(b"\n")
            .and_then(|_| stdout.flush())
            .map_err(|error| format!("failed to write MCP response: {error}"))?;
    }
    Ok(())
}

enum BoundedLine {
    Line(Vec<u8>),
    TooLarge,
    Eof,
}

fn read_bounded_line(reader: &mut impl BufRead) -> io::Result<BoundedLine> {
    let mut line = Vec::with_capacity(8 * 1024);
    let mut too_large = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if line.is_empty() && !too_large {
                return Ok(BoundedLine::Eof);
            }
            return Ok(if too_large {
                BoundedLine::TooLarge
            } else {
                BoundedLine::Line(line)
            });
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        if !too_large {
            let content_len = consumed - usize::from(newline.is_some());
            if line.len().saturating_add(content_len) > MAX_REQUEST_BYTES {
                too_large = true;
                line.clear();
            } else {
                line.extend_from_slice(&available[..content_len]);
            }
        }
        reader.consume(consumed);
        if newline.is_some() {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(if too_large {
                BoundedLine::TooLarge
            } else {
                BoundedLine::Line(line)
            });
        }
    }
}

fn handle(message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let result = match method {
        "initialize" => {
            let protocol = message
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2024-11-05");
            json!({
                "protocolVersion": protocol,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "codetas-project", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "CODETAS project tools are local and read-only."
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let name = message
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = message
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            call_tool(name, &arguments)
        }
        _ => return Some(jsonrpc_error(id, -32601, "Method not found")),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn tools() -> Vec<Value> {
    [
        (
            "inspect_project_context",
            "Inspect Hermes/Codex context and reusable skills without modifying files.",
        ),
        (
            "list_project_skills",
            "List Hermes-compatible SKILL.md files in the current project.",
        ),
        (
            "read_project_context",
            "Read .hermes.md or HERMES.md after local safety checks.",
        ),
    ]
    .into_iter()
    .map(|(name, description)| {
        json!({
            "name": name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "projectPath": {
                        "type": "string",
                        "description": "Absolute project directory; defaults to the server working directory."
                    }
                },
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": true, "destructiveHint": false}
        })
    })
    .collect()
}

fn call_tool(name: &str, arguments: &Value) -> Value {
    let project = match requested_project(arguments) {
        Ok(project) => project,
        Err(message) => return text_result(Value::String(message), true),
    };
    match name {
        "inspect_project_context" => text_result(inspect_project(&project), false),
        "list_project_skills" => text_result(
            json!({
                "projectRoot": project_root(&project),
                "skills": list_skills(&project)
            }),
            false,
        ),
        "read_project_context" => read_context(&project),
        _ => text_result(Value::String(format!("Unknown tool: {name}")), true),
    }
}

fn requested_project(arguments: &Value) -> Result<PathBuf, String> {
    let supplied = arguments.get("projectPath").and_then(Value::as_str);
    let path = supplied
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    if supplied.is_some() && !path.is_absolute() {
        return Err("projectPath must be absolute".into());
    }
    let canonical =
        fs::canonicalize(path).map_err(|error| format!("projectPath cannot be opened: {error}"))?;
    if !canonical.is_dir() {
        return Err("projectPath must be a directory".into());
    }
    Ok(canonical)
}

fn inspect_project(start: &Path) -> Value {
    let root = project_root(start);
    let context = find_file(&root, &[".hermes.md", "HERMES.md"]);
    let agents = find_file(&root, &["AGENTS.md"]);
    let mcp = find_file(&root, &[".hermes/mcp.json", "mcp.json", ".mcp.json"]);
    let skills = list_skills(&root);
    json!({
        "projectRoot": root,
        "contextFile": context,
        "agentsFile": agents,
        "mcpFile": mcp,
        "skills": skills,
        "readOnly": true
    })
}

fn list_skills(start: &Path) -> Vec<Value> {
    let root = project_root(start);
    let mut output = Vec::new();
    for directory in [
        root.join(".hermes/skills"),
        root.join("skills"),
        root.join(".agents/skills"),
    ] {
        scan_skills(&root, &directory, 0, &mut output);
    }
    output.sort_by(|left, right| {
        left.get("path")
            .and_then(Value::as_str)
            .cmp(&right.get("path").and_then(Value::as_str))
    });
    output.dedup_by(|left, right| left.get("path") == right.get("path"));
    output
}

fn scan_skills(root: &Path, directory: &Path, depth: usize, output: &mut Vec<Value>) {
    if depth > MAX_SCAN_DEPTH || output.len() >= 500 {
        return;
    }
    let Ok(metadata) = fs::symlink_metadata(directory) else {
        return;
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return;
    }
    let Ok(canonical_directory) = fs::canonicalize(directory) else {
        return;
    };
    if !canonical_directory.starts_with(root) {
        return;
    }
    let Ok(entries) = fs::read_dir(canonical_directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            scan_skills(root, &path, depth + 1, output);
        } else if file_type.is_file()
            && path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md")
        {
            let Ok(canonical) = fs::canonicalize(&path) else {
                continue;
            };
            if !canonical.starts_with(root) {
                continue;
            }
            let description = read_bounded_file(&canonical, 8 * 1024)
                .ok()
                .and_then(|content| {
                    let content = String::from_utf8_lossy(&content);
                    content.lines().find_map(|line| {
                        line.strip_prefix("description:")
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(|value| value.chars().take(500).collect::<String>())
                    })
                });
            output.push(json!({
                "name": path.parent().and_then(Path::file_name).and_then(|name| name.to_str()).unwrap_or("skill"),
                "description": description,
                "path": canonical
            }));
        }
    }
}

fn read_context(start: &Path) -> Value {
    let root = project_root(start);
    let Some(path) = find_file(&root, &[".hermes.md", "HERMES.md"]) else {
        return text_result(
            Value::String("No .hermes.md or HERMES.md was found.".into()),
            true,
        );
    };
    let bytes = match read_bounded_file(&path, MAX_CONTEXT_BYTES + 1) {
        Ok(bytes) => bytes,
        Err(error) => {
            return text_result(
                Value::String(format!("Could not read context: {error}")),
                true,
            )
        }
    };
    let truncated = bytes.len() > MAX_CONTEXT_BYTES;
    let content =
        String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_CONTEXT_BYTES)]).into_owned();
    let reasons = suspicious_reasons(&content);
    if !reasons.is_empty() {
        return text_result(
            json!({"blocked": true, "path": path, "reasons": reasons}),
            true,
        );
    }
    text_result(
        json!({"path": path, "content": content, "truncated": truncated}),
        false,
    )
}

fn read_bounded_file(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::other(
            "refusing non-regular or symbolic-link file",
        ));
    }
    let mut output = Vec::with_capacity(limit.min(8 * 1024));
    File::open(path)?
        .take(limit as u64)
        .read_to_end(&mut output)?;
    Ok(output)
}

fn suspicious_reasons(content: &str) -> Vec<&'static str> {
    let lower = content.to_ascii_lowercase();
    let mut reasons = Vec::new();
    if lower.contains("ignore previous instructions")
        || lower.contains("ignore all previous")
        || lower.contains("system prompt")
    {
        reasons.push("instruction override marker");
    }
    if lower.contains("api key")
        || lower.contains("auth.json")
        || lower.contains(".env")
        || lower.contains("private key")
    {
        reasons.push("credential access marker");
    }
    if content.chars().any(|character| {
        matches!(
            character,
            '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{2060}' | '\u{feff}'
        )
    }) {
        reasons.push("invisible Unicode control");
    }
    reasons
}

fn project_root(start: &Path) -> PathBuf {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return current;
        }
        let Some(parent) = current.parent() else {
            return start.to_path_buf();
        };
        current = parent.to_path_buf();
    }
}

fn find_file(root: &Path, candidates: &[&str]) -> Option<PathBuf> {
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

fn text_result(value: Value, is_error: bool) -> Value {
    let text = value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into()));
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn jsonrpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}
