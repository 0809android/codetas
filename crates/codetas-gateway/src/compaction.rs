use crate::config::LocalCompactionSettings;
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

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
                write!(
                    f,
                    "compaction summary is missing a required checkpoint heading"
                )
            }
            Self::DuplicateHeading => {
                write!(
                    f,
                    "compaction summary repeats a required checkpoint heading"
                )
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

/// Deterministic checkpoint used when local compaction cannot call a summarizer.
/// This keeps Codex's remote-compact recovery alive while a provider target is
/// cooling down, instead of failing the turn with HTTP 503.
pub(crate) fn cooldown_fallback_checkpoint() -> String {
    render_offline_checkpoint(&ExtractedProgress::default())
}

pub(crate) fn offline_checkpoint(history: &NormalizedHistory) -> String {
    let extracted = extract_offline_progress(history);
    if let Some(previous) = history.previous_checkpoint.as_deref() {
        if validate_checkpoint_summary(previous).is_ok()
            && !checkpoint_is_generic_cooldown_fallback(previous)
        {
            return merge_extracted_offline_progress(previous, &extracted);
        }
    }
    render_offline_checkpoint(&extracted)
}

pub(crate) fn build_offline_compacted_context(
    history: &NormalizedHistory,
    settings: &LocalCompactionSettings,
) -> Result<(CompactedContext, CompactionMetrics), String> {
    build_compacted_context_for_offline(history, offline_checkpoint(history), settings)
}

pub(crate) fn offline_recovery_has_progress(history: &NormalizedHistory) -> bool {
    let extracted = extract_offline_progress(history);
    if !extracted.requirements.is_empty() || !extracted.observations.is_empty() {
        return true;
    }
    history
        .previous_checkpoint
        .as_deref()
        .is_some_and(|previous| {
            validate_checkpoint_summary(previous).is_ok()
                && !checkpoint_is_generic_cooldown_fallback(previous)
        })
}

const GENERIC_COOLDOWN_FACT: &str =
    "Provider cooldown blocked a new summarizer call. Confirmed user facts stay in the retained tail below.";
const GENERIC_COOLDOWN_OBS: &str =
    "Local compaction continued without an upstream summarizer because the selected provider target is cooling down.";
const GENERIC_COOLDOWN_REMAINING: &str =
    "Resume from the retained recent turns. A later user message outranks this checkpoint if they disagree.";

#[derive(Clone, Debug, Default, PartialEq)]
struct ExtractedProgress {
    requirements: Vec<String>,
    observations: Vec<String>,
    conclusions: Vec<String>,
    remaining: Vec<String>,
}

fn checkpoint_is_generic_cooldown_fallback(checkpoint: &str) -> bool {
    checkpoint.contains(GENERIC_COOLDOWN_FACT) && checkpoint.contains(GENERIC_COOLDOWN_OBS)
}

fn render_offline_checkpoint(extracted: &ExtractedProgress) -> String {
    [
        "## User requirements and confirmed facts",
        &bullets_or(&extracted.requirements, GENERIC_COOLDOWN_FACT),
        "",
        "## User corrections and open disagreements",
        "- none",
        "",
        "## Durable observations",
        &offline_observation_bullets(&extracted.observations),
        "",
        "## Agent conclusions (unverified)",
        &bullets_or(&extracted.conclusions, "none"),
        "",
        "## Remaining work",
        &bullets_or(&extracted.remaining, GENERIC_COOLDOWN_REMAINING),
    ]
    .join("\n")
}

fn offline_observation_bullets(observations: &[String]) -> String {
    let mut lines = vec![format!("- {GENERIC_COOLDOWN_OBS}")];
    for observation in observations {
        lines.push(format!("- {observation}"));
    }
    lines.join("\n")
}

fn bullets_or(items: &[String], fallback: &str) -> String {
    if items.is_empty() {
        return format!("- {fallback}");
    }
    items
        .iter()
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn merge_extracted_offline_progress(previous: &str, extracted: &ExtractedProgress) -> String {
    if extracted.observations.is_empty() {
        return previous.to_string();
    }
    let mut next = append_unique_checkpoint_bullets(
        previous,
        "## Durable observations",
        &extracted.observations,
    );
    next = replace_generic_remaining_work(&next, &extracted.remaining);
    append_unique_checkpoint_bullets(&next, "## Remaining work", &extracted.remaining)
}

fn append_unique_checkpoint_bullets(checkpoint: &str, heading: &str, extras: &[String]) -> String {
    let Some(body) = checkpoint_section_body(checkpoint, heading) else {
        return checkpoint.to_string();
    };
    let mut extra = String::new();
    for item in extras {
        if body.contains(item) || extra.contains(item) || checkpoint.contains(item) {
            continue;
        }
        extra.push_str("\n- ");
        extra.push_str(item);
    }
    if extra.is_empty() {
        return checkpoint.to_string();
    }
    let start = checkpoint.find(heading).expect("heading present") + heading.len();
    let end = start + body.len();
    let mut merged = String::new();
    merged.push_str(&checkpoint[..end].trim_end());
    merged.push_str(&extra);
    merged.push('\n');
    merged.push_str(&checkpoint[end..]);
    merged
}

fn replace_generic_remaining_work(checkpoint: &str, remaining: &[String]) -> String {
    let Some(body) = checkpoint_section_body(checkpoint, "## Remaining work") else {
        return checkpoint.to_string();
    };
    if !body.contains(GENERIC_COOLDOWN_REMAINING) {
        return checkpoint.to_string();
    }
    let heading = "## Remaining work";
    let start = checkpoint.find(heading).expect("heading present") + heading.len();
    let end = start + body.len();
    let mut next = String::new();
    next.push_str(&checkpoint[..start]);
    next.push('\n');
    next.push_str(&bullets_or(remaining, GENERIC_COOLDOWN_REMAINING));
    next.push('\n');
    next.push_str(&checkpoint[end..]);
    next
}

fn extract_offline_progress(history: &NormalizedHistory) -> ExtractedProgress {
    let mut extracted = ExtractedProgress::default();
    for item in &history.items {
        if is_task_user_message(item) {
            if let Some(text) = clipped_message_text(item, 280) {
                push_unique(&mut extracted.requirements, text);
            }
        } else if is_assistant_message(item) {
            if let Some(text) = clipped_message_text(item, 280) {
                if !is_placeholder_assistant_text(&text)
                    && !is_synthetic_tool_observation_text(&text)
                {
                    push_unique(&mut extracted.conclusions, text);
                }
            }
        } else if let Some(paths) =
            tool_file_observations(item).or_else(|| inspect_file_observations(item))
        {
            for path in paths {
                push_unique(&mut extracted.observations, path);
            }
        }
    }
    if extracted.requirements.len() > 4 {
        extracted.requirements = extracted
            .requirements
            .split_off(extracted.requirements.len() - 4);
    }
    if extracted.conclusions.len() > 3 {
        extracted.conclusions = extracted
            .conclusions
            .split_off(extracted.conclusions.len() - 3);
    }
    if extracted.observations.len() > 8 {
        extracted.observations = extracted
            .observations
            .split_off(extracted.observations.len() - 8);
    }
    if !extracted.observations.is_empty() {
        extracted.remaining.push(
            "Continue from the written files and last user request. Do not restart inspection from scratch."
                .to_string(),
        );
    } else if extracted.requirements.last().is_some() {
        extracted
            .remaining
            .push("Resume the latest user request from the retained recent turns.".to_string());
    }
    extracted
}

fn push_unique(items: &mut Vec<String>, item: String) {
    if !items.iter().any(|existing| existing == &item) {
        items.push(item);
    }
}

fn clipped_message_text(item: &Value, max_chars: usize) -> Option<String> {
    let text = collapse_ws(&message_text(item)?);
    if text.is_empty() {
        return None;
    }
    Some(clip_chars(&text, max_chars))
}

fn collapse_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip_chars(text: &str, max_chars: usize) -> String {
    let mut clipped = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        clipped.push('…');
    }
    clipped
}

fn is_placeholder_assistant_text(text: &str) -> bool {
    matches!(
        text.trim(),
        "Still working…" | "Still working..." | "Still working."
    )
}

fn is_synthetic_tool_observation_text(text: &str) -> bool {
    text.trim_start().starts_with("[compacted tool files]")
}

fn is_task_user_message(item: &Value) -> bool {
    is_user_message(item) && !is_injected_control_user_message(item)
}

fn is_injected_control_user_message(item: &Value) -> bool {
    message_text(item).is_some_and(|text| injected_control_user_text(&text))
}

fn injected_control_user_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("<recommended_plugins>")
        || trimmed.starts_with("<environment_context>")
        || trimmed.starts_with("<app-context>")
        || trimmed.starts_with("<skills_instructions>")
        || trimmed.starts_with("<collaboration_mode>")
        || trimmed.starts_with("<multi_agent_mode>")
        || trimmed.contains("# AGENTS.md instructions")
        || (trimmed.contains("<INSTRUCTIONS>") && trimmed.contains("Verification Policy"))
}

