use crate::config::{ProviderProtocol, ProviderTransport};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{Map, Value};
use std::{
    collections::BTreeSet,
    io::Cursor,
    sync::{Arc, OnceLock},
};
use tokio::sync::Semaphore;

const MIB: u64 = 1024 * 1024;
const ANTHROPIC_MAX_IMAGE_BASE64_CHARS: u64 = 5 * MIB;
const ANTHROPIC_TOTAL_IMAGE_BASE64_CHARS: u64 = 20 * MIB;
const GEMINI_TOTAL_INLINE_BASE64_CHARS: u64 = 20 * MIB;
const KIRO_TOTAL_IMAGE_BASE64_CHARS: u64 = 18 * MIB;
const MAX_NORMALIZABLE_IMAGE_BASE64_CHARS: usize = 64 * 1024 * 1024;
// Keep each decoded RGBA frame below roughly 40 MiB while still accepting a
// 4K screenshot. Two workers may run at once, so decoded frames stay near
// 80 MiB before encoder buffers.
const MAX_NORMALIZABLE_PIXELS: u64 = 10_000_000;
const IMAGE_NORMALIZATION_CONCURRENCY: usize = 2;
const OMITTED_FOR_BUDGET: &str =
    "[image omitted: older image data exceeded the provider request budget]";
const OMITTED_PER_IMAGE: &str =
    "[image omitted: image data exceeded the provider per-image limit]";
const OMITTED_UNSAFE_PNG: &str =
    "[image omitted: PNG could not be normalized within the safety limits]";
const OMITTED_FOR_COMPACTION: &str = "[image omitted for compaction]";
const OMITTED_FOR_ADMISSION: &str =
    "[image omitted: older image data exceeded the gateway request body limit]";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImageHistoryReport {
    pub inline_images: usize,
    pub omitted_images: usize,
    pub kept_base64_chars: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AdmissionImageRewriteReport {
    pub inline_images: usize,
    pub omitted_images: usize,
}

#[derive(Clone, Copy, Debug)]
struct ImageHistoryPolicy {
    max_image_base64_chars: u64,
    max_total_base64_chars: u64,
}

impl ImageHistoryPolicy {
    fn for_provider(
        protocol: ProviderProtocol,
        transport: ProviderTransport,
        max_request_bytes: u64,
    ) -> Self {
        // Keep room for instructions, tool schemas, JSON framing, and translated
        // provider envelopes. Final serialized-size fitting in the sender remains
        // authoritative and may omit additional oldest images.
        let reserved = (max_request_bytes / 8).clamp(MIB, 8 * MIB);
        let request_image_budget = max_request_bytes.saturating_sub(reserved);
        let provider_total = if transport == ProviderTransport::Kiro {
            KIRO_TOTAL_IMAGE_BASE64_CHARS
        } else {
            match protocol {
                ProviderProtocol::AnthropicMessages => ANTHROPIC_TOTAL_IMAGE_BASE64_CHARS,
                ProviderProtocol::GeminiGenerateContent => GEMINI_TOTAL_INLINE_BASE64_CHARS,
                ProviderProtocol::Responses | ProviderProtocol::ChatCompletions => u64::MAX,
            }
        };
        let max_image_base64_chars = if protocol == ProviderProtocol::AnthropicMessages {
            ANTHROPIC_MAX_IMAGE_BASE64_CHARS
        } else {
            u64::MAX
        };
        Self {
            max_image_base64_chars,
            max_total_base64_chars: request_image_budget.min(provider_total),
        }
    }
}

static IMAGE_NORMALIZATION_SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();

pub async fn normalize_translated_image_history(
    body: &mut Value,
    protocol: ProviderProtocol,
    transport: ProviderTransport,
    max_request_bytes: u64,
) -> Result<ImageHistoryReport, String> {
    // This probe must not validate or scan the Base64 payload. Requests can
    // contain hundreds of MiB of image text, and that work belongs on the
    // blocking worker below rather than a Tokio executor thread.
    if !contains_inline_image_candidate(body) {
        return Ok(ImageHistoryReport::default());
    }
    let permit = IMAGE_NORMALIZATION_SLOTS
        .get_or_init(|| Arc::new(Semaphore::new(IMAGE_NORMALIZATION_CONCURRENCY)))
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| "image normalization worker pool is unavailable".to_string())?;
    let owned_body = std::mem::take(body);
    let (normalized_body, report) = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut normalized_body = owned_body;
        let report = normalize_translated_image_history_sync(
            &mut normalized_body,
            protocol,
            transport,
            max_request_bytes,
        );
        (normalized_body, report)
    })
    .await
    .map_err(|error| format!("image normalization worker failed: {error}"))?;
    *body = normalized_body;
    Ok(report)
}

fn normalize_translated_image_history_sync(
    body: &mut Value,
    protocol: ProviderProtocol,
    transport: ProviderTransport,
    max_request_bytes: u64,
) -> ImageHistoryReport {
    let policy = ImageHistoryPolicy::for_provider(protocol, transport, max_request_bytes);
    let mut images = Vec::new();
    collect_inline_images(body, &mut images);
    if images.is_empty() {
        return ImageHistoryReport::default();
    }
    let original_image_count = images.len();
    let initial_total = images.iter().copied().sum::<u64>();
    let exceeds_individual_limit = images
        .iter()
        .any(|bytes| *bytes > policy.max_image_base64_chars);
    let enforce_byte_tiers =
        initial_total > policy.max_total_base64_chars || exceeds_individual_limit;
    let mut image_index = 0_usize;
    let mut unsafe_png_omissions = 0_usize;
    normalize_png_age_tiers(
        body,
        original_image_count,
        &mut image_index,
        enforce_byte_tiers,
        &mut unsafe_png_omissions,
    );
    images.clear();
    collect_inline_images(body, &mut images);

    let mut omitted = BTreeSet::new();
    let mut kept_total = 0_u64;
    for (index, bytes) in images.iter().copied().enumerate() {
        if bytes > policy.max_image_base64_chars {
            omitted.insert(index);
        } else {
            kept_total = kept_total.saturating_add(bytes);
        }
    }

    // History is traversed oldest-first. Preserve the newest screenshots at
    // full fidelity and remove older payloads until the aggregate fits.
    for (index, bytes) in images.iter().copied().enumerate() {
        if kept_total <= policy.max_total_base64_chars {
            break;
        }
        if omitted.insert(index) {
            kept_total = kept_total.saturating_sub(bytes);
        }
    }

    let mut image_index = 0_usize;
    replace_selected_images(body, &omitted, &mut image_index, policy.max_image_base64_chars);
    ImageHistoryReport {
        inline_images: original_image_count,
        omitted_images: omitted.len().saturating_add(unsafe_png_omissions),
        kept_base64_chars: kept_total,
    }
}

