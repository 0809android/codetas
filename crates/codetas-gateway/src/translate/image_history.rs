use crate::config::{ProviderProtocol, ProviderTransport};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{Map, Value};
use std::{
    collections::BTreeSet,
    io::Cursor,
    sync::{Arc, OnceLock},
};
use tokio::sync::Semaphore;

#[cfg(test)]
mod cache_stability_tests {
    use super::*;
    use serde_json::json;

    fn screenshot() -> Value {
        // Synthetic pixels only; no prompts, credentials, or user images.
        let mut state = 0x1234_5678_u32;
        let pixels = (0..1200 * 800 * 3)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect::<Vec<_>>();
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 1200, 800);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&pixels)
                .unwrap();
        }
        json!({"type": "input_image", "image_url": format!(
            "data:image/png;base64,{}", STANDARD.encode(bytes)
        )})
    }

    fn compressible_screenshot() -> Value {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2_400, 800);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&vec![127; 2_400 * 800 * 3])
                .unwrap();
        }
        json!({"type": "input_image", "image_url": format!(
            "data:image/png;base64,{}", STANDARD.encode(bytes)
        )})
    }

    fn provider_cases() -> [(ProviderProtocol, ProviderTransport); 5] {
        [
            (
                ProviderProtocol::AnthropicMessages,
                ProviderTransport::Standard,
            ),
            (
                ProviderProtocol::GeminiGenerateContent,
                ProviderTransport::Standard,
            ),
            (
                ProviderProtocol::ChatCompletions,
                ProviderTransport::Standard,
            ),
            (ProviderProtocol::Responses, ProviderTransport::Standard),
            (ProviderProtocol::ChatCompletions, ProviderTransport::Kiro),
        ]
    }

    #[tokio::test]
    async fn aggregate_image_budget_requires_compaction_without_pruning_for_all_providers() {
        let image = json!({"type": "input_image", "image_url": format!(
            "data:image/jpeg;base64,{}", "A".repeat(400 * 1024)
        )});
        for (protocol, transport) in provider_cases() {
            let mut body = json!({"input": [{"role": "user", "content": vec![image.clone(); 7]}]});
            normalize_translated_image_history(&mut body, protocol, transport, 4 * MIB)
                .await
                .expect("initial history fits");
            body["input"][0]["content"]
                .as_array_mut()
                .unwrap()
                .extend([image.clone(), image.clone()]);
            let expected = body.clone();
            for _ in 0..2 {
                let result =
                    normalize_translated_image_history(&mut body, protocol, transport, 4 * MIB)
                        .await;
                assert!(
                    matches!(result, Err(ImageHistoryError::BudgetExceeded { .. })),
                    "oversized image history must require compaction: {protocol:?}/{transport:?}"
                );
                assert!(
                    body == expected,
                    "budget handling pruned cached history: {protocol:?}/{transport:?}"
                );
            }
            // An explicit caller-created compaction boundary starts a new,
            // bounded history; rejection must not leave a sticky blocked state.
            let mut compacted = json!({"input": [
                {"role": "user", "content": "checkpoint; keep the latest question"},
                {"role": "user", "content": [image.clone()]}
            ]});
            normalize_translated_image_history(&mut compacted, protocol, transport, 4 * MIB)
                .await
                .expect("explicitly compacted history fits again");
        }
    }

    #[tokio::test]
    async fn final_wire_budget_never_prunes_any_provider_history() {
        let data = "A".repeat(16 * 1024);
        let image_url = format!("data:image/jpeg;base64,{data}");
        let bodies = [
            json!({"input": [{"content": [{"type": "input_image", "image_url": image_url}]}]}),
            json!({"messages": [{"content": [{"type": "image_url", "image_url": {"url": image_url}}]}]}),
            json!({"messages": [{"content": [{"type": "image", "source": {
                "type": "base64", "media_type": "image/jpeg", "data": data}}]}]}),
            json!({"contents": [{"parts": [{"inlineData": {"mimeType": "image/jpeg", "data": data}}]}]}),
            json!({"request": {"contents": [{"parts": [{"inlineData": {
                "mimeType": "image/jpeg", "data": data}}]}]}}),
            json!({"conversationState": {"history": [{"userInputMessage": {
                "content": "keep the original screenshots", "images": [{
                    "format": "jpeg", "source": {"bytes": data}}]}}]}}),
        ];
        for mut body in bodies {
            let expected = body.clone();
            assert!(matches!(
                serialize_preserving_image_history_async(&mut body, 8 * 1024).await,
                Err(ImageHistoryError::BudgetExceeded { .. })
            ));
            assert!(
                body == expected,
                "wire-size fitting changed existing history"
            );
            let bytes = serialize_preserving_image_history_async(&mut body, 32 * 1024)
                .await
                .expect("larger budget accepts the exact history");
            assert!(bytes == serde_json::to_vec(&expected).unwrap());
        }
    }

    #[test]
    fn all_protocols_keep_existing_png_bytes_when_images_age() {
        let first = compressible_screenshot();
        let filler = json!({"type": "input_image", "image_url": format!(
            "data:image/jpeg;base64,{}", "A".repeat(1024)
        )});
        for (protocol, transport) in provider_cases() {
            let normalize = |count| {
                let mut content = vec![filler.clone(); count];
                content[0] = first.clone();
                let mut body = json!({"input": [{"content": content}]});
                let report = normalize_translated_image_history_sync(
                    &mut body,
                    protocol,
                    transport,
                    64 * MIB,
                );
                assert_eq!(report.omitted_images, 0);
                body["input"][0]["content"][0].clone()
            };
            let original = normalize(1);
            for count in [2, 6, 7, 20, 21] {
                assert!(
                    original == normalize(count),
                    "aging rewrote a cached PNG: {protocol:?}/{transport:?}, count={count}"
                );
            }
        }
    }

    #[test]
    fn all_protocols_keep_existing_non_png_images_when_images_age() {
        let first = json!({"type": "input_image", "image_url": format!(
            "data:image/jpeg;base64,{}", "A".repeat(400 * 1024)
        )});
        let filler = json!({"type": "input_image", "image_url": format!(
            "data:image/jpeg;base64,{}", "A".repeat(1024)
        )});
        for (protocol, transport) in provider_cases() {
            for count in [1, 7, 21] {
                let mut content = vec![filler.clone(); count];
                content[0] = first.clone();
                let mut body = json!({"input": [{"content": content}]});
                let report = normalize_translated_image_history_sync(
                    &mut body,
                    protocol,
                    transport,
                    64 * MIB,
                );
                assert_eq!(
                    report.omitted_images, 0,
                    "aging omitted a valid non-PNG: {protocol:?}/{transport:?}, count={count}"
                );
                assert!(
                    body["input"][0]["content"][0] == first,
                    "aging rewrote a non-PNG: {protocol:?}/{transport:?}"
                );
            }
        }
    }

    #[test]
    fn aggregate_budget_does_not_switch_gemini_responses_or_kiro_png_encoding() {
        let image = screenshot();
        for (protocol, transport, limit) in [
            (
                ProviderProtocol::GeminiGenerateContent,
                ProviderTransport::Standard,
                64 * MIB,
            ),
            (
                ProviderProtocol::Responses,
                ProviderTransport::Standard,
                16 * MIB,
            ),
            (
                ProviderProtocol::ChatCompletions,
                ProviderTransport::Kiro,
                64 * MIB,
            ),
        ] {
            let normalize = |count| {
                let mut body = json!({"input": [{"content": vec![image.clone(); count]}]});
                let report =
                    normalize_translated_image_history_sync(&mut body, protocol, transport, limit);
                assert_eq!(report.omitted_images, 0);
                body["input"][0]["content"][0].clone()
            };
            assert!(
                normalize(1) == normalize(6),
                "aggregate pressure rewrote a PNG: {protocol:?}/{transport:?}"
            );
        }
    }

    #[test]
    fn all_protocols_keep_normalized_source_and_wire_images_stable() {
        let image = compressible_screenshot();
        for (protocol, transport) in provider_cases() {
            let normalize = |body: &mut Value| {
                let report =
                    normalize_translated_image_history_sync(body, protocol, transport, 64 * MIB);
                assert_eq!(report.omitted_images, 0);
            };
            let mut source = json!({"input": [{"content": [image.clone()]}]});
            normalize(&mut source);
            let expected_source = source.clone();
            normalize(&mut source);
            assert!(
                source == expected_source,
                "source replay rewrote pixels: {protocol:?}/{transport:?}"
            );

            let url = source["input"][0]["content"][0]["image_url"]
                .as_str()
                .unwrap();
            let payload = url.split_once(',').unwrap().1;
            let native = if transport == ProviderTransport::Kiro {
                json!({"format": "png", "source": {"bytes": payload}})
            } else {
                match protocol {
                    ProviderProtocol::AnthropicMessages => json!({"type": "image",
                        "source": {"type": "base64", "media_type": "image/png", "data": payload}}),
                    ProviderProtocol::GeminiGenerateContent => json!({
                        "inlineData": {"mimeType": "image/png", "data": payload}}),
                    ProviderProtocol::ChatCompletions => json!({"type": "image_url",
                        "image_url": {"url": url}}),
                    ProviderProtocol::Responses => source["input"][0]["content"][0].clone(),
                }
            };
            let mut wire = match (protocol, transport) {
                (_, ProviderTransport::Kiro) => json!({"conversationState": {
                    "currentMessage": {"userInputMessage": {"images": [native.clone()]}}}}),
                (ProviderProtocol::GeminiGenerateContent, _) => json!({
                    "contents": [{"role": "user", "parts": [native.clone()]}]}),
                (ProviderProtocol::Responses, _) => json!({
                    "input": [{"role": "user", "content": [native.clone()]}]}),
                _ => json!({"messages": [{"role": "user", "content": [native.clone()]}]}),
            };
            let expected_wire = wire.clone();
            normalize(&mut wire);
            normalize(&mut wire);
            let (bytes, omitted) = serialize_with_image_budget(&mut wire, 64 * MIB).unwrap();
            assert_eq!(omitted, 0);
            assert!(
                wire == expected_wire,
                "wire replay rewrote pixels: {protocol:?}/{transport:?}"
            );
            assert!(
                bytes == serde_json::to_vec(&expected_wire).unwrap(),
                "wire serialization changed the cached prefix: {protocol:?}/{transport:?}"
            );
        }
    }

    #[test]
    fn anthropic_append_does_not_reencode_an_existing_screenshot() {
        let image = screenshot();
        let normalize = |count: usize, request_limit| {
            let mut body = json!({"input": [{"type": "message", "role": "user",
                "content": vec![image.clone(); count]}]});
            let report = normalize_translated_image_history_sync(
                &mut body,
                ProviderProtocol::AnthropicMessages,
                ProviderTransport::Standard,
                request_limit,
            );
            assert_eq!(report.omitted_images, 0);
            body["input"][0]["content"][0].clone()
        };
        // Exercise both the installed 16 MiB request budget and the Anthropic
        // aggregate-image budget reached by larger configured requests.
        for request_limit in [16 * MIB, 64 * MIB] {
            let original = normalize(1, request_limit);
            for count in [2, 3, 5, 6] {
                let appended = normalize(count, request_limit);
                // Never print a Base64 payload on assertion failure.
                assert!(original == appended,
                    "appending screenshots changed an already-sent image at count={count}, limit={request_limit}");
            }
        }
    }

    #[test]
    fn anthropic_image_normalization_is_stable_across_replay_and_wire_translation() {
        let mut body = json!({"input": [{"type": "message", "role": "user",
            "content": [screenshot()]}]});
        let normalize = |body: &mut Value| {
            normalize_translated_image_history_sync(
                body,
                ProviderProtocol::AnthropicMessages,
                ProviderTransport::Standard,
                16 * MIB,
            )
        };
        assert_eq!(normalize(&mut body).omitted_images, 0);
        let normalized = body.clone();
        assert_eq!(normalize(&mut body).omitted_images, 0);
        assert!(
            body == normalized,
            "replaying normalized source changed its pixels"
        );

        let mut wire =
            crate::anthropic::responses_to_anthropic_with_oauth(&body, "claude-sonnet-5-5", true)
                .unwrap();
        let expected = wire.clone();
        assert_eq!(normalize(&mut wire).omitted_images, 0);
        assert!(
            wire == expected,
            "wire normalization re-encoded the source image"
        );
    }
}

