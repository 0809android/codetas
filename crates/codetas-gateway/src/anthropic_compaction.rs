//! Versioned, bounded transport for Anthropic's signed on-demand checkpoint.
//! This is not encryption. The signature and block are opaque and never logged.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub(crate) const PREFIX: &str = "codetas-anthropic1:";
pub(crate) const BETA: &str = "compact-2026-09-04";
pub(crate) const HISTORY_REWRITTEN: &str = "__codetas_compaction_history_rewritten";
pub(crate) const REQUEST_FLAG: &str = "__codetas_anthropic_compact";
const MAX_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_RETAINED_BYTES: usize = MAX_BYTES - 128 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Checkpoint {
    pub version: u8,
    pub scope: String,
    pub context_hash: String,
    pub block: Value,
    pub retained: Vec<Value>,
}

pub(crate) fn is_item(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("compaction" | "compaction_summary" | "context_compaction")
    ) && item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .is_some_and(|s| s.starts_with(PREFIX))
}
fn validate(checkpoint: &Checkpoint) -> Result<(), String> {
    if checkpoint.version != 1 {
        return Err("unsupported Anthropic checkpoint version".into());
    }
    if checkpoint.scope.len() != 64
        || checkpoint.context_hash.len() != 64
        || !checkpoint
            .scope
            .bytes()
            .chain(checkpoint.context_hash.bytes())
            .all(|b| b.is_ascii_hexdigit())
    {
        return Err("invalid Anthropic checkpoint scope".into());
    }
    validate_block(&checkpoint.block)?;
    if checkpoint.retained.len() > 256 || checkpoint.retained.iter().any(is_item) {
        return Err("invalid nested or oversized Anthropic checkpoint tail".into());
    }
    Ok(())
}
pub(crate) fn validate_block(block: &Value) -> Result<(), String> {
    if block.get("type").and_then(Value::as_str) != Some("compaction")
        || !["content", "signature"].iter().all(|key| {
            block
                .get(*key)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty())
        })
    {
        return Err("native compaction returned no complete signed block".into());
    }
    Ok(())
}
pub(crate) fn decode(item: &Value) -> Result<Option<Checkpoint>, String> {
    if !is_item(item) {
        return Ok(None);
    }
    let encoded = item["encrypted_content"]
        .as_str()
        .unwrap()
        .strip_prefix(PREFIX)
        .unwrap();
    if encoded.len() > MAX_BYTES * 4 / 3 + 4 {
        return Err("Anthropic checkpoint exceeds transport limit".into());
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| "invalid Anthropic checkpoint encoding")?;
    if bytes.len() > MAX_BYTES {
        return Err("Anthropic checkpoint exceeds transport limit".into());
    }
    let checkpoint: Checkpoint =
        serde_json::from_slice(&bytes).map_err(|_| "invalid Anthropic checkpoint payload")?;
    validate(&checkpoint)?;
    Ok(Some(checkpoint))
}
pub(crate) fn encode(checkpoint: &Checkpoint) -> Result<Value, String> {
    validate(checkpoint)?;
    let bytes = serde_json::to_vec(checkpoint).map_err(|_| "cannot encode Anthropic checkpoint")?;
    if bytes.len() > MAX_BYTES {
        return Err("Anthropic checkpoint exceeds transport limit".into());
    }
    Ok(
        json!({"id":format!("cmpctitem_{}",uuid::Uuid::new_v4().simple()),"type":"compaction","encrypted_content":format!("{PREFIX}{}",URL_SAFE_NO_PAD.encode(bytes)),"created_by":"codetas"}),
    )
}
pub(crate) fn summary(checkpoint: &Checkpoint) -> &str {
    checkpoint.block["content"]
        .as_str()
        .expect("validated block")
}
pub(crate) fn digest(value: &Value) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).expect("JSON serialization"))
    )
}
pub(crate) fn context_hash(wire: &Value) -> String {
    digest(&json!({"system":wire.get("system"),"tools":wire.get("tools")}))
}
pub(crate) fn strip_signed_thinking(items: &mut [Value]) {
    for item in items {
        if let Some(metadata) = item
            .get_mut("provider_metadata")
            .and_then(Value::as_object_mut)
        {
            metadata.remove("anthropic");
        }
    }
}
pub(crate) fn fallback(checkpoint: &Checkpoint) -> Vec<Value> {
    let mut items = vec![
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":crate::compaction::format_checkpoint_text(summary(checkpoint))}]}),
    ];
    let mut tail = checkpoint.retained.clone();
    strip_signed_thinking(&mut tail);
    items.extend(tail);
    items
}

