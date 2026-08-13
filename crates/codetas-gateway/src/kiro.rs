use crate::compaction::decode_summary;
use crate::translate::tool_input_to_value;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

const MAX_KIRO_IMAGES: usize = 20;
const MAX_KIRO_IMAGE_BASE64_BYTES: usize = 18 * 1024 * 1024;
const MAX_KIRO_CLIENT_TOOLS: usize = 47;
const MAX_KIRO_TOOL_BYTES: usize = 96_000;
const KIRO_COMPLETION_TOOL_PREFIX: &str = "codetas_private_final_answer";
const MAX_EVENTSTREAM_FRAME: usize = 16 * 1024 * 1024;
const MAX_EVENTSTREAM_HEADERS: usize = 128 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct KiroRequestContext {
    pub conversation_id: String,
    pub response_id: String,
    pub streaming: bool,
    pub tool_names: BTreeMap<String, String>,
    pub completion_tool: Option<String>,
}

pub(crate) struct KiroStreamDecoder {
    pending: Vec<u8>,
    context: KiroRequestContext,
    exposed_model: String,
    message_id: String,
    text: String,
    reasoning: String,
    usage: KiroUsage,
    stop_reason: Option<String>,
    meaningful: bool,
    message_started: bool,
    sequence: u64,
}

impl KiroStreamDecoder {
    pub(crate) fn new(context: KiroRequestContext, exposed_model: String) -> Self {
        Self {
            pending: Vec::new(),
            context,
            exposed_model,
            message_id: format!("msg_{}", Uuid::new_v4().simple()),
            text: String::new(),
            reasoning: String::new(),
            usage: KiroUsage::default(),
            stop_reason: None,
            meaningful: false,
            message_started: false,
            sequence: 0,
        }
    }

