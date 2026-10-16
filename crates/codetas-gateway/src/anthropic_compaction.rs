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
    let mut seen = false;
    let mut changed = false;
    for original in items {
        if let Some(mut checkpoint) = decode(original)? {
            if seen || !output.is_empty() {
                return Err("native checkpoint must be first and unique".into());
            }
            seen = true;
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
