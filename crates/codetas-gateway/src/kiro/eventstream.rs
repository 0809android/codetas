use super::*;

pub(crate) fn is_limit_stop_reason(reason: &str) -> bool {
    let reason = reason.to_ascii_uppercase();
    reason.contains("TOKEN") || reason.contains("LENGTH") || reason.contains("CONTEXT")
}

#[derive(Default)]
pub(crate) struct ToolOutput {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
}

#[derive(Default)]
pub(crate) struct KiroUsage {
    input: u64,
    output: u64,
    cached: u64,
    total: u64,
}

impl KiroUsage {
    pub(crate) fn update(&mut self, value: &Value) -> Result<(), String> {
        let number = |key: &str| -> Result<u64, String> {
            match value.get(key) {
                Some(value) => value
                    .as_u64()
                    .ok_or_else(|| format!("Kiro tokenUsage.{key} must be a non-negative integer")),
                None => Ok(0),
            }
        };
        let uncached = number("uncachedInputTokens")?;
        let cache_read = number("cacheReadInputTokens")?;
        let cache_write = number("cacheWriteInputTokens")?;
        self.input = uncached
            .saturating_add(cache_read)
            .saturating_add(cache_write);
        self.cached = cache_read;
        self.output = number("outputTokens")?;
        self.total = number("totalTokens")?.max(self.input.saturating_add(self.output));
        Ok(())
    }

    pub(crate) fn to_responses(&self) -> Value {
        json!({
            "input_tokens": self.input,
            "input_tokens_details": {"cached_tokens": self.cached},
            "output_tokens": self.output,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": self.total,
        })
    }
}

pub(crate) struct EventStreamFrame {
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) payload: Vec<u8>,
}

pub(crate) fn decode_eventstream(bytes: &[u8]) -> Result<Vec<EventStreamFrame>, String> {
    let mut offset = 0_usize;
    let mut frames = Vec::new();
    while offset < bytes.len() {
        if bytes.len() - offset < 16 {
            return Err("Kiro eventstream ended with a truncated frame".into());
        }
        let total = read_u32(&bytes[offset..offset + 4]) as usize;
        let headers_len = read_u32(&bytes[offset + 4..offset + 8]) as usize;
        if !(16..=MAX_EVENTSTREAM_FRAME).contains(&total) || offset + total > bytes.len() {
            return Err("Kiro eventstream frame length is invalid".into());
        }
        if headers_len > MAX_EVENTSTREAM_HEADERS || headers_len > total - 16 {
            return Err("Kiro eventstream header length is invalid".into());
        }
        let frame = &bytes[offset..offset + total];
        if crc32fast::hash(&frame[..8]) != read_u32(&frame[8..12]) {
            return Err("Kiro eventstream prelude CRC mismatch".into());
        }
        if crc32fast::hash(&frame[..total - 4]) != read_u32(&frame[total - 4..]) {
            return Err("Kiro eventstream message CRC mismatch".into());
        }
        frames.push(EventStreamFrame {
            headers: parse_eventstream_headers(&frame[12..12 + headers_len])?,
            payload: frame[12 + headers_len..total - 4].to_vec(),
        });
        offset += total;
    }
    Ok(frames)
}

pub(crate) fn drain_eventstream_frames(
    pending: &mut Vec<u8>,
    bytes: &[u8],
) -> Result<Vec<EventStreamFrame>, String> {
    pending.extend_from_slice(bytes);
    let mut frames = Vec::new();
    loop {
        if pending.len() < 12 {
            break;
        }
        let total = read_u32(&pending[..4]) as usize;
        let headers_len = read_u32(&pending[4..8]) as usize;
        if !(16..=MAX_EVENTSTREAM_FRAME).contains(&total) {
            return Err("Kiro eventstream frame length is invalid".into());
        }
        if headers_len > MAX_EVENTSTREAM_HEADERS || headers_len > total - 16 {
            return Err("Kiro eventstream header length is invalid".into());
        }
        if pending.len() < total {
            break;
        }
        let frame = pending.drain(..total).collect::<Vec<_>>();
        if crc32fast::hash(&frame[..8]) != read_u32(&frame[8..12]) {
            return Err("Kiro eventstream prelude CRC mismatch".into());
        }
        if crc32fast::hash(&frame[..total - 4]) != read_u32(&frame[total - 4..]) {
            return Err("Kiro eventstream message CRC mismatch".into());
        }
        frames.push(EventStreamFrame {
            headers: parse_eventstream_headers(&frame[12..12 + headers_len])?,
            payload: frame[12 + headers_len..total - 4].to_vec(),
        });
    }
    if pending.len() > MAX_EVENTSTREAM_FRAME {
        return Err("Kiro eventstream pending frame exceeded its limit".into());
    }
    Ok(frames)
}

fn parse_eventstream_headers(bytes: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let mut output = BTreeMap::new();
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let name_len = take_u8(bytes, &mut offset)? as usize;
        let name = take(bytes, &mut offset, name_len)?;
        let name = std::str::from_utf8(name)
            .map_err(|_| "Kiro eventstream header name is not UTF-8")?
            .to_string();
        let kind = take_u8(bytes, &mut offset)?;
        let value = match kind {
            0 => "true".into(),
            1 => "false".into(),
            2 => i8::from_be_bytes([take_u8(bytes, &mut offset)?]).to_string(),
            3 => i16::from_be_bytes(take_array::<2>(bytes, &mut offset)?).to_string(),
            4 => i32::from_be_bytes(take_array::<4>(bytes, &mut offset)?).to_string(),
            5 | 8 => i64::from_be_bytes(take_array::<8>(bytes, &mut offset)?).to_string(),
            6 | 7 => {
                let len = u16::from_be_bytes(take_array::<2>(bytes, &mut offset)?) as usize;
                let value = take(bytes, &mut offset, len)?;
                if kind == 7 {
                    std::str::from_utf8(value)
                        .map_err(|_| "Kiro eventstream string header is not UTF-8")?
                        .to_string()
                } else {
                    STANDARD.encode(value)
                }
            }
            9 => {
                let value = take(bytes, &mut offset, 16)?;
                format!(
                    "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                    value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
                    value[8], value[9], value[10], value[11], value[12], value[13], value[14], value[15]
                )
            }
            _ => return Err("Kiro eventstream header type is unsupported".into()),
        };
        output.insert(name, value);
    }
    Ok(output)
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn take_u8(bytes: &[u8], offset: &mut usize) -> Result<u8, String> {
    let value = *bytes
        .get(*offset)
        .ok_or("Kiro eventstream header is truncated")?;
    *offset += 1;
    Ok(value)
}

fn take<'a>(bytes: &'a [u8], offset: &mut usize, len: usize) -> Result<&'a [u8], String> {
    let end = offset
        .checked_add(len)
        .filter(|end| *end <= bytes.len())
        .ok_or("Kiro eventstream header is truncated")?;
    let value = &bytes[*offset..end];
    *offset = end;
    Ok(value)
}

fn take_array<const N: usize>(bytes: &[u8], offset: &mut usize) -> Result<[u8; N], String> {
    take(bytes, offset, N)?
        .try_into()
        .map_err(|_| "Kiro eventstream header is truncated".into())
}

