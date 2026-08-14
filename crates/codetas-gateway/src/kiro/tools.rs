use super::*;
use super::eventstream::{is_limit_stop_reason, KiroUsage, ToolOutput};
use serde_json::json;

#[derive(Default)]
pub(crate) struct ToolAliases {
    used: BTreeSet<String>,
    pub(crate) reverse: BTreeMap<String, String>,
}

impl ToolAliases {
    pub(crate) fn alias(&mut self, original: &str) -> Result<String, String> {
        if original.is_empty() || original.len() > 512 || original.chars().any(char::is_control) {
            return Err("Kiro tool name is invalid".into());
        }
        if let Some((alias, _)) = self
            .reverse
            .iter()
            .find(|(_, value)| value.as_str() == original)
        {
            return Ok(alias.clone());
        }
        let cleaned = original
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                    character
                } else {
                    '_'
                }
            })
            .collect::<String>();
        let mut alias = if cleaned == original && cleaned.len() <= 64 {
            cleaned
        } else {
            let prefix = cleaned.chars().take(54).collect::<String>();
            format!(
                "{}_{}",
                if prefix.is_empty() { "tool" } else { &prefix },
                short_hash(original)
            )
        };
        let mut salt = 1_u64;
        while self.used.contains(&alias) {
            alias = format!("tool_{}_{}", short_hash(original), salt);
            salt = salt.saturating_add(1);
        }
        self.used.insert(alias.clone());
        self.reverse.insert(alias.clone(), original.to_string());
        Ok(alias)
    }
}

pub(crate) fn reserve_completion_tool(aliases: &mut ToolAliases) -> String {
    let mut name = KIRO_COMPLETION_TOOL_PREFIX.to_string();
    let mut suffix = 1_u16;
    while aliases.used.contains(&name) {
        name = format!("{KIRO_COMPLETION_TOOL_PREFIX}_{suffix}");
        suffix = suffix.saturating_add(1);
    }
    aliases.used.insert(name.clone());
    name
}

fn short_hash(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:08x}").chars().take(8).collect()
}

pub(crate) fn normalize_tool_id(value: &str) -> String {
    let value = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect::<String>();
    if value.is_empty() {
        format!("toolu_{}", Uuid::new_v4().simple())
    } else {
        value
    }
}

pub(crate) fn convert_tools(
    value: Option<&Value>,
    model: &str,
    aliases: &mut ToolAliases,
) -> Result<Vec<Value>, String> {
    let Some(tools) = value.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    if tools.len() > MAX_KIRO_CLIENT_TOOLS {
        return Err(
            "Kiro accepts at most 47 client tools when completion validation is enabled".into(),
        );
    }
    let description_limit = if normalize_model_id(model) == "gpt-5.6-sol" {
        9_216
    } else {
        1_024
    };
    let mut output = Vec::new();
    for tool in tools {
        if tool.get("type").and_then(Value::as_str) != Some("function") {
            return Err("Kiro supports function tools only".into());
        }
        let original = tool
            .get("name")
            .and_then(Value::as_str)
            .ok_or("Kiro function tool requires a name")?;
        let name = aliases.alias(original)?;
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or(original)
            .chars()
            .take(description_limit)
            .collect::<String>();
        let schema = sanitize_schema(tool.get("parameters").cloned().unwrap_or_else(|| json!({})));
        output.push(json!({
            "toolSpecification": {
                "name": name,
                "description": description,
                "inputSchema": {"json": ensure_object_schema(schema)},
            }
        }));
        if serde_json::to_vec(&output).is_ok_and(|bytes| bytes.len() > MAX_KIRO_TOOL_BYTES) {
            return Err("Kiro tool catalog exceeds the 96 KiB budget".into());
        }
    }
    Ok(output)
}

