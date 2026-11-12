use super::*;

/// Deterministic checkpoint used when local compaction cannot call a summarizer.
/// This keeps Codex's remote-compact recovery alive while a provider target is
/// cooling down, instead of failing the turn with HTTP 503.
#[cfg(test)]
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
    if !extracted.requirements.is_empty()
        || !extracted.observations.is_empty()
        || !extracted.tool_results.is_empty()
    {
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
pub(super) const GENERIC_COOLDOWN_REMAINING: &str =
    "Resume from the retained recent turns. A later user message outranks this checkpoint if they disagree.";

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct ExtractedProgress {
    requirements: Vec<String>,
    pub(super) observations: Vec<String>,
    pub(super) conclusions: Vec<String>,
    remaining: Vec<String>,
    /// Results are quoted losslessly; checkpoint rollover archives the complete
    /// text and source history before publishing a bounded preview.
    pub(super) tool_results: Vec<String>,
}

pub(super) fn checkpoint_is_generic_cooldown_fallback(checkpoint: &str) -> bool {
    strip_offline_replay_metadata(checkpoint).trim()
        == render_offline_checkpoint(&ExtractedProgress::default()).trim()
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OfflineReplay {
    item_count: usize,
    sha256: String,
}

fn replay_fingerprint(items: &[Value]) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(items).expect("JSON history serialization"))
    )
}

pub(super) fn offline_replay_metadata(items: &[Value]) -> String {
    let replay = OfflineReplay {
        item_count: items.len(),
        sha256: replay_fingerprint(items),
    };
    format!(
        "{OFFLINE_REPLAY_PREFIX}{}",
        serde_json::to_string(&replay).expect("replay metadata serialization")
    )
}

pub(super) fn strip_offline_replay_metadata(checkpoint: &str) -> String {
    checkpoint
        .split_inclusive('\n')
        .filter(|line| !line.trim().starts_with(OFFLINE_REPLAY_PREFIX))
        .collect()
}

/// A checked prefix, not content-based deduplication. Identical new results
/// following this prefix are still new events. Legacy or altered input falls
/// back to transcribing everything instead of silently skipping unknown data.
pub(super) fn offline_replayed_item_count(history: &NormalizedHistory) -> usize {
    let Some(previous) = history.previous_checkpoint.as_deref() else {
        return 0;
    };
    if validate_checkpoint_summary(previous).is_err() {
        return 0;
    }
    let mut markers = previous
        .lines()
        .filter_map(|line| line.trim().strip_prefix(OFFLINE_REPLAY_PREFIX));
    let Some(raw) = markers.next() else {
        return 0;
    };
    if markers.next().is_some() {
        return 0;
    }
    let Ok(replay) = serde_json::from_str::<OfflineReplay>(raw) else {
        return 0;
    };
    if replay.item_count > MAX_RETAINED_ITEMS || replay.item_count > history.items.len() {
        return 0;
    }
    if replay.sha256 != replay_fingerprint(&history.items[..replay.item_count]) {
        return 0;
    }
    replay.item_count
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
        &offline_observation_bullets(&extracted.observations, &extracted.tool_results),
        "",
        "## Agent conclusions (unverified)",
        &bullets_or(&extracted.conclusions, "none"),
        "",
        "## Remaining work",
        &bullets_or(&extracted.remaining, GENERIC_COOLDOWN_REMAINING),
    ]
    .join("\n")
}

fn offline_observation_bullets(observations: &[String], tool_results: &[String]) -> String {
    let mut lines = vec![format!("- {GENERIC_COOLDOWN_OBS}")];
    for observation in observations {
        lines.push(format!("- {}", quote_control_data(observation)));
    }
    for result in tool_results {
        lines.push(format!("- {result}"));
    }
    lines.join("\n")
}

