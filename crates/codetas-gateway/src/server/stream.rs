use super::*;

const MAX_SNAPSHOT_REPAIR_BYTES: usize = 8 * 1024 * 1024;
const MAX_SNAPSHOT_REPAIR_INDEX: usize = 4_096;

#[derive(Default)]
pub(crate) struct ResponsesSnapshotAccumulator {
    items: std::collections::BTreeMap<usize, Value>,
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
        let kind = event.get("type").and_then(Value::as_str).unwrap_or_default();
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
                ensure_array_slot(item, "content", content_index, || {
                    json!({"type": "output_text", "text": "", "annotations": []})
                });
                let field = if kind.ends_with(".done") { "text" } else { "delta" };
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
                let Some(item) = self.tracked_item_mut(index, event, "message") else {
                    return;
                };
                if let Some(part) = event.get("part").cloned() {
                    ensure_array_slot(item, "content", content_index, || Value::Null);
                    let current = std::mem::take(&mut item["content"][content_index]);
                    item["content"][content_index] = merge_snapshot_item(current, &part);
                }
                self.open_content_parts.insert((index, content_index));
                if kind.ends_with(".done") {
                    self.completed_content_parts.insert((index, content_index));
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
                ensure_array_slot(item, "summary", summary_index, || {
                    json!({"type": "summary_text", "text": ""})
                });
                let field = if kind.ends_with(".done") { "text" } else { "delta" };
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
                ensure_array_slot(item, "content", content_index, || {
                    json!({"type": "reasoning_text", "text": ""})
                });
                let field = if kind.ends_with(".done") { "text" } else { "delta" };
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
            "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done" => {
                let Some(index) = self.event_index(event) else {
                    return;
                };
                let _ = self.tracked_item_mut(index, event, "reasoning");
            }
            "response.function_call_arguments.delta"
            | "response.function_call_arguments.done" => {
                self.observe_scalar_stream(event, kind, "function_call", "arguments");
            }
            "response.custom_tool_call_input.delta"
            | "response.custom_tool_call_input.done" => {
                self.observe_scalar_stream(event, kind, "custom_tool_call", "input");
            }
            _ => {}
        }
        self.enforce_size_limit();
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
            || self.items.get(&index).and_then(|item| item.get("type")).and_then(Value::as_str)
                != Some("message")
        {
            return Vec::new();
        }
        let Some(item_id) = self.items.get(&index)
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
        })
        {
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
        if self.tainted
            || event.get("type").and_then(Value::as_str) != Some("response.completed")
        {
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
                    let text = item.pointer("/content/0/text")
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
            .unwrap_or_default();
        if !matches!(
            terminal_type,
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
            if self.items.get(&index).and_then(|item| item.get("id"))
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
        let index = event
            .get(field)
            .and_then(Value::as_u64)
            .unwrap_or(0);
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

    fn taint(&mut self) {
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
        let Some(index) = self.event_index(event) else { return };
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

fn ensure_array_slot(
    object: &mut Value,
    field: &str,
    index: usize,
    create: impl Fn() -> Value,
) {
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
    value.as_str().is_some_and(|choice| !choice.trim().is_empty())
        || value
            .as_object()
            .and_then(|choice| choice.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|choice_type| !choice_type.trim().is_empty())
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
                    let Some(part) = part.as_object_mut() else {
                        continue;
                    };
                    if part.get("type").and_then(Value::as_str) == Some("output_text") {
                        if !part.get("text").is_some_and(Value::is_string) {
                            part.insert("text".into(), Value::String(String::new()));
                        }
                        if !part.get("annotations").is_some_and(Value::is_array) {
                            part.insert("annotations".into(), Value::Array(Vec::new()));
                        }
                    }
                }
            }
        }
        "reasoning" => {
            if !item.get("summary").is_some_and(Value::is_array) {
                item.insert("summary".into(), Value::Array(Vec::new()));
            }
            if let Some(summary) = item.get_mut("summary").and_then(Value::as_array_mut) {
                for part in summary {
                    let Some(part) = part.as_object_mut() else {
                        continue;
                    };
                    if part.get("type").and_then(Value::as_str) == Some("summary_text")
                        && !part.get("text").is_some_and(Value::is_string)
                    {
                        part.insert("text".into(), Value::String(String::new()));
                    }
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
}

pub(crate) fn repair_responses_snapshot_json(value: &mut Value, request_body: &Value) {
    let Some(response) = value.as_object_mut() else {
        return;
    };
    repair_response_snapshot(response, "completed", request_body, true);
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

pub(crate) fn repairing_responses_stream(
    upstream: reqwest::Response,
    limit: u64,
    idle_timeout: Duration,
    observation: ObservationSeed,
    mut repair: ResponsesItemIdRepair,
    response_state: Arc<ResponseStateStore>,
    request_body: Value,
    force_record: bool,
    snapshot_repair: bool,
) -> Response<Body> {
    let status = upstream.status();
    let mut source = upstream.bytes_stream();
    let output = stream! {
        let mut completion = StreamObservation::new(observation);
        let mut usage = TokenUsage::default();
        let mut pending = Vec::new();
        let mut received = 0_u64;
        let mut failure = None;
        let mut terminal_response = None;
        let mut terminal_recorded = false;
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        loop {
            let item = match tokio::time::timeout(idle_timeout, source.next()).await {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(_) => {
                    failure = Some("stream_idle_timeout");
                    yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                        "provider stream exceeded the idle timeout",
                    ));
                    break;
                }
            };
            match item {
                Ok(bytes) => {
                    received = received.saturating_add(bytes.len() as u64);
                    if received > limit {
                        failure = Some("response_too_large");
                        yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                            "provider response exceeded the configured limit",
                        ));
                        break;
                    }
                    let values = match drain_sse_values(&mut pending, &bytes) {
                        Ok(values) => values,
                        Err(error) => {
                            failure = Some("invalid_provider_stream");
                            yield Err::<Bytes, std::io::Error>(std::io::Error::other(error));
                            break;
                        }
                    };
                    for mut value in values {
                        let mut injected = if snapshot_repair {
                            snapshot.injected_events_before(&value)
                        } else {
                            Vec::new()
                        };
                        snapshot.observe(&value);
                        if snapshot_repair {
                            injected.extend(snapshot.closing_events_before_terminal(&value));
                        }
                        for mut injected_event in injected {
                            repair.repair_event(&mut injected_event);
                            usage.merge_max(TokenUsage::from_json(&injected_event));
                            let event = injected_event
                                .get("type")
                                .and_then(Value::as_str)
                                .unwrap_or("message");
                            yield Ok(Bytes::from(sse(event, &injected_event)));
                        }
                        repair.repair_event(&mut value);
                        let mut repaired_response = if snapshot_repair {
                            snapshot.repair_terminal_event_with_request(&mut value, &request_body)
                        } else {
                            snapshot.preserve_generic_terminal_event(&mut value)
                        };
                        if snapshot_repair && repaired_response.is_some() {
                            // Reconstruction uses the raw lifecycle collector by design. Repair
                            // the reconstructed terminal once more so raw placeholder/invalid IDs
                            // cannot be reintroduced after the first client-facing repair pass.
                            repair.repair_event(&mut value);
                            repaired_response = value.get("response").cloned();
                        }
                        usage.merge_max(TokenUsage::from_json(&value));
                        match value.get("type").and_then(Value::as_str) {
                            Some(
                                "response.completed" | "response.failed" | "response.incomplete",
                            ) => {
                                if terminal_response.is_none() {
                                    terminal_response = repaired_response
                                        .or_else(|| value.get("response").cloned());
                                }
                            }
                            _ => {}
                        }
                        if !terminal_recorded {
                            if let Some(response) = terminal_response.as_ref() {
                                response_state.remember(&request_body, response, force_record);
                                completion.finish(status, None, usage.clone());
                                terminal_recorded = true;
                            }
                        }
                        let event = value
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("message");
                        yield Ok(Bytes::from(sse(event, &value)));
                    }
                }
                Err(error) => {
                    failure = Some("upstream_stream_error");
                    yield Err(std::io::Error::other(error.to_string()));
                    break;
                }
            }
        }
        if !terminal_recorded {
            if let Some(response) = terminal_response {
                crate::debug::log(&format!(
                    "remember stream: id={} status={} force={} out={}",
                    response.get("id").and_then(serde_json::Value::as_str).unwrap_or(""),
                    response.get("status").and_then(serde_json::Value::as_str).unwrap_or(""),
                    force_record,
                    response.get("output").and_then(serde_json::Value::as_array).map(|a| a.len()).unwrap_or(0)
                ));
                response_state.remember(&request_body, &response, force_record);
            }
        }
        if failure.is_none() && !pending.iter().all(u8::is_ascii_whitespace) {
            failure = Some("invalid_provider_stream");
            yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                "provider stream ended with an incomplete SSE frame",
            ));
        }
        completion.finish(
            if failure.is_some() { StatusCode::BAD_GATEWAY } else { status },
            failure,
            usage,
        );
    };
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(output))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "gateway_error",
                "failed to build response",
            )
        });
    if snapshot_repair {
        response
            .extensions_mut()
            .insert(SparseSnapshotRepairApplied);
    }
    response
}

