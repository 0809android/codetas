use super::*;

pub(super) const FROZEN_CONCLUSION_BANNER: &str = "CHECKPOINT SANITIZED: Some sentences below stated agent beliefs as fact. Treat those as unverified. A later user message outranks them.";

/// Keep a usable handoff, but strip authority from frozen agent conclusions.
pub(crate) fn sanitize_model_summary(summary: &str) -> String {
    let trimmed = summary.trim();
    if !summary_freezes_unverified_conclusion(trimmed) {
        return trimmed.to_string();
    }
    format!("{FROZEN_CONCLUSION_BANNER}\n\n{trimmed}")
}

fn summary_freezes_unverified_conclusion(summary: &str) -> bool {
    let lower = summary.to_ascii_lowercase();
    if lower.contains("already established") {
        return true;
    }
    for needle in ["the reference is", "reference terrain is"] {
        let mut rest = lower.as_str();
        while let Some(index) = rest.find(needle) {
            let after = rest[index + needle.len()..].trim_start();
            if !after.starts_with("not ") && !after.starts_with("n't") {
                return true;
            }
            rest = &rest[index + needle.len()..];
        }
    }
    if summary.contains("すでに確定")
        || summary.contains("既に確定")
        || summary.contains("確定済み")
        || summary.contains("確定事実")
    {
        return true;
    }
    if let Some(index) = summary.find("参考地形は") {
        let after: String = summary[index + "参考地形は".len()..]
            .chars()
            .take(16)
            .collect();
        if !after.contains("ない") && !after.contains('違') {
            return true;
        }
    }
    let mut rest = summary;
    while let Some(index) = rest.find("参考は") {
        let after: String = rest[index + "参考は".len()..].chars().take(24).collect();
        if !after.contains('違') && !after.contains("ではなく") && !after.contains("ではない")
        {
            return true;
        }
        rest = &rest[index + "参考は".len()..];
    }
    false
}

pub(crate) fn encode_summary(summary: &str) -> Result<String, String> {
    if summary.trim().is_empty() {
        return Err("compaction summary is empty".into());
    }
    if summary.len() > MAX_SUMMARY_BYTES {
        return Err("compaction summary exceeds the CODETAS limit".into());
    }
    let payload = serde_json::to_vec(&json!({"version": 1, "summary": summary}))
        .map_err(|_| "compaction summary cannot be encoded".to_string())?;
    if payload.len() > MAX_SUMMARY_BYTES {
        return Err("compaction summary exceeds the CODETAS limit".into());
    }
    Ok(format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(payload)))
}

pub(crate) fn encode_compacted_context(context: &CompactedContext) -> Result<String, String> {
    validate_context_output(context)?;
    let payload = context_envelope_bytes(context)?;
    Ok(format!("{PREFIX_V2}{}", URL_SAFE_NO_PAD.encode(payload)))
}

pub(super) fn context_envelope_bytes(context: &CompactedContext) -> Result<Vec<u8>, String> {
    validate_checkpoint_summary(&context.checkpoint).map_err(|error| error.to_string())?;
    validate_retained_items(&context.retained)?;
    let envelope = EnvelopeV2 {
        version: 2,
        generation: context.generation,
        checkpoint: CheckpointPayload {
            format: CHECKPOINT_FORMAT.to_string(),
            source: "model-generated".into(),
            authority: CHECKPOINT_AUTHORITY.into(),
            text: context.checkpoint.clone(),
        },
        retained: context.retained.clone(),
        selection: context.selection.clone(),
    };
    serde_json::to_vec(&envelope).map_err(|_| "compaction envelope cannot be encoded".to_string())
}

pub(super) fn validate_context_output(context: &CompactedContext) -> Result<(), String> {
    let envelope = context_envelope_bytes(context)?;
    let standalone = serde_json::to_vec(&json!({"output": raw_standalone_output_items(context)}))
        .map_err(|_| "compaction output cannot be encoded".to_string())?;
    if envelope.len().max(standalone.len()) > MAX_SUMMARY_BYTES {
        return Err("compaction summary exceeds the CODETAS limit".into());
    }
    Ok(())
}

