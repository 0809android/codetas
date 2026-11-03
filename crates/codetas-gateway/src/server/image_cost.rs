//! Provider-neutral inline image token cost.
//!
//! Providers bill an image by its pixels, not its bytes, and the bill is
//! bounded. A flat guess of 8,192 or 16,384 tokens per screenshot overstated
//! real cost about five times, so the gateway's budget filled long before the
//! provider (or Codex, which reads provider usage) thought the context was full.
//! Read the dimensions from the image header and price the image the way the
//! providers do, taking the larger of the OpenAI and Anthropic figures so one
//! number is safe for every protocol.

/// Fallback when the header cannot be read (unknown format, truncated data).
pub(super) const UNKNOWN_IMAGE_TOKENS: u64 = ANTHROPIC_HIGH_RES_MAX_TOKENS;
pub(super) const UNKNOWN_ORIGINAL_IMAGE_TOKENS: u64 = 4_096;
pub(super) const LOW_DETAIL_IMAGE_TOKENS: u64 = 1_024;
const OPENAI_PATCH_PIXELS: u64 = 32;
const OPENAI_MAX_PATCHES: u64 = 1_536;
const OPENAI_MAX_PATCHES_ORIGINAL: u64 = 10_000;
// Anthropic bills a 28x28 patch as one token after fitting the image to the
// model's edge and token limits (vision docs, "How Claude resizes and pads
// images"). Claude 4.7 and later models use the high-resolution limits.
const ANTHROPIC_PATCH_PIXELS: u64 = 28;
const ANTHROPIC_STANDARD_MAX_EDGE: u64 = 1_568;
const ANTHROPIC_STANDARD_MAX_TOKENS: u64 = 1_568;
const ANTHROPIC_HIGH_RES_MAX_EDGE: u64 = 2_576;
const ANTHROPIC_HIGH_RES_MAX_TOKENS: u64 = 4_784;
const MIN_IMAGE_TOKENS: u64 = 85;

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
    // Provider-neutral: the larger Anthropic tier is the safe upper bound.
    let anthropic = anthropic_tokens_for_dimensions(true, width, height);
    patches.min(openai_cap).max(anthropic).max(MIN_IMAGE_TOKENS)
}

/// How an estimate prices an image.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum ImageCostProfile {
    /// Unknown or non-Anthropic target: the larger of the OpenAI and Anthropic
    /// figures, so one number is safe for every protocol.
    #[default]
    Neutral,
    /// Anthropic Messages: the exact patch price for the model's tier.
    Anthropic { high_resolution: bool },
}

impl ImageCostProfile {
    /// Per-image cost that sizes the image-drop step. A constant of the target,
    /// so the step never changes with the history being sent.
    pub(super) fn step_cost_tokens(self) -> u64 {
        match self {
            Self::Anthropic {
                high_resolution: false,
            } => 2_048,
            Self::Anthropic {
                high_resolution: true,
            }
            | Self::Neutral => 4_096,
        }
    }

    pub(super) fn anthropic_for_model(model: &str) -> Self {
        Self::Anthropic {
            high_resolution: anthropic_high_resolution_model(model),
        }
    }
}

/// Claude 4.7 and later models are high-resolution; every older model is
/// standard. The vision docs name the tier by version only, so the older
/// families are listed explicitly and anything unrecognised is treated as
/// high-resolution: a larger estimate costs a few omitted images, a smaller
/// one risks an upstream rejection that stops the task.
pub(super) fn anthropic_high_resolution_model(model: &str) -> bool {
    const STANDARD: [&str; 12] = [
        "claude-3",
        "haiku-4",
        "sonnet-4-0",
        "sonnet-4-2025",
        "sonnet-4-5",
        "sonnet-4-6",
        "opus-4-0",
        "opus-4-1",
        "opus-4-2025",
        "opus-4-5",
        "opus-4-6",
        "claude-instant",
    ];
    let model = model.to_ascii_lowercase();
    !STANDARD.iter().any(|family| model.contains(family))
}