pub(crate) fn passthrough_stream(
    upstream: reqwest::Response,
    limit: u64,
    idle_timeout: Duration,
    observation: ObservationSeed,
    response_state: Arc<ResponseStateStore>,
    request_body: Value,
    force_record: bool,
    snapshot_repair: bool,
) -> Response<Body> {
    let status = upstream.status();
    let mut source = upstream.bytes_stream();
    let output = stream! {
        let mut completion = StreamObservation::new(observation);
        let mut usage = TokenUsage::default();
        let mut pending = Vec::new();
        let mut received = 0_u64;
        let mut failure = None;
        let mut terminal_response = None;
        let mut terminal_recorded = false;
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        loop {
            let item = match tokio::time::timeout(idle_timeout, source.next()).await {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(_) => {
                    failure = Some("stream_idle_timeout");
                    yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                        "provider stream exceeded the idle timeout",
                    ));
                    break;
                }
            };
            match item {
                Ok(bytes) => {
                    received = received.saturating_add(bytes.len() as u64);
                    if received > limit {
                        failure = Some("response_too_large");
                        yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                            "provider response exceeded the configured limit",
                        ));
                        break;
                    }
                    if snapshot_repair {
                        let frames = match drain_snapshot_repair_frames(
                            &mut pending,
                            &bytes,
                            &mut usage,
                            &mut snapshot,
                            &request_body,
                        ) {
                            Ok(frames) => frames,
                            Err(error) => {
                                failure = Some("invalid_provider_stream");
                                yield Err::<Bytes, std::io::Error>(std::io::Error::other(error));
                                break;
                            }
                        };
                        let mut terminal = false;
                        for frame in frames {
                            if frame.terminal {
                                terminal = true;
                                terminal_response = frame.response;
                                if let Some(response) = terminal_response.as_ref() {
                                    response_state.remember(&request_body, response, force_record);
                                    terminal_recorded = true;
                                }
                                completion.finish(status, None, usage.clone());
                            }
                            yield Ok(frame.bytes);
                            if terminal {
                                break;
                            }
                        }
                        if terminal {
                            break;
                        }
                        continue;
                    }
                    let terminal = inspect_sse_usage(
                        &mut pending,
                        &bytes,
                        &mut usage,
                        &mut terminal_response,
                        &mut snapshot,
                    );
                    // Codex closes the downstream body as soon as it receives a terminal
                    // Responses event. Record success before yielding that chunk so Drop does
                    // not misclassify a completed turn as a cancelled 502.
                    if terminal {
                        if let Some(response) = terminal_response.as_ref() {
                            response_state.remember(&request_body, response, force_record);
                            terminal_recorded = true;
                        }
                        completion.finish(status, None, usage.clone());
                    }
                    yield Ok(bytes);
                    if terminal {
                        break;
                    }
                }
                Err(error) => {
                    failure = Some("upstream_stream_error");
                    yield Err(std::io::Error::other(error.to_string()));
                    break;
                }
            }
        }
        if !terminal_recorded {
            if let Some(response) = terminal_response {
                crate::debug::log(&format!(
                    "remember stream2: id={} status={} force={} out={}",
                    response.get("id").and_then(serde_json::Value::as_str).unwrap_or(""),
                    response.get("status").and_then(serde_json::Value::as_str).unwrap_or(""),
                    force_record,
                    response.get("output").and_then(serde_json::Value::as_array).map(|a| a.len()).unwrap_or(0)
                ));
                response_state.remember(&request_body, &response, force_record);
            }
        }
        if failure.is_none()
            && snapshot_repair
            && !pending.iter().all(u8::is_ascii_whitespace)
        {
            failure = Some("invalid_provider_stream");
            yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                "provider stream ended with an incomplete SSE frame",
            ));
        }
        completion.finish(
            if failure.is_some() { StatusCode::BAD_GATEWAY } else { status },
            failure,
            usage,
        );
    };
    let mut response = Response::builder()
        .status(status)
        // This helper is only used for a validated streaming Responses request.
        // Normalize the downstream type even when the upstream mislabeled its SSE body.
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(output))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "gateway_error",
                "failed to build response",
            )
        });
    if snapshot_repair {
        response
            .extensions_mut()
            .insert(SparseSnapshotRepairApplied);
    }
    response
}

pub(crate) async fn responses_json_response(
    upstream: reqwest::Response,
    limit: u64,
    observation: ObservationSeed,
    mut repair: Option<ResponsesItemIdRepair>,
    response_state: Arc<ResponseStateStore>,
    request_body: Value,
    force_record: bool,
    snapshot_repair: bool,
) -> Response<Body> {
    let status = upstream.status();
    let value = match bounded_json(upstream, limit).await {
        Ok(value) => value,
        Err(message) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("invalid_provider_response"),
                TokenUsage::default(),
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            );
        }
    };
    let mut value = value;
    if snapshot_repair {
        repair_responses_snapshot_json(&mut value, &request_body);
    }
    if let Some(repair) = repair.as_mut() {
        repair.repair_response(&mut value);
    }
    crate::debug::log(&format!(
        "remember json: id={} status={} force={} out={}",
        value.get("id").and_then(serde_json::Value::as_str).unwrap_or(""),
        value.get("status").and_then(serde_json::Value::as_str).unwrap_or(""),
        force_record,
        value.get("output").and_then(serde_json::Value::as_array).map(|a| a.len()).unwrap_or(0)
    ));
    response_state.remember(&request_body, &value, force_record);
    observation.finish(status, None, TokenUsage::from_json(&value));
    json_response(status, value)
}

