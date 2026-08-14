use crate::config::{CredentialSource, ProviderDefinition, ResponseItemIdRepairSettings};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

const TOOL_NAME_PREFIX: &str = "cx_";
const REPEATED_FUNCTION_TOOL_LIMIT: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RepairableItemType {
    Message,
    Reasoning,
}

impl RepairableItemType {
    fn from_item(item: &Value) -> Option<Self> {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => Some(Self::Message),
            Some("reasoning") => Some(Self::Reasoning),
            _ => None,
        }
    }

    fn from_event(event_type: &str) -> Option<Self> {
        match event_type {
            "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.annotation.added"
            | "response.output_text.delta"
            | "response.output_text.done"
            | "response.refusal.delta"
            | "response.refusal.done" => Some(Self::Message),
            "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_text.delta"
            | "response.reasoning_text.done" => Some(Self::Reasoning),
            _ => None,
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Message => "msg_",
            Self::Reasoning => "rs_",
        }
    }
}

/// A request-local repair map for malformed Responses-compatible providers.
///
/// Only message and reasoning IDs are eligible. Function-call `id` and
/// `call_id` fields retain their upstream meaning and are never rewritten.
pub struct ResponsesItemIdRepair {
    settings: ResponseItemIdRepairSettings,
    message_ids: HashMap<usize, String>,
    reasoning_ids: HashMap<usize, String>,
    scope: String,
}

impl ResponsesItemIdRepair {
    pub fn new(settings: &ResponseItemIdRepairSettings) -> Option<Self> {
        if settings.message.is_empty()
            && settings.reasoning.is_empty()
            && !settings.repair_missing_terminal_ids
        {
            return None;
        }
        Some(Self {
            settings: settings.clone(),
            message_ids: HashMap::new(),
            reasoning_ids: HashMap::new(),
            scope: Uuid::new_v4().simple().to_string(),
        })
    }

    pub fn repair_response(&mut self, response: &mut Value) {
        let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) else {
            return;
        };
        for (output_index, item) in output.iter_mut().enumerate() {
            self.repair_output_item(output_index, item);
        }
    }

    pub fn repair_event(&mut self, event: &mut Value) {
        let output_index = event
            .get("output_index")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok());

        if let Some(output_index) = output_index {
            if let Some(item) = event.get_mut("item") {
                self.repair_output_item(output_index, item);
            }
            if let Some(item_type) = event
                .get("type")
                .and_then(Value::as_str)
                .and_then(RepairableItemType::from_event)
            {
                self.repair_event_item_id(output_index, item_type, event);
            }
        }
        if let Some(response) = event.get_mut("response") {
            self.repair_response(response);
        }
    }

    fn repair_output_item(&mut self, output_index: usize, item: &mut Value) {
        let Some(item_type) = RepairableItemType::from_item(item) else {
            return;
        };
        let current = item.get("id").and_then(Value::as_str).map(str::to_string);
        let mapped = if let Some(existing) = self.mapped_id(item_type, output_index) {
            existing.to_string()
        } else {
            let Some(current) = current.as_deref() else {
                return;
            };
            let mapped = if self.is_placeholder(item_type, current) {
                format!(
                    "{}codetas_{}_{}",
                    item_type.prefix(),
                    self.scope,
                    output_index
                )
            } else if self.settings.repair_missing_terminal_ids {
                current.to_string()
            } else {
                return;
            };
            self.remember_id(item_type, output_index, mapped.clone());
            mapped
        };

        if current.as_deref() == Some(mapped.as_str()) {
            return;
        }
        if current.is_none() && !self.settings.repair_missing_terminal_ids {
            return;
        }
        if let Some(object) = item.as_object_mut() {
            object.insert("id".into(), Value::String(mapped));
        }
    }

    fn repair_event_item_id(
        &self,
        output_index: usize,
        item_type: RepairableItemType,
        event: &mut Value,
    ) {
        let Some(mapped) = self.mapped_id(item_type, output_index) else {
            return;
        };
        let current = event.get("item_id").and_then(Value::as_str);
        if current == Some(mapped)
            || (current.is_none() && !self.settings.repair_missing_terminal_ids)
        {
            return;
        }
        if let Some(object) = event.as_object_mut() {
            object.insert("item_id".into(), Value::String(mapped.to_string()));
        }
    }

    fn is_placeholder(&self, item_type: RepairableItemType, value: &str) -> bool {
        match item_type {
            RepairableItemType::Message => self.settings.message.iter().any(|id| id == value),
            RepairableItemType::Reasoning => self.settings.reasoning.iter().any(|id| id == value),
        }
    }

    fn mapped_id(&self, item_type: RepairableItemType, output_index: usize) -> Option<&str> {
        match item_type {
            RepairableItemType::Message => self.message_ids.get(&output_index),
            RepairableItemType::Reasoning => self.reasoning_ids.get(&output_index),
        }
        .map(String::as_str)
    }

    fn remember_id(&mut self, item_type: RepairableItemType, output_index: usize, value: String) {
        match item_type {
            RepairableItemType::Message => &mut self.message_ids,
            RepairableItemType::Reasoning => &mut self.reasoning_ids,
        }
        .insert(output_index, value);
    }
}

