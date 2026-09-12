use super::chat_tools::ensure_function_parameters_object;
use super::*;

const LOCAL_REASONING_PREFIXES: &[&str] = &["codetas1:", "ocxr1:"];
const MAX_RESPONSES_CALL_ID_LENGTH: usize = 64;
const REPAIRED_CALL_ID_PREFIX: &str = "call_cdt_";
const VALID_ITEM_ID_PREFIXES: &[(&str, &str)] = &[
    ("message", "msg_"),
    ("agent_message", "amsg_"),
    ("reasoning", "rs_"),
    ("function_call", "fc_"),
    ("custom_tool_call", "ctc_"),
    ("tool_search_call", "tsc_"),
    ("web_search_call", "ws_"),
];

/// ChatGPT's Codex backend uses a strict parameter allowlist. Forwarding
/// `metadata`, `max_output_tokens`, or raw reasoning content produces HTTP 400.
/// Keep `previous_response_id` so continuation matches official Codex CLI
/// instead of replaying the full local history. Do not force `store=true`;
/// ChatGPT Codex rejects that flag with HTTP 400.
pub fn uses_chatgpt_codex_backend(provider: &ProviderDefinition) -> bool {
    let base = provider.base_url.to_ascii_lowercase();
    provider.credential.source == CredentialSource::Forward
        || base.contains("chatgpt.com/backend-api")
        || base.contains("/backend-api/codex")
}

fn chatgpt_codex_keeps_previous_response(provider: &ProviderDefinition) -> bool {
    let base = provider.base_url.to_ascii_lowercase();
    base.contains("chatgpt.com/backend-api") || base.contains("/backend-api/codex")
}

/// Expand local CODETAS compaction envelopes before any provider-specific
/// sanitization. Callers may invoke this function directly, so expansion
/// cannot stay only on the production Responses compatibility path.
pub fn sanitize_responses_upstream_request(
    body: &mut Value,
    provider: &ProviderDefinition,
    model: &str,
) {
    crate::compaction::expand_local_compactions(body);
    let chatgpt = uses_chatgpt_codex_backend(provider);
    let chatgpt_stateful = chatgpt_codex_keeps_previous_response(provider);
    let stateless = provider.stateless_responses;
    let has_previous_response = body
        .get("previous_response_id")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());
    if !chatgpt && !stateless {
        if !has_previous_response {
            repair_orphaned_input_items(body, false);
        }
        sanitize_reasoning_input_content(body);
        strip_invalid_item_ids(body);
        strip_item_ids_when_unstored(body);
        normalize_function_tool_schemas(body);
        return;
    }

    let unexpanded_miss = has_previous_response;

    // Official ChatGPT Codex is stateful, like CLI. Strip the id only when the
    // upstream cannot resolve it. Generic Forward is not automatically stateful.
    if !chatgpt_stateful && (chatgpt || unexpanded_miss || stateless) {
        if has_previous_response {
            crate::debug::log_always(&format!(
                "sanitize stripped previous_response_id chatgpt={chatgpt} stateful={chatgpt_stateful} stateless={stateless} unexpanded_miss={unexpanded_miss} model={model}"
            ));
        }
        if let Some(object) = body.as_object_mut() {
            object.remove("previous_response_id");
        }
    }
    if stateless {
        if let Some(object) = body.as_object_mut() {
            object.remove("conversation");
            object.remove("background");
            object.remove("metadata");
            object.remove("prompt");
            object.insert("store".into(), Value::Bool(false));
        }
    }
    if chatgpt {
        if let Some(object) = body.as_object_mut() {
            object.remove("max_output_tokens");
            object.remove("metadata");
            // ChatGPT Codex rejects an explicit `store=true` with HTTP 400.
            // Leave the client's store flag alone; do not force CLI API store.
        }
    }
    if chatgpt || stateless {
        // A ChatGPT delta after previous_response_id is a legitimate unpaired
        // tool output. Do not rewrite it into a user message.
        if !(chatgpt_stateful && has_previous_response) {
            repair_orphaned_input_items(
                body,
                chatgpt && !chatgpt_stateful && unexpanded_miss,
            );
        }
    }
    if chatgpt || (!chatgpt && unexpanded_miss) {
        repair_oversized_replay_call_ids(body);
    }
    sanitize_reasoning_input_content(body);
    strip_invalid_item_ids(body);
    strip_item_ids_when_unstored(body);
    if model.contains("codex-spark") {
        strip_spark_compatibility(body);
    }
    normalize_function_tool_schemas(body);
}

