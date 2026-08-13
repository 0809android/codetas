use serde_json::{json, Map, Value};
use std::collections::HashMap;
use uuid::Uuid;

pub(crate) fn chat_request_to_responses(value: &Value) -> Result<Value, String> {
    let object = value
        .as_object()
        .ok_or("Chat Completions request must be a JSON object")?;
    if object
        .get("n")
        .and_then(Value::as_u64)
        .is_some_and(|count| count != 1)
    {
        return Err("CODETAS Chat endpoint supports n=1 only".into());
    }
    for unsupported in ["stop", "logprobs", "top_logprobs", "audio", "modalities"] {
        if object
            .get(unsupported)
            .is_some_and(|value| !value.is_null())
        {
            return Err(format!(
                "Chat field {unsupported} cannot be translated safely"
            ));
        }
    }

    let model = object
        .get("model")
        .and_then(Value::as_str)
        .ok_or("Chat request requires model")?;
    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("Chat request requires messages")?;
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for message in messages {
        translate_message(message, &mut instructions, &mut input)?;
    }

    let mut response = Map::new();
    response.insert("model".into(), Value::String(model.into()));
    response.insert("input".into(), Value::Array(input));
    response.insert(
        "stream".into(),
        Value::Bool(
            object
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
    );
    if !instructions.is_empty() {
        response.insert(
            "instructions".into(),
            Value::String(instructions.join("\n\n")),
        );
    }
    copy_field(object, &mut response, "temperature", "temperature");
    copy_field(object, &mut response, "top_p", "top_p");
    copy_field(
        object,
        &mut response,
        "parallel_tool_calls",
        "parallel_tool_calls",
    );
    copy_field(object, &mut response, "metadata", "metadata");
    if let Some(tokens) = object
        .get("max_completion_tokens")
        .or_else(|| object.get("max_tokens"))
    {
        response.insert("max_output_tokens".into(), tokens.clone());
    }
    if let Some(effort) = object.get("reasoning_effort") {
        response.insert("reasoning".into(), json!({"effort": effort}));
    }
    if let Some(tools) = object.get("tools").and_then(Value::as_array) {
        response.insert(
            "tools".into(),
            Value::Array(
                tools
                    .iter()
                    .map(chat_tool_to_response)
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        );
    }
    if let Some(choice) = object.get("tool_choice") {
        response.insert("tool_choice".into(), chat_tool_choice(choice)?);
    }
    if let Some(format) = object.get("response_format") {
        response.insert(
            "text".into(),
            json!({"format": chat_response_format(format)?}),
        );
    }
    Ok(Value::Object(response))
}

fn translate_message(
    message: &Value,
    instructions: &mut Vec<String>,
    input: &mut Vec<Value>,
) -> Result<(), String> {
    let object = message
        .as_object()
        .ok_or("Chat message must be an object")?;
    let role = object
        .get("role")
        .and_then(Value::as_str)
        .ok_or("Chat message requires role")?;
    if matches!(role, "system" | "developer") {
        instructions.push(content_as_text(object.get("content"))?);
        return Ok(());
    }
    if role == "tool" {
        let call_id = object
            .get("tool_call_id")
            .and_then(Value::as_str)
            .ok_or("tool message requires tool_call_id")?;
        input.push(json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": content_as_text(object.get("content"))?
        }));
        return Ok(());
    }
    if !matches!(role, "user" | "assistant") {
        return Err(format!("unsupported Chat message role: {role}"));
    }
    if let Some(content) = object.get("content").filter(|value| !value.is_null()) {
        let content = chat_content_to_response(content, role)?;
        if !content.is_empty() {
            input.push(json!({"type": "message", "role": role, "content": content}));
        }
    }
    if role == "assistant" {
        if let Some(tool_calls) = object.get("tool_calls").and_then(Value::as_array) {
            for tool_call in tool_calls {
                let function = tool_call
                    .get("function")
                    .and_then(Value::as_object)
                    .ok_or("assistant tool_call requires function")?;
                input.push(json!({
                    "type": "function_call",
                    "call_id": tool_call.get("id").and_then(Value::as_str).ok_or("assistant tool_call requires id")?,
                    "name": function.get("name").and_then(Value::as_str).ok_or("assistant tool_call requires function name")?,
                    "arguments": function.get("arguments").and_then(Value::as_str).unwrap_or("{}")
                }));
            }
        }
    }
    Ok(())
}