pub(crate) async fn chat_json_response(
    upstream: reqwest::Response,
    exposed_model: &str,
    limit: u64,
    observation: ObservationSeed,
    tool_map: ResponseToolMap,
    response_state: Arc<ResponseStateStore>,
    request_body: Value,
    force_record: bool,
) -> Response<Body> {
    let value = match bounded_json(upstream, limit).await {
        Ok(value) => value,
        Err(message) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("invalid_provider_response"),
                TokenUsage::default(),
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            );
        }
    };
    match chat_to_response(&value, exposed_model, &tool_map) {
        Ok(value) => {
            response_state.remember(&request_body, &value, force_record);
            observation.finish(StatusCode::OK, None, TokenUsage::from_json(&value));
            json_response(StatusCode::OK, value)
        }
        Err(message) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("response_translation_error"),
                TokenUsage::default(),
            );
            error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            )
        }
    }
}

pub(crate) async fn adapted_json_response<F>(
    upstream: reqwest::Response,
    exposed_model: &str,
    adapter: F,
    limit: u64,
    observation: ObservationSeed,
    restore_escaped_anthropic_names: bool,
    tool_map: ResponseToolMap,
    response_state: Arc<ResponseStateStore>,
    request_body: Value,
    force_record: bool,
) -> Response<Body>
where
    F: Fn(&Value, &str, &ResponseToolMap) -> Result<Value, String>,
{
    let mut value = match bounded_json(upstream, limit).await {
        Ok(value) => value,
        Err(message) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("invalid_provider_response"),
                TokenUsage::default(),
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            );
        }
    };
    if restore_escaped_anthropic_names {
        restore_anthropic_tool_names(&mut value);
    }
    match adapter(&value, exposed_model, &tool_map) {
        Ok(value) => {
            response_state.remember(&request_body, &value, force_record);
            observation.finish(StatusCode::OK, None, TokenUsage::from_json(&value));
            json_response(StatusCode::OK, value)
        }
        Err(message) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("response_translation_error"),
                TokenUsage::default(),
            );
            error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            )
        }
    }
}

pub(crate) enum StreamAdapter {
    Chat,
    Anthropic {
        state: AnthropicStreamState,
        subscription_oauth: bool,
    },
    Gemini,
}

pub(crate) fn translated_stream_response(
    upstream: reqwest::Response,
    exposed_model: String,
    mut adapter: StreamAdapter,
    limit: u64,
    idle_timeout: Duration,
    observation: ObservationSeed,
    restore_escaped_anthropic_names: bool,
    tool_map: ResponseToolMap,
    progress_policy: ToolProgressPolicy,
    response_state: Arc<ResponseStateStore>,
    request_body: Value,
    force_record: bool,
    tolerate_incomplete_eof: bool,
) -> Response<Body> {
    let mut upstream_stream = upstream.bytes_stream();
    let output = stream! {
        let mut completion = StreamObservation::new(observation);
        let mut usage = TokenUsage::default();
        let (mut state, initial) =
            ChatStreamState::new_with_progress(exposed_model, tool_map, progress_policy);
        for event in initial {
            yield Ok::<Bytes, Infallible>(Bytes::from(event));
        }
        let mut pending = Vec::new();
        let mut failure = None;
        let mut received = 0_u64;
        let mut response_snapshot_tool_count = 0_usize;
        let mut flushed_tolerated_eof = false;
        'upstream: loop {
            let mut synthetic_eof_chunk = false;
            let chunk = match tokio::time::timeout(idle_timeout, upstream_stream.next()).await {
                Ok(Some(chunk)) => chunk,
                Ok(None) if tolerated_eof_delimiter(
                    &pending,
                    tolerate_incomplete_eof,
                    flushed_tolerated_eof,
                ).is_some() => {
                    flushed_tolerated_eof = true;
                    synthetic_eof_chunk = true;
                    // Some Anthropic-compatible providers omit the final SSE
                    // delimiter. Feed one synthetic delimiter through the
                    // normal strict parser so a complete final frame is
                    // emitted, while truncated JSON still fails.
                    Ok(Bytes::from_static(b"\n\n"))
                }
                Ok(None) => break,
                Err(_) => {
                    yield Ok(Bytes::from(state.fail("provider stream exceeded the idle timeout")));
                    failure = Some("stream_idle_timeout");
                    break;
                }
            };
            match chunk {
                Ok(bytes) => {
                    if !synthetic_eof_chunk {
                        received = received.saturating_add(bytes.len() as u64);
                    }
                    if received > limit {
                        yield Ok(Bytes::from(state.fail("provider stream exceeded the configured limit")));
                        failure = Some("response_too_large");
                        break;
                    }
                    let values = match drain_sse_values(&mut pending, &bytes) {
                        Ok(values) => values,
                        Err(error) => {
                            yield Ok(Bytes::from(state.fail(&format!(
                                "provider returned an invalid SSE frame: {error}"
                            ))));
                            failure = Some("invalid_provider_stream");
                            break 'upstream;
                        }
                    };
                    for mut value in values {
                        if restore_escaped_anthropic_names {
                            restore_anthropic_stream_tool_names(&mut value);
                        }
                        usage.merge_max(TokenUsage::from_json(&value));
                        let converted = match &mut adapter {
                            StreamAdapter::Chat => {
                                if let Some(error) = value.get("error") {
                                    let message = error
                                        .get("message")
                                        .and_then(Value::as_str)
                                        .unwrap_or("provider reported a streaming error")
                                        .to_string();
                                    let context_window_exceeded = serde_json::to_vec(error)
                                        .ok()
                                        .is_some_and(|body| {
                                            upstream_context_window_exceeded(&body)
                                        });
                                    Err((message, context_window_exceeded))
                                } else {
                                    Ok(Some(value))
                                }
                            }
                            StreamAdapter::Anthropic {
                                state: anthropic_state,
                                subscription_oauth,
                            } => {
                                anthropic_stream_to_chat(
                                    &value,
                                    anthropic_state,
                                    *subscription_oauth,
                                )
                                .map_err(|message| {
                                    let context_window_exceeded =
                                        context_window_error_message(&message);
                                    (message, context_window_exceeded)
                                })
                            }
                            StreamAdapter::Gemini => gemini_stream_to_chat(&value).map_err(
                                |message| {
                                    let context_window_exceeded =
                                        context_window_error_message(&message);
                                    (message, context_window_exceeded)
                                },
                            ),
                        };
                        match converted {
                            Ok(Some(chunk)) => {
                                usage.merge_max(TokenUsage::from_json(&chunk));
                                let events = state.push_chat_chunk(&chunk);
                                let actionable_tool_call_count =
                                    state.actionable_tool_call_count();
                                if actionable_tool_call_count > response_snapshot_tool_count {
                                    let response = state.completed_response_snapshot();
                                    response_state.remember(
                                        &request_body,
                                        &response,
                                        force_record,
                                    );
                                    response_snapshot_tool_count = actionable_tool_call_count;
                                }
                                for event in events {
                                    if actionable_tool_call_count > 0
                                        && translated_event_is_function_call_delta(&event)
                                    {
                                        completion.succeed_if_cancelled(usage.clone());
                                    }
                                    yield Ok(Bytes::from(event));
                                }
                            }
                            Ok(None) => {}
                            Err((_message, context_window_exceeded)) => {
                                if context_window_exceeded {
                                    yield Ok(Bytes::from(state.fail_with_code(
                                        "context_length_exceeded",
                                        "Your input exceeds the context window of this model.",
                                    )));
                                    failure = Some("context_length_exceeded");
                                } else {
                                    yield Ok(Bytes::from(state.fail(
                                        "The upstream provider reported a streaming error.",
                                    )));
                                    failure = Some("provider_stream_error");
                                }
                                break 'upstream;
                            }
                        }
                    }
                }
                Err(error) => {
                    yield Ok(Bytes::from(state.fail(&format!("upstream stream failed: {error}"))));
                    failure = Some("upstream_stream_error");
                    break;
                }
            }
        }
        if failure.is_none()
            && !pending.iter().all(u8::is_ascii_whitespace)
            && !tolerate_incomplete_eof
        {
            yield Ok(Bytes::from(state.fail("provider stream ended with an incomplete SSE frame")));
            failure = Some("invalid_provider_stream");
        } else if failure.is_none() {
            let (events, response) = state.finish_with_response();
            response_state.remember(&request_body, &response, force_record);
            // Upstream completion and continuation persistence are authoritative. Mark
            // the observation successful before yielding terminal events because Codex
            // may intentionally close the downstream body as soon as it receives them.
            completion.finish(StatusCode::OK, None, usage.clone());
            for event in events {
                yield Ok(Bytes::from(event));
            }
        }
        completion.finish(
            if failure.is_some() { StatusCode::BAD_GATEWAY } else { StatusCode::OK },
            failure,
            usage,
        );
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(output))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "gateway_error",
                "failed to build stream",
            )
        })
}

