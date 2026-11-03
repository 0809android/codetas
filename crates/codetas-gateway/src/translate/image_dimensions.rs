//! Image dimensions read from the first bytes of an inline base64 image.

use base64::{
    alphabet,
    engine::{general_purpose::GeneralPurposeConfig, DecodePaddingMode, GeneralPurpose},
    Engine as _,
};

/// Decoded bytes inspected for the header. JPEG may carry large EXIF/ICC
/// segments before its frame header.
const HEADER_BASE64_CHARS: usize = 180_000;

/// `payload` is the base64 part of a data URL (after the comma).
pub(crate) fn dimensions_from_base64(payload: &str) -> Option<(u64, u64)> {
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
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        let width = u16::from_le_bytes(bytes.get(6..8)?.try_into().ok()?);
        let height = u16::from_le_bytes(bytes.get(8..10)?.try_into().ok()?);
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
    fn reads_gif_headers() {
        let gif = [
            b'G', b'I', b'F', b'8', b'9', b'a', 0x20, 0x03, 0x58, 0x02, 0, 0,
        ];
        assert_eq!(dimensions_from_bytes(&gif), Some((800, 600)));
    }
}