#[derive(Debug)]
pub(super) enum LocalEnvelope {
    V1 { summary: String },
    V2 { context: CompactedContext },
    Legacy { summary: String },
}

#[derive(Debug)]
pub(super) enum EnvelopeError {
    Malformed(&'static str),
    UnsupportedVersion,
    NotLocal,
}

impl EnvelopeError {
    pub(super) fn as_validation_message(&self) -> String {
        match self {
            Self::Malformed(message) => (*message).into(),
            Self::UnsupportedVersion => "CODETAS compaction envelope version is unsupported".into(),
            Self::NotLocal => {
                "translated providers can consume only CODETAS compaction envelopes".into()
            }
        }
    }
}

pub(super) fn decode_local_envelope(encrypted: &str) -> Result<LocalEnvelope, EnvelopeError> {
    if let Some(encoded) = encrypted.strip_prefix(PREFIX_V2) {
        return decode_v2_payload(encoded);
    }
    if let Some(encoded) = encrypted.strip_prefix(PREFIX) {
        return decode_v1_payload(encoded);
    }
    if let Some(encoded) = encrypted.strip_prefix(LEGACY_PREFIX) {
        return decode_legacy_payload(encoded);
    }
    Err(EnvelopeError::NotLocal)
}

fn decode_v1_payload(encoded: &str) -> Result<LocalEnvelope, EnvelopeError> {
    let payload = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| EnvelopeError::Malformed("CODETAS compaction envelope is not valid base64"))?;
    if payload.len() > MAX_SUMMARY_BYTES {
        return Err(EnvelopeError::Malformed(
            "CODETAS compaction envelope exceeds the size limit",
        ));
    }
    let value: Value = serde_json::from_slice(&payload)
        .map_err(|_| EnvelopeError::Malformed("CODETAS compaction envelope is invalid"))?;
    match value.get("version").and_then(Value::as_u64) {
        Some(1) => {}
        Some(_) => return Err(EnvelopeError::UnsupportedVersion),
        None => {
            return Err(EnvelopeError::Malformed(
                "CODETAS compaction envelope is missing version",
            ))
        }
    }
    let summary = value
        .get("summary")
        .and_then(Value::as_str)
        .filter(|summary| !summary.trim().is_empty())
        .ok_or(EnvelopeError::Malformed(
            "CODETAS compaction envelope has no summary",
        ))?;
    Ok(LocalEnvelope::V1 {
        summary: summary.to_string(),
    })
}

fn decode_legacy_payload(encoded: &str) -> Result<LocalEnvelope, EnvelopeError> {
    let payload = STANDARD
        .decode(encoded)
        .or_else(|_| URL_SAFE_NO_PAD.decode(encoded))
        .map_err(|_| EnvelopeError::Malformed("Compaction envelope is not valid base64"))?;
    if payload.len() > MAX_SUMMARY_BYTES {
        return Err(EnvelopeError::Malformed(
            "Compaction envelope exceeds the size limit",
        ));
    }
    let summary = String::from_utf8(payload)
        .map_err(|_| EnvelopeError::Malformed("Compaction envelope is not valid UTF-8"))?;
    if summary.trim().is_empty() {
        return Err(EnvelopeError::Malformed(
            "Compaction envelope has no summary",
        ));
    }
    Ok(LocalEnvelope::Legacy { summary })
}

