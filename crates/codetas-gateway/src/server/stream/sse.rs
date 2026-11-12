use super::*;

pub(crate) fn drain_sse_values(pending: &mut Vec<u8>, bytes: &[u8]) -> Result<Vec<Value>, String> {
    drain_sse_values_inner(pending, bytes, false)
}

// Only the physical Chat guard needs the wire terminator. Translators keep the
// existing behavior and never expose this internal marker to clients.
fn drain_sse_values_inner(
    pending: &mut Vec<u8>,
    bytes: &[u8],
    include_done: bool,
) -> Result<Vec<Value>, String> {
    drain_sse_results(pending, bytes, include_done)
        .into_iter()
        .collect()
}

pub(crate) fn drain_sse_results(
    pending: &mut Vec<u8>,
    bytes: &[u8],
    include_done: bool,
) -> Vec<Result<Value, String>> {
    pending.extend_from_slice(bytes);
    let mut values = Vec::new();
    while let Some((boundary, separator_len)) = sse_frame_boundary(pending) {
        let frame = pending.drain(..boundary).collect::<Vec<_>>();
        pending.drain(..separator_len);
        if include_done
            && std::str::from_utf8(&frame).ok().is_some_and(|frame| {
                frame.lines().any(|line| {
                    line.strip_prefix("data:")
                        .is_some_and(|data| data.trim() == "[DONE]")
                })
            })
        {
            values.push(Ok(json!({"type":"codetas.chat_stream_end"})));
        } else {
            match parse_sse_frame(&frame) {
                Ok(Some(value)) => values.push(Ok(value)),
                Ok(None) => {}
                Err(error) => {
                    values.push(Err(error));
                    break;
                }
            }
        }
    }
    values
}

pub(super) fn parse_sse_frame(frame: &[u8]) -> Result<Option<Value>, String> {
    let frame =
        std::str::from_utf8(frame).map_err(|_| "SSE frame is not valid UTF-8".to_string())?;
    let normalized = frame.replace("\r\n", "\n");
    let data = normalized
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() || data == "[DONE]" {
        return Ok(None);
    }
    serde_json::from_str::<Value>(&data)
        .map(Some)
        .map_err(|error| format!("invalid JSON data: {error}"))
}

pub(crate) fn sse_frame_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let lf = bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2));
    let crlf = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4));
    match (lf, crlf) {
        (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
        (Some(boundary), None) | (None, Some(boundary)) => Some(boundary),
        (None, None) => None,
    }
}

// Accounting accepts only complete, validated frames and never emits lifecycle events.
#[derive(Default)]
pub(crate) struct SseUsageCollector {
    pending: Vec<u8>,
    pub(crate) usage: TokenUsage,
    invalid: bool,
    uncached: u64,
    cached: u64,
    created: u64,
}
impl SseUsageCollector {
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        if self.invalid {
            return;
        }
        for result in drain_sse_results(&mut self.pending, bytes, false) {
            match result {
                Ok(value) => self.observe(&value),
                Err(_) => {
                    self.invalid = true;
                    self.pending.clear();
                    break;
                }
            }
        }
    }
    fn observe(&mut self, value: &Value) {
        self.usage.merge_max(TokenUsage::from_json(value));
        if matches!(
            value.get("type").and_then(Value::as_str),
            Some("message_start" | "message_delta")
        ) {
            if let Some(raw) = value
                .pointer("/message/usage")
                .or_else(|| value.get("usage"))
            {
                if let Some(n) = raw.get("input_tokens").and_then(Value::as_u64) {
                    self.uncached = n;
                }
                if let Some(n) = raw.get("cache_read_input_tokens").and_then(Value::as_u64) {
                    self.cached = n;
                }
                if let Some(n) = raw
                    .get("cache_creation_input_tokens")
                    .and_then(Value::as_u64)
                {
                    self.created = n;
                }
                self.usage.input_tokens = self
                    .uncached
                    .saturating_add(self.cached)
                    .saturating_add(self.created);
                self.usage.cache_creation_input_tokens = self.created;
                self.usage.total_tokens = self.usage.total_tokens.max(
                    self.usage
                        .input_tokens
                        .saturating_add(self.usage.output_tokens),
                );
            }
        }
    }
}

pub(crate) async fn read_provider_bytes_with_usage(
    mut response: reqwest::Response,
    limit: u64,
) -> Result<Vec<u8>, (String, TokenUsage)> {
    let _lease = take_routing_attempt_lease(&mut response);
    let mut source = response.bytes_stream();
    let mut bytes = Vec::new();
    let mut collector = SseUsageCollector::default();
    while let Some(chunk) = source.next().await {
        let chunk = chunk.map_err(|_| {
            (
                "provider response body read failed".into(),
                provider_prefix_usage(&bytes, &collector),
            )
        })?;
        let take = chunk
            .len()
            .min(usize::try_from(limit.saturating_sub(bytes.len() as u64)).unwrap_or(usize::MAX));
        collector.push(&chunk[..take]);
        bytes.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            return Err((
                "provider response exceeded the configured limit".into(),
                provider_prefix_usage(&bytes, &collector),
            ));
        }
    }
    Ok(bytes)
}
pub(crate) fn provider_prefix_usage(bytes: &[u8], collector: &SseUsageCollector) -> TokenUsage {
    let mut usage = collector.usage.clone();
    if let Some(Ok(value)) = serde_json::Deserializer::from_slice(bytes)
        .into_iter::<Value>()
        .next()
    {
        usage.merge_max(TokenUsage::from_json(&value));
    }
    usage
}
pub(crate) async fn bounded_json_with_usage(
    response: reqwest::Response,
    limit: u64,
) -> Result<Value, (String, TokenUsage)> {
    let bytes = read_provider_bytes_with_usage(response, limit).await?;
    serde_json::from_slice(&bytes).map_err(|error| {
        let mut collector = SseUsageCollector::default();
        collector.push(&bytes);
        (error.to_string(), provider_prefix_usage(&bytes, &collector))
    })
}
