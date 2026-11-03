//! Provider-neutral inline image token cost.
//!
//! Providers bill an image by its pixels, not its bytes, and the bill is
//! bounded. A flat guess of 8,192 or 16,384 tokens per screenshot overstated
//! real cost about five times, so the gateway's budget filled long before the
//! provider (or Codex, which reads provider usage) thought the context was full.
//! Read the dimensions from the image header and price the image the way the
//! providers do, taking the larger of the OpenAI and Anthropic figures so one
//! number is safe for every protocol.

use base64::{
    alphabet,
    engine::{general_purpose::GeneralPurposeConfig, DecodePaddingMode, GeneralPurpose},
    Engine as _,
};

/// Fallback when the header cannot be read (unknown format, truncated data).
pub(super) const UNKNOWN_IMAGE_TOKENS: u64 = 3_300;
pub(super) const UNKNOWN_ORIGINAL_IMAGE_TOKENS: u64 = 4_096;
pub(super) const LOW_DETAIL_IMAGE_TOKENS: u64 = 1_024;
const OPENAI_PATCH_PIXELS: u64 = 32;
const OPENAI_MAX_PATCHES: u64 = 1_536;
const OPENAI_MAX_PATCHES_ORIGINAL: u64 = 10_000;
const ANTHROPIC_PIXELS_PER_TOKEN: u64 = 750;
const ANTHROPIC_MAX_IMAGE_TOKENS: u64 = 3_300;
const MIN_IMAGE_TOKENS: u64 = 85;
/// Decoded bytes inspected for the header. JPEG may carry large EXIF/ICC
/// segments before its frame header.
const HEADER_BASE64_CHARS: usize = 180_000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ImageDetail {
    Low,
    Default,
    Original,
}

impl ImageDetail {
    pub(super) fn parse(detail: Option<&str>) -> Self {
        match detail {
            Some(value) if value.eq_ignore_ascii_case("low") => Self::Low,
            Some(value) if value.eq_ignore_ascii_case("original") => Self::Original,
            _ => Self::Default,
        }
    }
}

pub(super) fn fallback_tokens(detail: ImageDetail) -> u64 {
    match detail {
        ImageDetail::Low => LOW_DETAIL_IMAGE_TOKENS,
        ImageDetail::Default => UNKNOWN_IMAGE_TOKENS,
        ImageDetail::Original => UNKNOWN_ORIGINAL_IMAGE_TOKENS,
    }
}

pub(super) fn tokens_for_dimensions(detail: ImageDetail, width: u64, height: u64) -> u64 {
    if detail == ImageDetail::Low {
        return LOW_DETAIL_IMAGE_TOKENS;
    }
    if width == 0 || height == 0 {
        return fallback_tokens(detail);
    }
    let patches = width.div_ceil(OPENAI_PATCH_PIXELS) * height.div_ceil(OPENAI_PATCH_PIXELS);
    let openai_cap = if detail == ImageDetail::Original {
        OPENAI_MAX_PATCHES_ORIGINAL
    } else {
        OPENAI_MAX_PATCHES
    };
    let anthropic =
        (width.saturating_mul(height) / ANTHROPIC_PIXELS_PER_TOKEN).min(ANTHROPIC_MAX_IMAGE_TOKENS);
    patches.min(openai_cap).max(anthropic).max(MIN_IMAGE_TOKENS)
}

/// `payload` is the base64 part of a data URL (after the comma).
pub(super) fn dimensions_from_base64(payload: &str) -> Option<(u64, u64)> {
    let mut end = payload.len().min(HEADER_BASE64_CHARS);
    if end < payload.len() {
        end -= end % 4;
    }
    let prefix = payload.get(..end)?;
    let engine = if prefix.contains(['-', '_']) {
        GeneralPurpose::new(&alphabet::URL_SAFE, header_config())
    } else {
        GeneralPurpose::new(&alphabet::STANDARD, header_config())
    };
    let bytes = engine.decode(prefix).ok()?;
    dimensions_from_bytes(&bytes)
}