const MIB: u64 = 1024 * 1024;
const ANTHROPIC_MAX_IMAGE_BASE64_CHARS: u64 = 5 * MIB;
const ANTHROPIC_TOTAL_IMAGE_BASE64_CHARS: u64 = 20 * MIB;
const GEMINI_TOTAL_INLINE_BASE64_CHARS: u64 = 20 * MIB;
const KIRO_TOTAL_IMAGE_BASE64_CHARS: u64 = 18 * MIB;
const CHAT_COMPAT_MAX_IMAGE_BASE64_CHARS: u64 = 2 * MIB;
const CHAT_COMPAT_TOTAL_IMAGE_BASE64_CHARS: u64 = 3 * MIB;
const MAX_NORMALIZABLE_IMAGE_BASE64_CHARS: usize = 64 * 1024 * 1024;
// Keep each decoded RGBA frame below roughly 40 MiB while still accepting a
// 4K screenshot. Two workers may run at once, so decoded frames stay near
// 80 MiB before encoder buffers.
const MAX_NORMALIZABLE_PIXELS: u64 = 10_000_000;
const IMAGE_NORMALIZATION_CONCURRENCY: usize = 2;
const OMITTED_FOR_BUDGET: &str =
    "[image omitted: older image data exceeded the provider request budget]";
const OMITTED_PER_IMAGE: &str = "[image omitted: image data exceeded the provider per-image limit]";
const OMITTED_UNSAFE_PNG: &str =
    "[image omitted: PNG could not be normalized within the safety limits]";
const OMITTED_CHAT_COMPAT_IMAGE: &str =
    "[image omitted: image data exceeded the Chat Completions inline-image size tier]";
const OMITTED_FOR_COMPACTION: &str = "[image omitted for compaction]";
const OMITTED_FOR_ADMISSION: &str =
    "[image omitted: older image data exceeded the gateway request body limit]";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImageHistoryReport {
    pub inline_images: usize,
    pub omitted_images: usize,
    pub kept_base64_chars: u64,
    pub requires_compaction: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ImageHistoryError {
    #[error("an image exceeds a per-image safety limit; compact explicitly before continuing (history was not pruned)")]
    OmissionRequired,
    #[error("image history requires {required} bytes but the request budget is {limit} bytes; compact the conversation before continuing (history was not pruned)")]
    BudgetExceeded { required: u64, limit: u64 },
    #[error("{0}")]
    Worker(String),
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
                ProviderProtocol::ChatCompletions => CHAT_COMPAT_TOTAL_IMAGE_BASE64_CHARS,
                ProviderProtocol::Responses => u64::MAX,
            }
        };
        let max_image_base64_chars = match protocol {
            ProviderProtocol::AnthropicMessages => ANTHROPIC_MAX_IMAGE_BASE64_CHARS,
            ProviderProtocol::ChatCompletions => CHAT_COMPAT_MAX_IMAGE_BASE64_CHARS,
            ProviderProtocol::GeminiGenerateContent | ProviderProtocol::Responses => u64::MAX,
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
) -> Result<ImageHistoryReport, ImageHistoryError> {
    normalize_image_history_with_policy(body,protocol,transport,max_request_bytes,false).await
}
pub async fn normalize_translated_image_history_for_compaction(body:&mut Value,protocol:ProviderProtocol,transport:ProviderTransport,max_request_bytes:u64)->Result<ImageHistoryReport,ImageHistoryError> {
    normalize_image_history_with_policy(body,protocol,transport,max_request_bytes,true).await
}
async fn normalize_image_history_with_policy(body:&mut Value,protocol:ProviderProtocol,transport:ProviderTransport,max_request_bytes:u64,allow_omission:bool)->Result<ImageHistoryReport,ImageHistoryError> {
    // This probe must not validate or scan the Base64 payload. Requests can
    // contain hundreds of MiB of image text, and that work belongs on the
    // blocking worker below rather than a Tokio executor thread.
    if !contains_inline_image_candidate(body) {
        if protocol == ProviderProtocol::ChatCompletions {
            sanitize_chat_compat_image_detail(body);
        }
        return Ok(ImageHistoryReport::default());
    }
    let permit = IMAGE_NORMALIZATION_SLOTS
        .get_or_init(|| Arc::new(Semaphore::new(IMAGE_NORMALIZATION_CONCURRENCY)))
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| {
            ImageHistoryError::Worker("image normalization worker pool is unavailable".into())
        })?;
    let owned_body = std::mem::take(body);
    let (normalized_body, report) = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut normalized_body = owned_body;
        let mut report = normalize_translated_image_history_sync_with_policy(
            &mut normalized_body,
            protocol,
            transport,
            max_request_bytes,
            allow_omission,
        );
        if allow_omission {
            let policy=ImageHistoryPolicy::for_provider(protocol,transport,max_request_bytes);
            while report.kept_base64_chars>policy.max_total_base64_chars && omit_oldest_wire_history_image(&mut normalized_body) {
                report.omitted_images+=1;let mut images=Vec::new();collect_inline_images(&normalized_body,&mut images);
                report.kept_base64_chars=images.iter().copied().sum();
            }
            report.requires_compaction=report.kept_base64_chars>policy.max_total_base64_chars;
        } else if report.omitted_images>0 {report.requires_compaction=true;}
        (normalized_body,report)
    })
    .await
    .map_err(|error| {
        ImageHistoryError::Worker(format!("image normalization worker failed: {error}"))
    })?;
    *body = normalized_body;
    if report.requires_compaction {
        if !allow_omission && report.omitted_images>0 {return Err(ImageHistoryError::OmissionRequired);}
        return Err(ImageHistoryError::BudgetExceeded {
            required: report.kept_base64_chars,
            limit: ImageHistoryPolicy::for_provider(protocol, transport, max_request_bytes)
                .max_total_base64_chars,
        });
    }
    Ok(report)
}