fn sanitize_schema(value: Value) -> Value {
    const REJECTED: &[&str] = &[
        "additionalProperties",
        "pattern",
        "format",
        "minLength",
        "maxLength",
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "minItems",
        "maxItems",
        "uniqueItems",
        "minProperties",
        "maxProperties",
        "$schema",
        "encrypted",
        "patternProperties",
        "propertyNames",
        "dependentSchemas",
        "dependentRequired",
        "if",
        "then",
        "else",
        "contains",
        "unevaluatedProperties",
        "unevaluatedItems",
    ];
    fn sanitize_schema_map(value: Value) -> Value {
        match value {
            Value::Object(values) => Value::Object(
                values
                    .into_iter()
                    .map(|(name, schema)| (name, sanitize_schema(schema)))
                    .collect(),
            ),
            other => sanitize_schema(other),
        }
    }
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(sanitize_schema).collect()),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .filter(|(key, value)| {
                    !REJECTED.contains(&key.as_str())
                        && !(key == "required" && value.as_array().is_some_and(Vec::is_empty))
                })
                .map(|(key, value)| {
                    let value = if matches!(key.as_str(), "properties" | "$defs" | "definitions") {
                        sanitize_schema_map(value)
                    } else {
                        sanitize_schema(value)
                    };
                    (key, value)
                })
                .collect(),
        ),
        value => value,
    }
}

fn ensure_object_schema(mut value: Value) -> Value {
    if !value.is_object() {
        return json!({"type": "object"});
    }
    let compositions = ["oneOf", "anyOf", "allOf"];
    let mut merged_properties = Map::new();
    let mut required = BTreeSet::new();
    if let Some(object) = value.as_object() {
        if let Some(properties) = object.get("properties").and_then(Value::as_object) {
            merged_properties.extend(properties.clone());
        }
        if let Some(values) = object.get("required").and_then(Value::as_array) {
            required.extend(values.iter().filter_map(Value::as_str).map(str::to_string));
        }
        for key in compositions {
            let Some(variants) = object.get(key).and_then(Value::as_array) else {
                continue;
            };
            for variant in variants {
                if let Some(properties) = variant.get("properties").and_then(Value::as_object) {
                    merged_properties.extend(properties.clone());
                }
                if key == "allOf" {
                    if let Some(values) = variant.get("required").and_then(Value::as_array) {
                        required
                            .extend(values.iter().filter_map(Value::as_str).map(str::to_string));
                    }
                }
            }
        }
    }
    if let Some(object) = value.as_object_mut() {
        for key in compositions {
            object.remove(key);
        }
        object.insert("type".into(), Value::String("object".into()));
        if !merged_properties.is_empty() {
            object.insert("properties".into(), Value::Object(merged_properties));
        }
        if !required.is_empty() {
            object.insert(
                "required".into(),
                Value::Array(required.into_iter().map(Value::String).collect()),
            );
        }
    }
    value
}

