use super::*;

#[derive(Clone)]
pub(crate) struct ToolState {
    pub(crate) item_id: String,
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
    pub(crate) output_index: usize,
    pub(crate) is_custom: bool,
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
    reasoning_output_index: Option<usize>,
    next_output_index: usize,
    tools: BTreeMap<usize, ToolState>,
    usage: Value,
    sequence_number: u64,
    custom_tools: BTreeSet<String>,
}

impl ChatStreamState {
    pub fn new(exposed_model: String, custom_tools: BTreeSet<String>) -> (Self, Vec<String>) {
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
            reasoning_output_index: None,
            next_output_index: 0,
            tools: BTreeMap::new(),
            usage: Value::Null,
            sequence_number: 1,
            custom_tools,
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
        let Some(delta) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("delta"))
        else {
            return events;
        };

        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            self.ensure_message_started(&mut events);
            self.text.push_str(text);
            let output_index = self.message_output_index.unwrap_or(0);
            let payload = json!({
                "type": "response.output_text.delta",
                "item_id": self.message_id,
                "output_index": output_index,
                "content_index": 0,
                "delta": text
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
                        .unwrap_or("unknown")
                        .to_string();
                    let output_index = self.next_output_index;
                    self.next_output_index += 1;
                    let is_custom = self.custom_tools.contains(&name);
                    let announced = name != "unknown";
                    let state = ToolState {
                        item_id: format!(
                            "{}_{}",
                            if is_custom { "ctc" } else { "fc" },
                            Uuid::new_v4().simple()
                        ),
                        call_id,
                        name,
                        arguments: String::new(),
                        output_index,
                        is_custom,
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
                    // A Chat provider may stream the tool name in a later chunk
                    // than the index/id. Re-evaluate the custom-tool
                    // classification when the name first appears instead of
                    // freezing it at the first chunk (where it was "unknown").
                    if let Some(name) = tool_call.pointer("/function/name").and_then(Value::as_str)
                    {
                        if state.name == "unknown" && !name.is_empty() {
                            state.name = name.to_string();
                            state.is_custom = self.custom_tools.contains(&state.name);
                        }
                    }
                    let just_announced = !state.announced && state.name != "unknown";
                    if just_announced {
                        state.announced = true;
                        state.item_id = format!(
                            "{}_{}",
                            if state.is_custom { "ctc" } else { "fc" },
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
                            let event_type = if state.is_custom {
                                "response.custom_tool_call_input.delta"
                            } else {
                                "response.function_call_arguments.delta"
                            };
                            let delta = if just_announced {
                                // Announce chunk: the client has not seen any
                                // delta for this item yet, so emit everything
                                // accumulated (including pre-name chunks).
                                state.arguments.clone()
                            } else {
                                arguments.to_string()
                            };
                            delta_payload = Some(json!({
                                "type": event_type,
                                "item_id": state.item_id.clone(),
                                "output_index": state.output_index,
                                "delta": delta
                            }));
                        }
                    } else if just_announced && !state.arguments.is_empty() {
                        // Name arrived without arguments in this chunk; flush
                        // what accumulated while the name was still unknown.
                        let event_type = if state.is_custom {
                            "response.custom_tool_call_input.delta"
                        } else {
                            "response.function_call_arguments.delta"
                        };
                        delta_payload = Some(json!({
                            "type": event_type,
                            "item_id": state.item_id.clone(),
                            "output_index": state.output_index,
                            "delta": state.arguments.clone()
                        }));
                    }
                }
                if let Some(payload) = added_payload {
                    events.push(self.event("response.output_item.added", payload));
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

    /// Returns true once an ordinary function call has a known name and a
    /// complete JSON argument value. Codex can then execute the tool and may
    /// intentionally close the response body before `response.completed`.
    ///
    /// Custom tools are excluded because their inputs do not have to be JSON,
    /// so an intermediate delta cannot prove that their input is complete.
    pub fn has_actionable_function_call(&self) -> bool {
        self.tools.values().any(|tool| {
            tool.announced
                && !tool.is_custom
                && tool.name != "unknown"
                && serde_json::from_str::<Value>(&tool.arguments).is_ok()
        })
    }

    pub fn finish(mut self) -> Vec<String> {
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
            let (done_type, done_field) = if state.is_custom {
                ("response.custom_tool_call_input.done", "input")
            } else {
                ("response.function_call_arguments.done", "arguments")
            };
            let arguments_done = json!({
                "type": done_type,
                "item_id": state.item_id,
                "name": state.name,
                "output_index": state.output_index,
                done_field: state.arguments
            });
            events.push(self.event(done_type, arguments_done));
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
        let completed = json!({
            "type": "response.completed",
            "response": self.response_object("completed", output)
        });
        events.push(self.event("response.completed", completed));
        events
    }

    pub fn fail(&mut self, message: &str) -> String {
        let mut response = self.response_object("failed", Vec::new());
        response["error"] = json!({
            "code": "upstream_stream_error",
            "message": message
        });
        let payload = json!({
            "type": "response.failed",
            "response": response
        });
        self.event("response.failed", payload)
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
        let mut item = json!({
            "id": self.reasoning_id,
            "type": "reasoning",
            "status": status,
            "summary": [{"type": "summary_text", "text": self.reasoning_text}]
        });
        if !self.reasoning_signature.is_empty() {
            item["encrypted_content"] = Value::String(self.reasoning_signature.clone());
            item["provider_metadata"] = json!({"anthropic": {"thinking_blocks": [{
                "type": "thinking",
                "thinking": self.reasoning_text,
                "signature": self.reasoning_signature
            }]}});
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