/// Stop a model from spending an unbounded turn repeatedly invoking the same
/// successful function tool. The guard is intentionally request-local: it
/// only activates when the reconstructed history since the latest user
/// message ends with at least `REPEATED_FUNCTION_TOOL_LIMIT` completed calls
/// to one function, or `REPEATED_READONLY_INSPECT_LIMIT` completed read-only
/// `exec` inspections. Explicit wait/poll operations are exempt: a running
/// delegated process may legitimately require an unbounded number of waits
/// before the parent can inspect its result and report completion.
///
/// A repeated ordinary function is removed from the next request while all
/// other tools remain available. Readonly inspection loops and the shared
/// shell/exec surface are handled specially: neither is removed because the
/// latter is also the write path (`tools.apply_patch` inside `exec`). For a
/// shared execution surface, only a forced tool choice is cleared so the
/// model can stop polling and either write or provide the final result.
pub fn guard_repeated_function_tool_loop(body: &mut Value) -> Option<String> {
    let repeated = detect_repeated_tool_loop(body)?;
    let repeated_name = repeated.name().to_string();
    let remove_repeated_tool = repeated.removes_tool();
    let warning = repeated.warning_text();

    if remove_repeated_tool {
        if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
            remove_named_function_tool(tools, &repeated_name);
        }
        if let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) {
            for item in items.iter_mut() {
                if item.get("type").and_then(Value::as_str) != Some("additional_tools") {
                    continue;
                }
                if let Some(tools) = item.get_mut("tools").and_then(Value::as_array_mut) {
                    remove_named_function_tool(tools, &repeated_name);
                }
            }
        }
    }
    if let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) {
        items.push(json!({
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": warning
            }]
        }));
    }

    let remove_tool_choice = repeated.clears_forced_tool_choice()
        && body
            .get("tool_choice")
            .and_then(Value::as_object)
            .and_then(|choice| {
                choice.get("name").and_then(Value::as_str).or_else(|| {
                    choice
                        .get("function")
                        .and_then(Value::as_object)
                        .and_then(|function| function.get("name"))
                        .and_then(Value::as_str)
                })
            })
            == Some(repeated_name.as_str());
    if remove_tool_choice {
        if let Some(object) = body.as_object_mut() {
            object.remove("tool_choice");
        }
    }

    crate::debug::log_always(&format!("blocked repeated tool loop name={repeated_name}"));
    Some(repeated_name)
}

enum RepeatedToolLoop {
    Function(String),
    ReadonlyInspect(String),
    SharedExecution(String),
}

impl RepeatedToolLoop {
    fn name(&self) -> &str {
        match self {
            Self::Function(name) | Self::ReadonlyInspect(name) | Self::SharedExecution(name) => {
                name
            }
        }
    }

    fn is_readonly_inspect(&self) -> bool {
        matches!(self, Self::ReadonlyInspect(_))
    }

    fn removes_tool(&self) -> bool {
        matches!(self, Self::Function(_))
    }

    fn clears_forced_tool_choice(&self) -> bool {
        !self.is_readonly_inspect()
    }

