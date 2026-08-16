use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

// Responses has no portable progress-only event that Codex Desktop renders.
// Keep this fixed and content-free so it cannot expose reasoning or tool input.
pub(crate) const TOOL_PROGRESS_MESSAGE: &str = "Still working…";
pub(crate) const TOOL_PROGRESS_INTERVAL: usize = 6;

static EMPTY_CONTENT_TOOL_ONLY_TURNS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ToolProgressPolicy {
    pub(crate) emit_on_tool_call: bool,
}

impl ToolProgressPolicy {
    pub(crate) fn from_request(body: &Value) -> Self {
        let Some(items) = body.get("input").and_then(Value::as_array) else {
            return Self::default();
        };
        let mut saw_user = false;
        let mut saw_visible_assistant = false;
        let mut tool_groups_since_visible = 0_usize;
        let mut in_tool_group = false;

        // The reconstructed Responses history lets the gateway rate-limit
        // progress without process-global conversation identifiers. Parallel
        // tool calls are one group because they belong to the same model turn.
        for item in items {
            if item.get("role").and_then(Value::as_str) == Some("user") {
                saw_user = true;
                saw_visible_assistant = false;
                tool_groups_since_visible = 0;
                in_tool_group = false;
                continue;
            }
            if !saw_user {
                continue;
            }

            match item.get("type").and_then(Value::as_str) {
                Some("message") if item.get("role").and_then(Value::as_str) == Some("assistant") => {
                    in_tool_group = false;
                    if response_message_has_visible_text(item) {
                        saw_visible_assistant = true;
                        tool_groups_since_visible = 0;
                    }
                }
                Some("function_call" | "custom_tool_call" | "tool_search_call") => {
                    if !in_tool_group {
                        tool_groups_since_visible = tool_groups_since_visible.saturating_add(1);
                        in_tool_group = true;
                    }
                }
                Some("function_call_output" | "custom_tool_call_output" | "tool_search_output") => {
                    in_tool_group = false;
                }
                _ => {}
            }
        }

        Self {
            emit_on_tool_call: saw_user
                && (!saw_visible_assistant
                    || tool_groups_since_visible.saturating_add(1) >= TOOL_PROGRESS_INTERVAL),
        }
    }
}

fn response_message_has_visible_text(item: &Value) -> bool {
    match item.get("content") {
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(Value::Array(parts)) => parts.iter().any(|part| {
            part.get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty())
        }),
        _ => false,
    }
}

#[derive(Clone)]
pub(crate) struct ToolState {
    pub(crate) item_id: String,
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) identity: ResponseToolIdentity,
    pub(crate) arguments: String,
    pub(crate) provider_metadata: Option<Value>,
    pub(crate) output_index: usize,
    pub(crate) announced: bool,
}

pub struct ChatStreamState {
    response_id: String,
    exposed_model: String,
    message_id: String,
    text: String,
    message_started: bool,
    message_output_index: Option<usize>,
    reasoning_id: String,
    reasoning_text: String,
    reasoning_signature: String,
    anthropic_thinking_blocks: Vec<Value>,
    reasoning_output_index: Option<usize>,
    next_output_index: usize,
    tools: BTreeMap<usize, ToolState>,
    usage: Value,
    sequence_number: u64,
    tool_map: ResponseToolMap,
    progress_policy: ToolProgressPolicy,
    provider_visible_text: bool,
    synthetic_progress_emitted: bool,
    tool_stream_finished: bool,
}

impl ChatStreamState {
    pub fn new(exposed_model: String, tool_map: ResponseToolMap) -> (Self, Vec<String>) {
        Self::new_with_progress(
            exposed_model,
            tool_map,
            ToolProgressPolicy::default(),
        )
    }