/// Remove exactly one oldest inline image. The sender uses this after wire
/// translation when JSON framing or tool schemas still exceed maxRequestBytes.
pub fn omit_oldest_translated_input_image(body: &mut Value) -> bool {
    omit_first_image(body)
}

/// Replace older inline images with path-bearing text before the 128 MiB
/// admission cap. Older pixels stay on disk and can be reopened with
/// `view_image`. The first omit count is estimated from payload sizes so the
/// full request is not cloned during search; extra oldest images are dropped
/// only if the serialized body is still over the limit.
pub fn rewrite_oversized_request_images_for_admission(
    body: &mut Value,
    body_limit: u64,
) -> AdmissionImageRewriteReport {
    let mut images = Vec::new();
    collect_inline_image_refs(body, &mut Vec::new(), &mut None, &mut images);
    if images.is_empty() {
        return AdmissionImageRewriteReport::default();
    }
    let inline_images = images.len();
    if estimate_json_request_bytes(body) <= body_limit {
        return AdmissionImageRewriteReport {
            inline_images,
            omitted_images: 0,
        };
    }
    let original_bytes = estimate_json_request_bytes(body);
    let mut omit_count = estimated_omit_count(original_bytes, &images, body_limit);
    let omit = (0..omit_count).collect::<BTreeSet<_>>();
    let mut image_index = 0_usize;
    replace_admission_images(body, &omit, &mut image_index, &images);
    while estimate_json_request_bytes(body) > body_limit {
        if omit_count >= inline_images {
            break;
        }
        let remaining = &images[omit_count..];
        let next = BTreeSet::from([0]);
        let mut image_index = 0_usize;
        replace_admission_images(body, &next, &mut image_index, remaining);
        omit_count += 1;
    }
    AdmissionImageRewriteReport {
        inline_images,
        omitted_images: omit_count,
    }
}

fn estimated_omit_count(
    original_bytes: u64,
    images: &[InlineImageRef],
    body_limit: u64,
) -> usize {
    if original_bytes <= body_limit {
        return 0;
    }
    let mut saved = 0_u64;
    for (index, image) in images.iter().enumerate() {
        let marker_bytes = (admission_image_marker(image.source_path.as_deref()).len() as u64)
            .saturating_add(24);
        saved = saved.saturating_add(image.payload_bytes.saturating_sub(marker_bytes));
        if original_bytes.saturating_sub(saved) <= body_limit {
            return index + 1;
        }
    }
    images.len()
}

/// Shrink an already-buffered Responses JSON body so admission can accept it.
/// Non-JSON bodies are left untouched.
pub fn shrink_admitted_request_bytes(
    bytes: &[u8],
    body_limit: u64,
) -> Option<(Vec<u8>, AdmissionImageRewriteReport)> {
    if (bytes.len() as u64) <= body_limit {
        return None;
    }
    let mut body = serde_json::from_slice::<Value>(bytes).ok()?;
    let report = rewrite_oversized_request_images_for_admission(&mut body, body_limit);
    if report.omitted_images == 0 {
        return None;
    }
    let rewritten = serde_json::to_vec(&body).ok()?;
    if (rewritten.len() as u64) > body_limit {
        return None;
    }
    Some((rewritten, report))
}

/// Kiro stores images on `userInputMessage.images`. Budget omission marks those
/// entries; this converts markers into content text and drops invalid slots.
pub fn scrub_kiro_omitted_images(payload: &mut Value) {
    let Some(state) = payload
        .get_mut("conversationState")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    if let Some(history) = state.get_mut("history").and_then(Value::as_array_mut) {
        for turn in history {
            if let Some(message) = turn
                .get_mut("userInputMessage")
                .and_then(Value::as_object_mut)
            {
                scrub_kiro_user_message_images(message);
            }
        }
    }
    if let Some(message) = state
        .get_mut("currentMessage")
        .and_then(|current| current.get_mut("userInputMessage"))
        .and_then(Value::as_object_mut)
    {
        scrub_kiro_user_message_images(message);
    }
}

fn scrub_kiro_user_message_images(message: &mut Map<String, Value>) {
    let Some(images) = message.get_mut("images").and_then(Value::as_array_mut) else {
        return;
    };
    let mut markers = Vec::new();
    images.retain(|image| {
        let Some(object) = image.as_object() else {
            return false;
        };
        if object.get("type").and_then(Value::as_str) == Some("omitted_image") {
            if let Some(text) = object.get("text").and_then(Value::as_str) {
                markers.push(text.to_string());
            }
            return false;
        }
        object
            .get("source")
            .and_then(|source| source.get("bytes"))
            .and_then(Value::as_str)
            .is_some_and(|bytes| !bytes.is_empty())
    });
    if images.is_empty() {
        message.remove("images");
    }
    for marker in markers {
        match message.get_mut("content") {
            Some(Value::String(content)) if !content.is_empty() => {
                content.push_str("

");
                content.push_str(&marker);
            }
            _ => {
                message.insert("content".into(), Value::String(marker));
            }
        }
    }
}

pub fn count_translated_input_images(body: &Value) -> usize {
    match body {
        Value::Array(items) => items.iter().map(count_translated_input_images).sum(),
        Value::Object(object) => {
            let is_kiro_image = object.get("format").and_then(Value::as_str).is_some()
                && object
                    .get("source")
                    .and_then(|source| source.get("bytes"))
                    .and_then(Value::as_str)
                    .is_some();
            let is_wire_inline_image = match wire_image_kind(object) {
                Some(WireImageKind::Responses | WireImageKind::Chat) => inline_image_url(object)
                    .is_some_and(|url| url.starts_with("data:image/")),
                Some(WireImageKind::Anthropic | WireImageKind::Gemini | WireImageKind::Kiro) => true,
                None => false,
            };
            if is_wire_inline_image || is_kiro_image {
                1
            } else {
                object.values().map(count_translated_input_images).sum()
            }
        }
        _ => 0,
    }
}

pub fn strip_translated_input_images_for_compaction(body: &mut Value) {
    replace_all_images(body, OMITTED_FOR_COMPACTION);
}

fn collect_inline_images(value: &Value, images: &mut Vec<u64>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_inline_images(item, images);
            }
        }
        Value::Object(object) => {
            if let Some(chars) = inline_image_base64_chars(object) {
                images.push(chars);
                return;
            }
            for child in object.values() {
                collect_inline_images(child, images);
            }
        }
        _ => {}
    }
}