fn tool_file_observations(item: &Value) -> Option<Vec<String>> {
    let kind = item.get("type").and_then(Value::as_str)?;
    let name = item.get("name").and_then(Value::as_str).unwrap_or("");
    let payload = tool_payload_text(item);
    match kind {
        "custom_tool_call" | "function_call"
            if name == "apply_patch" || payload.contains("*** Begin Patch") =>
        {
            let paths = extract_patch_paths(&payload);
            (!paths.is_empty()).then_some(paths)
        }
        _ => None,
    }
}

fn inspect_file_observations(item: &Value) -> Option<Vec<String>> {
    let kind = item.get("type").and_then(Value::as_str)?;
    if !matches!(kind, "custom_tool_call" | "function_call") {
        return None;
    }
    let name = item.get("name").and_then(Value::as_str).unwrap_or("");
    if !matches!(name, "exec_command" | "exec" | "shell" | "bash") {
        return None;
    }
    let payload = tool_payload_text(item);
    let paths = extract_inspected_paths(&payload);
    (!paths.is_empty()).then_some(paths)
}

/// Collect a searchable payload from tool `arguments` / `input`.
///
/// Providers may retain args as a JSON string, object, or array. Offline and
/// cooldown checkpoints must still see `cmd` / patch text so written and
/// inspected paths are not dropped from history.
fn tool_payload_text(item: &Value) -> String {
    let mut parts = Vec::new();
    for key in ["arguments", "input"] {
        if let Some(value) = item.get(key) {
            push_tool_payload_value(&mut parts, value);
        }
    }
    parts.join("\n")
}

fn push_tool_payload_value(parts: &mut Vec<String>, value: &Value) {
    match value {
        Value::String(text) => {
            if !text.is_empty() {
                parts.push(text.clone());
            }
        }
        Value::Object(map) => {
            // Prefer an explicit shell command first so path extraction can use
            // the command line without tokenizing decoy strings from other fields.
            if let Some(cmd) = map
                .get("cmd")
                .or_else(|| map.get("command"))
                .and_then(Value::as_str)
            {
                if !cmd.is_empty() {
                    parts.push(cmd.to_owned());
                }
            }
            for field in map.values() {
                match field {
                    Value::String(text) if !text.is_empty() => parts.push(text.clone()),
                    Value::Object(_) | Value::Array(_) => parts.push(field.to_string()),
                    _ => {}
                }
            }
            // Keep a full object rendering so nested markers remain visible even
            // when the interesting text is not a top-level string field.
            parts.push(value.to_string());
        }
        Value::Array(_) => parts.push(value.to_string()),
        _ => {}
    }
}

fn extract_inspected_paths(arguments: &str) -> Vec<String> {
    // Prefer structured tool args (`{"cmd":"..."}` / `{"command":"..."}`).
    // Tokenizing the raw JSON string leaves punctuation glued to paths.
    // `tool_payload_text` may prefix the cmd string before a stringified object;
    // prefer that leading command line over decoy paths in other fields.
    let command = command_text_from_tool_payload(arguments);
    let mut paths = Vec::new();
    for token in command.split_whitespace() {
        let token = token.trim_matches(|ch| matches!(ch, '"' | '\'' | '`' | ',' | ';' | ')' | '('));
        if looks_like_repo_path(token) {
            push_unique(&mut paths, format!("inspected {token}"));
        }
    }
    paths
}

fn command_text_from_tool_payload(arguments: &str) -> String {
    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(map)) => map
            .get("cmd")
            .or_else(|| map.get("command"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| arguments.to_owned()),
        Ok(_) => arguments.to_owned(),
        Err(_) => {
            let mut non_empty = arguments
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty());
            match non_empty.next() {
                Some(first) if !first.starts_with('{') && !first.starts_with('[') => {
                    first.to_owned()
                }
                Some(first) => match serde_json::from_str::<Value>(first) {
                    Ok(Value::Object(map)) => map
                        .get("cmd")
                        .or_else(|| map.get("command"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| arguments.to_owned()),
                    _ => arguments.to_owned(),
                },
                None => arguments.to_owned(),
            }
        }
    }
}

fn looks_like_repo_path(token: &str) -> bool {
    if token.contains("://") || token.starts_with('-') {
        return false;
    }
    let has_sep = token.contains('/') || token.contains('\\');
    (has_sep || has_source_ext(token))
        && token.chars().all(|ch| {
            ch.is_ascii_alphanumeric() || matches!(ch, '/' | '\\' | '.' | '_' | '-' | '@')
        })
}

fn has_source_ext(token: &str) -> bool {
    matches!(
        token.rsplit('.').next().unwrap_or(""),
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "py"
            | "md"
            | "html"
            | "css"
            | "json"
            | "toml"
            | "astro"
            | "vue"
            | "svelte"
    )
}