pub(crate) fn tolerated_eof_delimiter(
    pending: &[u8],
    tolerate_incomplete_eof: bool,
    already_flushed: bool,
) -> Option<&'static [u8]> {
    (tolerate_incomplete_eof
        && !already_flushed
        && !pending.iter().all(u8::is_ascii_whitespace))
    .then_some(b"\n\n")
}

pub(crate) fn translated_event_is_function_call_delta(event: &str) -> bool {
    event
        .lines()
        .any(|line| line.trim() == "event: response.function_call_arguments.delta")
}

pub(crate) struct StreamObservation {
    seed: Option<ObservationSeed>,
    cancelled_outcome: Option<(StatusCode, TokenUsage)>,
}

impl StreamObservation {
    pub(crate) fn new(seed: ObservationSeed) -> Self {
        Self {
            seed: Some(seed),
            cancelled_outcome: None,
        }
    }

    fn succeed_if_cancelled(&mut self, usage: TokenUsage) {
        self.cancelled_outcome = Some((StatusCode::OK, usage));
    }

    pub(crate) fn finish(
        &mut self,
        status: StatusCode,
        failure_category: Option<&str>,
        usage: TokenUsage,
    ) {
        if let Some(seed) = self.seed.take() {
            seed.finish(status, failure_category, usage);
        }
    }
}

impl Drop for StreamObservation {
    fn drop(&mut self) {
        if let Some(seed) = self.seed.take() {
            if let Some((status, usage)) = self.cancelled_outcome.take() {
                seed.finish(status, None, usage);
            } else {
                seed.finish(
                    StatusCode::BAD_GATEWAY,
                    Some("stream_cancelled"),
                    TokenUsage::default(),
                );
            }
        }
    }
}

pub(crate) fn inspect_sse_usage(
    pending: &mut Vec<u8>,
    bytes: &[u8],
    usage: &mut TokenUsage,
    terminal_response: &mut Option<Value>,
    snapshot: &mut ResponsesSnapshotAccumulator,
) -> bool {
    match drain_sse_values(pending, bytes) {
        Ok(values) => {
            let mut terminal = false;
            for mut value in values {
                usage.merge_max(TokenUsage::from_json(&value));
                snapshot.observe(&value);
                match value.get("type").and_then(Value::as_str) {
                    Some("response.completed" | "response.failed" | "response.incomplete") => {
                        terminal = true;
                        if terminal_response.is_none() {
                            *terminal_response = snapshot
                                .preserve_generic_terminal_event(&mut value)
                                .or_else(|| value.get("response").cloned());
                        }
                    }
                    _ => {}
                }
            }
            terminal
        }
        Err(_) => {
            pending.clear();
            false
        }
    }
}

struct SnapshotRepairFrame {
    bytes: Bytes,
    terminal: bool,
    response: Option<Value>,
}

/// Snapshot repair buffers only until an SSE frame boundary. Non-terminal
/// frames are emitted byte-for-byte, including their original field layout and
/// CRLF/LF delimiter. A terminal frame is deliberately reserialized because
/// its repaired `response.output` must be visible to the downstream client.
fn drain_snapshot_repair_frames(
    pending: &mut Vec<u8>,
    bytes: &[u8],
    usage: &mut TokenUsage,
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
        usage.merge_max(TokenUsage::from_json(&value));
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
        let original = value.clone();
        let response = snapshot.repair_terminal_event_with_request(&mut value, request_body);
        let terminal = response.is_some();
        let bytes = if terminal && value != original {
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

pub(crate) fn drain_sse_values(pending: &mut Vec<u8>, bytes: &[u8]) -> Result<Vec<Value>, String> {
    pending.extend_from_slice(bytes);
    let mut values = Vec::new();
    while let Some((boundary, separator_len)) = sse_frame_boundary(pending) {
        let frame = pending.drain(..boundary).collect::<Vec<_>>();
        pending.drain(..separator_len);
        if let Some(value) = parse_sse_frame(&frame)? {
            values.push(value);
        }
    }
    Ok(values)
}

fn parse_sse_frame(frame: &[u8]) -> Result<Option<Value>, String> {
    let frame =
        std::str::from_utf8(frame).map_err(|_| "SSE frame is not valid UTF-8".to_string())?;
    let normalized = frame.replace("\r\n", "\n");
    let data = normalized
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() || data == "[DONE]" {
        return Ok(None);
    }
    serde_json::from_str::<Value>(&data)
        .map(Some)
        .map_err(|error| format!("invalid JSON data: {error}"))
}

pub(crate) fn sse_frame_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let lf = bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2));
    let crlf = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4));
    match (lf, crlf) {
        (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
        (Some(boundary), None) | (None, Some(boundary)) => Some(boundary),
        (None, None) => None,
    }
}

#[cfg(test)]
mod eof_tolerance_tests {
    use super::*;

    #[test]
    fn anthropic_eof_tolerance_emits_a_complete_undelimited_final_frame() {
        let mut pending = br#"data: {"type":"message_stop"}"#.to_vec();
        let delimiter = tolerated_eof_delimiter(&pending, true, false)
            .expect("synthetic EOF delimiter");

        let values = drain_sse_values(&mut pending, delimiter).expect("complete final frame");

        assert_eq!(values, vec![json!({"type": "message_stop"})]);
        assert!(pending.is_empty());
    }