    pub(crate) fn new_with_progress(
        exposed_model: String,
        tool_map: ResponseToolMap,
        progress_policy: ToolProgressPolicy,
    ) -> (Self, Vec<String>) {
        let response_id = response_id();
        let mut state = Self {
            response_id: response_id.clone(),
            exposed_model: exposed_model.clone(),
            message_id: format!("msg_{}", Uuid::new_v4().simple()),
            text: String::new(),
            message_started: false,
            message_output_index: None,
            reasoning_id: format!("rs_{}", Uuid::new_v4().simple()),
            reasoning_text: String::new(),
            reasoning_signature: String::new(),
            anthropic_thinking_blocks: Vec::new(),
            reasoning_output_index: None,
            next_output_index: 0,
            tools: BTreeMap::new(),
            usage: Value::Null,
            sequence_number: 1,
            tool_map,
            progress_policy,
            provider_visible_text: false,
            synthetic_progress_emitted: false,
            tool_stream_finished: false,
        };
        let created = json!({
            "type": "response.created",
            "response": state.response_object("in_progress", Vec::new())
        });
        let created = state.event("response.created", created);
        let in_progress_payload = json!({
            "type": "response.in_progress",
            "response": state.response_object("in_progress", Vec::new())
        });
        let in_progress = state.event("response.in_progress", in_progress_payload);
        (state, vec![created, in_progress])
    }

    pub fn push_chat_chunk(&mut self, chunk: &Value) -> Vec<String> {
        let mut events = Vec::new();
        if let Some(usage) = chunk.get("usage") {
            if !usage.is_null() {
                self.usage = chat_usage_to_responses(Some(usage));
            }
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return events;
        };
        if choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| reason == "tool_calls")
        {
            self.tool_stream_finished = true;
        }
        let Some(delta) = choice.get("delta") else {
            return events;
        };

