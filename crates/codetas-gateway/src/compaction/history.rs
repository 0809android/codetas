use super::*;

pub(crate) fn estimate_input_items_tokens(items: &[Value]) -> u64 {
    crate::server::estimate_input_items_tokens(items)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NormalizedHistory {
    pub(crate) previous_checkpoint: Option<String>,
    pub(crate) previous_generation: u64,
    pub(crate) items: Vec<Value>,
}

pub(crate) fn normalize_compaction_history(items: &[Value]) -> Result<NormalizedHistory, String> {
    let mut previous_checkpoint = None;
    let mut previous_generation = 0;
    let mut normalized = Vec::new();
    for item in items {
        if let Some(checkpoint) = crate::anthropic_compaction::decode(item)? {
            previous_checkpoint =
                Some(crate::anthropic_compaction::summary(&checkpoint).to_string());
            previous_generation = previous_generation.max(1);
            let mut tail = checkpoint.retained;
            crate::anthropic_compaction::strip_signed_thinking(&mut tail);
            normalized.extend(tail);
            continue;
        }
        // Standalone returns a framed assistant handoff instead of an envelope.
        // Recover that exact writer form so its checkpoint (and archive links)
        // does not get reduced to a clipped assistant conclusion on the next run.
        if is_assistant_message(item) {
            if let Some(text) = message_text(item) {
                if let Some(summary) = text
                    .strip_prefix(&format!(
                        "{SUMMARY_PREFIX}\n\n<codetas_compaction_summary>\n"
                    ))
                    .and_then(|text| text.strip_suffix("\n</codetas_compaction_summary>"))
                    .filter(|summary| validate_checkpoint_summary(summary).is_ok())
                {
                    previous_checkpoint = Some(summary.to_string());
                    previous_generation = previous_generation.max(1);
                    continue;
                }
            }
        }
        let kind = item.get("type").and_then(Value::as_str);
        if matches!(
            kind,
            Some("compaction" | "compaction_summary" | "context_compaction")
        ) {
            let encrypted = item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .ok_or_else(|| "compaction item requires encrypted_content".to_string())?;
            if !(encrypted.starts_with(PREFIX)
                || encrypted.starts_with(PREFIX_V2)
                || encrypted.starts_with(LEGACY_PREFIX))
            {
                // Opaque provider blobs stay out of the retained tail and are
                // summarized from the unread note only.
                continue;
            }
            match decode_local_envelope(encrypted) {
                Ok(LocalEnvelope::V1 { summary } | LocalEnvelope::Legacy { summary }) => {
                    previous_checkpoint = Some(summary);
                    previous_generation = previous_generation.max(1);
                }
                Ok(LocalEnvelope::V2 { context }) => {
                    previous_checkpoint = Some(context.checkpoint);
                    previous_generation = previous_generation.max(context.generation);
                    normalized.extend(context.retained);
                }
                Err(error) => return Err(error.as_validation_message()),
            }
            continue;
        }
        if matches!(
            kind,
            Some("compaction_trigger" | "additional_tools" | "reasoning")
        ) {
            continue;
        }
        if let Some(sanitized) = sanitize_history_item(item)? {
            normalized.push(sanitized);
        }
    }
    normalize_completed_empty_function_arguments(&mut normalized);
    Ok(NormalizedHistory {
        previous_checkpoint,
        previous_generation,
        items: normalized,
    })
}

/// Some historical connector calls were recorded with an empty argument string
/// and an actual tool result (including schema-validation failures). An empty
/// string describes no supplied parameters, but is not JSON. Canonicalize only
/// answered occurrences to the equivalent empty object before selecting a tail.
/// Never invent parameters, repair nonempty malformed JSON, or rewrite a live
/// call. Keep the result verbatim so a failed invocation remains a failure.
pub(crate) fn normalize_completed_empty_function_arguments(items: &mut [Value]) {
    let mut open = std::collections::HashMap::<String, Option<usize>>::new();
    let mut repaired = 0;
    for index in 0..items.len() {
        match items[index].get("type").and_then(Value::as_str) {
            Some("function_call") => {
                if let Some(id) = items[index]
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                {
                    // Duplicate open IDs are ambiguous and must still fail
                    // validation; do not pick one occurrence to rewrite.
                    open.entry(id.to_string())
                        .and_modify(|call| *call = None)
                        .or_insert(Some(index));
                }
            }
            Some("function_call_output") => {
                let call = items[index]
                    .get("call_id")
                    .and_then(Value::as_str)
                    .and_then(|id| open.remove(id))
                    .flatten();
                if let Some(call) = call {
                    if items[call]
                        .get("arguments")
                        .and_then(Value::as_str)
                        .is_some_and(|arguments| arguments.trim().is_empty())
                    {
                        items[call]["arguments"] = json!("{}");
                        repaired += 1;
                    }
                }
            }
            _ => {}
        }
    }
    if repaired > 0 {
        crate::debug::log(&format!(
            "compaction normalized completed empty function arguments count={repaired}"
        ));
    }
}

fn sanitize_history_item(item: &Value) -> Result<Option<Value>, String> {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => match item.get("role").and_then(Value::as_str) {
            Some("user") if is_injected_control_user_message(item) => Ok(None),
            Some("user" | "assistant") => Ok(Some(replace_inline_images(item))),
            Some("system" | "developer") => Ok(None),
            _ => Ok(None),
        },
        Some(
            "function_call"
            | "function_call_output"
            | "custom_tool_call"
            | "custom_tool_call_output"
            | "local_shell_call"
            | "local_shell_call_output"
            | "tool_search_call"
            | "tool_search_output",
        ) => Ok(Some(replace_inline_images(item))),
        Some("compaction" | "compaction_trigger" | "additional_tools" | "reasoning") => Ok(None),
        Some(_) => Ok(None),
        None => Ok(None),
    }
}