fn decode_v2_payload(encoded: &str) -> Result<LocalEnvelope, EnvelopeError> {
    let payload = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| EnvelopeError::Malformed("CODETAS compaction envelope is not valid base64"))?;
    if payload.len() > MAX_SUMMARY_BYTES {
        return Err(EnvelopeError::Malformed(
            "CODETAS compaction envelope exceeds the size limit",
        ));
    }
    let value: Value = serde_json::from_slice(&payload)
        .map_err(|_| EnvelopeError::Malformed("CODETAS compaction envelope is invalid"))?;
    match value.get("version").and_then(Value::as_u64) {
        Some(2) => {}
        Some(_) => return Err(EnvelopeError::UnsupportedVersion),
        None => {
            return Err(EnvelopeError::Malformed(
                "CODETAS compaction envelope is missing version",
            ))
        }
    }
    if let Some(object) = value.as_object() {
        const KNOWN: [&str; 5] = [
            "version",
            "generation",
            "checkpoint",
            "retained",
            "selection",
        ];
        if object.keys().any(|key| !KNOWN.contains(&key.as_str())) {
            return Err(EnvelopeError::Malformed(
                "CODETAS compaction envelope has unknown fields",
            ));
        }
    }
    let envelope: EnvelopeV2 = serde_json::from_value(value).map_err(|_| {
        EnvelopeError::Malformed("CODETAS compaction envelope is missing a required field")
    })?;
    if envelope.checkpoint.authority != CHECKPOINT_AUTHORITY {
        return Err(EnvelopeError::Malformed(
            "CODETAS compaction envelope checkpoint authority is invalid",
        ));
    }
    if envelope.checkpoint.format != CHECKPOINT_FORMAT {
        return Err(EnvelopeError::Malformed(
            "CODETAS compaction envelope checkpoint format is invalid",
        ));
    }
    if envelope.checkpoint.text.trim().is_empty() {
        return Err(EnvelopeError::Malformed(
            "CODETAS compaction envelope has no checkpoint",
        ));
    }
    if envelope.retained.len() > MAX_RETAINED_ITEMS {
        return Err(EnvelopeError::Malformed(
            "CODETAS compaction envelope retained item count exceeds the limit",
        ));
    }
    validate_retained_items(&envelope.retained).map_err(|_| {
        EnvelopeError::Malformed("CODETAS compaction envelope retained items are invalid")
    })?;
    Ok(LocalEnvelope::V2 {
        context: CompactedContext {
            checkpoint: envelope.checkpoint.text,
            retained: envelope.retained,
            generation: envelope.generation,
            selection: envelope.selection,
        },
    })
}

pub(crate) fn decode_summary(item: &Value) -> Result<Option<String>, String> {
    match item.get("type").and_then(Value::as_str) {
        Some("compaction" | "compaction_summary" | "context_compaction") => {}
        _ => return Ok(None),
    }
    let encrypted = item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .ok_or("compaction item requires encrypted_content")?;
    if let Some(checkpoint) = crate::anthropic_compaction::decode(item)? {
        return Ok(Some(format_checkpoint_text(
            crate::anthropic_compaction::summary(&checkpoint),
        )));
    }
    match decode_local_envelope(encrypted) {
        Ok(LocalEnvelope::V1 { summary } | LocalEnvelope::Legacy { summary }) => {
            Ok(Some(format_checkpoint_text(&summary)))
        }
        Ok(LocalEnvelope::V2 { context }) => Ok(Some(format_checkpoint_text(&context.checkpoint))),
        Err(error) => Err(error.as_validation_message()),
    }
}

pub(crate) fn format_checkpoint_text(summary: &str) -> String {
    format!(
        "{SUMMARY_PREFIX}\n\n<codetas_compaction_summary>\n{summary}\n</codetas_compaction_summary>"
    )
}

pub(super) fn checkpoint_handoff_message(summary: &str) -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": format_checkpoint_text(summary)}]
    })
}

pub(super) fn invalid_envelope_marker() -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": "[invalid local compaction summary omitted]"
        }]
    })
}

pub(super) fn opaque_compaction_marker() -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": OPAQUE_COMPACTION_NOTE}]
    })
}