fn extract_patch_paths(arguments: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in arguments.lines() {
        let line = line.trim();
        for prefix in ["*** Add File:", "*** Update File:", "*** Delete File:"] {
            if let Some(rest) = line.strip_prefix(prefix) {
                let path = rest.trim();
                if !path.is_empty() {
                    push_unique(&mut paths, format!("{prefix} {path}"));
                }
            }
        }
    }
    paths
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
            Self::NotLocal => {
                "translated providers can consume only CODETAS compaction envelopes".into()
            }
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
        if !matches!(
            item_type,
            Some("compaction" | "compaction_summary" | "context_compaction")
        ) {
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
            format!(
                "invalid local compaction envelope: {}",
                error.as_validation_message()
            )
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
    Ok(NormalizedHistory {
        previous_checkpoint,
        previous_generation,
        items: normalized,
    })
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
    fn starts_with_user(&self) -> bool {
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
        if last_user_tokens > tail_token_limit
            && groups.last() == Some(last_user)
            && last_user.items.len() == 1
        {
            return Err("last user message exceeds the retained tail token limit".into());
        }
    }
    let mut retained_from = groups.len();
    let mut retained_tokens: u64 = 0;
    for index in (0..groups.len()).rev() {
        let group_tokens = estimate_input_items_tokens(&groups[index].items);
        if retained_from < groups.len()
            && retained_tokens.saturating_add(group_tokens) > tail_token_limit
        {
            break;
        }
        if retained_from == groups.len() && group_tokens > tail_token_limit {
            // Oversized latest complete turn goes to the checkpoint instead of
            // being sliced mid-message. Keep any later smaller complete turns.
            // Offline recovery shrinks this group separately so a live
            // summarizer still sees the original payloads.
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
    let mut split = HistorySplit {
        prefix,
        tail: tail.clone(),
        selection: CompactionSelection {
            target_tokens: tail_token_limit,
            estimated_tokens: estimate_input_items_tokens(&tail),
            truncated: retained_from > 0,
        },
        retained_turns: groups.len().saturating_sub(retained_from),
    };
    pin_last_question_in_tail(&mut split, items);
    Ok(split)
}

fn recover_split_for_offline(
    items: &[Value],
    tail_token_limit: u64,
) -> Result<HistorySplit, String> {
    let mut split = split_prefix_and_tail(items, tail_token_limit)?;
    let groups = split_interaction_groups(items);
    let Some(latest) = groups.last() else {
        return Ok(split);
    };
    if estimate_input_items_tokens(&latest.items) <= tail_token_limit {
        return Ok(split);
    }
    let recovered = recover_oversized_latest_group(&latest.items, tail_token_limit);
    if recovered.is_empty() {
        return Ok(split);
    }
    let recovered_ids = tool_ids(&recovered);
    // Offline retained items cannot share call IDs with the prefix. The live
    // summarizer path still uses split_prefix_and_tail and keeps originals.
    split.prefix.retain(|item| match call_id(item) {
        Some(id) => !recovered_ids.contains(id),
        None => true,
    });
    split.tail.retain(|item| match call_id(item) {
        Some(id) => !recovered_ids.contains(id),
        None => !recovered.iter().any(|kept| kept == item),
    });
    split.tail.extend(recovered);
    pin_last_meaningful_progress_in_tail(&mut split, items);
    split.selection.estimated_tokens = estimate_input_items_tokens(&split.tail);
    split.selection.truncated = true;
    if split.retained_turns == 0 && !split.tail.is_empty() {
        split.retained_turns = 1;
    }
    let prefix_ids = tool_ids(&split.prefix);
    let tail_ids = tool_ids(&split.tail);
    if prefix_ids.intersection(&tail_ids).next().is_some() {
        return Err("compaction split a tool call from its result".into());
    }
    Ok(split)
}

/// A numbered user reply is useless if the previous assistant question was
/// evicted with an oversized last turn. Always keep that pair in the tail.
fn pin_last_question_in_tail(split: &mut HistorySplit, original: &[Value]) {
    let last_user = original
        .iter()
        .rev()
        .find(|item| is_task_user_message(item))
        .cloned();
    let last_assistant = original
        .iter()
        .rev()
        .find(|item| {
            is_assistant_message(item)
                && !is_placeholder_assistant_item(item)
                && !is_synthetic_tool_observation_item(item)
        })
        .or_else(|| {
            original.iter().rev().find(|item| {
                is_assistant_message(item) && !is_synthetic_tool_observation_item(item)
            })
        })
        .cloned();
    let mut pinned = Vec::new();
    if let Some(user) = last_user {
        if !split.tail.iter().any(|item| item == &user) {
            pinned.push(user);
        }
    }
    if let Some(assistant) = last_assistant {
        if !split.tail.iter().any(|item| item == &assistant) {
            pinned.push(assistant);
        }
    }
    if pinned.is_empty() {
        return;
    }
    split
        .prefix
        .retain(|item| !pinned.iter().any(|pinned| pinned == item));
    for item in &pinned {
        split.tail.retain(|existing| existing != item);
    }
    split.tail.extend(pinned);
    split.selection.estimated_tokens = estimate_input_items_tokens(&split.tail);
    split.selection.truncated = !split.prefix.is_empty();
    if split.retained_turns == 0 && !split.tail.is_empty() {
        split.retained_turns = 1;
    }
}

fn pin_last_meaningful_progress_in_tail(split: &mut HistorySplit, original: &[Value]) {
    pin_last_question_in_tail(split, original);
    let Some(write) = original
        .iter()
        .rev()
        .find(|item| {
            tool_file_observations(item).is_some() || inspect_file_observations(item).is_some()
        })
        .cloned()
    else {
        ensure_synthetic_observations_before_last_real_assistant(split);
        split.selection.estimated_tokens = estimate_input_items_tokens(&split.tail);
        return;
    };
    let Some(paths) = tool_file_observations(&write).or_else(|| inspect_file_observations(&write))
    else {
        ensure_synthetic_observations_before_last_real_assistant(split);
        split.selection.estimated_tokens = estimate_input_items_tokens(&split.tail);
        return;
    };
    if !split
        .tail
        .iter()
        .any(|item| item_mentions_paths(item, &paths))
    {
        insert_tool_observation_before_last_real_assistant(split, tool_observation_message(&paths));
    }
    ensure_synthetic_observations_before_last_real_assistant(split);
    split.selection.estimated_tokens = estimate_input_items_tokens(&split.tail);
}

/// Synthetic tool-file observations must never become the newest assistant.
/// Keep the real last assistant question as the tail tip so later pin/recovery
/// walks do not treat the file list as the last question.
fn insert_tool_observation_before_last_real_assistant(
    split: &mut HistorySplit,
    observation: Value,
) {
    let insert_at = split
        .tail
        .iter()
        .rposition(|item| {
            is_assistant_message(item)
                && !is_placeholder_assistant_item(item)
                && !is_synthetic_tool_observation_item(item)
        })
        .unwrap_or(split.tail.len());
    split.tail.insert(insert_at, observation);
}

fn ensure_synthetic_observations_before_last_real_assistant(split: &mut HistorySplit) {
    let Some(real_at) = split.tail.iter().rposition(|item| {
        is_assistant_message(item)
            && !is_placeholder_assistant_item(item)
            && !is_synthetic_tool_observation_item(item)
    }) else {
        return;
    };
    let mut trailing_synthetics = Vec::new();
    let mut kept = Vec::with_capacity(split.tail.len());
    for (index, item) in split.tail.drain(..).enumerate() {
        if index > real_at && is_synthetic_tool_observation_item(&item) {
            trailing_synthetics.push(item);
        } else {
            kept.push(item);
        }
    }
    if trailing_synthetics.is_empty() {
        split.tail = kept;
        return;
    }
    let insert_at = kept
        .iter()
        .rposition(|item| {
            is_assistant_message(item)
                && !is_placeholder_assistant_item(item)
                && !is_synthetic_tool_observation_item(item)
        })
        .unwrap_or(kept.len());
    for (offset, observation) in trailing_synthetics.into_iter().enumerate() {
        kept.insert(insert_at + offset, observation);
    }
    split.tail = kept;
}

fn item_mentions_paths(item: &Value, paths: &[String]) -> bool {
    if tool_file_observations(item).as_deref() == Some(paths)
        || inspect_file_observations(item).as_deref() == Some(paths)
    {
        return true;
    }
    // Prefer structured observation helpers and message text over whole-item
    // JSON serialization so a raw path already in the tail is recognized even
    // when the observation list uses the "inspected {path}" form.
    let mut haystacks = Vec::new();
    if let Some(observed) = tool_file_observations(item) {
        haystacks.extend(observed);
    }
    if let Some(observed) = inspect_file_observations(item) {
        haystacks.extend(observed);
    }
    if let Some(text) = message_text(item) {
        haystacks.push(text);
    }
    let payload = tool_payload_text(item);
    if !payload.is_empty() {
        haystacks.push(payload);
    }
    if let Some(output) = item.get("output").and_then(Value::as_str) {
        haystacks.push(output.to_string());
    }
    paths.iter().any(|path| {
        let raw = path.strip_prefix("inspected ").unwrap_or(path.as_str());
        haystacks.iter().any(|haystack| {
            haystack.contains(path.as_str()) || (!raw.is_empty() && haystack.contains(raw))
        })
    })
}

fn recover_oversized_latest_group(items: &[Value], tail_token_limit: u64) -> Vec<Value> {
    let recovered = shrink_complete_history_for_recovery(items);
    if estimate_input_items_tokens(&recovered) <= tail_token_limit {
        return recovered;
    }
    let user = items
        .iter()
        .rev()
        .find(|item| is_task_user_message(item))
        .map(shrink_history_item_for_recovery);
    let observations = items
        .iter()
        .filter_map(|item| {
            tool_file_observations(item)
                .or_else(|| inspect_file_observations(item))
                .map(|paths| tool_observation_message(&paths))
        })
        .collect::<Vec<_>>();
    let assistant = items
        .iter()
        .rev()
        .find(|item| {
            is_assistant_message(item)
                && !is_placeholder_assistant_item(item)
                && !is_synthetic_tool_observation_item(item)
        })
        .or_else(|| {
            items.iter().rev().find(|item| {
                is_assistant_message(item) && !is_synthetic_tool_observation_item(item)
            })
        })
        .map(shrink_history_item_for_recovery);

    let mut candidates = Vec::new();
    if !observations.is_empty() {
        candidates.push(observations.clone());
        if let Some(last) = observations.last().cloned() {
            candidates.push(vec![last]);
        }
    }
    candidates.push(Vec::new());
    for observations in candidates {
        let mut essential = Vec::new();
        if let Some(user) = user.clone() {
            essential.push(user);
        }
        essential.extend(observations);
        if let Some(assistant) = assistant.clone() {
            essential.push(assistant);
        }
        if !essential.is_empty() && estimate_input_items_tokens(&essential) <= tail_token_limit {
            return essential;
        }
    }
    Vec::new()
}

fn shrink_complete_history_for_recovery(items: &[Value]) -> Vec<Value> {
    let mut keep = vec![false; items.len()];
    let mut open_calls = std::collections::HashMap::<String, usize>::new();
    for (index, item) in items.iter().enumerate() {
        if is_tool_call(item) {
            if let Some(id) = call_id(item).filter(|value| !value.is_empty()) {
                open_calls.entry(id.to_string()).or_insert(index);
            }
            continue;
        }
        if is_tool_result(item) {
            if let Some(id) = call_id(item).filter(|value| !value.is_empty()) {
                if let Some(call_index) = open_calls.remove(id) {
                    keep[call_index] = true;
                    keep[index] = true;
                }
            }
            continue;
        }
        keep[index] = true;
    }
    items
        .iter()
        .zip(keep)
        .filter_map(|(item, keep)| keep.then(|| shrink_history_item_for_recovery(item)))
        .collect()
}

fn is_placeholder_assistant_item(item: &Value) -> bool {
    message_text(item).is_some_and(|text| is_placeholder_assistant_text(&text))
}

fn is_synthetic_tool_observation_item(item: &Value) -> bool {
    message_text(item).is_some_and(|text| is_synthetic_tool_observation_text(&text))
}

fn shrink_history_item_for_recovery(item: &Value) -> Value {
    if is_tool_call(item) {
        if let Some(paths) =
            tool_file_observations(item).or_else(|| inspect_file_observations(item))
        {
            return summarize_tool_call(item, &paths);
        }
        if item.get("type").and_then(Value::as_str) == Some("function_call") {
            return compact_function_call_arguments(item);
        }
        let field = if item.get("input").is_some() {
            "input"
        } else {
            "arguments"
        };
        return clip_tool_payload(item, field, 400);
    }
    if is_tool_result(item) {
        return summarize_tool_result(item);
    }
    item.clone()
}

fn tool_observation_message(paths: &[String]) -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": format!("[compacted tool files]\n{}", paths.join("\n"))
        }]
    })
}

