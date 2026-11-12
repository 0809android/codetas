use super::*;

pub(super) async fn require_compaction_on_image_rejection(
    first: reqwest::Response,
    first_status: StatusCode,
) -> Result<reqwest::Response, AttemptFailure> {
    let sent_images = first
        .extensions()
        .get::<SentImageState>()
        .map_or(0, |sent| sent.sent_images);
    if sent_images == 0 {
        return Ok(first);
    }
    let (mut first, error_bytes, error_value) = snapshot_provider_error(first, 64 * 1024).await?;
    if first_status != StatusCode::PAYLOAD_TOO_LARGE
        && !provider_error_is_image_history_limit(&error_bytes, error_value.as_ref())
    {
        return Ok(first);
    }
    // Do not delete an older image and charge for another generation attempt.
    // The caller must establish a new compacted history explicitly instead.
    let mut retry = first
        .extensions_mut()
        .remove::<ProviderRetryObservation>()
        .unwrap_or_default();
    let diagnostic = first
        .extensions()
        .get::<crate::observability::RequestDiagnostic>()
        .cloned();
    let lease = take_routing_attempt_lease(&mut first);
    let classified = upstream_responses_error_classified(first, None).await;
    retry.usage = sum_token_usage(retry.usage, classified.usage);
    let mut failure = failure_with_provider_metadata(image_history_compaction_required(
        "provider rejected the image history; compact the conversation before continuing (no images were pruned and no reduced-history retry was sent)",
    ), Some(&retry), diagnostic.as_ref());
    if let Some(error) = classified
        .response
        .extensions()
        .get::<UpstreamErrorDiagnostic>()
    {
        failure.response.extensions_mut().insert(error.clone());
    }
    if let Some(lease) = lease {
        failure.response.extensions_mut().insert(lease);
    }
    Err(failure)
}

