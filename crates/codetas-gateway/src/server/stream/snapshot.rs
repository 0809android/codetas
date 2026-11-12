use super::*;

pub(super) const MAX_SNAPSHOT_REPAIR_BYTES: usize = 8 * 1024 * 1024;
const MAX_SNAPSHOT_REPAIR_INDEX: usize = 4_096;

#[derive(Default)]
pub(crate) struct ResponsesSnapshotAccumulator {
    items: std::collections::BTreeMap<usize, Value>,
    continuation_done: std::collections::BTreeMap<usize, Value>,
    continuation_done_bytes: usize,
    item_indexes: HashMap<String, usize>,
    open_indexes: HashSet<usize>,
    closed_indexes: HashSet<usize>,
    open_content_parts: HashSet<(usize, usize)>,
    completed_text_parts: HashSet<(usize, usize)>,
    completed_content_parts: HashSet<(usize, usize)>,
    tainted: bool,
}

impl ResponsesSnapshotAccumulator {
    pub(crate) fn observe(&mut self, event: &Value) {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        self.observe_continuation_done(event);
        if self.tainted {
            return;
        }
        if kind != "response.output_item.done"
            && (kind.ends_with(".delta")
                || kind.ends_with(".done")
                || kind == "response.content_part.added")
        {
            let index = self.event_index(event);
            if self.tainted {
                return;
            }
            if index.is_none() {
                self.taint();
                return;
            }
            if index.is_some_and(|index| self.closed_indexes.contains(&index)) {
                self.taint();
                return;
            }
        }
        match kind {
            "response.output_item.added" | "response.output_item.done" => {
                let Some(index) = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .filter(|index| *index <= MAX_SNAPSHOT_REPAIR_INDEX)
                else {
                    self.taint();
                    return;
                };
                let Some(item_object) = event.get("item").and_then(Value::as_object) else {
                    self.taint();
                    return;
                };
                let Some(id) = item_object
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                else {
                    self.taint();
                    return;
                };
                let Some(item_type) = item_object
                    .get("type")
                    .and_then(Value::as_str)
                    .filter(|item_type| !item_type.trim().is_empty())
                else {
                    self.taint();
                    return;
                };
                let item = Value::Object(item_object.clone());
                if serde_json::to_vec(&item)
                    .is_ok_and(|encoded| encoded.len() > MAX_SNAPSHOT_REPAIR_BYTES)
                {
                    self.taint();
                    return;
                }
                if self
                    .item_indexes
                    .get(id)
                    .is_some_and(|existing| *existing != index)
                    || self
                        .items
                        .get(&index)
                        .and_then(|existing| existing.get("id"))
                        .and_then(Value::as_str)
                        .is_some_and(|existing| existing != id)
                    || self
                        .items
                        .get(&index)
                        .and_then(|existing| existing.get("type"))
                        .and_then(Value::as_str)
                        .is_some_and(|existing| existing != item_type)
                {
                    self.taint();
                    return;
                }
                if kind.ends_with(".added") {
                    if !self.open_indexes.insert(index) || self.closed_indexes.contains(&index) {
                        self.taint();
                        return;
                    }
                } else {
                    if self.closed_indexes.contains(&index) {
                        self.taint();
                        return;
                    }
                    self.open_indexes.remove(&index);
                    self.closed_indexes.insert(index);
                }
                self.item_indexes.insert(id.to_string(), index);
                let item = if let Some(existing) = self.items.remove(&index) {
                    merge_snapshot_item(existing, &item)
                } else {
                    item
                };
                self.items.insert(index, item.clone());
            }
            "response.output_text.delta" | "response.output_text.done" => {
                let index = self.event_index(event);
                let Some(index) = index else { return };
                let Some(content_index) = self.subindex(event, "content_index") else {
                    return;
                };
                let Some(item) = self.tracked_item_mut(index, event, "message") else {
                    return;
                };
                ensure_array_slot(
                    item,
                    "content",
                    content_index,
                    || json!({"type": "output_text", "text": "", "annotations": []}),
                );
                let field = if kind.ends_with(".done") {
                    "text"
                } else {
                    "delta"
                };
                if let Some(text) = event.get(field).and_then(Value::as_str) {
                    let target = &mut item["content"][content_index]["text"];
                    if kind.ends_with(".done") {
                        *target = Value::String(text.to_string());
                    } else {
                        append_string(target, text);
                    }
                }
                if kind.ends_with(".done") {
                    self.completed_text_parts.insert((index, content_index));
                }
            }
            "response.content_part.added" | "response.content_part.done" => {
                let index = self.event_index(event);
                let Some(index) = index else { return };
                let Some(content_index) = self.subindex(event, "content_index") else {
                    return;
                };
                let Some(item_type) = self
                    .items
                    .get(&index)
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str)
                    .filter(|item_type| matches!(*item_type, "message" | "reasoning"))
                    .map(str::to_string)
                else {
                    self.taint();
                    return;
                };
                let Some(item) = self.tracked_item_mut(index, event, &item_type) else {
                    return;
                };
                if let Some(part) = event.get("part").cloned() {
                    ensure_array_slot(item, "content", content_index, || Value::Null);
                    let current = std::mem::take(&mut item["content"][content_index]);
                    item["content"][content_index] = merge_snapshot_item(current, &part);
                }
                if item_type == "message" {
                    self.open_content_parts.insert((index, content_index));
                    if kind.ends_with(".done") {
                        self.completed_content_parts.insert((index, content_index));
                    }
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_summary_text.done" => {
                let index = self.event_index(event);
                let Some(index) = index else { return };
                let Some(summary_index) = self.subindex(event, "summary_index") else {
                    return;
                };
                let Some(item) = self.tracked_item_mut(index, event, "reasoning") else {
                    return;
                };
                ensure_array_slot(
                    item,
                    "summary",
                    summary_index,
                    || json!({"type": "summary_text", "text": ""}),
                );
                let field = if kind.ends_with(".done") {
                    "text"
                } else {
                    "delta"
                };
                if let Some(text) = event.get(field).and_then(Value::as_str) {
                    let target = &mut item["summary"][summary_index]["text"];
                    if kind.ends_with(".done") {
                        *target = Value::String(text.to_string());
                    } else {
                        append_string(target, text);
                    }
                }
            }
            "response.reasoning_text.delta" | "response.reasoning_text.done" => {
                let index = self.event_index(event);
                let Some(index) = index else { return };
                let Some(content_index) = self.subindex(event, "content_index") else {
                    return;
                };
                let Some(item) = self.tracked_item_mut(index, event, "reasoning") else {
                    return;
                };
                ensure_array_slot(
                    item,
                    "content",
                    content_index,
                    || json!({"type": "reasoning_text", "text": ""}),
                );
                let field = if kind.ends_with(".done") {
                    "text"
                } else {
                    "delta"
                };
                if let Some(text) = event.get(field).and_then(Value::as_str) {
                    let target = &mut item["content"][content_index]["text"];
                    if kind.ends_with(".done") {
                        *target = Value::String(text.to_string());
                    } else {
                        append_string(target, text);
                    }
                }
            }
            "response.reasoning_signature.delta" => {
                let index = self.event_index(event);
                let Some(index) = index else { return };
                let Some(item) = self.tracked_item_mut(index, event, "reasoning") else {
                    return;
                };
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    append_string(&mut item["encrypted_content"], delta);
                }
            }
            "response.reasoning_summary_part.added" | "response.reasoning_summary_part.done" => {
                let Some(index) = self.event_index(event) else {
                    return;
                };
                let _ = self.tracked_item_mut(index, event, "reasoning");
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                self.observe_scalar_stream(event, kind, "function_call", "arguments");
            }
            "response.custom_tool_call_input.delta" | "response.custom_tool_call_input.done" => {
                self.observe_scalar_stream(event, kind, "custom_tool_call", "input");
            }
            _ => {}
        }
        self.enforce_size_limit();
    }

    // Completed wire items remain usable for local history even when sparse
    // lifecycle repair is unsafe. Never use this backfill to alter client events.
    fn observe_continuation_done(&mut self, event: &Value) {
        if event.get("type").and_then(Value::as_str) != Some("response.output_item.done") {
            return;
        }
        let Some(index) = event
            .get("output_index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .filter(|index| *index <= MAX_SNAPSHOT_REPAIR_INDEX)
        else {
            return;
        };
        let Some(item) = event.get("item").filter(|item| {
            item.is_object()
                && item
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
        }) else {
            return;
        };
        // Keep the first complete item if a malformed duplicate follows it.
        if self.continuation_done.contains_key(&index) {
            return;
        }
        let Ok(bytes) = serde_json::to_vec(item) else {
            return;
        };
        if self.continuation_done_bytes.saturating_add(bytes.len()) > MAX_SNAPSHOT_REPAIR_BYTES {
            return;
        }
        self.continuation_done_bytes += bytes.len();
        self.continuation_done.insert(index, item.clone());
    }

    pub(super) fn response_for_continuation(&self, mut response: Value) -> Value {
        if response.is_object()
            && response
                .get("output")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty)
            && !self.continuation_done.is_empty()
        {
            response["output"] = Value::Array(self.continuation_done.values().cloned().collect());
        }
        response
    }

    pub(crate) fn injected_events_before(&mut self, event: &Value) -> Vec<Value> {
        if self.tainted
            || !matches!(
                event.get("type").and_then(Value::as_str),
                Some("response.output_text.delta" | "response.output_text.done")
            )
        {
            return Vec::new();
        }
        let Some(index) = self.event_index(event) else {
            return Vec::new();
        };
        let Some(content_index) = self.subindex(event, "content_index") else {
            return Vec::new();
        };
        if self.open_content_parts.contains(&(index, content_index))
            || !self.open_indexes.contains(&index)
            || self
                .items
                .get(&index)
                .and_then(|item| item.get("type"))
                .and_then(Value::as_str)
                != Some("message")
        {
            return Vec::new();
        }
        let Some(item_id) = self
            .items
            .get(&index)
            .and_then(|item| item.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            self.taint();
            return Vec::new();
        };
        if event.get("item_id").is_some_and(|value| {
            value
                .as_str()
                .is_none_or(|id| id.trim().is_empty() || id != item_id)
        }) {
            self.taint();
            return Vec::new();
        }
        let injected = json!({
            "type": "response.content_part.added",
            "item_id": item_id,
            "output_index": index,
            "content_index": content_index,
            "part": {"type": "output_text", "text": "", "annotations": []}
        });
        self.observe(&injected);
        vec![injected]
    }

    pub(crate) fn closing_events_before_terminal(&mut self, event: &Value) -> Vec<Value> {
        if self.tainted || event.get("type").and_then(Value::as_str) != Some("response.completed") {
            return Vec::new();
        }
        let mut indexes = self.open_indexes.iter().copied().collect::<Vec<_>>();
        indexes.sort_unstable();
        let mut events = Vec::new();
        for index in indexes {
            let Some(item) = self.items.get(&index).cloned() else {
                self.taint();
                return Vec::new();
            };
            let Some(item_id) = item.get("id").and_then(Value::as_str).map(str::to_string) else {
                self.taint();
                return Vec::new();
            };
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    let text = item
                        .pointer("/content/0/text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if !self.open_content_parts.contains(&(index, 0)) {
                        let injected = json!({
                            "type": "response.content_part.added",
                            "item_id": item_id.clone(),
                            "output_index": index,
                            "content_index": 0,
                            "part": {"type": "output_text", "text": "", "annotations": []}
                        });
                        self.observe(&injected);
                        events.push(injected);
                    }
                    if !self.completed_text_parts.contains(&(index, 0)) {
                        let injected = json!({
                            "type": "response.output_text.done",
                            "item_id": item_id.clone(),
                            "output_index": index,
                            "content_index": 0,
                            "logprobs": [],
                            "text": text.clone()
                        });
                        self.observe(&injected);
                        events.push(injected);
                    }
                    if !self.completed_content_parts.contains(&(index, 0)) {
                        let injected = json!({
                            "type": "response.content_part.done",
                            "item_id": item_id.clone(),
                            "output_index": index,
                            "content_index": 0,
                            "part": {"type": "output_text", "text": text.clone(), "annotations": []}
                        });
                        self.observe(&injected);
                        events.push(injected);
                    }
                    let Some(mut completed) = self.items.get(&index).cloned() else {
                        self.taint();
                        return Vec::new();
                    };
                    completed["status"] = Value::String("completed".into());
                    let injected = json!({
                        "type": "response.output_item.done",
                        "output_index": index,
                        "item": completed
                    });
                    self.observe(&injected);
                    events.push(injected);
                }
                Some("reasoning") => {
                    let mut completed = item;
                    completed["status"] = Value::String("completed".into());
                    let injected = json!({
                        "type": "response.output_item.done",
                        "output_index": index,
                        "item": completed
                    });
                    self.observe(&injected);
                    events.push(injected);
                }
                _ => {}
            }
            if self.tainted {
                return Vec::new();
            }
        }
        events
    }

    pub(crate) fn repair_terminal_event(&mut self, event: &mut Value) -> Option<Value> {
        self.repair_terminal_event_with_request(event, &Value::Null)
    }

    pub(crate) fn repair_terminal_event_with_request(
        &mut self,
        event: &mut Value,
        request_body: &Value,
    ) -> Option<Value> {
        let terminal_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !matches!(
            terminal_type.as_str(),
            "response.completed" | "response.failed" | "response.incomplete"
        ) {
            return None;
        }
        let response = event.get_mut("response")?.as_object_mut()?;
        let default_status = match terminal_type.as_str() {
            "response.completed" => "completed",
            "response.failed" => "failed",
            "response.incomplete" => "incomplete",
            _ => unreachable!(),
        };
        if terminal_type == "response.completed"
            && !response.get("output").is_some_and(Value::is_array)
            && !self.tainted
            && self.open_indexes.is_empty()
        {
            if let Some(output) = self.reconstructed_output(Vec::new()) {
                response.insert("output".into(), Value::Array(output));
            }
        }
        repair_response_snapshot(response, default_status, request_body, false);
        Some(Value::Object(response.clone()))
    }

    pub(crate) fn preserve_generic_terminal_event(&self, event: &mut Value) -> Option<Value> {
        let terminal_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !matches!(
            terminal_type.as_str(),
            "response.completed" | "response.failed" | "response.incomplete"
        ) {
            return None;
        }
        let response = event.get_mut("response")?.as_object_mut()?;
        if terminal_type != "response.completed"
            || response
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| status != "completed")
            || self.tainted
        {
            return Some(Value::Object(response.clone()));
        }
        Some(Value::Object(response.clone()))
    }

    pub(crate) fn repair_compaction_terminal_event(&mut self, event: &mut Value) -> Option<Value> {
        if event.get("type").and_then(Value::as_str) != Some("response.completed") {
            return None;
        }
        let response = event.get_mut("response")?.as_object_mut()?;
        if response.get("status").and_then(Value::as_str) != Some("completed") || self.tainted {
            return Some(Value::Object(response.clone()));
        }
        let terminal = response
            .get("output")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let Some(output) = self.reconstructed_output(terminal) else {
            return Some(Value::Object(response.clone()));
        };
        response.insert("output".into(), Value::Array(output));
        Some(Value::Object(response.clone()))
    }

    pub(crate) fn is_tainted(&self) -> bool {
        self.tainted
    }

    fn reconstructed_output(&mut self, terminal: Vec<Value>) -> Option<Vec<Value>> {
        let mut merged = self.items.clone();
        for (position, item) in terminal.into_iter().enumerate() {
            let index = item
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| self.item_indexes.get(id).copied())
                .unwrap_or(position);
            let value = merged
                .remove(&index)
                .map(|accumulated| merge_snapshot_item(accumulated, &item))
                .unwrap_or(item);
            merged.insert(index, value);
        }
        if !merged.keys().copied().eq(0..merged.len()) {
            self.taint();
            return None;
        }
        let mut output = merged.into_values().collect::<Vec<_>>();
        remove_orphan_tool_results(&mut output);
        for item in &mut output {
            if item.is_object() && item.get("status").is_some() {
                item["status"] = Value::String("completed".into());
            }
        }
        Some(output)
    }

    fn event_index(&mut self, event: &Value) -> Option<usize> {
        let explicit = event
            .get("output_index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok());
        if explicit.is_some_and(|index| index > MAX_SNAPSHOT_REPAIR_INDEX) {
            self.taint();
            return None;
        }
        let item_id = event
            .get("item_id")
            .and_then(Value::as_str)
            .or_else(|| event.pointer("/item/id").and_then(Value::as_str));
        let known = item_id.and_then(|id| self.item_indexes.get(id).copied());
        if matches!((explicit, known), (Some(left), Some(right)) if left != right) {
            self.taint();
            return None;
        }
        if let (Some(index), Some(id)) = (explicit, item_id) {
            if self
                .items
                .get(&index)
                .and_then(|item| item.get("id"))
                .and_then(Value::as_str)
                .is_some_and(|existing| existing != id)
            {
                self.taint();
                return None;
            }
        }
        explicit.or(known)
    }

    fn subindex(&mut self, event: &Value, field: &str) -> Option<usize> {
        let index = event.get(field).and_then(Value::as_u64).unwrap_or(0);
        let index = usize::try_from(index).ok()?;
        if index > MAX_SNAPSHOT_REPAIR_INDEX {
            self.taint();
            None
        } else {
            Some(index)
        }
    }

    fn enforce_size_limit(&mut self) {
        if serde_json::to_vec(&self.items)
            .is_ok_and(|encoded| encoded.len() > MAX_SNAPSHOT_REPAIR_BYTES)
        {
            self.taint();
        }
    }

    pub(super) fn taint(&mut self) {
        self.tainted = true;
        self.items.clear();
        self.item_indexes.clear();
        self.open_indexes.clear();
        self.closed_indexes.clear();
        self.open_content_parts.clear();
        self.completed_text_parts.clear();
        self.completed_content_parts.clear();
    }

    fn tracked_item_mut<'a>(
        &'a mut self,
        index: usize,
        event: &Value,
        expected_type: &str,
    ) -> Option<&'a mut Value> {
        let item_id = match event.get("item_id") {
            None => None,
            Some(value) => match value.as_str().filter(|item_id| !item_id.trim().is_empty()) {
                Some(item_id) => Some(item_id),
                None => {
                    self.taint();
                    return None;
                }
            },
        };
        let proven = self.open_indexes.contains(&index)
            && self.items.get(&index).is_some_and(|item| {
                item.get("type").and_then(Value::as_str) == Some(expected_type)
                    && item_id.is_none_or(|item_id| {
                        item.get("id").and_then(Value::as_str) == Some(item_id)
                    })
            });
        if !proven {
            self.taint();
            return None;
        }
        self.items.get_mut(&index)
    }

    fn observe_scalar_stream(
        &mut self,
        event: &Value,
        kind: &str,
        item_type: &str,
        output_field: &str,
    ) {
        let Some(index) = self.event_index(event) else {
            return;
        };
        let event_field = if kind.ends_with(".done") {
            output_field
        } else {
            "delta"
        };
        let Some(value) = event.get(event_field).and_then(Value::as_str) else {
            return;
        };
        let Some(item) = self.tracked_item_mut(index, event, item_type) else {
            return;
        };
        if kind.ends_with(".done") {
            item[output_field] = Value::String(value.to_string());
        } else {
            append_string(&mut item[output_field], value);
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SparseSnapshotRepairApplied;

fn ensure_array_slot(object: &mut Value, field: &str, index: usize, create: impl Fn() -> Value) {
    if !object.get(field).is_some_and(Value::is_array) {
        object[field] = Value::Array(Vec::new());
    }
    let values = object[field].as_array_mut().expect("array initialized");
    while values.len() <= index {
        values.push(create());
    }
}

fn append_string(target: &mut Value, value: &str) {
    let text = target.as_str().unwrap_or_default();
    *target = Value::String(format!("{text}{value}"));
}

fn structurally_valid_tool_choice(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|choice| !choice.trim().is_empty())
        || value
            .as_object()
            .and_then(|choice| choice.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|choice_type| !choice_type.trim().is_empty())
}

fn repair_output_text_part(part: &mut Value) {
    let Some(part) = part.as_object_mut() else {
        return;
    };
    if part.get("type").and_then(Value::as_str) != Some("output_text") {
        return;
    }
    if !part.get("text").is_some_and(Value::is_string) {
        part.insert("text".into(), Value::String(String::new()));
    }
    if !part.get("annotations").is_some_and(Value::is_array) {
        part.insert("annotations".into(), Value::Array(Vec::new()));
    }
}

fn repair_summary_part(part: &mut Value) {
    let Some(part) = part.as_object_mut() else {
        return;
    };
    if part.get("type").and_then(Value::as_str) == Some("summary_text")
        && !part.get("text").is_some_and(Value::is_string)
    {
        part.insert("text".into(), Value::String(String::new()));
    }
}

fn repair_snapshot_output_item(item: &mut Value, inferred_status: Option<&str>) {
    let Some(item) = item.as_object_mut() else {
        return;
    };
    let item_type = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    match item_type.as_str() {
        "message" => {
            item.insert("role".into(), Value::String("assistant".into()));
            if !item.get("content").is_some_and(Value::is_array) {
                item.insert("content".into(), Value::Array(Vec::new()));
            }
            if let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) {
                for part in content {
                    repair_output_text_part(part);
                }
            }
        }
        "reasoning" => {
            if !item.get("summary").is_some_and(Value::is_array) {
                item.insert("summary".into(), Value::Array(Vec::new()));
            }
            if let Some(summary) = item.get_mut("summary").and_then(Value::as_array_mut) {
                for part in summary {
                    repair_summary_part(part);
                }
            }
        }
        _ => {}
    }
    if item
        .get("status")
        .and_then(Value::as_str)
        .is_none_or(|status| status.trim().is_empty())
    {
        if let Some(status) = inferred_status {
            item.insert("status".into(), Value::String(status.into()));
        }
    }
}

