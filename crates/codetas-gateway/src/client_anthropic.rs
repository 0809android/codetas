use crate::translate::tool_input_to_value;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use uuid::Uuid;

pub(crate) fn anthropic_request_to_responses(value: &Value) -> Result<Value, String> {
    let object = value
        .as_object()
        .ok_or("Anthropic request must be a JSON object")?;
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .ok_or("Anthropic request requires model")?;
    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("Anthropic request requires messages")?;
    if object
        .get("stop_sequences")
        .and_then(Value::as_array)
        .is_some_and(|values| !values.is_empty())
    {
        return Err("Anthropic stop_sequences cannot be translated safely".into());
    }
    let mut response = Map::new();
    response.insert("model".into(), Value::String(model.into()));
    response.insert(
        "stream".into(),
        Value::Bool(
            object
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
    );
    response.insert(
        "max_output_tokens".into(),
        object
            .get("max_tokens")
            .cloned()
            .unwrap_or_else(|| json!(4096)),
    );
    if let Some(system) = object.get("system") {
        response.insert("instructions".into(), Value::String(system_text(system)?));
    }
    let mut input = Vec::new();
    for message in messages {
        anthropic_message_to_responses(message, &mut input)?;
    }
    response.insert("input".into(), Value::Array(input));
    copy_field(object, &mut response, "temperature");
    copy_field(object, &mut response, "top_p");
    copy_field(object, &mut response, "metadata");
    if let Some(thinking) = object.get("thinking") {
        let effort = if thinking.get("type").and_then(Value::as_str) == Some("enabled") {
            match thinking
                .get("budget_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0)
            {
                0..=1_024 => "low",
                1_025..=8_192 => "medium",
                _ => "high",
            }
        } else {
            "none"
        };
        response.insert(
            "reasoning".into(),
            json!({"effort": effort, "summary": "auto"}),
        );
    }
    if let Some(tools) = object.get("tools").and_then(Value::as_array) {
        response.insert(
            "tools".into(),
            Value::Array(
                tools
                    .iter()
                    .map(anthropic_tool_to_response)
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        );
    }
    if let Some(choice) = object.get("tool_choice") {
        response.insert("tool_choice".into(), anthropic_tool_choice(choice)?);
    }
    Ok(Value::Object(response))
}

fn anthropic_message_to_responses(message: &Value, input: &mut Vec<Value>) -> Result<(), String> {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .ok_or("Anthropic message requires role")?;
    if !matches!(role, "user" | "assistant") {
        return Err(format!("unsupported Anthropic message role: {role}"));
    }
    let content = message
        .get("content")
        .ok_or("Anthropic message requires content")?;
    if let Some(text) = content.as_str() {
        input.push(json!({
            "type": "message",
            "role": role,
            "content": [{"type": if role == "assistant" { "output_text" } else { "input_text" }, "text": text}]
        }));
        return Ok(());
    }
    let blocks = content
        .as_array()
        .ok_or("Anthropic message content must be text or an array")?;
    let mut message_content = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => message_content.push(json!({
                "type": if role == "assistant" { "output_text" } else { "input_text" },
                "text": block.get("text").and_then(Value::as_str).unwrap_or_default()
            })),
            Some("image") if role == "user" => {
                message_content.push(anthropic_image_to_response(block)?);
            }
            Some("tool_result") if role == "user" => {
                flush_message_content(role, &mut message_content, input);
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": block.get("tool_use_id").and_then(Value::as_str).ok_or("tool_result requires tool_use_id")?,
                    "output": block_text(block.get("content"))?
                }));
            }
            Some("tool_use") if role == "assistant" => {
                flush_message_content(role, &mut message_content, input);
                let arguments = block.get("input").cloned().unwrap_or_else(|| json!({}));
                input.push(json!({
                    "type": "function_call",
                    "call_id": block.get("id").and_then(Value::as_str).ok_or("tool_use requires id")?,
                    "name": block.get("name").and_then(Value::as_str).ok_or("tool_use requires name")?,
                    "arguments": serde_json::to_string(&arguments).map_err(|error| error.to_string())?
                }));
            }
            Some("thinking") if role == "assistant" => {
                flush_message_content(role, &mut message_content, input);
                let signature = block
                    .get("signature")
                    .and_then(Value::as_str)
                    .filter(|signature| !signature.is_empty())
                    .ok_or("thinking block requires a non-empty signature")?;
                input.push(json!({
                    "type": "reasoning",
                    "encrypted_content": signature,
                    "summary": [{"type": "summary_text", "text": block.get("thinking").and_then(Value::as_str).unwrap_or_default()}],
                    "provider_metadata": {"anthropic": {"thinking_blocks": [block.clone()]}}
                }));
            }
            Some("redacted_thinking") if role == "assistant" => {
                flush_message_content(role, &mut message_content, input);
                input.push(json!({
                    "type": "reasoning",
                    "summary": [],
                    "provider_metadata": {"anthropic": {"thinking_blocks": [block.clone()]}}
                }));
            }
            Some(kind) => return Err(format!("unsupported Anthropic content block: {kind}")),
            None => return Err("Anthropic content block requires type".into()),
        }
    }
    flush_message_content(role, &mut message_content, input);
    Ok(())
}