fn replace_inline_images(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(replace_inline_images).collect()),
        Value::Object(object) => {
            if matches!(
                object.get("type").and_then(Value::as_str),
                Some("input_image" | "image_url")
            ) {
                return json!({
                    "type": "input_text",
                    "text": IMAGE_MARKER
                });
            }
            let mut next = Map::new();
            for (key, child) in object {
                next.insert(key.clone(), replace_inline_images(child));
            }
            Value::Object(next)
        }
        other => other.clone(),
    }
}

pub(super) fn validate_retained_items(items: &[Value]) -> Result<(), String> {
    if items.len() > MAX_RETAINED_ITEMS {
        return Err("compaction retained item count exceeds the limit".into());
    }
    let mut open_calls = std::collections::HashSet::new();
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => match item.get("role").and_then(Value::as_str) {
                Some("user" | "assistant") => {}
                Some("system" | "developer") => {
                    return Err(
                        "compaction retained items cannot include system or developer messages"
                            .into(),
                    );
                }
                _ => return Err("compaction retained message role is not allowed".into()),
            },
            Some(
                "function_call" | "custom_tool_call" | "local_shell_call" | "tool_search_call",
            ) => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "compaction retained tool calls require call_id".to_string())?;
                if !open_calls.insert(call_id.to_string()) {
                    return Err(
                        "compaction retained items contain a duplicate open tool call".into(),
                    );
                }
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    validate_function_call_arguments(item.get("arguments"))?;
                }
            }
            Some(
                "function_call_output"
                | "custom_tool_call_output"
                | "local_shell_call_output"
                | "tool_search_output",
            ) => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        "compaction retained tool outputs require call_id".to_string()
                    })?;
                if !open_calls.remove(call_id) {
                    return Err("compaction retained items contain an orphan tool output".into());
                }
            }
            Some("compaction" | "compaction_trigger" | "additional_tools" | "reasoning") => {
                return Err("compaction retained items contain a forbidden item type".into());
            }
            Some(_) => return Err("compaction retained items contain an unknown item type".into()),
            None => return Err("compaction retained items require a type".into()),
        }
        if item_contains_inline_image(item) {
            return Err("compaction retained items cannot include inline images".into());
        }
    }
    // In-flight tool calls may remain open at the end of a live retained/tail
    // window (compact during a long tool turn). Orphan outputs and duplicate
    // opens are still rejected above; recovery helpers must not leave unpaired
    // calls in offline tails.
    let _ = open_calls;
    Ok(())
}

fn validate_function_call_arguments(arguments: Option<&Value>) -> Result<(), String> {
    match arguments {
        Some(Value::String(raw)) => {
            serde_json::from_str::<Value>(raw).map_err(|_| {
                "compaction retained function_call arguments must contain valid JSON".to_string()
            })?;
            Ok(())
        }
        Some(Value::Object(_)) | Some(Value::Array(_)) => Ok(()),
        Some(Value::Null) | None => Err(
            "compaction retained function_call arguments must be a JSON string or object".into(),
        ),
        Some(_) => Err(
            "compaction retained function_call arguments must be a JSON string or object".into(),
        ),
    }
}