/// `ceil(w/28) * ceil(h/28)` after the largest aspect-preserving shrink that
/// keeps both sides within the tier's edge limit and the cost within its token
/// budget. Padding to a multiple of 28 counts toward the edge limit.
pub(super) fn anthropic_tokens_for_dimensions(
    high_resolution: bool,
    width: u64,
    height: u64,
) -> u64 {
    if width == 0 || height == 0 {
        return if high_resolution {
            UNKNOWN_IMAGE_TOKENS
        } else {
            ANTHROPIC_STANDARD_MAX_TOKENS
        };
    }
    let (max_edge, max_tokens) = if high_resolution {
        (ANTHROPIC_HIGH_RES_MAX_EDGE, ANTHROPIC_HIGH_RES_MAX_TOKENS)
    } else {
        (ANTHROPIC_STANDARD_MAX_EDGE, ANTHROPIC_STANDARD_MAX_TOKENS)
    };
    let patches =
        |w: u64, h: u64| w.div_ceil(ANTHROPIC_PATCH_PIXELS) * h.div_ceil(ANTHROPIC_PATCH_PIXELS);
    let fits = |w: u64, h: u64| {
        w.div_ceil(ANTHROPIC_PATCH_PIXELS) * ANTHROPIC_PATCH_PIXELS <= max_edge
            && h.div_ceil(ANTHROPIC_PATCH_PIXELS) * ANTHROPIC_PATCH_PIXELS <= max_edge
            && patches(w, h) <= max_tokens
    };
    if fits(width, height) {
        return patches(width, height);
    }
    // Largest long edge that fits; the short edge follows the aspect ratio.
    let (long, short) = if width >= height {
        (width, height)
    } else {
        (height, width)
    };
    let size_for = |long_edge: u64| {
        let short_edge = ((long_edge as f64) * (short as f64) / (long as f64))
            .round()
            .max(1.0) as u64;
        if width >= height {
            (long_edge, short_edge)
        } else {
            (short_edge, long_edge)
        }
    };
    let (mut low, mut high) = (1_u64, long.min(max_edge));
    while low < high {
        let mid = (low + high).div_ceil(2);
        let (w, h) = size_for(mid);
        if fits(w, h) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let (w, h) = size_for(low);
    patches(w, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translate::dimensions_from_base64;
    use base64::Engine as _;

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
                <= ANTHROPIC_HIGH_RES_MAX_TOKENS.max(OPENAI_MAX_PATCHES)
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
    fn unreadable_headers_fall_back_to_the_conservative_cost() {
        assert_eq!(dimensions_from_base64("QUJD"), None);
        assert_eq!(dimensions_from_base64("!!!!"), None);
    }

    #[test]
    fn anthropic_prices_28px_patches_within_each_tier() {
        // Documented example: 1920x1080 shrinks to 1456x819 on the standard tier.
        assert_eq!(anthropic_tokens_for_dimensions(false, 1920, 1080), 52 * 30);
        // Documented example: an A4 scan (1075x1520) is 39 x 55 = 2145 tokens and
        // is not resized on a high-resolution model.
        assert_eq!(anthropic_tokens_for_dimensions(true, 1075, 1520), 2_145);
        assert_eq!(anthropic_tokens_for_dimensions(false, 1075, 1520), 33 * 47);
        // Small images are not resized and have no minimum.
        assert_eq!(anthropic_tokens_for_dimensions(true, 200, 200), 64);
        // A Retina screenshot never exceeds the high-resolution budget.
        let retina = anthropic_tokens_for_dimensions(true, 2880, 1800);
        assert!(
            retina <= ANTHROPIC_HIGH_RES_MAX_TOKENS && retina > 4_000,
            "{retina}"
        );
    }

    #[test]
    fn model_tier_follows_the_claude_version() {
        for standard in [
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-sonnet-4-5-20250929",
            "claude-opus-4-5-20251101",
            "claude-haiku-4-5-20251001",
        ] {
            assert!(!anthropic_high_resolution_model(standard), "{standard}");
        }
        for high in [
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
            "claude-fable-5-1",
            "claude-mythos-5",
            "some-unknown-model",
        ] {
            assert!(anthropic_high_resolution_model(high), "{high}");
        }
    }
}