fn flush_message_content(role: &str, content: &mut Vec<Value>, input: &mut Vec<Value>) {
    if !content.is_empty() {
        input.push(json!({
            "type": "message",
            "role": role,
            "content": std::mem::take(content)
        }));
    }
}

fn anthropic_image_to_response(block: &Value) -> Result<Value, String> {
    let source = block
        .get("source")
        .and_then(Value::as_object)
        .ok_or("Anthropic image requires source")?;
    match source.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .ok_or("base64 image requires media_type")?;
            let data = source
                .get("data")
                .and_then(Value::as_str)
                .ok_or("base64 image requires data")?;
            Ok(json!({
                "type": "input_image",
                "image_url": format!("data:{media_type};base64,{data}")
            }))
        }
        Some("url") => Ok(json!({
            "type": "input_image",
            "image_url": source.get("url").and_then(Value::as_str).ok_or("URL image requires url")?
        })),
        Some(kind) => Err(format!("unsupported Anthropic image source: {kind}")),
        None => Err("Anthropic image source requires type".into()),
    }
}

fn anthropic_tool_to_response(tool: &Value) -> Result<Value, String> {
    Ok(json!({
        "type": "function",
        "name": tool.get("name").and_then(Value::as_str).ok_or("Anthropic tool requires name")?,
        "description": tool.get("description").cloned().unwrap_or(Value::Null),
        "parameters": tool.get("input_schema").cloned().ok_or("Anthropic tool requires input_schema")?,
        "strict": false
    }))
}

fn anthropic_tool_choice(choice: &Value) -> Result<Value, String> {
    match choice.get("type").and_then(Value::as_str) {
        Some("auto") => Ok(Value::String("auto".into())),
        Some("any") => Ok(Value::String("required".into())),
        Some("none") => Ok(Value::String("none".into())),
        Some("tool") => Ok(json!({
            "type": "function",
            "name": choice.get("name").and_then(Value::as_str).ok_or("tool choice requires name")?
        })),
        Some(kind) => Err(format!("unsupported Anthropic tool_choice: {kind}")),
        None => Err("Anthropic tool_choice requires type".into()),
    }
}

fn system_text(system: &Value) -> Result<String, String> {
    if let Some(text) = system.as_str() {
        return Ok(text.to_string());
    }
    block_text(Some(system))
}

fn block_text(value: Option<&Value>) -> Result<String, String> {
    let Some(value) = value else {
        return Ok(String::new());
    };
    if let Some(text) = value.as_str() {
        return Ok(text.to_string());
    }
    let blocks = value.as_array().ok_or("content must be text or an array")?;
    let mut output = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            return Err("non-text tool result cannot be translated safely".into());
        }
        output.push(
            block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        );
    }
    Ok(output.join("\n"))
}

fn copy_field(source: &Map<String, Value>, target: &mut Map<String, Value>, name: &str) {
    if let Some(value) = source.get(name) {
        target.insert(name.into(), value.clone());
    }
}