#[derive(Clone, Debug)]
struct InlineImageRef {
    payload_bytes: u64,
    source_path: Option<String>,
}

fn child_values(object: &Map<String, Value>) -> Vec<&Value> {
    const PREFERRED: &[&str] = &["input", "output", "content", "messages", "parts"];
    let mut children = Vec::with_capacity(object.len());
    for key in PREFERRED {
        if let Some(child) = object.get(*key) {
            children.push(child);
        }
    }
    for (key, child) in object {
        if !PREFERRED.contains(&key.as_str()) {
            children.push(child);
        }
    }
    children
}

fn child_values_mut(object: &mut Map<String, Value>) -> Vec<&mut Value> {
    const PREFERRED: &[&str] = &["input", "output", "content", "messages", "parts"];
    let mut preferred = Vec::new();
    let mut rest = Vec::new();
    for (key, child) in object.iter_mut() {
        if PREFERRED.contains(&key.as_str()) {
            preferred.push((PREFERRED.iter().position(|item| *item == key.as_str()).unwrap_or(PREFERRED.len()), child));
        } else {
            rest.push(child);
        }
    }
    preferred.sort_by_key(|(index, _)| *index);
    preferred.into_iter().map(|(_, child)| child).chain(rest).collect()
}

fn collect_inline_image_refs(
    value: &Value,
    pending_paths: &mut Vec<(String, String)>,
    caption_path: &mut Option<String>,
    images: &mut Vec<InlineImageRef>,
) {
    match value {
        Value::Array(items) => {
            let mut local_caption = None;
            for item in items {
                collect_inline_image_refs(item, pending_paths, &mut local_caption, images);
            }
        }
        Value::String(text) if looks_like_image_data_url(text) => {
            images.push(InlineImageRef {
                payload_bytes: estimate_value_bytes(value),
                source_path: caption_path.take(),
            });
        }
        Value::String(text) => {
            if let Some(path) = parse_caption_image_path(text) {
                *caption_path = Some(path);
            }
            if let Some(parsed) = parse_encoded_json_value(value) {
                collect_inline_image_refs(&parsed, pending_paths, caption_path, images);
            }
        }
        Value::Object(object) => {
            if let Some(path) = recorded_view_image_path(object) {
                if let Some(call_id) = object.get("call_id").and_then(Value::as_str) {
                    pending_paths.push((call_id.to_string(), path));
                }
            }
            if let Some(path) = object_text_caption_path(object) {
                *caption_path = Some(path);
            }
            if inline_image_base64_chars(object).is_some() {
                images.push(InlineImageRef {
                    payload_bytes: estimate_value_bytes(value),
                    source_path: recorded_inline_image_path(object)
                        .or_else(|| caption_path.take())
                        .or_else(|| pending_tool_path(object, pending_paths)),
                });
                return;
            }
            if tool_output_contains_inline_images(object) {
                let source_path = pending_tool_path(object, pending_paths);
                let before = images.len();
                if let Some(output) = object.get("output") {
                    collect_inline_image_refs(output, pending_paths, caption_path, images);
                }
                if let Some(path) = source_path {
                    for image in &mut images[before..] {
                        if image.source_path.is_none() {
                            image.source_path = Some(path.clone());
                        }
                    }
                }
                return;
            }
            for child in child_values(object) {
                collect_inline_image_refs(child, pending_paths, caption_path, images);
            }
        }
        _ => {}
    }
}

fn object_text_caption_path(object: &Map<String, Value>) -> Option<String> {
    if !matches!(
        object.get("type").and_then(Value::as_str),
        Some("input_text" | "output_text" | "text")
    ) {
        return None;
    }
    parse_caption_image_path(object.get("text").and_then(Value::as_str)?)
}

fn recorded_inline_image_path(object: &Map<String, Value>) -> Option<String> {
    object
        .get("path")
        .and_then(Value::as_str)
        .and_then(sanitize_local_image_path)
}

fn parse_caption_image_path(text: &str) -> Option<String> {
    const MARKERS: &[&str] = &["path=\"", "path='"];
    for marker in MARKERS {
        let Some(found) = text.find(marker) else {
            continue;
        };
        let rest = &text[found + marker.len()..];
        let quote = marker.chars().last()?;
        let end = rest.find(quote)?;
        if let Some(path) = sanitize_local_image_path(&rest[..end]) {
            return Some(path);
        }
    }
    None
}

fn recorded_view_image_path(object: &Map<String, Value>) -> Option<String> {
    if !matches!(
        object.get("type").and_then(Value::as_str),
        Some("function_call" | "custom_tool_call")
    ) {
        return None;
    }
    if object.get("name").and_then(Value::as_str) != Some("view_image") {
        return None;
    }
    parse_view_image_path(object.get("arguments"))
        .or_else(|| parse_view_image_path(object.get("input")))
}

fn pending_tool_path(
    object: &Map<String, Value>,
    pending_paths: &mut Vec<(String, String)>,
) -> Option<String> {
    let call_id = object.get("call_id").and_then(Value::as_str)?;
    if let Some(index) = pending_paths.iter().position(|(id, _)| id == call_id) {
        Some(pending_paths.remove(index).1)
    } else {
        None
    }
}

fn is_tool_output_item(object: &Map<String, Value>) -> bool {
    matches!(
        object.get("type").and_then(Value::as_str),
        Some("function_call_output" | "custom_tool_call_output")
    )
}

fn tool_output_contains_inline_images(object: &Map<String, Value>) -> bool {
    is_tool_output_item(object)
        && object.get("output").is_some_and(value_contains_inline_image)
}

fn value_contains_inline_image(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(value_contains_inline_image),
        Value::Object(object) => {
            inline_image_base64_chars(object).is_some()
                || object.values().any(value_contains_inline_image)
        }
        Value::String(text) => parse_encoded_json_value(value)
            .as_ref()
            .is_some_and(value_contains_inline_image)
            || looks_like_image_data_url(text),
        _ => false,
    }
}

fn parse_encoded_json_value(value: &Value) -> Option<Value> {
    let text = value.as_str()?.trim();
    if !(text.starts_with('[') || text.starts_with('{')) {
        return None;
    }
    serde_json::from_str(text).ok()
}