fn chat_content_to_response(content: &Value, role: &str) -> Result<Vec<Value>, String> {
    if let Some(text) = content.as_str() {
        return Ok(vec![json!({
            "type": if role == "assistant" { "output_text" } else { "input_text" },
            "text": text
        })]);
    }
    let parts = content
        .as_array()
        .ok_or("Chat message content must be text or an array")?;
    let mut output = Vec::new();
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => output.push(json!({
                "type": if role == "assistant" { "output_text" } else { "input_text" },
                "text": part.get("text").and_then(Value::as_str).unwrap_or_default()
            })),
            Some("image_url") if role == "user" => {
                let image = part
                    .get("image_url")
                    .ok_or("image_url part requires image_url")?;
                let (url, detail) = if let Some(url) = image.as_str() {
                    (url, None)
                } else {
                    (
                        image
                            .get("url")
                            .and_then(Value::as_str)
                            .ok_or("image_url part requires url")?,
                        image.get("detail").and_then(Value::as_str),
                    )
                };
                output.push(json!({"type": "input_image", "image_url": url, "detail": detail}));
            }
            Some(kind) => return Err(format!("unsupported Chat content part: {kind}")),
            None => return Err("Chat content part requires type".into()),
        }
    }
    Ok(output)
}

fn content_as_text(content: Option<&Value>) -> Result<String, String> {
    let Some(content) = content else {
        return Ok(String::new());
    };
    if let Some(text) = content.as_str() {
        return Ok(text.to_string());
    }
    let parts = content
        .as_array()
        .ok_or("message content must be text or an array")?;
    let mut output = Vec::new();
    for part in parts {
        if part.get("type").and_then(Value::as_str) != Some("text") {
            return Err("non-text system or tool content cannot be translated safely".into());
        }
        output.push(
            part.get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        );
    }
    Ok(output.join("\n"))
}

fn chat_tool_to_response(tool: &Value) -> Result<Value, String> {
    if tool.get("type").and_then(Value::as_str) != Some("function") {
        return Err("only function tools are supported by the Chat endpoint".into());
    }
    let function = tool
        .get("function")
        .and_then(Value::as_object)
        .ok_or("Chat function tool requires function")?;
    Ok(json!({
        "type": "function",
        "name": function.get("name").and_then(Value::as_str).ok_or("function tool requires name")?,
        "description": function.get("description").cloned().unwrap_or(Value::Null),
        "parameters": function.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        "strict": function.get("strict").cloned().unwrap_or(Value::Bool(false))
    }))
}

fn chat_tool_choice(choice: &Value) -> Result<Value, String> {
    if let Some(choice) = choice.as_str() {
        return match choice {
            "none" | "auto" | "required" => Ok(Value::String(choice.into())),
            _ => Err(format!("unsupported Chat tool_choice: {choice}")),
        };
    }
    let name = choice
        .pointer("/function/name")
        .and_then(Value::as_str)
        .ok_or("function tool_choice requires function.name")?;
    Ok(json!({"type": "function", "name": name}))
}

fn chat_response_format(format: &Value) -> Result<Value, String> {
    match format.get("type").and_then(Value::as_str) {
        Some("text") => Ok(json!({"type": "text"})),
        Some("json_object") => Ok(json!({"type": "json_object"})),
        Some("json_schema") => {
            let schema = format
                .get("json_schema")
                .and_then(Value::as_object)
                .ok_or("json_schema response format requires json_schema")?;
            Ok(json!({
                "type": "json_schema",
                "name": schema.get("name").and_then(Value::as_str).ok_or("json_schema requires name")?,
                "schema": schema.get("schema").cloned().ok_or("json_schema requires schema")?,
                "strict": schema.get("strict").cloned().unwrap_or(Value::Bool(false))
            }))
        }
        Some(kind) => Err(format!("unsupported Chat response_format: {kind}")),
        None => Err("Chat response_format requires type".into()),
    }
}

fn copy_field(source: &Map<String, Value>, target: &mut Map<String, Value>, from: &str, to: &str) {
    if let Some(value) = source.get(from) {
        target.insert(to.into(), value.clone());
    }
}

