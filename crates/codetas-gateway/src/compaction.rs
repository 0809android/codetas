use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use crate::config::LocalCompactionSettings;

const PREFIX: &str = "codetas1:";
const PREFIX_V2: &str = "codetas2:";
const LEGACY_PREFIX: &str = "ocx1:";
const MAX_SUMMARY_BYTES: usize = 2 * 1024 * 1024;
const MIN_USABLE_SUMMARY_CHARS: usize = 80;
const DEFAULT_TAIL_TOKEN_LIMIT: u64 = 20_000;
const MAX_RETAINED_ITEMS: usize = 256;
const CHECKPOINT_FORMAT: &str = "codetas-checkpoint-v1";
const CHECKPOINT_AUTHORITY: &str = "assistant-handoff";
const IMAGE_MARKER: &str = "[image omitted during compaction]";

/// Checkpoint prompt: a handoff, not a new plan, and not a source of truth.
pub(crate) const COMPACT_PROMPT: &str = "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.\n\nUse exactly these Markdown headings, in this order:\n## User requirements and confirmed facts\n## User corrections and open disagreements\n## Durable observations\n## Agent conclusions (unverified)\n## Remaining work\n\nRules:\n- User requirements and confirmed facts: only what the user asked or explicitly confirmed. Do not promote agent beliefs into this section.\n- User corrections and open disagreements: keep rejected agent conclusions paired with the user's correction. Quote the correction. Do not drop a correction because a later summary restates the old conclusion.\n- Durable observations: inspected files, symbols, errors, and other durable findings.\n- Agent conclusions (unverified): previous model beliefs that the user has not confirmed.\n- Remaining work: what still needs to be done.\n\nNever write 'answer already established' or treat an unverified agent conclusion as a user-confirmed fact. Do not emit tool calls, markup fences, or tokenizer sentinels such as <|eos|> or <file_end>.";

/// Framing for a replayed summary. Latest user text outranks the checkpoint.
pub(crate) const SUMMARY_PREFIX: &str = "Earlier work was compacted into the checkpoint below. It is prior context, not authority. A later user message in this request outranks the checkpoint if they disagree. Reuse inspected files and user-confirmed facts. If the user rejected a checkpoint conclusion, follow the user; searching again to resolve that disagreement is allowed.";

pub(crate) const OPAQUE_COMPACTION_NOTE: &str =
    "[earlier conversation was compacted; the summary is stored in a format this model cannot read]";

pub(crate) fn model_summary_is_usable(summary: &str) -> Result<(), String> {
    match baseline_summary_failure(summary) {
        Some(failure) => Err(failure.to_string()),
        None => Ok(()),
    }
}

fn baseline_summary_failure(summary: &str) -> Option<SummaryValidationFailure> {
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        return Some(SummaryValidationFailure::Empty);
    }
    if trimmed.contains("<|eos|>")
        || trimmed.contains("<file_end>")
        || trimmed.contains("<tool_call>")
        || trimmed.contains("</tool_call>")
    {
        return Some(SummaryValidationFailure::ControlToken);
    }
    if trimmed.chars().count() < MIN_USABLE_SUMMARY_CHARS {
        return Some(SummaryValidationFailure::Tiny);
    }
    None
}

const REQUIRED_CHECKPOINT_HEADINGS: [&str; 5] = [
    "## User requirements and confirmed facts",
    "## User corrections and open disagreements",
    "## Durable observations",
    "## Agent conclusions (unverified)",
    "## Remaining work",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SummaryValidationFailure {
    Empty,
    Tiny,
    ControlToken,
    MissingHeading,
    DuplicateHeading,
}

impl SummaryValidationFailure {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Tiny => "tiny",
            Self::ControlToken => "control-token",
            Self::MissingHeading => "missing-heading",
            Self::DuplicateHeading => "duplicate-heading",
        }
    }
}

impl std::fmt::Display for SummaryValidationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "compaction summary is empty"),
            Self::Tiny => write!(f, "compaction summary is too short to be a usable handoff"),
            Self::ControlToken => write!(f, "compaction summary contains leaked control tokens"),
            Self::MissingHeading => {
                write!(f, "compaction summary is missing a required checkpoint heading")
            }
            Self::DuplicateHeading => {
                write!(f, "compaction summary repeats a required checkpoint heading")
            }
        }
    }
}

/// Shared validator for every compact generation path.
pub(crate) fn validate_checkpoint_summary(summary: &str) -> Result<(), SummaryValidationFailure> {
    if let Some(failure) = baseline_summary_failure(summary) {
        return Err(failure);
    }
    let trimmed = summary.trim();
    let mut seen = [false; REQUIRED_CHECKPOINT_HEADINGS.len()];
    for line in trimmed.lines().map(str::trim) {
        if let Some(index) = REQUIRED_CHECKPOINT_HEADINGS
            .iter()
            .position(|heading| *heading == line)
        {
            if seen[index] {
                return Err(SummaryValidationFailure::DuplicateHeading);
            }
            seen[index] = true;
        }
    }
    if seen.iter().any(|present| !*present) {
        return Err(SummaryValidationFailure::MissingHeading);
    }
    Ok(())
}