fn looks_like_image_data_url(text: &str) -> bool {
    let Some((metadata, payload)) = text.trim().split_once(',') else {
        return false;
    };
    metadata.starts_with("data:image/") && metadata.ends_with(";base64") && !payload.is_empty()
}

fn parse_view_image_path(value: Option<&Value>) -> Option<String> {
    let raw = match value? {
        Value::String(text) => text.as_str(),
        Value::Object(object) => object.get("path").and_then(Value::as_str)?,
        _ => return None,
    };
    if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
        if let Some(path) = parsed.get("path").and_then(Value::as_str) {
            return sanitize_local_image_path(path);
        }
    }
    sanitize_local_image_path(raw)
}

fn sanitize_local_image_path(path: &str) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed.len() > 1024 || trimmed.contains('\0') {
        return None;
    }
    if trimmed.contains("://") {
        return None;
    }
    let bytes = trimmed.as_bytes();
    let looks_absolute = trimmed.starts_with('/')
        || trimmed.starts_with("\\\\")
        || (bytes.len() > 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':');
    if !looks_absolute {
        return None;
    }
    Some(trimmed.to_string())
}

fn replace_admission_images(
    value: &mut Value,
    omitted: &BTreeSet<usize>,
    image_index: &mut usize,
    images: &[InlineImageRef],
) {
    match value {
        Value::Array(items) => {
            for item in items {
                replace_admission_images(item, omitted, image_index, images);
            }
        }
        Value::String(text) if looks_like_image_data_url(text) => {
            let index = *image_index;
            *image_index += 1;
            if omitted.contains(&index) {
                *value = Value::String(admission_image_marker(
                    images.get(index).and_then(|image| image.source_path.as_deref()),
                ));
            }
        }
        Value::String(_) => {
            if let Some(mut parsed) = parse_encoded_json_value(value) {
                replace_admission_images(&mut parsed, omitted, image_index, images);
                if let Ok(encoded) = serde_json::to_string(&parsed) {
                    *value = Value::String(encoded);
                }
            }
        }
        Value::Object(object) => {
            if inline_image_base64_chars(object).is_some() {
                let index = *image_index;
                *image_index += 1;
                if omitted.contains(&index) {
                    replace_admission_image_object(
                        object,
                        images.get(index).and_then(|image| image.source_path.as_deref()),
                    );
                }
                return;
            }
            if is_tool_output_item(object) {
                if let Some(output) = object.get_mut("output") {
                    replace_images_in_tool_output(output, omitted, image_index, images);
                }
                return;
            }
            for child in child_values_mut(object) {
                replace_admission_images(child, omitted, image_index, images);
            }
        }
        _ => {}
    }
}

fn replace_images_in_tool_output(
    output: &mut Value,
    omitted: &BTreeSet<usize>,
    image_index: &mut usize,
    images: &[InlineImageRef],
) {
    if let Some(mut parsed) = parse_encoded_json_value(output) {
        replace_admission_images(&mut parsed, omitted, image_index, images);
        if let Ok(encoded) = serde_json::to_string(&parsed) {
            *output = Value::String(encoded);
        }
        return;
    }
    replace_admission_images(output, omitted, image_index, images);
}

fn replace_admission_image_object(object: &mut Map<String, Value>, source_path: Option<&str>) {
    let marker = admission_image_marker(source_path);
    if let Some(kind) = wire_image_kind(object) {
        replace_wire_image_object(object, kind, &marker);
    } else {
        replace_image_object(object, &marker);
    }
}

fn admission_image_marker(source_path: Option<&str>) -> String {
    match source_path {
        Some(path) => format!("{OMITTED_FOR_ADMISSION}\npath: {path}"),
        None => OMITTED_FOR_ADMISSION.to_string(),
    }
}

fn estimate_json_request_bytes(value: &Value) -> u64 {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len() as u64)
        .unwrap_or_else(|_| estimate_value_bytes(value))
}

fn estimate_value_bytes(value: &Value) -> u64 {
    match value {
        Value::Null => 4,
        Value::Bool(_) => 5,
        Value::Number(number) => number.to_string().len() as u64,
        Value::String(text) => (text.len() as u64).saturating_add(2),
        Value::Array(items) => items
            .iter()
            .map(estimate_value_bytes)
            .fold(2_u64, u64::saturating_add),
        Value::Object(object) => object.iter().fold(2_u64, |total, (key, child)| {
            total
                .saturating_add(key.len() as u64)
                .saturating_add(3)
                .saturating_add(estimate_value_bytes(child))
        }),
    }
}

fn contains_inline_image_candidate(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(contains_inline_image_candidate),
        Value::Object(object) => {
            if is_image_object(object)
                && inline_image_url(object).is_some_and(|url| url.starts_with("data:image/"))
            {
                return true;
            }
            if wire_inline_image_base64_chars(object).is_some() {
                return true;
            }
            object.values().any(contains_inline_image_candidate)
        }
        _ => false,
    }
}

fn normalize_png_age_tiers(
    value: &mut Value,
    image_count: usize,
    image_index: &mut usize,
    enforce_byte_tiers: bool,
    unsafe_png_omissions: &mut usize,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                normalize_png_age_tiers(
                    item,
                    image_count,
                    image_index,
                    enforce_byte_tiers,
                    unsafe_png_omissions,
                );
            }
        }
        Value::Object(object) => {
            if inline_image_base64_chars(object).is_some() {
                let from_newest = image_count.saturating_sub(*image_index + 1);
                *image_index += 1;
                let (max_edge, hard_cap) = if from_newest < 6 {
                    (2_000_u32, 2 * 1024 * 1024_usize)
                } else if from_newest < 20 {
                    (1_024_u32, 512 * 1024_usize)
                } else {
                    (700_u32, 192 * 1024_usize)
                };
                let outcome = inline_image_url(object)
                    .map(|url| {
                        normalize_png_data_url(
                            url,
                            max_edge,
                            enforce_byte_tiers.then_some(hard_cap),
                        )
                    })
                    .unwrap_or(PngNormalization::Unchanged);
                match outcome {
                    PngNormalization::Unchanged => {}
                    PngNormalization::Replace(normalized) => {
                        if let Some(url) = inline_image_url_mut(object) {
                            *url = normalized;
                        }
                    }
                    PngNormalization::Omit => {
                        replace_image_object(object, OMITTED_UNSAFE_PNG);
                        *unsafe_png_omissions = unsafe_png_omissions.saturating_add(1);
                    }
                }
                return;
            }
            if let Some(kind) = wire_image_kind(object) {
                if matches!(
                    kind,
                    WireImageKind::Anthropic | WireImageKind::Gemini | WireImageKind::Kiro
                ) {
                    // Provider-native wire images keep original fidelity here; budgets omit
                    // whole images instead of re-encoding provider-native binary fields.
                    *image_index += 1;
                    return;
                }
            }
            for child in object.values_mut() {
                normalize_png_age_tiers(
                    child,
                    image_count,
                    image_index,
                    enforce_byte_tiers,
                    unsafe_png_omissions,
                );
            }
        }
        _ => {}
    }
}