pub(crate) fn responses_to_anthropic_response(
    value: &Value,
    fallback_model: &str,
) -> Result<Value, String> {
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .ok_or("Responses output must be an array")?;
    let mut content = Vec::new();
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if matches!(
                        part.get("type").and_then(Value::as_str),
                        Some("output_text" | "text")
                    ) {
                        content.push(json!({
                            "type": "text",
                            "text": part.get("text").and_then(Value::as_str).unwrap_or_default()
                        }));
                    }
                }
            }
            Some("reasoning") => {
                if let Some(blocks) = item
                    .pointer("/provider_metadata/anthropic/thinking_blocks")
                    .and_then(Value::as_array)
                {
                    content.extend(blocks.iter().cloned());
                } else if let Some(signature) = item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .filter(|signature| !signature.is_empty())
                {
                    content.push(json!({
                        "type": "thinking",
                        "thinking": item
                            .get("summary")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .collect::<String>(),
                        "signature": signature
                    }));
                }
            }
            Some("function_call" | "custom_tool_call") => {
                // custom_tool_call items carry the payload in `input`
                // while function_call uses `arguments`. A structured
                // payload is preserved as-is instead of collapsing to {}.
                let input = item
                    .get("input")
                    .or_else(|| item.get("arguments"))
                    .map(tool_input_to_value)
                    .unwrap_or_else(|| json!({}));
                content.push(json!({
                    "type": "tool_use",
                    "id": item.get("call_id").or_else(|| item.get("id")).and_then(Value::as_str).unwrap_or_default(),
                    "name": item.get("name").and_then(Value::as_str).unwrap_or_default(),
                    "input": input
                }));
            }
            _ => {}
        }
    }
    let has_tools = content
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"));
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    Ok(json!({
        "id": value.get("id").and_then(Value::as_str).unwrap_or("msg_codetas"),
        "type": "message",
        "role": "assistant",
        "model": value.get("model").and_then(Value::as_str).unwrap_or(fallback_model),
        "content": content,
        "stop_reason": if has_tools { "tool_use" } else if status == "incomplete" { "max_tokens" } else { "end_turn" },
        "stop_sequence": Value::Null,
        "usage": responses_usage_to_anthropic(value.get("usage"))
    }))
}

pub(crate) struct ResponsesToAnthropicStream {
    id: String,
    model: String,
    started: bool,
    finished: bool,
    next_index: usize,
    text_index: Option<usize>,
    thinking_index: Option<usize>,
    thinking_has_signature: bool,
    tool_indices: HashMap<String, usize>,
}

impl ResponsesToAnthropicStream {
    pub(crate) fn new(model: String) -> Self {
        Self {
            id: format!("msg_{}", Uuid::new_v4().simple()),
            model,
            started: false,
            finished: false,
            next_index: 0,
            text_index: None,
            thinking_index: None,
            thinking_has_signature: false,
            tool_indices: HashMap::new(),
        }
    }