pub fn escape_anthropic_tool_names(body: &mut Value) -> Result<(), String> {
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            escape_name_field(tool, "name")?;
        }
    }
    if let Some(choice) = body.get_mut("tool_choice") {
        escape_name_field(choice, "name")?;
    }
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) else {
                continue;
            };
            for part in parts {
                if part.get("type").and_then(Value::as_str) == Some("tool_use") {
                    escape_name_field(part, "name")?;
                }
            }
        }
    }
    Ok(())
}

pub fn restore_anthropic_tool_names(value: &mut Value) {
    if let Some(blocks) = value.get_mut("content").and_then(Value::as_array_mut) {
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                restore_name_field(block, "name");
            }
        }
    }
}

pub fn restore_anthropic_stream_tool_names(value: &mut Value) {
    if value.get("type").and_then(Value::as_str) == Some("content_block_start") {
        if let Some(block) = value.get_mut("content_block") {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                restore_name_field(block, "name");
            }
        }
    }
}

fn escape_name_field(value: &mut Value, field: &str) -> Result<(), String> {
    let Some(name) = value.get(field).and_then(Value::as_str) else {
        return Ok(());
    };
    let escaped = format!("{TOOL_NAME_PREFIX}{name}");
    if escaped.len() > 64 {
        return Err(format!(
            "tool name {name} is too long for reversible provider escaping"
        ));
    }
    if let Some(object) = value.as_object_mut() {
        object.insert(field.into(), Value::String(escaped));
    }
    Ok(())
}

fn restore_name_field(value: &mut Value, field: &str) {
    let Some(name) = value.get(field).and_then(Value::as_str) else {
        return;
    };
    let Some(restored) = name.strip_prefix(TOOL_NAME_PREFIX) else {
        return;
    };
    let restored = restored.to_string();
    if let Some(object) = value.as_object_mut() {
        object.insert(field.into(), Value::String(restored));
    }
}

pub fn is_codetas_repaired_item_id(value: &str) -> bool {
    value.starts_with("msg_codetas_") || value.starts_with("rs_codetas_")
}

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

pub fn is_zen_chat_endpoint(base_url: &str) -> bool {
    let base = base_url.trim().trim_end_matches('/');
    base == "https://opencode.ai/zen/v1" || base == "https://opencode.ai/zen/go/v1"
}

pub fn is_kimi_chat_endpoint(base_url: &str) -> bool {
    url::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == "api.kimi.com" || host.ends_with(".kimi.com"))
}

pub fn is_xai_chat_endpoint(base_url: &str) -> bool {
    url::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == "api.x.ai" || host == "cli-chat-proxy.grok.com")
}

pub fn ensure_chat_function_parameters(body: &mut Map<String, Value>) {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools {
        let Some(function) = tool.get_mut("function") else {
            continue;
        };
        let needs_type = !function
            .pointer("/parameters/type")
            .and_then(Value::as_str)
            .is_some_and(|value| value == "object");
        if !needs_type {
            continue;
        }
        let mut parameters = match function.get("parameters") {
            Some(Value::Object(object)) => object.clone(),
            _ => Map::new(),
        };
        parameters.insert("type".into(), Value::String("object".into()));
        if let Some(object) = function.as_object_mut() {
            object.insert("parameters".into(), Value::Object(parameters));
        }
    }
}

pub fn sanitize_kimi_chat_tools(body: &mut Map<String, Value>) {
    ensure_chat_function_parameters(body);
}

pub fn sanitize_xai_chat_tools(body: &mut Map<String, Value>) {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools {
        let Some(function) = tool.get_mut("function").and_then(Value::as_object_mut) else {
            continue;
        };
        let parameters = function.remove("parameters").unwrap_or_else(|| json!({}));
        function.insert(
            "parameters".into(),
            ensure_zen_root_object_schema(parameters),
        );
    }
}

pub fn sanitize_zen_chat_tools(body: &mut Map<String, Value>) {
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            let Some(function) = tool.get_mut("function").and_then(Value::as_object_mut) else {
                continue;
            };
            let parameters = function.remove("parameters").unwrap_or_else(|| json!({}));
            function.insert(
                "parameters".into(),
                ensure_zen_root_object_schema(parameters),
            );
        }
    }
}