fn item_contains_inline_image(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(item_contains_inline_image),
        Value::Object(object) => {
            matches!(
                object.get("type").and_then(Value::as_str),
                Some("input_image" | "image_url")
            ) || object.values().any(item_contains_inline_image)
        }
        _ => false,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InteractionGroup {
    pub(crate) items: Vec<Value>,
}

impl InteractionGroup {
    pub(super) fn starts_with_user(&self) -> bool {
        self.items.first().is_some_and(is_user_message)
    }
}

pub(crate) fn is_user_message(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("message")
        && item.get("role").and_then(Value::as_str) == Some("user")
}

pub(crate) fn is_assistant_message(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("message")
        && item.get("role").and_then(Value::as_str) == Some("assistant")
}

pub(crate) fn item_is_local_compaction(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("compaction" | "compaction_summary" | "context_compaction")
    ) && item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .is_some_and(|value| {
            value.starts_with(PREFIX)
                || value.starts_with(PREFIX_V2)
                || value.starts_with(LEGACY_PREFIX)
        })
}

pub(super) fn is_tool_call(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("function_call" | "custom_tool_call" | "local_shell_call" | "tool_search_call")
    )
}

pub(super) fn is_tool_result(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some(
            "function_call_output"
                | "custom_tool_call_output"
                | "local_shell_call_output"
                | "tool_search_output"
        )
    )
}

pub(super) fn call_id(item: &Value) -> Option<&str> {
    item.get("call_id").and_then(Value::as_str)
}

pub(crate) fn split_interaction_groups(items: &[Value]) -> Vec<InteractionGroup> {
    let mut groups = Vec::new();
    let mut current = Vec::new();
    let mut open_calls = std::collections::HashSet::new();
    for item in items {
        let starts_new_user_turn =
            is_user_message(item) && !current.is_empty() && open_calls.is_empty();
        if starts_new_user_turn {
            groups.push(InteractionGroup {
                items: std::mem::take(&mut current),
            });
        }
        if is_tool_call(item) {
            if let Some(id) = call_id(item) {
                open_calls.insert(id.to_string());
            }
        } else if is_tool_result(item) {
            if let Some(id) = call_id(item) {
                open_calls.remove(id);
            }
        }
        current.push(item.clone());
    }
    if !current.is_empty() {
        groups.push(InteractionGroup { items: current });
    }
    groups
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HistorySplit {
    pub(crate) prefix: Vec<Value>,
    pub(crate) tail: Vec<Value>,
    pub(crate) selection: CompactionSelection,
    pub(crate) retained_turns: usize,
}

pub(crate) fn split_prefix_and_tail(
    items: &[Value],
    tail_token_limit: u64,
) -> Result<HistorySplit, String> {
    select_retained_history(items, tail_token_limit)
}

pub(super) fn recover_split_for_offline(
    items: &[Value],
    tail_token_limit: u64,
) -> Result<HistorySplit, String> {
    select_retained_history(items, tail_token_limit)
}

/// Index the original occurrences before selecting anything. A completed call
/// and its result are one selection unit, even with parallel calls or reused
/// IDs. Mandatory occurrences are reserved up front; the reverse selection
/// never evicts, rewrites, reorders, or repairs a previously selected item.
fn select_retained_history(items: &[Value], tail_token_limit: u64) -> Result<HistorySplit, String> {
    let mut partner = vec![None; items.len()];
    let mut open = std::collections::HashMap::new();
    let mut last_user = None;
    let mut last_assistant = None;
    let mut last_substantive = None;
    for (index, item) in items.iter().enumerate() {
        if is_tool_call(item) {
            let id = call_id(item)
                .filter(|id| !id.is_empty())
                .ok_or("compaction tool call requires call_id")?;
            // Validate before selecting: eviction into prefix must not turn an
            // invalid call into an accepted compacted history.
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                validate_function_call_arguments(item.get("arguments"))?;
            }
            if open.insert(id, index).is_some() {
                return Err("compaction history contains a duplicate open tool call".into());
            }
        } else if is_tool_result(item) {
            let call = call_id(item)
                .and_then(|id| open.remove(id))
                .ok_or("compaction history contains an orphan tool output")?;
            partner[call] = Some(index);
            partner[index] = Some(call);
        } else if is_task_user_message(item) {
            last_user = Some(index);
        } else if is_assistant_message(item) && !is_synthetic_tool_observation_item(item) {
            last_assistant = Some(index);
            if !is_placeholder_assistant_item(item) {
                last_substantive = Some(index);
            }
        }
    }

    let mut selected = vec![false; items.len()];
    for index in last_user
        .into_iter()
        .chain(last_substantive.or(last_assistant))
        .chain(open.values().copied())
    {
        selected[index] = true;
    }
    let mut count = selected.iter().filter(|keep| **keep).count();
    if count > MAX_RETAINED_ITEMS {
        // No valid envelope can satisfy both requirements. Fail instead of
        // silently losing a task or a call whose result has not arrived yet.
        return Err("mandatory compaction retained item count exceeds the limit".into());
    }
    let costs: Vec<u64> = items
        .iter()
        .map(|item| estimate_input_items_tokens(std::slice::from_ref(item)))
        .collect();
    let mut tokens: u64 = costs
        .iter()
        .zip(&selected)
        .filter_map(|(cost, keep)| keep.then_some(*cost))
        .sum();
    let mut retained_ids: std::collections::HashSet<&str> = open.keys().copied().collect();

    for index in (0..items.len()).rev() {
        if selected[index] || is_tool_call(&items[index]) {
            continue;
        }
        let mate = partner[index];
        if let Some(call) = mate {
            // A live occurrence takes precedence over an older answered copy.
            // Never keep that older result as the answer to the live call.
            if retained_ids.contains(call_id(&items[call]).expect("indexed call")) {
                continue;
            }
        }
        let extra_count = 1 + usize::from(mate.is_some());
        let extra_tokens = costs[index].saturating_add(mate.map_or(0, |call| costs[call]));
        if count + extra_count > MAX_RETAINED_ITEMS
            || tokens.saturating_add(extra_tokens) > tail_token_limit
        {
            break;
        }
        selected[index] = true;
        if let Some(call) = mate {
            selected[call] = true;
            retained_ids.insert(call_id(&items[call]).expect("indexed call"));
        }
        count += extra_count;
        tokens = tokens.saturating_add(extra_tokens);
    }

    let mut prefix = Vec::new();
    let mut tail = Vec::with_capacity(count);
    for (item, keep) in items.iter().zip(selected) {
        if keep {
            tail.push(item.clone());
        } else {
            prefix.push(item.clone());
        }
    }
    validate_retained_items(&tail)?;
    let retained_turns = split_interaction_groups(&tail)
        .iter()
        .filter(|group| group.starts_with_user())
        .count()
        .max(usize::from(!tail.is_empty()));
    Ok(HistorySplit {
        selection: CompactionSelection {
            target_tokens: tail_token_limit,
            estimated_tokens: estimate_input_items_tokens(&tail),
            truncated: !prefix.is_empty(),
        },
        prefix,
        tail,
        retained_turns,
    })
}