fn summarize_tool_call(item: &Value, paths: &[String]) -> Value {
    let mut next = item.clone();
    if let Some(object) = next.as_object_mut() {
        let summary = format!("[compacted tool files]\n{}", paths.join("\n"));
        if item.get("type").and_then(Value::as_str) == Some("function_call") {
            object.insert(
                "arguments".into(),
                Value::String(json!({"codetas_compacted_files": paths}).to_string()),
            );
        } else if object.contains_key("input") {
            object.insert("input".into(), Value::String(summary));
        } else if object.contains_key("arguments") {
            object.insert("arguments".into(), Value::String(summary));
        }
    }
    next
}

fn compact_function_call_arguments(item: &Value) -> Value {
    let mut next = item.clone();
    let Some(object) = next.as_object_mut() else {
        return next;
    };
    let raw = object
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or("");
    if raw.chars().count() <= 400 && serde_json::from_str::<Value>(raw).is_ok() {
        return next;
    }
    let summary = match serde_json::from_str::<Value>(raw) {
        Ok(value) => clip_chars(&collapse_ws(&value.to_string()), 240),
        Err(_) => clip_chars(&collapse_ws(raw), 240),
    };
    object.insert(
        "arguments".into(),
        Value::String(
            json!({
                "codetas_compacted": true,
                "summary": summary
            })
            .to_string(),
        ),
    );
    next
}

