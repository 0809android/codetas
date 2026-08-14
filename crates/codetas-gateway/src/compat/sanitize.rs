use super::*;
use super::chat_tools::ensure_function_parameters_object;

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
/// `previous_response_id`, `metadata`, `max_output_tokens`, raw reasoning
/// content, or orphaned tool outputs produces HTTP 400 and breaks native Codex.
pub fn uses_chatgpt_codex_backend(provider: &ProviderDefinition) -> bool {
    let base = provider.base_url.to_ascii_lowercase();
    provider.credential.source == CredentialSource::Forward
        || base.contains("chatgpt.com/backend-api")
        || base.contains("/backend-api/codex")
}

pub fn sanitize_responses_upstream_request(
    body: &mut Value,
    provider: &ProviderDefinition,
    model: &str,
) {
    let chatgpt = uses_chatgpt_codex_backend(provider);
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

    if chatgpt || unexpanded_miss || stateless {
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
        }
    }
    if chatgpt || stateless {
        repair_orphaned_input_items(body, chatgpt && unexpanded_miss);
    }
    if chatgpt || unexpanded_miss {
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
/// to one function.
///
/// The repeated function is removed from the next request while all other
/// tools remain available. A synthetic user message tells the model to
/// continue without calling the blocked function again. This lets native
/// Codex resume the same turn instead of accumulating tool calls until the
/// context window is exhausted.
pub fn guard_repeated_function_tool_loop(body: &mut Value) -> Option<String> {
    let repeated_name = repeated_function_tool_name(body)?;

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
        items.push(json!({
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": format!(
                    "CODETAS stopped a repeated tool loop: `{repeated_name}` already completed \
                     successfully at least {REPEATED_FUNCTION_TOOL_LIMIT} times in succession. \
                     Do not call that tool again in this turn. Continue the requested work using \
                     another available tool or provide the final result."
                )
            }]
        }));
    }

    let remove_tool_choice = body
        .get("tool_choice")
        .and_then(Value::as_object)
        .and_then(|choice| {
            choice
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| {
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

    Some(repeated_name)
}

fn repeated_function_tool_name(body: &Value) -> Option<String> {
    let items = body.get("input").and_then(Value::as_array)?;
    let mut calls = HashMap::<String, (String, String)>::new();
    let mut completed_calls = Vec::<(String, String)>::new();

    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("message") if item.get("role").and_then(Value::as_str) == Some("user") => {
                calls.clear();
                completed_calls.clear();
            }
            Some("function_call" | "local_shell_call") => {
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
                calls.insert(call_id.to_string(), (name.to_string(), arguments));
            }
            Some("function_call_output" | "local_shell_call_output") => {
                let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
                    continue;
                };
                let completed = item
                    .get("output")
                    .is_some_and(|output| match output {
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
    (repeated >= REPEATED_FUNCTION_TOOL_LIMIT).then_some(last.0)
}

fn canonical_tool_arguments(arguments: &Value) -> String {
    match arguments {
        Value::String(text) => serde_json::from_str::<Value>(text)
            .map(|value| value.to_string())
            .unwrap_or_else(|_| text.trim().to_string()),
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
        let is_fn_output = item_type == "function_call_output";
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