    fn warning_text(&self) -> String {
        match self {
            Self::Function(name) => format!(
                "CODETAS stopped a repeated tool loop: `{name}` already completed \
                 successfully at least {REPEATED_FUNCTION_TOOL_LIMIT} times in succession. \
                 Do not call that tool again in this turn. Continue the requested work using \
                 another available tool or provide the final result."
            ),
            Self::ReadonlyInspect(name) => format!(
                "CODETAS stopped a repeated readonly inspect loop on `{name}` after \
                 {REPEATED_READONLY_INSPECT_LIMIT} successful read-only inspections in succession. \
                 Stop further readonly inspect (cat / sed -n / rg / head / tail / wc / nl / grep). \
                 Continue this turn by writing files with apply_patch or another write path \
                 through `{name}`."
            ),
            Self::SharedExecution(name) => format!(
                "CODETAS detected a repeated shared execution loop on `{name}` after \
                 {REPEATED_FUNCTION_TOOL_LIMIT} successful calls in succession. Stop repeating \
                 the same inspection or polling operation. The tool remains available because \
                 it is also the write path; use it only for a concrete write now, or provide \
                 the final result if no write remains."
            ),
        }
    }
}

fn detect_repeated_tool_loop(body: &Value) -> Option<RepeatedToolLoop> {
    let items = body.get("input").and_then(Value::as_array)?;
    let mut calls = HashMap::<String, (String, String)>::new();
    let mut completed_calls = Vec::<(String, String)>::new();

    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("message") if item.get("role").and_then(Value::as_str) == Some("user") => {
                if !is_repeated_tool_guard_message(item) {
                    calls.clear();
                    completed_calls.clear();
                }
            }
            Some("function_call" | "local_shell_call" | "custom_tool_call") => {
                let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(name) = item.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let arguments = item
                    .get("arguments")
                    .or_else(|| item.get("input"))
                    .map(canonical_tool_arguments)
                    .unwrap_or_default();
                calls.insert(call_id.to_string(), tool_loop_key(name, &arguments));
            }
            Some(
                "function_call_output" | "local_shell_call_output" | "custom_tool_call_output",
            ) => {
                let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
                    continue;
                };
                let completed = item.get("output").is_some_and(|output| match output {
                    Value::Null => false,
                    Value::String(text) => !text.trim().is_empty(),
                    Value::Array(items) => !items.is_empty(),
                    Value::Object(object) => !object.is_empty(),
                    Value::Bool(_) | Value::Number(_) => true,
                });
                if completed {
                    if let Some(call) = calls.remove(call_id) {
                        completed_calls.push(call);
                    }
                } else {
                    calls.remove(call_id);
                    completed_calls.clear();
                }
            }
            _ => {}
        }
    }

    let last = completed_calls.last()?.clone();
    let repeated = completed_calls
        .iter()
        .rev()
        .take_while(|call| **call == last)
        .count();
    if last.1 == "wait-poll" || is_wait_poll_tool(&last.0) {
        None
    } else if last.1 == "readonly-inspect" {
        (repeated >= REPEATED_READONLY_INSPECT_LIMIT)
            .then_some(RepeatedToolLoop::ReadonlyInspect(last.0))
    } else if is_shared_execution_tool(&last.0) {
        (repeated >= REPEATED_FUNCTION_TOOL_LIMIT)
            .then_some(RepeatedToolLoop::SharedExecution(last.0))
    } else {
        (repeated >= REPEATED_FUNCTION_TOOL_LIMIT).then_some(RepeatedToolLoop::Function(last.0))
    }
}

fn tool_loop_key(name: &str, arguments: &str) -> (String, String) {
    if is_wait_poll_call(name, arguments) {
        (name.to_string(), "wait-poll".into())
    } else if is_readonly_inspect_tool(name, arguments) {
        (name.to_string(), "readonly-inspect".into())
    } else {
        (name.to_string(), arguments.to_string())
    }
}

/// Returns true when a tool call is an executable read-only inspection. This
/// is shared with the response-side fail-closed guard so request and response
/// paths agree on what is safe to repeat.
pub(crate) fn is_readonly_inspect_tool(name: &str, arguments: &str) -> bool {
    matches!(
        tool_name_leaf(name).to_ascii_lowercase().as_str(),
        "exec" | "exec_command" | "shell" | "bash"
    ) && is_readonly_inspect_command(&extract_exec_command(arguments))
}