        if let Some(block) = delta.get("codetas_anthropic_thinking_block") {
            if block.get("type").and_then(Value::as_str) == Some("redacted_thinking") {
                self.anthropic_thinking_blocks.push(block.clone());
                self.ensure_reasoning_started(&mut events);
            }
        }

        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            let has_visible_text = !text.trim().is_empty();
            let needs_progress_separator = self.synthetic_progress_emitted
                && has_visible_text
                && !self.provider_visible_text;
            self.provider_visible_text |= has_visible_text;
            self.ensure_message_started(&mut events);
            let visible_delta = if needs_progress_separator {
                format!("\n\n{text}")
            } else {
                text.to_string()
            };
            self.text.push_str(&visible_delta);
            let output_index = self.message_output_index.unwrap_or(0);
            let payload = json!({
                "type": "response.output_text.delta",
                "item_id": self.message_id,
                "output_index": output_index,
                "content_index": 0,
                "delta": visible_delta
            });
            events.push(self.event("response.output_text.delta", payload));
        }

        if let Some(text) = delta.get("reasoning_content").and_then(Value::as_str) {
            self.ensure_reasoning_started(&mut events);
            self.reasoning_text.push_str(text);
            let output_index = self.reasoning_output_index.unwrap_or(0);
            events.push(self.event(
                "response.reasoning_summary_text.delta",
                json!({
                    "type": "response.reasoning_summary_text.delta",
                    "item_id": self.reasoning_id,
                    "output_index": output_index,
                    "summary_index": 0,
                    "delta": text
                }),
            ));
        }

        if let Some(signature) = delta.get("reasoning_signature").and_then(Value::as_str) {
            self.ensure_reasoning_started(&mut events);
            self.reasoning_signature.push_str(signature);
            let output_index = self.reasoning_output_index.unwrap_or(0);
            events.push(self.event(
                "response.reasoning_signature.delta",
                json!({
                    "type": "response.reasoning_signature.delta",
                    "item_id": self.reasoning_id,
                    "output_index": output_index,
                    "delta": signature
                }),
            ));
        }

        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for tool_call in tool_calls {
                let index = tool_call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let is_new = !self.tools.contains_key(&index);
                if is_new {
                    let call_id = tool_call
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
                    let name = tool_call
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .unwrap_or("unknown")
                        .to_string();
                    let output_index = self.next_output_index;
                    self.next_output_index += 1;
                    let identity = stream_tool_identity(&self.tool_map, &name);
                    let announced = name != "unknown";
                    let state = ToolState {
                        item_id: format!(
                            "{}_{}",
                            tool_item_prefix(identity.kind),
                            Uuid::new_v4().simple()
                        ),
                        call_id,
                        name,
                        identity,
                        arguments: String::new(),
                        provider_metadata: tool_call
                            .get("codetas_provider_metadata")
                            .cloned(),
                        output_index,
                        announced,
                    };
                    if announced {
                        events.push(self.event(
                            "response.output_item.added",
                            json!({
                                "type": "response.output_item.added",
                                "output_index": state.output_index,
                                "item": tool_item(&state, "in_progress")
                            }),
                        ));
                    }
                    self.tools.insert(index, state);
                }
                let mut added_payload: Option<Value> = None;
                let mut delta_payload: Option<Value> = None;
                if let Some(state) = self.tools.get_mut(&index) {
                    if let Some(metadata) = tool_call.get("codetas_provider_metadata") {
                        state.provider_metadata = Some(metadata.clone());
                    }
                    // A Chat provider may stream the tool name in a later chunk
                    // than the index/id. Re-evaluate the custom-tool
                    // classification when the name first appears instead of
                    // freezing it at the first chunk (where it was "unknown").
                    if let Some(name) = tool_call.pointer("/function/name").and_then(Value::as_str)
                    {
                        if state.name == "unknown" && !name.is_empty() {
                            state.name = name.to_string();
                            state.identity = stream_tool_identity(&self.tool_map, &state.name);
                        }
                    }
                    let just_announced = !state.announced && state.name != "unknown";
                    if just_announced {
                        state.announced = true;
                        state.item_id = format!(
                            "{}_{}",
                            tool_item_prefix(state.identity.kind),
                            Uuid::new_v4().simple()
                        );
                        added_payload = Some(json!({
                            "type": "response.output_item.added",
                            "output_index": state.output_index,
                            "item": tool_item(state, "in_progress")
                        }));
                    }
                    if let Some(arguments) = tool_call
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                    {
                        state.arguments.push_str(arguments);
                        if state.announced {
                            if state.identity.kind == ResponseToolKind::Function {
                                let delta = if just_announced {
                                    // Announce chunk: the client has not seen any
                                    // delta for this item yet, so emit everything
                                    // accumulated (including pre-name chunks).
                                    state.arguments.clone()
                                } else {
                                    arguments.to_string()
                                };
                                delta_payload = Some(json!({
                                    "type": "response.function_call_arguments.delta",
                                    "item_id": state.item_id.clone(),
                                    "output_index": state.output_index,
                                    "delta": delta
                                }));
                            }
                        }
                    } else if just_announced && !state.arguments.is_empty() {
                        // Name arrived without arguments in this chunk; flush
                        // what accumulated while the name was still unknown.
                        if state.identity.kind == ResponseToolKind::Function {
                            delta_payload = Some(json!({
                                "type": "response.function_call_arguments.delta",
                                "item_id": state.item_id.clone(),
                                "output_index": state.output_index,
                                "delta": state.arguments.clone()
                            }));
                        }
                    }
                }
                if let Some(payload) = added_payload {
                    events.push(self.event("response.output_item.added", payload));
                }
                if self
                    .tools
                    .get(&index)
                    .is_some_and(|state| state.announced)
                {
                    self.maybe_emit_tool_progress(&mut events);
                }
                if let Some(payload) = delta_payload {
                    let event_type = payload
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("response.function_call_arguments.delta")
                        .to_string();
                    events.push(self.event(&event_type, payload));
                }
            }
        }
        events
    }

    /// Returns true once at least one tool call is complete enough for Codex to execute.
    /// JSON tools become actionable when their arguments parse; custom tools become
    /// actionable when the provider emits the authoritative `tool_calls` finish marker.
    pub fn has_actionable_function_call(&self) -> bool {
        self.actionable_tool_call_count() > 0
    }

    pub(crate) fn actionable_tool_call_count(&self) -> usize {
        self.tools
            .values()
            .filter(|tool| self.tool_is_actionable(tool))
            .count()
    }

    fn tool_is_actionable(&self, tool: &ToolState) -> bool {
        tool.announced
            && tool.name != "unknown"
            && if tool.identity.kind == ResponseToolKind::Custom {
                // Custom inputs are arbitrary text rather than JSON. The provider's
                // tool_calls finish marker is the first authoritative completion signal.
                self.tool_stream_finished
            } else {
                serde_json::from_str::<Value>(&tool.arguments).is_ok()
            }
    }

    pub fn finish(self) -> Vec<String> {
        self.finish_with_response().0
    }

    pub(crate) fn finish_with_response(mut self) -> (Vec<String>, Value) {
        let mut events = Vec::new();
        let mut indexed_output = Vec::new();
        if let Some(output_index) = self.reasoning_output_index {
            let item = self.reasoning_item("completed");
            events.push(self.event(
                "response.reasoning_summary_text.done",
                json!({
                    "type": "response.reasoning_summary_text.done",
                    "item_id": self.reasoning_id,
                    "output_index": output_index,
                    "summary_index": 0,
                    "text": self.reasoning_text
                }),
            ));
            events.push(self.event(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "item": item
                }),
            ));
            indexed_output.push((output_index, self.reasoning_item("completed")));
        }
        if let Some(message_output_index) = self.message_output_index {
            let text_done = json!({
                "type": "response.output_text.done",
                "item_id": self.message_id,
                "output_index": message_output_index,
                "content_index": 0,
                "text": self.text
            });
            events.push(self.event("response.output_text.done", text_done));
            let message = self.message_item("completed");
            let content_done = json!({
                "type": "response.content_part.done",
                "item_id": self.message_id,
                "output_index": message_output_index,
                "content_index": 0,
                "part": {"type": "output_text", "text": self.text, "annotations": []}
            });
            events.push(self.event("response.content_part.done", content_done));
            let item_done = json!({
                "type": "response.output_item.done",
                "output_index": message_output_index,
                "item": message
            });
            events.push(self.event("response.output_item.done", item_done));
            indexed_output.push((message_output_index, self.message_item("completed")));
        }
        let tool_states = self.tools.values().cloned().collect::<Vec<_>>();
        for state in &tool_states {
            // If the name never arrived (pathological provider), announce the
            // item now so the client has a matching output_item.added before
            // the done events.
            if !state.announced {
                events.push(self.event(
                    "response.output_item.added",
                    json!({
                        "type": "response.output_item.added",
                        "output_index": state.output_index,
                        "item": tool_item(state, "in_progress")
                    }),
                ));
            }
            match state.identity.kind {
                ResponseToolKind::Custom => {
                    let input = unwrap_custom_tool_arguments(&state.arguments);
                    if !input.is_empty() {
                        events.push(self.event(
                            "response.custom_tool_call_input.delta",
                            json!({
                                "type": "response.custom_tool_call_input.delta",
                                "item_id": state.item_id,
                                "output_index": state.output_index,
                                "delta": input
                            }),
                        ));
                    }
                    events.push(self.event(
                        "response.custom_tool_call_input.done",
                        json!({
                            "type": "response.custom_tool_call_input.done",
                            "item_id": state.item_id,
                            "output_index": state.output_index,
                            "input": input
                        }),
                    ));
                }
                ResponseToolKind::Function => events.push(self.event(
                    "response.function_call_arguments.done",
                    json!({
                        "type": "response.function_call_arguments.done",
                        "item_id": state.item_id,
                        "name": state.identity.name,
                        "output_index": state.output_index,
                        "arguments": state.arguments
                    }),
                )),
                ResponseToolKind::ToolSearch => {}
            }
            let item = tool_item(state, "completed");
            let item_done = json!({
                "type": "response.output_item.done",
                "output_index": state.output_index,
                "item": item
            });
            events.push(self.event("response.output_item.done", item_done));
            indexed_output.push((state.output_index, tool_item(state, "completed")));
        }
        indexed_output.sort_by_key(|(index, _)| *index);
        let output = indexed_output
            .into_iter()
            .map(|(_, item)| item)
            .collect::<Vec<_>>();
        let response = self.response_object("completed", output);
        let completed = json!({
            "type": "response.completed",
            "response": response.clone()
        });
        events.push(self.event("response.completed", completed));
        (events, response)
    }

    pub(crate) fn completed_response_snapshot(&self) -> Value {
        let mut indexed_output = Vec::new();
        if let Some(output_index) = self.reasoning_output_index {
            indexed_output.push((output_index, self.reasoning_item("completed")));
        }
        if let Some(output_index) = self.message_output_index {
            indexed_output.push((output_index, self.message_item("completed")));
        }
        for state in self
            .tools
            .values()
            .filter(|state| self.tool_is_actionable(state))
        {
            indexed_output.push((state.output_index, tool_item(state, "completed")));
        }
        indexed_output.sort_by_key(|(index, _)| *index);
        self.response_object(
            "completed",
            indexed_output
                .into_iter()
                .map(|(_, item)| item)
                .collect(),
        )
    }

    pub fn fail(&mut self, message: &str) -> String {
        self.fail_with_code("upstream_stream_error", message)
    }

    pub fn fail_with_code(&mut self, code: &str, message: &str) -> String {
        let mut response = self.response_object("failed", Vec::new());
        response["error"] = json!({
            "code": code,
            "message": message
        });
        let payload = json!({
            "type": "response.failed",
            "response": response
        });
        self.event("response.failed", payload)
    }

    fn maybe_emit_tool_progress(&mut self, events: &mut Vec<String>) {
        if !self.progress_policy.emit_on_tool_call
            || self.provider_visible_text
            || self.synthetic_progress_emitted
        {
            return;
        }
        self.synthetic_progress_emitted = true;
        self.ensure_message_started(events);
        self.text.push_str(TOOL_PROGRESS_MESSAGE);
        let output_index = self.message_output_index.unwrap_or(0);
        events.push(self.event(
            "response.output_text.delta",
            json!({
                "type": "response.output_text.delta",
                "item_id": self.message_id,
                "output_index": output_index,
                "content_index": 0,
                "delta": TOOL_PROGRESS_MESSAGE
            }),
        ));
    }

    fn ensure_message_started(&mut self, events: &mut Vec<String>) {
        if self.message_started {
            return;
        }
        self.message_started = true;
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        self.message_output_index = Some(output_index);
        let item_added = json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": {
                "id": self.message_id,
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": []
            }
        });
        events.push(self.event("response.output_item.added", item_added));
        let content_added = json!({
            "type": "response.content_part.added",
            "item_id": self.message_id,
            "output_index": output_index,
            "content_index": 0,
            "part": {"type": "output_text", "text": "", "annotations": []}
        });
        events.push(self.event("response.content_part.added", content_added));
    }

    fn ensure_reasoning_started(&mut self, events: &mut Vec<String>) {
        if self.reasoning_output_index.is_some() {
            return;
        }
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        self.reasoning_output_index = Some(output_index);
        events.push(self.event(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": self.reasoning_item("in_progress")
            }),
        ));
    }

    fn reasoning_item(&self, status: &str) -> Value {
        let summary = if self.reasoning_text.is_empty() {
            Vec::new()
        } else {
            vec![json!({"type": "summary_text", "text": self.reasoning_text})]
        };
        let mut item = json!({
            "id": self.reasoning_id,
            "type": "reasoning",
            "status": status,
            "summary": summary
        });
        let mut thinking_blocks = self.anthropic_thinking_blocks.clone();
        if !self.reasoning_signature.is_empty() {
            item["encrypted_content"] = Value::String(self.reasoning_signature.clone());
            thinking_blocks.insert(0, json!({
                "type": "thinking",
                "thinking": self.reasoning_text,
                "signature": self.reasoning_signature
            }));
        }
        if !thinking_blocks.is_empty() {
            item["provider_metadata"] = json!({"anthropic": {
                "thinking_blocks": thinking_blocks
            }});
        }
        item
    }

    fn message_item(&self, status: &str) -> Value {
        json!({
            "id": self.message_id,
            "type": "message",
            "status": status,
            "role": "assistant",
            "content": [{"type": "output_text", "text": self.text, "annotations": []}]
        })
    }

    fn response_object(&self, status: &str, output: Vec<Value>) -> Value {
        let completed_at = if status == "completed" {
            json!(unix_seconds())
        } else {
            Value::Null
        };
        json!({
            "id": self.response_id,
            "object": "response",
            "created_at": unix_seconds(),
            "completed_at": completed_at,
            "status": status,
            "instructions": Value::Null,
            "max_output_tokens": Value::Null,
            "model": self.exposed_model,
            "output": output,
            "parallel_tool_calls": true,
            "previous_response_id": Value::Null,
            "reasoning": {"effort": Value::Null, "summary": Value::Null},
            "store": false,
            "temperature": 1,
            "text": {"format": {"type": "text"}},
            "tool_choice": "auto",
            "tools": [],
            "top_p": 1,
            "truncation": "disabled",
            "usage": self.usage,
            "user": Value::Null,
            "metadata": {},
            "error": Value::Null,
            "incomplete_details": Value::Null
        })
    }

    fn event(&mut self, event: &str, mut payload: Value) -> String {
        if let Some(object) = payload.as_object_mut() {
            object.insert("sequence_number".into(), json!(self.sequence_number));
        }
        self.sequence_number += 1;
        sse(event, &payload)
    }
}