#[cfg(test)]
fn normalize_translated_image_history_sync(
    body: &mut Value,
    protocol: ProviderProtocol,
    transport: ProviderTransport,
    max_request_bytes: u64,
) -> ImageHistoryReport {
    normalize_translated_image_history_sync_with_policy(body,protocol,transport,max_request_bytes,true)
}
fn normalize_translated_image_history_sync_with_policy(body:&mut Value,protocol:ProviderProtocol,transport:ProviderTransport,max_request_bytes:u64,allow_omission:bool)->ImageHistoryReport {
    let policy = ImageHistoryPolicy::for_provider(protocol, transport, max_request_bytes);
    if protocol == ProviderProtocol::ChatCompletions {
        sanitize_chat_compat_image_detail(body);
    }
    let mut images = Vec::new();
    collect_inline_images(body, &mut images);
    if images.is_empty() {
        return ImageHistoryReport::default();
    }
    let original_image_count = images.len();
    let chat_compat = protocol == ProviderProtocol::ChatCompletions;
    // Every route needs a stable prompt prefix, not just Anthropic. Normalize
    // each image from its first send using fixed per-image limits. Neither the
    // aggregate byte budget nor a screenshot's age may change its encoding;
    // aggregate limits are enforced separately by omission below.
    let mut unsafe_png_omissions = 0_usize;
    normalize_pngs_for_stable_history(body, chat_compat, &mut unsafe_png_omissions,allow_omission);
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

    // A rolling oldest-image deletion changes an already cached prefix on
    // every appended screenshot. Normal inference must instead stop before
    // sending and let the caller establish an explicit compaction boundary.
    let requires_compaction = kept_total > policy.max_total_base64_chars || (!allow_omission && (!omitted.is_empty() || unsafe_png_omissions>0));

    let mut image_index = 0_usize;
    if allow_omission {replace_selected_images(
        body,
        &omitted,
        &mut image_index,
        policy.max_image_base64_chars,
    );}
    ImageHistoryReport {
        inline_images: original_image_count,
        omitted_images: omitted.len().saturating_add(unsafe_png_omissions),
        kept_base64_chars: kept_total,
        requires_compaction,
    }
}

/// Normal inference must preserve its already-sent history. Unlike the explicit
/// compact-trigger serializer below, this never substitutes image placeholders.
pub fn serialize_preserving_image_history(
    body: &Value,
    max_request_bytes: u64,
) -> Result<Vec<u8>, ImageHistoryError> {
    let bytes = serde_json::to_vec(body)
        .map_err(|error| ImageHistoryError::Worker(format!("request encoding failed: {error}")))?;
    if bytes.len() as u64 > max_request_bytes {
        return Err(ImageHistoryError::BudgetExceeded {
            required: bytes.len() as u64,
            limit: max_request_bytes,
        });
    }
    Ok(bytes)
}

pub async fn serialize_preserving_image_history_async(
    body: &mut Value,
    max_request_bytes: u64,
) -> Result<Vec<u8>, ImageHistoryError> {
    let permit = IMAGE_NORMALIZATION_SLOTS
        .get_or_init(|| Arc::new(Semaphore::new(IMAGE_NORMALIZATION_CONCURRENCY)))
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| {
            ImageHistoryError::Worker("image encoding worker pool is unavailable".into())
        })?;
    let owned = std::mem::take(body);
    let (owned, result) = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let result = serialize_preserving_image_history(&owned, max_request_bytes);
        (owned, result)
    })
    .await
    .map_err(|error| ImageHistoryError::Worker(format!("image encoding worker failed: {error}")))?;
    *body = owned;
    result
}

/// Remove exactly one oldest inline image. The sender uses this after wire
/// translation when JSON framing or tool schemas still exceed maxRequestBytes.
pub fn omit_oldest_translated_input_image(body: &mut Value) -> bool {
    omit_first_image(body)
}

/// Keep potentially large JSON encoding and image rewriting off async I/O
/// workers. Share the image-normalization limit so concurrent requests cannot
/// create an unbounded number of memory-heavy blocking jobs.
pub async fn serialize_with_image_budget_async(
    body: &mut Value,
    max_request_bytes: u64,
) -> Result<(Vec<u8>, usize), String> {
    let permit = IMAGE_NORMALIZATION_SLOTS
        .get_or_init(|| Arc::new(Semaphore::new(IMAGE_NORMALIZATION_CONCURRENCY)))
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| "image budget worker pool is unavailable".to_string())?;
    let mut owned_body = std::mem::take(body);
    let (rewritten, result) = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let result = serialize_with_image_budget(&mut owned_body, max_request_bytes);
        (owned_body, result)
    })
    .await
    .map_err(|error| format!("image budget worker failed: {error}"))?;
    *body = rewritten;
    result.map_err(|error| format!("image budget encoding failed: {error}"))
}

/// Fit the final wire body by replacing oldest inline images only when needed.
/// Preserve text, non-image tool results, and compaction control items. The caller must
/// still reject an oversized result when images alone cannot make it fit.
pub fn serialize_with_image_budget(
    body: &mut Value,
    max_request_bytes: u64,
) -> Result<(Vec<u8>, usize), serde_json::Error> {
    let mut serialized = serde_json::to_vec(body)?;
    let mut omitted = 0;
    while serialized.len() as u64 > max_request_bytes && omit_oldest_wire_history_image(body) {
        omitted += 1;
        serialized = serde_json::to_vec(body)?;
    }
    Ok((serialized, omitted))
}

// Walk only the history slots of supported outgoing protocols. Traversing the
// whole request could corrupt tool-schema examples, metadata, or other options
// that merely resemble an image block. Cloud Code Assist nests Gemini's body.
fn omit_oldest_wire_history_image(body: &mut Value) -> bool {
    for path in ["/input", "/messages", "/contents", "/request/contents"] {
        if let Some(history) = body.pointer_mut(path) {
            if history.is_array() && omit_oldest_translated_input_image(history) {
                return true;
            }
        }
    }
    false
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

fn estimated_omit_count(original_bytes: u64, images: &[InlineImageRef], body_limit: u64) -> usize {
    if original_bytes <= body_limit {
        return 0;
    }
    let mut saved = 0_u64;
    for (index, image) in images.iter().enumerate() {
        let marker_bytes =
            (admission_image_marker(image.source_path.as_deref()).len() as u64).saturating_add(24);
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

/// Admission recovery is permitted only at an explicit compaction boundary.
/// Ordinary inference bytes are never rewritten by this helper.
pub fn shrink_explicit_compaction_request_bytes(
    bytes: &[u8], body_limit: u64, standalone_route: bool,
) -> Option<(Vec<u8>, AdmissionImageRewriteReport)> {
    let mut body: Value = serde_json::from_slice(bytes).ok()?;
    if !standalone_route && !crate::compaction::request_is_remote_compaction(&body) { return None; }
    let report = rewrite_oversized_request_images_for_admission(&mut body, body_limit);
    if report.omitted_images == 0 { return None; }
    body[crate::anthropic_compaction::HISTORY_REWRITTEN] = Value::Bool(true);
    let encoded = serde_json::to_vec(&body).ok()?;
    (encoded.len() as u64 <= body_limit).then_some((encoded, report))
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
                content.push_str(
                    "

",
                );
                content.push_str(&marker);
            }
            _ => {
                message.insert("content".into(), Value::String(marker));
            }
        }
    }
}

// Reference-only leaves belong to image/token accounting, not inline byte budgets.
pub(super) fn is_reference_image_leaf(value: &Value) -> bool {
    if value.get("type").and_then(Value::as_str) == Some("input_image")
        && value.get("file_id").and_then(Value::as_str).is_some_and(|id| !id.is_empty()) {
        return true;
    }
    if value.get("type").and_then(Value::as_str) == Some("image")
        && value.pointer("/source/type").and_then(Value::as_str) == Some("file")
        && value.pointer("/source/file_id").and_then(Value::as_str).is_some_and(|id| !id.is_empty()) {
        return true;
    }
    if value.get("type").is_some() { return false; }
    value.get("fileData").or_else(|| value.get("file_data")).is_some_and(|data| {
        data.get("mimeType").or_else(|| data.get("mime_type")).and_then(Value::as_str)
            .is_none_or(|mime| mime.starts_with("image/"))
            && data.get("fileUri").or_else(|| data.get("file_uri")).and_then(Value::as_str)
                .is_some_and(|uri| !uri.is_empty())
    })
}

pub fn count_translated_input_images(body:&Value)->usize {
    super::image_scope::rendered_image_nodes(body).into_iter().filter(|node|node.as_object().is_some_and(|object|match wire_image_kind(object) {
        Some(WireImageKind::Responses|WireImageKind::Chat)=>inline_image_url(object).is_some_and(|url|url.starts_with("data:image/")),
        Some(WireImageKind::Anthropic|WireImageKind::Gemini|WireImageKind::Kiro)=>true,None=>false,
    })).count()
}

pub(super) fn is_scoped_image_leaf(value:&Value)->bool {
    let Some(object)=value.as_object() else{return false;};
    if object.get("type").and_then(Value::as_str).is_some_and(|kind| !matches!(kind,"input_image"|"image_url"|"image")) { return false; }
    if object.get("type").and_then(Value::as_str)==Some("image") && object.get("source").and_then(|source|source.get("type")).and_then(Value::as_str)==Some("url") {return true;}
    if object.get("fileData").or_else(||object.get("file_data")).and_then(|data|data.get("mimeType").or_else(||data.get("mime_type"))).and_then(Value::as_str).is_some_and(|mime|mime.starts_with("image/")) {return true;}
    if matches!(object.get("type").and_then(Value::as_str),Some("message"|"function_call"|"custom_tool_call"|"tool_search_call"|"tool_use"|"tool_result"|"additional_tools"|"reasoning"|"thinking"|"redacted_thinking"|"compaction"|"compaction_summary"|"context_compaction")) {return false;}
    match wire_image_kind(object) {
        Some(WireImageKind::Gemini)=>object.get("inlineData").or_else(||object.get("inline_data"))
            .and_then(|inline|inline.get("mimeType").or_else(||inline.get("mime_type"))).and_then(Value::as_str).is_none_or(|mime|mime.starts_with("image/")),
        Some(_)=>true,None=>false,
    }
}