fn header_config() -> GeneralPurposeConfig {
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true)
}

fn dimensions_from_bytes(bytes: &[u8]) -> Option<(u64, u64)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
        let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
        return Some((width.into(), height.into()));
    }
    if bytes.starts_with(&[0xFF, 0xD8]) {
        return jpeg_dimensions(bytes);
    }
    if bytes.len() >= 30 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return webp_dimensions(bytes);
    }
    None
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u64, u64)> {
    let mut index = 2;
    while index + 4 <= bytes.len() {
        if bytes[index] != 0xFF {
            index += 1;
            continue;
        }
        let marker = bytes[index + 1];
        if marker == 0xFF {
            index += 1;
            continue;
        }
        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            index += 2;
            continue;
        }
        let length = u16::from_be_bytes([bytes[index + 2], bytes[index + 3]]) as usize;
        let is_frame = (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_frame {
            let frame = bytes.get(index + 5..index + 9)?;
            let height = u16::from_be_bytes([frame[0], frame[1]]);
            let width = u16::from_be_bytes([frame[2], frame[3]]);
            return Some((width.into(), height.into()));
        }
        if length < 2 {
            return None;
        }
        index += 2 + length;
    }
    None
}

fn webp_dimensions(bytes: &[u8]) -> Option<(u64, u64)> {
    match bytes.get(12..16)? {
        b"VP8X" => {
            let w = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]) + 1;
            let h = u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]) + 1;
            Some((w.into(), h.into()))
        }
        b"VP8 " => {
            let w = u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3FFF;
            let h = u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3FFF;
            Some((w.into(), h.into()))
        }
        b"VP8L" => {
            let b = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
            Some((((b & 0x3FFF) + 1).into(), (((b >> 14) & 0x3FFF) + 1).into()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> String {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend(width.to_be_bytes());
        bytes.extend(height.to_be_bytes());
        bytes.extend([8, 6, 0, 0, 0]);
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn reads_png_dimensions_and_prices_a_screenshot_near_its_real_cost() {
        let (w, h) = dimensions_from_base64(&png(1440, 900)).unwrap();
        assert_eq!((w, h), (1440, 900));
        let cost = tokens_for_dimensions(ImageDetail::Original, w, h);
        assert!((1_300..=2_000).contains(&cost), "{cost}");
        assert!(
            tokens_for_dimensions(ImageDetail::Default, 4000, 3000)
                <= ANTHROPIC_MAX_IMAGE_TOKENS.max(OPENAI_MAX_PATCHES)
        );
        assert_eq!(
            tokens_for_dimensions(ImageDetail::Low, 4000, 3000),
            LOW_DETAIL_IMAGE_TOKENS
        );
    }

    #[test]
    fn original_detail_of_a_huge_image_is_bounded_but_larger_than_default() {
        let original = tokens_for_dimensions(ImageDetail::Original, 3840, 2160);
        let default = tokens_for_dimensions(ImageDetail::Default, 3840, 2160);
        assert!(original > default && original <= OPENAI_MAX_PATCHES_ORIGINAL);
    }

    #[test]
    fn reads_jpeg_and_webp_headers() {
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 0, 0, 0xFF, 0xC0, 0, 17, 8, 0x02, 0x58, 0x03, 0x20, 3,
        ];
        assert_eq!(dimensions_from_bytes(&jpeg), Some((800, 600)));
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X\n\0\0\0\0\0\0\0".to_vec();
        webp.extend([0x1F, 0x03, 0, 0x57, 0x02, 0]); // 800 x 600
        assert_eq!(dimensions_from_bytes(&webp), Some((800, 600)));
    }

    #[test]
    fn unreadable_headers_fall_back_to_the_conservative_cost() {
        assert_eq!(dimensions_from_base64("QUJD"), None);
        assert_eq!(dimensions_from_base64("!!!!"), None);
    }
}
