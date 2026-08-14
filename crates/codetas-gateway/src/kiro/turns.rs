use super::*;
use super::tools::ToolAliases;

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

pub(crate) fn normalize_model_id(value: &str) -> String {
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