pub fn strip_translated_input_images_for_compaction(body: &mut Value) {
    replace_all_images(body, OMITTED_FOR_COMPACTION);
}

fn sanitize_chat_compat_image_detail(value:&mut Value) {
    for node in super::image_scope::rendered_image_nodes_mut(value) {
        if let Some(object)=node.as_object_mut() {remap_chat_compat_detail_field(object);
            if let Some(Value::Object(url))=object.get_mut("image_url") {remap_chat_compat_detail_field(url);}
        }
    }
}

fn remap_chat_compat_detail_field(object: &mut Map<String, Value>) {
    if object.get("detail").and_then(Value::as_str) == Some("original") {
        object.insert("detail".into(), Value::String("high".into()));
    }
}

fn collect_inline_images(value:&Value,images:&mut Vec<u64>) {
    images.extend(super::image_scope::rendered_image_nodes(value).into_iter().filter_map(|node|node.as_object()).filter_map(inline_image_base64_chars));
}

#[derive(Clone, Debug)]
struct InlineImageRef {
    payload_bytes: u64,
    source_path: Option<String>,
}

fn child_values(object: &Map<String, Value>) -> Vec<&Value> {
    const PREFERRED: &[&str] = &["input", "output", "content", "messages", "parts"];
    let mut children = Vec::with_capacity(object.len());
    if matches!(object.get("type").and_then(Value::as_str),Some("function_call"|"custom_tool_call"|"tool_search_call"|"tool_use"|"compaction")) {return Vec::new();}
    for key in PREFERRED {
        if let Some(child) = object.get(*key) {
            children.push(child);
        }
    }
    for (key, child) in object {
        if !PREFERRED.contains(&key.as_str()) && !matches!(key.as_str(),"tools"|"metadata"|"provider_metadata"|"arguments"|"parameters"|"input_schema"|"inputSchema"|"const"|"examples"|"properties"|"$defs"|"functionCall"|"tool_calls"|"toolUses") {
            children.push(child);
        }
    }
    children
}

fn child_values_mut(object: &mut Map<String, Value>) -> Vec<&mut Value> {
    const PREFERRED: &[&str] = &["input", "output", "content", "messages", "parts"];
    if matches!(object.get("type").and_then(Value::as_str),Some("function_call"|"custom_tool_call"|"tool_search_call"|"tool_use"|"compaction")) {return Vec::new();}
    let mut preferred = Vec::new();
    let mut rest = Vec::new();
    for (key, child) in object.iter_mut() {
        if matches!(key.as_str(),"tools"|"metadata"|"provider_metadata"|"arguments"|"parameters"|"input_schema"|"inputSchema"|"const"|"examples"|"properties"|"$defs"|"functionCall"|"tool_calls"|"toolUses") {continue;}
        if PREFERRED.contains(&key.as_str()) {
            preferred.push((
                PREFERRED
                    .iter()
                    .position(|item| *item == key.as_str())
                    .unwrap_or(PREFERRED.len()),
                child,
            ));
        } else {
            rest.push(child);
        }
    }
    preferred.sort_by_key(|(index, _)| *index);
    preferred
        .into_iter()
        .map(|(_, child)| child)
        .chain(rest)
        .collect()
}

fn collect_inline_image_refs(
    value: &Value,
    pending_paths: &mut Vec<(String, String)>,
    caption_path: &mut Option<String>,
    images: &mut Vec<InlineImageRef>,
) {
    let scope=super::image_scope::admission_scope(&*value);
    collect_inline_image_refs_scoped(value,pending_paths,caption_path,images,&scope)
}