fn repair_response_snapshot(
    response: &mut serde_json::Map<String, Value>,
    default_status: &str,
    request_body: &Value,
    ensure_output: bool,
) {
    if ensure_output && !response.get("output").is_some_and(Value::is_array) {
        response.insert("output".into(), Value::Array(Vec::new()));
    }
    let effective_status = response
        .get("status")
        .and_then(Value::as_str)
        .filter(|status| !status.trim().is_empty())
        .unwrap_or(default_status)
        .to_string();
    let output_status = matches!(effective_status.as_str(), "completed" | "incomplete")
        .then_some(effective_status.as_str());
    if let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) {
        for item in output {
            repair_snapshot_output_item(item, output_status);
        }
    }
    if !response
        .get("parallel_tool_calls")
        .is_some_and(Value::is_boolean)
    {
        response.insert(
            "parallel_tool_calls".into(),
            Value::Bool(
                request_body
                    .get("parallel_tool_calls")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            ),
        );
    }
    if !response
        .get("tool_choice")
        .is_some_and(structurally_valid_tool_choice)
    {
        let tool_choice = request_body
            .get("tool_choice")
            .filter(|choice| structurally_valid_tool_choice(choice))
            .cloned()
            .unwrap_or_else(|| Value::String("auto".into()));
        response.insert("tool_choice".into(), tool_choice);
    }
    if !response.get("tools").is_some_and(Value::is_array) {
        response.insert(
            "tools".into(),
            request_body
                .get("tools")
                .filter(|tools| tools.is_array())
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new())),
        );
    }
    if response
        .get("status")
        .and_then(Value::as_str)
        .is_none_or(|status| status.trim().is_empty())
    {
        response.insert("status".into(), Value::String(default_status.into()));
    }
    if let Some(usage) = response.get_mut("usage") {
        crate::translate::drop_incomplete_input_tokens_details(usage);
    }
}