fn inline_image_url_mut(object: &mut Map<String, Value>) -> Option<&mut String> {
    let direct_image_url = matches!(object.get("image_url"), Some(Value::String(_)));
    if direct_image_url {
        return match object.get_mut("image_url") {
            Some(Value::String(url)) => Some(url),
            _ => None,
        };
    }
    let nested_image_url = object
        .get("image_url")
        .and_then(Value::as_object)
        .is_some_and(|image_url| matches!(image_url.get("url"), Some(Value::String(_))));
    if nested_image_url {
        return match object.get_mut("image_url") {
            Some(Value::Object(image_url)) => match image_url.get_mut("url") {
                Some(Value::String(url)) => Some(url),
                _ => None,
            },
            _ => None,
        };
    }
    match object.get_mut("url") {
        Some(Value::String(url)) => Some(url),
        _ => None,
    }
}

fn inline_image_url(object: &Map<String, Value>) -> Option<&str> {
    object
        .get("image_url")
        .and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("url").and_then(Value::as_str))
        })
        .or_else(|| object.get("url").and_then(Value::as_str))
}

enum PngNormalization {
    Unchanged,
    Replace(String),
    Omit,
}

fn normalize_png_data_url(
    value: &str,
    max_edge: u32,
    hard_cap: Option<usize>,
) -> PngNormalization {
    let Some(payload) = value
        .strip_prefix("data:image/png;base64,")
        .or_else(|| value.strip_prefix("data:image/x-png;base64,"))
    else {
        return PngNormalization::Unchanged;
    };
    let original_payload_len = payload.len();
    if payload.len() > MAX_NORMALIZABLE_IMAGE_BASE64_CHARS {
        return PngNormalization::Omit;
    }
    let Ok(encoded) = STANDARD.decode(payload.as_bytes()) else {
        return PngNormalization::Omit;
    };
    let mut decoder = png::Decoder::new(Cursor::new(encoded));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let Ok(mut reader) = decoder.read_info() else {
        return PngNormalization::Omit;
    };
    if u64::from(reader.info().width).saturating_mul(u64::from(reader.info().height))
        > MAX_NORMALIZABLE_PIXELS
    {
        return PngNormalization::Omit;
    }
    let exceeds_max_edge = hard_cap.is_some()
        && reader.info().width.max(reader.info().height) > max_edge;
    let exceeds_byte_tier = hard_cap.is_some_and(|cap| payload.len() > cap);
    if !exceeds_byte_tier && !exceeds_max_edge {
        return PngNormalization::Unchanged;
    }
    let Some(output_size) = reader.output_buffer_size() else {
        return PngNormalization::Omit;
    };
    let Some(max_output_size) = usize::try_from(MAX_NORMALIZABLE_PIXELS)
        .ok()
        .map(|pixels| pixels.saturating_mul(4))
    else {
        return PngNormalization::Omit;
    };
    if output_size > max_output_size {
        return PngNormalization::Omit;
    }
    let mut pixels = vec![0_u8; output_size];
    let Ok(info) = reader.next_frame(&mut pixels) else {
        return PngNormalization::Omit;
    };
    if info.bit_depth != png::BitDepth::Eight
        || u64::from(info.width).saturating_mul(u64::from(info.height))
            > MAX_NORMALIZABLE_PIXELS
    {
        return PngNormalization::Omit;
    }
    let channels = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return PngNormalization::Omit,
    };
    pixels.truncate(info.buffer_size());
    let mut target_edge = max_edge.min(info.width.max(info.height)).max(1);
    loop {
        let scale = target_edge as f64 / f64::from(info.width.max(info.height));
        let width = (f64::from(info.width) * scale).round().max(1.0) as u32;
        let height = (f64::from(info.height) * scale).round().max(1.0) as u32;
        let Some(resized) = resize_nearest(
            &pixels,
            info.width,
            info.height,
            width,
            height,
            channels,
        ) else {
            return PngNormalization::Omit;
        };
        let mut png_bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png_bytes, width, height);
            encoder.set_color(info.color_type);
            encoder.set_depth(png::BitDepth::Eight);
            let Ok(mut writer) = encoder.write_header() else {
                return PngNormalization::Omit;
            };
            if writer.write_image_data(&resized).is_err() {
                return PngNormalization::Omit;
            }
        }
        let normalized_payload = STANDARD.encode(png_bytes);
        if hard_cap.is_none_or(|cap| normalized_payload.len() <= cap) || target_edge <= 320 {
            if exceeds_max_edge || normalized_payload.len() < original_payload_len {
                return PngNormalization::Replace(format!(
                    "data:image/png;base64,{normalized_payload}"
                ));
            }
            return PngNormalization::Unchanged;
        }
        target_edge = ((target_edge as u64 * 3) / 4).max(320) as u32;
    }
}

fn resize_nearest(
    source: &[u8],
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
    channels: usize,
) -> Option<Vec<u8>> {
    let target_len = usize::try_from(target_width)
        .ok()?
        .checked_mul(usize::try_from(target_height).ok()?)?
        .checked_mul(channels)?;
    let mut target = vec![0_u8; target_len];
    for y in 0..target_height {
        let source_y = (u64::from(y) * u64::from(source_height) / u64::from(target_height)) as usize;
        for x in 0..target_width {
            let source_x = (u64::from(x) * u64::from(source_width) / u64::from(target_width)) as usize;
            let source_offset = source_y
                .checked_mul(usize::try_from(source_width).ok()?)?
                .checked_add(source_x)?
                .checked_mul(channels)?;
            let target_offset = usize::try_from(y)
                .ok()?
                .checked_mul(usize::try_from(target_width).ok()?)?
                .checked_add(usize::try_from(x).ok()?)?
                .checked_mul(channels)?;
            target.get_mut(target_offset..target_offset + channels)?
                .copy_from_slice(source.get(source_offset..source_offset + channels)?);
        }
    }
    Some(target)
}