fn collect_inline_image_refs_scoped(
    value: &Value,
    pending_paths: &mut Vec<(String, String)>,
    caption_path: &mut Option<String>,
    images: &mut Vec<InlineImageRef>,
    scope:&super::image_scope::AdmissionScope,
) {
    let node_id=(&*value) as *const Value as usize;
    if !scope.nodes.contains(&node_id) {return Default::default();}

    match value {
        Value::Array(items) => {
            let mut local_caption = None;
            for item in items {
                collect_inline_image_refs_scoped(item, pending_paths, &mut local_caption, images,scope);
            }
        }
        Value::String(text) => {
            if let Some(path) = parse_caption_image_path(text) { *caption_path = Some(path); }
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
            if scope.images.contains(&node_id) && inline_image_base64_chars(object).is_some() {
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
                    collect_inline_image_refs_scoped(output, pending_paths, caption_path, images,scope);
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
                collect_inline_image_refs_scoped(child, pending_paths, caption_path, images,scope);
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
    // The scoped traversal below decides which array elements are rendered.
    is_tool_output_item(object) && object.get("output").is_some_and(Value::is_array)
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
    let scope=super::image_scope::admission_scope(&*value);
    replace_admission_images_scoped(value,omitted,image_index,images,&scope)
}

fn replace_admission_images_scoped(
    value: &mut Value,
    omitted: &BTreeSet<usize>,
    image_index: &mut usize,
    images: &[InlineImageRef],
    scope:&super::image_scope::AdmissionScope,
) {
    let node_id=(&*value) as *const Value as usize;
    if !scope.nodes.contains(&node_id) {return Default::default();}

    match value {
        Value::Array(items) => {
            for item in items {
                replace_admission_images_scoped(item, omitted, image_index, images,scope);
            }
        }
        Value::Object(object) => {
            if scope.images.contains(&node_id) && inline_image_base64_chars(object).is_some() {
                let index = *image_index;
                *image_index += 1;
                if omitted.contains(&index) {
                    replace_admission_image_object(
                        object,
                        images
                            .get(index)
                            .and_then(|image| image.source_path.as_deref()),
                    );
                }
                return;
            }
            if is_tool_output_item(object) {
                if let Some(output) = object.get_mut("output") {
                    replace_admission_images_scoped(output, omitted, image_index, images, scope);
                }
                return;
            }
            for child in child_values_mut(object) {
                replace_admission_images_scoped(child, omitted, image_index, images,scope);
            }
        }
        _ => {}
    }
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

fn contains_inline_image_candidate(value:&Value)->bool {
    super::image_scope::rendered_image_nodes(value).into_iter().any(|node| {
        let object=node.as_object().expect("image leaf");
        inline_image_url(object).is_some_and(|url|url.starts_with("data:image/")) || matches!(wire_image_kind(object),Some(WireImageKind::Anthropic|WireImageKind::Gemini|WireImageKind::Kiro))
    })
}

fn normalize_pngs_for_stable_history(value:&mut Value,chat_compat:bool,unsafe_png_omissions:&mut usize,allow_omission:bool) {
    for node in super::image_scope::rendered_image_nodes_mut(value) {normalize_png_leaf(node,chat_compat,unsafe_png_omissions,allow_omission);}
}
fn normalize_png_leaf(
    value: &mut Value,
    chat_compat: bool,
    unsafe_png_omissions: &mut usize,
    allow_omission:bool,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                let _=item;
            }
        }
        Value::Object(object) => {
            if inline_image_base64_chars(object).is_some() {
                // Use the existing newest-image fidelity tier for every image.
                // A new screenshot must not downsize or omit an earlier one
                // merely because it moved past an age threshold (6 / 20).
                let (max_edge, hard_cap) = if chat_compat {
                    (1_024_u32, 768 * 1024_usize)
                } else {
                    (2_000_u32, 2 * 1024 * 1024_usize)
                };
                let url = inline_image_url(object);
                let outcome = url
                    .map(|url| normalize_png_data_url(url, max_edge, Some(hard_cap)))
                    .unwrap_or(PngNormalization::Unchanged);
                match outcome {
                    PngNormalization::Unchanged => {
                        let omit_non_png = chat_compat
                            && url.is_some_and(|candidate| {
                                !is_png_data_url(candidate)
                                    && inline_image_payload_chars(candidate)
                                        .is_some_and(|chars| chars > hard_cap)
                            });
                        if omit_non_png {
                            if allow_omission {replace_image_object(object, OMITTED_CHAT_COMPAT_IMAGE);}
                            *unsafe_png_omissions = unsafe_png_omissions.saturating_add(1);
                        }
                    }
                    PngNormalization::Replace(normalized) => {
                        if let Some(url) = inline_image_url_mut(object) {
                            *url = normalized;
                        }
                    }
                    PngNormalization::Omit => {
                        if allow_omission {replace_image_object(object, OMITTED_UNSAFE_PNG);}
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
                    return;
                }
            }
            for child in object.values_mut() {
                let _=child;
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

fn normalize_png_data_url(value: &str, max_edge: u32, hard_cap: Option<usize>) -> PngNormalization {
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
    let exceeds_max_edge =
        hard_cap.is_some() && reader.info().width.max(reader.info().height) > max_edge;
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
        || u64::from(info.width).saturating_mul(u64::from(info.height)) > MAX_NORMALIZABLE_PIXELS
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
        let Some(resized) =
            resize_nearest(&pixels, info.width, info.height, width, height, channels)
        else {
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

fn is_png_data_url(value: &str) -> bool {
    value.starts_with("data:image/png;base64,") || value.starts_with("data:image/x-png;base64,")
}

fn inline_image_payload_chars(value: &str) -> Option<usize> {
    let (metadata, payload) = value.split_once(',')?;
    if !metadata.starts_with("data:image/") || !metadata.ends_with(";base64") || payload.is_empty()
    {
        return None;
    }
    Some(payload.len())
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
        let source_y =
            (u64::from(y) * u64::from(source_height) / u64::from(target_height)) as usize;
        for x in 0..target_width {
            let source_x =
                (u64::from(x) * u64::from(source_width) / u64::from(target_width)) as usize;
            let source_offset = source_y
                .checked_mul(usize::try_from(source_width).ok()?)?
                .checked_add(source_x)?
                .checked_mul(channels)?;
            let target_offset = usize::try_from(y)
                .ok()?
                .checked_mul(usize::try_from(target_width).ok()?)?
                .checked_add(usize::try_from(x).ok()?)?
                .checked_mul(channels)?;
            target
                .get_mut(target_offset..target_offset + channels)?
                .copy_from_slice(source.get(source_offset..source_offset + channels)?);
        }
    }
    Some(target)
}

fn replace_selected_images(value:&mut Value,omitted:&BTreeSet<usize>,image_index:&mut usize,per_image_limit:u64) {
    for node in super::image_scope::rendered_image_nodes_mut(value) {
        if node.as_object().and_then(inline_image_base64_chars).is_some() {replace_selected_images_leaf(node,omitted,image_index,per_image_limit);}
    }
}
fn replace_selected_images_leaf(
    value: &mut Value,
    omitted: &BTreeSet<usize>,
    image_index: &mut usize,
    per_image_limit: u64,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                replace_selected_images_leaf(item, omitted, image_index, per_image_limit);
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
                replace_selected_images_leaf(child, omitted, image_index, per_image_limit);
            }
        }
        _ => {}
    }
}

// Omission uses the same rendered boundary as counting and normalization.
fn omit_first_image(value: &mut Value) -> bool {
    for node in super::image_scope::rendered_image_nodes_mut(value) {
        let Some(object) = node.as_object_mut() else { continue };
        if inline_image_base64_chars(object).is_none() { continue; }
        if let Some(kind) = wire_image_kind(object) {
            replace_wire_image_object(object, kind, OMITTED_FOR_BUDGET);
            return true;
        }
    }
    false
}

fn replace_all_images(value:&mut Value,marker:&str) {
    for node in super::image_scope::rendered_image_nodes_mut(value) {
        if let Some(object)=node.as_object_mut() {if let Some(kind)=wire_image_kind(object) {replace_wire_image_object(object,kind,marker);}}
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

fn replace_wire_image_object(object: &mut Map<String, Value>, kind: WireImageKind, marker: &str) {
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
    if !metadata.starts_with("data:image/") || !metadata.ends_with(";base64") || payload.is_empty()
    {
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
    fn aggregate_budget_preserves_old_images_and_requires_compaction() {
        let mut body =
            json!({"input": [{"content": [image(8 * MIB as usize), image(8 * MIB as usize)]}]});
        let original = body.clone();
        let report = normalize_translated_image_history_sync(
            &mut body,
            ProviderProtocol::GeminiGenerateContent,
            ProviderTransport::Standard,
            16 * MIB,
        );
        assert_eq!(report.omitted_images, 0);
        assert!(report.requires_compaction);
        assert_eq!(report.kept_base64_chars, 16 * MIB);
        assert!(
            body == original,
            "aggregate fitting must not rewrite the cached prefix"
        );
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
        assert!(body
            .pointer("/input/0/content/0/text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("per-image")));
    }

    #[test]
    fn leaves_tiny_under_budget_chat_pngs_unchanged() {
        let original_url = png_image_data_url(64, 64);
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
        assert_eq!(
            body.pointer("/input/0/content/0/image_url").cloned(),
            original
        );
    }

    #[test]
    fn chat_completions_enforce_a_per_image_limit_before_aggregate_pruning() {
        let mut body = json!({"input": [{"content": [image(3 * 1024 * 1024), image(1024)]}]});
        let report = normalize_translated_image_history_sync(
            &mut body,
            ProviderProtocol::ChatCompletions,
            ProviderTransport::Standard,
            64 * MIB,
        );
        assert_eq!(report.omitted_images, 1);
        assert!(body
            .pointer("/input/0/content/0/text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("per-image") || text.contains("size tier")));
        assert_eq!(
            body.pointer("/input/0/content/1/type")
                .and_then(Value::as_str),
            Some("input_image")
        );
    }

    #[test]
    fn chat_completions_omit_oversized_non_png_images_under_the_byte_budget() {
        let mut body = json!({"input": [{"content": [image(900 * 1024), image(1024)]}]});
        let report = normalize_translated_image_history_sync(
            &mut body,
            ProviderProtocol::ChatCompletions,
            ProviderTransport::Standard,
            64 * MIB,
        );
        assert_eq!(report.omitted_images, 1);
        assert!(body
            .pointer("/input/0/content/0/text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("size tier")));
        assert_eq!(
            body.pointer("/input/0/content/1/type")
                .and_then(Value::as_str),
            Some("input_image")
        );
    }

    #[test]
    fn chat_completions_rewrite_original_image_detail_to_high() {
        let mut body = json!({"input": [{"content": [{
            "type": "input_image",
            "detail": "original",
            "image_url": {
                "url": format!("data:image/jpeg;base64,{}", "A".repeat(1024)),
                "detail": "original"
            }
        }]}]});
        let report = normalize_translated_image_history_sync(
            &mut body,
            ProviderProtocol::ChatCompletions,
            ProviderTransport::Standard,
            64 * MIB,
        );
        assert_eq!(report.omitted_images, 0);
        assert_eq!(
            body.pointer("/input/0/content/0/detail")
                .and_then(Value::as_str),
            Some("high")
        );
        assert_eq!(
            body.pointer("/input/0/content/0/image_url/detail")
                .and_then(Value::as_str),
            Some("high")
        );
    }

    #[test]
    fn chat_completions_rewrite_original_detail_even_without_inline_bytes() {
        let mut body = json!({"input": [{"content": [{
            "type": "input_image",
            "detail": "original",
            "image_url": {
                "url": "https://example.test/screenshot.png",
                "detail": "original"
            }
        }]}]});
        let report = normalize_translated_image_history_sync(
            &mut body,
            ProviderProtocol::ChatCompletions,
            ProviderTransport::Standard,
            64 * MIB,
        );
        assert_eq!(report.omitted_images, 0);
        assert_eq!(
            body.pointer("/input/0/content/0/detail")
                .and_then(Value::as_str),
            Some("high")
        );
        assert_eq!(
            body.pointer("/input/0/content/0/image_url/detail")
                .and_then(Value::as_str),
            Some("high")
        );
    }

    #[test]
    fn chat_completions_resize_large_pngs_even_when_encoded_bytes_are_under_cap() {
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
        assert!(reader.info().width.max(reader.info().height) <= 1_024);
    }

    #[test]
    fn chat_completions_resize_under_budget_pngs_for_every_provider() {
        let original = png_image_data_url(1_536, 1_024);
        let mut body = json!({"input": [{"content": [{
            "type": "input_image",
            "image_url": original
        }]}]});
        let report = normalize_translated_image_history_sync(
            &mut body,
            ProviderProtocol::ChatCompletions,
            ProviderTransport::Standard,
            64 * MIB,
        );
        assert_eq!(report.omitted_images, 0);
        let normalized = body
            .pointer("/input/0/content/0/image_url")
            .and_then(Value::as_str)
            .expect("normalized image URL");
        let payload = normalized.split_once(',').expect("data URL").1;
        let decoded = STANDARD.decode(payload).expect("base64");
        let reader = png::Decoder::new(Cursor::new(decoded))
            .read_info()
            .expect("PNG info");
        assert!(reader.info().width.max(reader.info().height) <= 1_024);
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
            body.pointer("/input/0/content/0/text")
                .and_then(Value::as_str),
            Some(OMITTED_UNSAFE_PNG)
        );
    }

    #[test]
    fn native_transport_small_budget_requires_compaction_without_pruning() {
        let mut body = json!({"input": [{"content": [image(128 * 1024)]}]});
        let original = body.clone();
        let report = normalize_translated_image_history_sync(
            &mut body,
            ProviderProtocol::ChatCompletions,
            ProviderTransport::Kiro,
            64 * 1024,
        );
        assert_eq!(report.omitted_images, 0);
        assert!(report.requires_compaction);
        assert!(
            body == original,
            "small transport budgets must not delete images"
        );
    }

    #[test]
    fn omit_oldest_skips_remote_urls_and_drops_inline_data() {
        let mut body = json!({
            "input": [
                {
                    "type": "input_image",
                    "image_url": "https://example.test/oldest.png"
                },
                {
                    "type": "input_image",
                    "image_url": "data:image/png;base64,AAA="
                }
            ]
        });
        assert_eq!(count_translated_input_images(&body), 1);
        assert!(omit_oldest_translated_input_image(&mut body));
        assert_eq!(
            body.pointer("/input/0/image_url").and_then(Value::as_str),
            Some("https://example.test/oldest.png")
        );
        assert_eq!(count_translated_input_images(&body), 0);
        assert!(!omit_oldest_translated_input_image(&mut body));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compact_wire_budget_large_request_does_not_block_async_io() {
        // Reproduce the original scale: ~120 MiB input vs a 64 MiB provider cap.
        // A current-thread executor exposes any accidental synchronous rewrite.
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": [
                image(40 * MIB as usize), image(40 * MIB as usize), image(40 * MIB as usize)
            ]},
            {"type": "message", "role": "user", "content": "latest request"},
            {"type": "message", "role": "assistant", "content": "latest question?"},
            {"type": "compaction_trigger"}
        ]});
        let mut work = Box::pin(serialize_with_image_budget_async(&mut body, 64 * MIB));
        tokio::select! {
            biased;
            _ = tokio::time::sleep(std::time::Duration::from_millis(1)) => {},
            result = &mut work => panic!("budgeting completed without yielding to I/O: {:?}", result.map(|(bytes, count)| (bytes.len(), count))),
        }
        let (bytes, omitted) = work.await.unwrap();
        assert!(bytes.len() as u64 <= 64 * MIB);
        assert_eq!(omitted, 2);
        assert_eq!(body["input"][0]["content"][0]["text"], OMITTED_FOR_BUDGET);
        assert_eq!(body["input"][0]["content"][1]["text"], OMITTED_FOR_BUDGET);
        assert_eq!(body["input"][0]["content"][2]["type"], "input_image");
        assert_eq!(body["input"][1]["content"], "latest request");
        assert_eq!(body["input"][2]["content"], "latest question?");
        assert_eq!(body["input"][3]["type"], "compaction_trigger");
    }

    #[test]
    fn compact_wire_budget_preserves_under_limit_request_exactly() {
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": [image(2048)]},
            {"type": "compaction_trigger"}
        ]});
        let original = body.clone();
        let expected = serde_json::to_vec(&body).unwrap();
        let (bytes, omitted) = serialize_with_image_budget(&mut body, expected.len() as u64).unwrap();
        assert_eq!(omitted, 0);
        assert_eq!(body, original);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn compact_wire_budget_keeps_recent_image_text_tools_and_trigger() {
        // Both native compact (no trigger) and Responses compact-trigger bodies
        // use the same final-wire budget without changing their control fields.
        for trigger in [false, true] {
            let mut body = json!({"model": "test", "instructions": "retain task facts", "input": [
                {"type": "message", "role": "user", "content": [image(4096)]},
                {"type": "function_call", "call_id": "call_1", "name": "read", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "important result"},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "latest request"}, image(1024)]},
                {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "latest question?"}]}
            ]});
            if trigger {
                body["input"].as_array_mut().unwrap().push(json!({"type": "compaction_trigger"}));
            }
            let original = body.clone();
            let mut expected = original.clone();
            assert!(omit_oldest_translated_input_image(&mut expected));
            let limit = serde_json::to_vec(&expected).unwrap().len() as u64;
            let (bytes, omitted) = serialize_with_image_budget(&mut body, limit).unwrap();
            assert_eq!(omitted, 1);
            assert!(bytes.len() as u64 <= limit);
            assert_eq!(body, expected);
            assert_eq!(body["input"][3], original["input"][3]);
            assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), expected);
        }
    }

    #[test]
    fn compact_wire_budget_preserves_encoded_tool_data_without_losing_text() {
        for kind in ["function_call_output", "custom_tool_call_output"] {
            let image = serde_json::json!({"type":"input_image","image_url":format!("data:image/jpeg;base64,{}", "A".repeat(4096))});
            let encoded = serde_json::json!([{"type":"input_text","text":"literal tool data"},image]).to_string();
            let mut body = serde_json::json!({"input":[{"type":kind,"call_id":"call","output":encoded},
                {"type":"message","role":"user","content":[image]}]});
            let original = body["input"][0].clone();
            let (bytes, omitted) = serialize_with_image_budget(&mut body, 2048).unwrap();
            assert_eq!(omitted,1);
            assert_eq!(body["input"][0],original);
            assert!(bytes.len()>2048, "literal tool data is not pruned to fit the wire budget");
            assert_eq!(count_translated_input_images(&body),0);
        }
    }

    #[test]
    fn compact_wire_budget_leaves_unrelated_and_invalid_json_strings_verbatim() {
        let encoded = serde_json::to_string(&json!([image(4096)])).unwrap();
        for output in ["{broken JSON".to_string(), " [  {\"text\": \"keep whitespace\"}  ] ".to_string()] {
            let mut body = json!({"input": [
                {"type": "message", "role": "user", "content": encoded},
                {"type": "function_call", "call_id": "c", "arguments": encoded},
                {"type": "custom_tool_call_output", "call_id": "c", "output": output}
            ]});
            let original = body.clone();
            let (bytes, omitted) = serialize_with_image_budget(&mut body, 2048).unwrap();
            assert_eq!(omitted, 0);
            assert!(bytes.len() > 2048);
            assert_eq!(body, original);
        }
    }

    #[test]
    fn compact_wire_budget_preserves_nonrendered_tool_urls_in_all_output_forms() {
        let url = format!("data:image/jpeg;base64,{}", "A".repeat(4096));
        let image = serde_json::json!({"type":"input_image","image_url":url});
        for output in [serde_json::json!(url),serde_json::json!([url]),
            serde_json::json!(serde_json::json!([url]).to_string()),serde_json::json!({"payload":image})] {
            for kind in ["function_call_output", "custom_tool_call_output"] {
                let mut body = serde_json::json!({"input":[{"type":kind,"call_id":"call","output":output},
                    {"type":"message","role":"user","content":[image]}]});
                let original = body["input"][0].clone();
                let (bytes,omitted) = serialize_with_image_budget(&mut body,2048).unwrap();
                assert_eq!(omitted,1);
                assert_eq!(body["input"][0],original);
                assert!(bytes.len()>2048);
                assert!(!omit_oldest_wire_history_image(&mut body));
            }
        }
    }

    #[test]
    fn compact_wire_budget_preserves_bare_urls_in_ordinary_text() {
        let url = format!("data:image/png;base64,{}", "A".repeat(4096));
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": url},
            {"type": "custom_tool_call_output", "call_id": "c", "output": [
                {"type": "input_text", "text": url}
            ]}
        ]});
        let original = body.clone();
        let (bytes, omitted) = serialize_with_image_budget(&mut body, 2048).unwrap();
        assert_eq!(omitted, 0);
        assert!(bytes.len() > 2048);
        assert_eq!(body, original);
    }

    #[test]
    fn compact_wire_budget_omits_multiple_images_in_chronological_order() {
        let image = |payload: &str| serde_json::json!({"type":"input_image","image_url":format!("data:image/jpeg;base64,{}",payload.repeat(4096))});
        let mut body = serde_json::json!({"input":[
            {"type":"function_call_output","call_id":"call","output":[image("A"),{"type":"input_text","text":"keep caption"},image("B")]},
            {"type":"message","role":"user","content":[image("C")]}]});
        let latest = body["input"][1].clone();
        let mut expected = body.clone();
        for index in [0,2] {
            expected["input"][0]["output"][index] = serde_json::json!({"type":"input_text","text":OMITTED_FOR_BUDGET});
        }
        let limit = serde_json::to_vec(&expected).unwrap().len() as u64;
        let (bytes,omitted) = serialize_with_image_budget(&mut body,limit).unwrap();
        assert_eq!(omitted,2);
        assert_eq!(body,expected);
        assert_eq!(body["input"][1],latest);
        assert_eq!(count_translated_input_images(&body),1);
        assert!(bytes.len() as u64 <= limit);
    }

    #[test]
    fn compact_wire_budget_preserves_non_image_inline_attachments() {
        for mime in ["application/pdf","audio/wav","video/mp4"] {
            for snake in [false,true] {
                let attachment = if snake { serde_json::json!({"inline_data":{"mime_type":mime,"data":"A".repeat(4096)}}) }
                    else { serde_json::json!({"inlineData":{"mimeType":mime,"data":"A".repeat(4096)}}) };
                let image = serde_json::json!({"inlineData":{"mimeType":"image/jpeg","data":"A".repeat(4096)}});
                let mut body = serde_json::json!({"contents":[{"role":"user","parts":[attachment.clone(),image]}]});
                let (bytes,omitted) = serialize_with_image_budget(&mut body,2048).unwrap();
                assert_eq!(omitted,1,"only the rendered image may be omitted: {mime}");
                assert_eq!(body["contents"][0]["parts"][0],attachment);
                assert!(bytes.len()>2048);
                assert!(!omit_oldest_wire_history_image(&mut body));
            }
        }
    }

    #[test]
    fn compact_wire_budget_never_rewrites_tools_or_other_request_options() {
        for history_image in [false, true] {
            let mut body = json!({
                "tools": [{"type": "function", "name": "example", "parameters": {
                    "type": "object", "properties": {"payload": {"const": image(4096)}}
                }}],
                "metadata": {"example": image(4096)},
                "input": [{"type": "message", "role": "user", "content": "keep text"}]
            });
            if history_image {
                body["input"].as_array_mut().unwrap().push(json!({
                    "type": "message", "role": "user", "content": [image(4096)]
                }));
            }
            let original = body.clone();
            let (bytes, omitted) = serialize_with_image_budget(&mut body, 2048).unwrap();
            assert_eq!(omitted, usize::from(history_image));
            assert!(bytes.len() > 2048, "oversized schemas must be rejected, not rewritten");
            assert_eq!(body["tools"], original["tools"]);
            assert_eq!(body["metadata"], original["metadata"]);
            assert_eq!(body["input"][0], original["input"][0]);
        }
    }

    #[test]
    fn compact_wire_budget_fits_supported_history_envelopes_only() {
        let payload = "A".repeat(4096);
        let gemini = json!({"role": "user", "parts": [{
            "inlineData": {"mimeType": "image/png", "data": payload}
        }]});
        let requests = [
            json!({"input": [{"role": "user", "content": [image(4096)]}]}),
            json!({"messages": [{"role": "user", "content": [{
                "type": "image_url", "image_url": {"url": format!("data:image/png;base64,{payload}")}
            }]}]}),
            json!({"messages": [{"role": "user", "content": [{
                "type": "image", "source": {"type": "base64", "media_type": "image/png", "data": payload}
            }]}]}),
            json!({"contents": [gemini.clone()]}),
            json!({"request": {"contents": [gemini], "tools": [{"example": image(64)}]}}),
        ];
        for mut body in requests {
            let tools = body.pointer("/request/tools").cloned();
            let (bytes, omitted) = serialize_with_image_budget(&mut body, 2048).unwrap();
            assert_eq!(omitted, 1);
            assert!(bytes.len() <= 2048);
            assert_eq!(body.pointer("/request/tools").cloned(), tools);
        }
    }

    #[test]
    fn compact_wire_budget_does_not_truncate_oversized_text() {
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": "x".repeat(4096)},
            {"type": "compaction_trigger"}
        ]});
        let original = body.clone();
        let (bytes, omitted) = serialize_with_image_budget(&mut body, 1024).unwrap();
        assert_eq!(omitted, 0);
        assert!(bytes.len() > 1024); // Sender must reject, not silently lose history.
        assert_eq!(body, original);
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
        assert_eq!(
            body.pointer("/input/0/content/0/text")
                .and_then(Value::as_str),
            Some(OMITTED_FOR_COMPACTION)
        );
        assert_eq!(
            body.pointer("/input/0/content/1/text")
                .and_then(Value::as_str),
            Some(OMITTED_FOR_COMPACTION)
        );
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
            body.pointer("/messages/0/content/0/type")
                .and_then(Value::as_str),
            Some("text")
        );
        assert!(body
            .pointer("/messages/0/content/0/text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("per-image")));
        assert_eq!(
            body.pointer("/messages/0/content/1/type")
                .and_then(Value::as_str),
            Some("image")
        );
    }

    #[test]
    fn gemini_inline_data_budget_requires_compaction_without_pruning() {
        let data = "A".repeat(12 * MIB as usize);
        let mut body = json!({"contents": [{"parts": [
            {"inlineData": {"mimeType": "image/jpeg", "data": data}},
            {"inlineData": {"mimeType": "image/jpeg", "data": data}}
        ]}]});
        let original = body.clone();
        let report = normalize_translated_image_history_sync(
            &mut body,
            ProviderProtocol::GeminiGenerateContent,
            ProviderTransport::Standard,
            64 * MIB,
        );
        assert_eq!(report.omitted_images, 0);
        assert!(report.requires_compaction);
        assert_eq!(report.kept_base64_chars, 24 * MIB);
        assert!(body == original, "Gemini-native history must remain intact");
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
            body.pointer("/input/1/output/0/type")
                .and_then(Value::as_str),
            Some("input_text")
        );
        assert_eq!(
            body.pointer("/input/1/output/0/text").and_then(Value::as_str),
            Some("[image omitted: older image data exceeded the gateway request body limit]\npath: /Volumes/D/Project/review/old.png")
        );
        assert_eq!(
            body.pointer("/input/3/output/0/type")
                .and_then(Value::as_str),
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
            parsed
                .pointer("/input/0/content/3/type")
                .and_then(Value::as_str),
            Some("input_image")
        );
    }

    #[test]
    fn admission_rewrite_preserves_string_encoded_tool_output_images() {
        let image=serde_json::json!({"type":"input_image","image_url":format!("data:image/jpeg;base64,{}","A".repeat(4096))});
        let encoded=serde_json::json!([{"type":"input_text","text":"path=\"/fixture/image.jpg\""},image.clone(),image.clone()]).to_string();
        let mut body=serde_json::json!({"input":[{"type":"custom_tool_call_output","call_id":"c","output":encoded}]});
        let original=body.clone();let report=rewrite_oversized_request_images_for_admission(&mut body,512);
        assert_eq!(report.inline_images,0);assert_eq!(report.omitted_images,0);assert_eq!(body,original);
        body["input"][0]["output"]=serde_json::json!([image.clone(),image]);
        let report=rewrite_oversized_request_images_for_admission(&mut body,512);
        assert_eq!(report.inline_images,2);assert_eq!(report.omitted_images,2);
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
            body.pointer("/input/0/content/1/type")
                .and_then(Value::as_str),
            Some("input_text")
        );
        assert!(body
            .pointer("/input/0/content/1/text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("path: /tmp/codex-remote-attachments/photo.jpg")));
        assert_eq!(
            body.pointer("/input/0/content/3/type")
                .and_then(Value::as_str),
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
            body.pointer("/input/0/content/20/type")
                .and_then(Value::as_str),
            Some("input_image")
        );
        assert!(estimate_json_request_bytes(&body) <= 20 * 1024);
    }
}

