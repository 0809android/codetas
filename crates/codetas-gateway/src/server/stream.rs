use super::*;

const MAX_SNAPSHOT_REPAIR_BYTES: usize = 8 * 1024 * 1024;
const MAX_SNAPSHOT_REPAIR_INDEX: usize = 4_096;

#[derive(Default)]
pub(crate) struct ResponsesSnapshotAccumulator {
    items: std::collections::BTreeMap<usize, Value>,
    done_items: std::collections::BTreeMap<usize, Value>,
    item_indexes: HashMap<String, usize>,
    open_indexes: HashSet<usize>,
    closed_indexes: HashSet<usize>,
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
            if index.is_some_and(|index| self.closed_indexes.contains(&index)) {
                self.taint();
                return;
            }
        }
        match kind {
            "response.output_item.added" | "response.output_item.done" => {
                let index = self.event_index(event);
                let (Some(index), Some(item)) = (index, event.get("item").cloned()) else {
                    return;
                };
                if serde_json::to_vec(&item)
                    .is_ok_and(|encoded| encoded.len() > MAX_SNAPSHOT_REPAIR_BYTES)
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
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    if self
                        .item_indexes
                        .get(id)
                        .is_some_and(|existing| *existing != index)
                    {
                        self.taint();
                        return;
                    }
                    if self.items.get(&index).and_then(|existing| existing.get("id"))
                        .and_then(Value::as_str)
                        .is_some_and(|existing| existing != id)
                    {
                        self.taint();
                        return;
                    }
                    self.item_indexes.insert(id.to_string(), index);
                }
                let item = if let Some(existing) = self.items.remove(&index) {
                    merge_snapshot_item(existing, &item)
                } else {
                    item
                };
                self.items.insert(index, item.clone());
                if kind.ends_with(".done") {
                    self.done_items.insert(index, item);
                }
            }
            "response.output_text.delta" | "response.output_text.done" => {
                let index = self.event_index(event);
                let Some(index) = index else { return };
                let Some(content_index) = self.subindex(event, "content_index") else {
                    return;
                };
                let item = self.ensure_item(index, event, "message");
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
            }
            "response.content_part.added" | "response.content_part.done" => {
                let index = self.event_index(event);
                let Some(index) = index else { return };
                let Some(content_index) = self.subindex(event, "content_index") else {
                    return;
                };
                if let Some(part) = event.get("part").cloned() {
                    let item = self.ensure_item(index, event, "message");
                    ensure_array_slot(item, "content", content_index, || Value::Null);
                    let current = std::mem::take(&mut item["content"][content_index]);
                    item["content"][content_index] = merge_snapshot_item(current, &part);
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_summary_text.done" => {
                let index = self.event_index(event);
                let Some(index) = index else { return };
                let Some(summary_index) = self.subindex(event, "summary_index") else {
                    return;
                };
                let item = self.ensure_item(index, event, "reasoning");
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
                let item = self.ensure_item(index, event, "reasoning");
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
                let item = self.ensure_item(index, event, "reasoning");
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    append_string(&mut item["encrypted_content"], delta);
                }
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

    pub(crate) fn repair_terminal_event(&self, event: &mut Value) -> Option<Value> {
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
        if terminal_type != "response.completed" {
            return Some(Value::Object(response.clone()));
        }
        if response
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status != "completed")
        {
            return Some(Value::Object(response.clone()));
        }
        if response.get("output").is_some_and(Value::is_array) || self.tainted {
            return Some(Value::Object(response.clone()));
        }
        response.insert("status".into(), Value::String("completed".into()));
        response.insert("output".into(), Value::Array(self.reconstructed_output(Vec::new())));
        Some(Value::Object(response.clone()))
    }

    pub(crate) fn backfill_generic_terminal_event(&self, event: &mut Value) -> Option<Value> {
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
        let output_needs_backfill = response
            .get("output")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty);
        if output_needs_backfill && !self.done_items.is_empty() {
            let contiguous = self
                .done_items
                .keys()
                .copied()
                .eq(0..self.done_items.len());
            if contiguous {
                response.insert(
                    "output".into(),
                    Value::Array(self.done_items.values().cloned().collect()),
                );
            }
        }
        Some(Value::Object(response.clone()))
    }

    pub(crate) fn repair_compaction_terminal_event(&self, event: &mut Value) -> Option<Value> {
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
        response.insert("output".into(), Value::Array(self.reconstructed_output(terminal)));
        Some(Value::Object(response.clone()))
    }

    fn reconstructed_output(&self, terminal: Vec<Value>) -> Vec<Value> {
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
        let mut output = merged.into_values().collect::<Vec<_>>();
        remove_orphan_tool_results(&mut output);
        for item in &mut output {
            if item.is_object() && item.get("status").is_some() {
                item["status"] = Value::String("completed".into());
            }
        }
        output
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
        self.done_items.clear();
        self.item_indexes.clear();
        self.open_indexes.clear();
        self.closed_indexes.clear();
    }

    fn ensure_item<'a>(
        &'a mut self,
        index: usize,
        event: &Value,
        item_type: &str,
    ) -> &'a mut Value {
        let item_id = event
            .get("item_id")
            .and_then(Value::as_str)
            .unwrap_or("item_reconstructed");
        self.item_indexes.insert(item_id.to_string(), index);
        self.items.entry(index).or_insert_with(|| match item_type {
            "message" => json!({
                "id": item_id, "type": "message", "status": "completed",
                "role": "assistant", "content": []
            }),
            "reasoning" => json!({
                "id": item_id, "type": "reasoning", "status": "completed", "summary": []
            }),
            "custom_tool_call" => json!({
                "id": item_id, "type": "custom_tool_call", "status": "completed",
                "call_id": event.get("call_id").cloned().unwrap_or(Value::Null),
                "name": event.get("name").cloned().unwrap_or(Value::Null), "input": ""
            }),
            _ => json!({
                "id": item_id, "type": "function_call", "status": "completed",
                "call_id": event.get("call_id").cloned().unwrap_or(Value::Null),
                "name": event.get("name").cloned().unwrap_or(Value::Null), "arguments": ""
            }),
        })
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
        let item = self.ensure_item(index, event, item_type);
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
                        repair.repair_event(&mut value);
                        snapshot.observe(&value);
                        let repaired_response = if snapshot_repair {
                            snapshot.repair_terminal_event(&mut value)
                        } else {
                            snapshot.backfill_generic_terminal_event(&mut value)
                        };
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
                                .backfill_generic_terminal_event(&mut value)
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
        snapshot.observe(&value);
        let original = value.clone();
        let response = snapshot.repair_terminal_event(&mut value);
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
    fn reconstructs_tool_arguments_without_output_item_done() {
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

        snapshot.repair_terminal_event(&mut terminal);

        assert_eq!(
            terminal["response"]["output"][0]["arguments"],
            "{\"q\":\"x\"}"
        );
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
    fn removes_orphan_tool_results_from_repaired_snapshot() {
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

        snapshot.repair_terminal_event(&mut terminal);

        assert!(terminal["response"]["output"]
            .as_array()
            .is_some_and(Vec::is_empty));
    }

    #[test]
    fn keeps_paired_tool_results_and_reconstructs_custom_input() {
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

        snapshot.repair_terminal_event(&mut terminal);

        assert_eq!(terminal["response"]["output"].as_array().map(Vec::len), Some(2));
        assert_eq!(terminal["response"]["output"][0]["input"], "pwd");
        assert_eq!(terminal["response"]["output"][1]["call_id"], "call_one");
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

        snapshot.repair_terminal_event(&mut terminal);

        assert!(terminal["response"]["output"]
            .as_array()
            .is_some_and(Vec::is_empty));
    }

    #[test]
    fn generic_http_continuation_backfills_empty_terminal_from_done_items() {
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
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["id"], "rs_one");
        assert_eq!(output[1]["id"], "msg_one");
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
            snapshot.backfill_generic_terminal_event(&mut terminal);
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
            snapshot.backfill_generic_terminal_event(&mut generic_terminal);
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
        )
        .expect("repair frames");

        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].bytes.as_ref(), nonterminal);
        assert!(!frames[0].terminal);
        assert!(frames[1].terminal);
        assert_ne!(frames[1].bytes.as_ref(), terminal);
        let repaired = std::str::from_utf8(&frames[1].bytes).expect("UTF-8 terminal");
        assert!(repaired.contains("msg_one"));
    }

    #[test]
    fn passthrough_preserves_an_already_complete_terminal_frame() {
        let terminal = b"event: response.completed\r\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_complete\",\"status\":\"completed\",\"output\":[]}}\r\n\r\n";
        let mut pending = Vec::new();
        let mut usage = TokenUsage::default();
        let mut snapshot = ResponsesSnapshotAccumulator::default();

        let frames = drain_snapshot_repair_frames(
            &mut pending,
            terminal,
            &mut usage,
            &mut snapshot,
        )
        .expect("complete terminal frame");

        assert_eq!(frames.len(), 1);
        assert!(frames[0].terminal);
        assert_eq!(frames[0].bytes.as_ref(), terminal);
    }
}