// Explicit allow-list from the on-demand compaction beta documentation. Unknown
// models and noncanonical gateways retain the existing local summarizer route.
pub(crate) fn supported_model(model: &str) -> bool {
    const MODELS: &[&str] = &[
        "claude-fable-5-1",
        "claude-mythos-5-1",
        "claude-fable-5",
        "claude-mythos-5",
        "claude-mythos-preview",
        "claude-opus-5-5",
        "claude-opus-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-sonnet-5-5",
        "claude-sonnet-5",
        "claude-sonnet-4-6",
        "claude-haiku-5-5",
    ];
    MODELS.iter().any(|base| {
        model == *base
            || model.strip_prefix(base).is_some_and(|suffix| {
                suffix.len() == 9
                    && suffix.starts_with('-')
                    && suffix[1..].bytes().all(|b| b.is_ascii_digit())
            })
    })
}

/// Expose the retained suffix before tool pairing, tool-map construction, token
/// admission and image normalization. The signed block is not changed. Clearing
/// only our transport's retained field makes repeated preparation idempotent.
pub(crate) fn expand_retained(body: &mut Value) -> Result<bool, String> {
    let Some(items) = body.get("input").and_then(Value::as_array) else {
        return Ok(false);
    };
    if !items.iter().any(is_item) {
        return Ok(false);
    }
    let mut output = Vec::new();
    let mut changed = false;
    for original in items {
        if let Some(mut checkpoint) = decode(original)? {
            // Codex re-injects context items before a checkpoint on mid-turn
            // compaction, and a history can hold several checkpoints after
            // repeated compactions. Anthropic ignores everything before the
            // newest compaction block, so neither case is an error; rejecting
            // them made the thread permanently unusable (400).
            if checkpoint.retained.is_empty() {
                output.push(original.clone());
                continue;
            }
            let mut retained = std::mem::take(&mut checkpoint.retained);
            strip_signed_thinking(&mut retained);
            let encoded = encode(&checkpoint)?;
            let mut item = original.clone();
            item["encrypted_content"] = encoded["encrypted_content"].clone();
            output.push(item);
            output.extend(retained);
            changed = true;
        } else {
            output.push(original.clone());
        }
    }
    if changed {
        body["input"] = Value::Array(output);
    }
    Ok(changed)
}

fn is_task_user(item: &Value) -> bool {
    if item.get("type").is_some() || !item.is_object() {
        return crate::compaction::is_task_user_message(item);
    }
    let mut message = item.clone();
    message["type"] = json!("message");
    crate::compaction::is_task_user_message(&message)
}

/// Instructions and injected context a checkpoint does not replace. Everything else
/// before it is what the checkpoint summarized.
fn outlives_checkpoint(item: &Value) -> bool {
    if !item
        .get("type")
        .and_then(Value::as_str)
        .is_none_or(|kind| kind == "message")
    {
        return false;
    }
    match item.get("role").and_then(Value::as_str) {
        Some("developer" | "system") => true,
        Some("user") => !is_task_user(item),
        _ => false,
    }
}