pub(super) fn is_placeholder_assistant_item(item: &Value) -> bool {
    message_text(item).is_some_and(|text| is_placeholder_assistant_text(&text))
}

pub(super) fn is_synthetic_tool_observation_item(item: &Value) -> bool {
    message_text(item).is_some_and(|text| is_synthetic_tool_observation_text(&text))
}

#[cfg(test)]
pub(super) fn tool_observation_message(paths: &[String]) -> Value {
    json!({"type": "message", "role": "assistant", "content": [{
        "type": "output_text", "text": format!("[compacted tool files]\n{}", paths.join("\n"))
    }]})
}

pub(crate) fn build_summarizer_input(
    previous_checkpoint: Option<&str>,
    prefix: &[Value],
) -> Vec<Value> {
    let mut input = Vec::new();
    if let Some(checkpoint) = previous_checkpoint {
        input.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": format!(
                    "Previous checkpoint to update. Replace corrected claims instead of repeating them.\n\n{checkpoint}"
                )
            }]
        }));
    }
    input.extend(prefix.iter().cloned());
    input
}

/// Items a summarizer should read.
///
/// `split_prefix_and_tail` keeps recent turns in the tail so they survive
/// compaction verbatim. When the whole conversation fits in the tail — which is
/// what happens once the injected control blocks are removed and only a short
/// real task is left — the prefix is empty and the summarizer would otherwise be
/// asked to summarize nothing. Fall back to the tail so the model still sees the
/// conversation it is summarizing.
pub(crate) fn summarizer_source_items(split: &HistorySplit) -> &[Value] {
    if split.prefix.is_empty() {
        &split.tail
    } else {
        &split.prefix
    }
}