    pub(crate) fn created_event(&mut self) -> Value {
        let response = self.response_value("in_progress", Vec::new(), None);
        self.event("response.created", json!({"response": response}))
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>, String> {
        let frames = drain_eventstream_frames(&mut self.pending, bytes)?;
        let mut events = Vec::new();
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
                    let Some(delta) = value.get("content").and_then(Value::as_str) else {
                        continue;
                    };
                    if delta.is_empty() {
                        continue;
                    }
                    if !self.message_started {
                        self.message_started = true;
                        events.push(self.event(
                            "response.output_item.added",
                            json!({
                                "output_index": 0,
                                "item": {
                                    "id": self.message_id.clone(),
                                    "type": "message",
                                    "status": "in_progress",
                                    "role": "assistant",
                                    "content": [],
                                }
                            }),
                        ));
                        events.push(self.event(
                            "response.content_part.added",
                            json!({
                                "item_id": self.message_id.clone(),
                                "output_index": 0,
                                "content_index": 0,
                                "part": {"type": "output_text", "text": "", "annotations": []},
                            }),
                        ));
                    }
                    self.text.push_str(delta);
                    events.push(self.event(
                        "response.output_text.delta",
                        json!({
                            "item_id": self.message_id.clone(),
                            "output_index": 0,
                            "content_index": 0,
                            "delta": delta,
                        }),
                    ));
                    self.meaningful = true;
                }
                "reasoningContentEvent" => {
                    if let Some(delta) = value.get("text").and_then(Value::as_str) {
                        self.reasoning.push_str(delta);
                        self.meaningful |= !delta.is_empty();
                    }
                }
                "metadataEvent" => {
                    if let Some(token_usage) = value.get("tokenUsage") {
                        self.usage.update(token_usage)?;
                    }
                    self.stop_reason = value
                        .get("stopReason")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    self.meaningful = true;
                }
                "toolUseEvent" => {
                    return Err(
                        "Kiro returned a tool call when no client tools were enabled".into(),
                    )
                }
                "invalidStateEvent" => {
                    return Err("Kiro rejected the native conversation state".into())
                }
                "messageMetadataEvent" => {}
                _ => {}
            }
        }
        Ok(events)
    }

    pub(crate) fn finish(&mut self) -> Result<(Vec<Value>, Value), String> {
        if !self.pending.is_empty() {
            return Err("Kiro eventstream ended with a truncated frame".into());
        }
        if !self.meaningful {
            return Err("Kiro native stream contained no recognized response events".into());
        }
        let mut events = Vec::new();
        let mut output = Vec::new();
        if self.message_started {
            let message = json!({
                "id": self.message_id.clone(),
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": self.text.clone(), "annotations": []}],
            });
            events.push(self.event(
                "response.output_text.done",
                json!({
                    "item_id": self.message_id.clone(),
                    "output_index": 0,
                    "content_index": 0,
                    "text": self.text.clone(),
                }),
            ));
            events.push(self.event(
                "response.content_part.done",
                json!({
                    "item_id": self.message_id.clone(),
                    "output_index": 0,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": self.text.clone(), "annotations": []},
                }),
            ));
            events.push(self.event(
                "response.output_item.done",
                json!({"output_index": 0, "item": message.clone()}),
            ));
            output.push(message);
        }
        if !self.reasoning.is_empty() {
            let index = output.len();
            let reasoning = json!({
                "id": format!("rs_{}", Uuid::new_v4().simple()),
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": self.reasoning.clone()}],
            });
            events.push(self.event(
                "response.output_item.added",
                json!({"output_index": index, "item": reasoning.clone()}),
            ));
            events.push(self.event(
                "response.output_item.done",
                json!({"output_index": index, "item": reasoning.clone()}),
            ));
            output.push(reasoning);
        }
        let incomplete = self
            .stop_reason
            .as_deref()
            .is_some_and(is_limit_stop_reason);
        let status = if incomplete {
            "incomplete"
        } else {
            "completed"
        };
        let response = self.response_value(status, output, self.stop_reason.as_deref());
        events.push(self.event(
            if incomplete {
                "response.incomplete"
            } else {
                "response.completed"
            },
            json!({"response": response.clone()}),
        ));
        Ok((events, response))
    }

    fn response_value(&self, status: &str, output: Vec<Value>, reason: Option<&str>) -> Value {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        json!({
            "id": self.context.response_id.clone(),
            "object": "response",
            "created_at": now,
            "completed_at": if status == "completed" { json!(now) } else { Value::Null },
            "status": status,
            "model": self.exposed_model.clone(),
            "output": output,
            "parallel_tool_calls": false,
            "usage": self.usage.to_responses(),
            "error": Value::Null,
            "incomplete_details": if status == "incomplete" {
                json!({"reason": reason.unwrap_or("provider_limit").to_ascii_lowercase()})
            } else {
                Value::Null
            },
        })
    }

    fn event(&mut self, event_type: &str, fields: Value) -> Value {
        let mut event = fields.as_object().cloned().unwrap_or_default();
        event.insert("type".into(), Value::String(event_type.into()));
        event.insert("sequence_number".into(), json!(self.sequence));
        self.sequence = self.sequence.saturating_add(1);
        Value::Object(event)
    }
}

#[derive(Debug)]
enum Turn {
    User {
        content: String,
        images: Vec<Value>,
        tool_results: Vec<Value>,
    },
    Assistant {
        content: String,
        tool_uses: Vec<Value>,
    },
}