/// True when the current reconstructed history contains the request-local
/// repeated-read warning. This marker is intentionally not global or persisted
/// outside the current turn.
pub(crate) fn repeated_readonly_inspect_guard_active(body: &Value) -> bool {
    body.get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("message")
                    && item.get("role").and_then(Value::as_str) == Some("user")
                    && item
                        .get("content")
                        .and_then(Value::as_array)
                        .and_then(|parts| parts.first())
                        .and_then(|part| part.get("text"))
                        .and_then(Value::as_str)
                        .is_some_and(|text| {
                            text.starts_with("CODETAS stopped a repeated readonly inspect loop")
                        })
            })
        })
}

fn is_shared_execution_tool(name: &str) -> bool {
    matches!(
        tool_name_leaf(name).to_ascii_lowercase().as_str(),
        "exec" | "exec_command" | "shell" | "bash"
    )
}

fn is_wait_poll_tool(name: &str) -> bool {
    matches!(
        tool_name_leaf(name).to_ascii_lowercase().as_str(),
        "wait" | "write_stdin" | "wait_threads" | "wait_agent" | "read_thread_terminal"
    )
}

fn is_wait_poll_call(name: &str, arguments: &str) -> bool {
    if is_wait_poll_tool(name) {
        return true;
    }
    if !is_shared_execution_tool(name) {
        return false;
    }

    let source = arguments.to_ascii_lowercase();
    [
        "tools.wait(",
        "tools.write_stdin(",
        "tools.wait_threads(",
        "tools.wait_agent(",
        "tools.read_thread_terminal(",
        "tools.codex_app__wait_threads(",
        "tools.codex_app__read_thread_terminal(",
    ]
    .iter()
    .any(|marker| source.contains(marker))
}

/// Tool calls can arrive through an adapter namespace (for example
/// `functions.exec` or `tools::wait_threads`). The loop guard must classify
/// the executable leaf name, otherwise a namespaced write-capable tool falls
/// through to the ordinary-function branch and gets removed after repetition.
fn tool_name_leaf(name: &str) -> &str {
    name.rsplit(|character| matches!(character, '.' | ':' | '/'))
        .next()
        .unwrap_or(name)
}

fn is_repeated_tool_guard_message(item: &Value) -> bool {
    item.get("content")
        .and_then(Value::as_array)
        .and_then(|parts| parts.first())
        .and_then(|part| part.get("text"))
        .and_then(Value::as_str)
        .is_some_and(|text| {
            text.starts_with("CODETAS stopped a repeated")
                || text.starts_with("CODETAS detected a repeated")
        })
}

fn extract_exec_command(arguments: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(arguments) {
        if let Some(command) = value
            .get("cmd")
            .or_else(|| value.get("command"))
            .and_then(Value::as_str)
        {
            return command.to_string();
        }
    }
    for marker in ["cmd: \"", "cmd:\"", "cmd: '", "command: \""] {
        if let Some(start) = arguments.find(marker) {
            let rest = &arguments[start + marker.len()..];
            let quote = if marker.ends_with('\'') { '\'' } else { '"' };
            if let Some(end) = rest.find(quote) {
                return rest[..end].replace("\\n", " ").replace("\\\"", "\"");
            }
        }
    }
    arguments.to_string()
}

fn is_readonly_inspect_command(command: &str) -> bool {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    const WRITES: &[&str] = &[
        "rm ",
        "mv ",
        "cp ",
        "tee ",
        "mkdir ",
        "touch ",
        "chmod ",
        ">",
        ">>",
        "sed -i",
        "apply_patch",
        "git add",
        "git commit",
        "git restore",
    ];
    if WRITES.iter().any(|token| lower.contains(token)) {
        return false;
    }
    trimmed.split("&&").all(|part| {
        let part = part
            .trim()
            .trim_start_matches("sudo ")
            .trim_start_matches("/bin/")
            .trim_start_matches("/usr/bin/");
        part.starts_with("sed -n")
            || part.starts_with("sed -E -n")
            || part.starts_with("cat ")
            || part == "cat"
            || part.starts_with("head ")
            || part.starts_with("tail ")
            || part.starts_with("wc ")
            || part.starts_with("nl ")
            || part.starts_with("rg ")
            || part.starts_with("grep ")
    })
}