#[cfg(test)]
mod normal_per_image_boundary_tests {
    use super::*;
    #[tokio::test]
    async fn per_image_overflow_requires_compaction_without_replacing_pixels() {
        for (protocol,size) in [(ProviderProtocol::ChatCompletions,900*1024),(ProviderProtocol::AnthropicMessages,8*1024*1024)] {
            let mut body=serde_json::json!({"input":[{"type":"message","role":"user","content":[{"type":"input_image","image_url":format!("data:image/jpeg;base64,{}","A".repeat(size))}]}]});let original=body.clone();
            assert!(matches!(normalize_translated_image_history(&mut body,protocol,ProviderTransport::Standard,64*MIB).await,Err(ImageHistoryError::OmissionRequired)));
            assert_eq!(body,original);
            let report=normalize_translated_image_history_for_compaction(&mut body,protocol,ProviderTransport::Standard,64*MIB).await.unwrap();
            assert_eq!(report.omitted_images,1);assert_eq!(count_translated_input_images(&body),0);
        }
    }
}

#[cfg(test)]
mod rendered_image_scope_tests {
    use super::*;
    #[tokio::test]
    async fn schemas_arguments_and_metadata_are_neither_changed_nor_budgeted_as_images() {
        let mut png=Vec::new();{let mut encoder=png::Encoder::new(&mut png,2400,800);encoder.set_color(png::ColorType::Rgb);encoder.set_depth(png::BitDepth::Eight);let mut writer=encoder.write_header().unwrap();writer.write_image_data(&vec![127;2400*800*3]).unwrap();}
        let image=serde_json::json!({"type":"input_image","image_url":format!("data:image/png;base64,{}",STANDARD.encode(png)),"detail":"original"});
        let mut body=serde_json::json!({"input":[{"type":"message","role":"user","content":"text","metadata":{"image":image.clone()}},{"type":"function_call","call_id":"c","name":"f","arguments":{"example":image.clone()}}],"tools":[{"type":"function","name":"f","parameters":{"type":"object","properties":{"image":{"const":image.clone(),"examples":[image.clone()]}}}}],"metadata":{"image":image.clone()}});let original=body.clone();
        assert_eq!(count_translated_input_images(&body),0);let report=normalize_translated_image_history(&mut body,ProviderProtocol::ChatCompletions,ProviderTransport::Standard,64*MIB).await.unwrap();assert_eq!(report.inline_images,0);assert_eq!(body,original);
        body["input"][0]["content"]=serde_json::json!([image.clone()]);normalize_translated_image_history(&mut body,ProviderProtocol::ChatCompletions,ProviderTransport::Standard,64*MIB).await.unwrap();
        assert_ne!(body["input"][0]["content"][0]["image_url"],image["image_url"]);assert_eq!(body["tools"],original["tools"]);assert_eq!(body["metadata"],original["metadata"]);assert_eq!(body["input"][1],original["input"][1]);assert_eq!(body["input"][0]["metadata"],original["input"][0]["metadata"]);
        body["input"].as_array_mut().unwrap().push(serde_json::json!({"type":"function_call_output","call_id":"c","output":[image.clone()]}));
        normalize_translated_image_history(&mut body,ProviderProtocol::ChatCompletions,ProviderTransport::Standard,64*MIB).await.unwrap();
        assert_ne!(body["input"][2]["output"][0]["image_url"],image["image_url"]);
    }
}