fn ensure_zen_root_object_schema(schema: Value) -> Value {
    let Value::Object(mut object) = sanitize_zen_schema_value(schema) else {
        return json!({"type": "object"});
    };
    let has_composition = ["oneOf", "anyOf", "allOf"]
        .iter()
        .any(|key| object.get(*key).is_some_and(Value::is_array));
    if !has_composition {
        if object.get("type").and_then(Value::as_str) != Some("object") {
            object.insert("type".into(), Value::String("object".into()));
        }
        return Value::Object(object);
    }

    let mut properties = Map::new();
    if let Some(Value::Object(existing)) = object.get("properties") {
        properties = existing.clone();
    }
    let mut required = Vec::new();
    if let Some(Value::Array(existing)) = object.get("required") {
        required = existing
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
    }
    for key in ["oneOf", "anyOf", "allOf"] {
        let Some(Value::Array(variants)) = object.get(key) else {
            continue;
        };
        let merge_required = key == "allOf";
        for variant in variants {
            let Some(variant) = variant.as_object() else {
                continue;
            };
            if let Some(Value::Object(props)) = variant.get("properties") {
                for (name, value) in props {
                    properties.insert(name.clone(), value.clone());
                }
            }
            if merge_required {
                if let Some(Value::Array(values)) = variant.get("required") {
                    for name in values.iter().filter_map(Value::as_str) {
                        if !required.iter().any(|existing| existing == name) {
                            required.push(name.to_string());
                        }
                    }
                }
            }
        }
    }
    object.remove("oneOf");
    object.remove("anyOf");
    object.remove("allOf");
    object.insert("type".into(), Value::String("object".into()));
    if !properties.is_empty() {
        object.insert("properties".into(), Value::Object(properties));
    }
    if !required.is_empty() {
        object.insert(
            "required".into(),
            Value::Array(required.into_iter().map(Value::String).collect()),
        );
    }
    Value::Object(object)
}

fn sanitize_zen_schema_value(value: Value) -> Value {
    match value {
        Value::Array(items) => {
            Value::Array(items.into_iter().map(sanitize_zen_schema_value).collect())
        }
        Value::Object(object) => {
            let mut out = Map::new();
            for (key, child) in object {
                if key == "encrypted" {
                    continue;
                }
                if key == "required" && child.as_array().is_some_and(Vec::is_empty) {
                    continue;
                }
                if key == "type" {
                    if let Value::Array(types) = child {
                        let nullable = types.iter().any(|entry| entry.as_str() == Some("null"));
                        if let Some(first) = types
                            .iter()
                            .find(|entry| entry.as_str() != Some("null"))
                            .cloned()
                        {
                            out.insert("type".into(), first);
                        }
                        if nullable {
                            out.insert("nullable".into(), Value::Bool(true));
                        }
                        continue;
                    }
                    out.insert(key, child);
                    continue;
                }
                if matches!(key.as_str(), "properties" | "$defs" | "definitions") {
                    if let Value::Object(map) = child {
                        let cleaned = map
                            .into_iter()
                            .map(|(name, value)| (name, sanitize_zen_schema_value(value)))
                            .collect();
                        out.insert(key, Value::Object(cleaned));
                        continue;
                    }
                }
                out.insert(key, sanitize_zen_schema_value(child));
            }
            Value::Object(out)
        }
        other => other,
    }
}