fn replace_selected_images(
    value: &mut Value,
    omitted: &BTreeSet<usize>,
    image_index: &mut usize,
    per_image_limit: u64,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                replace_selected_images(item, omitted, image_index, per_image_limit);
            }
        }
        Value::Object(object) => {
            if let Some(chars) = inline_image_base64_chars(object) {
                let index = *image_index;
                *image_index += 1;
                if omitted.contains(&index) {
                    let marker = if chars > per_image_limit {
                        OMITTED_PER_IMAGE
                    } else {
                        OMITTED_FOR_BUDGET
                    };
                    if let Some(kind) = wire_image_kind(object) {
                        replace_wire_image_object(object, kind, marker);
                    } else {
                        replace_image_object(object, marker);
                    }
                }
                return;
            }
            for child in object.values_mut() {
                replace_selected_images(child, omitted, image_index, per_image_limit);
            }
        }
        _ => {}
    }
}

fn omit_first_image(value: &mut Value) -> bool {
    match value {
        Value::Array(items) => items.iter_mut().any(omit_first_image),
        Value::Object(object) => {
            if let Some(kind) = wire_image_kind(object) {
                replace_wire_image_object(object, kind, OMITTED_FOR_BUDGET);
                true
            } else {
                object.values_mut().any(omit_first_image)
            }
        }
        _ => false,
    }
}

fn replace_all_images(value: &mut Value, marker: &str) {
    match value {
        Value::Array(items) => {
            for item in items {
                replace_all_images(item, marker);
            }
        }
        Value::Object(object) => {
            if is_image_object(object) {
                replace_image_object(object, marker);
                return;
            }
            if let Some(kind) = wire_image_kind(object) {
                replace_wire_image_object(object, kind, marker);
                return;
            }
            for child in object.values_mut() {
                replace_all_images(child, marker);
            }
        }
        _ => {}
    }
}

fn replace_image_object(object: &mut Map<String, Value>, marker: &str) {
    object.clear();
    object.insert("type".into(), Value::String("input_text".into()));
    object.insert("text".into(), Value::String(marker.into()));
}

#[derive(Clone, Copy)]
enum WireImageKind {
    Responses,
    Chat,
    Anthropic,
    Gemini,
    Kiro,
}

fn wire_image_kind(object: &Map<String, Value>) -> Option<WireImageKind> {
    if is_image_object(object) && inline_image_url(object).is_some() {
        return Some(
            if object.get("type").and_then(Value::as_str) == Some("image_url")
                && object.get("image_url").is_some_and(Value::is_object)
            {
                WireImageKind::Chat
            } else {
                WireImageKind::Responses
            },
        );
    }
    if object.get("type").and_then(Value::as_str) == Some("image")
        && object
            .get("source")
            .and_then(|source| source.get("type"))
            .and_then(Value::as_str)
            == Some("base64")
        && object
            .get("source")
            .and_then(|source| source.get("data"))
            .and_then(Value::as_str)
            .is_some()
    {
        return Some(WireImageKind::Anthropic);
    }
    if object
        .get("inlineData")
        .or_else(|| object.get("inline_data"))
        .and_then(|inline| inline.get("data"))
        .and_then(Value::as_str)
        .is_some()
    {
        return Some(WireImageKind::Gemini);
    }
    if object.get("format").and_then(Value::as_str).is_some()
        && object
            .get("source")
            .and_then(|source| source.get("bytes"))
            .and_then(Value::as_str)
            .is_some()
    {
        return Some(WireImageKind::Kiro);
    }
    None
}

fn replace_wire_image_object(
    object: &mut Map<String, Value>,
    kind: WireImageKind,
    marker: &str,
) {
    object.clear();
    match kind {
        WireImageKind::Responses => {
            object.insert("type".into(), Value::String("input_text".into()));
            object.insert("text".into(), Value::String(marker.into()));
        }
        WireImageKind::Chat => {
            object.insert("type".into(), Value::String("text".into()));
            object.insert("text".into(), Value::String(marker.into()));
        }
        WireImageKind::Anthropic => {
            object.insert("type".into(), Value::String("text".into()));
            object.insert("text".into(), Value::String(marker.into()));
        }
        WireImageKind::Gemini => {
            object.insert("text".into(), Value::String(marker.into()));
        }
        WireImageKind::Kiro => {
            // Mark the slot as omitted; callers scrub `images` arrays into message text.
            object.insert("type".into(), Value::String("omitted_image".into()));
            object.insert("text".into(), Value::String(marker.into()));
        }
    }
}

fn inline_image_base64_chars(object: &Map<String, Value>) -> Option<u64> {
    if let Some(chars) = wire_inline_image_base64_chars(object) {
        return Some(chars);
    }
    if !is_image_object(object) {
        return None;
    }
    let url = object
        .get("image_url")
        .and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("url").and_then(Value::as_str))
        })
        .or_else(|| object.get("url").and_then(Value::as_str))?;
    let (metadata, payload) = url.split_once(',')?;
    if !metadata.starts_with("data:image/") || !metadata.ends_with(";base64") || payload.is_empty() {
        return None;
    }
    if !is_base64_payload(payload) {
        return None;
    }
    Some(payload.len() as u64)
}

fn wire_inline_image_base64_chars(object: &Map<String, Value>) -> Option<u64> {
    match wire_image_kind(object)? {
        WireImageKind::Responses | WireImageKind::Chat => {
            let url = inline_image_url(object)?;
            let (metadata, payload) = url.split_once(',')?;
            if !metadata.starts_with("data:image/")
                || !metadata.ends_with(";base64")
                || payload.is_empty()
            {
                return None;
            }
            if !is_base64_payload(payload) {
                return None;
            }
            Some(payload.len() as u64)
        }
        WireImageKind::Anthropic => {
            let payload = object
                .get("source")
                .and_then(|source| source.get("data"))
                .and_then(Value::as_str)?;
            if payload.is_empty() || !is_base64_payload(payload) {
                return None;
            }
            Some(payload.len() as u64)
        }
        WireImageKind::Gemini => {
            let payload = object
                .get("inlineData")
                .or_else(|| object.get("inline_data"))
                .and_then(|inline| inline.get("data"))
                .and_then(Value::as_str)?;
            if payload.is_empty() || !is_base64_payload(payload) {
                return None;
            }
            Some(payload.len() as u64)
        }
        WireImageKind::Kiro => {
            let payload = object
                .get("source")
                .and_then(|source| source.get("bytes"))
                .and_then(Value::as_str)?;
            if payload.is_empty() || !is_base64_payload(payload) {
                return None;
            }
            Some(payload.len() as u64)
        }
    }
}