pub(crate) fn responses_to_kiro(
    body: &Value,
    model: &str,
    profile_arn: Option<&str>,
) -> Result<(Value, KiroRequestContext), String> {
    let object = body
        .as_object()
        .ok_or_else(|| "Responses request must be a JSON object".to_string())?;
    if object.get("parallel_tool_calls").and_then(Value::as_bool) == Some(true) {
        return Err("Kiro does not support parallel tool calls".into());
    }
    if let Some(choice) = object.get("tool_choice").filter(|value| !value.is_null()) {
        if !matches!(choice.as_str(), Some("auto" | "none")) {
            return Err("Kiro supports only tool_choice auto or none".into());
        }
    }

    let conversation_id = object
        .get("previous_response_id")
        .and_then(Value::as_str)
        .and_then(conversation_id_from_response_id)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let response_id = format!(
        "resp_kiro_{}_{}",
        conversation_id.replace('-', ""),
        Uuid::new_v4().simple()
    );
    let mut aliases = ToolAliases::default();
    let tools_enabled = object.get("tool_choice").and_then(Value::as_str) != Some("none");
    let mut tools = if tools_enabled {
        convert_tools(object.get("tools"), model, &mut aliases)?
    } else {
        Vec::new()
    };
    let completion_tool = if tools.is_empty() {
        None
    } else {
        let name = reserve_completion_tool(&mut aliases);
        tools.push(json!({
            "toolSpecification": {
                "name": name.clone(),
                "description": "Return the complete final answer only after the task is finished.",
                "inputSchema": {"json": {
                    "type": "object",
                    "properties": {"answer": {"type": "string"}},
                    "required": ["answer"]
                }}
            }
        }));
        if serde_json::to_vec(&tools).is_ok_and(|bytes| bytes.len() > MAX_KIRO_TOOL_BYTES) {
            return Err(
                "Kiro tool catalog plus completion control exceeds the 96 KiB budget".into(),
            );
        }
        Some(name)
    };
    let mut turns = Vec::<Turn>::new();
    let mut prior_calls = BTreeSet::<String>::new();
    let mut prior_results = BTreeSet::<String>::new();
    match object.get("input") {
        Some(Value::String(text)) => push_user(&mut turns, text.clone(), Vec::new(), Vec::new()),
        Some(Value::Array(items)) => {
            for item in items {
                let Some(item) = item.as_object() else {
                    return Err("Kiro input items must be objects".into());
                };
                match item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("message")
                {
                    "message" => {
                        let (text, images) = kiro_content(item.get("content"))?;
                        if item.get("role").and_then(Value::as_str) == Some("assistant") {
                            push_assistant(&mut turns, text, Vec::new());
                        } else {
                            push_user(&mut turns, text, images, Vec::new());
                        }
                    }
                    "function_call" | "custom_tool_call" => {
                        let original = item
                            .get("name")
                            .and_then(Value::as_str)
                            .ok_or("Kiro function call requires a name")?;
                        let name = aliases.alias(original)?;
                        let id = normalize_tool_id(
                            item.get("call_id")
                                .or_else(|| item.get("id"))
                                .and_then(Value::as_str)
                                .unwrap_or("tool_unknown"),
                        );
                        if !prior_calls.insert(id.clone()) {
                            return Err(format!(
                                "Kiro history contains a duplicate tool call ID: {id}"
                            ));
                        }
                        // custom_tool_call items carry the payload in `input`
                        // while function_call uses `arguments`. A structured
                        // payload is preserved as-is instead of collapsing to {}.
                        let input = match item
                            .get("input")
                            .or_else(|| item.get("arguments"))
                            .map(tool_input_to_value)
                        {
                            Some(value) if value.is_object() => value,
                            Some(Value::String(text)) => json!({"input": text}),
                            Some(value) => json!({"input": value}),
                            None => json!({}),
                        };
                        push_assistant(
                            &mut turns,
                            String::new(),
                            vec![json!({"name": name, "input": input, "toolUseId": id})],
                        );
                    }
                    "function_call_output" | "custom_tool_call_output" => {
                        let id = normalize_tool_id(
                            item.get("call_id")
                                .and_then(Value::as_str)
                                .ok_or("Kiro tool output requires call_id")?,
                        );
                        if !prior_calls.contains(&id) {
                            return Err(format!(
                                "Kiro history contains an orphaned tool result: {id}"
                            ));
                        }
                        if !prior_results.insert(id.clone()) {
                            return Err(format!(
                                "Kiro history contains a duplicate tool result: {id}"
                            ));
                        }
                        let output = item.get("output").cloned().unwrap_or(Value::Null);
                        let text = output
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| output.to_string());
                        push_user(
                            &mut turns,
                            String::new(),
                            Vec::new(),
                            vec![json!({
                                "content": [{"text": if text.trim().is_empty() { "The tool completed without textual output." } else { &text }}],
                                "status": "success",
                                "toolUseId": id,
                            })],
                        );
                    }
                    "reasoning" => {}
                    "compaction" => {
                        if let Ok(Some(summary)) = decode_summary(&Value::Object(item.clone())) {
                            push_user(&mut turns, summary, Vec::new(), Vec::new());
                        }
                    }
                    _ => {}
                }
            }
        }
        Some(Value::Null) | None => {}
        Some(_) => return Err("Responses input must be a string or array".into()),
    }

    if let Some(id) = prior_calls.difference(&prior_results).next() {
        return Err(format!(
            "Kiro history contains a tool call without a matching result: {id}"
        ));
    }

    if turns.is_empty() || matches!(turns.first(), Some(Turn::Assistant { .. })) {
        turns.insert(
            0,
            Turn::User {
                content: "Continue the requested task.".into(),
                images: Vec::new(),
                tool_results: Vec::new(),
            },
        );
    }
    if matches!(turns.last(), Some(Turn::Assistant { .. })) {
        turns.push(Turn::User {
            content: "Continue from the prior response.".into(),
            images: Vec::new(),
            tool_results: Vec::new(),
        });
    }
    let instructions = object
        .get("instructions")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    if let Some(instructions) = instructions {
        if let Some(Turn::User { content, .. }) = turns.first_mut() {
            *content = format!("{instructions}\n\n{content}");
        }
    }
    if let Some(completion_tool) = completion_tool.as_deref() {
        if let Some(Turn::User { content, .. }) = turns.first_mut() {
            *content = format!(
                "When the task is fully complete, call `{completion_tool}` exactly once with the complete final answer in `answer`. Ordinary assistant text is progress and is not a final answer. Do not call it while requesting another tool.\n\n{content}"
            );
        }
    }

    let current = turns
        .pop()
        .ok_or("Kiro request has no current user message")?;
    let Turn::User {
        mut content,
        images,
        tool_results,
    } = current
    else {
        return Err("Kiro request must end with a user message".into());
    };
    if content.trim().is_empty() && !tool_results.is_empty() {
        content = "The requested tool result is attached.".into();
    }
    if content.trim().is_empty() && images.is_empty() {
        return Err("Kiro current user message must not be empty".into());
    }
    let mut current_context = Map::new();
    if !tools.is_empty() {
        current_context.insert("tools".into(), Value::Array(tools));
    }
    if !tool_results.is_empty() {
        current_context.insert("toolResults".into(), Value::Array(tool_results));
    }
    let origin = if profile_arn.is_some() {
        "AI_EDITOR"
    } else {
        "KIRO_CLI"
    };
    let mut current_message = json!({
        "content": content,
        "modelId": normalize_model_id(model),
        "origin": origin,
    });
    if !images.is_empty() {
        current_message["images"] = Value::Array(images);
    }
    if !current_context.is_empty() {
        current_message["userInputMessageContext"] = Value::Object(current_context);
    }
    let model_id = normalize_model_id(model);
    let history = turns
        .into_iter()
        .map(|turn| turn_to_wire(turn, &model_id, origin))
        .collect::<Vec<_>>();
    let mut payload = json!({
        "conversationState": {
            "chatTriggerType": "MANUAL",
            "agentContinuationId": Uuid::new_v4().to_string(),
            "agentTaskType": "vibe",
            "conversationId": conversation_id,
            "currentMessage": {"userInputMessage": current_message},
        }
    });
    if !history.is_empty() {
        payload["conversationState"]["history"] = Value::Array(history);
    }
    if let Some(profile_arn) = profile_arn {
        payload["profileArn"] = Value::String(profile_arn.to_string());
    }
    if let Some(effort) = object
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str)
    {
        if matches!(effort, "low" | "medium" | "high" | "xhigh" | "max") {
            let field = if normalize_model_id(model) == "gpt-5.6-sol" {
                Some("reasoning")
            } else if normalize_model_id(model) == "claude-opus-5" {
                Some("output_config")
            } else {
                None
            };
            if let Some(field) = field {
                let mut additional = Map::new();
                additional.insert(field.into(), json!({"effort": effort}));
                payload["additionalModelRequestFields"] = Value::Object(additional);
            }
        }
    }
    Ok((
        payload,
        KiroRequestContext {
            conversation_id,
            response_id,
            streaming: object
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            tool_names: aliases.reverse,
            completion_tool,
        },
    ))
}