fn summarize_tool_result(item: &Value) -> Value {
    clip_tool_payload(item, "output", 240)
}

fn clip_tool_payload(item: &Value, field: &str, max_chars: usize) -> Value {
    let mut next = item.clone();
    let Some(object) = next.as_object_mut() else {
        return next;
    };
    let Some(Value::String(payload)) = object.get(field).cloned() else {
        return next;
    };
    object.insert(
        field.into(),
        Value::String(clip_chars(&collapse_ws(&payload), max_chars)),
    );
    next
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
    let repaired = repaired
        .ok_or_else(|| validate_checkpoint_summary(summary).expect_err("invalid summary"))?;
    validate_checkpoint_summary(repaired)?;
    Ok((repaired.trim().to_string(), true))
}

fn checkpoint_section_body<'a>(checkpoint: &'a str, heading: &str) -> Option<&'a str> {
    let start = checkpoint.find(heading)? + heading.len();
    let next = REQUIRED_CHECKPOINT_HEADINGS
        .iter()
        .filter(|candidate| **candidate != heading)
        .filter_map(|candidate| {
            checkpoint[start..]
                .find(candidate)
                .map(|index| start + index)
        })
        .min()
        .unwrap_or(checkpoint.len());
    Some(&checkpoint[start..next])
}