fn canonical_tool_arguments(arguments: &Value) -> String {
    match arguments {
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(Value::String(nested)) => serde_json::from_str::<Value>(&nested)
                .map(|value| value.to_string())
                .unwrap_or_else(|_| nested.trim().to_string()),
            Ok(value) => value.to_string(),
            Err(_) => text.trim().to_string(),
        },
        value => value.to_string(),
    }
}

fn remove_named_function_tool(tools: &mut Vec<Value>, name: &str) {
    tools.retain_mut(|tool| {
        if tool.get("type").and_then(Value::as_str) == Some("namespace") {
            if let Some(inner) = tool.get_mut("tools").and_then(Value::as_array_mut) {
                remove_named_function_tool(inner, name);
                return !inner.is_empty();
            }
        }
        let tool_name = tool
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| tool.pointer("/function/name").and_then(Value::as_str));
        tool_name != Some(name)
    });
}

/// Apply the narrow sanitization needed before forwarding a native
/// `/responses/compact` request.
///
/// OpenCodex removes the top-level `reasoning` request option and sanitizes
/// replayed reasoning input items before sending the compact request directly
/// to the native backend. Local `codetas1:` / `ocx1:` compaction envelopes are
/// CODETAS transport and must be expanded before that hop; the native backend
/// cannot consume them. Keep this separate from the broader normal-turn
/// compatibility pass: compact envelopes and history must otherwise remain
/// unchanged.
pub(crate) fn sanitize_compact_request(body: &mut Value) {
    crate::compaction::expand_local_compactions(body);
    if let Some(object) = body.as_object_mut() {
        object.remove("reasoning");
    }
    repair_orphaned_input_items(body, false);
    sanitize_reasoning_input_content(body);
}

/// Apply the OpenCodex reasoning-item sanitizer to a native v2
/// `compaction_trigger` request without changing its top-level options.
///
/// The v2 request is forwarded through the normal `/responses` endpoint, so its
/// `reasoning` option must remain intact. Raw replayed reasoning content still
/// has to be removed, just as it is on OpenCodex's regular Responses path.
/// Previous local compaction envelopes are expanded here so the native backend
/// never receives a `codetas1:` payload.
pub(crate) fn sanitize_compact_trigger_request(body: &mut Value) {
    crate::compaction::expand_local_compactions(body);
    repair_orphaned_input_items(body, false);
    sanitize_reasoning_input_content(body);
}

fn sanitize_reasoning_input_content(body: &mut Value) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    for item in items {
        let Some(object) = item.as_object_mut() else {
            continue;
        };
        if object.get("type").and_then(Value::as_str) != Some("reasoning") {
            continue;
        }
        let has_raw_content = object
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|content| !content.is_empty());
        let has_local_envelope = object
            .get("encrypted_content")
            .and_then(Value::as_str)
            .is_some_and(|value| {
                LOCAL_REASONING_PREFIXES
                    .iter()
                    .any(|prefix| value.starts_with(prefix))
            });
        if !has_raw_content && !has_local_envelope {
            continue;
        }
        object.insert("content".into(), Value::Array(Vec::new()));
        if has_local_envelope {
            object.remove("encrypted_content");
        }
    }
}

fn strip_invalid_item_ids(body: &mut Value) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    for item in items {
        let Some(object) = item.as_object_mut() else {
            continue;
        };
        let Some(item_type) = object.get("type").and_then(Value::as_str) else {
            continue;
        };
        let Some((_, prefix)) = VALID_ITEM_ID_PREFIXES
            .iter()
            .find(|(kind, _)| *kind == item_type)
        else {
            continue;
        };
        match object.get("id").and_then(Value::as_str) {
            Some(id) if id.starts_with(prefix) => {}
            Some(_) => {
                object.remove("id");
            }
            None => {}
        }
    }
}