fn provider_error_is_image_history_limit(bytes: &[u8], parsed: Option<&Value>) -> bool {
    if !provider_error_looks_like_image_rejection(bytes, parsed) {
        return false;
    }
    // Invalid formats or unsupported dimensions are not fixed by compaction.
    // Keep their original errors instead of inducing a compact/retry loop.
    let text = parsed
        .map(Value::to_string)
        .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned())
        .to_ascii_lowercase();
    [
        "too many images",
        "too_many_images",
        "number of images",
        "max_images",
        "max_num_images",
        "image count",
        "total image",
        "total inline",
        "request too large",
        "request size",
        "payload too large",
        "payload_too_large",
        "max_request",
        "画像数",
        "画像の合計",
        "画像が多",
        "画像枚数",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

pub(super) fn provider_error_looks_like_image_rejection(
    bytes: &[u8],
    parsed: Option<&Value>,
) -> bool {
    if looks_like_html_error_body(bytes) {
        return false;
    }
    let mut fragments = Vec::new();
    if let Some(value) = parsed {
        collect_provider_error_fragments(value, &mut fragments);
    } else if let Ok(value) = serde_json::from_slice::<Value>(bytes) {
        collect_provider_error_fragments(&value, &mut fragments);
    }
    let informative = fragments
        .iter()
        .map(|text| text.trim())
        .filter(|text| !text.is_empty() && !is_generic_provider_error_token(text))
        .collect::<Vec<_>>();
    if informative.is_empty() {
        let raw = String::from_utf8_lossy(bytes);
        if raw.trim().is_empty()
            || (json_error_envelope_is_generic(parsed, bytes)
                && !has_generic_error_message(&fragments))
        {
            // Providers often strip the 400 body, return only
            // `invalid_request_error`, or wrap that in envelope fields such as
            // `request_id`. After images were sent, still allow one tightened
            // retry instead of failing closed. A present generic message such as
            // "invalid request" is a named non-image cause, not a stripped body.
            return true;
        }
        return haystack_looks_like_image_rejection(&raw.to_ascii_lowercase());
    }
    haystack_looks_like_image_rejection(&informative.join("\n").to_ascii_lowercase())
}

fn has_generic_error_message(fragments: &[String]) -> bool {
    fragments.iter().any(|text| {
        let trimmed = text.trim();
        !trimmed.is_empty() && !is_generic_error_classification(trimmed)
    })
}

fn json_error_envelope_is_generic(parsed: Option<&Value>, bytes: &[u8]) -> bool {
    let Some(value) = parsed
        .cloned()
        .or_else(|| serde_json::from_slice::<Value>(bytes).ok())
    else {
        return false;
    };
    let Value::Object(object) = value else {
        return false;
    };
    json_object_is_generic_error_envelope(&object)
}

fn json_object_is_generic_error_envelope(object: &serde_json::Map<String, Value>) -> bool {
    const ERROR_MARKERS: &[&str] = &["error", "errors", "message", "msg", "type", "code"];
    object
        .keys()
        .any(|key| ERROR_MARKERS.contains(&key.as_str()))
        && json_value_is_generic_error_payload(&Value::Object(object.clone()))
}

fn json_value_is_generic_error_payload(value: &Value) -> bool {
    const ENVELOPE_KEYS: &[&str] = &[
        "error",
        "errors",
        "message",
        "msg",
        "type",
        "code",
        "status",
        "request_id",
        "id",
        "object",
        "created",
        "param",
        "param_name",
        "reason",
        "error_type",
        "detail",
        "details",
    ];
    match value {
        Value::Object(object) => {
            object
                .keys()
                .all(|key| ENVELOPE_KEYS.contains(&key.as_str()))
                && object.values().all(json_value_is_generic_error_payload)
        }
        Value::Array(items) => items.iter().all(json_value_is_generic_error_payload),
        Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => true,
    }
}

fn haystack_looks_like_image_rejection(haystack: &str) -> bool {
    let haystack = strip_urls_for_error_scan(haystack);
    if contains_cjk_image_rejection(&haystack) {
        return true;
    }
    const EXACT_TOKENS: &[&str] = &["vision", "multimodal"];
    const PIXEL_TOKENS: &[&str] = &["pixel", "pixels"];
    const BARE_IMAGE_TOKENS: &[&str] = &["image", "images"];
    const IMAGE_COMPANIONS: &[&str] = &[
        "too",
        "large",
        "size",
        "sizes",
        "many",
        "count",
        "limit",
        "limits",
        "exceed",
        "exceeded",
        "invalid",
        "unsupported",
        "format",
        "dimension",
        "dimensions",
        "width",
        "height",
        "bytes",
        "payload",
        "media",
        "mime",
        "resolution",
    ];
    let tokens = provider_error_tokens(&haystack);
    let mut saw_bare_image = false;
    let mut saw_companion = false;
    for token in &tokens {
        if is_generation_or_edit_image_token(token) || is_non_vision_image_token(token) {
            continue;
        }
        if EXACT_TOKENS.contains(&token.as_str())
            || suffix_token_matches(token, PIXEL_TOKENS)
            || is_invalid_image_code(token)
            || is_strong_input_image_token(token)
        {
            return true;
        }
        if BARE_IMAGE_TOKENS.contains(&token.as_str()) {
            saw_bare_image = true;
        }
        if IMAGE_COMPANIONS.contains(&token.as_str()) {
            saw_companion = true;
        }
    }
    saw_bare_image && saw_companion
}

fn contains_cjk_image_rejection(haystack: &str) -> bool {
    if haystack.contains("像素") || haystack.contains("画素") {
        return true;
    }
    const IMAGE: &[&str] = &["图片", "图像", "画像"];
    const COMPANION: &[&str] = &[
        "尺寸",
        "大小",
        "过大",
        "太大",
        "超限",
        "超過",
        "超える",
        "サイズ",
        "大きすぎ",
        "大き過ぎ",
        "太多",
        "多すぎ",
        "枚数",
        "上限",
    ];
    if IMAGE.iter().any(|marker| haystack.contains(marker))
        && COMPANION.iter().any(|marker| haystack.contains(marker))
    {
        return true;
    }
    false
}

fn is_invalid_image_code(token: &str) -> bool {
    token == "invalid_image"
        || token
            .strip_prefix("invalid_image_")
            .is_some_and(|rest| !rest.is_empty())
}

fn suffix_token_matches(token: &str, needles: &[&str]) -> bool {
    needles.iter().copied().any(|needle| {
        token == needle
            || token
                .strip_suffix(needle)
                .is_some_and(|prefix| prefix.ends_with('_'))
    })
}

fn is_strong_input_image_token(token: &str) -> bool {
    matches!(token, "image_url" | "input_image" | "image_too_large") || is_image_limit_token(token)
}

fn is_image_limit_token(token: &str) -> bool {
    let image_like = token.starts_with("image_")
        || token.starts_with("images_")
        || token.contains("_image_")
        || token.ends_with("_image")
        || token.ends_with("_images")
        || token.ends_with("_image_url")
        || token.ends_with("_input_image");
    image_like
        && token.split('_').any(|part| {
            matches!(
                part,
                "max"
                    | "too"
                    | "many"
                    | "invalid"
                    | "oversize"
                    | "parse"
                    | "limit"
                    | "pixel"
                    | "pixels"
                    | "large"
                    | "exceed"
                    | "exceeded"
            )
        })
}

fn looks_like_html_error_body(bytes: &[u8]) -> bool {
    let trimmed = String::from_utf8_lossy(bytes)
        .trim_start_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
        .to_ascii_lowercase();
    if trimmed.is_empty() {
        // A stripped (empty) body is not HTML; callers treat it as unknown.
        return false;
    }
    match skip_leading_html_comments(&trimmed) {
        None => true,
        Some(rest) => rest.is_empty() || rest.starts_with('<'),
    }
}

fn skip_leading_html_comments(text: &str) -> Option<&str> {
    let mut rest = text.trim_start();
    while let Some(inner) = rest.strip_prefix("<!--") {
        let index = inner.find("-->")?;
        rest = inner[index + 3..].trim_start();
    }
    Some(rest)
}

fn strip_urls_for_error_scan(haystack: &str) -> String {
    if !haystack.contains("://") {
        return haystack.to_string();
    }
    let mut output = String::with_capacity(haystack.len());
    let mut rest = haystack;
    while let Some(separator) = rest.find("://") {
        let prefix = &rest[..separator];
        let scheme_start = prefix
            .rfind(|ch: char| !ch.is_ascii_alphabetic())
            .map(|index| index + 1)
            .unwrap_or(0);
        output.push_str(&prefix[..scheme_start]);
        let after = &rest[separator + 3..];
        let url_end = after
            .find(|ch: char| {
                ch.is_ascii_whitespace()
                    || matches!(ch, '"' | '\'' | '<' | '>' | ')' | ']' | '}' | ',')
            })
            .unwrap_or(after.len());
        rest = &after[url_end..];
    }
    output.push_str(rest);
    output
}

fn is_non_vision_image_token(token: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "docker_image",
        "container_image",
        "runtime_image",
        "disk_image",
        "ami_image",
        "os_image",
        "machine_image",
        "non_image",
        "not_an_image",
        "not_image",
        "no_image",
    ];
    PREFIXES.iter().any(|prefix| {
        token == *prefix
            || token
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('_'))
    })
}