fn bullets_or(items: &[String], fallback: &str) -> String {
    if items.is_empty() {
        return format!("- {fallback}");
    }
    items
        .iter()
        .map(|item| format!("- {}", quote_control_data(item)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn merge_extracted_offline_progress(previous: &str, extracted: &ExtractedProgress) -> String {
    if extracted.observations.is_empty() && extracted.tool_results.is_empty() {
        return previous.to_string();
    }
    let next = refresh_checkpoint_observations(previous, &extracted.observations);
    let mut next = append_checkpoint_bullets(
        &next,
        "## Durable observations",
        &extracted.tool_results,
        false,
    );
    next = replace_generic_remaining_work(&next, &extracted.remaining);
    append_unique_checkpoint_bullets(&next, "## Remaining work", &extracted.remaining)
}

pub(super) fn append_unique_checkpoint_bullets(
    checkpoint: &str,
    heading: &str,
    extras: &[String],
) -> String {
    append_checkpoint_bullets(checkpoint, heading, extras, true)
}

fn append_checkpoint_bullets(
    checkpoint: &str,
    heading: &str,
    extras: &[String],
    deduplicate: bool,
) -> String {
    let Some(range) = checkpoint_section_range(checkpoint, heading) else {
        return checkpoint.to_string();
    };
    let body = &checkpoint[range.clone()];
    let mut extra = String::new();
    for item in extras {
        let item = quote_control_data(item);
        if deduplicate
            && body
                .lines()
                .chain(extra.lines())
                .any(|line| line == format!("- {item}"))
        {
            continue;
        }
        extra.push_str("\n- ");
        extra.push_str(&item);
    }
    if extra.is_empty() {
        return checkpoint.to_string();
    }
    let end = range.end;
    let mut merged = String::new();
    merged.push_str(checkpoint[..end].trim_end());
    merged.push_str(&extra);
    merged.push('\n');
    merged.push_str(&checkpoint[end..]);
    merged
}

pub(super) fn replace_generic_remaining_work(checkpoint: &str, remaining: &[String]) -> String {
    let Some(range) = checkpoint_section_range(checkpoint, "## Remaining work") else {
        return checkpoint.to_string();
    };
    let body = &checkpoint[range.clone()];
    let placeholder = format!("- {GENERIC_COOLDOWN_REMAINING}");
    if remaining.is_empty() || !body.lines().any(|line| line.trim() == placeholder) {
        return checkpoint.to_string();
    }
    let start = range.start;
    let end = range.end;
    let mut next = String::new();
    next.push_str(&checkpoint[..start]);
    // Replace only the writer's exact placeholder, never the whole section or
    // a task that merely quotes it. Real tasks and retrieval links must survive.
    for line in body
        .split_inclusive('\n')
        .filter(|line| line.trim() != placeholder)
    {
        next.push_str(line);
    }
    next.push_str(&checkpoint[end..]);
    append_unique_checkpoint_bullets(&next, "## Remaining work", remaining)
}

/// Observations use last-seen order (the same contract as push_observation).
/// An Add -> Delete -> Add sequence must not end in an apparently newer Delete
/// merely because Add was already present in an earlier checkpoint.
fn refresh_checkpoint_observations(checkpoint: &str, observations: &[String]) -> String {
    if observations.is_empty() {
        return checkpoint.to_string();
    }
    let heading = "## Durable observations";
    let Some(range) = checkpoint_section_range(checkpoint, heading) else {
        return checkpoint.to_string();
    };
    let refreshed: std::collections::HashSet<String> = observations
        .iter()
        .map(|entry| format!("- {}", quote_control_data(entry)))
        .collect();
    let mut next = checkpoint[..range.start].to_string();
    for line in checkpoint[range.clone()].split_inclusive('\n') {
        if !refreshed.contains(line.trim()) {
            next.push_str(line);
        }
    }
    next.push_str(&checkpoint[range.end..]);
    append_unique_checkpoint_bullets(&next, heading, observations)
}

pub(super) fn extract_offline_progress(history: &NormalizedHistory) -> ExtractedProgress {
    let mut extracted = ExtractedProgress::default();
    let mut open_calls = std::collections::HashMap::new();
    let replayed = offline_replayed_item_count(history);
    for (index, item) in history.items.iter().enumerate() {
        if is_tool_call(item) {
            if let Some(id) = call_id(item) {
                open_calls.insert(id, item);
            }
        } else if is_task_user_message(item) {
            if let Some(text) = clipped_message_text(item, 280) {
                push_unique(&mut extracted.requirements, text);
            }
        } else if is_assistant_message(item) {
            // Read the raw text first: `clipped_message_text` collapses newlines,
            // which would merge the observation's path lines into one.
            if let Some(raw) = message_text(item) {
                if let Some(paths) = synthetic_observation_paths(&raw) {
                    if index < replayed {
                        continue;
                    }
                    // A synthetic tool-file observation carries the only record
                    // of paths whose calls already left the history (a previous
                    // envelope's retained items). Excluding it dropped those
                    // paths with no other home once the observation fell outside
                    // the retained window.
                    for path in paths {
                        push_observation(&mut extracted.observations, path);
                    }
                    continue;
                }
            }
            if let Some(text) = clipped_message_text(item, 280) {
                if !is_placeholder_assistant_text(&text) {
                    push_unique(&mut extracted.conclusions, text);
                }
            }
        } else if is_tool_result(item) {
            // Record paths at the matching result, not at call dispatch. An
            // unanswered call is retained raw and is not fresh evidence when
            // the same retained tail is compacted again. Parallel results and
            // reused IDs are paired with their actual open occurrence.
            let call = call_id(item).and_then(|id| open_calls.remove(id));
            if index < replayed {
                continue;
            }
            if let Some(call) = call {
                if let Some(paths) = legacy_compacted_files(call)
                    .or_else(|| tool_file_observations(call))
                    .or_else(|| inspect_file_observations(call))
                {
                    for path in paths {
                        push_observation(&mut extracted.observations, path);
                    }
                }
            }
            if let Some(text) = offline_tool_output(item) {
                extracted.tool_results.push(text);
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

/// Keep unique observations in last-seen order without dropping older paths.
fn push_observation(items: &mut Vec<String>, item: String) {
    if item.is_empty() {
        return;
    }
    items.retain(|existing| existing != &item);
    items.push(item);
}

fn clipped_message_text(item: &Value, max_chars: usize) -> Option<String> {
    let text = collapse_ws(&message_text(item)?);
    if text.is_empty() {
        return None;
    }
    Some(clip_chars(&text, max_chars))
}

/// Preserve result values, including whitespace and text beyond a prefix clip.
fn offline_tool_output(item: &Value) -> Option<String> {
    let raw = item
        .get("output")
        .map(|value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .or_else(|| message_text(item))?;
    // An explicit empty/whitespace-only result is data (for example an empty
    // file or no search matches), unlike an absent output field. Preserve it.
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| item.get("call_id").and_then(Value::as_str))
        .unwrap_or("tool");
    // JSON strings preserve whitespace and make embedded headings data, not
    // checkpoint structure. Escaping '<' prevents data from becoming sentinels.
    Some(format!(
        "{} -> {}",
        quote_control_data(name),
        quote_checkpoint_string(&raw)
    ))
}

pub(super) fn quote_checkpoint_string(text: &str) -> String {
    serde_json::to_string(text)
        .expect("string serialization")
        .replace('<', "\\u003c")
}

pub(super) fn quote_control_data(text: &str) -> String {
    // Multi-line user text must not introduce checkpoint headings or bullets.
    // JSON escaping also keeps CRLF and embedded whitespace recoverable.
    if text.contains(['<', '\n', '\r']) {
        quote_checkpoint_string(text)
    } else {
        text.to_string()
    }
}

fn collapse_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn clip_chars(text: &str, max_chars: usize) -> String {
    let mut clipped = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        clipped.push('…');
    }
    clipped
}

pub(super) fn is_placeholder_assistant_text(text: &str) -> bool {
    matches!(
        text.trim(),
        "Still working…" | "Still working..." | "Still working."
    )
}

pub(super) fn is_synthetic_tool_observation_text(text: &str) -> bool {
    // A message is an observation only when its first non-empty line is the
    // marker *and* its whole body parses as writer-generated entries. Keeping two
    // notions of "observation" let a message count as one for selection (so an
    // assistant question was dropped from the retained tail) while the
    // transcription rejected or truncated it.
    observation_body(text).is_some_and(|body| {
        !body.is_empty() && body.iter().all(|line| observation_entry(line).is_some())
    })
}

/// Paths listed by a synthetic tool-file observation, if this is one.
///
/// The observation is written as `[compacted tool files]` followed by one path
/// per line. Returns `None` when the text is not an observation, so a `None`
/// result distinguishes "not an observation" from "an observation with no paths".
///
/// Only lines that look like paths are kept. The marker is matched after
/// trimming, but the body is split on the original lines, so a leading blank line
/// before the marker must not turn the marker itself into a path.
fn synthetic_observation_paths(text: &str) -> Option<Vec<String>> {
    let body = observation_body(text)?;
    // A message counts as an observation only when its whole body is
    // writer-generated entries. Checking each line on its own let an ordinary
    // assistant message that began with the marker lose its text (the question
    // was neither a path nor a conclusion); this way such a message stays a
    // normal assistant message.
    let paths: Vec<String> = body
        .iter()
        .map(|line| observation_entry(line))
        .collect::<Option<Vec<_>>>()?;
    // An observation with no entries is not an observation. Returning `Some([])`
    // here made selection and transcription disagree about a bare marker.
    (!paths.is_empty()).then_some(paths)
}

/// The lines after the observation marker, or `None` when this is not one.
///
/// Only the first non-empty line may carry the marker. Matching it anywhere let
/// an ordinary assistant question that quoted the marker be treated as an
/// observation.
fn observation_body(text: &str) -> Option<Vec<&str>> {
    let mut lines = text.lines().skip_while(|line| line.trim().is_empty());
    if lines.next()?.trim() != SYNTHETIC_OBSERVATION_MARKER {
        return None;
    }
    Some(lines.filter(|line| !line.trim().is_empty()).collect())
}

pub(crate) fn is_task_user_message(item: &Value) -> bool {
    is_user_message(item) && !is_injected_control_user_message(item)
}

/// The writer's file-operation prefixes, which identify an observation entry.
///
/// The gateway writes these, so a line carrying one is an observation the writer
/// produced and its remainder is taken verbatim. Guessing from punctuation which
/// free text "looks like" a path both lost real names (`docs/my directory`) and
/// accepted prose (`Please inspect docs/a.md`).
const OBSERVATION_PREFIXES: [&str; 3] = ["*** Add File:", "*** Update File:", "*** Delete File:"];

/// Prefix the inspection form uses. The path follows and the prefix is not part
/// of the stored value, because `inspected ` also appears inside ordinary prose.
const INSPECTED_PREFIX: &str = "inspected ";

/// One observation entry, or `None` when the line is not one.
fn observation_entry(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    for prefix in OBSERVATION_PREFIXES {
        if let Some(rest) = line.strip_prefix(prefix) {
            let rest = rest.trim();
            if rest.is_empty() {
                return None;
            }
            // Keep the writer's operation with the path. Dropping it merged
            // `*** Add File: docs/a.md` and `*** Delete File: docs/a.md` into one
            // entry and lost the fact that the file was deleted.
            return Some(clip_observation(&format!("{prefix} {rest}")));
        }
    }
    if let Some(rest) = line.strip_prefix(INSPECTED_PREFIX) {
        let rest = rest.trim();
        if rest.is_empty() || rest.split_whitespace().count() != 1 {
            return None;
        }
        return Some(clip_observation(rest));
    }
    // A bare line has no writer prefix. Only a whitespace-free token is taken:
    // any line with a space is prose unless a prefix above identified it, and
    // "Please inspect docs/a.md" must not become an observation.
    if line.split_whitespace().count() != 1 {
        return None;
    }
    // A bare token must look like a path: it needs a separator, a leading dot, or
    // an extension. An all-alphanumeric token such as `STOP` is prose.
    let has_separator = line.contains('/') || line.contains('\\');
    let has_extension = line
        .rsplit(['/', '\\'])
        .next()
        .is_some_and(|name| name.contains('.') && !name.ends_with('.'));
    let looks_like_path =
        !line.ends_with('.') && (has_separator || has_extension || line.starts_with('.'));
    looks_like_path.then(|| clip_observation(line))
}

/// Clip an observation entry while keeping distinct paths distinguishable.
///
/// A plain prefix clip made two different long paths with a shared prefix
/// identical, so the second was dropped as a duplicate. Prefix the clip with a
/// short digest of the whole value.
pub(super) fn clip_observation(value: &str) -> String {
    if value.chars().count() <= MAX_OBSERVATION_CHARS {
        return value.to_string();
    }
    let digest = observation_digest(value);
    let head = MAX_OBSERVATION_CHARS.saturating_sub(OBSERVATION_DIGEST_CHARS + 4);
    format!("{}…{}", clip_chars(value, head), digest)
}

/// Stable digest used to keep clipped observations distinct.
///
/// Two FNV-1a passes over the value and its reverse give 96 bits, so two
/// different long paths sharing a prefix do not collapse into one entry. A single
/// truncated 32-bit hash was shown to collide on a real pair.
fn observation_digest(value: &str) -> String {
    let forward = fnv1a(value.as_bytes());
    let mut reversed = value.as_bytes().to_vec();
    reversed.reverse();
    let backward = fnv1a(&reversed);
    let combined = format!("{forward:016x}{:08x}", backward >> 32);
    combined[..OBSERVATION_DIGEST_CHARS].to_string()
}

/// FNV-1a, stable across runs and dependency-free.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Paths stored by the older `summarize_tool_call` form.
///
/// That form replaced a call's arguments with
/// `{"codetas_compacted_files": [...]}`. An envelope written by it keeps the
/// paths only there, so the extraction has to read them back; the current
/// observation form is a separate assistant message.
pub(super) fn legacy_compacted_files(item: &Value) -> Option<Vec<String>> {
    if !is_tool_call(item) {
        return None;
    }
    // The writer only ever produced this key from a patch tool, and the value is
    // `*** <Kind> File: <path>` or `inspected <path>`. Requiring that shape keeps
    // an ordinary lookup whose arguments happen to carry the key from promoting
    // its payload into the checkpoint.
    let name = item.get("name").and_then(Value::as_str).unwrap_or("");
    if !matches!(
        name,
        "apply_patch" | "exec_command" | "exec" | "shell" | "bash" | "inspect" | "read_file"
    ) {
        return None;
    }
    let mut paths = Vec::new();
    // Either field may be null, empty, or an unrelated placeholder. Parse both
    // instead of letting field presence hide the older writer's input.
    for value in [item.get("arguments"), item.get("input")]
        .into_iter()
        .flatten()
    {
        let raw = value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string());
        if let Some(entries) = legacy_json_paths(&raw).or_else(|| synthetic_observation_paths(&raw))
        {
            for entry in entries {
                push_unique(&mut paths, entry);
            }
        }
    }
    (!paths.is_empty()).then_some(paths)
}

/// Paths from the older `{"codetas_compacted_files": [...]}` argument form.
fn legacy_json_paths(raw: &str) -> Option<Vec<String>> {
    let parsed: Value = serde_json::from_str(raw).ok()?;
    let paths = parsed.get("codetas_compacted_files")?.as_array()?;
    let paths: Vec<String> = paths
        .iter()
        .filter_map(Value::as_str)
        .filter_map(observation_entry)
        .collect();
    (!paths.is_empty()).then_some(paths)
}

pub(super) fn is_injected_control_user_message(item: &Value) -> bool {
    message_text(item).is_some_and(|text| injected_control_user_text(&text))
}

pub(super) fn injected_control_user_text(text: &str) -> bool {
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

pub(super) fn tool_file_observations(item: &Value) -> Option<Vec<String>> {
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

pub(super) fn inspect_file_observations(item: &Value) -> Option<Vec<String>> {
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

pub(super) fn extract_inspected_paths(arguments: &str) -> Vec<String> {
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