fn strip_item_ids_when_unstored(body: &mut Value) {
    if body.get("store") != Some(&Value::Bool(false)) {
        return;
    }
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    for item in items {
        if let Some(object) = item.as_object_mut() {
            object.remove("id");
        }
    }
}

pub fn repair_translated_input_items(body: &mut Value) {
    repair_orphaned_input_items(body, false);
}

fn repair_orphaned_input_items(body: &mut Value, drop_orphaned_reasoning: bool) {
    let Some(items) = body.get("input").and_then(Value::as_array) else {
        return;
    };
    let mut function_call_ids = HashSet::new();
    let mut custom_call_ids = HashSet::new();
    let mut tool_search_call_ids = HashSet::new();
    for item in items {
        let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
            continue;
        };
        match item.get("type").and_then(Value::as_str) {
            Some("function_call" | "local_shell_call") => {
                function_call_ids.insert(call_id.to_string());
            }
            Some("custom_tool_call") => {
                custom_call_ids.insert(call_id.to_string());
            }
            Some("tool_search_call") => {
                tool_search_call_ids.insert(call_id.to_string());
            }
            _ => {}
        }
    }

    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    let mut repaired = Vec::with_capacity(items.len());
    let mut changed = false;
    for item in items.iter() {
        let Some(item_type) = item.get("type").and_then(Value::as_str) else {
            repaired.push(item.clone());
            continue;
        };
        if drop_orphaned_reasoning && item_type == "reasoning" {
            changed = true;
            continue;
        }
        let is_fn_output = matches!(
            item_type,
            "function_call_output" | "local_shell_call_output"
        );
        let is_custom_output = item_type == "custom_tool_call_output";
        let is_tool_search_output = item_type == "tool_search_output";
        if is_fn_output || is_custom_output || is_tool_search_output {
            let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
            let paired = if is_fn_output {
                function_call_ids.contains(call_id)
            } else if is_custom_output {
                custom_call_ids.contains(call_id)
            } else {
                tool_search_call_ids.contains(call_id)
            };
            if !paired {
                changed = true;
                repaired.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": [{
                        "type": "input_text",
                        "text": format!(
                            "[tool output for {}]\n{}",
                            if call_id.is_empty() { "unknown call" } else { call_id },
                            orphaned_tool_output_text(item)
                        )
                    }]
                }));
                continue;
            }
        }
        repaired.push(item.clone());
    }
    if changed {
        *items = repaired;
    }
}