fn is_generation_or_edit_image_token(token: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "image_generation",
        "imagegen",
        "imagen",
        "image_edit",
        "image_edits",
    ];
    PREFIXES.iter().any(|prefix| {
        token == *prefix
            || token
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('_'))
            || token.split_once(prefix).is_some_and(|(head, tail)| {
                (head.is_empty() || head.ends_with('_'))
                    && (tail.is_empty() || tail.starts_with('_'))
            })
    })
}

fn provider_error_tokens(haystack: &str) -> Vec<String> {
    haystack
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn is_generic_provider_error_token(text: &str) -> bool {
    is_generic_error_classification(text) || text.trim().eq_ignore_ascii_case("invalid request")
}

fn is_generic_error_classification(text: &str) -> bool {
    let normalized = text.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "" | "error"
            | "message"
            | "type"
            | "code"
            | "invalid_request_error"
            | "invalid_request"
            | "bad_request"
            | "invalid_argument"
            | "400"
            | "null"
    )
}

fn collect_provider_error_fragments(value: &Value, fragments: &mut Vec<String>) {
    const TEXT_KEYS: &[&str] = &[
        "message",
        "msg",
        "errmsg",
        "err_msg",
        "error_msg",
        "error_message",
        "code",
        "type",
        "error_type",
        "param",
        "param_name",
        "reason",
        "info",
        "description",
        "detail",
        "details",
    ];
    const NESTED_KEYS: &[&str] = &[
        "error", "errors", "detail", "details", "data", "cause", "inner",
    ];
    match value {
        Value::Object(object) => {
            for key in TEXT_KEYS {
                if let Some(text) = object.get(*key).and_then(Value::as_str) {
                    fragments.push(text.to_string());
                }
            }
            for key in NESTED_KEYS {
                if let Some(child) = object.get(*key) {
                    collect_provider_error_fragments(child, fragments);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_provider_error_fragments(item, fragments);
            }
        }
        Value::String(text) => fragments.push(text.clone()),
        _ => {}
    }
}

async fn snapshot_provider_error(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<(reqwest::Response, Vec<u8>, Option<Value>), AttemptFailure> {
    let status = response.status();
    let version = response.version();
    let headers = response.headers().clone();
    let retry_observation = response
        .extensions()
        .get::<ProviderRetryObservation>()
        .cloned();
    let routing_attempt_lease = take_routing_attempt_lease(&mut response);
    let request_diagnostic = response
        .extensions()
        .get::<crate::observability::RequestDiagnostic>()
        .cloned();
    let sent_image_state = response.extensions().get::<SentImageState>().copied();
    let mut stream = response.bytes_stream();
    let mut output = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            break;
        };
        if output.len() >= limit {
            break;
        }
        let take = (limit - output.len()).min(chunk.len());
        output.extend_from_slice(&chunk[..take]);
    }
    let value = serde_json::from_slice(&output).ok();
    let mut headers = headers;
    headers.remove(header::CONTENT_LENGTH);
    headers.remove(header::TRANSFER_ENCODING);
    if let Ok(length) = HeaderValue::from_str(&output.len().to_string()) {
        headers.insert(header::CONTENT_LENGTH, length);
    }
    let mut builder = axum::http::Response::builder()
        .status(status)
        .version(version);
    if let Some(target) = builder.headers_mut() {
        *target = headers;
    }
    let mut rebuilt = builder
        .body(reqwest::Body::from(output.clone()))
        .map(reqwest::Response::from)
        .map_err(|_| request_failure("gateway_error", "failed to rebuild provider response"))?;
    if let Some(observation) = retry_observation {
        rebuilt.extensions_mut().insert(observation);
    }
    if let Some(lease) = routing_attempt_lease {
        rebuilt.extensions_mut().insert(lease);
    }
    if let Some(diagnostic) = request_diagnostic {
        rebuilt.extensions_mut().insert(diagnostic);
    }
    if let Some(sent) = sent_image_state {
        rebuilt.extensions_mut().insert(sent);
    }
    Ok((rebuilt, output, value))
}
