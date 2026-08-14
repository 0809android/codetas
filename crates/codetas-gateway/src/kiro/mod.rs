use crate::compaction::decode_summary;
use crate::translate::tool_input_to_value;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

mod eventstream;
mod tools;
mod turns;

pub(crate) use tools::kiro_eventstream_to_response;
pub(crate) use turns::responses_to_kiro;

use eventstream::{decode_eventstream, drain_eventstream_frames, is_limit_stop_reason, KiroUsage};
use tools::{convert_tools, normalize_tool_id, reserve_completion_tool};
use turns::normalize_model_id;

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