pub(crate) fn repair_responses_snapshot_json(value: &mut Value, request_body: &Value) {
    let Some(response) = value.as_object_mut() else {
        return;
    };
    repair_response_snapshot(response, "completed", request_body, true);
}

pub(crate) fn repair_responses_snapshot_event(event: &mut Value, request_body: &Value) {
    if !event.is_object() {
        return;
    }
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if let Some(default_status) = match event_type.as_str() {
        "response.created" | "response.in_progress" => Some("in_progress"),
        "response.completed" => Some("completed"),
        "response.failed" => Some("failed"),
        "response.incomplete" => Some("incomplete"),
        "response.queued" => Some("queued"),
        _ => None,
    } {
        if let Some(response) = event.get_mut("response").and_then(Value::as_object_mut) {
            repair_response_snapshot(response, default_status, request_body, false);
        }
    }
    match event_type.as_str() {
        "response.output_item.added" => {
            if let Some(item) = event.get_mut("item") {
                repair_snapshot_output_item(item, Some("in_progress"));
            }
        }
        "response.output_item.done" => {
            if let Some(item) = event.get_mut("item") {
                repair_snapshot_output_item(item, Some("completed"));
            }
        }
        "response.output_text.delta" | "response.output_text.done" => {
            if !event.get("logprobs").is_some_and(Value::is_array) {
                event["logprobs"] = Value::Array(Vec::new());
            }
            if event_type == "response.output_text.done"
                && !event.get("text").is_some_and(Value::is_string)
            {
                event["text"] = Value::String(String::new());
            }
        }
        "response.content_part.added" | "response.content_part.done" => {
            if let Some(part) = event.get_mut("part") {
                repair_output_text_part(part);
            }
        }
        "response.reasoning_summary_part.added" | "response.reasoning_summary_part.done" => {
            if let Some(part) = event.get_mut("part") {
                repair_summary_part(part);
            }
        }
        _ => {}
    }
}