pub(crate) fn responses_to_chat_response(
    value: &Value,
    fallback_model: &str,
) -> Result<Value, String> {
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .ok_or("Responses output must be an array")?;
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for content in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if matches!(
                        content.get("type").and_then(Value::as_str),
                        Some("output_text" | "text")
                    ) {
                        text.push_str(
                            content
                                .get("text")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                        );
                    }
                }
            }
            Some("reasoning") => {
                for summary in item
                    .get("summary")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    reasoning.push_str(
                        summary
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    );
                }
            }
            Some("function_call" | "custom_tool_call") => {
                let (arguments_field, fallback) =
                    if item.get("type").and_then(Value::as_str) == Some("custom_tool_call") {
                        ("input", "{input:null}")
                    } else {
                        ("arguments", "{}")
                    };
                // A structured payload (malformed client) is JSON-encoded so
                // the tool call still carries its arguments instead of
                // silently collapsing to the fallback.
                let arguments = item
                    .get(arguments_field)
                    .map(crate::translate::payload_to_arguments)
                    .unwrap_or_else(|| fallback.to_string());
                tool_calls.push(json!({
                    "id": item.get("call_id").or_else(|| item.get("id")).and_then(Value::as_str).unwrap_or_default(),
                    "type": "function",
                    "function": {
                        "name": item.get("name").and_then(Value::as_str).unwrap_or_default(),
                        "arguments": arguments
                    }
                }));
            }
            _ => {}
        }
    }
    let mut message = json!({"role": "assistant", "content": text});
    if !reasoning.is_empty() {
        message["reasoning_content"] = Value::String(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let has_tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty());
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    let finish_reason = if has_tool_calls {
        "tool_calls"
    } else if status == "incomplete" {
        "length"
    } else {
        "stop"
    };
    Ok(json!({
        "id": value.get("id").and_then(Value::as_str).unwrap_or("chatcmpl-codetas"),
        "object": "chat.completion",
        "created": unix_seconds(),
        "model": value.get("model").and_then(Value::as_str).unwrap_or(fallback_model),
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
        "usage": responses_usage_to_chat(value.get("usage"))
    }))
}

pub(crate) struct ResponsesToChatStream {
    id: String,
    model: String,
    created: u64,
    started: bool,
    finished: bool,
    tool_indices: HashMap<String, usize>,
}

impl ResponsesToChatStream {
    pub(crate) fn new(model: String) -> Self {
        Self {
            id: format!("chatcmpl-{}", Uuid::new_v4().simple()),
            model,
            created: unix_seconds(),
            started: false,
            finished: false,
            tool_indices: HashMap::new(),
        }
    }

    pub(crate) fn push(&mut self, event: &Value) -> Vec<String> {
        if let Some(id) = event
            .pointer("/response/id")
            .or_else(|| event.get("response_id"))
            .and_then(Value::as_str)
        {
            self.id = id.to_string();
        }
        if let Some(model) = event.pointer("/response/model").and_then(Value::as_str) {
            self.model = model.to_string();
        }
        let mut output = self.ensure_started();
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => {
                output.push(self.chunk(json!({"content": event.get("delta").and_then(Value::as_str).unwrap_or_default()}), None, None));
            }
            Some("response.reasoning_summary_text.delta") => {
                output.push(self.chunk(json!({"reasoning_content": event.get("delta").and_then(Value::as_str).unwrap_or_default()}), None, None));
            }
            Some("response.output_item.added")
                if matches!(
                    event.pointer("/item/type").and_then(Value::as_str),
                    Some("function_call" | "custom_tool_call")
                ) =>
            {
                let item = &event["item"];
                let item_id = item
                    .get("id")
                    .or_else(|| item.get("call_id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let index = self.tool_indices.len();
                self.tool_indices.insert(item_id, index);
                let arguments_field =
                    if item.get("type").and_then(Value::as_str) == Some("custom_tool_call") {
                        "input"
                    } else {
                        "arguments"
                    };
                output.push(self.chunk(
                    json!({"tool_calls": [{
                        "index": index,
                        "id": item.get("call_id").or_else(|| item.get("id")).and_then(Value::as_str).unwrap_or_default(),
                        "type": "function",
                        "function": {
                            "name": item.get("name").and_then(Value::as_str).unwrap_or_default(),
                            arguments_field: ""
                        }
                    }]}),
                    None,
                    None,
                ));
            }
            Some(
                "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta",
            ) => {
                let item_id = event
                    .get("item_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let index = self
                    .tool_indices
                    .get(item_id)
                    .copied()
                    .unwrap_or(self.tool_indices.len());
                output.push(self.chunk(
                    json!({"tool_calls": [{
                        "index": index,
                        "function": {"arguments": event.get("delta").and_then(Value::as_str).unwrap_or_default()}
                    }]}),
                    None,
                    None,
                ));
            }
            Some("response.completed" | "response.incomplete") => {
                let response = event.get("response").unwrap_or(event);
                let finish = if response
                    .get("output")
                    .and_then(Value::as_array)
                    .is_some_and(|items| {
                        items.iter().any(|item| {
                            matches!(
                                item.get("type").and_then(Value::as_str),
                                Some("function_call" | "custom_tool_call")
                            )
                        })
                    }) {
                    "tool_calls"
                } else if response.get("status").and_then(Value::as_str) == Some("incomplete") {
                    "length"
                } else {
                    "stop"
                };
                output.push(self.chunk(
                    json!({}),
                    Some(finish),
                    Some(responses_usage_to_chat(response.get("usage"))),
                ));
                output.push("data: [DONE]\n\n".into());
                self.finished = true;
            }
            Some("response.failed" | "error") => {
                output.push(format!(
                    "data: {}\n\n",
                    json!({"error": {"message": "CODETAS upstream Responses stream failed"}})
                ));
                output.push("data: [DONE]\n\n".into());
                self.finished = true;
            }
            _ => {}
        }
        output
    }

    pub(crate) fn finish(&mut self) -> Vec<String> {
        if self.finished {
            return Vec::new();
        }
        let mut output = self.ensure_started();
        output.push(self.chunk(json!({}), Some("stop"), None));
        output.push("data: [DONE]\n\n".into());
        self.finished = true;
        output
    }

    fn ensure_started(&mut self) -> Vec<String> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        vec![self.chunk(json!({"role": "assistant", "content": ""}), None, None)]
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>, usage: Option<Value>) -> String {
        let mut chunk = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]
        });
        if let Some(usage) = usage {
            chunk["usage"] = usage;
        }
        format!("data: {chunk}\n\n")
    }
}