fn is_base64_payload(payload: &str) -> bool {
    payload
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
}

fn is_image_object(object: &Map<String, Value>) -> bool {
    matches!(
        object.get("type").and_then(Value::as_str),
        Some("input_image" | "image_url")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn image(chars: usize) -> Value {
        json!({
            "type": "input_image",
            "image_url": format!("data:image/jpeg;base64,{}", "A".repeat(chars))
        })
    }

    fn png_image_data_url(width: u32, height: u32) -> String {
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, width, height);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("PNG header");
            writer
                .write_image_data(&vec![0_u8; (width * height) as usize])
                .expect("PNG data");
        }
        format!("data:image/png;base64,{}", STANDARD.encode(encoded))
    }

    #[test]
    fn keeps_newest_images_and_omits_oldest_until_the_request_budget_fits() {
        let mut body = json!({
            "input": [{
                "type": "custom_tool_call_output",
                "output": [image(7 * 1024 * 1024), image(6 * 1024 * 1024), image(2 * 1024 * 1024)]
            }]
        });
        let report = normalize_translated_image_history_sync(
            &mut body,
            ProviderProtocol::ChatCompletions,
            ProviderTransport::Standard,
            16 * MIB,
        );
        assert_eq!(report.inline_images, 3);
        assert_eq!(report.omitted_images, 1);
        assert_eq!(body.pointer("/input/0/output/0/type").and_then(Value::as_str), Some("input_text"));
        assert_eq!(body.pointer("/input/0/output/2/type").and_then(Value::as_str), Some("input_image"));
    }

    #[test]
    fn enforces_anthropic_per_image_limit_before_aggregate_pruning() {
        let mut body = json!({"input": [{"content": [image(6 * 1024 * 1024), image(1024)]}]});
        let report = normalize_translated_image_history_sync(
            &mut body,
            ProviderProtocol::AnthropicMessages,
            ProviderTransport::Standard,
            32 * MIB,
        );
        assert_eq!(report.omitted_images, 1);
        assert!(body.pointer("/input/0/content/0/text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("per-image")));
    }

    #[test]
    fn leaves_under_budget_chat_images_at_original_fidelity() {
        let original_url = png_image_data_url(4_096, 1_024);
        let mut body = json!({"input": [{"content": [{
            "type": "input_image",
            "image_url": original_url
        }]}]});
        let original = body.pointer("/input/0/content/0/image_url").cloned();
        let report = normalize_translated_image_history_sync(
            &mut body,
            ProviderProtocol::ChatCompletions,
            ProviderTransport::Standard,
            64 * MIB,
        );
        assert_eq!(report.omitted_images, 0);
        assert_eq!(body.pointer("/input/0/content/0/image_url").cloned(), original);
    }

    #[test]
    fn resizes_dimension_overflow_even_when_encoded_png_is_under_byte_cap() {
        let original = png_image_data_url(4_096, 1_024);
        assert!(original.len() < 2 * 1024 * 1024);
        let mut body = json!({"input": [{"content": [
            image(15 * 1024 * 1024),
            {"type": "input_image", "image_url": original}
        ]}]});
        let report = normalize_translated_image_history_sync(
            &mut body,
            ProviderProtocol::ChatCompletions,
            ProviderTransport::Standard,
            16 * MIB,
        );
        assert_eq!(report.omitted_images, 1);
        let normalized = body
            .pointer("/input/0/content/1/image_url")
            .and_then(Value::as_str)
            .expect("normalized image URL");
        let payload = normalized.split_once(',').expect("data URL").1;
        let decoded = STANDARD.decode(payload).expect("base64");
        let reader = png::Decoder::new(Cursor::new(decoded))
            .read_info()
            .expect("PNG info");
        assert!(reader.info().width.max(reader.info().height) <= 2_000);
    }

    #[test]
    fn omits_pngs_that_exceed_the_safe_decode_pixel_limit() {
        let oversized = png_image_data_url(4_000, 2_501);
        let mut body = json!({"input": [{"content": [{
            "type": "input_image",
            "image_url": oversized
        }]}]});
        let report = normalize_translated_image_history_sync(
            &mut body,
            ProviderProtocol::ChatCompletions,
            ProviderTransport::Standard,
            64 * MIB,
        );
        assert_eq!(report.omitted_images, 1);
        assert_eq!(
            body.pointer("/input/0/content/0/text").and_then(Value::as_str),
            Some(OMITTED_UNSAFE_PNG)
        );
    }

    #[test]
    fn omits_images_when_a_native_transport_request_limit_is_below_one_mibibyte() {
        let mut body = json!({"input": [{"content": [image(128 * 1024)]}]});
        let report = normalize_translated_image_history_sync(
            &mut body,
            ProviderProtocol::ChatCompletions,
            ProviderTransport::Kiro,
            64 * 1024,
        );
        assert_eq!(report.omitted_images, 1);
        assert_eq!(
            body.pointer("/input/0/content/0/type").and_then(Value::as_str),
            Some("input_text")
        );
    }

    #[test]
    fn compaction_strips_remote_and_inline_images() {
        let mut body = json!({
            "input": [{
                "content": [
                    image(16),
                    {"type": "image_url", "image_url": {"url": "https://example.test/image.png"}}
                ]
            }]
        });
        strip_translated_input_images_for_compaction(&mut body);
        assert_eq!(body.pointer("/input/0/content/0/text").and_then(Value::as_str), Some(OMITTED_FOR_COMPACTION));
        assert_eq!(body.pointer("/input/0/content/1/text").and_then(Value::as_str), Some(OMITTED_FOR_COMPACTION));
    }

    #[test]
    fn enforces_anthropic_wire_base64_budget_after_translation_shape() {
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/jpeg",
                            "data": "A".repeat(6 * 1024 * 1024)
                        }
                    },
                    {
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/jpeg",
                            "data": "B".repeat(1024)
                        }
                    }
                ]
            }]
        });
        let report = normalize_translated_image_history_sync(
            &mut body,
            ProviderProtocol::AnthropicMessages,
            ProviderTransport::Standard,
            32 * MIB,
        );
        assert_eq!(report.inline_images, 2);
        assert_eq!(report.omitted_images, 1);
        assert_eq!(
            body.pointer("/messages/0/content/0/type").and_then(Value::as_str),
            Some("text")
        );
        assert!(body
            .pointer("/messages/0/content/0/text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("per-image")));
        assert_eq!(
            body.pointer("/messages/0/content/1/type").and_then(Value::as_str),
            Some("image")
        );
    }

    #[test]
    fn enforces_gemini_inline_data_total_budget() {
        let mut body = json!({
            "contents": [{
                "parts": [
                    {"inlineData": {"mimeType": "image/jpeg", "data": "A".repeat(12 * 1024 * 1024)}},
                    {"inlineData": {"mimeType": "image/jpeg", "data": "B".repeat(12 * 1024 * 1024)}},
                    {"inlineData": {"mimeType": "image/jpeg", "data": "C".repeat(1024)}}
                ]
            }]
        });
        let report = normalize_translated_image_history_sync(
            &mut body,
            ProviderProtocol::GeminiGenerateContent,
            ProviderTransport::Standard,
            64 * MIB,
        );
        assert_eq!(report.inline_images, 3);
        assert!(report.omitted_images >= 1);
        assert_eq!(
            body.pointer("/contents/0/parts/2/inlineData/data")
                .and_then(Value::as_str)
                .map(|s| s.len()),
            Some(1024)
        );
    }

    #[test]
    fn admission_rewrite_keeps_newest_images_and_records_source_paths() {
        let mut body = json!({
            "model": "xai/grok-4.6",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_old",
                    "name": "view_image",
                    "arguments": "{\"path\":\"/Volumes/D/Project/review/old.png\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_old",
                    "output": [image(64 * 1024)]
                },
                {
                    "type": "function_call",
                    "call_id": "call_new",
                    "name": "view_image",
                    "arguments": "{\"path\":\"/Volumes/D/Project/review/new.png\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_new",
                    "output": [image(1024)]
                }
            ]
        });
        let report = rewrite_oversized_request_images_for_admission(&mut body, 40 * 1024);
        assert_eq!(report.inline_images, 2);
        assert_eq!(report.omitted_images, 1);
        assert_eq!(
            body.pointer("/input/1/output/0/type").and_then(Value::as_str),
            Some("input_text")
        );
        assert_eq!(
            body.pointer("/input/1/output/0/text").and_then(Value::as_str),
            Some("[image omitted: older image data exceeded the gateway request body limit]\npath: /Volumes/D/Project/review/old.png")
        );
        assert_eq!(
            body.pointer("/input/3/output/0/type").and_then(Value::as_str),
            Some("input_image")
        );
    }

    #[test]
    fn admission_byte_shrink_rewrites_json_over_the_body_limit() {
        let body = json!({
            "input": [{
                "content": [image(8 * 1024), image(8 * 1024), image(8 * 1024), image(256)]
            }]
        });
        let bytes = serde_json::to_vec(&body).expect("request bytes");
        let (rewritten, report) =
            shrink_admitted_request_bytes(&bytes, 12 * 1024).expect("oversized request");
        assert!(report.omitted_images >= 1);
        assert!(rewritten.len() < bytes.len());
        assert!((rewritten.len() as u64) <= 12 * 1024);
        let parsed = serde_json::from_slice::<Value>(&rewritten).expect("rewritten json");
        assert_eq!(
            parsed.pointer("/input/0/content/3/type").and_then(Value::as_str),
            Some("input_image")
        );
    }

    #[test]
    fn admission_rewrite_reads_string_encoded_tool_output_images() {
        let mut body = json!({
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_old",
                    "name": "view_image",
                    "arguments": "{\"path\":\"/tmp/old.png\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_old",
                    "output": serde_json::to_string(&json!([image(64 * 1024)])).expect("encoded output")
                },
                {
                    "type": "function_call",
                    "call_id": "call_new",
                    "name": "view_image",
                    "arguments": "{\"path\":\"/tmp/new.png\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_new",
                    "output": serde_json::to_string(&json!([image(1024)])).expect("encoded output")
                }
            ]
        });
        let report = rewrite_oversized_request_images_for_admission(&mut body, 40 * 1024);
        assert_eq!(report.inline_images, 2);
        assert_eq!(report.omitted_images, 1);
        let old_output = body.pointer("/input/1/output").and_then(Value::as_str).expect("string output");
        let parsed = serde_json::from_str::<Value>(old_output).expect("rewritten output");
        assert_eq!(parsed[0]["type"], "input_text");
        assert!(parsed[0]["text"].as_str().unwrap().contains("path: /tmp/old.png"));
        let new_output = body.pointer("/input/3/output").and_then(Value::as_str).expect("new string output");
        assert!(new_output.contains("data:image/jpeg;base64,"));
    }

    #[test]
    fn admission_rewrite_reads_adjacent_caption_paths() {
        let mut body = json!({
            "input": [{
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "<image name=[Image #1] path=\"/tmp/codex-remote-attachments/photo.jpg\">"},
                    {"type": "input_image", "image_url": format!("data:image/jpeg;base64,{}", "A".repeat(64 * 1024)), "detail": "high"},
                    {"type": "input_text", "text": "</image>"},
                    {"type": "input_image", "path": "/Volumes/D/Project/review/new.png", "image_url": format!("data:image/jpeg;base64,{}", "B".repeat(1024))}
                ]
            }]
        });
        let report = rewrite_oversized_request_images_for_admission(&mut body, 40 * 1024);
        assert_eq!(report.inline_images, 2);
        assert_eq!(report.omitted_images, 1);
        assert_eq!(
            body.pointer("/input/0/content/1/type").and_then(Value::as_str),
            Some("input_text")
        );
        assert!(body
            .pointer("/input/0/content/1/text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("path: /tmp/codex-remote-attachments/photo.jpg")));
        assert_eq!(
            body.pointer("/input/0/content/3/type").and_then(Value::as_str),
            Some("input_image")
        );
    }

    #[test]
    fn admission_rewrite_omits_the_minimum_prefix_across_many_images() {
        let mut content = Vec::new();
        for _ in 0..20 {
            content.push(image(4 * 1024));
        }
        content.push(image(256));
        let mut body = json!({ "input": [{ "content": content }] });
        let report = rewrite_oversized_request_images_for_admission(&mut body, 20 * 1024);
        assert_eq!(report.inline_images, 21);
        assert!(report.omitted_images >= 1);
        assert!(report.omitted_images < 21);
        assert_eq!(
            body.pointer("/input/0/content/20/type").and_then(Value::as_str),
            Some("input_image")
        );
        assert!(estimate_json_request_bytes(&body) <= 20 * 1024);
    }
}