#[cfg(test)]
mod review11_admission_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn admission_keeps_json_examples_and_nonrendered_data_verbatim() {
        let image = json!({"type":"input_image","image_url":format!("data:image/jpeg;base64,{}", "A".repeat(4096))});
        let example = format!("  {}  ", json!([image.clone()]));
        let mut body = json!({"input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":example},image.clone()],"metadata":{"example":image.clone()}},
            {"type":"function_call","name":"f","call_id":"c","arguments":{"example":image.clone()}},
            {"type":"compaction_trigger"}
        ],"metadata":{"example":image.clone()},"tools":[{"parameters":{"example":image}}]});
        let before = body.clone();
        let report = rewrite_oversized_request_images_for_admission(&mut body, 1);
        assert_eq!(report.inline_images, 1);
        assert_eq!(report.omitted_images, 1);
        assert_eq!(body["input"][0]["content"][0], before["input"][0]["content"][0]);
        assert_eq!(body["input"][0]["metadata"], before["input"][0]["metadata"]);
        assert_eq!(body["input"][1], before["input"][1]);
        assert_eq!(body["metadata"], before["metadata"]);
        assert_eq!(body["tools"], before["tools"]);
        assert_eq!(body["input"][0]["content"][1]["type"], "input_text");
        let text_only = json!({"input":[{"role":"user","content":[{"type":"input_text","text":example}]},{"type":"compaction_trigger"}]});
        assert!(shrink_explicit_compaction_request_bytes(&serde_json::to_vec(&text_only).unwrap(), 1000, false).is_none());
    }

    #[test]
    fn admission_collect_and_replace_agree_on_rendered_tool_output_boundaries() {
        let image = serde_json::json!({"type":"input_image","image_url":format!("data:image/jpeg;base64,{}","A".repeat(4096))});
        for output in [image["image_url"].clone(), serde_json::json!([image.clone()]).to_string().into(), image.clone()] {
            let mut body=serde_json::json!({"input":[{"type":"function_call_output","call_id":"c","output":output}]});
            let original=body.clone();
            let report=rewrite_oversized_request_images_for_admission(&mut body,512);
            assert_eq!(report.inline_images,0);assert_eq!(report.omitted_images,0);assert_eq!(body,original);
        }
        let mut body=serde_json::json!({"input":[{"type":"function_call_output","call_id":"c","output":[image]}]});
        let report=rewrite_oversized_request_images_for_admission(&mut body,512);
        assert_eq!(report.inline_images,1);assert_eq!(report.omitted_images,1);
        assert_eq!(body["input"][0]["output"][0]["type"],"input_text");
    }
}