fn responses_usage_to_chat(usage: Option<&Value>) -> Value {
    let input = usage
        .and_then(|value| value.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .and_then(|value| value.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": usage.and_then(|value| value.get("total_tokens")).and_then(Value::as_u64).unwrap_or(input + output),
        "prompt_tokens_details": {
            "cached_tokens": usage.and_then(|value| value.pointer("/input_tokens_details/cached_tokens")).and_then(Value::as_u64).unwrap_or(0)
        },
        "completion_tokens_details": {
            "reasoning_tokens": usage.and_then(|value| value.pointer("/output_tokens_details/reasoning_tokens")).and_then(Value::as_u64).unwrap_or(0)
        }
    })
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_chat_tools_and_images_to_responses() {
        let request = chat_request_to_responses(&json!({
            "model": "provider/model",
            "messages": [
                {"role": "system", "content": "be precise"},
                {"role": "user", "content": [
                    {"type": "text", "text": "inspect"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA==", "detail": "high"}}
                ]}
            ],
            "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {"type": "object"}}}]
        }))
        .unwrap();
        assert_eq!(request["instructions"], "be precise");
        assert_eq!(request["input"][0]["content"][1]["type"], "input_image");
        assert_eq!(request["tools"][0]["name"], "lookup");
    }

    #[test]
    fn translates_responses_output_to_chat() {
        let response = responses_to_chat_response(
            &json!({
                "id": "resp_1",
                "model": "provider/model",
                "status": "completed",
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "done"}]}],
                "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
            }),
            "fallback",
        )
        .unwrap();
        assert_eq!(response["choices"][0]["message"]["content"], "done");
        assert_eq!(response["usage"]["total_tokens"], 5);
    }

    #[test]
    fn streaming_finish_reason_counts_custom_tool_call() {
        // A Responses stream ending in a custom_tool_call must map to
        // finish_reason "tool_calls", matching the non-streaming path.
        let mut stream = ResponsesToChatStream::new("provider/model".into());
        let mut events = Vec::new();
        events.extend(stream.push(&json!({
            "type": "response.output_item.added",
            "item": {"id": "ctc_1", "type": "custom_tool_call", "status": "in_progress"}
        })));
        events.extend(stream.push(&json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [{"type": "custom_tool_call", "name": "exec", "input": "{}"}]
            }
        })));
        assert!(events
            .iter()
            .any(|event| { event.contains("\"finish_reason\":\"tool_calls\"") }));
    }
}