/// Anthropic drops every message before the newest compaction block, and Codex puts
/// re-injected context in front of a checkpoint, where the model would never see it.
/// On the wire copy only, move that context directly after the block, keeping its
/// order. Older checkpoints, their tails and the turns they summarized would be
/// ignored upstream, so they are not sent. The active request survives even if it
/// sits before the block, since nothing else would carry it.
pub(crate) fn place_context_after_newest_checkpoint(body: &mut Value) {
    let newest = body
        .get("input")
        .and_then(Value::as_array)
        .and_then(|items| items.iter().rposition(is_item));
    let Some(newest) = newest.filter(|newest| *newest > 0) else {
        return;
    };
    // Items about to go may declare tools, and the block's context hash covers them.
    crate::translate::merge_dynamic_tools(body);
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    let mut after = items.split_off(newest);
    let before = std::mem::take(items);
    items.push(after.remove(0));
    let active_request = if after.iter().any(is_task_user) {
        None
    } else {
        before.iter().rposition(is_task_user)
    };
    for (index, item) in before.into_iter().enumerate() {
        let carried = outlives_checkpoint(&item) || active_request == Some(index);
        if carried && !items.contains(&item) && !after.contains(&item) {
            items.push(item);
        }
    }
    items.extend(after);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn checkpoint() -> Checkpoint {
        Checkpoint {
            version: 1,
            scope: "a".repeat(64),
            context_hash: "b".repeat(64),
            block: json!({"type":"compaction","content":" keep\n日本語  \t","signature":"fake-opaque/=+\n","future":{"x":1}}),
            retained: vec![json!({"type":"message","role":"user","content":"latest question"})],
        }
    }
    #[test]
    fn checkpoints_may_follow_other_items_and_repeat() {
        let item = encode(&checkpoint()).unwrap();
        let mut body = json!({"input":[
            {"type":"message","role":"developer","content":"ctx"},
            item.clone(),
            {"type":"message","role":"user","content":"mid"},
            item,
        ]});
        assert!(expand_retained(&mut body).unwrap());
        let items = body["input"].as_array().unwrap();
        assert_eq!(items.len(), 6);
        assert_eq!(items[0]["content"], "ctx");
        assert!(is_item(&items[1]));
        assert_eq!(items[2]["content"], "latest question");
        assert_eq!(items[3]["content"], "mid");
        assert!(is_item(&items[4]));
        assert_eq!(items[5]["content"], "latest question");
        // Retained tails are spent, so a second pass changes nothing.
        let before = body.clone();
        assert!(!expand_retained(&mut body).unwrap());
        assert_eq!(body, before);
    }

    fn message(role: &str, text: &str) -> Value {
        json!({"type":"message","role":role,"content":[{"type":"input_text","text":text}]})
    }

    fn checkpoint_with(signature: &str, retained: Vec<Value>) -> Value {
        let mut source = checkpoint();
        source.block["signature"] = json!(signature);
        source.block["content"] = json!(format!("summary {signature}"));
        source.retained = retained;
        encode(&source).unwrap()
    }

    fn wire_after_placing(mut body: Value) -> Value {
        expand_retained(&mut body).unwrap();
        place_context_after_newest_checkpoint(&mut body);
        crate::anthropic::responses_to_anthropic_with_oauth(&body, "claude-sonnet-5-5", false)
            .unwrap()
    }

    #[test]
    fn context_before_the_checkpoint_goes_directly_after_it_on_the_wire() {
        let wire = wire_after_placing(json!({"input":[
            message("developer", "permissions"),
            message("user", "<environment_context>cwd</environment_context>"),
            checkpoint_with("newest", vec![message("user", "keep working")]),
            message("assistant", "reply after it"),
        ]}));
        let messages = wire["messages"].as_array().unwrap();
        // The block must open the wire, or everything after it would be dropped too.
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["content"][0]["type"], "compaction");
        let text = wire["messages"].to_string();
        let position = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle} is missing from {text}"))
        };
        assert!(position("summary newest") < position("permissions"));
        assert!(position("permissions") < position("<environment_context>"));
        assert!(position("<environment_context>") < position("keep working"));
        assert!(position("keep working") < position("reply after it"));
    }

    #[test]
    fn older_checkpoints_and_what_they_summarized_are_not_sent() {
        let wire = wire_after_placing(json!({"input":[
            checkpoint_with("older", vec![message("user", "older tail")]),
            message("assistant", "older answer"),
            {"type":"function_call","call_id":"old","name":"lookup","arguments":"{}"},
            {"type":"function_call_output","call_id":"old","output":"old output"},
            message("user", "already summarized"),
            message("developer", "permissions"),
            checkpoint_with("newest", vec![message("user", "keep working")]),
        ]}));
        let text = wire["messages"].to_string();
        for gone in [
            "summary older",
            "older tail",
            "older answer",
            "old output",
            "already summarized",
        ] {
            assert!(!text.contains(gone), "{gone} should not be sent: {text}");
        }
        assert!(text.contains("summary newest") && text.contains("permissions"));
        assert!(text.contains("keep working"));
    }

    #[test]
    fn the_active_request_survives_when_only_the_checkpoint_follows_it() {
        let wire = wire_after_placing(json!({"input":[
            message("user", "first request"),
            message("user", "active request"),
            checkpoint_with("newest", vec![]),
        ]}));
        let text = wire["messages"].to_string();
        assert!(text.contains("active request"));
        assert!(!text.contains("first request"));
    }

    #[test]
    fn dynamic_tools_declared_before_the_checkpoint_stay_declared() {
        let tools = json!([{"type":"function","name":"late_tool","parameters":{"type":"object"}}]);
        let wire = wire_after_placing(json!({"input":[
            {"type":"additional_tools","tools":tools},
            checkpoint_with("newest", vec![message("user", "keep working")]),
        ]}));
        assert!(wire["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "late_tool"));
    }

    #[test]
    fn context_already_after_the_checkpoint_is_not_duplicated() {
        let mut body = json!({"input":[
            message("developer", "permissions"),
            checkpoint_with("newest", vec![message("developer", "permissions")]),
        ]});
        expand_retained(&mut body).unwrap();
        place_context_after_newest_checkpoint(&mut body);
        let items = body["input"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert!(is_item(&items[0]));
    }

    #[test]
    fn a_checkpoint_already_first_is_left_alone() {
        let mut body = json!({"input":[
            checkpoint_with("newest", vec![]),
            message("user", "next"),
        ]});
        let before = body.clone();
        place_context_after_newest_checkpoint(&mut body);
        assert_eq!(body, before);
    }
    #[test]
    fn native_anthropic_transport_preserves_whole_block_and_bounds_validation() {
        let source = checkpoint();
        let item = encode(&source).unwrap();
        let decoded = decode(&item).unwrap().unwrap();
        assert_eq!(decoded.block, source.block);
        assert_eq!(decoded.retained, source.retained);
        let fallback = serde_json::to_string(&fallback(&decoded)).unwrap();
        assert!(fallback.contains("latest question"));
        assert!(!fallback.contains("fake-opaque"));
        for encrypted in [
            format!("{PREFIX}!bad"),
            format!("{PREFIX}{}", "a".repeat(MAX_BYTES * 2)),
        ] {
            assert!(decode(&json!({"type":"compaction","encrypted_content":encrypted})).is_err());
        }
        let mut invalid = source.clone();
        invalid.version = 2;
        assert!(encode(&invalid).is_err());
        invalid = source.clone();
        invalid.block["signature"] = Value::Null;
        assert!(encode(&invalid).is_err());
        let mut body = json!({"input":[item]});
        crate::compaction::validate_local_compactions(&body).unwrap();
        crate::compaction::expand_local_compactions(&mut body);
        assert!(body.to_string().contains("latest question"));
        assert!(!body.to_string().contains("fake-opaque"));
    }
}