pub(crate) fn model_summary_validation_error(summary: &str) -> Option<SummaryValidationFailure> {
    validate_checkpoint_summary(summary).err()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct CompactionSelection {
    pub(crate) target_tokens: u64,
    pub(crate) estimated_tokens: u64,
    pub(crate) truncated: bool,
}

impl Default for CompactionSelection {
    fn default() -> Self {
        Self {
            target_tokens: DEFAULT_TAIL_TOKEN_LIMIT,
            estimated_tokens: 0,
            truncated: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct CompactedContext {
    pub(crate) checkpoint: String,
    pub(crate) retained: Vec<Value>,
    pub(crate) generation: u64,
    pub(crate) selection: CompactionSelection,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct CheckpointPayload {
    format: String,
    source: String,
    authority: String,
    text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct EnvelopeV2 {
    version: u64,
    generation: u64,
    checkpoint: CheckpointPayload,
    retained: Vec<Value>,
    selection: CompactionSelection,
}

#[derive(Clone, Debug)]
pub(crate) struct CompactionMetrics {
    pub(crate) envelope_version: u64,
    pub(crate) generation: u64,
    pub(crate) tokens_before: u64,
    pub(crate) checkpoint_tokens: u64,
    pub(crate) retained_tokens: u64,
    pub(crate) retained_turns: usize,
    pub(crate) removed_items: usize,
    pub(crate) validation: &'static str,
    pub(crate) repaired: bool,
}

impl CompactionMetrics {
    pub(crate) fn log_line(&self) -> String {
        format!(
            "local compaction metrics version={} generation={} tokens_before={} checkpoint_tokens={} retained_tokens={} retained_turns={} removed_items={} validation={} repaired={}",
            self.envelope_version,
            self.generation,
            self.tokens_before,
            self.checkpoint_tokens,
            self.retained_tokens,
            self.retained_turns,
            self.removed_items,
            self.validation,
            self.repaired
        )
    }
}

const FROZEN_CONCLUSION_BANNER: &str = "CHECKPOINT SANITIZED: Some sentences below stated agent beliefs as fact. Treat those as unverified. A later user message outranks them.";

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
        let after: String = summary[index + "参考地形は".len()..].chars().take(16).collect();
        if !after.contains("ない") && !after.contains('違') {
            return true;
        }
    }
    let mut rest = summary;
    while let Some(index) = rest.find("参考は") {
        let after: String = rest[index + "参考は".len()..].chars().take(24).collect();
        if !after.contains('違')
            && !after.contains("ではなく")
            && !after.contains("ではない")
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
    Ok(format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(payload)))
}

pub(crate) fn encode_compacted_context(context: &CompactedContext) -> Result<String, String> {
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
    let payload = serde_json::to_vec(&envelope)
        .map_err(|_| "compaction envelope cannot be encoded".to_string())?;
    if payload.len() > MAX_SUMMARY_BYTES {
        return Err("compaction summary exceeds the CODETAS limit".into());
    }
    Ok(format!("{PREFIX_V2}{}", URL_SAFE_NO_PAD.encode(payload)))
}

#[derive(Debug)]
enum LocalEnvelope {
    V1 { summary: String },
    V2 { context: CompactedContext },
    Legacy { summary: String },
}

#[derive(Debug)]
enum EnvelopeError {
    Malformed(&'static str),
    UnsupportedVersion,
    NotLocal,
}

impl EnvelopeError {
    fn as_validation_message(&self) -> String {
        match self {
            Self::Malformed(message) => (*message).into(),
            Self::UnsupportedVersion => "CODETAS compaction envelope version is unsupported".into(),
            Self::NotLocal => "translated providers can consume only CODETAS compaction envelopes".into(),
        }
    }
}

fn decode_local_envelope(encrypted: &str) -> Result<LocalEnvelope, EnvelopeError> {
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
        return Err(EnvelopeError::Malformed("Compaction envelope has no summary"));
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
        const KNOWN: [&str; 5] = ["version", "generation", "checkpoint", "retained", "selection"];
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
    match decode_local_envelope(encrypted) {
        Ok(LocalEnvelope::V1 { summary } | LocalEnvelope::Legacy { summary }) => {
            Ok(Some(format_checkpoint_text(&summary)))
        }
        Ok(LocalEnvelope::V2 { context }) => Ok(Some(format_checkpoint_text(&context.checkpoint))),
        Err(error) => Err(error.as_validation_message()),
    }
}

fn format_checkpoint_text(summary: &str) -> String {
    format!(
        "{SUMMARY_PREFIX}\n\n<codetas_compaction_summary>\n{summary}\n</codetas_compaction_summary>"
    )
}

fn checkpoint_handoff_message(summary: &str) -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": format_checkpoint_text(summary)}]
    })
}

fn invalid_envelope_marker() -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": "[invalid local compaction summary omitted]"
        }]
    })
}

fn opaque_compaction_marker() -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": OPAQUE_COMPACTION_NOTE}]
    })
}

pub(crate) fn request_is_remote_compaction(body: &Value) -> bool {
    body.get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction_trigger"))
        })
}

pub(crate) fn compaction_item_count(response: &Value) -> usize {
    response
        .get("output")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    matches!(
                        item.get("type").and_then(Value::as_str),
                        Some("compaction" | "compaction_summary" | "context_compaction")
                    )
                })
                .count()
        })
        .unwrap_or(0)
}

pub(crate) fn response_output_text(response: &Value) -> String {
    let mut output = String::new();
    let Some(items) = response.get("output").and_then(Value::as_array) else {
        return output;
    };
    for item in items {
        let Some(parts) = item.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in parts {
            if matches!(
                part.get("type").and_then(Value::as_str),
                Some("output_text" | "text")
            ) {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    output.push_str(text);
                }
            }
        }
    }
    output
}

pub(crate) fn validate_local_compactions(body: &Value) -> Result<(), String> {
    let Some(items) = body.get("input").and_then(Value::as_array) else {
        return Ok(());
    };
    for item in items {
        let item_type = item.get("type").and_then(Value::as_str);
        if !matches!(item_type, Some("compaction" | "compaction_summary" | "context_compaction")) {
            continue;
        }
        let Some(encrypted) = item.get("encrypted_content").and_then(Value::as_str) else {
            continue;
        };
        if !(encrypted.starts_with(PREFIX)
            || encrypted.starts_with(PREFIX_V2)
            || encrypted.starts_with(LEGACY_PREFIX))
        {
            continue;
        }
        decode_local_envelope(encrypted).map_err(|error| {
            format!("invalid local compaction envelope: {}", error.as_validation_message())
        })?;
    }
    Ok(())
}

pub(crate) fn expand_local_compactions(body: &mut Value) {
    expand_compaction_items(body, false);
}

/// Expand local envelopes and replace OpenAI-opaque compaction blobs with a
/// note. Used on translated / synthetic hops that cannot decrypt `gAAAAA`.
pub(crate) fn expand_translated_compactions(body: &mut Value) {
    expand_compaction_items(body, true);
}

fn expand_compaction_items(body: &mut Value, rewrite_opaque: bool) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    let mut expanded = Vec::with_capacity(items.len());
    for item in items.drain(..) {
        let kind = item.get("type").and_then(Value::as_str);
        if !matches!(
            kind,
            Some("compaction" | "compaction_summary" | "context_compaction")
        ) {
            expanded.push(item);
            continue;
        }
        let encrypted = item.get("encrypted_content").and_then(Value::as_str);
        let local = encrypted.is_some_and(|value| {
            value.starts_with(PREFIX)
                || value.starts_with(PREFIX_V2)
                || value.starts_with(LEGACY_PREFIX)
        });
        if !local {
            if rewrite_opaque {
                expanded.push(opaque_compaction_marker());
            } else {
                expanded.push(item);
            }
            continue;
        }
        match decode_local_envelope(encrypted.unwrap_or_default()) {
            Ok(LocalEnvelope::V1 { summary } | LocalEnvelope::Legacy { summary }) => {
                expanded.push(checkpoint_handoff_message(&summary));
            }
            Ok(LocalEnvelope::V2 { context }) => {
                expanded.push(checkpoint_handoff_message(&context.checkpoint));
                expanded.extend(context.retained);
            }
            Err(error) => {
                crate::debug::log(&format!(
                    "invalid local compaction envelope replaced with a marker: {}",
                    error.as_validation_message()
                ));
                expanded.push(invalid_envelope_marker());
            }
        }
    }
    *items = expanded;
}

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
        if matches!(kind, Some("compaction_trigger" | "additional_tools" | "reasoning")) {
            continue;
        }
        if let Some(sanitized) = sanitize_history_item(item)? {
            normalized.push(sanitized);
        }
    }
    Ok(NormalizedHistory {
        previous_checkpoint,
        previous_generation,
        items: normalized,
    })
}