    pub(crate) fn push(&mut self, event: &Value) -> Vec<String> {
        if let Some(id) = event.pointer("/response/id").and_then(Value::as_str) {
            self.id = id.to_string();
        }
        if let Some(model) = event.pointer("/response/model").and_then(Value::as_str) {
            self.model = model.to_string();
        }
        let mut output = self.ensure_started();
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => {
                let index = if let Some(index) = self.text_index {
                    index
                } else {
                    let index = self.allocate_index();
                    self.text_index = Some(index);
                    output.push(sse(
                        "content_block_start",
                        json!({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}}),
                    ));
                    index
                };
                output.push(sse(
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": event.get("delta").and_then(Value::as_str).unwrap_or_default()}}),
                ));
            }
            Some("response.reasoning_summary_text.delta") => {
                let index = self.ensure_thinking_started(&mut output);
                output.push(sse(
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": index, "delta": {"type": "thinking_delta", "thinking": event.get("delta").and_then(Value::as_str).unwrap_or_default()}}),
                ));
            }
            Some("response.reasoning_signature.delta") => {
                let signature = event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !signature.is_empty() {
                    let index = self.ensure_thinking_started(&mut output);
                    output.push(sse(
                        "content_block_delta",
                        json!({"type": "content_block_delta", "index": index, "delta": {"type": "signature_delta", "signature": signature}}),
                    ));
                    self.thinking_has_signature = true;
                }
            }
            Some("response.output_item.done")
                if event.pointer("/item/type").and_then(Value::as_str) == Some("reasoning") =>
            {
                if !self.thinking_has_signature {
                    let signature = event
                        .pointer("/item/provider_metadata/anthropic/thinking_blocks/0/signature")
                        .or_else(|| event.pointer("/item/encrypted_content"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if !signature.is_empty() {
                        let index = self.ensure_thinking_started(&mut output);
                        output.push(sse(
                            "content_block_delta",
                            json!({"type": "content_block_delta", "index": index, "delta": {"type": "signature_delta", "signature": signature}}),
                        ));
                        self.thinking_has_signature = true;
                    }
                }
            }
            Some("response.output_item.added")
                if matches!(
                    event.pointer("/item/type").and_then(Value::as_str),
                    Some("function_call" | "custom_tool_call")
                ) =>
            {
                let item = &event["item"];
                let key = item
                    .get("id")
                    .or_else(|| item.get("call_id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let index = self.allocate_index();
                self.tool_indices.insert(key, index);
                output.push(sse(
                    "content_block_start",
                    json!({"type": "content_block_start", "index": index, "content_block": {
                        "type": "tool_use",
                        "id": item.get("call_id").or_else(|| item.get("id")).and_then(Value::as_str).unwrap_or_default(),
                        "name": item.get("name").and_then(Value::as_str).unwrap_or_default(),
                        "input": {}
                    }}),
                ));
            }
            Some(
                "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta",
            ) => {
                let key = event
                    .get("item_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(index) = self.tool_indices.get(key).copied() {
                    output.push(sse(
                        "content_block_delta",
                        json!({"type": "content_block_delta", "index": index, "delta": {
                            "type": "input_json_delta",
                            "partial_json": event.get("delta").and_then(Value::as_str).unwrap_or_default()
                        }}),
                    ));
                }
            }
            Some("response.completed" | "response.incomplete") => {
                let response = event.get("response").unwrap_or(event);
                output.extend(self.finish_events(response));
            }
            Some("response.failed" | "error") => {
                output.push(sse(
                    "error",
                    json!({"type": "error", "error": {"type": "api_error", "message": "CODETAS upstream Responses stream failed"}}),
                ));
                self.finished = true;
            }
            _ => {}
        }
        output
    }

    pub(crate) fn finish(&mut self) -> Vec<String> {
        if self.finished {
            Vec::new()
        } else {
            self.finish_events(&json!({"status": "completed", "usage": {}}))
        }
    }

    fn ensure_started(&mut self) -> Vec<String> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        vec![sse(
            "message_start",
            json!({"type": "message_start", "message": {
                "id": self.id,
                "type": "message",
                "role": "assistant",
                "model": self.model,
                "content": [],
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }}),
        )]
    }

    fn finish_events(&mut self, response: &Value) -> Vec<String> {
        if self.finished {
            return Vec::new();
        }
        let mut output = Vec::new();
        for index in 0..self.next_index {
            output.push(sse(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": index}),
            ));
        }
        let has_tools = !self.tool_indices.is_empty();
        let stop_reason = if has_tools {
            "tool_use"
        } else if response.get("status").and_then(Value::as_str) == Some("incomplete") {
            "max_tokens"
        } else {
            "end_turn"
        };
        let usage = responses_usage_to_anthropic(response.get("usage"));
        output.push(sse(
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": stop_reason, "stop_sequence": Value::Null}, "usage": {"output_tokens": usage["output_tokens"]}}),
        ));
        output.push(sse("message_stop", json!({"type": "message_stop"})));
        self.finished = true;
        output
    }

    fn allocate_index(&mut self) -> usize {
        let index = self.next_index;
        self.next_index = self.next_index.saturating_add(1);
        index
    }

    fn ensure_thinking_started(&mut self, output: &mut Vec<String>) -> usize {
        if let Some(index) = self.thinking_index {
            return index;
        }
        let index = self.allocate_index();
        self.thinking_index = Some(index);
        output.push(sse(
            "content_block_start",
            json!({"type": "content_block_start", "index": index, "content_block": {"type": "thinking", "thinking": ""}}),
        ));
        index
    }
}

fn responses_usage_to_anthropic(usage: Option<&Value>) -> Value {
    json!({
        "input_tokens": usage.and_then(|value| value.get("input_tokens")).and_then(Value::as_u64).unwrap_or(0),
        "output_tokens": usage.and_then(|value| value.get("output_tokens")).and_then(Value::as_u64).unwrap_or(0),
        "cache_read_input_tokens": usage.and_then(|value| value.pointer("/input_tokens_details/cached_tokens")).and_then(Value::as_u64).unwrap_or(0),
        "cache_creation_input_tokens": 0
    })
}

fn sse(event: &str, value: Value) -> String {
    format!("event: {event}\ndata: {value}\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_anthropic_tool_round_trip() {
        let request = anthropic_request_to_responses(&json!({
            "model": "provider/model",
            "max_tokens": 100,
            "messages": [
                {"role": "assistant", "content": [{"type": "tool_use", "id": "call_1", "name": "lookup", "input": {"q": "x"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "ok"}]}
            ]
        }))
        .unwrap();
        assert_eq!(request["input"][0]["type"], "function_call");
        assert_eq!(request["input"][1]["type"], "function_call_output");
    }
}