#[cfg(test)]
mod regression12_image_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn regression12_compact_omits_only_rendered_images() {
        let image = json!({"type":"input_image","image_url":format!("data:image/jpeg;base64,{}", "A".repeat(2048))});
        let structured = json!({"type":"function_call_output","call_id":"data","output":{"payload":image}});
        let encoded = json!({"type":"custom_tool_call_output","call_id":"text","output":image.to_string()});
        let mut body = json!({"input":[structured.clone(),encoded.clone(),
            {"type":"message","role":"user","content":[image.clone(),image]}]});
        let report = normalize_translated_image_history_for_compaction(&mut body,
            ProviderProtocol::AnthropicMessages, ProviderTransport::Standard, MIB + 2048).await.unwrap();
        assert_eq!(report.inline_images, 2);
        assert_eq!(report.omitted_images, 1);
        assert_eq!(body["input"][0], structured);
        assert_eq!(body["input"][1], encoded);
        assert_eq!(count_translated_input_images(&body), 1);
        assert_eq!(body["input"][2]["content"][0]["type"], "input_text");
        let (_, omitted) = serialize_with_image_budget(&mut body, 1).unwrap();
        assert_eq!(omitted, 1);
        assert_eq!(body["input"][0], structured);
        assert_eq!(body["input"][1], encoded);
    }

    #[test]
    fn regression12_omission_matches_supported_rendered_scopes() {
        let image = json!({"type":"input_image","image_url":"data:image/jpeg;base64,AAAA"});
        for mut body in [
            json!({"input":[{"type":"function_call_output","output":[image]}]}),
            json!({"messages":[{"role":"user","content":[{"type":"tool_result","content":[{"type":"image","source":{"type":"base64","data":"AAAA","media_type":"image/jpeg"}}]}]}]}),
            json!({"request":{"contents":[{"role":"user","parts":[{"functionResponse":{"response":{"payload":image},"parts":[{"inlineData":{"mimeType":"image/jpeg","data":"AAAA"}}]}}]}]}}),
        ] {
            assert_eq!(count_translated_input_images(&body), 1);
            assert!(omit_oldest_wire_history_image(&mut body));
            assert_eq!(count_translated_input_images(&body), 0);
            assert!(!omit_oldest_wire_history_image(&mut body));
        }
    }
}

#[cfg(test)]
mod review13_tool_object_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn review13_admission_keeps_standalone_tool_objects() {
        let object = json!({"type":"input_image","image_url":format!("data:image/png;base64,{}", "A".repeat(4096)),"other":"keep"});
        for output in [object.clone(), json!(object.to_string())] {
            let mut body = json!({"input":[{"type":"function_call_output","call_id":"c","output":output}, {"type":"compaction_trigger"}]});
            let original = body.clone();
            let report = rewrite_oversized_request_images_for_admission(&mut body, 1024);
            assert_eq!(report.omitted_images, 0);
            assert_eq!(body, original);
        }
        let mut body = json!({"input":[{"type":"function_call_output","call_id":"c","output":[
            {"type":"input_text","text":"keep"}, object
        ]}, {"type":"compaction_trigger"}]});
        assert_eq!(rewrite_oversized_request_images_for_admission(&mut body,1024).omitted_images,1);
        assert_eq!(body["input"][0]["output"][0]["text"], "keep");
    }
}

#[cfg(test)]
mod review13_tool_rendering_regressions {
    use super::*;
    use serde_json::json;

    #[test]
    fn review13_singleton_tool_data_stays_data_in_admission_and_provider_rendering() {
        let object = json!({"type":"input_image","image_url":format!("data:image/jpeg;base64,{}", "A".repeat(4096)),"other":"business data must survive"});
        for output in [object.clone(), Value::String(object.to_string())] {
            let mut body = json!({"input":[
                {"type":"message","role":"user","content":"run task"},
                {"type":"function_call","name":"f","call_id":"c","arguments":"{}"},
                {"type":"function_call_output","call_id":"c","output":output}
            ]});
            let original = body.clone();
            let report = rewrite_oversized_request_images_for_admission(&mut body, 1024);
            assert_eq!(report.inline_images, 0);
            assert_eq!(report.omitted_images, 0);
            assert_eq!(body, original);
            let anthropic = crate::anthropic::responses_to_anthropic(&body, "test").unwrap();
            let gemini = crate::gemini::responses_to_gemini(&body, "test").unwrap();
            let (kiro, _) = crate::kiro::responses_to_kiro(&body, "test", None).unwrap();
            let chat = crate::translate::responses_to_chat_with_options(&body, "test", false).unwrap();
            for wire in [anthropic, gemini, kiro, chat] {
                assert_eq!(count_translated_input_images(&wire), 0);
                assert!(wire.to_string().contains("business data must survive"));
            }
        }
    }
}

#[cfg(test)]
mod review14_admission_regressions {
    use super::*;
    use serde_json::json;
    #[test]
    fn review14_admission_preserves_text_and_structured_tool_data() {
        let image=json!({"type":"input_image","image_url":format!("data:image/jpeg;base64,{}","A".repeat(4096))});
        for output in [image.clone(),json!(image.to_string()),json!(vec![image.clone()].into_iter().collect::<Vec<_>>()).to_string().into(),image["image_url"].clone()] {
            let original=json!({"input":[{"type":"function_call_output","call_id":"c","output":output},{"type":"compaction_trigger"}]});
            let mut body=original.clone();
            let report=rewrite_oversized_request_images_for_admission(&mut body,256);
            assert_eq!(report.inline_images,0);
            assert_eq!(report.omitted_images,0);
            assert_eq!(body,original);
            assert!(shrink_explicit_compaction_request_bytes(&serde_json::to_vec(&body).unwrap(),256,false).is_none());
        }
    }
    #[test]
    fn review14_admission_omits_only_typed_array_leaves() {
        let image=json!({"type":"input_image","image_url":format!("data:image/jpeg;base64,{}","A".repeat(8192))});
        let data=json!({"type":"input_image","image_url":"data:image/jpeg;base64,QUJD"});
        let mut body=json!({"metadata":data,"tools":[{"parameters":{"const":data}}],"input":[
            {"type":"function_call","name":"inspect","call_id":"c","arguments":data},
            {"type":"function_call_output","call_id":"c","output":[{"type":"input_text","text":data.to_string()}, image]},
            {"type":"custom_tool_call_output","call_id":"d","output":data},
            {"type":"compaction_trigger"}]});
        let original=body.clone();
        let report=rewrite_oversized_request_images_for_admission(&mut body,2048);
        assert_eq!(report.inline_images,1); assert_eq!(report.omitted_images,1);
        assert_eq!(body["metadata"],original["metadata"]); assert_eq!(body["tools"],original["tools"]);
        assert_eq!(body["input"][0],original["input"][0]); assert_eq!(body["input"][2],original["input"][2]);
        assert_eq!(body["input"][1]["output"][0],original["input"][1]["output"][0]);
        assert_eq!(body["input"][1]["output"][1]["type"],"input_text");
    }
}