fn ensure_function_parameters_object(tool: &mut Value) {
    if tool.get("type").and_then(Value::as_str) != Some("function") {
        return;
    }
    let needs_type = !tool
        .pointer("/parameters/type")
        .and_then(Value::as_str)
        .is_some_and(|value| value == "object");
    if !needs_type {
        return;
    }
    let mut parameters = match tool.get("parameters") {
        Some(Value::Object(object)) => object.clone(),
        _ => Map::new(),
    };
    parameters.insert("type".into(), Value::String("object".into()));
    if let Some(object) = tool.as_object_mut() {
        object.insert("parameters".into(), Value::Object(parameters));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderDefinition;

    fn chatgpt_provider() -> ProviderDefinition {
        ProviderDefinition {
            id: "openai".into(),
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            credential: crate::config::ProviderCredential {
                source: CredentialSource::Forward,
                ..crate::config::ProviderCredential::default()
            },
            ..ProviderDefinition::default()
        }
    }

    fn api_key_provider() -> ProviderDefinition {
        ProviderDefinition {
            id: "openai-apikey".into(),
            base_url: "https://api.openai.com/v1".into(),
            ..ProviderDefinition::default()
        }
    }

    #[test]
    fn chatgpt_forward_strips_fields_that_400_the_codex_backend() {
        let provider = chatgpt_provider();
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "previous_response_id": "resp_123",
            "metadata": {"session": "abc"},
            "max_output_tokens": 4096,
            "input": [{
                "type": "message",
                "id": "msg_1",
                "role": "user",
                "content": [{"type": "input_text", "text": "hi"}]
            }]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
        assert!(body.get("previous_response_id").is_none());
        assert!(body.get("metadata").is_none());
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(body["input"][0]["id"], "msg_1");
    }

    fn repeated_function_history(count: usize) -> Value {
        let mut input = vec![json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "Review the changes"}]
        })];
        for index in 0..count {
            let call_id = format!("call_{index}");
            input.push(json!({
                "type": "function_call",
                "call_id": call_id,
                "name": "update_plan",
                "arguments": "{\"plan\":[]}"
            }));
            input.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": "Plan updated"
            }));
        }
        json!({
            "tools": [
                {"type": "function", "name": "update_plan", "parameters": {}},
                {"type": "function", "name": "exec_command", "parameters": {}}
            ],
            "tool_choice": {"type": "function", "name": "update_plan"},
            "input": input
        })
    }

    #[test]
    fn repeated_function_loop_removes_only_the_repeated_tool() {
        let mut body = repeated_function_history(REPEATED_FUNCTION_TOOL_LIMIT);
        assert_eq!(
            guard_repeated_function_tool_loop(&mut body).as_deref(),
            Some("update_plan")
        );
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["tools"][0]["name"], "exec_command");
        assert!(body.get("tool_choice").is_none());
        let warning = body["input"].as_array().unwrap().last().unwrap();
        assert_eq!(warning["role"], "user");
        assert!(warning["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Do not call that tool again"));
    }

    #[test]
    fn repeated_function_loop_allows_normal_retries() {
        let mut body = repeated_function_history(REPEATED_FUNCTION_TOOL_LIMIT - 1);
        assert_eq!(guard_repeated_function_tool_loop(&mut body), None);
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
        assert!(body.get("tool_choice").is_some());
    }

    #[test]
    fn repeated_function_loop_resets_after_new_user_input() {
        let mut body = repeated_function_history(REPEATED_FUNCTION_TOOL_LIMIT);
        body["input"].as_array_mut().unwrap().push(json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "Try the plan again"}]
        }));
        assert_eq!(guard_repeated_function_tool_loop(&mut body), None);
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn repeated_function_loop_allows_different_arguments() {
        let mut body = repeated_function_history(REPEATED_FUNCTION_TOOL_LIMIT);
        for (index, item) in body["input"].as_array_mut().unwrap().iter_mut().enumerate() {
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                item["arguments"] = Value::String(format!("{{\"step\":{index}}}"));
            }
        }
        assert_eq!(guard_repeated_function_tool_loop(&mut body), None);
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn chatgpt_forward_empties_raw_reasoning_content() {
        let provider = chatgpt_provider();
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "input": [{
                "type": "reasoning",
                "id": "rs_1",
                "content": [{"type": "reasoning_text", "text": "internal thoughts"}],
                "encrypted_content": "ocxr1:AAAA"
            }]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
        assert_eq!(body["input"][0]["content"], json!([]));
        assert!(body["input"][0].get("encrypted_content").is_none());
        assert_eq!(body["input"][0]["id"], "rs_1");
    }

    #[test]
    fn chatgpt_forward_repairs_orphaned_tool_output_after_replay_miss() {
        let provider = chatgpt_provider();
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "previous_response_id": "resp_missing",
            "input": [{
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "pwd=/tmp"
            }]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
        assert!(body.get("previous_response_id").is_none());
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "user");
        assert!(body["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("pwd=/tmp"));
    }

    #[test]
    fn chatgpt_forward_repairs_orphaned_tool_search_output() {
        let provider = chatgpt_provider();
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "input": [{
                "type": "tool_search_output",
                "call_id": "call_missing",
                "tools": [{"name": "exec_command"}]
            }]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "user");
        assert!(body["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("exec_command"));
    }

    #[test]
    fn chatgpt_forward_keeps_paired_tool_search_output() {
        let provider = chatgpt_provider();
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "input": [
                {
                    "type": "tool_search_call",
                    "call_id": "call_1",
                    "arguments": {"query": "shell"}
                },
                {
                    "type": "tool_search_output",
                    "call_id": "call_1",
                    "tools": [{"name": "exec_command"}]
                }
            ]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
        assert_eq!(body["input"][0]["type"], "tool_search_call");
        assert_eq!(body["input"][1]["type"], "tool_search_output");
    }

    #[test]
    fn api_key_provider_repairs_orphaned_tool_search_output_without_previous_response() {
        let provider = api_key_provider();
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "input": [{
                "type": "tool_search_output",
                "call_id": "call_missing",
                "tools": [{"name": "exec_command"}]
            }]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
        assert_eq!(body["input"][0]["type"], "message");
        assert!(body["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("exec_command"));
    }

    #[test]
    fn api_key_provider_keeps_remote_tool_output_with_previous_response() {
        let provider = api_key_provider();
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "previous_response_id": "resp_123",
            "input": [{
                "type": "tool_search_output",
                "call_id": "call_remote",
                "tools": [{"name": "exec_command"}]
            }]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
        assert_eq!(body["previous_response_id"], "resp_123");
        assert_eq!(body["input"][0]["type"], "tool_search_output");
    }

    #[test]
    fn chatgpt_forward_shortens_oversized_call_ids() {
        let provider = chatgpt_provider();
        let long_id = format!("call_{}", "x".repeat(80));
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "input": [
                {"type": "function_call", "call_id": long_id.clone(), "name": "exec", "arguments": "{}"},
                {"type": "function_call_output", "call_id": long_id, "output": "ok"}
            ]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
        let first = body["input"][0]["call_id"].as_str().unwrap();
        let second = body["input"][1]["call_id"].as_str().unwrap();
        assert_eq!(first, second);
        assert!(first.len() <= 64);
        assert!(first.starts_with("call_cdt_"));
    }

    #[test]
    fn store_false_strips_item_ids() {
        let provider = chatgpt_provider();
        let mut body = json!({
            "store": false,
            "input": [{"type": "message", "id": "msg_abc", "role": "user", "content": []}]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
        assert!(body["input"][0].get("id").is_none());
    }

    #[test]
    fn api_key_providers_keep_stateful_fields() {
        let provider = api_key_provider();
        let mut body = json!({
            "previous_response_id": "resp_123",
            "metadata": {"k": "v"},
            "max_output_tokens": 128,
            "input": [{
                "type": "reasoning",
                "id": "rs_1",
                "content": [{"type": "reasoning_text", "text": "keep summaries only"}]
            }]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.4");
        assert_eq!(body["previous_response_id"], "resp_123");
        assert_eq!(body["metadata"]["k"], "v");
        assert_eq!(body["max_output_tokens"], 128);
        assert_eq!(body["input"][0]["content"], json!([]));
    }

    #[test]
    fn host_detectors_cover_kimi_and_xai() {
        assert!(is_kimi_chat_endpoint("https://api.kimi.com/coding/v1"));
        assert!(is_xai_chat_endpoint("https://api.x.ai/v1"));
        assert!(!is_kimi_chat_endpoint("https://opencode.ai/zen/go/v1"));
        assert!(is_zen_chat_endpoint("https://opencode.ai/zen/go/v1"));
    }

    #[test]
    fn chat_function_parameters_gain_object_type() {
        let mut body = json!({
            "tools": [{
                "type": "function",
                "function": {"name": "exec", "parameters": {"properties": {"cmd": {"type": "string"}}}}
            }]
        });
        ensure_chat_function_parameters(body.as_object_mut().unwrap());
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn zen_tools_flatten_oneof_and_drop_encrypted() {
        let mut body = json!({
            "tools": [{
                "type": "function",
                "function": {
                    "name": "exec",
                    "parameters": {
                        "oneOf": [
                            {"properties": {"command": {"type": "string"}}, "required": ["command"]},
                            {"properties": {"script": {"type": ["string", "null"]}}}
                        ],
                        "encrypted": true,
                        "required": []
                    }
                }
            }]
        });
        let object = body.as_object_mut().unwrap();
        sanitize_zen_chat_tools(object);
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
        assert!(body["tools"][0]["function"]["parameters"]
            .get("oneOf")
            .is_none());
        assert!(body["tools"][0]["function"]["parameters"]
            .get("encrypted")
            .is_none());
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["properties"]["command"]["type"],
            "string"
        );
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["properties"]["script"]["nullable"],
            true
        );
    }

    #[test]
    fn spark_drops_hosted_image_generation() {
        let provider = chatgpt_provider();
        let mut body = json!({
            "model": "gpt-5.3-codex-spark",
            "tools": [
                {"type": "image_generation"},
                {"type": "function", "name": "exec", "parameters": {}}
            ],
            "input": []
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.3-codex-spark");
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["parameters"]["type"], "object");
    }
}