fn orphaned_tool_output_text(item: &Value) -> String {
    if item.get("type").and_then(Value::as_str) == Some("tool_search_output") {
        if let Some(tools) = item.get("tools").and_then(Value::as_array) {
            let names = tools
                .iter()
                .filter_map(|tool| tool.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>();
            if !names.is_empty() {
                return format!("Tool search loaded: {}", names.join(", "));
            }
            if !tools.is_empty() {
                return Value::Array(tools.clone()).to_string();
            }
        }
    }
    tool_output_text(item.get("output"))
}

fn tool_output_text(output: Option<&Value>) -> String {
    match output {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.get("refusal").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn repair_oversized_replay_call_ids(body: &mut Value) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    let mut occupied = HashSet::new();
    for item in items.iter() {
        if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
            if call_id.len() <= MAX_RESPONSES_CALL_ID_LENGTH {
                occupied.insert(call_id.to_string());
            }
        }
    }
    let mut aliases = HashMap::new();
    for item in items {
        let Some(object) = item.as_object_mut() else {
            continue;
        };
        let Some(original) = object
            .get("call_id")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        if original.len() <= MAX_RESPONSES_CALL_ID_LENGTH {
            continue;
        }
        let alias = aliases
            .entry(original.clone())
            .or_insert_with(|| {
                let mut salt = 0_u32;
                loop {
                    let mut digest = Sha256::new();
                    digest.update(original.as_bytes());
                    if salt > 0 {
                        digest.update(salt.to_le_bytes());
                    }
                    let hex = format!("{:x}", digest.finalize());
                    let digest_len = MAX_RESPONSES_CALL_ID_LENGTH - REPAIRED_CALL_ID_PREFIX.len();
                    let candidate = format!("{REPAIRED_CALL_ID_PREFIX}{}", &hex[..digest_len]);
                    if occupied.insert(candidate.clone()) {
                        return candidate;
                    }
                    salt += 1;
                }
            })
            .clone();
        object.insert("call_id".into(), Value::String(alias));
    }
}

fn strip_spark_compatibility(body: &mut Value) {
    const UNSUPPORTED_HOSTED: &[&str] = &["image_generation", "tool_search"];
    const SAFE_TOOL_TYPES: &[&str] = &["function", "web_search", "web_search_preview"];
    const UNSUPPORTED_INPUT: &[&str] = &[
        "tool_search_call",
        "tool_search_output",
        "custom_tool_call",
        "custom_tool_call_output",
    ];

    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        *tools = flatten_spark_tools(tools, SAFE_TOOL_TYPES, UNSUPPORTED_HOSTED);
    }
    if let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) {
        let mut cleaned = Vec::with_capacity(items.len());
        for item in items.iter() {
            let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
            if UNSUPPORTED_INPUT.contains(&item_type) {
                continue;
            }
            if item_type == "additional_tools" {
                let mut next = item.clone();
                if let Some(tools) = next.get_mut("tools").and_then(Value::as_array_mut) {
                    *tools = flatten_spark_tools(tools, SAFE_TOOL_TYPES, UNSUPPORTED_HOSTED);
                }
                cleaned.push(next);
                continue;
            }
            let mut next = item.clone();
            if let Some(object) = next.as_object_mut() {
                object.remove("namespace");
            }
            cleaned.push(next);
        }
        *items = cleaned;
    }
    if let Some(object) = body.as_object_mut() {
        if object.get("parallel_tool_calls") == Some(&Value::Bool(true)) {
            object.insert("parallel_tool_calls".into(), Value::Bool(false));
        }
        if let Some(reasoning) = object.get_mut("reasoning").and_then(Value::as_object_mut) {
            reasoning.remove("context");
            reasoning.remove("summary");
            reasoning.remove("generate_summary");
        }
        if let Some(stream_options) = object
            .get_mut("stream_options")
            .and_then(Value::as_object_mut)
        {
            stream_options.remove("reasoning_summary_delivery");
            if stream_options.is_empty() {
                object.remove("stream_options");
            }
        }
    }
}

fn flatten_spark_tools(
    tools: &[Value],
    safe_types: &[&str],
    unsupported_hosted: &[&str],
) -> Vec<Value> {
    let mut flattened = Vec::new();
    for tool in tools {
        let tool_type = tool.get("type").and_then(Value::as_str).unwrap_or_default();
        if tool_type == "namespace" {
            if let Some(inner) = tool.get("tools").and_then(Value::as_array) {
                flattened.extend(flatten_spark_tools(inner, safe_types, unsupported_hosted));
            }
            continue;
        }
        if unsupported_hosted.contains(&tool_type) || !safe_types.contains(&tool_type) {
            continue;
        }
        let mut next = tool.clone();
        if let Some(object) = next.as_object_mut() {
            object.remove("defer_loading");
        }
        flattened.push(next);
    }
    flattened
}

fn normalize_function_tool_schemas(body: &mut Value) {
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            ensure_function_parameters_object(tool);
        }
    }
    if let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) {
        for item in items {
            if item.get("type").and_then(Value::as_str) != Some("additional_tools") {
                continue;
            }
            if let Some(tools) = item.get_mut("tools").and_then(Value::as_array_mut) {
                for tool in tools {
                    ensure_function_parameters_object(tool);
                }
            }
        }
    }
}