fn sanitize_history_item(item: &Value) -> Result<Option<Value>, String> {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => {
            match item.get("role").and_then(Value::as_str) {
                Some("user" | "assistant") => Ok(Some(replace_inline_images(item))),
                Some("system" | "developer") => Ok(None),
                _ => Ok(None),
            }
        }
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

fn validate_retained_items(items: &[Value]) -> Result<(), String> {
    if items.len() > MAX_RETAINED_ITEMS {
        return Err("compaction retained item count exceeds the limit".into());
    }
    let mut open_calls = std::collections::HashSet::new();
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => match item.get("role").and_then(Value::as_str) {
                Some("user" | "assistant") => {}
                Some("system" | "developer") => {
                    return Err("compaction retained items cannot include system or developer messages".into());
                }
                _ => return Err("compaction retained message role is not allowed".into()),
            },
            Some(
                "function_call" | "custom_tool_call" | "local_shell_call" | "tool_search_call",
            ) => {
                if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                    open_calls.insert(call_id.to_string());
                }
            }
            Some(
                "function_call_output"
                | "custom_tool_call_output"
                | "local_shell_call_output"
                | "tool_search_output",
            ) => {
                if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                    open_calls.remove(call_id);
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
    Ok(())
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
    fn starts_with_user(&self) -> bool {
        self.items.first().is_some_and(is_user_message)
    }
}

fn is_user_message(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("message")
        && item.get("role").and_then(Value::as_str) == Some("user")
}

fn is_tool_call(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("function_call" | "custom_tool_call" | "local_shell_call" | "tool_search_call")
    )
}

fn is_tool_result(item: &Value) -> bool {
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

fn call_id(item: &Value) -> Option<&str> {
    item.get("call_id").and_then(Value::as_str)
}

pub(crate) fn split_interaction_groups(items: &[Value]) -> Vec<InteractionGroup> {
    let mut groups = Vec::new();
    let mut current = Vec::new();
    let mut open_calls = std::collections::HashSet::new();
    for item in items {
        let starts_new_user_turn = is_user_message(item) && !current.is_empty() && open_calls.is_empty();
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
    let groups = split_interaction_groups(items);
    if groups.is_empty() {
        return Ok(HistorySplit {
            prefix: Vec::new(),
            tail: Vec::new(),
            selection: CompactionSelection {
                target_tokens: tail_token_limit,
                estimated_tokens: 0,
                truncated: false,
            },
            retained_turns: 0,
        });
    }
    if let Some(last_user) = groups.iter().rev().find(|group| group.starts_with_user()) {
        let last_user_tokens = estimate_input_items_tokens(&last_user.items);
        if last_user_tokens > tail_token_limit && groups.last() == Some(last_user) && last_user.items.len() == 1 {
            return Err("last user message exceeds the retained tail token limit".into());
        }
    }
    let mut retained_from = groups.len();
    let mut retained_tokens: u64 = 0;
    for index in (0..groups.len()).rev() {
        let group_tokens = estimate_input_items_tokens(&groups[index].items);
        if retained_from < groups.len() && retained_tokens.saturating_add(group_tokens) > tail_token_limit {
            break;
        }
        if retained_from == groups.len() && group_tokens > tail_token_limit {
            // Oversized latest complete turn goes to the checkpoint instead of
            // being sliced mid-message. Keep any later smaller complete turns.
            break;
        }
        retained_tokens = retained_tokens.saturating_add(group_tokens);
        retained_from = index;
    }
    let prefix = groups[..retained_from]
        .iter()
        .flat_map(|group| group.items.iter().cloned())
        .collect::<Vec<_>>();
    let tail = groups[retained_from..]
        .iter()
        .flat_map(|group| group.items.iter().cloned())
        .collect::<Vec<_>>();
    let prefix_ids = tool_ids(&prefix);
    let tail_ids = tool_ids(&tail);
    if prefix_ids.intersection(&tail_ids).next().is_some() {
        return Err("compaction split a tool call from its result".into());
    }
    Ok(HistorySplit {
        prefix,
        tail: tail.clone(),
        selection: CompactionSelection {
            target_tokens: tail_token_limit,
            estimated_tokens: estimate_input_items_tokens(&tail),
            truncated: retained_from > 0,
        },
        retained_turns: groups.len().saturating_sub(retained_from),
    })
}

fn tool_ids(items: &[Value]) -> std::collections::HashSet<String> {
    items
        .iter()
        .filter(|item| is_tool_call(item) || is_tool_result(item))
        .filter_map(call_id)
        .map(str::to_string)
        .collect()
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

pub(crate) const REPAIR_PROMPT: &str = "The previous checkpoint failed validation. Rewrite it using exactly these headings, in this order:\n## User requirements and confirmed facts\n## User corrections and open disagreements\n## Durable observations\n## Agent conclusions (unverified)\n## Remaining work\nDo not emit control tokens. Keep user corrections in the corrections section. Do not promote unverified agent conclusions into confirmed facts.";

pub(crate) fn accepted_or_repaired_checkpoint(
    summary: &str,
    repaired: Option<&str>,
) -> Result<(String, bool), SummaryValidationFailure> {
    if validate_checkpoint_summary(summary).is_ok() {
        return Ok((summary.trim().to_string(), false));
    }
    let repaired = repaired.ok_or_else(|| {
        validate_checkpoint_summary(summary).expect_err("invalid summary")
    })?;
    validate_checkpoint_summary(repaired)?;
    Ok((repaired.trim().to_string(), true))
}

fn checkpoint_section_body<'a>(checkpoint: &'a str, heading: &str) -> Option<&'a str> {
    let start = checkpoint.find(heading)? + heading.len();
    let next = REQUIRED_CHECKPOINT_HEADINGS
        .iter()
        .filter(|candidate| **candidate != heading)
        .filter_map(|candidate| checkpoint[start..].find(candidate).map(|index| start + index))
        .min()
        .unwrap_or(checkpoint.len());
    Some(&checkpoint[start..next])
}

fn collect_preserved_user_texts(previous_checkpoint: Option<&str>, prefix: &[Value]) -> Vec<String> {
    let mut texts = Vec::new();
    if let Some(previous) = previous_checkpoint {
        if let Some(body) = checkpoint_section_body(previous, "## User corrections and open disagreements")
        {
            for line in body.lines() {
                let trimmed = line.trim().trim_start_matches('-').trim();
                if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
                    continue;
                }
                texts.push(trimmed.to_string());
            }
        }
    }
    for item in prefix {
        if is_user_message(item) {
            if let Some(text) = message_text(item) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    texts.push(trimmed.to_string());
                }
            }
        }
    }
    texts
}