fn collect_preserved_user_texts(
    previous_checkpoint: Option<&str>,
    prefix: &[Value],
) -> Vec<String> {
    let mut texts = Vec::new();
    if let Some(previous) = previous_checkpoint {
        if let Some(body) =
            checkpoint_section_body(previous, "## User corrections and open disagreements")
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
        if is_task_user_message(item) {
            if let Some(text) = message_text(item) {
                let trimmed = text.trim();
                if !trimmed.is_empty() && !injected_control_user_text(trimmed) {
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
        .filter_map(|candidate| {
            checkpoint[after..]
                .find(candidate)
                .map(|index| after + index)
        })
        .min()
        .unwrap_or(checkpoint.len());
    let existing = checkpoint[after..next].to_string();
    let mut extra = String::new();
    for correction in corrections {
        let trimmed = correction.trim();
        if trimmed.is_empty() || checkpoint.contains(trimmed) || extra.contains(trimmed) {
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
    build_compacted_context_from_split(
        history,
        split_prefix_and_tail(&history.items, settings.tail_token_limit())?,
        checkpoint,
        settings,
        repaired,
    )
}

fn build_compacted_context_for_offline(
    history: &NormalizedHistory,
    checkpoint: String,
    settings: &LocalCompactionSettings,
) -> Result<(CompactedContext, CompactionMetrics), String> {
    build_compacted_context_from_split(
        history,
        recover_split_for_offline(&history.items, settings.tail_token_limit())?,
        checkpoint,
        settings,
        false,
    )
}

fn build_compacted_context_from_split(
    history: &NormalizedHistory,
    split: HistorySplit,
    checkpoint: String,
    settings: &LocalCompactionSettings,
    repaired: bool,
) -> Result<(CompactedContext, CompactionMetrics), String> {
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

pub(crate) fn native_compaction_item(
    context: &CompactedContext,
    settings: &LocalCompactionSettings,
) -> Result<Value, String> {
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
        assert!(body["input"][0]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("keep this summary")
                && text.contains("outranks the checkpoint")));
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
        assert!(model_summary_is_usable(
            "隠れ敵の撃破描画と被ダメ位置のずれ原因を、関連関数から特定します。\n<file_end><|eos|>"
        )
        .is_err());
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

    #[test]
    fn offline_checkpoint_reuses_valid_previous_and_falls_back_when_missing() {
        let previous = fixture_checkpoint("- User: keep the moss path.");
        let with_previous = NormalizedHistory {
            previous_checkpoint: Some(previous.clone()),
            previous_generation: 3,
            items: vec![user_message("continue")],
        };
        assert_eq!(offline_checkpoint(&with_previous), previous);

        let without_previous = NormalizedHistory {
            previous_checkpoint: None,
            previous_generation: 0,
            items: vec![user_message("continue")],
        };
        let fallback = offline_checkpoint(&without_previous);
        assert!(validate_checkpoint_summary(&fallback).is_ok());
        assert!(fallback.contains("cooling down"));

        let (context, _) =
            build_offline_compacted_context(&without_previous, &LocalCompactionSettings::default())
                .expect("offline context");
        assert_eq!(context.checkpoint, fallback);
        assert_eq!(context.generation, 1);
    }

    #[test]
    fn offline_checkpoint_keeps_written_files_instead_of_a_generic_cooldown_stub() {
        let history = NormalizedHistory {
            previous_checkpoint: None,
            previous_generation: 0,
            items: vec![
                user_message("<recommended_plugins>\n- Airtable"),
                user_message("# AGENTS.md instructions for /tmp/app\n\n<INSTRUCTIONS>\n## Verification Policy"),
                user_message("トップページを情報はそのままで3案つくって"),
                assistant_message("3案を単一HTMLで作ります"),
                json!({
                    "type": "custom_tool_call",
                    "call_id": "call_a",
                    "name": "apply_patch",
                    "arguments": "*** Begin Patch\n*** Add File: docs/top-redesign-proto/proto-a-pop-circuit.html\n+<html></html>\n*** End Patch\n"
                }),
                json!({
                    "type": "custom_tool_call_output",
                    "call_id": "call_a",
                    "output": "Success. Updated the following files"
                }),
                json!({
                    "type": "function_call",
                    "call_id": "call_c",
                    "name": "apply_patch",
                    "arguments": "*** Begin Patch\n*** Add File: docs/top-redesign-proto/proto-c-kinoworld-console.html\n+<html></html>\n*** End Patch\n"
                }),
                json!({
                    "type": "function_call_output",
                    "call_id": "call_c",
                    "output": "Success"
                }),
                assistant_message("Still working…"),
            ],
        };
        let checkpoint = offline_checkpoint(&history);
        assert!(checkpoint.contains("トップページを情報はそのままで3案つくって"));
        assert!(checkpoint.contains("docs/top-redesign-proto/proto-a-pop-circuit.html"));
        assert!(checkpoint.contains("docs/top-redesign-proto/proto-c-kinoworld-console.html"));
        assert!(checkpoint.contains("Do not restart inspection from scratch"));
        assert!(!checkpoint.contains("<recommended_plugins>"));
        assert!(!checkpoint.contains("AGENTS.md instructions"));
        assert!(!checkpoint.contains("Still working"));
        let facts = checkpoint
            .split("## User corrections and open disagreements")
            .next()
            .unwrap();
        assert!(facts.contains("3案つくって"));
    }

    #[test]
    fn normalize_and_merge_ignore_injected_control_user_wrappers() {
        let items = vec![
            user_message("<recommended_plugins>\n- Airtable"),
            user_message("# AGENTS.md instructions for /tmp/app\n\n<INSTRUCTIONS>\n## Verification Policy\nDo not run local test"),
            user_message("keep the moss path"),
            assistant_message("ok"),
        ];
        let history = normalize_compaction_history(&items).expect("normalize");
        assert_eq!(history.items.len(), 2);
        assert!(history.items.iter().all(|item| {
            message_text(item).is_none_or(|text| {
                !text.contains("<recommended_plugins>") && !text.contains("AGENTS.md")
            })
        }));

        let (context, _) = build_compacted_context(
            &history,
            fixture_checkpoint("- keep the moss path"),
            &LocalCompactionSettings::default(),
        )
        .expect("compact");
        assert!(context.checkpoint.contains("keep the moss path"));
        assert!(!context.checkpoint.contains("<recommended_plugins>"));
        assert!(!context.checkpoint.contains("Verification Policy"));
    }

    #[test]
    fn generic_previous_cooldown_checkpoint_is_replaced_with_extracted_progress() {
        let previous = cooldown_fallback_checkpoint();
        let history = NormalizedHistory {
            previous_checkpoint: Some(previous.clone()),
            previous_generation: 1,
            items: vec![
                user_message("write three homepage prototypes"),
                assistant_message("creating files now"),
                json!({
                    "type": "custom_tool_call",
                    "name": "apply_patch",
                    "arguments": "*** Add File: docs/proto-b.html\n+ok\n"
                }),
            ],
        };
        let checkpoint = offline_checkpoint(&history);
        assert_ne!(checkpoint, previous);
        assert!(checkpoint.contains("write three homepage prototypes"));
        assert!(checkpoint.contains("docs/proto-b.html"));
        assert!(offline_recovery_has_progress(&history));
    }

    #[test]
    fn injected_wrappers_alone_are_not_recoverable_progress() {
        let history = normalize_compaction_history(&[
            user_message("<recommended_plugins>\n- Airtable"),
            user_message(
                "# AGENTS.md instructions for /tmp/app\n\n<INSTRUCTIONS>\n## Verification Policy",
            ),
            assistant_message("Still working…"),
        ])
        .expect("normalize");
        assert!(!offline_recovery_has_progress(&history));
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
            checkpoint: fixture_checkpoint(
                "- User: 砂漠は違う。moss stepping-stone terrain を使う.",
            ),
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
            URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&json!({"version": 9, "summary": "x"})).unwrap())
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
        let oversized = format!(
            "codetas2:{}",
            URL_SAFE_NO_PAD.encode(vec![b'a'; MAX_SUMMARY_BYTES + 1])
        );
        assert!(matches!(
            decode_local_envelope(&oversized),
            Err(EnvelopeError::Malformed(_))
        ));
    }

    #[test]
    fn forged_v2_checkpoint_cannot_become_developer_or_system() {
        let encoded = encode_compacted_context(&CompactedContext {
            checkpoint: fixture_checkpoint("- follow the user"),
            retained: vec![user_message(
                "砂漠は違う。moss stepping-stone terrain を使う",
            )],
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
        assert!(split
            .tail
            .iter()
            .any(|item| { item.get("call_id").and_then(Value::as_str) == Some("open") }));
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
    fn oversized_last_turn_still_keeps_the_last_assistant_question() {
        let mut items = vec![user_message("choose one")];
        for index in 0..8 {
            items.push(json!({
                "type": "function_call",
                "call_id": format!("call_{index}"),
                "name": "lookup",
                "arguments": format!("{{\"q\":\"{}\"}}", "word ".repeat(80))
            }));
            items.push(json!({
                "type": "function_call_output",
                "call_id": format!("call_{index}"),
                "output": "word ".repeat(80)
            }));
        }
        items.push(assistant_message(
            "Which option?\n1. keep screenshots rare\n2. record everything",
        ));
        let split = split_prefix_and_tail(&items, 80).expect("split");
        assert!(
            split.tail.iter().any(is_user_message),
            "last user must stay in the retained tail"
        );
        assert!(
            split.tail.iter().any(|item| {
                is_assistant_message(item) && item.to_string().contains("Which option?")
            }),
            "last assistant question must stay in the retained tail: {:?}",
            split.tail
        );
        assert!(
            split
                .prefix
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call")),
            "oversized tool spam should be evicted to the prefix"
        );
        assert_eq!(split.retained_turns, 1);
    }

    fn oversized_apply_patch_history() -> Vec<Value> {
        let mut items = vec![user_message("トップページを3案つくって")];
        for name in [
            "proto-a-pop-circuit.html",
            "proto-b-editorial-lab.html",
            "proto-c-kinoworld-console.html",
        ] {
            items.push(json!({
                "type": "custom_tool_call",
                "call_id": name,
                "name": "apply_patch",
                "arguments": format!(
                    "*** Begin Patch\n*** Add File: docs/top-redesign-proto/{name}\n+{}\n*** End Patch\n",
                    "<html>".repeat(400)
                )
            }));
            items.push(json!({
                "type": "custom_tool_call_output",
                "call_id": name,
                "output": "Success. Updated the following files"
            }));
        }
        items.push(assistant_message(
            "ツール実行が上限に達したので3案を直接作ります。",
        ));
        items
    }

    #[test]
    fn live_split_keeps_oversized_apply_patch_payloads_for_the_summarizer() {
        let items = oversized_apply_patch_history();
        let split = split_prefix_and_tail(&items, 80).expect("split");
        assert!(
            split
                .prefix
                .iter()
                .any(|item| item.to_string().contains("<html><html><html>")),
            "live summarizer prefix must keep the original apply_patch payload"
        );
        assert!(
            split.tail.iter().any(is_user_message),
            "live tail must still keep the last user request"
        );
    }

    #[test]
    fn oversized_apply_patch_turn_is_recovered_into_the_offline_tail() {
        let items = oversized_apply_patch_history();
        let split = recover_split_for_offline(&items, 80).expect("recover");
        assert!(
            split.tail.iter().any(is_user_message),
            "recovered tail must keep the last user request"
        );
        assert!(
            split
                .tail
                .iter()
                .any(|item| item.to_string().contains("proto-c-kinoworld-console.html")),
            "recovered tail must keep the written file path: {:?}",
            split.tail
        );
        assert!(
            !split
                .tail
                .iter()
                .any(|item| item.to_string().contains("<html><html><html>")),
            "recovered tail must not keep the raw oversized HTML payload"
        );
        assert!(
            !split
                .tail
                .iter()
                .any(|item| is_tool_call(item) || is_tool_result(item)),
            "tight offline fallback must represent writes as ordinary messages: {:?}",
            split.tail
        );
        validate_retained_items(&split.tail).expect("tight fallback remains valid history");
        assert!(split.selection.truncated);
    }

    #[test]
    fn oversized_recovery_keeps_only_balanced_tool_pairs() {
        let items = vec![
            user_message("inspect the configuration"),
            json!({
                "type": "function_call",
                "call_id": "call_config",
                "name": "lookup",
                "arguments": json!({"query": "word ".repeat(800)}).to_string()
            }),
            json!({
                "type": "function_call_output",
                "call_id": "call_config",
                "output": "result ".repeat(800)
            }),
            json!({
                "type": "function_call",
                "call_id": "call_unfinished",
                "name": "lookup",
                "arguments": "{}"
            }),
            assistant_message("The completed configuration lookup is available."),
        ];
        let recovered = recover_oversized_latest_group(&items, 400);
        validate_retained_items(&recovered).expect("recovered history must balance tool pairs");
        assert!(recovered
            .iter()
            .any(|item| call_id(item) == Some("call_config")));
        assert!(!recovered
            .iter()
            .any(|item| call_id(item) == Some("call_unfinished")));
    }

    #[test]
    fn summarized_function_call_arguments_remain_valid_json() {
        let item = json!({
            "type": "function_call",
            "call_id": "call_read",
            "name": "exec_command",
            "arguments": format!(
                "{{\"cmd\":\"sed -n 1,200p src/main.rs\",\"note\":\"{}\"}}",
                "word ".repeat(300)
            )
        });
        let summarized = shrink_history_item_for_recovery(&item);
        let arguments = summarized["arguments"].as_str().expect("arguments string");
        serde_json::from_str::<Value>(arguments).expect("summarized arguments are valid JSON");
        assert!(arguments.contains("src/main.rs"));
    }

    #[test]
    fn synthetic_tool_observation_never_steals_last_assistant_question() {
        let real_question = assistant_message("Which layout should we keep?");
        let observation = tool_observation_message(&["docs/a.html".to_string()]);
        let items = vec![
            user_message("build three layouts"),
            json!({
                "type": "custom_tool_call",
                "call_id": "call_write",
                "name": "apply_patch",
                "arguments": "*** Begin Patch\n*** Add File: docs/a.html\n+ok\n*** End Patch\n"
            }),
            json!({
                "type": "custom_tool_call_output",
                "call_id": "call_write",
                "output": "Success"
            }),
            real_question.clone(),
        ];
        let mut split = HistorySplit {
            prefix: Vec::new(),
            tail: vec![user_message("build three layouts"), real_question.clone()],
            selection: CompactionSelection {
                target_tokens: 1_000,
                estimated_tokens: 20,
                truncated: true,
            },
            retained_turns: 1,
        };
        pin_last_meaningful_progress_in_tail(&mut split, &items);
        let last_assistant = split
            .tail
            .iter()
            .rev()
            .find(|item| is_assistant_message(item))
            .expect("assistant in tail");
        assert_eq!(last_assistant, &real_question);
        assert!(
            split
                .tail
                .iter()
                .any(|item| is_synthetic_tool_observation_item(item)),
            "synthetic observation should still be retained: {:?}",
            split.tail
        );
        assert!(
            !is_synthetic_tool_observation_item(last_assistant),
            "newest assistant must remain the real question"
        );

        // A later pin pass must still recover the real question when a
        // synthetic observation is already present in history.
        let mut later = HistorySplit {
            prefix: vec![real_question.clone()],
            tail: vec![observation.clone()],
            selection: CompactionSelection {
                target_tokens: 1_000,
                estimated_tokens: 10,
                truncated: true,
            },
            retained_turns: 0,
        };
        let original_with_synthetic = vec![
            user_message("build three layouts"),
            observation,
            real_question.clone(),
        ];
        pin_last_question_in_tail(&mut later, &original_with_synthetic);
        assert!(
            later.tail.iter().any(|item| item == &real_question),
            "later pin must recover the real assistant question: {:?}",
            later.tail
        );
        assert_eq!(
            later
                .tail
                .iter()
                .rev()
                .find(|item| is_assistant_message(item)),
            Some(&real_question)
        );
    }

    #[test]
    fn extract_offline_progress_skips_synthetic_tool_observation_conclusions() {
        let history = NormalizedHistory {
            previous_checkpoint: None,
            previous_generation: 0,
            items: vec![
                user_message("write three homepage prototypes"),
                assistant_message("creating files now"),
                tool_observation_message(&["docs/proto-a.html".to_string()]),
                assistant_message("Still working…"),
            ],
        };
        let extracted = extract_offline_progress(&history);
        assert_eq!(
            extracted.conclusions,
            vec!["creating files now".to_string()]
        );
        assert!(extracted
            .conclusions
            .iter()
            .all(|text| !text.starts_with("[compacted tool files]")));

        let checkpoint = offline_checkpoint(&history);
        assert!(checkpoint.contains("creating files now"));
        assert!(!checkpoint.contains("[compacted tool files]"));
        assert!(!checkpoint.contains("Still working"));
    }

    #[test]
    fn retained_item_validation_accepts_object_form_function_call_arguments() {
        assert!(validate_retained_items(&[json!({
            "type": "function_call",
            "call_id": "call_obj",
            "name": "lookup",
            "arguments": {"query": "moss path", "limit": 3}
        })])
        .is_ok());
        assert!(validate_retained_items(&[json!({
            "type": "function_call",
            "call_id": "call_arr",
            "name": "lookup",
            "arguments": ["a", "b"]
        })])
        .is_ok());
        assert!(validate_retained_items(&[json!({
            "type": "function_call",
            "call_id": "call_str",
            "name": "lookup",
            "arguments": r#"{"query":"ok"}"#
        })])
        .is_ok());
        assert!(validate_retained_items(&[json!({
            "type": "function_call",
            "call_id": "call_bad",
            "name": "lookup",
            "arguments": "{not-json"
        })])
        .is_err());
        assert!(validate_retained_items(&[json!({
            "type": "function_call",
            "call_id": "call_null",
            "name": "lookup",
            "arguments": null
        })])
        .is_err());
        assert!(validate_retained_items(&[json!({
            "type": "function_call",
            "call_id": "call_num",
            "name": "lookup",
            "arguments": 12
        })])
        .is_err());
    }

    #[test]
    fn retained_item_validation_allows_in_flight_tool_calls_but_rejects_orphans() {
        // Live compact may retain a trailing unmatched tool *call* while the
        // tool is still running. That must not fail the whole compact.
        assert!(validate_retained_items(&[json!({
            "type": "function_call",
            "call_id": "call_open",
            "name": "lookup",
            "arguments": "{}"
        })])
        .is_ok());
        assert!(validate_retained_items(&[
            user_message("keep going"),
            json!({
                "type": "function_call",
                "call_id": "call_open",
                "name": "lookup",
                "arguments": "{}"
            }),
            json!({
                "type": "function_call",
                "call_id": "call_open",
                "name": "lookup",
                "arguments": "{}"
            }),
        ])
        .is_err());
        assert!(validate_retained_items(&[json!({
            "type": "function_call",
            "name": "lookup",
            "arguments": "{}"
        })])
        .is_err());
        assert!(validate_retained_items(&[json!({
            "type": "function_call_output",
            "call_id": "call_missing",
            "output": "orphan"
        })])
        .is_err());
    }

    #[test]
    fn item_mentions_paths_matches_raw_path_without_inspected_prefix() {
        let paths = vec!["inspected src/main.rs".to_string()];
        let assistant = assistant_message("I already read src/main.rs in detail.");
        assert!(item_mentions_paths(&assistant, &paths));
        let unrelated = assistant_message("I only looked at other crates.");
        assert!(!item_mentions_paths(&unrelated, &paths));
    }

    #[test]
    fn extract_inspected_paths_parses_json_cmd_arguments() {
        let paths = extract_inspected_paths(
            r#"{"cmd":"sed -n 1,200p src/main.rs","note":"long note with src/other.rs decoy"}"#,
        );
        assert_eq!(paths, vec!["inspected src/main.rs".to_string()]);

        let raw = extract_inspected_paths("rg -n TODO crates/codetas-gateway/src/compaction.rs");
        assert_eq!(
            raw,
            vec!["inspected crates/codetas-gateway/src/compaction.rs".to_string()]
        );
    }

    #[test]
    fn object_form_exec_command_arguments_produce_inspect_observations() {
        let item = json!({
            "type": "function_call",
            "call_id": "call_read",
            "name": "exec_command",
            "arguments": {"cmd": "sed -n '1,80p' src/main.rs"}
        });
        assert_eq!(
            inspect_file_observations(&item),
            Some(vec!["inspected src/main.rs".to_string()])
        );

        // String-form JSON args must keep the existing cmd-only behavior.
        let string_item = json!({
            "type": "function_call",
            "call_id": "call_read_str",
            "name": "exec_command",
            "arguments": r#"{"cmd":"sed -n 1,200p src/main.rs","note":"long note with src/other.rs decoy"}"#
        });
        assert_eq!(
            inspect_file_observations(&string_item),
            Some(vec!["inspected src/main.rs".to_string()])
        );
    }

    #[test]
    fn object_form_apply_patch_arguments_produce_write_observations() {
        let nested_input = json!({
            "type": "function_call",
            "call_id": "call_write_input",
            "name": "apply_patch",
            "arguments": {
                "input": "*** Begin Patch\n*** Add File: docs/object-form.html\n+ok\n*** End Patch\n"
            }
        });
        let paths = tool_file_observations(&nested_input).expect("object-form patch paths");
        assert!(
            paths
                .iter()
                .any(|path| path.contains("docs/object-form.html")),
            "expected written path from nested object field, got {paths:?}"
        );

        let nested_patch = json!({
            "type": "custom_tool_call",
            "call_id": "call_write_patch",
            "name": "apply_patch",
            "arguments": {
                "patch": "*** Begin Patch\n*** Update File: crates/codetas-gateway/src/compaction.rs\n+// note\n*** End Patch\n"
            }
        });
        let paths = tool_file_observations(&nested_patch).expect("nested patch field paths");
        assert!(
            paths
                .iter()
                .any(|path| path.contains("crates/codetas-gateway/src/compaction.rs")),
            "expected update path from nested patch field, got {paths:?}"
        );

        // Existing string-form patch text must still work.
        let string_item = json!({
            "type": "function_call",
            "call_id": "call_write_str",
            "name": "apply_patch",
            "arguments": "*** Begin Patch\n*** Add File: docs/string-form.html\n+ok\n*** End Patch\n"
        });
        let paths = tool_file_observations(&string_item).expect("string-form patch paths");
        assert!(
            paths
                .iter()
                .any(|path| path.contains("docs/string-form.html")),
            "expected string-form written path, got {paths:?}"
        );
    }

    #[test]
    fn oversized_inspect_turn_keeps_the_file_path_offline() {
        let items = vec![
            user_message("トップページの構成を把握して"),
            json!({
                "type": "function_call",
                "call_id": "call_read",
                "name": "exec_command",
                "arguments": format!(
                    "{{\"cmd\":\"sed -n '1,1357p' astro-home/src/pages/index.astro\",\"note\":\"{}\"}}",
                    "word ".repeat(200)
                )
            }),
            json!({
                "type": "function_call_output",
                "call_id": "call_read",
                "output": "word ".repeat(200)
            }),
            assistant_message("現行ページの全情報構成を把握しました。"),
        ];
        let live = split_prefix_and_tail(&items, 80).expect("live");
        assert!(
            live.prefix
                .iter()
                .any(|item| item.to_string().contains("word word word")),
            "live prefix must keep the original inspect payload"
        );
        let recovered = recover_split_for_offline(&items, 80).expect("recover");
        assert!(
            recovered.tail.iter().any(|item| item
                .to_string()
                .contains("astro-home/src/pages/index.astro")),
            "offline tail must keep the inspected path: {:?}",
            recovered.tail
        );
        assert!(
            !recovered
                .tail
                .iter()
                .any(|item| is_tool_call(item) || is_tool_result(item)),
            "tight inspect fallback must not leave orphan tool items: {:?}",
            recovered.tail
        );
    }

    #[test]
    fn repair_is_attempted_once_and_second_failure_stays_a_compact_failure() {
        let first = "too short";
        assert!(accepted_or_repaired_checkpoint(first, None).is_err());
        let repaired = fixture_checkpoint("- User: 砂漠は違う");
        let (text, was_repaired) =
            accepted_or_repaired_checkpoint(first, Some(&repaired)).expect("repair");
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
            retained: vec![user_message(
                "砂漠は違う。moss stepping-stone terrain を使う",
            )],
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
        assert_eq!(
            body["input"][2]["content"][0]["text"],
            "and keep the moss path"
        );
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
        let in_checkpoint = context
            .checkpoint
            .contains("use moss stepping-stone terrain");
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
            let (context, _) =
                build_compacted_context(&history, lossy_summarizer_checkpoint(), &settings)
                    .expect("compact");
            last_checkpoint = context.checkpoint.clone();
            last_encoded = encode_compacted_context(&context).expect("encode");
            let in_checkpoint = last_checkpoint.contains(correction);
            let in_retained = context
                .retained
                .iter()
                .any(|item| message_text(item).is_some_and(|text| text.contains(correction)));
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
            !history
                .items
                .iter()
                .any(|item| { message_text(item).is_some_and(|text| text.contains(correction)) }),
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
            format!(
                "{SUMMARY_PREFIX}\n{}",
                fixture_checkpoint("- already captured")
            ),
            &settings,
        )
        .expect("compact");
        assert!(!context.checkpoint.contains(SUMMARY_PREFIX));
        assert_eq!(context.generation, 5);
    }
}