fn normalize_model_id(value: &str) -> String {
    let mut value = value.trim().to_ascii_lowercase();
    if let Some(stripped) = value.strip_prefix("kiro/") {
        value = stripped.to_string();
    }
    if let Some(stripped) = value.strip_prefix("kiro-") {
        value = stripped.to_string();
    }
    if value == "auto" {
        return value;
    }
    for suffix in ["-low", "-medium", "-high", "-xhigh", "-max"] {
        if value.ends_with(suffix) {
            value.truncate(value.len() - suffix.len());
            break;
        }
    }
    let date_start = value.len().saturating_sub(9);
    if value.len() > 9
        && value.as_bytes().get(date_start) == Some(&b'-')
        && value.as_bytes()[date_start + 1..]
            .iter()
            .all(u8::is_ascii_digit)
    {
        value.truncate(date_start);
    }
    value = dotted_model_version(&value);
    for family in ["sonnet", "opus", "haiku"] {
        if let Some(version) = value
            .strip_prefix("claude-")
            .and_then(|rest| rest.strip_suffix(&format!("-{family}")))
            .filter(|version| {
                version
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b'.')
            })
        {
            value = format!("claude-{family}-{version}");
            break;
        }
    }
    value
}

fn dotted_model_version(value: &str) -> String {
    let characters = value.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(value.len());
    for (index, character) in characters.iter().copied().enumerate() {
        if character == '-'
            && index > 0
            && index + 1 < characters.len()
            && characters[index - 1].is_ascii_digit()
            && characters[index + 1].is_ascii_digit()
        {
            output.push('.');
        } else {
            output.push(character);
        }
    }
    output
}