fn message_text(item: &Value) -> Option<String> {
    match item.get("content") {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(parts)) => {
            let text = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<String>();
            (!text.trim().is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn merge_prefix_corrections(
    checkpoint: &str,
    previous_checkpoint: Option<&str>,
    prefix: &[Value],
) -> String {
    let corrections = collect_preserved_user_texts(previous_checkpoint, prefix);
    if corrections.is_empty() {
        return checkpoint.to_string();
    }
    let heading = "## User corrections and open disagreements";
    let Some(start) = checkpoint.find(heading) else {
        return checkpoint.to_string();
    };
    let after = start + heading.len();
    let next = REQUIRED_CHECKPOINT_HEADINGS
        .iter()
        .skip(2)
        .filter_map(|candidate| checkpoint[after..].find(candidate).map(|index| after + index))
        .min()
        .unwrap_or(checkpoint.len());
    let existing = checkpoint[after..next].to_string();
    let mut extra = String::new();
    for correction in corrections {
        let trimmed = correction.trim();
        if trimmed.is_empty()
            || checkpoint.contains(trimmed)
            || extra.contains(trimmed)
        {
            continue;
        }
        extra.push_str("\n- ");
        extra.push_str(trimmed);
    }
    if extra.is_empty() {
        return checkpoint.to_string();
    }
    let mut merged = String::new();
    merged.push_str(&checkpoint[..after]);
    merged.push_str(&existing.trim_end());
    merged.push_str(&extra);
    merged.push('\n');
    merged.push_str(&checkpoint[next..]);
    merged
}

fn strip_proliferating_framing(checkpoint: &str) -> String {
    checkpoint
        .replace(SUMMARY_PREFIX, "")
        .replace("<codetas_compaction_summary>", "")
        .replace("</codetas_compaction_summary>", "")
        .replace(FROZEN_CONCLUSION_BANNER, "")
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

pub(crate) fn build_compacted_context(
    history: &NormalizedHistory,
    checkpoint: String,
    settings: &LocalCompactionSettings,
) -> Result<(CompactedContext, CompactionMetrics), String> {
    build_compacted_context_with_repair(history, checkpoint, settings, false)
}

pub(crate) fn build_compacted_context_with_repair(
    history: &NormalizedHistory,
    checkpoint: String,
    settings: &LocalCompactionSettings,
    repaired: bool,
) -> Result<(CompactedContext, CompactionMetrics), String> {
    let split = split_prefix_and_tail(&history.items, settings.tail_token_limit())?;
    let checkpoint = strip_proliferating_framing(&checkpoint);
    validate_checkpoint_summary(&checkpoint).map_err(|error| error.to_string())?;
    let checkpoint = merge_prefix_corrections(
        &checkpoint,
        history.previous_checkpoint.as_deref(),
        &split.prefix,
    );
    validate_retained_items(&split.tail)?;
    let context = CompactedContext {
        checkpoint: checkpoint.clone(),
        retained: split.tail.clone(),
        generation: history.previous_generation.saturating_add(1),
        selection: split.selection.clone(),
    };
    let metrics = CompactionMetrics {
        envelope_version: if settings.generate_v2() { 2 } else { 1 },
        generation: context.generation,
        tokens_before: estimate_input_items_tokens(&history.items),
        checkpoint_tokens: estimate_input_items_tokens(&[checkpoint_handoff_message(&checkpoint)]),
        retained_tokens: split.selection.estimated_tokens,
        retained_turns: split.retained_turns,
        removed_items: split.prefix.len(),
        validation: "accepted",
        repaired,
    };
    crate::debug::log(&metrics.log_line());
    Ok((context, metrics))
}

pub(crate) fn encode_context_for_settings(
    context: &CompactedContext,
    settings: &LocalCompactionSettings,
) -> Result<String, String> {
    if settings.generate_v2() {
        encode_compacted_context(context)
    } else {
        encode_summary(&context.checkpoint)
    }
}

pub(crate) fn standalone_output_items(context: &CompactedContext) -> Vec<Value> {
    let mut output = vec![checkpoint_handoff_message(&context.checkpoint)];
    output.extend(context.retained.iter().cloned());
    output
}

pub(crate) fn native_compaction_item(context: &CompactedContext, settings: &LocalCompactionSettings) -> Result<Value, String> {
    let encrypted = encode_context_for_settings(context, settings)?;
    Ok(json!({
        "id": format!("cmpctitem_{}", uuid::Uuid::new_v4().simple()),
        "type": "compaction",
        "encrypted_content": encrypted,
        "created_by": "codetas"
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_local_compaction_envelopes_before_translation() {
        let valid = encode_summary("keep this summary").expect("valid envelope");
        assert!(validate_local_compactions(&json!({
            "input": [{"type": "compaction", "encrypted_content": valid}]
        }))
        .is_ok());
        assert!(validate_local_compactions(&json!({
            "input": [{"type": "compaction", "encrypted_content": "codetas1:not-base64"}]
        }))
        .is_err());
        assert!(validate_local_compactions(&json!({
            "input": [{"type": "reasoning", "encrypted_content": "codetas1:opaque"}]
        }))
        .is_ok());
    }

    #[test]
    fn malformed_local_compaction_is_not_silently_dropped() {
        let mut body = json!({
            "input": [{"type": "compaction", "encrypted_content": "codetas1:not-base64"}]
        });
        expand_local_compactions(&mut body);
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "assistant");
        assert_eq!(
            body["input"][0]["content"][0]["text"],
            "[invalid local compaction summary omitted]"
        );
    }

    #[test]
    fn expand_local_compactions_leaves_native_trigger_and_non_local_items() {
        let encrypted = encode_summary("keep this summary").expect("valid envelope");
        let mut body = json!({
            "input": [
                {"type": "compaction", "encrypted_content": encrypted},
                {"type": "compaction", "encrypted_content": "gAAAAABopaque"},
                {"type": "message", "role": "user", "content": "continue"},
                {"type": "compaction_trigger", "id": "trigger_1"}
            ]
        });
        expand_translated_compactions(&mut body);
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "assistant");
        assert!(
            body["input"][0]["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("keep this summary")
                    && text.contains("outranks the checkpoint"))
        );
        assert_eq!(body["input"][1]["type"], "message");
        assert_eq!(
            body["input"][1]["content"][0]["text"],
            OPAQUE_COMPACTION_NOTE
        );
        assert_eq!(body["input"][2]["role"], "user");
        assert_eq!(body["input"][3]["type"], "compaction_trigger");
        assert!(!body["input"][0].to_string().contains("codetas1:"));
    }

    #[test]
    fn checkpoint_prompt_separates_unverified_conclusions_from_user_facts() {
        assert!(COMPACT_PROMPT.contains("unverified"));
        assert!(COMPACT_PROMPT.contains("User corrections and open disagreements"));
        assert!(SUMMARY_PREFIX.contains("outranks the checkpoint"));
        assert!(!SUMMARY_PREFIX.contains("do not restart the same search"));
        assert!(SUMMARY_PREFIX.contains("searching again"));
    }

    #[test]
    fn sanitizes_summaries_that_freeze_unverified_conclusions() {
        let frozen = sanitize_model_summary(
            "User wants rolling ground. Answer already established: reference terrain is Code Desert dunes. Next: keep that formula.",
        );
        assert!(frozen.starts_with("CHECKPOINT SANITIZED:"));
        assert!(model_summary_is_usable(&frozen).is_ok());

        let japanese = sanitize_model_summary(
            "ユーザーは起伏地面を求めている。参考はコード砂漠です。次に砂丘の式を入れる。",
        );
        assert!(japanese.starts_with("CHECKPOINT SANITIZED:"));

        let correction = sanitize_model_summary(
            "User requirements: the reference is not Code Desert. 参考はコード砂漠→違います。Agent conclusions (unverified): Sky City hills. Next: inspect the other worktree.",
        );
        assert!(!correction.starts_with("CHECKPOINT SANITIZED:"));
        assert!(model_summary_is_usable(&correction).is_ok());
    }

    #[test]
    fn rejects_leaked_control_tokens_and_tiny_handoffs() {
        assert!(model_summary_is_usable("隠れ敵の撃破描画と被ダメ位置のずれ原因を、関連関数から特定します。\n<file_end><|eos|>").is_err());
        assert!(model_summary_is_usable("<tool_call>sed -n '1,20p' file.js</tool_call>").is_err());
        assert!(model_summary_is_usable("too short").is_err());
        assert!(model_summary_is_usable(
            "User wants attack cues removed and chapter-two hidden enemies to despawn. \
             renderer.js#drawEnemyTelegraph still emits 攻撃がくる. Next: delete that string \
             and skip defeated silt/mud burrowers in the draw loop."
        )
        .is_ok());
    }

    #[test]
    fn detects_remote_compaction_trigger() {
        assert!(request_is_remote_compaction(&json!({
            "input": [
                {"type": "message", "role": "user", "content": []},
                {"type": "compaction_trigger"}
            ]
        })));
        assert!(!request_is_remote_compaction(&json!({
            "input": [{"type": "message", "role": "user", "content": []}]
        })));
    }

    #[test]
    fn counts_only_compaction_output_items() {
        assert_eq!(
            compaction_item_count(&json!({
                "output": [
                    {"type": "reasoning"},
                    {"type": "message"}
                ]
            })),
            0
        );
        assert_eq!(
            compaction_item_count(&json!({
                "output": [{"type": "compaction", "encrypted_content": "codetas1:x"}]
            })),
            1
        );
    }

    fn fixture_checkpoint(corrections: &str) -> String {
        format!(
            "## User requirements and confirmed facts\n\
             - User wants moss stepping-stone terrain.\n\n\
             ## User corrections and open disagreements\n\
             {corrections}\n\n\
             ## Durable observations\n\
             - Inspected terrain references in the current worktree.\n\n\
             ## Agent conclusions (unverified)\n\
             - Earlier agent said reference terrain is Code Desert dunes.\n\n\
             ## Remaining work\n\
             - Continue from the moss stepping-stone terrain, not the rejected desert formula."
        )
    }

    fn user_message(text: &str) -> Value {
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]})
    }

    fn assistant_message(text: &str) -> Value {
        json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]})
    }

    #[test]
    fn v2_envelope_round_trips_and_rejects_unknown_or_malformed_payloads() {
        let context = CompactedContext {
            checkpoint: fixture_checkpoint("- User: 砂漠は違う。moss stepping-stone terrain を使う."),
            retained: vec![user_message("continue with moss")],
            generation: 3,
            selection: CompactionSelection {
                target_tokens: 20_000,
                estimated_tokens: 12,
                truncated: true,
            },
        };
        let encoded = encode_compacted_context(&context).expect("encode v2");
        assert!(encoded.starts_with("codetas2:"));
        match decode_local_envelope(&encoded).expect("decode v2") {
            LocalEnvelope::V2 { context: decoded } => {
                assert_eq!(decoded.checkpoint, context.checkpoint);
                assert_eq!(decoded.retained, context.retained);
                assert_eq!(decoded.generation, 3);
                assert_eq!(decoded.selection.estimated_tokens, 12);
            }
            _ => panic!("expected v2 envelope"),
        }

        assert!(matches!(
            decode_local_envelope("codetas2:not-base64"),
            Err(EnvelopeError::Malformed(_))
        ));
        let unknown = format!(
            "codetas2:{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"version": 9, "summary": "x"})).unwrap())
        );
        assert!(matches!(
            decode_local_envelope(&unknown),
            Err(EnvelopeError::UnsupportedVersion)
        ));
        let missing = format!(
            "codetas2:{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"version": 2})).unwrap())
        );
        assert!(matches!(
            decode_local_envelope(&missing),
            Err(EnvelopeError::Malformed(_))
        ));
        let extra = format!(
            "codetas2:{}",
            URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&json!({
                    "version": 2,
                    "generation": 1,
                    "checkpoint": {
                        "format": CHECKPOINT_FORMAT,
                        "source": "model-generated",
                        "authority": CHECKPOINT_AUTHORITY,
                        "text": fixture_checkpoint("- keep")
                    },
                    "retained": [],
                    "selection": {"target_tokens": 1, "estimated_tokens": 0, "truncated": false},
                    "extra": true
                }))
                .unwrap()
            )
        );
        assert!(matches!(
            decode_local_envelope(&extra),
            Err(EnvelopeError::Malformed(_))
        ));
    }

    #[test]
    fn v1_generation_still_decodes_existing_v2_envelopes() {
        let v2 = encode_compacted_context(&CompactedContext {
            checkpoint: fixture_checkpoint("- keep the correction"),
            retained: vec![user_message("continue")],
            generation: 2,
            selection: CompactionSelection::default(),
        })
        .expect("v2");
        let v1_settings = LocalCompactionSettings {
            envelope: crate::config::LocalCompactionEnvelope::V1,
            tail_token_limit: 20_000,
        };
        let history = NormalizedHistory {
            previous_checkpoint: None,
            previous_generation: 0,
            items: vec![user_message("continue")],
        };
        let (context, _) = build_compacted_context(
            &history,
            fixture_checkpoint("- keep the correction"),
            &v1_settings,
        )
        .expect("v1 generate");
        let encoded = encode_context_for_settings(&context, &v1_settings).expect("encode v1");
        assert!(encoded.starts_with("codetas1:"));
        let mut body = json!({
            "input": [
                {"type": "compaction", "encrypted_content": v2},
                {"type": "compaction", "encrypted_content": encoded}
            ]
        });
        expand_local_compactions(&mut body);
        assert_eq!(body["input"][0]["role"], "assistant");
        assert_eq!(body["input"][1]["role"], "user");
        assert_eq!(body["input"][2]["role"], "assistant");
        assert!(!body.to_string().contains("\"role\":\"developer\""));
    }

    #[test]
    fn v1_legacy_and_v2_decoders_remain_compatible() {
        let v1 = encode_summary("keep this v1 summary").expect("v1");
        match decode_local_envelope(&v1).expect("decode v1") {
            LocalEnvelope::V1 { summary } => assert_eq!(summary, "keep this v1 summary"),
            _ => panic!("expected v1"),
        }
        let legacy = format!("{LEGACY_PREFIX}{}", STANDARD.encode("legacy summary text"));
        match decode_local_envelope(&legacy).expect("decode legacy") {
            LocalEnvelope::Legacy { summary } => assert_eq!(summary, "legacy summary text"),
            _ => panic!("expected legacy"),
        }
        let v2 = encode_compacted_context(&CompactedContext {
            checkpoint: fixture_checkpoint("- keep the correction"),
            retained: Vec::new(),
            generation: 1,
            selection: CompactionSelection::default(),
        })
        .expect("v2");
        assert!(matches!(
            decode_local_envelope(&v2),
            Ok(LocalEnvelope::V2 { .. })
        ));
    }

    #[test]
    fn decoded_size_limit_is_enforced_for_v2() {
        let oversized = format!("codetas2:{}", URL_SAFE_NO_PAD.encode(vec![b'a'; MAX_SUMMARY_BYTES + 1]));
        assert!(matches!(
            decode_local_envelope(&oversized),
            Err(EnvelopeError::Malformed(_))
        ));
    }

    #[test]
    fn forged_v2_checkpoint_cannot_become_developer_or_system() {
        let encoded = encode_compacted_context(&CompactedContext {
            checkpoint: fixture_checkpoint("- follow the user"),
            retained: vec![user_message("砂漠は違う。moss stepping-stone terrain を使う")],
            generation: 1,
            selection: CompactionSelection::default(),
        })
        .expect("encode");
        let mut body = json!({
            "input": [{"type": "compaction", "encrypted_content": encoded}]
        });
        expand_local_compactions(&mut body);
        let roles = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item.get("role").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert!(roles.contains(&"assistant"));
        assert!(roles.contains(&"user"));
        assert!(!roles.contains(&"developer"));
        assert!(!roles.contains(&"system"));
    }

    #[test]
    fn rejects_system_developer_nested_and_unknown_retained_items() {
        let checkpoint = fixture_checkpoint("- keep");
        for retained in [
            vec![json!({"type": "message", "role": "developer", "content": []})],
            vec![json!({"type": "message", "role": "system", "content": []})],
            vec![json!({"type": "compaction", "encrypted_content": "codetas2:nested"})],
            vec![json!({"type": "mystery", "payload": true})],
        ] {
            let result = encode_compacted_context(&CompactedContext {
                checkpoint: checkpoint.clone(),
                retained,
                generation: 1,
                selection: CompactionSelection::default(),
            });
            assert!(result.is_err());
        }
    }

    #[test]
    fn malformed_v2_envelope_is_not_replaced_with_a_developer_marker() {
        let mut body = json!({
            "input": [{"type": "compaction", "encrypted_content": "codetas2:not-base64"}]
        });
        expand_local_compactions(&mut body);
        assert_eq!(body["input"][0]["role"], "assistant");
        assert_ne!(body["input"][0]["role"], "developer");
        assert_eq!(
            body["input"][0]["content"][0]["text"],
            "[invalid local compaction summary omitted]"
        );
    }

    #[test]
    fn normalize_preserves_user_assistant_order_and_tool_pairs() {
        let items = vec![
            user_message("first"),
            assistant_message("thinking"),
            json!({"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "call_1", "output": "ok"}),
            user_message("second"),
            json!({"type": "compaction_trigger"}),
            json!({"type": "additional_tools", "tools": []}),
            json!({"type": "reasoning", "encrypted_content": "secret"}),
            json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_image", "image_url": "data:image/png;base64,AA=="}]
            }),
        ];
        let history = normalize_compaction_history(&items).expect("normalize");
        assert_eq!(history.items[0]["role"], "user");
        assert_eq!(history.items[1]["role"], "assistant");
        assert_eq!(history.items[2]["type"], "function_call");
        assert_eq!(history.items[3]["type"], "function_call_output");
        assert_eq!(history.items[4]["content"][0]["text"], "second");
        assert_eq!(history.items[5]["content"][0]["text"], IMAGE_MARKER);
        assert!(history.items.iter().all(|item| {
            !matches!(
                item.get("type").and_then(Value::as_str),
                Some("compaction_trigger" | "additional_tools" | "reasoning")
            )
        }));
    }

    #[test]
    fn parallel_tool_calls_stay_in_one_group_and_orphans_do_not_split() {
        let items = vec![
            user_message("lookup both"),
            json!({"type": "function_call", "call_id": "a", "name": "one", "arguments": "{}"}),
            json!({"type": "function_call", "call_id": "b", "name": "two", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "a", "output": "A"}),
            json!({"type": "function_call_output", "call_id": "b", "output": "B"}),
            json!({"type": "function_call_output", "call_id": "missing", "output": "orphan"}),
            user_message("next"),
        ];
        let groups = split_interaction_groups(&items);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].items.len(), 6);
        assert_eq!(groups[1].items[0]["content"][0]["text"], "next");
    }

    #[test]
    fn incomplete_tool_turn_stays_in_the_tail() {
        let items = vec![
            user_message("old"),
            assistant_message("done"),
            user_message("new"),
            json!({"type": "function_call", "call_id": "open", "name": "lookup", "arguments": "{}"}),
        ];
        let split = split_prefix_and_tail(&items, 20_000).expect("split");
        assert!(split.prefix.is_empty() || split.tail.iter().any(is_user_message));
        assert!(split.tail.iter().any(|item| {
            item.get("call_id").and_then(Value::as_str) == Some("open")
        }));
    }

    #[test]
    fn prefix_and_tail_are_disjoint_and_rejoin_to_normalized_history() {
        let items = (0..8)
            .flat_map(|index| {
                vec![
                    user_message(&format!("turn {index} {}", "word ".repeat(40))),
                    assistant_message(&format!("reply {index} {}", "text ".repeat(40))),
                ]
            })
            .collect::<Vec<_>>();
        let split = split_prefix_and_tail(&items, 200).expect("split");
        assert!(split.selection.estimated_tokens <= 200);
        let mut seen = std::collections::HashSet::new();
        for item in split.prefix.iter().chain(split.tail.iter()) {
            let key = item.to_string();
            assert!(seen.insert(key), "duplicate item across prefix/tail");
        }
        let mut rejoined = split.prefix.clone();
        rejoined.extend(split.tail.iter().cloned());
        assert_eq!(rejoined, items);
        assert!(split.tail.iter().any(is_user_message));
    }

    #[test]
    fn last_user_message_over_budget_is_retryable() {
        let huge = user_message(&"x".repeat(200_000));
        let error = split_prefix_and_tail(&[huge], 20).expect_err("over budget");
        assert!(error.contains("last user message exceeds"));
    }

    #[test]
    fn repair_is_attempted_once_and_second_failure_stays_a_compact_failure() {
        let first = "too short";
        assert!(accepted_or_repaired_checkpoint(first, None).is_err());
        let repaired = fixture_checkpoint("- User: 砂漠は違う");
        let (text, was_repaired) = accepted_or_repaired_checkpoint(first, Some(&repaired)).expect("repair");
        assert!(was_repaired);
        assert!(text.contains("砂漠は違う"));
        assert!(accepted_or_repaired_checkpoint(first, Some("still too short")).is_err());
    }

    #[test]
    fn validator_rejects_empty_tiny_control_tokens_and_heading_problems() {
        assert_eq!(
            validate_checkpoint_summary(""),
            Err(SummaryValidationFailure::Empty)
        );
        assert_eq!(
            validate_checkpoint_summary("too short"),
            Err(SummaryValidationFailure::Tiny)
        );
        assert_eq!(
            validate_checkpoint_summary("隠れ敵の撃破描画と被ダメ位置のずれ原因を、関連関数から特定します。\n<file_end><|eos|>"),
            Err(SummaryValidationFailure::ControlToken)
        );
        let missing = fixture_checkpoint("- keep").replace("## Remaining work", "## Next");
        assert_eq!(
            validate_checkpoint_summary(&missing),
            Err(SummaryValidationFailure::MissingHeading)
        );
        let duplicate = format!(
            "{}\n## User requirements and confirmed facts\n- again",
            fixture_checkpoint("- keep")
        );
        assert_eq!(
            validate_checkpoint_summary(&duplicate),
            Err(SummaryValidationFailure::DuplicateHeading)
        );
        assert!(validate_checkpoint_summary(&fixture_checkpoint(
            "- User rejected Code Desert dunes."
        ))
        .is_ok());
    }

    #[test]
    fn english_and_japanese_corrections_stay_out_of_confirmed_facts() {
        let checkpoint = fixture_checkpoint(
            "- User: 砂漠は違う。moss stepping-stone terrain を使う.\n- User: the desert formula is wrong.",
        );
        assert!(checkpoint.contains("砂漠は違う"));
        assert!(checkpoint.contains("desert formula is wrong"));
        let facts = checkpoint
            .split("## User corrections and open disagreements")
            .next()
            .unwrap();
        assert!(!facts.contains("Code Desert dunes"));
        assert!(checkpoint.contains("## Agent conclusions (unverified)"));
        assert!(checkpoint.contains("Code Desert dunes"));
    }

    #[test]
    fn v1_envelope_promotes_into_v2_internal_history() {
        let encoded = encode_summary(&fixture_checkpoint("- older correction")).expect("v1");
        let history = normalize_compaction_history(&[
            json!({"type": "compaction", "encrypted_content": encoded}),
            user_message("continue"),
        ])
        .expect("normalize");
        assert_eq!(history.previous_generation, 1);
        assert!(history
            .previous_checkpoint
            .as_deref()
            .is_some_and(|text| text.contains("older correction")));
        assert_eq!(history.items[0]["content"][0]["text"], "continue");
    }

    #[test]
    fn expand_places_raw_user_correction_after_checkpoint() {
        let encoded = encode_compacted_context(&CompactedContext {
            checkpoint: fixture_checkpoint("- dunes were rejected"),
            retained: vec![user_message("砂漠は違う。moss stepping-stone terrain を使う")],
            generation: 2,
            selection: CompactionSelection::default(),
        })
        .expect("encode");
        let mut body = json!({
            "input": [
                {"type": "compaction", "encrypted_content": encoded},
                user_message("and keep the moss path")
            ]
        });
        expand_local_compactions(&mut body);
        assert_eq!(body["input"][0]["role"], "assistant");
        assert!(body["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("dunes were rejected"));
        assert_eq!(body["input"][1]["role"], "user");
        assert!(body["input"][1]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("砂漠は違う"));
        assert_eq!(body["input"][2]["content"][0]["text"], "and keep the moss path");
    }

    fn lossy_summarizer_checkpoint() -> String {
        "## User requirements and confirmed facts\n\
         - Build rolling ground from the approved reference.\n\n\
         ## User corrections and open disagreements\n\
         - none\n\n\
         ## Durable observations\n\
         - none\n\n\
         ## Agent conclusions (unverified)\n\
         - Reference terrain is Code Desert dunes.\n\n\
         ## Remaining work\n\
         - continue"
            .to_string()
    }

    #[test]
    fn prefix_user_text_is_preserved_without_correction_keywords() {
        let history = NormalizedHistory {
            previous_checkpoint: None,
            previous_generation: 0,
            items: vec![
                user_message("use moss stepping-stone terrain"),
                assistant_message("ok"),
                user_message(&format!("later turn {}", "x".repeat(80))),
                assistant_message(&format!("later reply {}", "y".repeat(80))),
            ],
        };
        let (context, _) = build_compacted_context(
            &history,
            lossy_summarizer_checkpoint(),
            &LocalCompactionSettings {
                envelope: crate::config::LocalCompactionEnvelope::V2,
                tail_token_limit: 40,
            },
        )
        .expect("compact");
        let in_checkpoint = context.checkpoint.contains("use moss stepping-stone terrain");
        let in_retained = context.retained.iter().any(|item| {
            message_text(item).is_some_and(|text| text.contains("use moss stepping-stone terrain"))
        });
        assert!(
            in_checkpoint,
            "evicted user text must land in the checkpoint, not only the tail: retained={} checkpoint={}",
            in_retained,
            context.checkpoint
        );
        let facts = context
            .checkpoint
            .split("## User corrections and open disagreements")
            .next()
            .unwrap();
        assert!(
            !facts.contains("use moss stepping-stone terrain"),
            "merge must not copy evicted user text into confirmed facts"
        );
    }

    #[test]
    fn repeated_compaction_keeps_one_correction_and_does_not_promote_dunes() {
        let settings = LocalCompactionSettings {
            envelope: crate::config::LocalCompactionEnvelope::V2,
            tail_token_limit: 400,
        };
        let correction = "砂漠は違う。moss stepping-stone terrain を使う";
        let mut history = NormalizedHistory {
            previous_checkpoint: None,
            previous_generation: 0,
            items: vec![
                user_message("Build rolling ground from the approved reference."),
                assistant_message("Reference terrain is Code Desert dunes."),
                user_message(correction),
                assistant_message("Understood, use moss stepping-stone terrain."),
            ],
        };
        let mut last_checkpoint = String::new();
        let mut last_encoded = String::new();
        for generation in 0..5 {
            let (context, _) = build_compacted_context(
                &history,
                lossy_summarizer_checkpoint(),
                &settings,
            )
            .expect("compact");
            last_checkpoint = context.checkpoint.clone();
            last_encoded = encode_compacted_context(&context).expect("encode");
            let in_checkpoint = last_checkpoint.contains(correction);
            let in_retained = context.retained.iter().any(|item| {
                message_text(item).is_some_and(|text| text.contains(correction))
            });
            assert!(
                in_checkpoint || in_retained,
                "generation {generation} dropped the raw correction"
            );
            // Push the original correction out of the retained tail so later
            // generations must keep it in the checkpoint only.
            let extra = (0..8)
                .flat_map(|index| {
                    vec![
                        user_message(&format!(
                            "continue inspection {generation}-{index} {}",
                            "detail ".repeat(80)
                        )),
                        assistant_message(&format!(
                            "inspected file {generation}-{index} {}",
                            "note ".repeat(80)
                        )),
                    ]
                })
                .collect::<Vec<_>>();
            history = normalize_compaction_history(
                &std::iter::once(json!({
                    "type": "compaction",
                    "encrypted_content": last_encoded
                }))
                .chain(extra.into_iter())
                .collect::<Vec<_>>(),
            )
            .expect("renormalize");
        }

        let mut expanded = json!({
            "input": [{"type": "compaction", "encrypted_content": last_encoded}]
        });
        expand_local_compactions(&mut expanded);
        let dump = expanded.to_string();
        assert!(dump.contains(correction) || last_checkpoint.contains(correction));
        let occurrences = dump.matches(correction).count();
        assert!(
            (1..=2).contains(&occurrences),
            "correction should not proliferate, found {occurrences}"
        );
        let facts = last_checkpoint
            .split("## User corrections and open disagreements")
            .next()
            .unwrap();
        // Merge copies raw user text into corrections. It does not rewrite a
        // summarizer that already placed dunes under confirmed facts.
        assert!(!facts.contains("Code Desert dunes"));
        assert!(last_checkpoint.contains("## Agent conclusions (unverified)"));
        assert!(last_checkpoint.contains("Reference terrain is Code Desert dunes."));
        assert!(last_checkpoint.contains(correction));
        assert_eq!(last_checkpoint.matches(correction).count(), 1);
        assert!(
            !history.items.iter().any(|item| {
                message_text(item).is_some_and(|text| text.contains(correction))
            }),
            "after five compactions the original correction should have left the retained tail"
        );
        assert_eq!(dump.matches(SUMMARY_PREFIX).count(), 1);
        assert!(!dump.contains("role\":\"developer"));
    }

    #[test]
    fn twenty_repeated_compactions_do_not_grow_linearly() {
        let settings = LocalCompactionSettings::default();
        let mut history = NormalizedHistory {
            previous_checkpoint: None,
            previous_generation: 0,
            items: vec![
                user_message("start"),
                assistant_message("working"),
                user_message("砂漠は違う"),
            ],
        };
        let mut previous_len: usize = 0;
        for generation in 1..=20 {
            let (context, _) = build_compacted_context(
                &history,
                fixture_checkpoint("- User: 砂漠は違う"),
                &settings,
            )
            .expect("compact");
            assert_eq!(context.generation, generation);
            let encoded = encode_compacted_context(&context).expect("encode");
            if generation > 3 {
                assert!(encoded.len() < previous_len.saturating_mul(2));
            }
            previous_len = encoded.len();
            history = normalize_compaction_history(&[
                json!({"type": "compaction", "encrypted_content": encoded}),
                user_message(&format!("continue {generation}")),
                assistant_message("ack"),
            ])
            .expect("normalize");
            assert_eq!(
                history
                    .items
                    .iter()
                    .filter(|item| {
                        message_text(item).is_some_and(|text| text.contains("砂漠は違う"))
                    })
                    .count(),
                usize::from(history.items.iter().any(|item| {
                    message_text(item).is_some_and(|text| text.contains("砂漠は違う"))
                }))
            );
        }
    }

    #[test]
    fn empty_prefix_recompact_does_not_duplicate_framing() {
        let settings = LocalCompactionSettings {
            envelope: crate::config::LocalCompactionEnvelope::V2,
            tail_token_limit: 20_000,
        };
        let history = NormalizedHistory {
            previous_checkpoint: Some(fixture_checkpoint("- already captured")),
            previous_generation: 4,
            items: vec![user_message("tiny follow-up")],
        };
        let (context, _) = build_compacted_context(
            &history,
            format!("{SUMMARY_PREFIX}\n{}", fixture_checkpoint("- already captured")),
            &settings,
        )
        .expect("compact");
        assert!(!context.checkpoint.contains(SUMMARY_PREFIX));
        assert_eq!(context.generation, 5);
    }
}