fn stream_tool_identity(tool_map: &ResponseToolMap, wire_name: &str) -> ResponseToolIdentity {
    tool_map
        .identity(wire_name)
        .cloned()
        .unwrap_or_else(|| ResponseToolIdentity {
            name: wire_name.to_string(),
            namespace: None,
            kind: ResponseToolKind::Function,
        })
}

fn tool_item_prefix(kind: ResponseToolKind) -> &'static str {
    match kind {
        ResponseToolKind::Function => "fc",
        ResponseToolKind::Custom => "ctc",
        ResponseToolKind::ToolSearch => "tsc",
    }
}

impl Drop for ChatStreamState {
    fn drop(&mut self) {
        if self.provider_visible_text || self.tools.is_empty() {
            return;
        }
        // This is intentionally metadata-only. The opt-in debug log never
        // receives reasoning text, assistant text, tool names, or arguments.
        let total = EMPTY_CONTENT_TOOL_ONLY_TURNS
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        crate::debug::log(&format!(
            "translated_stream empty_content_tool_only_turn total={} model={} tool_calls={} reasoning_chars={} progress_emitted={}",
            total,
            self.exposed_model,
            self.tools.len(),
            self.reasoning_text.chars().count(),
            self.synthetic_progress_emitted
        ));
    }
}