fn valid_conversation_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

fn conversation_id_from_response_id(value: &str) -> Option<String> {
    let encoded = value.strip_prefix("resp_kiro_")?;
    let encoded = encoded.split('_').next()?;
    if encoded.len() != 32 || !encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let id = Uuid::parse_str(encoded).ok()?.to_string();
    valid_conversation_id(&id).then_some(id)
}

fn push_user(turns: &mut Vec<Turn>, content: String, images: Vec<Value>, results: Vec<Value>) {
    match turns.last_mut() {
        Some(Turn::User {
            content: current,
            images: current_images,
            tool_results,
        }) => {
            if !content.is_empty() {
                if !current.is_empty() {
                    current.push_str("\n\n");
                }
                current.push_str(&content);
            }
            current_images.extend(images);
            tool_results.extend(results);
        }
        _ => turns.push(Turn::User {
            content,
            images,
            tool_results: results,
        }),
    }
}

fn push_assistant(turns: &mut Vec<Turn>, content: String, tools: Vec<Value>) {
    match turns.last_mut() {
        Some(Turn::Assistant {
            content: current,
            tool_uses,
        }) => {
            if !content.is_empty() {
                if !current.is_empty() {
                    current.push_str("\n\n");
                }
                current.push_str(&content);
            }
            tool_uses.extend(tools);
        }
        _ => turns.push(Turn::Assistant {
            content,
            tool_uses: tools,
        }),
    }
}