    #[test]
    fn anthropic_eof_tolerance_rejects_truncated_final_json() {
        let mut pending = br#"data: {"type":"content_block_delta""#.to_vec();
        let delimiter = tolerated_eof_delimiter(&pending, true, false)
            .expect("synthetic EOF delimiter");

        assert!(drain_sse_values(&mut pending, delimiter).is_err());
    }

    #[test]
    fn ordinary_streams_do_not_synthesize_an_eof_delimiter() {
        let pending = br#"data: {"type":"message_stop"}"#.to_vec();
        assert!(tolerated_eof_delimiter(&pending, false, false).is_none());
        assert!(tolerated_eof_delimiter(&pending, true, true).is_none());
    }
}

#[cfg(test)]
mod snapshot_repair_tests {
    use super::*;

    fn observe(snapshot: &mut ResponsesSnapshotAccumulator, value: Value) {
        snapshot.observe(&value);
    }

    #[test]
    fn open_tool_call_blocks_terminal_reconstruction() {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        observe(&mut snapshot, json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "fc_one", "type": "function_call", "status": "in_progress",
                "call_id": "call_one", "name": "lookup", "arguments": ""}
        }));
        observe(&mut snapshot, json!({
            "type": "response.function_call_arguments.delta", "output_index": 0,
            "item_id": "fc_one", "delta": "{\"q\":"
        }));
        observe(&mut snapshot, json!({
            "type": "response.function_call_arguments.delta", "output_index": 0,
            "item_id": "fc_one", "delta": "\"x\"}"
        }));
        let mut terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_one", "status": "completed"}
        });

        assert!(snapshot.closing_events_before_terminal(&terminal).is_empty());
        snapshot.repair_terminal_event(&mut terminal);
        assert!(terminal["response"].get("output").is_none());
    }

    #[test]
    fn explicit_terminal_output_is_authoritative_even_when_collector_has_more() {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        observe(&mut snapshot, json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "msg_one", "type": "message", "status": "in_progress",
                "role": "assistant", "content": [{"type": "output_text", "text": ""}]}
        }));
        observe(&mut snapshot, json!({
            "type": "response.output_text.delta", "output_index": 0,
            "item_id": "msg_one", "content_index": 0, "delta": "hello world"
        }));
        observe(&mut snapshot, json!({
            "type": "response.output_item.added", "output_index": 1,
            "item": {"id": "rs_one", "type": "reasoning", "status": "in_progress", "summary": []}
        }));
        observe(&mut snapshot, json!({
            "type": "response.reasoning_summary_text.delta", "output_index": 1,
            "item_id": "rs_one", "summary_index": 0, "delta": "checked"
        }));
        let mut terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_partial", "status": "completed", "output": [{
                "id": "msg_one", "type": "message", "status": "completed",
                "role": "assistant", "content": [{"type": "output_text", "text": "hello"}]
            }]}
        });

        snapshot.repair_terminal_event(&mut terminal);

        assert_eq!(terminal["response"]["output"].as_array().map(Vec::len), Some(1));
        assert_eq!(terminal["response"]["output"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn open_orphan_tool_result_blocks_repaired_snapshot() {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        observe(&mut snapshot, json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "out_orphan", "type": "function_call_output",
                "call_id": "missing_call", "output": "unsafe"}
        }));
        let mut terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_orphan", "status": "completed"}
        });

        assert!(snapshot.closing_events_before_terminal(&terminal).is_empty());
        snapshot.repair_terminal_event(&mut terminal);
        assert!(terminal["response"].get("output").is_none());
    }

    #[test]
    fn open_custom_tool_lifecycle_blocks_terminal_reconstruction() {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        observe(&mut snapshot, json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "ct_one", "type": "custom_tool_call", "status": "in_progress",
                "call_id": "call_one", "name": "exec", "input": ""}
        }));
        observe(&mut snapshot, json!({
            "type": "response.custom_tool_call_input.delta", "output_index": 0,
            "item_id": "ct_one", "call_id": "call_one", "name": "exec", "delta": "pwd"
        }));
        observe(&mut snapshot, json!({
            "type": "response.output_item.added", "output_index": 1,
            "item": {"id": "out_one", "type": "custom_tool_call_output",
                "call_id": "call_one", "output": "ok"}
        }));
        let mut terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_tool", "status": "completed"}
        });

        assert!(snapshot.closing_events_before_terminal(&terminal).is_empty());
        snapshot.repair_terminal_event(&mut terminal);
        assert!(terminal["response"].get("output").is_none());
    }

    #[test]
    fn failed_terminal_is_never_reconstructed_or_marked_completed() {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        observe(&mut snapshot, json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "rs_failed", "type": "reasoning", "status": "in_progress",
                "content": []}
        }));
        observe(&mut snapshot, json!({
            "type": "response.reasoning_text.delta", "output_index": 0,
            "item_id": "rs_failed", "content_index": 0, "delta": "partial reasoning"
        }));
        let mut terminal = json!({
            "type": "response.failed",
            "response": {"id": "resp_failed", "status": "failed", "output": []}
        });

        snapshot.repair_terminal_event(&mut terminal);

        assert!(terminal["response"]["output"]
            .as_array()
            .is_some_and(Vec::is_empty));
        assert_eq!(terminal["response"]["status"], "failed");
    }

    #[test]
    fn explicit_empty_terminal_output_is_authoritative() {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        observe(&mut snapshot, json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "msg_hidden", "type": "message", "status": "in_progress",
                "role": "assistant", "content": []}
        }));
        let mut terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_empty", "status": "completed", "output": []}
        });

        let closing = snapshot.closing_events_before_terminal(&terminal);
        snapshot.repair_terminal_event(&mut terminal);

        assert_eq!(closing.last().and_then(|event| event.get("type")).and_then(Value::as_str),
            Some("response.output_item.done"));
        assert!(terminal["response"]["output"]
            .as_array()
            .is_some_and(Vec::is_empty));
    }

    #[test]
    fn open_reasoning_injects_only_output_item_done_before_terminal() {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        observe(&mut snapshot, json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "rs_one", "type": "reasoning", "status": "in_progress",
                "summary": []}
        }));
        observe(&mut snapshot, json!({
            "type": "response.reasoning_summary_text.delta", "output_index": 0,
            "item_id": "rs_one", "summary_index": 0, "delta": "checked"
        }));
        let mut terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_reasoning", "status": "completed"}
        });

        let closing = snapshot.closing_events_before_terminal(&terminal);
        snapshot.repair_terminal_event(&mut terminal);

        assert_eq!(closing.len(), 1);
        assert_eq!(closing[0]["type"], "response.output_item.done");
        assert_eq!(terminal["response"]["output"][0]["summary"][0]["text"], "checked");
    }

    #[test]
    fn generic_http_continuation_preserves_explicit_empty_terminal() {
        let done_reasoning = json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {"id": "rs_one", "type": "reasoning", "status": "completed",
                "summary": [{"type": "summary_text", "text": "checked"}]}
        });
        let done_message = json!({
            "type": "response.output_item.done", "output_index": 1,
            "item": {"id": "msg_one", "type": "message", "status": "completed",
                "role": "assistant", "content": [{"type": "output_text", "text": "hello"}]}
        });
        let completed = json!({
            "type": "response.completed",
            "response": {"id": "resp_generic", "status": "completed", "output": []}
        });
        let bytes = [
            sse("response.output_item.done", &done_reasoning),
            sse("response.output_item.done", &done_message),
            sse("response.completed", &completed),
        ]
        .concat();
        let mut pending = Vec::new();
        let mut usage = TokenUsage::default();
        let mut terminal = None;
        let mut snapshot = ResponsesSnapshotAccumulator::default();

        assert!(inspect_sse_usage(
            &mut pending,
            bytes.as_bytes(),
            &mut usage,
            &mut terminal,
            &mut snapshot,
        ));

        let output = terminal
            .as_ref()
            .and_then(|response| response.get("output"))
            .and_then(Value::as_array)
            .expect("generic continuation output");
        assert!(output.is_empty());
    }

    #[test]
    fn generic_http_continuation_does_not_backfill_missing_output() {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        observe(&mut snapshot, json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {"id": "msg_one", "type": "message", "status": "completed",
                "role": "assistant", "content": []}
        }));
        let mut terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_generic", "status": "completed"}
        });

        snapshot.preserve_generic_terminal_event(&mut terminal);

        assert!(terminal["response"].get("output").is_none());
    }

    #[test]
    fn opt_in_sparse_repair_backfills_only_missing_output() {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        observe(&mut snapshot, json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {"id": "msg_one", "type": "message", "status": "completed",
                "role": "assistant", "content": []}
        }));
        let mut missing = json!({
            "type": "response.completed",
            "response": {"id": "resp_missing", "status": "completed"}
        });
        snapshot.repair_terminal_event(&mut missing);
        assert_eq!(missing["response"]["output"][0]["id"], "msg_one");

        let mut explicit_empty = json!({
            "type": "response.completed",
            "response": {"id": "resp_empty", "status": "completed", "output": []}
        });
        snapshot.repair_terminal_event(&mut explicit_empty);
        assert!(explicit_empty["response"]["output"]
            .as_array()
            .is_some_and(Vec::is_empty));

        let mut malformed = json!({
            "type": "response.completed",
            "response": {"id": "resp_malformed", "status": "completed", "output": {}}
        });
        snapshot.repair_terminal_event(&mut malformed);
        assert_eq!(malformed["response"]["output"][0]["id"], "msg_one");
    }

    #[test]
    fn generic_backfill_never_changes_failed_or_incomplete_terminals() {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        observe(&mut snapshot, json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {"id": "msg_partial", "type": "message", "status": "completed",
                "role": "assistant", "content": []}
        }));

        for (kind, status) in [
            ("response.failed", "failed"),
            ("response.incomplete", "incomplete"),
        ] {
            let mut terminal = json!({
                "type": kind,
                "response": {"id": "resp_nonterminal", "status": status, "output": []}
            });
            snapshot.preserve_generic_terminal_event(&mut terminal);
            assert!(terminal["response"]["output"]
                .as_array()
                .is_some_and(Vec::is_empty));
            assert_eq!(terminal["response"]["status"], status);
        }
    }

    #[test]
    fn deltas_after_output_item_done_taint_all_snapshot_reconstruction() {
        let cases = [
            (
                json!({"id": "msg_closed", "type": "message", "status": "completed",
                    "role": "assistant", "content": [{"type": "output_text", "text": "safe"}]}),
                json!({"type": "response.output_text.delta", "output_index": 0,
                    "item_id": "msg_closed", "content_index": 0, "delta": "evil"}),
            ),
            (
                json!({"id": "rs_closed", "type": "reasoning", "status": "completed",
                    "summary": [{"type": "summary_text", "text": "safe"}]}),
                json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
                    "item_id": "rs_closed", "summary_index": 0, "delta": "evil"}),
            ),
            (
                json!({"id": "fc_closed", "type": "function_call", "status": "completed",
                    "call_id": "call_closed", "name": "lookup", "arguments": "{}"}),
                json!({"type": "response.function_call_arguments.delta", "output_index": 0,
                    "item_id": "fc_closed", "delta": "evil"}),
            ),
            (
                json!({"id": "ct_closed", "type": "custom_tool_call", "status": "completed",
                    "call_id": "call_closed", "name": "exec", "input": "safe"}),
                json!({"type": "response.custom_tool_call_input.delta", "output_index": 0,
                    "item_id": "ct_closed", "delta": "evil"}),
            ),
        ];

        for (item, late_delta) in cases {
            let mut snapshot = ResponsesSnapshotAccumulator::default();
            observe(&mut snapshot, json!({
                "type": "response.output_item.done", "output_index": 0, "item": item
            }));
            observe(&mut snapshot, late_delta);

            let mut sparse_terminal = json!({
                "type": "response.completed",
                "response": {"id": "resp_sparse", "status": "completed"}
            });
            snapshot.repair_terminal_event(&mut sparse_terminal);
            assert!(sparse_terminal["response"].get("output").is_none());

            let mut generic_terminal = json!({
                "type": "response.completed",
                "response": {"id": "resp_generic", "status": "completed", "output": []}
            });
            snapshot.preserve_generic_terminal_event(&mut generic_terminal);
            assert!(generic_terminal["response"]["output"]
                .as_array()
                .is_some_and(Vec::is_empty));
        }
    }

    #[test]
    fn incomplete_terminal_is_never_reconstructed_or_status_rewritten() {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        observe(&mut snapshot, json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "msg_partial", "type": "message", "status": "in_progress",
                "role": "assistant", "content": []}
        }));
        let mut terminal = json!({
            "type": "response.incomplete",
            "response": {"id": "resp_partial", "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"}}
        });

        snapshot.repair_terminal_event(&mut terminal);

        assert_eq!(terminal["response"]["status"], "incomplete");
        assert!(terminal["response"].get("output").is_none());
    }

    #[test]
    fn duplicate_indexes_and_item_id_conflicts_taint_reconstruction() {
        let mut duplicate = ResponsesSnapshotAccumulator::default();
        let added = json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "msg_one", "type": "message", "status": "in_progress",
                "role": "assistant", "content": []}
        });
        observe(&mut duplicate, added.clone());
        observe(&mut duplicate, added);
        let mut duplicate_terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_duplicate", "status": "completed"}
        });
        duplicate.repair_terminal_event(&mut duplicate_terminal);
        assert!(duplicate_terminal["response"].get("output").is_none());

        let mut mismatch = ResponsesSnapshotAccumulator::default();
        observe(&mut mismatch, json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "msg_one", "type": "message", "status": "in_progress",
                "role": "assistant", "content": []}
        }));
        observe(&mut mismatch, json!({
            "type": "response.output_text.done", "output_index": 0,
            "item_id": "msg_other", "content_index": 0, "text": "foreign"
        }));
        let mut mismatch_terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_mismatch", "status": "completed"}
        });
        mismatch.repair_terminal_event(&mut mismatch_terminal);
        assert!(mismatch_terminal["response"].get("output").is_none());
    }

    #[test]
    fn malformed_or_type_mismatched_item_lifecycle_fails_closed() {
        let malformed = [
            json!({
                "type": "response.output_item.done",
                "item": {"id": "msg_one", "type": "message", "status": "completed"}
            }),
            json!({
                "type": "response.output_item.done", "output_index": 1,
                "item": []
            }),
            json!({
                "type": "response.output_item.done", "output_index": 1,
                "item": {"type": "message", "status": "completed"}
            }),
            json!({
                "type": "response.output_item.done", "output_index": 1,
                "item": {"id": "msg_one", "type": "", "status": "completed"}
            }),
        ];

        for event in malformed {
            let mut snapshot = ResponsesSnapshotAccumulator::default();
            observe(&mut snapshot, json!({
                "type": "response.output_item.done", "output_index": 0,
                "item": {"id": "msg_safe", "type": "message", "status": "completed",
                    "role": "assistant", "content": []}
            }));
            observe(&mut snapshot, event);

            let mut sparse = json!({
                "type": "response.completed",
                "response": {"id": "resp_sparse", "status": "completed"}
            });
            snapshot.repair_terminal_event(&mut sparse);
            assert!(sparse["response"].get("output").is_none());

            let mut generic = json!({
                "type": "response.completed",
                "response": {"id": "resp_generic", "status": "completed", "output": []}
            });
            snapshot.preserve_generic_terminal_event(&mut generic);
            assert!(generic["response"]["output"]
                .as_array()
                .is_some_and(Vec::is_empty));
        }

        let mut mismatch = ResponsesSnapshotAccumulator::default();
        observe(&mut mismatch, json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "item_one", "type": "message", "status": "in_progress",
                "role": "assistant", "content": []}
        }));
        observe(&mut mismatch, json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {"id": "item_one", "type": "function_call", "status": "completed",
                "call_id": "call_one", "name": "lookup", "arguments": "{}"}
        }));
        let mut terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_hybrid", "status": "completed"}
        });
        mismatch.repair_terminal_event(&mut terminal);
        assert!(terminal["response"].get("output").is_none());
    }

    #[test]
    fn raw_missing_or_conflicting_ids_taint_before_id_repair() {
        let cases = [
            vec![json!({
                "type": "response.output_item.added", "output_index": 0,
                "item": {"type": "message", "status": "in_progress",
                    "role": "assistant", "content": []}
            })],
            vec![
                json!({
                    "type": "response.output_item.added", "output_index": 0,
                    "item": {"id": "msg_first", "type": "message", "status": "in_progress",
                        "role": "assistant", "content": []}
                }),
                json!({
                    "type": "response.output_item.done", "output_index": 0,
                    "item": {"id": "msg_other", "type": "message", "status": "completed",
                        "role": "assistant", "content": []}
                }),
            ],
        ];

        for events in cases {
            let mut snapshot = ResponsesSnapshotAccumulator::default();
            let mut repair = ResponsesItemIdRepair::new_with_policy(
                &crate::config::ResponseItemIdRepairSettings::default(),
                true,
            ).expect("ID repair");
            for mut event in events {
                snapshot.observe(&event);
                repair.repair_event(&mut event);
            }
            let mut terminal = json!({
                "type": "response.completed",
                "response": {"id": "resp_raw_id", "status": "completed"}
            });
            snapshot.closing_events_before_terminal(&terminal);
            repair.repair_event(&mut terminal);
            snapshot.repair_terminal_event(&mut terminal);
            assert!(terminal["response"].get("output").is_none());
        }
    }

    #[test]
    fn http_snapshot_repair_rejects_delta_only_items_without_added_identity() {
        for event in [
            json!({
                "type": "response.output_text.delta", "output_index": 0,
                "item_id": "msg_unproven", "content_index": 0, "delta": "text"
            }),
            json!({
                "type": "response.reasoning_summary_text.delta", "output_index": 0,
                "item_id": "reasoning_unproven", "summary_index": 0, "delta": "thought"
            }),
            json!({
                "type": "response.function_call_arguments.delta", "output_index": 0,
                "item_id": "function_unproven", "delta": "{}"
            }),
            json!({
                "type": "response.custom_tool_call_input.delta", "output_index": 0,
                "item_id": "custom_unproven", "delta": "pwd"
            }),
        ] {
            let mut snapshot = ResponsesSnapshotAccumulator::default();
            assert!(snapshot.injected_events_before(&event).is_empty());
            snapshot.observe(&event);
            let mut terminal = json!({
                "type": "response.completed",
                "response": {"id": "resp_unproven", "status": "completed"}
            });

            snapshot.closing_events_before_terminal(&terminal);
            snapshot.repair_terminal_event(&mut terminal);

            assert!(snapshot.is_tainted());
            assert!(terminal["response"].get("output").is_none());
        }
    }

    #[test]
    fn proven_output_index_accepts_omitted_item_id_but_rejects_a_foreign_one() {
        let cases = [
            (
                json!({"id": "msg_one", "type": "message", "status": "in_progress",
                    "role": "assistant", "content": []}),
                json!({"type": "response.output_text.delta", "output_index": 0,
                    "content_index": 0, "delta": "hello"}),
                json!({"type": "response.output_text.done", "output_index": 0,
                    "content_index": 0, "text": "hello"}),
            ),
            (
                json!({"id": "rs_one", "type": "reasoning", "status": "in_progress",
                    "summary": []}),
                json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
                    "summary_index": 0, "delta": "checked"}),
                json!({"type": "response.reasoning_summary_text.done", "output_index": 0,
                    "summary_index": 0, "text": "checked"}),
            ),
            (
                json!({"id": "fc_one", "type": "function_call", "status": "in_progress",
                    "call_id": "call_one", "name": "lookup", "arguments": ""}),
                json!({"type": "response.function_call_arguments.delta", "output_index": 0,
                    "delta": "{}"}),
                json!({"type": "response.function_call_arguments.done", "output_index": 0,
                    "arguments": "{}"}),
            ),
            (
                json!({"id": "ct_one", "type": "custom_tool_call", "status": "in_progress",
                    "call_id": "call_one", "name": "exec", "input": ""}),
                json!({"type": "response.custom_tool_call_input.delta", "output_index": 0,
                    "delta": "pwd"}),
                json!({"type": "response.custom_tool_call_input.done", "output_index": 0,
                    "input": "pwd"}),
            ),
        ];

        for (item, event, done) in cases {
            let mut snapshot = ResponsesSnapshotAccumulator::default();
            snapshot.observe(&json!({
                "type": "response.output_item.added", "output_index": 0, "item": item
            }));
            snapshot.observe(&event);
            snapshot.observe(&done);
            assert!(!snapshot.is_tainted());

            let mut foreign = event;
            foreign["item_id"] = Value::String("foreign".into());
            snapshot.observe(&foreign);
            assert!(snapshot.is_tainted());
        }

        let mut content_part = ResponsesSnapshotAccumulator::default();
        content_part.observe(&json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "msg_part", "type": "message", "status": "in_progress"}
        }));
        content_part.observe(&json!({
            "type": "response.content_part.added", "output_index": 0, "content_index": 0,
            "part": {"type": "output_text"}
        }));
        content_part.observe(&json!({
            "type": "response.content_part.done", "output_index": 0, "content_index": 0,
            "part": {"type": "output_text", "text": "", "annotations": []}
        }));
        assert!(!content_part.is_tainted());
    }

    #[test]
    fn completed_event_without_status_closes_items_and_backfills_request_defaults() {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        snapshot.observe(&json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "msg_sparse", "type": "message"}
        }));
        snapshot.observe(&json!({
            "type": "response.output_text.delta", "output_index": 0,
            "content_index": 0, "delta": "hello"
        }));
        let mut terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_sparse"}
        });
        let request = json!({
            "parallel_tool_calls": false,
            "tool_choice": {"type": "none"},
            "tools": [{"type": "function", "name": "lookup"}]
        });

        let closing = snapshot.closing_events_before_terminal(&terminal);
        snapshot.repair_terminal_event_with_request(&mut terminal, &request);

        assert_eq!(
            closing
                .last()
                .and_then(|event| event.get("type"))
                .and_then(Value::as_str),
            Some("response.output_item.done")
        );
        assert_eq!(terminal["response"]["status"], "completed");
        assert_eq!(terminal["response"]["parallel_tool_calls"], false);
        assert_eq!(terminal["response"]["tool_choice"]["type"], "none");
        assert_eq!(terminal["response"]["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(terminal["response"]["output"][0]["role"], "assistant");
        assert_eq!(terminal["response"]["output"][0]["content"][0]["annotations"], json!([]));
    }

    #[test]
    fn non_stream_snapshot_repair_canonicalizes_before_id_repair() {
        let request = json!({"parallel_tool_calls": false, "tools": []});
        let mut missing = json!({"id": "resp_json", "output": {}});
        repair_responses_snapshot_json(&mut missing, &request);
        assert_eq!(missing["status"], "completed");
        assert!(missing["output"].as_array().is_some_and(Vec::is_empty));

        let mut explicit = json!({
            "id": "resp_json_items", "output": [{
                "id": "placeholder", "type": "message",
                "content": [{"type": "output_text"}]
            }]
        });
        repair_responses_snapshot_json(&mut explicit, &request);
        let mut settings = crate::config::ResponseItemIdRepairSettings::default();
        settings.message = vec!["placeholder".into()];
        let mut id_repair = ResponsesItemIdRepair::new(&settings).expect("ID repair");
        id_repair.repair_response(&mut explicit);

        assert_ne!(explicit["output"][0]["id"], "placeholder");
        assert_eq!(explicit["output"][0]["role"], "assistant");
        assert_eq!(explicit["output"][0]["content"][0]["text"], "");
        assert_eq!(explicit["output"][0]["content"][0]["annotations"], json!([]));
    }

    #[test]
    fn reconstructed_terminal_ids_are_repaired_after_raw_lifecycle_collection() {
        let mut settings = crate::config::ResponseItemIdRepairSettings::default();
        settings.message = vec!["placeholder".into()];
        let mut id_repair = ResponsesItemIdRepair::new(&settings).expect("ID repair");
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        let raw_added = json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"id": "placeholder", "type": "message", "status": "in_progress",
                "role": "assistant", "content": []}
        });
        snapshot.observe(&raw_added);
        let mut client_added = raw_added;
        id_repair.repair_event(&mut client_added);
        let repaired_id = client_added["item"]["id"]
            .as_str()
            .expect("repaired item ID")
            .to_string();
        let raw_delta = json!({
            "type": "response.output_text.delta", "output_index": 0,
            "content_index": 0, "delta": "hello"
        });
        snapshot.injected_events_before(&raw_delta);
        snapshot.observe(&raw_delta);
        let mut terminal = json!({
            "type": "response.completed", "response": {"id": "resp_ids"}
        });
        let mut closing = snapshot.closing_events_before_terminal(&terminal);
        for event in &mut closing {
            id_repair.repair_event(event);
        }
        id_repair.repair_event(&mut terminal);
        snapshot.repair_terminal_event(&mut terminal);
        id_repair.repair_event(&mut terminal);

        assert_eq!(terminal["response"]["output"][0]["id"], repaired_id);
        assert_eq!(closing.last().expect("output item done")["item"]["id"], repaired_id);
        assert!(closing.iter().all(|event| {
            event
                .get("item_id")
                .and_then(Value::as_str)
                .is_none_or(|item_id| item_id == repaired_id)
        }));
    }

    #[test]
    fn http_snapshot_repair_rejects_completed_output_index_gaps() {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        observe(&mut snapshot, json!({
            "type": "response.output_item.done", "output_index": 1,
            "item": {"id": "msg_gap", "type": "message", "status": "completed",
                "role": "assistant", "content": []}
        }));
        let mut terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_gap", "status": "completed"}
        });

        snapshot.repair_terminal_event(&mut terminal);

        assert!(snapshot.is_tainted());
        assert!(terminal["response"].get("output").is_none());
    }

    #[test]
    fn oversized_collector_taints_and_fails_closed() {
        let mut snapshot = ResponsesSnapshotAccumulator::default();
        observe(&mut snapshot, json!({
            "type": "response.output_item.done", "output_index": 0,
            "item": {"id": "msg_big", "type": "message", "status": "completed",
                "role": "assistant", "content": [{"type": "output_text",
                    "text": "x".repeat(MAX_SNAPSHOT_REPAIR_BYTES + 1)}]}
        }));
        let mut terminal = json!({
            "type": "response.completed",
            "response": {"id": "resp_big", "status": "completed"}
        });

        snapshot.repair_terminal_event(&mut terminal);

        assert!(terminal["response"].get("output").is_none());
    }

    #[test]
    fn passthrough_preserves_nonterminal_bytes_and_reserializes_repaired_terminal() {
        let nonterminal = b"event: response.output_item.added\r\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_one\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\r\n\r\n";
        let terminal = b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_one\",\"status\":\"completed\"}}\n\n";
        let mut bytes = nonterminal.to_vec();
        bytes.extend_from_slice(terminal);
        let mut pending = Vec::new();
        let mut usage = TokenUsage::default();
        let mut snapshot = ResponsesSnapshotAccumulator::default();

        let frames = drain_snapshot_repair_frames(
            &mut pending,
            &bytes,
            &mut usage,
            &mut snapshot,
            &Value::Null,
        )
        .expect("repair frames");

        assert_eq!(frames.len(), 6);
        assert_eq!(frames[0].bytes.as_ref(), nonterminal);
        assert!(!frames[0].terminal);
        let injected_types = frames[1..5]
            .iter()
            .filter_map(|frame| parse_sse_frame(frame.bytes.as_ref()).ok().flatten())
            .filter_map(|event| event.get("type").and_then(Value::as_str).map(str::to_string))
            .collect::<Vec<_>>();
        assert_eq!(injected_types, vec![
            "response.content_part.added",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
        ]);
        assert!(frames[5].terminal);
        assert_ne!(frames[5].bytes.as_ref(), terminal);
        let repaired = std::str::from_utf8(&frames[5].bytes).expect("UTF-8 terminal");
        assert!(repaired.contains("msg_one"));
    }

    #[test]
    fn passthrough_preserves_an_already_complete_terminal_frame() {
        let terminal = b"event: response.completed\r\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_complete\",\"status\":\"completed\",\"output\":[],\"parallel_tool_calls\":true,\"tool_choice\":\"auto\",\"tools\":[]}}\r\n\r\n";
        let mut pending = Vec::new();
        let mut usage = TokenUsage::default();
        let mut snapshot = ResponsesSnapshotAccumulator::default();

        let frames = drain_snapshot_repair_frames(
            &mut pending,
            terminal,
            &mut usage,
            &mut snapshot,
            &Value::Null,
        )
        .expect("complete terminal frame");

        assert_eq!(frames.len(), 1);
        assert!(frames[0].terminal);
        assert_eq!(frames[0].bytes.as_ref(), terminal);
    }
}