#[cfg(test)]
mod provider_metadata_tests {
    use super::*;

    #[test]
    fn streamed_tool_provider_metadata_reaches_the_responses_snapshot() {
        let (mut state, _) =
            ChatStreamState::new("gemini-test".into(), ResponseToolMap::default());
        state.push_chat_chunk(&json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": "call_signed",
                "type": "function",
                "function": {"name": "lookup", "arguments": "{}"},
                "codetas_provider_metadata": {
                    "gemini": {"thought_signature": "signed-stream-part"}
                }
            }]}}]
        }));

        let snapshot = state.completed_response_snapshot();
        assert_eq!(
            snapshot["output"][0]["provider_metadata"]["gemini"]["thought_signature"],
            "signed-stream-part"
        );
    }

    #[test]
    fn redacted_only_anthropic_stream_emits_reasoning_metadata_and_replays_verbatim() {
        let (mut state, _) =
            ChatStreamState::new("claude-test".into(), ResponseToolMap::default());
        state.push_chat_chunk(&json!({
            "choices": [{"delta": {"codetas_anthropic_thinking_block": {
                "type": "redacted_thinking", "data": "OPAQUE1"
            }}}]
        }));

        let snapshot = state.completed_response_snapshot();
        assert_eq!(snapshot["output"][0]["type"], "reasoning");
        assert_eq!(snapshot["output"][0]["summary"], json!([]));
        assert_eq!(
            snapshot["output"][0]["provider_metadata"]["anthropic"]["thinking_blocks"][0],
            json!({"type": "redacted_thinking", "data": "OPAQUE1"})
        );

        let replay = crate::anthropic::responses_to_anthropic_with_oauth(
            &json!({"input": snapshot["output"].clone()}),
            "claude-test",
            false,
        )
        .expect("redacted thinking replay");
        assert_eq!(
            replay["messages"][0]["content"][0],
            json!({"type": "redacted_thinking", "data": "OPAQUE1"})
        );
    }
}