fn turn_to_wire(turn: Turn, model_id: &str, origin: &str) -> Value {
    match turn {
        Turn::User {
            mut content,
            images,
            tool_results,
        } => {
            if content.trim().is_empty() && !tool_results.is_empty() {
                content = "The requested tool result is attached.".into();
            }
            let mut message = json!({
                "content": content,
                "modelId": model_id,
                "origin": origin
            });
            if !images.is_empty() {
                message["images"] = Value::Array(images);
            }
            if !tool_results.is_empty() {
                message["userInputMessageContext"] = json!({"toolResults": tool_results});
            }
            json!({"userInputMessage": message})
        }
        Turn::Assistant { content, tool_uses } => {
            let mut message = json!({"content": content});
            if !tool_uses.is_empty() {
                message["toolUses"] = Value::Array(tool_uses);
            }
            json!({"assistantResponseMessage": message})
        }
    }
}

fn kiro_content(value: Option<&Value>) -> Result<(String, Vec<Value>), String> {
    match value {
        None | Some(Value::Null) => Ok((String::new(), Vec::new())),
        Some(Value::String(text)) => Ok((text.clone(), Vec::new())),
        Some(Value::Array(parts)) => {
            let mut text = String::new();
            let mut images = Vec::new();
            let mut image_bytes = 0_usize;
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("input_text" | "output_text" | "text") => {
                        if let Some(value) = part.get("text").and_then(Value::as_str) {
                            text.push_str(value);
                        }
                    }
                    Some("input_image" | "image_url") => {
                        if images.len() >= MAX_KIRO_IMAGES {
                            return Err("Kiro accepts at most 20 images per message".into());
                        }
                        let url = part
                            .get("image_url")
                            .and_then(|value| {
                                value
                                    .as_str()
                                    .or_else(|| value.get("url").and_then(Value::as_str))
                            })
                            .or_else(|| part.get("url").and_then(Value::as_str))
                            .ok_or("Kiro image requires image_url")?;
                        let (format, bytes) = parse_image_data_url(url)?;
                        image_bytes = image_bytes.saturating_add(bytes.len());
                        if image_bytes > MAX_KIRO_IMAGE_BASE64_BYTES {
                            return Err("Kiro image data exceeds the 18 MiB base64 budget".into());
                        }
                        images.push(json!({"format": format, "source": {"bytes": bytes}}));
                    }
                    Some(other) => return Err(format!("unsupported Kiro content part: {other}")),
                    None => return Err("Kiro content part requires a type".into()),
                }
            }
            Ok((text, images))
        }
        Some(_) => Err("Kiro message content must be a string or array".into()),
    }
}

fn parse_image_data_url(value: &str) -> Result<(String, &str), String> {
    let value = value
        .strip_prefix("data:image/")
        .ok_or("Kiro supports inline data URL images only")?;
    let (metadata, bytes) = value
        .split_once(',')
        .ok_or("Kiro image data URL is malformed")?;
    let subtype = metadata
        .strip_suffix(";base64")
        .ok_or("Kiro image data URL must use base64")?;
    let format = match subtype.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "jpeg",
        "png" => "png",
        "webp" => "webp",
        "gif" => "gif",
        _ => return Err("Kiro image format must be jpeg, png, webp, or gif".into()),
    };
    let decoded_upper_bound = bytes.len().saturating_mul(3).saturating_add(3) / 4;
    if bytes.is_empty()
        || decoded_upper_bound > MAX_KIRO_IMAGE_BASE64_BYTES
        || STANDARD.decode(bytes.as_bytes()).is_err()
    {
        return Err("Kiro image data is not valid base64".into());
    }
    Ok((format.to_string(), bytes))
}

#[derive(Default)]
struct ToolAliases {
    used: BTreeSet<String>,
    reverse: BTreeMap<String, String>,
}