fn merge_snapshot_item(mut accumulated: Value, terminal: &Value) -> Value {
    merge_snapshot_value(&mut accumulated, terminal);
    accumulated
}

fn merge_snapshot_value(accumulated: &mut Value, terminal: &Value) {
    match (accumulated, terminal) {
        (Value::Object(left), Value::Object(right)) => {
            for (key, value) in right {
                if let Some(existing) = left.get_mut(key) {
                    merge_snapshot_value(existing, value);
                } else {
                    left.insert(key.clone(), value.clone());
                }
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            for (index, value) in right.iter().enumerate() {
                if let Some(existing) = left.get_mut(index) {
                    merge_snapshot_value(existing, value);
                } else {
                    left.push(value.clone());
                }
            }
        }
        (Value::String(left), Value::String(right)) => {
            if right.len() >= left.len() || !left.starts_with(right.as_str()) {
                *left = right.clone();
            }
        }
        (left, Value::Null) if !left.is_null() => {}
        (left, right) => *left = right.clone(),
    }
}

fn remove_orphan_tool_results(output: &mut Vec<Value>) {
    let calls = output
        .iter()
        .filter(|item| {
            matches!(
                item.get("type").and_then(Value::as_str),
                Some("function_call" | "custom_tool_call" | "tool_search_call")
            )
        })
        .filter_map(|item| item.get("call_id").and_then(Value::as_str))
        .map(str::to_string)
        .collect::<HashSet<_>>();
    output.retain(|item| {
        let is_result = matches!(
            item.get("type").and_then(Value::as_str),
            Some("function_call_output" | "custom_tool_call_output" | "tool_search_output")
        );
        !is_result
            || item
                .get("call_id")
                .and_then(Value::as_str)
                .is_some_and(|call_id| calls.contains(call_id))
    });
}

pub(super) struct SnapshotRepairFrame {
    pub(super) bytes: Bytes,
    pub(super) terminal: bool,
    pub(super) response: Option<Value>,
}

/// Snapshot repair buffers only until an SSE frame boundary. Already-canonical
/// frames are emitted byte-for-byte, including their original field layout and
/// CRLF/LF delimiter. Sparse lifecycle and terminal frames are reserialized so
/// their canonical backfills are visible to the downstream client.
pub(super) fn drain_snapshot_repair_frames(
    pending: &mut Vec<u8>,
    bytes: &[u8],
    usage: &mut TokenUsage,
    raw_snapshot: &mut ResponsesSnapshotAccumulator,
    snapshot: &mut ResponsesSnapshotAccumulator,
    request_body: &Value,
) -> Result<Vec<SnapshotRepairFrame>, String> {
    pending.extend_from_slice(bytes);
    let mut frames = Vec::new();
    while let Some((boundary, separator_len)) = sse_frame_boundary(pending) {
        let raw = pending
            .drain(..boundary.saturating_add(separator_len))
            .collect::<Vec<_>>();
        let Some(mut value) = parse_sse_frame(&raw[..boundary])? else {
            frames.push(SnapshotRepairFrame {
                bytes: Bytes::from(raw),
                terminal: false,
                response: None,
            });
            continue;
        };
        let original = value.clone();
        usage.merge_max(TokenUsage::from_json(&value));
        raw_snapshot.observe(&value);
        if raw_snapshot.is_tainted() {
            snapshot.taint();
        }
        repair_responses_snapshot_event(&mut value, request_body);
        let prefix = snapshot.injected_events_before(&value);
        for injected in prefix {
            let event = injected
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("message");
            frames.push(SnapshotRepairFrame {
                bytes: Bytes::from(sse(event, &injected)),
                terminal: false,
                response: None,
            });
        }
        snapshot.observe(&value);
        let closing = snapshot.closing_events_before_terminal(&value);
        for injected in closing {
            let event = injected
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("message");
            frames.push(SnapshotRepairFrame {
                bytes: Bytes::from(sse(event, &injected)),
                terminal: false,
                response: None,
            });
        }
        let response = snapshot
            .repair_terminal_event_with_request(&mut value, request_body)
            .map(|response| raw_snapshot.response_for_continuation(response));
        let terminal = response.is_some();
        let bytes = if value != original {
            let event = value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("message");
            Bytes::from(sse(event, &value))
        } else {
            Bytes::from(raw)
        };
        frames.push(SnapshotRepairFrame {
            bytes,
            terminal,
            response,
        });
    }
    Ok(frames)
}