pub(crate) fn kiro_eventstream_to_response(
    bytes: &[u8],
    context: &KiroRequestContext,
    exposed_model: &str,
) -> Result<Value, String> {
    let frames = decode_eventstream(bytes)?;
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tools = Vec::<ToolOutput>::new();
    let mut usage = KiroUsage::default();
    let mut stop_reason = None::<String>;
    let mut active_tool = None::<usize>;
    let mut meaningful = false;
    for frame in frames {
        let event_type = frame
            .headers
            .get(":event-type")
            .map(String::as_str)
            .unwrap_or_default();
        let message_type = frame
            .headers
            .get(":message-type")
            .map(String::as_str)
            .unwrap_or_default();
        if message_type == "exception" || event_type == "error" {
            return Err("Kiro native stream reported an upstream error".into());
        }
        if !matches!(
            event_type,
            "assistantResponseEvent"
                | "reasoningContentEvent"
                | "toolUseEvent"
                | "metadataEvent"
                | "invalidStateEvent"
                | "messageMetadataEvent"
        ) {
            continue;
        }
        let value: Value = serde_json::from_slice(&frame.payload)
            .map_err(|_| format!("invalid Kiro {event_type} JSON payload"))?;
        if !value.is_object() {
            return Err(format!(
                "invalid Kiro {event_type} payload: expected an object"
            ));
        }
        match event_type {
            "assistantResponseEvent" => {
                if let Some(value) = value.get("content").and_then(Value::as_str) {
                    text.push_str(value);
                    meaningful |= !value.is_empty();
                }
            }
            "reasoningContentEvent" => {
                if let Some(value) = value.get("text").and_then(Value::as_str) {
                    reasoning.push_str(value);
                    meaningful |= !value.is_empty();
                }
            }
            "toolUseEvent" => {
                let explicit_id = value
                    .get("toolUseId")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(normalize_tool_id);
                let index = if let Some(id) = explicit_id {
                    tools
                        .iter()
                        .position(|tool| tool.id == id)
                        .unwrap_or_else(|| {
                            tools.push(ToolOutput {
                                id,
                                ..ToolOutput::default()
                            });
                            tools.len() - 1
                        })
                } else if let Some(index) = active_tool {
                    index
                } else {
                    tools.push(ToolOutput {
                        id: format!("toolu_{}", Uuid::new_v4().simple()),
                        ..ToolOutput::default()
                    });
                    tools.len() - 1
                };
                active_tool = Some(index);
                if let Some(name) = value.get("name").and_then(Value::as_str) {
                    tools[index].name = context
                        .tool_names
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| name.to_string());
                }
                if let Some(input) = value.get("input").and_then(Value::as_str) {
                    tools[index].arguments.push_str(input);
                }
                if value.get("stop").and_then(Value::as_bool) == Some(true) {
                    active_tool = None;
                }
                meaningful = true;
            }
            "metadataEvent" => {
                if let Some(token_usage) = value.get("tokenUsage") {
                    usage.update(token_usage)?;
                }
                stop_reason = value
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                meaningful = true;
            }
            "invalidStateEvent" => return Err("Kiro rejected the native conversation state".into()),
            "messageMetadataEvent" => {}
            _ => {}
        }
    }
    if !meaningful {
        return Err("Kiro native stream contained no recognized response events".into());
    }
    let mut completion_answer = None::<String>;
    if let Some(completion_tool) = context.completion_tool.as_deref() {
        let mut retained = Vec::new();
        for tool in tools {
            if tool.name == completion_tool {
                if completion_answer.is_some() {
                    return Err("Kiro returned the private final-answer tool more than once".into());
                }
                let arguments: Value = serde_json::from_str(if tool.arguments.trim().is_empty() {
                    "{}"
                } else {
                    &tool.arguments
                })
                .map_err(|_| "Kiro returned malformed private final-answer arguments")?;
                let answer = arguments
                    .get("answer")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or("Kiro private final-answer tool requires a non-empty answer")?;
                completion_answer = Some(answer.to_string());
            } else {
                retained.push(tool);
            }
        }
        tools = retained;
        if completion_answer.is_none() && tools.is_empty() {
            return Err("Kiro ended a tool-enabled turn without an explicit final answer or client tool call".into());
        }
        if completion_answer.is_some() && !tools.is_empty() {
            return Err("Kiro mixed a final answer with another tool call".into());
        }
    }
    let mut output = Vec::new();
    if !reasoning.is_empty() {
        output.push(json!({
            "id": format!("rs_{}", Uuid::new_v4().simple()),
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": reasoning}],
        }));
    }
    let visible_text = completion_answer.as_deref().unwrap_or(&text);
    if !visible_text.is_empty() && tools.is_empty() {
        output.push(json!({
            "id": format!("msg_{}", Uuid::new_v4().simple()),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": visible_text, "annotations": []}],
        }));
    }
    for tool in tools {
        if tool.name.is_empty() {
            return Err("Kiro returned a tool call without a name".into());
        }
        let arguments = if tool.arguments.trim().is_empty() {
            "{}".to_string()
        } else {
            serde_json::from_str::<Value>(&tool.arguments)
                .map_err(|_| "Kiro returned incomplete tool arguments")?;
            tool.arguments
        };
        output.push(json!({
            "id": format!("fc_{}", Uuid::new_v4().simple()),
            "type": "function_call",
            "status": "completed",
            "call_id": tool.id,
            "name": tool.name,
            "arguments": arguments,
        }));
    }
    let incomplete = stop_reason.as_deref().is_some_and(is_limit_stop_reason);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    Ok(json!({
        "id": context.response_id,
        "object": "response",
        "created_at": now,
        "completed_at": if incomplete { Value::Null } else { json!(now) },
        "status": if incomplete { "incomplete" } else { "completed" },
        "model": exposed_model,
        "output": output,
        "parallel_tool_calls": false,
        "usage": usage.to_responses(),
        "error": Value::Null,
        "incomplete_details": if incomplete {
            json!({"reason": stop_reason.unwrap_or_else(|| "provider_limit".into()).to_ascii_lowercase()})
        } else {
            Value::Null
        },
    }))
}