impl ToolAliases {
    fn alias(&mut self, original: &str) -> Result<String, String> {
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

fn reserve_completion_tool(aliases: &mut ToolAliases) -> String {
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

fn normalize_tool_id(value: &str) -> String {
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

fn convert_tools(
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

fn is_limit_stop_reason(reason: &str) -> bool {
    let reason = reason.to_ascii_uppercase();
    reason.contains("TOKEN") || reason.contains("LENGTH") || reason.contains("CONTEXT")
}

#[derive(Default)]
struct ToolOutput {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct KiroUsage {
    input: u64,
    output: u64,
    cached: u64,
    total: u64,
}

impl KiroUsage {
    fn update(&mut self, value: &Value) -> Result<(), String> {
        let number = |key: &str| -> Result<u64, String> {
            match value.get(key) {
                Some(value) => value
                    .as_u64()
                    .ok_or_else(|| format!("Kiro tokenUsage.{key} must be a non-negative integer")),
                None => Ok(0),
            }
        };
        let uncached = number("uncachedInputTokens")?;
        let cache_read = number("cacheReadInputTokens")?;
        let cache_write = number("cacheWriteInputTokens")?;
        self.input = uncached
            .saturating_add(cache_read)
            .saturating_add(cache_write);
        self.cached = cache_read;
        self.output = number("outputTokens")?;
        self.total = number("totalTokens")?.max(self.input.saturating_add(self.output));
        Ok(())
    }

    fn to_responses(&self) -> Value {
        json!({
            "input_tokens": self.input,
            "input_tokens_details": {"cached_tokens": self.cached},
            "output_tokens": self.output,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": self.total,
        })
    }
}

struct EventStreamFrame {
    headers: BTreeMap<String, String>,
    payload: Vec<u8>,
}

fn decode_eventstream(bytes: &[u8]) -> Result<Vec<EventStreamFrame>, String> {
    let mut offset = 0_usize;
    let mut frames = Vec::new();
    while offset < bytes.len() {
        if bytes.len() - offset < 16 {
            return Err("Kiro eventstream ended with a truncated frame".into());
        }
        let total = read_u32(&bytes[offset..offset + 4]) as usize;
        let headers_len = read_u32(&bytes[offset + 4..offset + 8]) as usize;
        if !(16..=MAX_EVENTSTREAM_FRAME).contains(&total) || offset + total > bytes.len() {
            return Err("Kiro eventstream frame length is invalid".into());
        }
        if headers_len > MAX_EVENTSTREAM_HEADERS || headers_len > total - 16 {
            return Err("Kiro eventstream header length is invalid".into());
        }
        let frame = &bytes[offset..offset + total];
        if crc32fast::hash(&frame[..8]) != read_u32(&frame[8..12]) {
            return Err("Kiro eventstream prelude CRC mismatch".into());
        }
        if crc32fast::hash(&frame[..total - 4]) != read_u32(&frame[total - 4..]) {
            return Err("Kiro eventstream message CRC mismatch".into());
        }
        frames.push(EventStreamFrame {
            headers: parse_eventstream_headers(&frame[12..12 + headers_len])?,
            payload: frame[12 + headers_len..total - 4].to_vec(),
        });
        offset += total;
    }
    Ok(frames)
}

fn drain_eventstream_frames(
    pending: &mut Vec<u8>,
    bytes: &[u8],
) -> Result<Vec<EventStreamFrame>, String> {
    pending.extend_from_slice(bytes);
    let mut frames = Vec::new();
    loop {
        if pending.len() < 12 {
            break;
        }
        let total = read_u32(&pending[..4]) as usize;
        let headers_len = read_u32(&pending[4..8]) as usize;
        if !(16..=MAX_EVENTSTREAM_FRAME).contains(&total) {
            return Err("Kiro eventstream frame length is invalid".into());
        }
        if headers_len > MAX_EVENTSTREAM_HEADERS || headers_len > total - 16 {
            return Err("Kiro eventstream header length is invalid".into());
        }
        if pending.len() < total {
            break;
        }
        let frame = pending.drain(..total).collect::<Vec<_>>();
        if crc32fast::hash(&frame[..8]) != read_u32(&frame[8..12]) {
            return Err("Kiro eventstream prelude CRC mismatch".into());
        }
        if crc32fast::hash(&frame[..total - 4]) != read_u32(&frame[total - 4..]) {
            return Err("Kiro eventstream message CRC mismatch".into());
        }
        frames.push(EventStreamFrame {
            headers: parse_eventstream_headers(&frame[12..12 + headers_len])?,
            payload: frame[12 + headers_len..total - 4].to_vec(),
        });
    }
    if pending.len() > MAX_EVENTSTREAM_FRAME {
        return Err("Kiro eventstream pending frame exceeded its limit".into());
    }
    Ok(frames)
}

fn parse_eventstream_headers(bytes: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let mut output = BTreeMap::new();
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let name_len = take_u8(bytes, &mut offset)? as usize;
        let name = take(bytes, &mut offset, name_len)?;
        let name = std::str::from_utf8(name)
            .map_err(|_| "Kiro eventstream header name is not UTF-8")?
            .to_string();
        let kind = take_u8(bytes, &mut offset)?;
        let value = match kind {
            0 => "true".into(),
            1 => "false".into(),
            2 => i8::from_be_bytes([take_u8(bytes, &mut offset)?]).to_string(),
            3 => i16::from_be_bytes(take_array::<2>(bytes, &mut offset)?).to_string(),
            4 => i32::from_be_bytes(take_array::<4>(bytes, &mut offset)?).to_string(),
            5 | 8 => i64::from_be_bytes(take_array::<8>(bytes, &mut offset)?).to_string(),
            6 | 7 => {
                let len = u16::from_be_bytes(take_array::<2>(bytes, &mut offset)?) as usize;
                let value = take(bytes, &mut offset, len)?;
                if kind == 7 {
                    std::str::from_utf8(value)
                        .map_err(|_| "Kiro eventstream string header is not UTF-8")?
                        .to_string()
                } else {
                    STANDARD.encode(value)
                }
            }
            9 => {
                let value = take(bytes, &mut offset, 16)?;
                format!(
                    "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                    value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
                    value[8], value[9], value[10], value[11], value[12], value[13], value[14], value[15]
                )
            }
            _ => return Err("Kiro eventstream header type is unsupported".into()),
        };
        output.insert(name, value);
    }
    Ok(output)
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn take_u8(bytes: &[u8], offset: &mut usize) -> Result<u8, String> {
    let value = *bytes
        .get(*offset)
        .ok_or("Kiro eventstream header is truncated")?;
    *offset += 1;
    Ok(value)
}

fn take<'a>(bytes: &'a [u8], offset: &mut usize, len: usize) -> Result<&'a [u8], String> {
    let end = offset
        .checked_add(len)
        .filter(|end| *end <= bytes.len())
        .ok_or("Kiro eventstream header is truncated")?;
    let value = &bytes[*offset..end];
    *offset = end;
    Ok(value)
}

fn take_array<const N: usize>(bytes: &[u8], offset: &mut usize) -> Result<[u8; N], String> {
    take(bytes, offset, N)?
        .try_into()
        .map_err(|_| "Kiro eventstream header is truncated".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_structured_custom_tool_input() {
        // A structured `input` (malformed client) must survive instead of
        // collapsing to {} — the toolUses input keeps the payload.
        let request = json!({
            "input": [
                {"type": "custom_tool_call", "call_id": "call_1", "name": "exec", "input": {"command": "ls"}},
                {"type": "custom_tool_call_output", "call_id": "call_1", "output": "src/"}
            ]
        });
        let (translated, _) =
            responses_to_kiro(&request, "gpt-5.6-sol", None).expect("request should translate");
        let serialized = serde_json::to_string(&translated).unwrap();
        assert!(
            serialized.contains("\"command\":\"ls\""),
            "payload lost: {serialized}"
        );
    }
}
