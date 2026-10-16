use super::*;

pub(crate) fn image_history_failure(error: crate::translate::ImageHistoryError) -> AttemptFailure {
    match error {
        crate::translate::ImageHistoryError::BudgetExceeded { .. } | crate::translate::ImageHistoryError::OmissionRequired => {
            image_history_compaction_required(&error.to_string())
        }
        crate::translate::ImageHistoryError::Worker(_) => {
            request_failure("image_normalization_failed", &error.to_string())
        }
    }
}

fn image_history_compaction_required(message: &str) -> AttemptFailure {
    let mut failure = request_failure("context_length_exceeded", message);
    failure.response =
        candidate_policy_error_response(&CandidatePolicyError::InputBudget(message.into()));
    failure
}

#[cfg(test)]
mod image_budget_boundary_tests {
    use super::*;

    #[tokio::test]
    async fn image_size_rejections_require_compaction_without_a_reduced_history_retry() {
        for status in [StatusCode::BAD_REQUEST, StatusCode::PAYLOAD_TOO_LARGE] {
            let upstream = Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "application/json")
                .body(reqwest::Body::from(
                    r#"{"error":{"message":"too many images"}}"#,
                ))
                .unwrap();
            let mut upstream = reqwest::Response::from(upstream);
            upstream.extensions_mut().insert(SentImageState {
                omitted_before_send: 0,
                sent_images: 5,
            });
            // This handler accepts neither a client nor a mutable request body:
            // it cannot prune history or issue another upstream request.
            let failure = require_compaction_on_image_rejection(upstream, status)
                .await
                .expect_err("image budget rejection must require a compaction boundary");
            assert_eq!(failure.response.status(), StatusCode::BAD_REQUEST);
            let bytes = to_bytes(failure.response.into_body(), 16 * 1024)
                .await
                .unwrap();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["error"]["code"], "context_length_exceeded");
            assert_eq!(value["error"]["type"], "invalid_request_error");
            assert_eq!(value["error"]["param"], "input");
        }
    }

    #[tokio::test]
    async fn unrelated_provider_errors_are_not_relabelled_as_image_limits() {
        for message in [
            "invalid tool schema",
            "invalid image format",
            "unsupported image dimensions",
        ] {
            let text = json!({"error": {"message": message}}).to_string();
            let upstream = Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::CONTENT_TYPE, "application/json")
                .body(reqwest::Body::from(text.clone()))
                .unwrap();
            let mut upstream = reqwest::Response::from(upstream);
            upstream.extensions_mut().insert(SentImageState {
                omitted_before_send: 0,
                sent_images: 5,
            });
            let response = require_compaction_on_image_rejection(upstream, StatusCode::BAD_REQUEST)
                .await
                .expect("unrelated error remains unchanged");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                read_bounded(response, 16 * 1024).await.unwrap(),
                text.as_bytes()
            );
        }
    }

    #[tokio::test]
    async fn local_image_budget_uses_the_same_context_window_error_contract() {
        let failure = image_history_failure(crate::translate::ImageHistoryError::BudgetExceeded {
            required: 8192,
            limit: 4096,
        });
        assert_eq!(failure.response.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(failure.response.into_body(), 16 * 1024)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "context_length_exceeded");
        assert_eq!(value["error"]["param"], "input");
    }
}

#[derive(Clone, Copy)]
struct SentImageState {
    omitted_before_send: usize,
    sent_images: usize,
}

#[derive(Clone)]
pub(crate) struct EmptyCompletionRecoveryFailure {
    pub(crate) usage: TokenUsage,
    pub(crate) additional_sends: u16,
}

#[derive(Clone, Copy)]
pub(crate) struct EmptyCompletionRecoverySuccess {
    pub(crate) additional_sends: u16,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ProviderRetryObservation {
    pub(crate) thinking_provenance: Option<super::anthropic_compact::ThinkingProvenance>,
    pub(crate) known_billing:Option<TokenUsage>,
    pub(crate) request_diagnostic: Option<crate::observability::RequestDiagnostic>,
    pub(crate) terminal_failure:bool,
    pub(crate) cache_observation: Option<crate::observability::CacheObservation>,
    pub(crate) additional_sends: u16,
    pub(crate) recovery_kinds: Vec<String>,
    pub(crate) usage: TokenUsage,
}

#[derive(Clone, Default)]
pub(crate) struct SharedProviderRetryObservation(
    pub(crate) Arc<std::sync::Mutex<ProviderRetryObservation>>,
);

#[derive(Clone)]
pub(crate) struct RoutingAttemptLease(Arc<RoutingAttemptLeaseInner>);

struct RoutingAttemptLeaseInner {
    routing: Arc<Mutex<RoutingRuntime>>,
    candidate: RouteCandidate,
}

impl Drop for RoutingAttemptLeaseInner {
    fn drop(&mut self) {
        // Prefer a synchronous release so Drop never depends solely on a live
        // Tokio runtime. Fall back to an async unlock only when contended.
        if let Ok(mut routing) = self.routing.try_lock() {
            routing.end_attempt(&self.candidate);
            return;
        }
        let routing = Arc::clone(&self.routing);
        let candidate = self.candidate.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                routing.lock().await.end_attempt(&candidate);
            });
            return;
        }
        // Last resort: block for the mutex when no runtime is available.
        // This path is rare (tests / teardown) but must not leak active leases.
        let mut routing = self.routing.blocking_lock();
        routing.end_attempt(&candidate);
    }
}

pub(crate) fn take_routing_attempt_lease(
    response: &mut reqwest::Response,
) -> Option<RoutingAttemptLease> {
    response.extensions_mut().remove::<RoutingAttemptLease>()
}

pub(crate) fn validate_candidate_image_references(body: &Value, candidate: &RouteCandidate) -> Result<(), AttemptFailure> {
    crate::compaction::rendered_compaction_view(body)
        .map_err(|message| request_failure("invalid_compaction_history", &message))?;
    if candidate.provider.transport != ProviderTransport::Standard
        || candidate.provider.protocol_for_model(&candidate.upstream_model) != ProviderProtocol::Responses {
        crate::translate::validate_translated_image_references(body)
            .map_err(|message| request_failure("unsupported_request", &message))?;
    }
    Ok(())
}

pub(crate) async fn send_candidate(
    state: &GatewayState,
    body: &mut Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
) -> Result<reqwest::Response, AttemptFailure> {
    validate_candidate_image_references(body, candidate)?;
    let cloud_request_id = (candidate.provider.google_mode == GoogleMode::CloudCodeAssist)
        .then(|| format!("agent-{}", Uuid::new_v4()));
    let cloud_envelope_cache = cloud_request_id
        .as_ref()
        .map(|_| Arc::new(Mutex::new(None::<Value>)));
    let remote_compaction = request_is_remote_compaction(body);
    let protocol = candidate
        .provider
        .protocol_for_model(&candidate.upstream_model);
    crate::compaction::validate_local_compactions(body)
        .map_err(|message| request_failure("invalid_compaction_history", &message))?;
    crate::anthropic_compaction::expand_retained(body)
        .map_err(|message|request_failure("invalid_compaction_history",&message))?;
    let strip_unsupported_images = state.settings.read().await.agents.image_input_mode
        != crate::config::AuxiliaryInputMode::Native;
    apply_provider_request_compatibility(body, candidate, protocol, strip_unsupported_images);
    if !remote_compaction {
        let image_report = normalize_translated_image_history(
            body,
            protocol,
            candidate.provider.transport,
            candidate.provider.limits.max_request_bytes,
        )
        .await
        .map_err(image_history_failure)?;
        if image_report.omitted_images > 0 {
            crate::debug::log(&format!(
                "image history normalized: provider={} model={} omitted={} total={} kept_base64_chars={}",
                candidate.provider.id,
                candidate.upstream_model,
                image_report.omitted_images,
                image_report.inline_images,
                image_report.kept_base64_chars,
            ));
        }
    }
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut first = {
        let prepared_body: &Value = body;
        retry_provider_request(state, candidate, streaming, || {
            send_candidate_once(
                state,
                prepared_body,
                candidate,
                caller_headers,
                cloud_request_id.as_deref(),
                cloud_envelope_cache.as_ref(),
            )
        })
        .await?
    };
    first =
        if matches!(
            first.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) && candidate_oauth_credential(candidate)
        {
            match crate::oauth::force_refresh_oauth_access_token(&candidate.provider.id).await {
                Ok(_) => {
                    crate::debug::log(&format!(
                    "provider returned {}; refreshed OAuth and retrying once: provider={} model={}",
                    first.status(), candidate.provider.id, candidate.upstream_model,
                ));
                    let (recovery, first_diagnostic, first_request_diagnostic) =
                        retire_oauth_response(first).await;
                    let prepared_body: &Value = body;
                    match retry_provider_request(state, candidate, streaming, || {
                        send_candidate_once(
                            state,
                            prepared_body,
                            candidate,
                            caller_headers,
                            cloud_request_id.as_deref(),
                            cloud_envelope_cache.as_ref(),
                        )
                    })
                    .await
                    {
                        Ok(mut response) => {
                            let nested = response
                                .extensions_mut()
                                .remove::<ProviderRetryObservation>();
                            let mut combined = Some(recovery);
                            merge_provider_retry_observation(&mut combined, nested);
                            if let Some(combined) = combined {
                                response.extensions_mut().insert(combined);
                            }
                            if let Some(diagnostic) = first_diagnostic {
                                response.extensions_mut().insert(diagnostic);
                            }
                            response
                        }
                        Err(mut failure) => {
                            let nested = failure.response.extensions_mut().remove::<ProviderRetryObservation>();
                            let mut combined = Some(recovery);
                            merge_provider_retry_observation(&mut combined, nested);
                            failure = failure_with_provider_metadata(failure, combined.as_ref(), first_request_diagnostic.as_ref());
                            if let Some(diagnostic) = first_diagnostic {
                                failure.response.extensions_mut().insert(diagnostic);
                            }
                            return Err(failure);
                        }
                    }
                }
                Err(error) => {
                    crate::debug::log(&format!(
                        "OAuth refresh after provider auth rejection failed: provider={} error={}",
                        candidate.provider.id, error,
                    ));
                    let mut recovery = first
                        .extensions_mut()
                        .remove::<ProviderRetryObservation>()
                        .unwrap_or_default();
                    recovery.recovery_kinds.push("oauth-refresh-failed".into());
                    first.extensions_mut().insert(recovery);
                    first
                }
            }
        } else {
            first
        };
    let first_status = first.status();
    let image_rejection_retry = matches!(
        first_status,
        StatusCode::PAYLOAD_TOO_LARGE | StatusCode::BAD_REQUEST
    ) && !remote_compaction;
    let response = if image_rejection_retry {
        require_compaction_on_image_rejection(first, first_status).await?
    } else {
        first
    };
    guard_empty_completion_response(
        state,
        body,
        candidate,
        caller_headers,
        streaming,
        response,
        cloud_request_id,
        cloud_envelope_cache,
    )
    .await
}

fn candidate_oauth_credential(candidate: &RouteCandidate) -> bool {
    candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source
        == CredentialSource::OAuth
}

async fn require_compaction_on_image_rejection(
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
    let mut retry = first.extensions_mut().remove::<ProviderRetryObservation>().unwrap_or_default();
    let diagnostic = first.extensions().get::<crate::observability::RequestDiagnostic>().cloned();
    let lease = take_routing_attempt_lease(&mut first);
    let classified = upstream_responses_error_classified(first, None).await;
    retry.usage = sum_token_usage(retry.usage, classified.usage);
    let mut failure = failure_with_provider_metadata(image_history_compaction_required(
        "provider rejected the image history; compact the conversation before continuing (no images were pruned and no reduced-history retry was sent)",
    ), Some(&retry), diagnostic.as_ref());
    if let Some(error) = classified.response.extensions().get::<UpstreamErrorDiagnostic>() {
        failure.response.extensions_mut().insert(error.clone());
    }
    if let Some(lease) = lease { failure.response.extensions_mut().insert(lease); }
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

fn provider_error_looks_like_image_rejection(bytes: &[u8], parsed: Option<&Value>) -> bool {
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

pub(crate) async fn retry_provider_request<F, Fut>(
    state: &GatewayState,
    candidate: &RouteCandidate,
    streaming: bool,
    mut operation: F,
) -> Result<reqwest::Response, AttemptFailure>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, AttemptFailure>>,
{
    state
        .routing
        .lock()
        .await
        .begin_attempt(candidate)
        .map_err(|message| AttemptFailure {
            response: error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "routing_capacity",
                &message,
            ),
            kind: AttemptFailureKind::Request,
        })?;
    let lease = RoutingAttemptLease(Arc::new(RoutingAttemptLeaseInner {
        routing: Arc::clone(&state.routing),
        candidate: candidate.clone(),
    }));
    let transport_retries = if streaming {
        candidate.provider.limits.stream_retries
    } else {
        candidate.provider.limits.request_retries
    };
    let mut transport_attempts = 0_u8;
    let mut rate_limit_attempts = 0_u8;
    let mut sends = 0_u16;
    let mut retry_observation = ProviderRetryObservation::default();
    let result = async {
        loop {
            if let Err(mut failure) = await_provider_pacing(state, candidate).await {
                attach_retry_observation_to_failure(&mut failure, sends, retry_observation);
                return Err(failure);
            }
            sends = sends.saturating_add(1);
            match operation().await {
                Ok(mut response) if response.status() == StatusCode::TOO_MANY_REQUESTS => {
                    let credential_source = candidate
                        .credential
                        .as_ref()
                        .unwrap_or(&candidate.provider.credential)
                        .source;
                    if should_retry_rate_limit(
                        &candidate.provider.limits,
                        credential_source,
                        rate_limit_attempts,
                    ) {
                        let delay = rate_limit_retry_delay(response.headers(), rate_limit_attempts);
                        merge_retry_response_usage(&mut retry_observation, response).await;
                        retry_observation.recovery_kinds.push("rate-limit".into());
                        tokio::time::sleep(delay).await;
                        rate_limit_attempts = rate_limit_attempts.saturating_add(1);
                        continue;
                    }
                    attach_retry_observation(&mut response, sends, retry_observation);
                    return Ok(response);
                }
                Ok(mut response)
                    if response.status() == StatusCode::REQUEST_TIMEOUT
                        || response.status().is_server_error() =>
                {
                    if transport_attempts < transport_retries {
                        let recovery = if response.status() == StatusCode::REQUEST_TIMEOUT {
                            "request-timeout"
                        } else {
                            "provider-5xx"
                        };
                        let delay = validated_retry_after(response.headers())
                            .map(|(_, delay)| delay.unwrap_or(Duration::ZERO))
                            .unwrap_or_else(|| retry_backoff(transport_attempts));
                        merge_retry_response_usage(&mut retry_observation, response).await;
                        retry_observation.recovery_kinds.push(recovery.into());
                        tokio::time::sleep(delay.min(Duration::from_secs(5))).await;
                        transport_attempts = transport_attempts.saturating_add(1);
                        continue;
                    }
                    attach_retry_observation(&mut response, sends, retry_observation);
                    return Ok(response);
                }
                Ok(mut response) => {
                    attach_retry_observation(&mut response, sends, retry_observation);
                    return Ok(response);
                }
                Err(failure)
                    if failure.kind == AttemptFailureKind::Retryable
                        && transport_attempts < transport_retries =>
                {
                    let usage = response_body_usage(failure.response).await;
                    retry_observation.usage = sum_token_usage(retry_observation.usage, usage);
                    retry_observation.recovery_kinds.push("transport".into());
                    tokio::time::sleep(retry_backoff(transport_attempts)).await;
                    transport_attempts = transport_attempts.saturating_add(1);
                }
                Err(mut failure) => {
                    attach_retry_observation_to_failure(&mut failure, sends, retry_observation);
                    return Err(failure);
                }
            }
        }
    }
    .await;
    match result {
        Ok(mut response) => {
            response.extensions_mut().insert(lease);
            Ok(response)
        }
        Err(mut failure) => {
            failure.response.extensions_mut().insert(lease);
            Err(failure)
        }
    }
}

fn attach_retry_observation(
    response: &mut reqwest::Response,
    sends: u16,
    mut observation: ProviderRetryObservation,
) {
    observation.additional_sends = sends.saturating_sub(1);
    if retry_observation_has_evidence(&observation) {
        response.extensions_mut().insert(observation);
    }
}

fn attach_retry_observation_to_failure(
    failure: &mut AttemptFailure,
    sends: u16,
    mut observation: ProviderRetryObservation,
) {
    observation.additional_sends = sends.saturating_sub(1);
    if retry_observation_has_evidence(&observation) {
        failure.response.extensions_mut().insert(observation);
    }
}

fn retry_observation_has_evidence(observation: &ProviderRetryObservation) -> bool {
    observation.additional_sends > 0 || !observation.recovery_kinds.is_empty()
        || observation.known_billing.is_some() || observation.cache_observation.is_some()
        || observation.terminal_failure
        || observation.usage.input_tokens > 0 || observation.usage.output_tokens > 0
        || observation.usage.total_tokens > 0 || observation.usage.cached_input_tokens > 0
        || observation.usage.cache_creation_input_tokens > 0 || observation.usage.reasoning_tokens > 0
        || observation.usage.cache_read_reported
}

pub(crate) fn merge_provider_retry_observation(
    target: &mut Option<ProviderRetryObservation>,
    next: Option<ProviderRetryObservation>,
) {
    let Some(next) = next else {
        return;
    };
    if let Some(current) = target.as_mut() {
        current.additional_sends = current
            .additional_sends
            .saturating_add(next.additional_sends);
        current.recovery_kinds.extend(next.recovery_kinds);
        if let Some(billing)=next.known_billing {
            current.known_billing=Some(current.known_billing.take().map(|previous|sum_token_usage(previous,billing.clone())).unwrap_or(billing));
        }
        current.terminal_failure|=next.terminal_failure;
        current.usage = sum_token_usage(current.usage.clone(), next.usage);
        if next.cache_observation.is_some() { current.cache_observation = next.cache_observation; }
        if next.request_diagnostic.is_some() { current.request_diagnostic = next.request_diagnostic; }
    } else {
        *target = Some(next);
    }
}

async fn merge_retry_response_usage(
    observation: &mut ProviderRetryObservation,
    response: reqwest::Response,
) {
    let usage = response_body_usage_from_reqwest(response).await;
    observation.usage = sum_token_usage(observation.usage.clone(), usage);
}

struct ProviderPacingReservation {
    pacing: Arc<ProviderPacing>,
    provider_id: String,
    ticket: u64,
    generation: u64,
    armed: bool,
}

impl ProviderPacingReservation {
    async fn complete(mut self) {
        {
            let mut state = self.pacing.state.lock().await;
            remove_provider_pacing_ticket(
                &mut state,
                &self.provider_id,
                self.ticket,
                self.generation,
                true,
            );
        }
        self.pacing.changed.notify_waiters();
        self.armed = false;
    }

    async fn cancel(&mut self) {
        {
            let mut state = self.pacing.state.lock().await;
            remove_provider_pacing_ticket(
                &mut state,
                &self.provider_id,
                self.ticket,
                self.generation,
                false,
            );
        }
        self.pacing.changed.notify_waiters();
        self.armed = false;
    }

    fn try_remove(&self, started: bool) -> bool {
        let Ok(mut state) = self.pacing.state.try_lock() else {
            return false;
        };
        remove_provider_pacing_ticket(
            &mut state,
            &self.provider_id,
            self.ticket,
            self.generation,
            started,
        );
        true
    }
}

impl Drop for ProviderPacingReservation {
    fn drop(&mut self) {
        if self.armed {
            if !self.try_remove(false) {
                let pacing = Arc::clone(&self.pacing);
                let provider_id = self.provider_id.clone();
                let ticket = self.ticket;
                let generation = self.generation;
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        let mut state = pacing.state.lock().await;
                        remove_provider_pacing_ticket(
                            &mut state,
                            &provider_id,
                            ticket,
                            generation,
                            false,
                        );
                        pacing.changed.notify_waiters();
                    });
                }
            }
            self.pacing.changed.notify_waiters();
        }
    }
}

fn remove_provider_pacing_ticket(
    state: &mut ProviderPacingState,
    provider_id: &str,
    ticket: u64,
    generation: u64,
    started: bool,
) {
    remove_provider_pacing_ticket_at(
        state,
        provider_id,
        ticket,
        generation,
        started,
        Instant::now(),
    );
}

fn remove_provider_pacing_ticket_at(
    state: &mut ProviderPacingState,
    provider_id: &str,
    ticket: u64,
    generation: u64,
    started: bool,
    now: Instant,
) {
    if state.generation != generation {
        return;
    }
    if let Some(queue) = state.queues.get_mut(provider_id) {
        if let Some(index) = queue
            .waiters
            .iter()
            .position(|waiter| waiter.ticket == ticket)
        {
            queue.waiters.remove(index);
        }
        if started {
            queue.last_started = Some(now);
        }
        let mut next = queue
            .last_started
            .and_then(|last| queue.waiters.front().map(|waiter| last + waiter.interval))
            .unwrap_or(now)
            .max(now);
        for waiter in &mut queue.waiters {
            waiter.scheduled = next;
            next = next + waiter.interval;
        }
        if queue.waiters.is_empty()
            && queue
                .last_started
                .is_none_or(|last| last + MAX_PROVIDER_PACING_WAIT <= now)
        {
            state.queues.remove(provider_id);
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ProviderPacingSlot {
    pub(crate) scheduled: Instant,
    pub(crate) ticket: u64,
    pub(crate) generation: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ProviderPacingError {
    Overloaded(Duration),
    SettingsChanged,
}

enum SuspendedProviderAdmission {
    Http(Arc<MemoryAdmission>, u64),
    WebSocket(Arc<MemoryAdmission>, u64),
    None,
}

async fn suspend_provider_admission_for_pacing() -> SuspendedProviderAdmission {
    if let Some((memory, bytes)) = suspend_http_admission_for_pacing().await {
        return SuspendedProviderAdmission::Http(memory, bytes);
    }
    if let Some((memory, bytes)) = suspend_websocket_admission_for_pacing().await {
        return SuspendedProviderAdmission::WebSocket(memory, bytes);
    }
    SuspendedProviderAdmission::None
}

async fn restore_provider_admission_after_pacing(
    suspended: SuspendedProviderAdmission,
) -> Result<(), &'static str> {
    match suspended {
        SuspendedProviderAdmission::Http(memory, bytes) => {
            restore_http_admission_after_pacing(Some((memory, bytes))).await
        }
        SuspendedProviderAdmission::WebSocket(memory, bytes) => {
            restore_websocket_admission_after_pacing(Some((memory, bytes))).await
        }
        SuspendedProviderAdmission::None => Ok(()),
    }
}

async fn await_provider_pacing(
    state: &GatewayState,
    candidate: &RouteCandidate,
) -> Result<(), AttemptFailure> {
    let interval = Duration::from_millis(candidate.provider.limits.request_pacing_ms);
    if interval.is_zero() {
        return Ok(());
    }
    // The provider queue is a pre-send admission boundary. Release the HTTP
    // request's global inflight/memory reservation while it waits, then restore
    // the exact reservation immediately before the upstream operation begins.
    let suspended_admission = suspend_provider_admission_for_pacing().await;
    let now = Instant::now();
    let slot = {
        let mut pacing = state.pacing.state.lock().await;
        reserve_provider_start(&mut pacing, &candidate.provider.id, interval, now)
            .map_err(provider_pacing_failure)?
    };
    let mut reservation = ProviderPacingReservation {
        pacing: Arc::clone(&state.pacing),
        provider_id: candidate.provider.id.clone(),
        ticket: slot.ticket,
        generation: slot.generation,
        armed: true,
    };
    loop {
        let changed = state.pacing.changed.notified();
        let waiter = {
            let pacing = state.pacing.state.lock().await;
            pacing
                .queues
                .get(&candidate.provider.id)
                .and_then(|queue| {
                    queue
                        .waiters
                        .iter()
                        .find(|waiter| waiter.ticket == slot.ticket)
                })
                .map(|waiter| (waiter.queued_at, provider_pacing_waiter_wake_at(waiter)))
        };
        let Some((queued_at, wake_at)) = waiter else {
            return Err(provider_pacing_failure(
                ProviderPacingError::SettingsChanged,
            ));
        };
        let expires_at = queued_at + MAX_PROVIDER_PACING_WAIT;
        tokio::select! {
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wake_at)) => {
                if Instant::now() >= expires_at {
                    reservation.cancel().await;
                    return Err(provider_pacing_failure(ProviderPacingError::Overloaded(
                        Duration::ZERO,
                    )));
                }
                break;
            },
            _ = changed => {
                let pacing = state.pacing.state.lock().await;
                let still_queued = pacing.generation == slot.generation
                    && pacing.queues.get(&candidate.provider.id)
                        .is_some_and(|queue| queue.waiters.iter().any(|waiter| waiter.ticket == slot.ticket));
                if !still_queued {
                    return Err(provider_pacing_failure(ProviderPacingError::SettingsChanged));
                }
            }
        }
    }
    restore_provider_admission_after_pacing(suspended_admission)
        .await
        .map_err(admission_reacquire_failure)?;
    reservation.complete().await;
    Ok(())
}

fn provider_pacing_waiter_wake_at(waiter: &ProviderPacingWaiter) -> Instant {
    waiter
        .scheduled
        .min(waiter.queued_at + MAX_PROVIDER_PACING_WAIT)
}

fn admission_reacquire_failure(message: &'static str) -> AttemptFailure {
    AttemptFailure {
        response: error_response(StatusCode::SERVICE_UNAVAILABLE, "gateway_capacity", message),
        // Local admission reacquire failure is terminal for this client
        // request. No alternate candidate may send after the reservation has
        // been released, and provider cooldown state must remain untouched.
        kind: AttemptFailureKind::Request,
    }
}

fn provider_pacing_failure(error: ProviderPacingError) -> AttemptFailure {
    let (status, code, message, retry_after) = match error {
        ProviderPacingError::Overloaded(retry_after) => (
            StatusCode::TOO_MANY_REQUESTS,
            "provider_pacing_overloaded",
            "provider pacing queue is full or would exceed its maximum wait",
            Some(retry_after),
        ),
        ProviderPacingError::SettingsChanged => (
            StatusCode::SERVICE_UNAVAILABLE,
            "provider_pacing_reconfigured",
            "provider settings changed while the request was queued",
            None,
        ),
    };
    let mut response = error_response(status, code, message);
    if let Some(delay) = retry_after {
        let seconds = retry_after_seconds_ceil(delay);
        if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
    }
    AttemptFailure {
        response,
        // Local pacing admission is not an upstream transport failure and must
        // not silently switch a policy route to another provider.
        kind: AttemptFailureKind::Request,
    }
}

fn retry_after_seconds_ceil(delay: Duration) -> u64 {
    delay
        .as_secs()
        .saturating_add(u64::from(delay.subsec_nanos() > 0))
        .max(1)
}

pub(crate) fn should_retry_rate_limit(
    limits: &crate::config::ProviderLimits,
    credential_source: CredentialSource,
    attempts: u8,
) -> bool {
    limits.retry_on_429
        && matches!(
            credential_source,
            CredentialSource::Environment | CredentialSource::Keychain | CredentialSource::Command
        )
        && attempts < limits.max_429_retries
}

pub(crate) fn rate_limit_retry_delay(headers: &HeaderMap, attempts: u8) -> Duration {
    validated_retry_after(headers)
        .map(|(_, delay)| delay.unwrap_or(Duration::ZERO))
        .unwrap_or_else(|| retry_backoff(attempts))
        .min(Duration::from_secs(10 * 60))
}

pub(crate) fn empty_completion_retry_enabled(
    candidate: &RouteCandidate,
    _streaming: bool,
    attempts: u8,
) -> bool {
    (model_matches_any(
        &candidate.upstream_model,
        &candidate.provider.empty_completion_retry_models,
    ) || model_matches_any(
        &candidate.upstream_model,
        &candidate.provider.terminal_continuation_guard_models,
    )) && attempts < candidate.provider.limits.empty_completion_retries
}

pub(crate) fn reserve_provider_start(
    pacing: &mut ProviderPacingState,
    key: &str,
    interval: Duration,
    now: Instant,
) -> Result<ProviderPacingSlot, ProviderPacingError> {
    if interval.is_zero() {
        return Ok(ProviderPacingSlot {
            scheduled: now,
            ticket: 0,
            generation: pacing.generation,
        });
    }
    pacing.queues.retain(|_, queue| {
        !queue.waiters.is_empty()
            || queue
                .last_started
                .is_some_and(|last| last + MAX_PROVIDER_PACING_WAIT > now)
    });
    if !pacing.queues.contains_key(key) && pacing.queues.len() >= MAX_PROVIDER_PACING_KEYS {
        let retry_after = pacing
            .queues
            .values()
            .filter_map(|queue| provider_pacing_queue_reclaim_at(queue, now))
            .min()
            .map(|deadline| deadline.saturating_duration_since(now))
            .unwrap_or(interval);
        return Err(ProviderPacingError::Overloaded(retry_after));
    }
    if pacing
        .queues
        .get(key)
        .is_some_and(|queue| queue.waiters.len() >= MAX_PROVIDER_PACING_QUEUE_DEPTH)
    {
        let retry_after = pacing
            .queues
            .get(key)
            .and_then(|queue| queue.waiters.front())
            .map(|waiter| waiter.scheduled.saturating_duration_since(now))
            .unwrap_or(interval);
        return Err(ProviderPacingError::Overloaded(retry_after));
    }
    let scheduled = pacing.queues.get(key).map_or(now, |queue| {
        queue
            .waiters
            .back()
            .map(|waiter| waiter.scheduled + waiter.interval)
            .or_else(|| queue.last_started.map(|last| last + interval))
            .unwrap_or(now)
            .max(now)
    });
    let wait = scheduled.saturating_duration_since(now);
    if wait >= MAX_PROVIDER_PACING_WAIT {
        return Err(ProviderPacingError::Overloaded(wait));
    }
    pacing.next_ticket = pacing.next_ticket.wrapping_add(1).max(1);
    let ticket = pacing.next_ticket;
    let queue = pacing.queues.entry(key.to_string()).or_default();
    queue.waiters.push_back(ProviderPacingWaiter {
        ticket,
        queued_at: now,
        scheduled,
        interval,
    });
    Ok(ProviderPacingSlot {
        scheduled,
        ticket,
        generation: pacing.generation,
    })
}

fn provider_pacing_queue_reclaim_at(queue: &ProviderPacingQueue, now: Instant) -> Option<Instant> {
    if queue.waiters.is_empty() {
        return queue
            .last_started
            .map(|last| last + MAX_PROVIDER_PACING_WAIT);
    }

    let mut last_started = queue.last_started;
    let mut final_waiter_removed_at = now;
    for waiter in &queue.waiters {
        let scheduled = last_started
            .map(|last| last + waiter.interval)
            .unwrap_or(waiter.scheduled)
            .max(final_waiter_removed_at);
        let expires = waiter.queued_at + MAX_PROVIDER_PACING_WAIT;
        if scheduled < expires {
            last_started = Some(scheduled);
            final_waiter_removed_at = scheduled;
        } else {
            final_waiter_removed_at = final_waiter_removed_at.max(expires);
        }
    }
    Some(
        last_started
            .map(|last| last + MAX_PROVIDER_PACING_WAIT)
            .unwrap_or(final_waiter_removed_at)
            .max(final_waiter_removed_at),
    )
}

pub(crate) async fn reset_provider_pacing(pacing: &ProviderPacing) {
    {
        let mut state = pacing.state.lock().await;
        state.generation = state.generation.wrapping_add(1);
        state.queues.clear();
    }
    pacing.changed.notify_waiters();
}

struct BufferedProviderResponse {
    thinking_provenance: Option<super::anthropic_compact::ThinkingProvenance>,
    cache_observation: crate::observability::CacheObservation,
    request_diagnostic: Option<crate::observability::RequestDiagnostic>,
    status: StatusCode,
    version: axum::http::Version,
    headers: HeaderMap,
    bytes: Bytes,
    value: Option<Value>,
    usage: TokenUsage,
    retry_observation: Option<ProviderRetryObservation>,
    routing_attempt_lease: Option<RoutingAttemptLease>,
}

impl BufferedProviderResponse {
    fn into_response(self) -> Result<reqwest::Response, AttemptFailure> {
        let mut builder = axum::http::Response::builder()
            .status(self.status)
            .version(self.version);
        if let Some(target) = builder.headers_mut() {
            *target = self.headers;
        }
        let mut response = builder
            .body(reqwest::Body::from(self.bytes))
            .map(reqwest::Response::from)
            .map_err(|_| request_failure("gateway_error", "failed to rebuild provider response"))?;
        if let Some(proof) = self.thinking_provenance { response.extensions_mut().insert(proof); }
        response.extensions_mut().insert(self.cache_observation);
        if let Some(diagnostic) = self.request_diagnostic {
            response.extensions_mut().insert(diagnostic);
        }
        if let Some(observation) = self.retry_observation {
            response.extensions_mut().insert(observation);
        }
        if let Some(lease) = self.routing_attempt_lease {
            response.extensions_mut().insert(lease);
        }
        Ok(response)
    }
}

async fn buffer_provider_response(
    mut response: reqwest::Response,
    limit: u64,
) -> Result<BufferedProviderResponse, AttemptFailure> {
    let status = response.status();
    let version = response.version();
    let headers = response.headers().clone();
    let retry_observation = response
        .extensions()
        .get::<ProviderRetryObservation>()
        .cloned();
    let thinking_provenance = response.extensions().get::<super::anthropic_compact::ThinkingProvenance>().cloned();
    let existing_cache_observation = response.extensions().get::<crate::observability::CacheObservation>().cloned();
    let routing_attempt_lease = take_routing_attempt_lease(&mut response);
    let request_diagnostic = response
        .extensions()
        .get::<crate::observability::RequestDiagnostic>()
        .cloned();
    let bytes = super::stream::read_provider_bytes_with_usage(response, limit)
        .await.map_err(|(message, usage)| {
            let mut failure = failure_with_provider_metadata(request_failure("invalid_provider_response", &message), retry_observation.as_ref(), request_diagnostic.as_ref());
            failure.response.extensions_mut().insert(crate::observability::ReportedUpstreamUsage(usage));
            failure
        })?;
    let value = serde_json::from_slice::<Value>(&bytes).ok();
    let usage = value.as_ref().map(TokenUsage::from_json).unwrap_or_else(|| {
        let mut collector = super::stream::SseUsageCollector::default();
        collector.push(&bytes);
        super::stream::provider_prefix_usage(&bytes, &collector)
    });
    let cache_observation = existing_cache_observation.unwrap_or_else(|| value.as_ref().map(crate::observability::CacheObservation::from_provider_json)
        .unwrap_or_default());
    Ok(BufferedProviderResponse {
        thinking_provenance,
        cache_observation,
        request_diagnostic,
        status,
        version,
        headers,
        bytes: Bytes::from(bytes),
        value,
        usage,
        retry_observation,
        routing_attempt_lease,
    })
}

const EMPTY_COMPLETION_MAX_BUFFERED_EVENTS: usize = 1_024;
const EMPTY_COMPLETION_MAX_BUFFERED_BYTES: usize = 1024 * 1024;
pub(crate) const EMPTY_COMPLETION_RETRY_FAILED_CODE: &str = "empty_completion_retry_failed";
pub(crate) const EMPTY_COMPLETION_RECOVERY_MARKER: &[u8] =
    b": codetas-recovery=empty-completion\n\n";

async fn guard_empty_completion_response(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
    streaming: bool,
    response: reqwest::Response,
    cloud_request_id: Option<String>,
    cloud_envelope_cache: Option<Arc<Mutex<Option<Value>>>>,
) -> Result<reqwest::Response, AttemptFailure> {
    if request_is_remote_compaction(body)
        || candidate.provider.transport == ProviderTransport::Kiro
        || !empty_completion_retry_enabled(candidate, streaming, 0)
        || !response.status().is_success()
    {
        return Ok(response);
    }
    if streaming {
        return guarded_provider_sse(
            state.clone(),
            body.clone(),
            candidate.clone(),
            caller_headers.cloned(),
            response,
            cloud_request_id,
            cloud_envelope_cache,
        );
    }

    guard_buffered_empty_completion_response(
        response, candidate.provider.limits.max_response_bytes,
        candidate.provider.limits.empty_completion_retries,
        || retry_provider_request(state, candidate, false, || {
            send_candidate_once(state, body, candidate, caller_headers,
                cloud_request_id.as_deref(), cloud_envelope_cache.as_ref())
        }),
    ).await
}

async fn guard_buffered_empty_completion_response<F, Fut>(
    response: reqwest::Response, limit: u64, budget: u8, mut operation: F,
) -> Result<reqwest::Response, AttemptFailure>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, AttemptFailure>>,
{
    let mut first = buffer_provider_response(response, limit).await?;
    let mut provider_retries = first.retry_observation.clone();
    let Some(first_value) = first.value.as_ref() else {
        return first.into_response();
    };
    if !completion_is_empty(first_value) {
        return first.into_response();
    }
    let mut usage = first.usage.clone();
    // The first provider body has reached EOF and will not be reconstructed;
    // release that attempt before starting an independent empty retry.
    drop(first.routing_attempt_lease.take());
    for retry_index in 0..budget {
        tokio::time::sleep(retry_backoff(retry_index)).await;
        let retry = match operation().await
        {
            Ok(retry) => retry,
            Err(failure) => {
                merge_provider_retry_observation(
                    &mut provider_retries,
                    failure
                        .response
                        .extensions()
                        .get::<ProviderRetryObservation>()
                        .cloned(),
                );
                let retry_usage = response_body_usage(failure.response).await;
                return empty_completion_retry_failed_response_with_observation(
                    sum_token_usage(usage, retry_usage),
                    u16::from(retry_index) + 1,
                    provider_retries,
                );
            }
        };
        if !retry.status().is_success() {
            merge_provider_retry_observation(
                &mut provider_retries,
                retry
                    .extensions()
                    .get::<ProviderRetryObservation>()
                    .cloned(),
            );
            let retry_usage = response_body_usage_from_reqwest(retry).await;
            return empty_completion_retry_failed_response_with_observation(
                sum_token_usage(usage, retry_usage),
                u16::from(retry_index) + 1,
                provider_retries,
            );
        }
        let retry_observation = retry
            .extensions()
            .get::<ProviderRetryObservation>()
            .cloned();
        let mut retry = match buffer_provider_response(retry, limit).await {
            Ok(retry) => retry,
            Err(failure) => {
                merge_provider_retry_observation(&mut provider_retries, retry_observation);
                let final_usage = failure.response.extensions().get::<crate::observability::ReportedUpstreamUsage>().map(|usage| usage.0.clone()).unwrap_or_default();
                return empty_completion_retry_failed_response_with_observation(
                    sum_token_usage(usage, final_usage),
                    u16::from(retry_index) + 1,
                    provider_retries,
                );
            }
        };
        merge_provider_retry_observation(&mut provider_retries, retry.retry_observation.clone());
        usage = sum_token_usage(usage, retry.usage.clone());
        let Some(mut retry_value) = retry.value.take() else {
            return empty_completion_retry_failed_response_with_observation(
                usage,
                u16::from(retry_index) + 1,
                provider_retries,
            );
        };
        if completion_is_empty(&retry_value) {
            continue;
        }
        let known_billing=usage.clone();
        write_token_usage(&mut retry_value, usage);
        retry.bytes =
            Bytes::from(serde_json::to_vec(&retry_value).map_err(|_| {
                request_failure("gateway_error", "failed to encode retried response")
            })?);
        retry.headers.remove(header::CONTENT_LENGTH);
        retry.value = Some(retry_value);
        let empty_sends = u16::from(retry_index) + 1;
        let observation = provider_retries.get_or_insert_with(ProviderRetryObservation::default);
        observation.known_billing=Some(known_billing);
        observation.additional_sends = observation.additional_sends.saturating_add(empty_sends);
        observation.recovery_kinds.extend(
            std::iter::repeat("empty-completion".to_string()).take(usize::from(empty_sends)),
        );
        retry.retry_observation = provider_retries;
        let mut response = retry.into_response()?;
        response
            .extensions_mut()
            .insert(EmptyCompletionRecoverySuccess {
                additional_sends: empty_sends,
            });
        return Ok(response);
    }
    empty_completion_retry_failed_response_with_observation(
        usage,
        u16::from(budget),
        provider_retries,
    )
}

#[derive(Default)]
struct FinalCacheObservation {
    value: crate::observability::CacheObservation,
    cache_creation_input_tokens:u64,
    anthropic: AnthropicStreamState,
}
impl FinalCacheObservation {
    fn update(&mut self, protocol: ProviderProtocol, event: &Value) {
        if protocol == ProviderProtocol::AnthropicMessages {
            if let Some(creation)=event.pointer("/usage/cache_creation_input_tokens").or_else(||event.pointer("/message/usage/cache_creation_input_tokens")).and_then(Value::as_u64) {self.cache_creation_input_tokens=creation;}
            let kind = event.get("type").and_then(Value::as_str);
            if kind == Some("message_start") {
                self.value = crate::observability::CacheObservation::from_provider_json(event);
            }
            if matches!(kind, Some("message_start" | "message_delta")) {
                if let Ok(Some(chunk)) = anthropic_stream_to_chat(event, &mut self.anthropic, false) {
                    self.value = crate::observability::CacheObservation::from_provider_json(&chunk);
                }
            }
            return;
        }
        let usage = event.get("usage").filter(|value|value.is_object()).or_else(|| event.pointer("/response/usage").filter(|value|value.is_object()))
            .or_else(|| event.get("usageMetadata").filter(|value|value.is_object())).or_else(|| event.pointer("/response/usageMetadata").filter(|value|value.is_object()));
        if let Some(usage) = usage {
            let next = crate::observability::CacheObservation::from_provider_json(event);
            if ["input_tokens", "prompt_tokens", "promptTokenCount"].iter().any(|key| usage.get(*key).and_then(Value::as_u64).is_some()) {
                self.value.input_tokens = next.input_tokens;
            }
            if next.cache_read_reported {
                self.value.cached_input_tokens = next.cached_input_tokens;
                self.value.cache_read_reported = true;
            }
        }
    }
}

fn guarded_provider_sse(
    state: GatewayState,
    body: Value,
    candidate: RouteCandidate,
    caller_headers: Option<HeaderMap>,
    response: reqwest::Response,
    cloud_request_id: Option<String>,
    cloud_envelope_cache: Option<Arc<Mutex<Option<Value>>>>,
) -> Result<reqwest::Response, AttemptFailure> {
    let thinking_provenance = response.extensions().get::<super::anthropic_compact::ThinkingProvenance>().cloned();
    let status = response.status();
    let version = response.version();
    let headers = response.headers().clone();
    let retry_observation = response
        .extensions()
        .get::<ProviderRetryObservation>()
        .cloned();
    let request_diagnostic = response
        .extensions()
        .get::<crate::observability::RequestDiagnostic>()
        .cloned();
    let shared_retry_observation = SharedProviderRetryObservation(Arc::new(std::sync::Mutex::new(
        ProviderRetryObservation { thinking_provenance: thinking_provenance.clone(), ..Default::default() },
    )));
    let stream_retry_observation = shared_retry_observation.clone();
    let limit = candidate.provider.limits.max_response_bytes;
    let protocol = candidate
        .provider
        .protocol_for_model(&candidate.upstream_model);
    let tolerate_anthropic_eof = protocol == ProviderProtocol::AnthropicMessages
        && model_matches_any(
            &candidate.upstream_model,
            &candidate.provider.anthropic_eof_tolerance_models,
        );
    let output = stream! {
        let mut source = provider_guard_stream_for_protocol(response, tolerate_anthropic_eof, protocol,limit);
        let mut pending = Vec::new();
        let mut held = Vec::<Bytes>::new();
        let mut held_bytes = 0_usize;
        let mut saw_content = false;
        let mut passthrough = false;
        let mut retries = 0_u8;
        let mut prior_usage = TokenUsage::default();
        let mut attempt_usage = TokenUsage::default();
        let mut failure_prefix = None::<Bytes>;
        let mut response_id = None::<String>;
        let mut prior_reasoning_history = Vec::<Value>::new();
        let mut attempt_reasoning_history = Vec::<Value>::new();
        let mut prior_reasoning_injected = false;
        let mut next_response_sequence = 0_u64;
        loop {
            let mut final_cache_observation = FinalCacheObservation::default();
            stream_retry_observation.0.lock().unwrap_or_else(|p| p.into_inner()).cache_observation
                = Some(crate::observability::CacheObservation::default());
            {
                let mut observation=stream_retry_observation.0.lock().unwrap_or_else(|p|p.into_inner());
                observation.terminal_failure=false;observation.known_billing=Some(prior_usage.clone());
            }
            let mut terminal_seen = false;
            let mut chat_finished = false;
            let mut chat_terminal = None::<Value>;
            while let Some(chunk) = source.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        if let Some(failure) = error.get_ref().and_then(|error| error.downcast_ref::<ProviderStreamReadFailure>()) {
                            attempt_usage.merge_max(failure.usage.clone());
                        }
                        {
                            let mut observation = stream_retry_observation.0.lock().unwrap_or_else(|p| p.into_inner());
                            observation.terminal_failure = true;
                            observation.known_billing = Some(sum_token_usage(prior_usage.clone(), attempt_usage.clone()));
                        }
                        if retries > 0 && !saw_content && !passthrough {
                            state.routing.lock().await.record_failure(&candidate);
                            if let Some(prefix) = failure_prefix.take() {
                                yield Ok(prefix);
                            }
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(provider_sse(
                                protocol,
                                provider_error_event_type(protocol),
                                &empty_completion_failed_event_for_protocol(
                                    protocol,
                                    sum_token_usage(
                                        prior_usage.clone(),
                                        attempt_usage.clone(),
                                    ),
                                    response_id.as_deref(),
                                ),
                            )));
                        } else {
                            yield Err(std::io::Error::other(error.to_string()));
                        }
                        return;
                    }
                };
                if chunk.as_ref()==KNOWN_PROVIDER_FAILURE_MARKER {
                    stream_retry_observation.0.lock().unwrap_or_else(|p|p.into_inner()).terminal_failure=true;
                    continue;
                }
                let values = super::stream::drain_sse_results(&mut pending, &chunk, protocol == ProviderProtocol::ChatCompletions);
                if values.is_empty() {yield Ok(Bytes::from_static(b": codetas-upstream-activity\n\n"));}
                for value in values {
                    let mut value = match value {
                        Ok(value) => value,
                    Err(error) => {
                        if retries > 0 && !saw_content && !passthrough {
                            state.routing.lock().await.record_failure(&candidate);
                            if let Some(prefix) = failure_prefix.take() {
                                yield Ok(prefix);
                            }
                            yield Ok(Bytes::from(provider_sse(
                                protocol,
                                provider_error_event_type(protocol),
                                &empty_completion_failed_event_for_protocol(
                                    protocol,
                                    sum_token_usage(
                                        prior_usage.clone(),
                                        attempt_usage.clone(),
                                    ),
                                    response_id.as_deref(),
                                ),
                            )));
                            return;
                        }
                        yield Err(std::io::Error::other(error));
                        return;
                    }
                    };
                    let chat_stream_end = protocol == ProviderProtocol::ChatCompletions
                        && value.get("type").and_then(Value::as_str) == Some("codetas.chat_stream_end");
                    if chat_stream_end && (saw_content || passthrough || !chat_finished) {
                        for buffered in held.drain(..) { yield Ok(buffered); }
                        return;
                    }
                    if chat_stream_end && chat_finished {
                        if let Some(mut terminal) = chat_terminal.take() {
                            if !provider_stream_event_is_empty_terminal(protocol, &terminal) {
                                for buffered in held.drain(..) { yield Ok(buffered); }
                                write_stream_token_usage(protocol,&mut terminal,sum_token_usage(prior_usage.clone(),attempt_usage.clone()));
                                yield Ok(Bytes::from(provider_sse(protocol,"message",&terminal)));
                                return;
                            }
                        }
                    }
                    let event_type = value
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("message")
                        .to_string();
                    if protocol == ProviderProtocol::Responses {
                        let semantic = responses_reasoning_semantic_history(&value);
                        if event_type == "response.output_item.done" {
                            attempt_reasoning_history.extend(semantic);
                        } else if matches!(
                            event_type.as_str(),
                            "response.completed" | "response.incomplete" | "response.failed"
                        ) && attempt_reasoning_history.is_empty()
                        {
                            attempt_reasoning_history.extend(semantic);
                        }
                    }
                    final_cache_observation.update(protocol, &value);
                    stream_retry_observation.0.lock().unwrap_or_else(|p| p.into_inner()).cache_observation
                        = Some(final_cache_observation.value.clone());
                    let event_usage = TokenUsage::from_json(&value);
                    attempt_usage.merge_max(event_usage);
                    if protocol==ProviderProtocol::AnthropicMessages {
                        attempt_usage.input_tokens=final_cache_observation.value.input_tokens;
                        attempt_usage.cached_input_tokens=final_cache_observation.value.cached_input_tokens;
                        attempt_usage.cache_read_reported=final_cache_observation.value.cache_read_reported;
                        attempt_usage.cache_creation_input_tokens=final_cache_observation.cache_creation_input_tokens;
                        attempt_usage.total_tokens=attempt_usage.total_tokens.max(attempt_usage.input_tokens.saturating_add(attempt_usage.output_tokens));
                    }
                    stream_retry_observation.0.lock().unwrap_or_else(|p|p.into_inner()).known_billing=Some(sum_token_usage(prior_usage.clone(),attempt_usage.clone()));
                    if retries>0 && ((protocol==ProviderProtocol::AnthropicMessages && value.get("type").and_then(Value::as_str)==Some("message_delta") && value.get("usage").and_then(Value::as_object).is_some_and(|raw|["input_tokens","cache_read_input_tokens","cache_creation_input_tokens"].iter().any(|key|raw.contains_key(*key))))
                        || (protocol==ProviderProtocol::GeminiGenerateContent && (value.get("usageMetadata").is_some()||value.pointer("/response/usageMetadata").is_some()))) {
                        write_stream_token_usage(protocol,&mut value,sum_token_usage(prior_usage.clone(),attempt_usage.clone()));
                    }
                    if retries > 0
                        && protocol == ProviderProtocol::AnthropicMessages
                        && value.get("type").and_then(Value::as_str) == Some("message_start")
                    {
                        write_anthropic_stream_start_usage(
                            &mut value,
                            sum_token_usage(prior_usage.clone(), attempt_usage.clone()),
                        );
                    }
                    if protocol == ProviderProtocol::Responses && retries > 0 {
                        let offset = prior_reasoning_history.len();
                        shift_responses_retry_event(&mut value, offset, &prior_reasoning_history);
                        if event_type != "response.created"
                            && !prior_reasoning_injected
                            && !prior_reasoning_history.is_empty()
                        {
                            for mut event in canonical_reasoning_lifecycle_events(
                                &prior_reasoning_history,
                            ) {
                                assign_responses_sequence(
                                    &mut event,
                                    &mut next_response_sequence,
                                );
                                let encoded = Bytes::from(provider_sse(
                                    protocol,
                                    event.get("type").and_then(Value::as_str).unwrap_or("message"),
                                    &event,
                                ));
                                held_bytes = held_bytes.saturating_add(encoded.len());
                                held.push(encoded);
                            }
                            prior_reasoning_injected = true;
                        }
                    }
                    if protocol == ProviderProtocol::Responses {
                        assign_responses_sequence(&mut value, &mut next_response_sequence);
                    }
                    if protocol == ProviderProtocol::Responses
                        && event_type == "response.created"
                    {
                        response_id = value
                            .pointer("/response/id")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        failure_prefix = Some(Bytes::from(provider_sse(
                            protocol,
                            &event_type,
                            &value,
                        )));
                    }
                    if protocol == ProviderProtocol::ChatCompletions && !chat_stream_end {
                        if provider_stream_event_is_terminal(protocol, &value) {
                            chat_finished = true;
                            chat_terminal = Some(value.clone());
                        }
                        // Finish reasons end generation, not the wire stream. Usage is
                        // normally a later frame (including for empty generations).
                        if !saw_content && chat_finished && !provider_stream_event_has_visible_content(protocol, &value)
                            && value.get("error").is_none() { continue; }
                    }
                    if saw_content || passthrough {
                        if (provider_stream_event_is_terminal(protocol, &value)
                            || protocol == ProviderProtocol::ChatCompletions && value.get("usage").is_some()
                            || protocol==ProviderProtocol::AnthropicMessages && value.get("type").and_then(Value::as_str)==Some("message_delta") && value.get("usage").and_then(Value::as_object).is_some_and(|raw|["input_tokens","cache_read_input_tokens","cache_creation_input_tokens"].iter().any(|key|raw.contains_key(*key)))) && retries > 0 {
                            let usage = sum_token_usage(
                                prior_usage.clone(),
                                attempt_usage.clone(),
                            );
                            write_stream_token_usage(protocol, &mut value, usage);
                        }
                        terminal_seen = protocol != ProviderProtocol::ChatCompletions && provider_stream_event_is_terminal(protocol, &value);
                        yield Ok(Bytes::from(provider_sse(protocol, &event_type, &value)));
                        if terminal_seen {
                            break;
                        }
                        continue;
                    }
                    if provider_stream_event_has_visible_content(protocol, &value) {
                        let terminal = protocol != ProviderProtocol::ChatCompletions && provider_stream_event_is_terminal(protocol, &value);
                        if terminal && retries > 0 {
                            let usage = sum_token_usage(
                                prior_usage.clone(),
                                attempt_usage.clone(),
                            );
                            write_stream_token_usage(protocol, &mut value, usage);
                        }
                        saw_content = true;
                        failure_prefix = None;
                        for buffered in held.drain(..) {
                            yield Ok(buffered);
                        }
                        held_bytes = 0;
                        yield Ok(Bytes::from(provider_sse(protocol, &event_type, &value)));
                        if terminal {
                            terminal_seen = true;
                            break;
                        }
                        continue;
                    }
                    if provider_stream_event_is_visible_failure(protocol, &value) {
                        if retries > 0 && provider_stream_event_is_error(protocol, &value) {
                            held.clear();
                            state.routing.lock().await.record_failure(&candidate);
                            if let Some(prefix) = failure_prefix.take() {
                                yield Ok(prefix);
                            }
                            yield Ok(Bytes::from(provider_sse(
                                protocol,
                                provider_error_event_type(protocol),
                                &empty_completion_failed_event_for_protocol(
                                    protocol,
                                    sum_token_usage(
                                        prior_usage.clone(),
                                        attempt_usage.clone(),
                                    ),
                                    response_id.as_deref(),
                                ),
                            )));
                            return;
                        }
                        for buffered in held.drain(..) {
                            yield Ok(buffered);
                        }
                        if retries > 0 {
                            let usage = sum_token_usage(
                                prior_usage.clone(),
                                attempt_usage.clone(),
                            );
                            write_stream_token_usage(protocol, &mut value, usage);
                        }
                        yield Ok(Bytes::from(provider_sse(protocol, &event_type, &value)));
                        return;
                    }
                    if (chat_stream_end && chat_finished) || (protocol != ProviderProtocol::ChatCompletions && provider_stream_event_is_empty_terminal(protocol, &value)) {
                        prior_usage = sum_token_usage(
                            prior_usage,
                            attempt_usage.clone(),
                        );
                        attempt_usage = TokenUsage::default();
                        if retries < candidate.provider.limits.empty_completion_retries {
                            prior_reasoning_history.append(&mut attempt_reasoning_history);
                            for item in &mut prior_reasoning_history {
                                if item.get("id").is_none() {
                                    item["id"] =
                                        json!(format!("rs_{}", Uuid::new_v4().simple()));
                                    item["status"] = json!("completed");
                                }
                            }
                            retries += 1;
                            if let Ok(mut observation) = stream_retry_observation.0.lock() {
                                observation.recovery_kinds.push("empty-completion".into());
                            }
                            yield Ok(Bytes::from_static(EMPTY_COMPLETION_RECOVERY_MARKER));
                            // The retry is a distinct upstream response lifecycle. Do not
                            // splice the first attempt's provider IDs/signatures into it.
                            if matches!(protocol, ProviderProtocol::Responses | ProviderProtocol::AnthropicMessages) {
                                held.clear();
                                held_bytes = 0;
                                prior_reasoning_injected = false;
                                next_response_sequence = 0;
                            }
                            drop(std::mem::replace(&mut source,Box::pin(futures_util::stream::empty())));
                            tokio::time::sleep(retry_backoff(retries.saturating_sub(1))).await;
                            stream_retry_observation.0.lock().unwrap_or_else(|p|p.into_inner()).additional_sends+=1;
                            let retry = retry_provider_request(&state, &candidate, true, || {
                                send_candidate_once(
                                    &state,
                                    &body,
                                    &candidate,
                                    caller_headers.as_ref(),
                                    cloud_request_id.as_deref(),
                                    cloud_envelope_cache.as_ref(),
                                )
                            })
                            .await;
                            match retry {
                                Ok(retry) if retry.status().is_success() => {
                                    stream_retry_observation.0.lock().unwrap_or_else(|p| p.into_inner()).thinking_provenance =
                                        retry.extensions().get::<super::anthropic_compact::ThinkingProvenance>().cloned();
                                    stream_retry_observation.0.lock().unwrap_or_else(|p| p.into_inner()).request_diagnostic =
                                        retry.extensions().get::<crate::observability::RequestDiagnostic>().cloned();
                                    if let Some(next) = retry
                                        .extensions()
                                        .get::<ProviderRetryObservation>()
                                        .cloned()
                                    {
                                        if let Ok(mut observation) =
                                            stream_retry_observation.0.lock()
                                        {
                                            let mut merged = Some(observation.clone());
                                            merge_provider_retry_observation(
                                                &mut merged,
                                                Some(next),
                                            );
                                            if let Some(merged) = merged {
                                                *observation = merged;
                                            }
                                        }
                                    }
                                    source = provider_guard_stream_for_protocol(retry, tolerate_anthropic_eof, protocol,limit);
                                    pending.clear();
                                    attempt_reasoning_history.clear();
                                    terminal_seen = true;
                                    break;
                                }
                                Ok(retry) => {
                                    if let Some(next) = retry
                                        .extensions()
                                        .get::<ProviderRetryObservation>()
                                        .cloned()
                                    {
                                        if let Ok(mut observation) =
                                            stream_retry_observation.0.lock()
                                        {
                                            let mut merged = Some(observation.clone());
                                            merge_provider_retry_observation(
                                                &mut merged,
                                                Some(next),
                                            );
                                            if let Some(merged) = merged {
                                                *observation = merged;
                                            }
                                        }
                                    }
                                    let retry_usage = response_body_usage_from_reqwest(retry).await;
                                    prior_usage = sum_token_usage(prior_usage, retry_usage);
                                    held.clear();
                                    state.routing.lock().await.record_failure(&candidate);
                                    if let Some(prefix) = failure_prefix.take() {
                                        yield Ok(prefix);
                                    }
                                    yield Ok(Bytes::from(provider_sse(
                                        protocol,
                                        provider_error_event_type(protocol),
                                        &empty_completion_failed_event_for_protocol(
                                            protocol,
                                            prior_usage.clone(),
                                            response_id.as_deref(),
                                        ),
                                    )));
                                    return;
                                }
                                Err(failure) => {
                                    if let Some(next) = failure
                                        .response
                                        .extensions()
                                        .get::<ProviderRetryObservation>()
                                        .cloned()
                                    {
                                        if let Ok(mut observation) =
                                            stream_retry_observation.0.lock()
                                        {
                                            let mut merged = Some(observation.clone());
                                            merge_provider_retry_observation(
                                                &mut merged,
                                                Some(next),
                                            );
                                            if let Some(merged) = merged {
                                                *observation = merged;
                                            }
                                        }
                                    }
                                    let retry_usage = response_body_usage(failure.response).await;
                                    prior_usage = sum_token_usage(prior_usage, retry_usage);
                                    held.clear();
                                    state.routing.lock().await.record_failure(&candidate);
                                    if let Some(prefix) = failure_prefix.take() {
                                        yield Ok(prefix);
                                    }
                                    yield Ok(Bytes::from(provider_sse(
                                        protocol,
                                        provider_error_event_type(protocol),
                                        &empty_completion_failed_event_for_protocol(
                                            protocol,
                                            sum_token_usage(
                                                prior_usage.clone(),
                                                attempt_usage.clone(),
                                            ),
                                            response_id.as_deref(),
                                        ),
                                    )));
                                    return;
                                }
                            }
                        } else {
                            held.clear();
                            state.routing.lock().await.record_failure(&candidate);
                            if let Some(prefix) = failure_prefix.take() {
                                yield Ok(prefix);
                            }
                            yield Ok(Bytes::from(provider_sse(
                                protocol,
                                provider_error_event_type(protocol),
                                &empty_completion_failed_event_for_protocol(
                                    protocol,
                                    sum_token_usage(
                                        prior_usage.clone(),
                                        attempt_usage.clone(),
                                    ),
                                    response_id.as_deref(),
                                ),
                            )));
                            return;
                        }
                        continue;
                    }
                    let encoded = Bytes::from(provider_sse(protocol, &event_type, &value));
                    if held.len() + 1 > EMPTY_COMPLETION_MAX_BUFFERED_EVENTS
                        || held_bytes.saturating_add(encoded.len())
                            > EMPTY_COMPLETION_MAX_BUFFERED_BYTES
                    {
                        for buffered in held.drain(..) {
                            yield Ok(buffered);
                        }
                        held_bytes = 0;
                        passthrough = true;
                        yield Ok(encoded);
                    } else {
                        held_bytes = held_bytes.saturating_add(encoded.len());
                        held.push(encoded);
                        if provider_stream_event_is_reasoning(protocol, &value) {
                            yield Ok(Bytes::from_static(b": codetas-empty-completion-guard\n\n"));
                        }
                    }
                }
                if terminal_seen {
                    break;
                }
            }
            if terminal_seen && retries > 0 && !saw_content {
                continue;
            }
            if retries > 0 && !saw_content && !passthrough {
                held.clear();
                state.routing.lock().await.record_failure(&candidate);
                if let Some(prefix) = failure_prefix.take() {
                    yield Ok(prefix);
                }
                yield Ok(Bytes::from(provider_sse(
                    protocol,
                    provider_error_event_type(protocol),
                    &empty_completion_failed_event_for_protocol(
                        protocol,
                        sum_token_usage(prior_usage.clone(), attempt_usage.clone()),
                        response_id.as_deref(),
                    ),
                )));
                return;
            }
            for buffered in held.drain(..) {
                yield Ok(buffered);
            }
            return;
        }
    };
    let mut builder = axum::http::Response::builder()
        .status(status)
        .version(version);
    if let Some(target) = builder.headers_mut() {
        *target = headers;
        target.remove(header::CONTENT_LENGTH);
    }
    let mut response = builder
        .body(reqwest::Body::wrap_stream(output))
        .map(reqwest::Response::from)
        .map_err(|_| request_failure("gateway_error", "failed to build guarded provider stream"))?;
    if let Some(observation) = retry_observation {
        response.extensions_mut().insert(observation);
    }
    if let Some(diagnostic) = request_diagnostic {
        response.extensions_mut().insert(diagnostic);
    }
    if let Some(proof) = thinking_provenance { response.extensions_mut().insert(proof); }
    response.extensions_mut().insert(shared_retry_observation);
    Ok(response)
}

const KNOWN_PROVIDER_FAILURE_MARKER:&[u8]=b": codetas-known-provider-failure\n\n";

fn provider_guard_stream_for_protocol(mut response:reqwest::Response,tolerate_eof:bool,protocol:ProviderProtocol,limit:u64)->Pin<Box<dyn futures_util::Stream<Item=Result<Bytes,std::io::Error>>+Send>> {
    let lease=take_routing_attempt_lease(&mut response);
    Box::pin(stream! {
        let mut lease=lease;let mut source=response.bytes_stream();let mut pending=Vec::new();let mut delayed=None::<Value>;let mut received=0_u64;
        let mut collector = super::stream::SseUsageCollector::default();
        while let Some(chunk)=source.next().await {
            let chunk=match chunk {Ok(chunk)=>chunk,Err(_)=>{yield Err(provider_stream_read_failure("provider stream read failed", &collector));return;}};
            let take = chunk.len().min(usize::try_from(limit.saturating_sub(received)).unwrap_or(usize::MAX));
            collector.push(&chunk[..take]);
            received=received.saturating_add(chunk.len() as u64);
            if take < chunk.len() {yield Err(provider_stream_read_failure("provider response exceeded its byte limit", &collector));return;}
            if protocol != ProviderProtocol::GeminiGenerateContent { yield Ok(chunk); continue; }
            let values = super::stream::drain_sse_results(&mut pending, &chunk, false);
            if values.is_empty() {yield Ok(Bytes::from_static(b": codetas-upstream-activity\n\n"));}
            for value in values {
                let mut value = match value { Ok(value) => value, Err(_) => { yield Err(provider_stream_read_failure("invalid provider SSE frame", &collector)); return; } };
                let payload=unwrap_gemini_stream_payload(&value);
                let stopped=provider_stream_event_is_terminal(protocol,&value);
                let failed=provider_stream_event_is_visible_failure(protocol,&value);
                if stopped && failed {
                    yield Ok(Bytes::from_static(KNOWN_PROVIDER_FAILURE_MARKER));
                    let mut terminal=value.clone();let wrapped=value.get("response").is_some_and(Value::is_object);
                    let terminal_payload=if wrapped {&mut terminal["response"]} else {&mut terminal};
                    if let Some(object)=terminal_payload.as_object_mut() {object.remove("usageMetadata");}
                    delayed=Some(terminal);
                    if let Some(metadata)=payload.get("usageMetadata").filter(|v|v.is_object()) {
                        let metadata=if wrapped {json!({"response":{"usageMetadata":metadata}})} else {json!({"usageMetadata":metadata})};
                        yield Ok(Bytes::from(provider_sse(protocol,"message",&metadata)));
                    }
                    continue;
                }
                if stopped {
                    let mut terminal=value.clone();
                    let wrapped=value.get("response").is_some_and(Value::is_object);
                    let current=if wrapped {&mut value["response"]} else {&mut value};
                    if let Some(candidates)=current.get_mut("candidates").and_then(Value::as_array_mut) {for candidate in candidates {if let Some(candidate)=candidate.as_object_mut() {candidate.remove("finishReason");}}}
                    let end=if wrapped {&mut terminal["response"]} else {&mut terminal};
                    if let Some(object)=end.as_object_mut() {object.remove("usageMetadata");}
                    if let Some(candidates)=end.get_mut("candidates").and_then(Value::as_array_mut) {for candidate in candidates {if let Some(candidate)=candidate.as_object_mut() {candidate.remove("content");}}}
                    delayed=Some(terminal);
                }
                yield Ok(Bytes::from(provider_sse(protocol,"message",&value)));
            }
        }
        drop(lease.take());
        if protocol != ProviderProtocol::GeminiGenerateContent {
            if protocol == ProviderProtocol::ChatCompletions { yield Ok(Bytes::from_static(b"data: [DONE]\n\n")); }
            if tolerate_eof { yield Ok(Bytes::from_static(b"\n\n")); }
            return;
        }
        if !pending.iter().all(u8::is_ascii_whitespace) {yield Err(std::io::Error::other("provider stream ended with an incomplete SSE frame"));return;}
        if let Some(terminal)=delayed {yield Ok(Bytes::from(provider_sse(protocol,"message",&terminal)));}
    })
}

fn provider_guard_stream(
    mut response: reqwest::Response,
    tolerate_incomplete_eof: bool,
    chat: bool,
) -> Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Send>> {
    let routing_attempt_lease = take_routing_attempt_lease(&mut response);
    Box::pin(stream! {
        let _routing_attempt_lease = routing_attempt_lease;
        let mut source = response.bytes_stream();
        while let Some(chunk) = source.next().await {
            yield chunk;
        }
        if chat {
            // Also support providers that close after finish/usage without [DONE].
            yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
        }
        if tolerate_incomplete_eof {
            // Anthropic-compatible servers are allowed by the provider policy to omit
            // the final blank SSE delimiter. Feed the same synthetic delimiter used by
            // the translated adapter so the empty-completion guard does not turn a
            // complete final frame into a retry failure.
            yield Ok(Bytes::from_static(b"\n\n"));
        }
    })
}

fn unwrap_gemini_stream_payload(value: &Value) -> &Value {
    value
        .get("response")
        .filter(|response| response.is_object())
        .unwrap_or(value)
}

pub(crate) fn provider_stream_event_is_terminal(protocol: ProviderProtocol, value: &Value) -> bool {
    match protocol {
        ProviderProtocol::Responses => matches!(
            value.get("type").and_then(Value::as_str),
            Some("response.completed" | "response.failed" | "response.incomplete")
        ),
        ProviderProtocol::ChatCompletions => {
            value
                .get("choices")
                .and_then(Value::as_array)
                .is_some_and(|choices| {
                    choices.iter().any(|choice| {
                        choice
                            .get("finish_reason")
                            .is_some_and(|reason| !reason.is_null())
                    })
                })
                || value.get("error").is_some()
        }
        ProviderProtocol::AnthropicMessages => {
            matches!(
                value.get("type").and_then(Value::as_str),
                Some("message_stop" | "error")
            ) || value.get("type").and_then(Value::as_str) == Some("message_delta")
                && value
                    .pointer("/delta/stop_reason")
                    .is_some_and(|reason| !reason.is_null())
        }
        ProviderProtocol::GeminiGenerateContent => {
            let payload = unwrap_gemini_stream_payload(value);
            payload
                .get("candidates")
                .and_then(Value::as_array)
                .is_some_and(|candidates| {
                    candidates.iter().any(|candidate| {
                        candidate
                            .get("finishReason")
                            .is_some_and(|reason| !reason.is_null())
                    })
                })
                || payload.get("error").is_some()
                || value.get("error").is_some()
        }
    }
}

fn provider_stream_event_is_visible_failure(protocol: ProviderProtocol, value: &Value) -> bool {
    match protocol {
        ProviderProtocol::Responses => {
            matches!(
                value.get("type").and_then(Value::as_str),
                Some("response.failed" | "response.incomplete")
            ) || value
                .pointer("/response/status")
                .and_then(Value::as_str)
                .is_some_and(|status| matches!(status, "failed" | "incomplete"))
                || value
                    .pointer("/response/error")
                    .is_some_and(|error| !error.is_null())
                || value
                    .pointer("/response/incomplete_details")
                    .is_some_and(|details| !details.is_null())
        }
        ProviderProtocol::ChatCompletions => {
            value.get("error").is_some()
                || value
                    .get("choices")
                    .and_then(Value::as_array)
                    .is_some_and(|choices| {
                        choices.iter().any(|choice| {
                            choice.get("error").is_some_and(|error| !error.is_null())
                                || choice
                                    .get("finish_reason")
                                    .and_then(Value::as_str)
                                    .is_some_and(|reason| !matches!(reason, "stop" | "tool_calls"))
                        })
                    })
        }
        ProviderProtocol::AnthropicMessages => {
            value.get("type").and_then(Value::as_str) == Some("error")
                || value
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| {
                        matches!(reason, "max_tokens" | "content_filter" | "refusal")
                    })
        }
        ProviderProtocol::GeminiGenerateContent => {
            let payload = unwrap_gemini_stream_payload(value);
            payload.get("error").is_some()
                || value.get("error").is_some()
                || payload
                    .get("candidates")
                    .and_then(Value::as_array)
                    .is_some_and(|candidates| {
                        candidates.iter().any(|candidate| {
                            candidate
                                .get("finishReason")
                                .and_then(Value::as_str)
                                .is_some_and(|reason| {
                                    gemini_finish_disposition(Some(reason))
                                        != GeminiFinishDisposition::Stop
                                })
                        })
                    })
        }
    }
}

fn provider_stream_event_is_error(protocol: ProviderProtocol, value: &Value) -> bool {
    match protocol {
        ProviderProtocol::Responses => {
            value.get("type").and_then(Value::as_str) == Some("response.failed")
                || value
                    .pointer("/response/error")
                    .is_some_and(|error| !error.is_null())
        }
        ProviderProtocol::ChatCompletions => {
            value.get("error").is_some_and(|error| !error.is_null())
                || value
                    .get("choices")
                    .and_then(Value::as_array)
                    .is_some_and(|choices| {
                        choices.iter().any(|choice| {
                            choice.get("error").is_some_and(|error| !error.is_null())
                                || choice
                                    .get("finish_reason")
                                    .and_then(Value::as_str)
                                    .is_some_and(|reason| {
                                        !matches!(
                                            reason,
                                            "stop"
                                                | "tool_calls"
                                                | "length"
                                                | "max_tokens"
                                                | "content_filter"
                                        )
                                    })
                        })
                    })
        }
        ProviderProtocol::GeminiGenerateContent => {
            value.get("error").is_some_and(|error| !error.is_null())
        }
        ProviderProtocol::AnthropicMessages => {
            value.get("type").and_then(Value::as_str) == Some("error")
        }
    }
}

pub(crate) fn provider_stream_event_is_empty_terminal(
    protocol: ProviderProtocol,
    value: &Value,
) -> bool {
    if protocol == ProviderProtocol::Responses {
        let response = value.get("response").unwrap_or(value);
        let reason = response
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str);
        if matches!(reason, Some("empty_response" | "invalid_tool_call"))
            && !responses_value_has_content(response)
        {
            return provider_stream_event_is_terminal(protocol, value);
        }
    }
    provider_stream_event_is_terminal(protocol, value)
        && !provider_stream_event_is_visible_failure(protocol, value)
        && match protocol {
            ProviderProtocol::Responses => value
                .get("response")
                .is_some_and(|response| !responses_value_has_content(response)),
            ProviderProtocol::ChatCompletions => !chat_value_has_content(value),
            ProviderProtocol::AnthropicMessages => true,
            ProviderProtocol::GeminiGenerateContent => !gemini_value_has_content(value),
        }
}

pub(crate) fn provider_stream_event_has_visible_content(
    protocol: ProviderProtocol,
    value: &Value,
) -> bool {
    match protocol {
        ProviderProtocol::Responses => responses_event_has_visible_content(value),
        ProviderProtocol::ChatCompletions => chat_value_has_content(value),
        ProviderProtocol::AnthropicMessages => match value.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                value.pointer("/content_block/type").and_then(Value::as_str) == Some("tool_use")
                    && json_tool_call_is_present(
                        value
                            .pointer("/content_block/name")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        value
                            .pointer("/content_block/input")
                            .unwrap_or(&Value::Null),
                    )
            }
            Some("content_block_delta") => {
                value.pointer("/delta/type").and_then(Value::as_str) == Some("input_json_delta")
                    && value
                        .pointer("/delta/partial_json")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.trim().is_empty())
                    || value
                        .pointer("/delta/text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| {
                            !text.trim().is_empty()
                                && !crate::translate::is_placeholder_progress_text(text)
                        })
            }
            _ => false,
        },
        ProviderProtocol::GeminiGenerateContent => gemini_value_has_content(value),
    }
}

fn provider_stream_event_is_reasoning(protocol: ProviderProtocol, value: &Value) -> bool {
    match protocol {
        ProviderProtocol::Responses => {
            value
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| {
                    kind.contains("reasoning")
                        || matches!(
                            kind,
                            "response.output_item.added" | "response.output_item.done"
                        ) && value.pointer("/item/type").and_then(Value::as_str)
                            == Some("reasoning")
                })
        }
        ProviderProtocol::AnthropicMessages => value
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| {
                kind == "content_block_start"
                    && matches!(
                        value.pointer("/content_block/type").and_then(Value::as_str),
                        Some("thinking" | "redacted_thinking")
                    )
                    || kind == "content_block_delta"
                        && matches!(
                            value.pointer("/delta/type").and_then(Value::as_str),
                            Some("thinking_delta" | "signature_delta")
                        )
            }),
        ProviderProtocol::GeminiGenerateContent => value
            .get("candidates")
            .and_then(Value::as_array)
            .is_some_and(|candidates| {
                candidates.iter().any(|candidate| {
                    candidate
                        .pointer("/content/parts")
                        .and_then(Value::as_array)
                        .is_some_and(|parts| {
                            parts.iter().any(|part| {
                                part.get("thought").and_then(Value::as_bool) == Some(true)
                                    || part.get("thoughtSignature").is_some()
                            })
                        })
                })
            }),
        ProviderProtocol::ChatCompletions => value
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(|choices| {
                choices.iter().any(|choice| {
                    choice
                        .get("delta")
                        .or_else(|| choice.get("message"))
                        .is_some_and(|message| {
                            message.get("reasoning_content").is_some()
                                || message.get("reasoning").is_some()
                        })
                })
            }),
    }
}

fn responses_event_has_visible_content(value: &Value) -> bool {
    match value.get("type").and_then(Value::as_str) {
        Some("response.completed") => value
            .get("response")
            .is_some_and(responses_value_has_content),
        Some("response.output_text.delta" | "response.output_text.done") => value
            .get("delta")
            .or_else(|| value.get("text"))
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty()),
        Some(
            "response.function_call_arguments.delta"
            | "response.function_call_arguments.done"
            | "response.custom_tool_call_input.delta"
            | "response.custom_tool_call_input.done"
            | "response.tool_search_call_arguments.delta"
            | "response.tool_search_call_arguments.done"
            | "response.local_shell_call.in_progress"
            | "response.local_shell_call.completed"
            | "response.web_search_call.in_progress"
            | "response.web_search_call.searching"
            | "response.web_search_call.completed",
        ) => true,
        Some("response.output_item.added" | "response.output_item.done") => value
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|kind| {
                matches!(
                    kind,
                    "function_call"
                        | "custom_tool_call"
                        | "tool_search_call"
                        | "web_search_call"
                        | "local_shell_call"
                )
            }),
        _ => false,
    }
}

fn responses_reasoning_semantic_history(value: &Value) -> Vec<Value> {
    let mut history = Vec::new();
    match value.get("type").and_then(Value::as_str) {
        Some("response.output_item.done")
            if value.pointer("/item/type").and_then(Value::as_str) == Some("reasoning") =>
        {
            if let Some(mut item) = value.get("item").cloned() {
                if let Some(object) = item.as_object_mut() {
                    object.remove("id");
                    object.remove("status");
                }
                history.push(item);
            }
        }
        Some("response.completed") => {
            if let Some(output) = value.pointer("/response/output").and_then(Value::as_array) {
                for item in output
                    .iter()
                    .filter(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
                {
                    let mut item = item.clone();
                    if let Some(object) = item.as_object_mut() {
                        object.remove("id");
                        object.remove("status");
                    }
                    history.push(item);
                }
            }
        }
        _ => {}
    }
    history
}

fn canonical_reasoning_lifecycle_events(items: &[Value]) -> Vec<Value> {
    let mut events = Vec::new();
    for (output_index, item) in items.iter().enumerate() {
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        events.push(json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": added,
        }));
        events.push(json!({
            "type": "response.output_item.done",
            "output_index": output_index,
            "item": item,
        }));
    }
    events
}

fn assign_responses_sequence(value: &mut Value, next: &mut u64) {
    value["sequence_number"] = json!(*next);
    *next = next.saturating_add(1);
}

fn shift_responses_retry_event(value: &mut Value, offset: usize, history: &[Value]) {
    if offset == 0 {
        return;
    }
    if let Some(index) = value.get("output_index").and_then(Value::as_u64) {
        value["output_index"] = json!(index.saturating_add(offset as u64));
    }
    if matches!(
        value.get("type").and_then(Value::as_str),
        Some("response.completed" | "response.incomplete" | "response.failed")
    ) {
        if let Some(output) = value
            .pointer_mut("/response/output")
            .and_then(Value::as_array_mut)
        {
            let mut combined = history.to_vec();
            combined.append(output);
            *output = combined;
        }
    }
}

fn chat_value_has_content(value: &Value) -> bool {
    value
        .get("choices")
        .and_then(Value::as_array)
        .is_some_and(|choices| {
            choices.iter().any(|choice| {
                choice
                    .get("delta")
                    .or_else(|| choice.get("message"))
                    .is_some_and(|message| {
                        message
                            .get("content")
                            .and_then(Value::as_str)
                            .is_some_and(|text| {
                                !text.trim().is_empty()
                                    && !crate::translate::is_placeholder_progress_text(text)
                            })
                            || message
                                .get("tool_calls")
                                .and_then(Value::as_array)
                                .is_some_and(|calls| {
                                    calls.iter().any(chat_tool_call_has_visible_content)
                                })
                    })
            })
        })
}

fn chat_tool_call_has_visible_content(call: &Value) -> bool {
    let name = call
        .pointer("/function/name")
        .or_else(|| call.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    json_tool_call_is_present(
        name,
        call.pointer("/function/arguments")
            .or_else(|| call.get("arguments"))
            .or_else(|| call.get("args"))
            .or_else(|| call.get("input"))
            .unwrap_or(&Value::Null),
    )
}

fn json_tool_call_is_present(name: &str, arguments: &Value) -> bool {
    let arguments = match arguments {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    crate::translate::custom_tool_input_is_present(name, &arguments)
}

fn gemini_value_has_content(value: &Value) -> bool {
    let value=unwrap_gemini_stream_payload(value);
    value
        .get("candidates")
        .and_then(Value::as_array)
        .is_some_and(|candidates| {
            candidates.iter().any(|candidate| {
                candidate
                    .pointer("/content/parts")
                    .and_then(Value::as_array)
                    .is_some_and(|parts| parts.iter().any(gemini_part_has_visible_content))
            })
        })
}

fn gemini_part_has_visible_content(part: &Value) -> bool {
    if part.get("thought").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    if part
        .get("text")
        .and_then(Value::as_str)
        .is_some_and(|text| {
            !text.trim().is_empty() && !crate::translate::is_placeholder_progress_text(text)
        })
    {
        return true;
    }
    part.get("functionCall")
        .or_else(|| part.get("function_call"))
        .and_then(Value::as_object)
        .is_some_and(|call| {
            json_tool_call_is_present(
                call.get("name").and_then(Value::as_str).unwrap_or(""),
                call.get("args")
                    .or_else(|| call.get("arguments"))
                    .or_else(|| call.get("input"))
                    .unwrap_or(&Value::Null),
            )
        })
}

fn provider_sse(protocol: ProviderProtocol, event_type: &str, value: &Value) -> String {
    match protocol {
        ProviderProtocol::Responses | ProviderProtocol::AnthropicMessages => sse(event_type, value),
        ProviderProtocol::ChatCompletions | ProviderProtocol::GeminiGenerateContent => format!(
            "data: {}\n\n",
            serde_json::to_string(value).unwrap_or_else(|_| "{}".into())
        ),
    }
}

fn provider_error_event_type(protocol: ProviderProtocol) -> &'static str {
    match protocol {
        ProviderProtocol::Responses => "response.failed",
        ProviderProtocol::AnthropicMessages => "error",
        ProviderProtocol::ChatCompletions | ProviderProtocol::GeminiGenerateContent => "message",
    }
}

fn empty_completion_failed_event_for_protocol(
    protocol: ProviderProtocol,
    usage: TokenUsage,
    response_id: Option<&str>,
) -> Value {
    let message = "The model returned an empty completion and the retry failed or was empty again.";
    match protocol {
        ProviderProtocol::Responses => json!({
            "type": "response.failed",
            "response": {
                "id": response_id
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("resp_{}", Uuid::new_v4().simple())),
                "status": "failed",
                "output": [],
                "error": {
                    "type": "upstream_error",
                    "code": EMPTY_COMPLETION_RETRY_FAILED_CODE,
                    "message": message
                },
                "usage": token_usage_json(&usage)
            }
        }),
        ProviderProtocol::ChatCompletions | ProviderProtocol::GeminiGenerateContent => json!({
            "error": {
                "type": "upstream_error",
                "code": EMPTY_COMPLETION_RETRY_FAILED_CODE,
                "message": message
            },
            "usage": token_usage_json(&usage)
        }),
        ProviderProtocol::AnthropicMessages => json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "code": EMPTY_COMPLETION_RETRY_FAILED_CODE,
                "message": message
            },
            "usage": token_usage_json(&usage)
        }),
    }
}

fn empty_completion_retry_failed_response(
    usage: TokenUsage,
    additional_sends: u16,
) -> Result<reqwest::Response, AttemptFailure> {
    empty_completion_retry_failed_response_with_observation(usage, additional_sends, None)
}

fn empty_completion_retry_failed_response_with_observation(
    usage: TokenUsage,
    additional_sends: u16,
    retry_observation: Option<ProviderRetryObservation>,
) -> Result<reqwest::Response, AttemptFailure> {
    let mut observation = retry_observation.unwrap_or_default();
    // The failure body/marker owns all known consumption. Metadata must not
    // charge the preceding HTTP attempts for a second time.
    let usage = sum_token_usage(std::mem::take(&mut observation.usage), usage);
    observation.known_billing = Some(usage.clone());
    empty_completion_retry_failed_response_with_observation_inner(usage, additional_sends, Some(observation))
}

fn empty_completion_retry_failed_response_with_observation_inner(
    usage: TokenUsage,
    additional_sends: u16,
    mut retry_observation: Option<ProviderRetryObservation>,
) -> Result<reqwest::Response, AttemptFailure> {
    let value = json!({
        "error": {
            "type": "upstream_error",
            "code": EMPTY_COMPLETION_RETRY_FAILED_CODE,
            "message": "The model returned an empty completion and the retry failed or was empty again."
        },
        "usage": token_usage_json(&usage)
    });
    let mut response = axum::http::Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .header(header::CONTENT_TYPE, "application/json")
        .body(reqwest::Body::from(value.to_string()))
        .map(reqwest::Response::from)
        .map_err(|_| request_failure("gateway_error", "failed to build empty completion error"))?;
    response
        .extensions_mut()
        .insert(EmptyCompletionRecoveryFailure {
            usage: usage.clone(),
            additional_sends,
        });
    let observation = retry_observation.get_or_insert_with(ProviderRetryObservation::default);
    observation.additional_sends = observation
        .additional_sends
        .saturating_add(additional_sends);
    observation.recovery_kinds.extend(
        std::iter::repeat("empty-completion".to_string()).take(usize::from(additional_sends)),
    );
    observation.known_billing = Some(usage);
    response.extensions_mut().insert(observation.clone());
    Ok(response)
}

async fn response_body_usage(response: Response<Body>) -> TokenUsage {
    let mut usage = response.extensions().get::<crate::observability::ReportedUpstreamUsage>().map(|usage| usage.0.clone()).unwrap_or_default();
    let body_usage = to_bytes(response.into_body(), 64 * 1024).await.ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()).as_ref().map(TokenUsage::from_json).unwrap_or_default();
    usage.merge_max(body_usage);
    usage
}

async fn response_body_usage_from_reqwest(response: reqwest::Response) -> TokenUsage {
    match super::stream::bounded_json_with_usage(response, 64 * 1024).await {
        Ok(value) => TokenUsage::from_json(&value), Err((_, usage)) => usage,
    }
}

pub(crate) fn sum_token_usage(left: TokenUsage, right: TokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: left.input_tokens.saturating_add(right.input_tokens),
        output_tokens: left.output_tokens.saturating_add(right.output_tokens),
        cached_input_tokens: left
            .cached_input_tokens
            .saturating_add(right.cached_input_tokens),
        cache_creation_input_tokens: left.cache_creation_input_tokens.saturating_add(right.cache_creation_input_tokens),
        reasoning_tokens: left.reasoning_tokens.saturating_add(right.reasoning_tokens),
        total_tokens: left.total_tokens.saturating_add(right.total_tokens),
        cache_read_reported: (left.input_tokens == 0 || left.cache_read_reported)
            && (right.input_tokens == 0 || right.cache_read_reported)
            && (left.cache_read_reported || right.cache_read_reported),
    }
}

pub(crate) fn token_usage_json(usage: &TokenUsage) -> Value {
    let mut input_details = serde_json::Map::new();
    if usage.cache_read_reported { input_details.insert("cached_tokens".into(), json!(usage.cached_input_tokens)); }
    if usage.cache_creation_input_tokens > 0 { input_details.insert("cache_creation_tokens".into(), json!(usage.cache_creation_input_tokens)); }
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "total_tokens": usage.total_tokens.max(
            usage.input_tokens.saturating_add(usage.output_tokens)
        ),
        "input_tokens_details": input_details,
        "output_tokens_details": {"reasoning_tokens": usage.reasoning_tokens}
    })
}

fn write_raw_anthropic_usage(value:&mut Value,usage:&TokenUsage) {
    if !value.get("usage").is_some_and(Value::is_object) {value["usage"]=json!({});}
    let raw=value.get_mut("usage").and_then(Value::as_object_mut);
    if let Some(raw)=raw {
        raw.insert("input_tokens".into(),json!(usage.input_tokens.saturating_sub(if usage.cache_read_reported {usage.cached_input_tokens} else {0}).saturating_sub(usage.cache_creation_input_tokens)));
        raw.insert("output_tokens".into(),json!(usage.output_tokens));
        if usage.cache_read_reported {raw.insert("cache_read_input_tokens".into(),json!(usage.cached_input_tokens));}
        else {raw.remove("cache_read_input_tokens");}
        if usage.cache_creation_input_tokens>0 || raw.contains_key("cache_creation_input_tokens") {raw.insert("cache_creation_input_tokens".into(),json!(usage.cache_creation_input_tokens));}
    }
}
fn write_token_usage(value: &mut Value, usage: TokenUsage) {
    if value.get("response").is_some_and(|response|response.get("usageMetadata").is_some() || response.get("candidates").is_some()) {
        write_token_usage(&mut value["response"],usage);return;
    }
    if value.get("type").and_then(Value::as_str)==Some("message") || value.get("content").is_some() && value.get("stop_reason").is_some() {
        write_raw_anthropic_usage(value,&usage);return;
    }
    let canonical = token_usage_json(&usage);
    if value
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| {
            matches!(
                kind,
                "response.completed" | "response.failed" | "response.incomplete"
            )
        })
    {
        value["response"]["usage"] = canonical;
    } else if value.get("candidates").is_some() || value.get("usageMetadata").is_some() {
        value["usageMetadata"] = json!({
            "promptTokenCount": usage.input_tokens,
            "candidatesTokenCount": usage.output_tokens.saturating_sub(usage.reasoning_tokens),
            "thoughtsTokenCount": usage.reasoning_tokens,
            "totalTokenCount": usage.total_tokens.max(
                usage.input_tokens.saturating_add(usage.output_tokens)
            )
        });
        if usage.cache_read_reported {value["usageMetadata"]["cachedContentTokenCount"]=json!(usage.cached_input_tokens);}
    } else if value.get("choices").is_some() {
        value["usage"] = json!({
            "prompt_tokens": usage.input_tokens,
            "completion_tokens": usage.output_tokens,
            "total_tokens": usage.total_tokens.max(
                usage.input_tokens.saturating_add(usage.output_tokens)
            ),
            "prompt_tokens_details": if usage.cache_read_reported { json!({"cached_tokens": usage.cached_input_tokens}) } else { json!({}) },
            "completion_tokens_details": {"reasoning_tokens": usage.reasoning_tokens}
        });
    } else {
        value["usage"] = canonical;
    }
}

fn write_anthropic_stream_start_usage(value: &mut Value, usage: TokenUsage) {
    write_raw_anthropic_usage(&mut value["message"],&usage);
}

fn write_stream_token_usage(protocol: ProviderProtocol, value: &mut Value, usage: TokenUsage) {
    if protocol == ProviderProtocol::AnthropicMessages
        && value.get("type").and_then(Value::as_str) == Some("message_delta")
    {
        let has_input_update=value.get("usage").and_then(Value::as_object).is_some_and(|raw|["input_tokens","cache_read_input_tokens","cache_creation_input_tokens"].iter().any(|key|raw.contains_key(*key)));
        if has_input_update {write_raw_anthropic_usage(value,&usage);}
        else {value["usage"]["output_tokens"] = json!(usage.output_tokens);}
        return;
    }
    write_token_usage(value, usage);
}

pub(crate) fn completion_is_empty(value: &Value) -> bool {
    let value=if value.get("response").is_some_and(|response|response.get("candidates").is_some()||response.get("usageMetadata").is_some()) {&value["response"]} else {value};
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return false;
    }
    let incomplete_reason = value
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str);
    if matches!(
        incomplete_reason,
        Some("empty_response" | "invalid_tool_call")
    ) && !responses_value_has_content(value)
    {
        // Translated streams mark placeholder-only / empty apply_patch turns
        // incomplete. Retry them; do not treat the incomplete flag as content.
        return true;
    }
    if value
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| matches!(status, "failed" | "incomplete"))
        || value
            .get("incomplete_details")
            .is_some_and(|details| !details.is_null())
    {
        return false;
    }
    if value.get("output").is_some_and(Value::is_array)
        || value.get("status").and_then(Value::as_str) == Some("completed")
    {
        return !responses_value_has_content(value);
    }
    if value.get("stop_reason").and_then(Value::as_str) == Some("end_turn") {
        return !value
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|content| {
                content.iter().any(|part| {
                    part.get("type").and_then(Value::as_str) == Some("text")
                        && part
                            .get("text")
                            .and_then(Value::as_str)
                            .is_some_and(|text| !text.is_empty())
                        || part.get("type").and_then(Value::as_str) == Some("tool_use")
                })
            });
    }
    if let Some(choices) = value.get("choices").and_then(Value::as_array) {
        return choices.is_empty()
            || choices.iter().all(|choice| {
                if choice
                    .get("finish_reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| {
                        matches!(reason, "length" | "max_tokens" | "content_filter")
                    })
                {
                    return false;
                }
                let message = choice.get("message").or_else(|| choice.get("delta"));
                message.is_none_or(|message| {
                    message
                        .get("content")
                        .and_then(Value::as_str)
                        .is_none_or(|text| {
                            text.is_empty() || crate::translate::is_placeholder_progress_text(text)
                        })
                        && message
                            .get("tool_calls")
                            .and_then(Value::as_array)
                            .is_none_or(|calls| {
                                calls
                                    .iter()
                                    .all(|call| !chat_tool_call_has_visible_content(call))
                            })
                })
            });
    }
    if let Some(content) = value.get("content").and_then(Value::as_array) {
        if value
            .get("stop_reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| matches!(reason, "max_tokens" | "content_filter" | "refusal"))
        {
            return false;
        }
        return !content
            .iter()
            .any(|part| match part.get("type").and_then(Value::as_str) {
                Some("text") => part
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty()),
                Some("tool_use") => true,
                _ => false,
            });
    }
    if let Some(candidates) = value.get("candidates").and_then(Value::as_array) {
        if candidates.iter().any(|candidate| {
            candidate
                .get("finishReason")
                .and_then(Value::as_str)
                .is_some_and(|reason| {
                    gemini_finish_disposition(Some(reason)) != GeminiFinishDisposition::Stop
                })
        }) || value.get("promptFeedback").is_some_and(|feedback| {
            feedback
                .get("blockReason")
                .is_some_and(|reason| !reason.is_null())
        }) {
            return false;
        }
        return !candidates.iter().any(|candidate| {
            candidate
                .pointer("/content/parts")
                .and_then(Value::as_array)
                .is_some_and(|parts| {
                    parts
                        .iter()
                        .any(|part| gemini_part_has_visible_content(part))
                })
        });
    }
    false
}

fn responses_value_has_content(value: &Value) -> bool {
    value
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|output| {
            output
                .iter()
                .any(|item| match item.get("type").and_then(Value::as_str) {
                    Some("message" | "agent_message") => item
                        .get("content")
                        .and_then(Value::as_array)
                        .is_some_and(|parts| {
                            parts.iter().any(|part| {
                                matches!(
                                    part.get("type").and_then(Value::as_str),
                                    Some("output_text" | "text")
                                ) && part
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .is_some_and(|text| {
                                        !text.is_empty()
                                            && !crate::translate::is_placeholder_progress_text(text)
                                    })
                            })
                        }),
                    Some("custom_tool_call") => {
                        item.get("status").and_then(Value::as_str) != Some("incomplete")
                            && crate::translate::custom_tool_input_is_actionable(
                                item.get("name").and_then(Value::as_str).unwrap_or(""),
                                item.get("input").and_then(Value::as_str).unwrap_or(""),
                            )
                    }
                    Some(
                        "function_call" | "tool_search_call" | "web_search_call"
                        | "local_shell_call",
                    ) => true,
                    _ => false,
                })
        })
}

pub(crate) fn retry_backoff(attempt: u8) -> Duration {
    Duration::from_millis(250_u64.saturating_mul(1_u64 << attempt.min(4)))
}

pub(crate) async fn send_candidate_once(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
    cloud_request_id: Option<&str>,
    cloud_envelope_cache: Option<&Arc<Mutex<Option<Value>>>>,
) -> Result<reqwest::Response, AttemptFailure> {
    validate_candidate_image_references(body, candidate)?;
    // Resolve once per physical Anthropic send, then reuse that very request's
    // auth for both signature admission and transport (including refresh retries).
    let authenticated_request = if candidate.provider.transport == ProviderTransport::Standard
        && candidate.provider.protocol_for_model(&candidate.upstream_model) == ProviderProtocol::AnthropicMessages
    {
        let wire_model = wire_model_for_request(candidate, body);
        let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        let endpoint = candidate.provider.endpoint_for_model_streaming(&wire_model, streaming);
        let request = apply_provider_auth(state.client.post(endpoint), &candidate.provider, candidate.credential.as_ref())
            .await.map_err(|message| AttemptFailure {
                response:error_response(StatusCode::UNAUTHORIZED,"missing_provider_credential",&message),
                kind:AttemptFailureKind::Credential,
            })?;
        let request = if candidate.credential.as_ref().unwrap_or(&candidate.provider.credential).source == CredentialSource::Forward {
            let headers = caller_headers.ok_or_else(|| AttemptFailure {
                response: error_response(StatusCode::UNAUTHORIZED, "missing_forward_credential", "caller authentication headers are required"),
                kind: AttemptFailureKind::Credential,
            })?;
            apply_forward_headers(request, &state.settings, headers).await.map_err(|message| AttemptFailure {
                response: error_response(StatusCode::UNAUTHORIZED, "missing_forward_credential", &message),
                kind: AttemptFailureKind::Credential,
            })?
        } else { request };
        Some(request.build().map_err(|_| request_failure("invalid_request", "cannot build authenticated provider request"))?)
    } else { None };
    let resolved_scope = authenticated_request.as_ref()
        .and_then(|request| resolved_anthropic_checkpoint_scope(candidate, request));
    let had_checkpoint = body.get("input").and_then(Value::as_array).is_some_and(|items| items.iter().any(crate::anthropic_compaction::is_item));
    // This function is also called directly by retry/compact paths.
    let mut scoped_body;
    let body = if body.get("input").and_then(Value::as_array).is_some_and(|items| items.iter().any(crate::anthropic_compaction::is_item)) {
        scoped_body = body.clone();
        crate::anthropic_compaction::expand_retained(&mut scoped_body).map_err(|message|request_failure("invalid_compaction_history",&message))?;
        prepare_native_anthropic_history(&mut scoped_body, candidate, resolved_scope.as_deref()).map_err(|message| request_failure("invalid_compaction_history", &message))?;
        &scoped_body
    } else { body };
    let remote_compaction = request_is_remote_compaction(body);
    let native_compact = remote_compaction && body.get(crate::anthropic_compaction::REQUEST_FLAG).and_then(Value::as_bool) == Some(true)
        && candidate.provider.protocol_for_model(&candidate.upstream_model) == ProviderProtocol::AnthropicMessages;
    let source_image_count = count_translated_input_images(body);
    let protocol = candidate
        .provider
        .protocol_for_model(&candidate.upstream_model);
    let wire_model = wire_model_for_request(candidate, body);
    if candidate.provider.transport == ProviderTransport::Kiro {
        return send_kiro_candidate(state, body, candidate, caller_headers, &wire_model).await;
    }
    if candidate.provider.transport == ProviderTransport::GithubCopilot {
        return send_github_copilot_candidate(state, body, candidate, caller_headers, &wire_model)
            .await;
    }
    let mut upstream_body = match protocol {
        ProviderProtocol::Responses => body.clone(),
        ProviderProtocol::ChatCompletions => match responses_to_chat_with_options(
            &chat_compatible_request(body, candidate.capabilities.provider_metadata),
            &wire_model,
            model_requires_reasoning_placeholder(&candidate.provider, &candidate.upstream_model),
        ) {
            Ok(value) => value,
            Err(message) => return Err(request_failure("unsupported_request", &message)),
        },
        ProviderProtocol::AnthropicMessages => {
            match responses_to_anthropic_with_oauth(
                body,
                &wire_model,
                uses_anthropic_subscription_oauth(&candidate.provider),
            ) {
                Ok(value) => value,
                Err(message) => return Err(request_failure("unsupported_request", &message)),
            }
        }
        ProviderProtocol::GeminiGenerateContent => match responses_to_gemini(body, &wire_model) {
            Ok(value) => value,
            Err(message) => return Err(request_failure("unsupported_request", &message)),
        },
    };
    // Continuity control fields stay on `body` for local `remember`; never wire them.
    crate::response_state::ResponseStateStore::strip_private_fields(&mut upstream_body);
    if let Some(object)=upstream_body.as_object_mut() {
        object.remove(crate::anthropic_compaction::REQUEST_FLAG);
        object.remove(crate::anthropic_compaction::HISTORY_REWRITTEN);
    }
    if !remote_compaction || native_compact {
        if let Err(message) =
            apply_provider_wire_compatibility(&mut upstream_body, body, candidate, protocol)
        {
            return Err(request_failure("unsupported_request", &message));
        }
    }
    let has_native_block = protocol == ProviderProtocol::AnthropicMessages && upstream_body.get("messages").and_then(Value::as_array)
        .is_some_and(|messages| messages.iter().any(|m| m.get("content").and_then(Value::as_array)
            .is_some_and(|blocks| blocks.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("compaction")))));
    let thinking_provenance = (protocol == ProviderProtocol::AnthropicMessages).then(||
        super::anthropic_compact::ThinkingProvenance::for_wire(&upstream_body, resolved_scope.as_deref(), had_checkpoint));
    if let Some(proof) = &thinking_provenance {
        super::anthropic_compact::filter_wire_thinking(&mut upstream_body, body, proof);
    }
    let mut native_context = native_compact.then(|| NativeAnthropicContext { scope:resolved_scope.clone().unwrap_or_else(|| crate::anthropic_compaction::digest(&json!({"unbound":Uuid::new_v4().to_string()}))),context_hash:crate::anthropic_compaction::context_hash(&upstream_body),
        history_rewritten: body.get(crate::anthropic_compaction::HISTORY_REWRITTEN).and_then(Value::as_bool)==Some(true) });
    if native_compact {
        upstream_body["compaction"] = json!({"type":"summarize"});
        if let Some(object)=upstream_body.as_object_mut() {
            object.remove("context_management");object.remove("stop_sequences");
            if object.get("tool_choice").and_then(|v| v.get("type")).and_then(Value::as_str).is_some_and(|s|matches!(s,"any"|"tool")) {object.remove("tool_choice");}
            if let Some(config)=object.get_mut("output_config").and_then(Value::as_object_mut) {config.remove("format");
                if let Some(budget)=config.get_mut("task_budget").and_then(Value::as_object_mut) {budget.remove("remaining");}
                if config.is_empty() {object.remove("output_config");}
            }
        }
    }
    let antigravity_project = if protocol == ProviderProtocol::GeminiGenerateContent
        && candidate.provider.google_mode == GoogleMode::CloudCodeAssist
        && candidate.provider.project.is_none()
        && candidate
            .credential
            .as_ref()
            .unwrap_or(&candidate.provider.credential)
            .source
            == CredentialSource::OAuth
        && candidate
            .credential
            .as_ref()
            .unwrap_or(&candidate.provider.credential)
            .reference
            .as_deref()
            == Some("google-antigravity")
    {
        let token = resolve_oauth_access_token("google-antigravity")
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_provider_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?;
        Some(
            resolve_antigravity_cloud_project(&token)
                .await
                .map_err(|message| AttemptFailure {
                    response: error_response(
                        StatusCode::BAD_GATEWAY,
                        "provider_configuration_error",
                        &message,
                    ),
                    kind: AttemptFailureKind::Retryable,
                })?,
        )
    } else {
        None
    };
    if protocol == ProviderProtocol::GeminiGenerateContent
        && candidate.provider.google_mode == GoogleMode::CloudCodeAssist
    {
        if let Some(cache) = cloud_envelope_cache {
            let mut cached = cache.lock().await;
            if let Some(envelope) = cached.as_ref() {
                upstream_body = envelope.clone();
            } else {
                upstream_body = cloud_code_assist_envelope(
                    upstream_body,
                    body,
                    candidate,
                    &wire_model,
                    antigravity_project.as_deref(),
                    cloud_request_id,
                )?;
                *cached = Some(upstream_body.clone());
            }
        } else {
            upstream_body = cloud_code_assist_envelope(
                upstream_body,
                body,
                candidate,
                &wire_model,
                antigravity_project.as_deref(),
                cloud_request_id,
            )?;
        }
    }
    if protocol != ProviderProtocol::GeminiGenerateContent {
        upstream_body["model"] = Value::String(wire_model.clone());
    }
    if remote_compaction && protocol == ProviderProtocol::Responses {
        // Match OpenCodex's normal Responses adapter: preserve the v2
        // compaction trigger and top-level options, expand local `codetas1:`
        // envelopes, and remove replayed raw reasoning content that the native
        // backend rejects.
        crate::compat::sanitize_compact_trigger_request(&mut upstream_body);
        crate::debug::log(
            "send_candidate: applied compact-trigger reasoning sanitize; broader compatibility skipped",
        );
    }
    if protocol == ProviderProtocol::Responses {
        // Provider metadata must remain on `body` for local continuation and
        // switching back to Gemini/Anthropic, but strict Responses backends
        // reject these private per-item fields (including compact requests).
        crate::compat::strip_responses_provider_metadata(&mut upstream_body);
    }
    let mut wire_budget_omissions = 0_usize;
    if !remote_compaction
        && matches!(
            protocol,
            ProviderProtocol::AnthropicMessages
                | ProviderProtocol::GeminiGenerateContent
                | ProviderProtocol::ChatCompletions
        )
    {
        let image_report = normalize_translated_image_history(
            &mut upstream_body,
            protocol,
            candidate.provider.transport,
            candidate.provider.limits.max_request_bytes,
        )
        .await
        .map_err(image_history_failure)?;
        wire_budget_omissions = image_report.omitted_images;
        if image_report.omitted_images > 0 {
            crate::debug::log(&format!(
                "wire image history normalized: provider={} model={} protocol={:?} omitted={} total={} kept_base64_chars={}",
                candidate.provider.id,
                candidate.upstream_model,
                protocol,
                image_report.omitted_images,
                image_report.inline_images,
                image_report.kept_base64_chars,
            ));
        }
    }
    let translation_image_omissions =
        source_image_count.saturating_sub(count_translated_input_images(&upstream_body));
    let _ = wire_budget_omissions;

    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    // Compact triggers bypass broad compatibility rewriting, not the wire byte
    // budget. Work on the outbound clone so replay/checkpoint history stays intact.
    let (serialized, wire_image_omissions) = if remote_compaction {
        // This is an explicit history-reset boundary, never an inference retry.
        crate::translate::serialize_with_image_budget_async(
            &mut upstream_body,
            candidate.provider.limits.max_request_bytes,
        ).await.map_err(|error| request_failure("invalid_request", &error))?
    } else {
        (crate::translate::serialize_preserving_image_history_async(
            &mut upstream_body,
            candidate.provider.limits.max_request_bytes,
        ).await.map_err(image_history_failure)?, 0)
    };
    if let Some(context)=native_context.as_mut() {
        context.history_rewritten |= translation_image_omissions > 0 || wire_image_omissions > 0;
    }
    if remote_compaction && wire_image_omissions > 0 {
        crate::debug::log(&format!(
            "compact wire image budget: provider={} omitted={} request_bytes={}",
            candidate.provider.id, wire_image_omissions, serialized.len(),
        ));
    }
    if serialized.len() as u64 > candidate.provider.limits.max_request_bytes {
        return Err(request_failure(
            "request_too_large",
            &format!(
                "translated request exceeds the configured provider limit: provider={} model={} request_bytes={} max_request_bytes={}",
                candidate.provider.id,
                candidate.upstream_model,
                serialized.len(),
                candidate.provider.limits.max_request_bytes,
            ),
        ));
    }

    let mut query_params = candidate.provider.query_params.clone();
    if let Some(version) = candidate.provider.azure_api_version.as_ref() {
        query_params.insert("api-version".into(), version.clone());
    }
    if protocol == ProviderProtocol::GeminiGenerateContent && streaming {
        query_params.insert("alt".into(), "sse".into());
    }

    let endpoint = candidate
        .provider
        .endpoint_for_model_streaming(&wire_model, streaming);
    let dns_pinning = state.settings.read().await.security.dns_pinning;
    let client = if dns_pinning {
        let client = pinned_client(&endpoint, &candidate.provider, "CODETAS-Gateway/0.1")
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "provider_dns_rejected",
                    &message,
                ),
                kind: AttemptFailureKind::Retryable,
            })?;
        client
    } else {
        state.client.clone()
    };
    let auth_already_resolved = authenticated_request.is_some();
    let mut request = match authenticated_request {
        Some(request) => reqwest::RequestBuilder::from_parts(client.clone(), request),
        None => client.post(endpoint.clone()),
    }
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::ACCEPT,
            if streaming {
                "text/event-stream"
            } else {
                "application/json"
            },
        )
        .query(&query_params)
        .timeout(Duration::from_millis(
            candidate.provider.limits.request_timeout_ms,
        ));
    if protocol == ProviderProtocol::AnthropicMessages {
        request = request.header("anthropic-version", "2023-06-01");
        if uses_anthropic_subscription_oauth(&candidate.provider) {
            for (name, value) in anthropic_subscription_oauth_headers() {
                request = request.header(name, value);
            }
        }
    }
    let request = match if auth_already_resolved { Ok(request) } else { apply_provider_auth(
        request,
        &candidate.provider,
        candidate.credential.as_ref(),
    ).await }
    {
        Ok(request) => request,
        Err(message) => {
            return Err(AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_provider_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })
        }
    };
    let credential_source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let request = if credential_source == CredentialSource::Forward && !auth_already_resolved {
        let Some(caller_headers) = caller_headers else {
            return Err(AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    "the selected provider requires caller authentication headers",
                ),
                kind: AttemptFailureKind::Credential,
            });
        };
        apply_forward_headers(request, &state.settings, caller_headers)
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?
    } else {
        request
    };
    let request = if candidate.provider.google_mode == GoogleMode::CloudCodeAssist {
        request.header(header::USER_AGENT, "antigravity")
    } else {
        request
    };

    let messages = upstream_body
        .get("messages")
        .and_then(serde_json::Value::as_array);
    let reasoning_count = messages.map(|m| {
        m.iter()
            .filter(|msg| msg.get("reasoning_content").is_some())
            .count()
    });
    let assistant_missing_reasoning = messages.map(|m| {
        m.iter()
            .filter(|msg| {
                msg.get("role").and_then(serde_json::Value::as_str) == Some("assistant")
                    && msg.get("reasoning_content").is_none()
            })
            .count()
    });
    let assistant_with_reasoning = messages.map(|m| {
        m.iter()
            .filter(|msg| {
                msg.get("role").and_then(serde_json::Value::as_str) == Some("assistant")
                    && msg.get("reasoning_content").is_some()
            })
            .count()
    });
    crate::debug::log(&format!(
        "send_candidate_once: POST {} (stream={} store={} input_items={} messages={} reasoning={} asst_no_reasoning={} asst_reasoning={} model={})",
        endpoint,
        upstream_body
            .get("stream")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        upstream_body
            .get("store")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
        upstream_body
            .get("input")
            .and_then(serde_json::Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0),
        messages.map(|m| m.len()).unwrap_or(0),
        reasoning_count.unwrap_or(0),
        assistant_missing_reasoning.unwrap_or(0),
        assistant_with_reasoning.unwrap_or(0),
        wire_model
    ));
    let mut request_diagnostic =
        diagnostics::wire_request_diagnostic(&upstream_body, serialized.len(), caller_headers);
    request_diagnostic.wire_model = Some(wire_model.clone());
    let sent = async {
        let mut request = request.body(serialized).build()?;
        if protocol == ProviderProtocol::AnthropicMessages && (native_compact || has_native_block) {
            let mut betas = request.headers().get_all("anthropic-beta").iter().filter_map(|v|v.to_str().ok())
                .flat_map(|v|v.split(',')).map(str::trim).filter(|v|!v.is_empty()).map(str::to_string).collect::<Vec<_>>();
            if !betas.iter().any(|v|v==crate::anthropic_compaction::BETA) {betas.push(crate::anthropic_compaction::BETA.into());}
            request.headers_mut().insert("anthropic-beta",HeaderValue::from_str(&betas.join(",")).expect("existing validated headers and beta constant"));
        }
        request_diagnostic.auth_binding = Some(diagnostics::resolved_auth_binding(&request));
        client.execute(request).await
    }.await;
    match sent {
        Ok(mut response) => {
            if let Some(proof) = thinking_provenance { response.extensions_mut().insert(proof); }
            if let Some(context) = native_context {response.extensions_mut().insert(context);}
            diagnostics::record_response_headers(&mut request_diagnostic, &response);
            response.extensions_mut().insert(request_diagnostic);
            response.extensions_mut().insert(SentImageState {
                omitted_before_send: translation_image_omissions
                    .saturating_add(wire_image_omissions),
                sent_images: crate::translate::count_rendered_sent_images(&upstream_body),
            });
            let status = response.status();
            crate::debug::log(&format!("send_candidate_once: -> {status}"));
            if !status.is_success() {
                let preview = response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("-");
                crate::debug::log_always(&format!(
                    "send_candidate_once error status={status} content_type={preview} content_length={:?}",
                    response.content_length()
                ));
            }
            if response
                .content_length()
                .is_some_and(|length| length > candidate.provider.limits.max_response_bytes)
            {
                Err(reject_oversized_provider_response(response, candidate.provider.limits.max_response_bytes, false).await)
            } else {
                Ok(response)
            }
        }
        Err(error) => Err(AttemptFailure {
            response: error_response(
                StatusCode::BAD_GATEWAY,
                "provider_unreachable",
                &format!("provider request failed: {error}"),
            ),
            kind: AttemptFailureKind::Retryable,
        }),
    }
}

fn chat_compatible_request(body: &Value, preserve_provider_metadata: bool) -> Value {
    let mut body = body.clone();
    if preserve_provider_metadata {
        return body;
    }
    if let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) {
        for item in items.iter_mut().filter_map(Value::as_object_mut) {
            item.remove("provider_metadata");
        }
    }
    body
}

pub(crate) async fn send_kiro_candidate(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
    wire_model: &str,
) -> Result<reqwest::Response, AttemptFailure> {
    validate_candidate_image_references(body, candidate)?;
    let source_image_count = count_translated_input_images(body);
    let credential_source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let mut credential_headers =
        resolve_provider_headers(&candidate.provider, candidate.credential.as_ref())
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_provider_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?;
    if credential_source == CredentialSource::Forward {
        let Some(caller_headers) = caller_headers else {
            return Err(AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    "the selected Kiro provider requires caller authentication headers",
                ),
                kind: AttemptFailureKind::Credential,
            });
        };
        let forwarded = collect_forward_headers(&state.settings, caller_headers)
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?;
        let authorization = forwarded
            .get(header::AUTHORIZATION)
            .cloned()
            .ok_or_else(|| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    "the selected Kiro provider requires a caller Authorization header",
                ),
                kind: AttemptFailureKind::Credential,
            })?;
        credential_headers.insert(header::AUTHORIZATION, authorization);
    }
    let kiro_api_key = credential_headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|value| value.starts_with("ksk_"));
    let profile_arn = (!kiro_api_key)
        .then(|| candidate.provider.kiro_profile_arn.as_deref())
        .flatten();
    let (mut payload, context) = crate::kiro::responses_to_kiro_checked(body, wire_model, profile_arn)
        .map_err(|error| match error {
            crate::kiro::KiroRequestError::ImageBudget => image_history_compaction_required(&error.to_string()),
            crate::kiro::KiroRequestError::Unsupported(message) => request_failure("unsupported_request",&message),
        })?;
    let mut wire_budget_omissions = 0_usize;
    {
        let protocol = candidate
            .provider
            .protocol_for_model(&candidate.upstream_model);
        let image_report = normalize_translated_image_history(
            &mut payload,
            protocol,
            candidate.provider.transport,
            candidate.provider.limits.max_request_bytes,
        )
        .await
        .map_err(image_history_failure)?;
        wire_budget_omissions = image_report.omitted_images;
        scrub_kiro_omitted_images(&mut payload);
        if image_report.omitted_images > 0 {
            crate::debug::log(&format!(
                "kiro wire image history normalized: provider={} model={} omitted={} total={} kept_base64_chars={}",
                candidate.provider.id,
                candidate.upstream_model,
                image_report.omitted_images,
                image_report.inline_images,
                image_report.kept_base64_chars,
            ));
        }
    }
    let translation_image_omissions =
        source_image_count.saturating_sub(count_translated_input_images(&payload));
    let _ = wire_budget_omissions;
    let serialized = crate::translate::serialize_preserving_image_history_async(
        &mut payload,
        candidate.provider.limits.max_request_bytes,
    ).await.map_err(image_history_failure)?;
    let wire_image_omissions = 0;

    let endpoint = format!(
        "{}/",
        candidate.provider.base_url.trim().trim_end_matches('/')
    );
    let dns_pinning = state.settings.read().await.security.dns_pinning;
    let client = if dns_pinning {
        pinned_client(&endpoint, &candidate.provider, "CODETAS-Gateway/0.1")
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "provider_dns_rejected",
                    &message,
                ),
                kind: AttemptFailureKind::Retryable,
            })?
    } else {
        state.client.clone()
    };
    let request = client
        .post(endpoint)
        .header(header::CONTENT_TYPE, "application/x-amz-json-1.0")
        .header(header::ACCEPT, "application/vnd.amazon.eventstream")
        .header(
            "x-amz-target",
            "AmazonCodeWhispererStreamingService.GenerateAssistantResponse",
        )
        .header("x-amzn-codewhisperer-optout", "true")
        .header("amz-sdk-request", "attempt=1; max=1")
        .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
        .header(
            header::USER_AGENT,
            "CODETAS-Gateway/0.1 (Kiro-compatible transport)",
        )
        .query(&candidate.provider.query_params)
        .timeout(Duration::from_millis(
            candidate.provider.limits.request_timeout_ms,
        ));
    let mut request = request;
    for (name, value) in &credential_headers {
        request = request.header(name, value);
    }
    if kiro_api_key {
        request = request.header("tokentype", "API_KEY");
    }
    if let Some(profile_arn) = profile_arn {
        request = request
            .header("x-amzn-kiro-profile-arn", profile_arn)
            .header("x-amzn-kiro-agent-mode", "vibe");
    }
    match diagnostics::send_with_wire_diagnostic(request, serialized, &payload, caller_headers).await {
        Ok(response)
            if response
                .content_length()
                .is_some_and(|length| length > candidate.provider.limits.max_response_bytes) =>
        {
            Err(reject_oversized_provider_response(response, candidate.provider.limits.max_response_bytes, true).await)
        }
        Ok(mut response) => {
            response.extensions_mut().insert(SentImageState {
                omitted_before_send: translation_image_omissions
                    .saturating_add(wire_image_omissions),
                sent_images: crate::translate::count_rendered_sent_images(&payload),
            });
            response.extensions_mut().insert(context);
            Ok(response)
        }
        Err(error) => Err(AttemptFailure {
            response: error_response(
                StatusCode::BAD_GATEWAY,
                "provider_unreachable",
                &format!("Kiro request failed: {error}"),
            ),
            kind: AttemptFailureKind::Retryable,
        }),
    }
}

pub(crate) async fn send_github_copilot_candidate(
    state: &GatewayState,
    body: &Value,
    candidate: &RouteCandidate,
    caller_headers: Option<&HeaderMap>,
    wire_model: &str,
) -> Result<reqwest::Response, AttemptFailure> {
    validate_candidate_image_references(body, candidate)?;
    let source_image_count = count_translated_input_images(body);
    let mut upstream_body = responses_to_chat_with_options(
        body,
        wire_model,
        model_requires_reasoning_placeholder(&candidate.provider, &candidate.upstream_model),
    )
    .map_err(|message| request_failure("unsupported_request", &message))?;
    apply_provider_wire_compatibility(
        &mut upstream_body,
        body,
        candidate,
        ProviderProtocol::ChatCompletions,
    )
    .map_err(|message| request_failure("unsupported_request", &message))?;
    upstream_body["model"] = Value::String(wire_model.to_string());
    let image_report = normalize_translated_image_history(
        &mut upstream_body,
        ProviderProtocol::ChatCompletions,
        candidate.provider.transport,
        candidate.provider.limits.max_request_bytes,
    )
    .await
    .map_err(image_history_failure)?;
    if image_report.omitted_images > 0 {
        crate::debug::log(&format!(
            "copilot wire image history normalized: provider={} model={} omitted={} total={}",
            candidate.provider.id,
            candidate.upstream_model,
            image_report.omitted_images,
            image_report.inline_images,
        ));
    }
    let translation_image_omissions =
        source_image_count.saturating_sub(count_translated_input_images(&upstream_body));
    let serialized = crate::translate::serialize_preserving_image_history_async(
        &mut upstream_body,
        candidate.provider.limits.max_request_bytes,
    ).await.map_err(image_history_failure)?;
    let wire_image_omissions = 0;

    let credential_source = candidate
        .credential
        .as_ref()
        .unwrap_or(&candidate.provider.credential)
        .source;
    let mut credential_headers =
        resolve_provider_headers(&candidate.provider, candidate.credential.as_ref())
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_provider_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?;
    if credential_source == CredentialSource::Forward {
        let Some(caller_headers) = caller_headers else {
            return Err(AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    "the selected GitHub Copilot provider requires caller authentication headers",
                ),
                kind: AttemptFailureKind::Credential,
            });
        };
        let forwarded = collect_forward_headers(&state.settings, caller_headers)
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    &message,
                ),
                kind: AttemptFailureKind::Credential,
            })?;
        let authorization = forwarded
            .get(header::AUTHORIZATION)
            .cloned()
            .ok_or_else(|| AttemptFailure {
                response: error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing_forward_credential",
                    "the selected GitHub Copilot provider requires a caller Authorization header",
                ),
                kind: AttemptFailureKind::Credential,
            })?;
        credential_headers.insert(header::AUTHORIZATION, authorization);
    }
    let github_authorization = credential_headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AttemptFailure {
            response: error_response(
                StatusCode::UNAUTHORIZED,
                "missing_provider_credential",
                "GitHub Copilot requires a GitHub token via a bearer credential",
            ),
            kind: AttemptFailureKind::Credential,
        })?;
    let token_endpoint = "https://api.github.com/copilot_internal/v2/token";
    let binding_request = state.client.get(token_endpoint).headers(credential_headers.clone()).build()
        .map_err(|_| request_failure("invalid_request", "cannot build Copilot authentication binding"))?;
    let auth_binding = diagnostics::resolved_auth_binding(&binding_request);
    let dns_pinning = state.settings.read().await.security.dns_pinning;
    let exchange_client = if dns_pinning {
        pinned_client(token_endpoint, &candidate.provider, "CODETAS/0.1")
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "provider_dns_rejected",
                    &message,
                ),
                kind: AttemptFailureKind::Retryable,
            })?
    } else {
        state.client.clone()
    };
    let session = exchange_copilot_token(
        &exchange_client,
        github_authorization,
        Duration::from_millis(candidate.provider.limits.request_timeout_ms),
    )
    .await
    .map_err(|message| AttemptFailure {
        response: error_response(
            StatusCode::UNAUTHORIZED,
            "copilot_token_exchange_failed",
            &message,
        ),
        kind: AttemptFailureKind::Credential,
    })?;
    let endpoint = format!(
        "{}/chat/completions",
        session.api_base.trim_end_matches('/')
    );
    let client = if dns_pinning {
        pinned_client(&endpoint, &candidate.provider, "CODETAS/0.1")
            .await
            .map_err(|message| AttemptFailure {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    "provider_dns_rejected",
                    &message,
                ),
                kind: AttemptFailureKind::Retryable,
            })?
    } else {
        state.client.clone()
    };
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut request = client
        .post(endpoint)
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::ACCEPT,
            if streaming {
                "text/event-stream"
            } else {
                "application/json"
            },
        )
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", session.token.as_str()),
        )
        .header("editor-version", "CODETAS/0.1")
        .header("editor-plugin-version", "CODETAS/0.1")
        .header("copilot-integration-id", "vscode-chat")
        .header("openai-intent", "conversation-panel")
        .header(header::USER_AGENT, "CODETAS/0.1")
        .query(&candidate.provider.query_params)
        .timeout(Duration::from_millis(
            candidate.provider.limits.request_timeout_ms,
        ));
    for (name, value) in &credential_headers {
        if name != header::AUTHORIZATION {
            request = request.header(name, value);
        }
    }
    match diagnostics::send_with_wire_diagnostic(request, serialized, &upstream_body, caller_headers).await {
        Ok(response)
            if response
                .content_length()
                .is_some_and(|length| length > candidate.provider.limits.max_response_bytes) =>
        {
            Err(reject_oversized_provider_response(response, candidate.provider.limits.max_response_bytes, false).await)
        }
        Ok(mut response) => {
            if let Some(diagnostic) = response.extensions_mut().get_mut::<crate::observability::RequestDiagnostic>() {
                diagnostic.auth_binding = Some(auth_binding);
            }
            response.extensions_mut().insert(SentImageState {
                omitted_before_send: translation_image_omissions
                    .saturating_add(wire_image_omissions),
                sent_images: crate::translate::count_rendered_sent_images(&upstream_body),
            });
            Ok(response)
        }
        Err(error) => Err(AttemptFailure {
            response: error_response(
                StatusCode::BAD_GATEWAY,
                "provider_unreachable",
                &format!("GitHub Copilot request failed: {error}"),
            ),
            kind: AttemptFailureKind::Retryable,
        }),
    }
}

pub(crate) fn google_thinking_level(effort: &str) -> Option<&'static str> {
    match effort {
        "minimal" => Some("minimal"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" | "xhigh" | "max" | "ultra" => Some("high"),
        _ => None,
    }
}

pub(crate) fn wire_model_for_request(candidate: &RouteCandidate, request: &Value) -> String {
    let model = candidate.upstream_model.as_str();
    if candidate.provider.google_mode != GoogleMode::CloudCodeAssist {
        return candidate.provider.wire_model_id(model);
    }
    let effort = request.pointer("/reasoning/effort").and_then(Value::as_str);
    match (model, effort) {
        ("claude-opus-5-5" | "claude-sonnet-5-5", Some("low")) => format!("{model}-low"),
        ("claude-opus-5-5" | "claude-sonnet-5-5", Some("high" | "xhigh" | "max" | "ultra")) => {
            format!("{model}-high")
        }
        ("claude-opus-5-5" | "claude-sonnet-5-5", _) => format!("{model}-medium"),
        ("gemini-4-argon" | "gemini-3.8-flash" | "gemini-3.7-flash", _) => {
            format!("{model}-tiered")
        }
        ("gemini-3.6-flash" | "gemini-3.5-flash", Some("low")) => format!("{model}-low"),
        ("gemini-3.6-flash" | "gemini-3.5-flash", Some("high" | "xhigh" | "max" | "ultra")) => {
            format!("{model}-high")
        }
        ("gemini-3.6-flash" | "gemini-3.5-flash", _) => format!("{model}-medium"),
        ("gemini-3.1-pro", Some("low")) => "gemini-3.1-pro-low".into(),
        ("gemini-3.1-pro", _) => "gemini-pro-agent".into(),
        _ => candidate.provider.wire_model_id(model),
    }
}

pub(crate) fn cloud_code_assist_envelope(
    mut request: Value,
    source_request: &Value,
    candidate: &RouteCandidate,
    wire_model: &str,
    project_override: Option<&str>,
    request_id: Option<&str>,
) -> Result<Value, AttemptFailure> {
    let project = candidate
        .provider
        .project
        .as_deref()
        .or(project_override)
        .ok_or_else(|| {
            request_failure(
                "provider_configuration_error",
                "Cloud Code Assist requires provider.project or an Antigravity OAuth session",
            )
        })?;
    let session_source = source_request
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .unwrap_or_else(|| candidate.exposed_model.as_str());
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in session_source.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    if let Some(object) = request.as_object_mut() {
        object.insert("sessionId".into(), Value::String(format!("-{hash}")));
    }
    Ok(json!({
        "model": wire_model,
        "userAgent": "antigravity",
        "requestType": "agent",
        "project": project,
        "requestId": request_id
            .map(str::to_string)
            .unwrap_or_else(|| format!("agent-{}", Uuid::new_v4())),
        "request": request,
    }))
}

#[cfg(test)]
mod image_retry_tests {
    use super::*;

    #[test]
    fn image_rejection_classifier_ignores_non_image_invalid_requests() {
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"The image is too large","type":"invalid_request_error"}}"#,
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"invalid image_url parameter"}}"#,
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            r#"{"error":{"message":"图片尺寸超限"}}"#.as_bytes(),
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"temperature must be between 0 and 2","type":"invalid_request_error"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"Unknown field `foo`","type":"invalid_request_error"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"unsupported mime type for audio","type":"invalid_request_error"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"Unsupported media type","type":"invalid_request_error"}}"#,
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(b"", None));
        assert!(provider_error_looks_like_image_rejection(b"   ", None));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"type":"invalid_request_error"}}"#,
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"type":"invalid_request_error"},"request_id":"abc"}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"revision required","type":"invalid_request_error"}}"#,
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"too many images"}}"#,
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"image_too_large","message":"invalid request"}}"#,
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"invalid_image"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"Unknown field in image_generation","type":"invalid_request_error"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"imagegen argument referenced_image_paths must be an array"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"referenced_image_paths"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"imagenet_model"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"preview_image_cache"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"account_image"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"image_cache"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"Unknown field `image_format`"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"image_size"}}"#,
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"max_image_size"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"image_url_cache"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(b"{}", None,));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"image_parse_error"}}"#,
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"too_many_images"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"cannot fetch https://example.test/cat.jpg"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"cannot fetch https://example.test/image.png"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"type":"https://example.test/errors/vision-unavailable"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"url":"https://example.test/image.png"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"foo":1}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"file image.png missing"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            b" \xef\xbb\xbf<html><title>Bad Request</title><p>image too large</p></html>",
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"invalid_image_url","message":"invalid request"}}"#,
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"unsupported image/png"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"invalid request","type":"invalid_request_error"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"computer_vision_unavailable","message":"model offline"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"computer_vision","message":"model offline"}}"#,
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"vision input is not supported"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"image_generation_failed","message":"quota exceeded"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"image_edit_failed"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"failed_image_generation"}}"#,
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"max_pixels exceeded"}}"#,
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"image_generation ok but image_url is invalid"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"unknown docker_image field","type":"invalid_request_error"}}"#,
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"invalid_image_format"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"invalid_imagegen"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            b"temperature must be between 0 and 2",
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            b"image too large",
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            b"<html><title>Bad Request</title></html>",
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"type":"invalid_request_error","request_id":"abc"},"id":"req-1"}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"["temperature must be between 0 and 2"]"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#""temperature must be between 0 and 2""#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"type":"insufficient_quota"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"type":"authentication_error"}}"#,
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"type":"invalid_image_url"}}"#,
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"image_parse_error"}}"#,
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"too_many_images"}}"#,
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"max_image_size"}}"#,
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            r#"{"error":{"message":"像素超限"}}"#.as_bytes(),
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"ok":false}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"success":false,"reason":"nope"}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"id":"req-1"}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"object":"error","created":1}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"errmsg":"temperature must be between 0 and 2"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"data":{"info":"unknown field foo"}}}"#,
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            br#"{"error":{"errmsg":"too many images"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"fetch failed","url":"https://example.test/image.png"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"crash","stack":"at image.rs:1"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"message":"unknown docker_image_layer field"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"non_image_field"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            br#"{"error":{"code":"not_image_related"}}"#,
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            b"<html><title>Bad Request</title><p>image too large</p></html>",
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            b"<!-- gateway --><html><title>Bad Request</title><p>image too large</p></html>",
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            b"<!-- a --><!-- b --><html><title>Bad Request</title><p>image too large</p></html>",
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            b"<!-- unclosed comment image too large",
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            b"<body><p>image too large</p></body>",
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            b"<!-- image too large -->",
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            b"<?xml version=\"1.0\"?><error>image too large</error>",
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            b"\xef\xbb\xbf<html><title>Bad Request</title><p>image too large</p></html>",
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            r#"{"error":{"message":"画像サイズ超過"}}"#.as_bytes(),
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            r#"{"error":{"message":"画像生成に失敗しました"}}"#.as_bytes(),
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            r#"{"error":{"message":"图像生成失败"}}"#.as_bytes(),
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            r#"{"error":{"message":"関連画像はありません"}}"#.as_bytes(),
            None,
        ));
        assert!(!provider_error_looks_like_image_rejection(
            r#"{"error":{"message":"大きな画像もあります"}}"#.as_bytes(),
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            r#"{"error":{"message":"画像が大きすぎます"}}"#.as_bytes(),
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            r#"{"error":{"message":"生成的图片尺寸过大"}}"#.as_bytes(),
            None,
        ));
        assert!(provider_error_looks_like_image_rejection(
            r#"{"error":{"message":"图片尺寸超限"}}"#.as_bytes(),
            None,
        ));
    }

    #[test]
    fn cloud_code_assist_sse_envelope_is_terminal() {
        let value = json!({
            "response": {
                "candidates": [{"content": {"parts": [{"text": "pong"}]}, "finishReason": "STOP"}]
            }
        });
        assert!(provider_stream_event_is_terminal(
            crate::config::ProviderProtocol::GeminiGenerateContent,
            &value
        ));
        assert!(!provider_stream_event_is_terminal(
            crate::config::ProviderProtocol::GeminiGenerateContent,
            &json!({"response": {"candidates": [{"content": {"parts": [{"text": "po"}]}}]}})
        ));
    }

    #[test]
    fn empty_completion_detection_has_positive_and_negative_cases() {
        assert!(completion_is_empty(&json!({"choices": []})));
        assert!(completion_is_empty(&json!({"output": []})));
        assert!(completion_is_empty(&json!({
            "status": "completed",
            "output": [{"type": "reasoning", "encrypted_content": "signature-only"}]
        })));
        assert!(completion_is_empty(&json!({
            "content": [{"type": "thinking", "thinking": "private"}],
            "stop_reason": "end_turn"
        })));
        assert!(completion_is_empty(&json!({
            "candidates": [{"content": {"parts": [{"thought": true, "text": "private"}]},
                "finishReason": "STOP"}]
        })));
        assert!(completion_is_empty(&json!({
            "choices": [{"message": {"content": "Still working…"}}]
        })));
        assert!(completion_is_empty(&json!({
            "status": "completed",
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "Still working…"}]
            }]
        })));
        assert!(completion_is_empty(&json!({
            "status": "completed",
            "output": [{"type": "custom_tool_call", "name": "apply_patch", "input": "", "status": "incomplete"}]
        })));
        assert!(completion_is_empty(&json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "empty_response"},
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "Still working…"}]
            }]
        })));
        assert!(completion_is_empty(&json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "invalid_tool_call"},
            "output": []
        })));
        assert!(!completion_is_empty(&json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "partial"}]}]
        })));
        assert!(!completion_is_empty(&json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "invalid_tool_call"},
            "output": [{"type": "function_call", "name": "lookup", "arguments": "{}"}]
        })));
        assert!(completion_is_empty(&json!({
            "choices": [{"finish_reason": "tool_calls", "message": {
                "content": "Still working…",
                "tool_calls": [{
                    "id": "call_empty",
                    "function": {"name": "apply_patch", "arguments": ""}
                }]
            }}]
        })));
        assert!(!completion_is_empty(&json!({
            "choices": [{"finish_reason": "tool_calls", "message": {
                "tool_calls": [{
                    "id": "call_lookup",
                    "function": {"name": "lookup", "arguments": "{}"}
                }]
            }}]
        })));
        assert!(!completion_is_empty(
            &json!({"choices": [{"message": {"content": "ok"}}]})
        ));
        assert!(!completion_is_empty(&json!({
            "status": "completed", "output": [{"type": "function_call", "name": "lookup"}]
        })));
        assert!(!completion_is_empty(&json!({
            "content": [], "stop_reason": "max_tokens"
        })));
        assert!(!completion_is_empty(&json!({
            "candidates": [{"content": {"parts": []}, "finishReason": "MAX_TOKENS"}]
        })));
        for reason in ["OTHER", "MALFORMED_FUNCTION_CALL", "UNEXPECTED_TOOL_CALL"] {
            assert!(!completion_is_empty(&json!({
                "candidates": [{"content": {"parts": []}, "finishReason": reason}]
            })));
        }
        assert!(!completion_is_empty(
            &json!({"error": {"message": "failed"}})
        ));
    }

    #[test]
    fn antigravity_wire_models_use_agy_variant_ids() {
        let provider = crate::config::ProviderDefinition {
            id: "google-antigravity".into(),
            google_mode: GoogleMode::CloudCodeAssist,
            ..crate::config::ProviderDefinition::default()
        };
        let candidate = RouteCandidate {
            capabilities: provider.capabilities.clone(),
            provider,
            upstream_model: "gemini-3.7-flash".into(),
            exposed_model: "google-antigravity/gemini-3.7-flash".into(),
            credential: None,
            account_id: None,
            target_key: "google-antigravity/gemini-3.7-flash".into(),
            route_id: None,
            failure_threshold: 1,
            quota_threshold_percent: 0,
            input_price_per_million: None,
            output_price_per_million: None,
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            routing_epoch: 0,
            routing_generation: 0,
            session_scope: None,
        };
        assert_eq!(
            wire_model_for_request(&candidate, &json!({})),
            "gemini-3.7-flash-tiered"
        );
        assert_eq!(
            wire_model_for_request(&candidate, &json!({"reasoning": {"effort": "high"}})),
            "gemini-3.7-flash-tiered"
        );
        for model in ["claude-opus-5-5", "claude-sonnet-5-5"] {
            let mut claude = candidate.clone();
            claude.upstream_model = model.into();
            for effort in ["low", "medium", "high"] {
                assert_eq!(
                    wire_model_for_request(&claude, &json!({"reasoning": {"effort": effort}})),
                    format!("{model}-{effort}")
                );
            }
            assert_eq!(
                wire_model_for_request(&claude, &json!({})),
                format!("{model}-medium")
            );
            claude.upstream_model = format!("{model}-low");
            assert_eq!(
                wire_model_for_request(&claude, &json!({})),
                format!("{model}-low")
            );
        }
        let mut flash38 = candidate.clone();
        flash38.upstream_model = "gemini-3.8-flash".into();
        assert_eq!(
            wire_model_for_request(&flash38, &json!({})),
            "gemini-3.8-flash-tiered"
        );
        let mut argon = candidate.clone();
        argon.upstream_model = "gemini-4-argon".into();
        assert_eq!(
            wire_model_for_request(&argon, &json!({})),
            "gemini-4-argon-tiered"
        );
        let mut flash = candidate.clone();
        flash.upstream_model = "gemini-3.6-flash".into();
        assert_eq!(
            wire_model_for_request(&flash, &json!({})),
            "gemini-3.6-flash-medium"
        );
        let mut pro = candidate.clone();
        pro.upstream_model = "gemini-3.1-pro".into();
        assert_eq!(wire_model_for_request(&pro, &json!({})), "gemini-pro-agent");
    }

    #[test]
    fn cloud_code_assist_retry_reuses_the_wire_request_id() {
        let provider = crate::config::ProviderDefinition {
            id: "cloud-code".into(),
            project: Some("project-one".into()),
            google_mode: GoogleMode::CloudCodeAssist,
            ..crate::config::ProviderDefinition::default()
        };
        let candidate = RouteCandidate {
            capabilities: provider.capabilities.clone(),
            provider,
            upstream_model: "gemini-3.1-pro".into(),
            exposed_model: "gemini-3.1-pro".into(),
            credential: None,
            account_id: None,
            target_key: "cloud-code/gemini-3.1-pro".into(),
            route_id: None,
            failure_threshold: 1,
            quota_threshold_percent: 0,
            input_price_per_million: None,
            output_price_per_million: None,
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            routing_epoch: 0,
            routing_generation: 0,
            session_scope: None,
        };
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let source = json!({"prompt_cache_key": "session-one"});
        let first = cloud_code_assist_envelope(
            request.clone(),
            &source,
            &candidate,
            "gemini-pro-agent",
            None,
            Some("agent-stable"),
        )
        .expect("first envelope");
        let retry = cloud_code_assist_envelope(
            request,
            &source,
            &candidate,
            "gemini-pro-agent",
            None,
            Some("agent-stable"),
        )
        .expect("retry envelope");
        assert_eq!(first, retry);
        assert_eq!(first["requestId"], "agent-stable");
    }

    #[test]
    fn nonstream_empty_retry_failure_carries_usage_and_dedicated_marker() {
        let usage = TokenUsage {
            input_tokens: 13,
            output_tokens: 2,
            total_tokens: 15,
            ..TokenUsage::default()
        };
        let response = empty_completion_retry_failed_response(usage.clone(), 2)
            .expect("dedicated failure response");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let marker = response
            .extensions()
            .get::<EmptyCompletionRecoveryFailure>()
            .expect("recovery failure marker");
        assert_eq!(marker.usage.total_tokens, usage.total_tokens);
        assert_eq!(marker.additional_sends, 2);
        let retry = response
            .extensions()
            .get::<ProviderRetryObservation>()
            .expect("provider retry observation");
        assert_eq!(retry.additional_sends, 2);
        assert_eq!(
            retry.recovery_kinds,
            ["empty-completion", "empty-completion"]
        );
        // Failure marker/known billing owns the aggregate; retry usage is not additive here.
        assert_eq!(retry.usage.total_tokens, 0);
    }

    #[test]
    fn streaming_guard_treats_reasoning_signatures_and_redaction_as_precontent() {
        for event in [
            json!({"type": "response.reasoning_text.delta", "delta": "private"}),
            json!({"type": "response.reasoning_signature.delta", "delta": "signature"}),
            json!({"type": "response.output_item.added", "item": {
                "type": "reasoning", "encrypted_content": "redacted"}}),
        ] {
            assert!(!provider_stream_event_has_visible_content(
                ProviderProtocol::Responses,
                &event
            ));
            assert!(provider_stream_event_is_reasoning(
                ProviderProtocol::Responses,
                &event
            ));
        }
        assert!(provider_stream_event_has_visible_content(
            ProviderProtocol::Responses,
            &json!({"type": "response.output_text.delta", "delta": "answer"})
        ));
        assert!(provider_stream_event_has_visible_content(
            ProviderProtocol::Responses,
            &json!({"type": "response.output_item.added", "item": {
                "type": "function_call", "name": "exec"}})
        ));
        assert!(provider_stream_event_has_visible_content(
            ProviderProtocol::Responses,
            &json!({"type": "response.completed", "response": {
                "status": "completed",
                "output": [{"type": "message", "content": [{
                    "type": "output_text", "text": "terminal-only answer"
                }]}]
            }})
        ));

        let anthropic_redacted = json!({
            "type": "content_block_start",
            "content_block": {"type": "redacted_thinking", "data": "opaque"}
        });
        assert!(provider_stream_event_is_reasoning(
            ProviderProtocol::AnthropicMessages,
            &anthropic_redacted
        ));
        assert!(!provider_stream_event_has_visible_content(
            ProviderProtocol::AnthropicMessages,
            &anthropic_redacted
        ));
        assert!(provider_stream_event_is_empty_terminal(
            ProviderProtocol::AnthropicMessages,
            &json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}})
        ));

        let gemini_signature = json!({
            "candidates": [{"content": {"parts": [{
                "thought": true, "thoughtSignature": "opaque", "text": "private"
            }]}}]
        });
        assert!(provider_stream_event_is_reasoning(
            ProviderProtocol::GeminiGenerateContent,
            &gemini_signature
        ));
        assert!(!provider_stream_event_has_visible_content(
            ProviderProtocol::GeminiGenerateContent,
            &gemini_signature
        ));

        let first_reasoning = json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "id": "rs_first_attempt",
                "type": "reasoning",
                "status": "completed",
                "encrypted_content": "opaque-first"
            }
        });
        let mut history = responses_reasoning_semantic_history(&first_reasoning);
        assert_eq!(history.len(), 1);
        assert!(history[0].get("id").is_none());
        history[0]["id"] = json!("rs_gateway");
        history[0]["status"] = json!("completed");
        let mut retry_message = json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "id": "msg_retry",
                "type": "message",
                "status": "completed",
                "content": [{"type": "output_text", "text": "answer"}]
            }
        });
        shift_responses_retry_event(&mut retry_message, history.len(), &history);
        let lifecycle = canonical_reasoning_lifecycle_events(&history);
        assert_eq!(lifecycle[0]["output_index"], 0);
        assert_eq!(lifecycle[1]["item"]["id"], "rs_gateway");
        assert_eq!(retry_message["output_index"], 1);
        assert_eq!(
            retry_message.pointer("/item/id").and_then(Value::as_str),
            Some("msg_retry")
        );
    }

    #[test]
    fn responses_multi_retry_reasoning_keeps_every_attempt_in_order() {
        let mut history = Vec::new();
        for (index, text) in ["first", "second", "third"].into_iter().enumerate() {
            let event = json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": format!("provider_reasoning_{index}"),
                    "type": "reasoning",
                    "status": "completed",
                    "summary": [{"type": "summary_text", "text": text}],
                    "encrypted_content": format!("signature-{index}")
                }
            });
            let mut semantic = responses_reasoning_semantic_history(&event);
            semantic[0]["id"] = json!(format!("rs_gateway_{index}"));
            semantic[0]["status"] = json!("completed");
            history.append(&mut semantic);
        }
        let mut retry_message = json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "sequence_number": 0,
            "item": {"id": "msg_final", "type": "message", "status": "completed"}
        });
        shift_responses_retry_event(&mut retry_message, history.len(), &history);
        let mut sequence = 0;
        let mut lifecycle = canonical_reasoning_lifecycle_events(&history);
        for event in &mut lifecycle {
            assign_responses_sequence(event, &mut sequence);
        }
        assign_responses_sequence(&mut retry_message, &mut sequence);

        assert_eq!(lifecycle.len(), 6);
        assert_eq!(lifecycle[0]["item"]["summary"][0]["text"], "first");
        assert_eq!(lifecycle[2]["item"]["summary"][0]["text"], "second");
        assert_eq!(lifecycle[4]["item"]["summary"][0]["text"], "third");
        assert!(lifecycle.iter().all(|event| !event["item"]["id"]
            .as_str()
            .unwrap_or_default()
            .starts_with("provider_")));
        assert_eq!(retry_message["output_index"], 3);
        assert_eq!(retry_message["sequence_number"], 6);
    }

    #[test]
    fn streaming_guard_retries_only_silent_completed_terminals() {
        let empty = json!({
            "type": "response.completed",
            "response": {"status": "completed", "output": [{"type": "reasoning"}]}
        });
        assert!(provider_stream_event_is_empty_terminal(
            ProviderProtocol::Responses,
            &empty
        ));
        for terminal in [
            json!({"type": "response.incomplete", "response": {
                "status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}}}),
            json!({"type": "response.incomplete", "response": {
                "status": "incomplete", "incomplete_details": {"reason": "content_filter"}}}),
            json!({"type": "response.failed", "response": {"status": "failed"}}),
        ] {
            assert!(provider_stream_event_is_visible_failure(
                ProviderProtocol::Responses,
                &terminal
            ));
            assert!(!provider_stream_event_is_empty_terminal(
                ProviderProtocol::Responses,
                &terminal
            ));
        }
        let failed = empty_completion_failed_event_for_protocol(
            ProviderProtocol::Responses,
            TokenUsage::default(),
            Some("resp_retry"),
        );
        assert_eq!(
            failed
                .pointer("/response/error/code")
                .and_then(Value::as_str),
            Some(EMPTY_COMPLETION_RETRY_FAILED_CODE)
        );
        assert_eq!(
            failed.pointer("/response/id").and_then(Value::as_str),
            Some("resp_retry")
        );

        let usage = sum_token_usage(
            TokenUsage {
                input_tokens: 3,
                output_tokens: 0,
                total_tokens: 3,
                ..TokenUsage::default()
            },
            TokenUsage {
                input_tokens: 5,
                output_tokens: 2,
                total_tokens: 7,
                ..TokenUsage::default()
            },
        );
        assert_eq!(usage.input_tokens, 8);
        assert_eq!(usage.output_tokens, 2);
        assert_eq!(usage.total_tokens, 10);

        let mut gemini_usage = json!({"candidates": []});
        write_token_usage(
            &mut gemini_usage,
            TokenUsage {
                input_tokens: 10,
                output_tokens: 9,
                reasoning_tokens: 4,
                total_tokens: 19,
                ..TokenUsage::default()
            },
        );
        assert_eq!(gemini_usage["usageMetadata"]["candidatesTokenCount"], 5);
        assert_eq!(gemini_usage["usageMetadata"]["thoughtsTokenCount"], 4);

        assert!(provider_stream_event_is_error(
            ProviderProtocol::Responses,
            &json!({"type": "response.failed", "response": {
                "status": "failed", "error": {"message": "upstream failed"}
            }})
        ));
        assert!(!provider_stream_event_is_error(
            ProviderProtocol::Responses,
            &json!({"type": "response.incomplete", "response": {
                "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"}
            }})
        ));
        let empty_patch = json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": "call_empty",
                "function": {"name": "apply_patch", "arguments": ""}
            }]}, "finish_reason": "tool_calls"}]
        });
        assert!(!chat_value_has_content(&empty_patch));
        assert!(provider_stream_event_is_empty_terminal(
            ProviderProtocol::ChatCompletions,
            &empty_patch
        ));
        let empty_gemini = json!({
            "candidates": [{"content": {"parts": [{
                "functionCall": {"name": "apply_patch", "args": {}}
            }]}, "finishReason": "STOP"}]
        });
        assert!(!gemini_value_has_content(&empty_gemini));
        assert!(provider_stream_event_is_empty_terminal(
            ProviderProtocol::GeminiGenerateContent,
            &empty_gemini
        ));
        let empty_anthropic_start = json!({
            "type": "content_block_start",
            "content_block": {"type": "tool_use", "name": "apply_patch", "input": {}}
        });
        assert!(!provider_stream_event_has_visible_content(
            ProviderProtocol::AnthropicMessages,
            &empty_anthropic_start
        ));
        let complete_patch = "*** Begin Patch\n*** Add File: docs/a.html\n+ok\n*** End Patch\n";
        let complete_patch_event = json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": "call_patch",
                "function": {"name": "apply_patch", "arguments": complete_patch}
            }]}, "finish_reason": "tool_calls"}]
        });
        assert!(chat_value_has_content(&complete_patch_event));
        assert!(!provider_stream_event_is_empty_terminal(
            ProviderProtocol::ChatCompletions,
            &complete_patch_event
        ));
        assert!(provider_stream_event_is_empty_terminal(
            ProviderProtocol::Responses,
            &json!({"type": "response.incomplete", "response": {
                "status": "incomplete",
                "incomplete_details": {"reason": "invalid_tool_call"},
                "output": []
            }})
        ));

        for failed_chat in [
            json!({"choices": [{"delta": {}, "finish_reason": "error"}]}),
            json!({"choices": [{"delta": {}, "error": {"message": "failed"}}]}),
        ] {
            assert!(provider_stream_event_is_visible_failure(
                ProviderProtocol::ChatCompletions,
                &failed_chat,
            ));
            assert!(provider_stream_event_is_error(
                ProviderProtocol::ChatCompletions,
                &failed_chat,
            ));
            assert!(!provider_stream_event_is_empty_terminal(
                ProviderProtocol::ChatCompletions,
                &failed_chat,
            ));
        }

        let mut anthropic_start = json!({
            "type": "message_start",
            "message": {"usage": {"input_tokens": 5}}
        });
        write_anthropic_stream_start_usage(&mut anthropic_start, usage.clone());
        assert_eq!(
            anthropic_start
                .pointer("/message/usage/input_tokens")
                .and_then(Value::as_u64),
            Some(8)
        );
    }

    #[test]
    fn chat_metadata_is_forwarded_only_when_provider_opts_in() {
        let body = json!({"input": [{"type": "reasoning", "provider_metadata": {"vendor": {"signature": "opaque"}}}]});
        assert!(chat_compatible_request(&body, false)
            .pointer("/input/0/provider_metadata")
            .is_none());
        assert!(chat_compatible_request(&body, true)
            .pointer("/input/0/provider_metadata")
            .is_some());
    }
}

#[cfg(test)]
mod provider_pacing_tests {
    use super::*;

    #[test]
    fn concurrent_reservations_for_one_provider_are_spaced() {
        let mut pacing = ProviderPacingState::default();
        let now = Instant::now();
        let interval = Duration::from_millis(250);
        let first = reserve_provider_start(&mut pacing, "provider", interval, now)
            .expect("first pacing reservation")
            .scheduled;
        let second = reserve_provider_start(&mut pacing, "provider", interval, now)
            .expect("second pacing reservation")
            .scheduled;
        assert_eq!(first, now);
        assert!(second.duration_since(first) >= interval);
    }

    #[test]
    fn disabled_pacing_and_different_providers_do_not_share_a_queue() {
        let mut pacing = ProviderPacingState::default();
        let now = Instant::now();
        assert_eq!(
            reserve_provider_start(&mut pacing, "off", Duration::ZERO, now)
                .expect("disabled pacing")
                .scheduled,
            now
        );
        assert!(pacing.queues.is_empty());
        assert_eq!(
            reserve_provider_start(&mut pacing, "one", Duration::from_secs(1), now)
                .expect("provider one")
                .scheduled,
            now
        );
        assert_eq!(
            reserve_provider_start(&mut pacing, "two", Duration::from_secs(1), now)
                .expect("provider two")
                .scheduled,
            now
        );
    }

    #[test]
    fn pacing_state_fails_closed_at_key_capacity_and_reclaims_expired_keys() {
        let mut pacing = ProviderPacingState::default();
        let now = Instant::now();
        for index in 0..MAX_PROVIDER_PACING_KEYS {
            let slot = reserve_provider_start(
                &mut pacing,
                &format!("provider-{index}"),
                Duration::from_secs(60),
                now,
            )
            .expect("bounded provider reservation");
            assert_eq!(slot.scheduled, now);
        }
        assert!(matches!(
            reserve_provider_start(&mut pacing, "overflow", Duration::from_secs(60), now,),
            Err(ProviderPacingError::Overloaded(_))
        ));
        for queue in pacing.queues.values_mut() {
            queue.waiters.clear();
        }
        let fresh = reserve_provider_start(
            &mut pacing,
            "fresh",
            Duration::from_millis(1),
            now + Duration::from_secs(61),
        )
        .expect("expired queues are reclaimed");
        assert_eq!(fresh.scheduled, now + Duration::from_secs(61));
        assert_eq!(pacing.queues.len(), 1);
        assert!(pacing.queues.contains_key("fresh"));
    }

    #[test]
    fn pacing_key_capacity_retry_after_covers_all_waiters_and_final_retention() {
        let mut pacing = ProviderPacingState::default();
        let now = Instant::now();
        let interval = Duration::from_secs(2);
        for index in 0..MAX_PROVIDER_PACING_KEYS {
            let queue = pacing
                .queues
                .entry(format!("provider-{index}"))
                .or_default();
            queue.last_started = Some(now - Duration::from_secs(1));
            queue.waiters.push_back(ProviderPacingWaiter {
                ticket: 1,
                queued_at: now,
                scheduled: now + Duration::from_secs(1),
                interval,
            });
            queue.waiters.push_back(ProviderPacingWaiter {
                ticket: 2,
                queued_at: now,
                scheduled: now + Duration::from_secs(3),
                interval,
            });
        }

        let retry_after =
            match reserve_provider_start(&mut pacing, "overflow", Duration::from_millis(1), now) {
                Err(ProviderPacingError::Overloaded(delay)) => delay,
                _ => panic!("expected key-capacity overload"),
            };
        assert_eq!(retry_after, Duration::from_secs(63));
        let failure = provider_pacing_failure(ProviderPacingError::Overloaded(retry_after));
        assert_eq!(
            failure
                .response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("63")
        );

        let reclaim_at = now + retry_after;
        for queue in pacing.queues.values_mut() {
            queue.waiters.clear();
            queue.last_started = Some(reclaim_at - MAX_PROVIDER_PACING_WAIT);
        }
        let admitted = reserve_provider_start(
            &mut pacing,
            "overflow",
            Duration::from_millis(1),
            reclaim_at,
        )
        .expect("advertised Retry-After permits a new pacing key");
        assert_eq!(admitted.scheduled, reclaim_at);
    }

    #[test]
    fn pacing_capacity_expiry_deadline_wakes_and_removes_waiters_as_advertised() {
        let mut pacing = ProviderPacingState::default();
        let now = Instant::now();
        let queued_at = now;
        let expires_at = queued_at + MAX_PROVIDER_PACING_WAIT;
        for index in 0..MAX_PROVIDER_PACING_KEYS {
            pacing
                .queues
                .entry(format!("provider-{index}"))
                .or_default()
                .waiters
                .push_back(ProviderPacingWaiter {
                    ticket: 1,
                    queued_at,
                    scheduled: now + Duration::from_secs(90),
                    interval: Duration::from_secs(30),
                });
        }

        let retry_after =
            match reserve_provider_start(&mut pacing, "overflow", Duration::from_millis(1), now) {
                Err(ProviderPacingError::Overloaded(delay)) => delay,
                _ => panic!("expected key-capacity overload"),
            };
        assert_eq!(retry_after, MAX_PROVIDER_PACING_WAIT);
        let first = pacing.queues.get("provider-0").expect("first queue");
        assert_eq!(
            provider_pacing_waiter_wake_at(first.waiters.front().expect("waiter")),
            expires_at
        );

        let keys = pacing.queues.keys().cloned().collect::<Vec<_>>();
        let generation = pacing.generation;
        for key in keys {
            remove_provider_pacing_ticket_at(&mut pacing, &key, 1, generation, false, expires_at);
        }
        assert!(pacing.queues.is_empty());
        let admitted = reserve_provider_start(
            &mut pacing,
            "overflow",
            Duration::from_millis(1),
            expires_at,
        )
        .expect("capacity is available at the advertised expiry deadline");
        assert_eq!(admitted.scheduled, expires_at);
    }

    #[test]
    fn pacing_queue_rejects_excessive_depth_or_age() {
        let mut pacing = ProviderPacingState::default();
        let now = Instant::now();
        for _ in 0..=MAX_PROVIDER_PACING_QUEUE_DEPTH {
            let result =
                reserve_provider_start(&mut pacing, "provider", Duration::from_secs(1), now);
            if matches!(result, Err(ProviderPacingError::Overloaded(_))) {
                return;
            }
        }
        panic!("pacing queue did not reject an excessive burst");
    }

    #[test]
    fn cancelled_waiter_is_removed_without_consuming_its_start_slot() {
        let mut pacing = ProviderPacingState::default();
        let now = Instant::now();
        let interval = Duration::from_secs(1);
        let first =
            reserve_provider_start(&mut pacing, "provider", interval, now).expect("first slot");
        let cancelled =
            reserve_provider_start(&mut pacing, "provider", interval, now).expect("cancelled slot");
        let trailing =
            reserve_provider_start(&mut pacing, "provider", interval, now).expect("trailing slot");
        assert_eq!(trailing.scheduled, now + Duration::from_secs(2));

        // Use the explicit clock: the wall-clock variant reschedules from
        // `Instant::now()`, which is a few microseconds after `now`.
        remove_provider_pacing_ticket_at(
            &mut pacing,
            "provider",
            cancelled.ticket,
            cancelled.generation,
            false,
            now,
        );
        let queue = pacing.queues.get("provider").expect("provider queue");
        assert_eq!(queue.waiters.len(), 2);
        assert_eq!(queue.waiters[0].ticket, first.ticket);
        assert_eq!(queue.waiters[1].ticket, trailing.ticket);
        assert_eq!(queue.waiters[1].scheduled, now + interval);
    }

    #[test]
    fn pacing_overload_is_a_local_429_with_retry_after_and_no_route_failover() {
        let failure =
            provider_pacing_failure(ProviderPacingError::Overloaded(Duration::from_secs(3)));
        assert_eq!(failure.response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            failure
                .response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("3")
        );
        assert!(matches!(failure.kind, AttemptFailureKind::Request));
    }

    #[test]
    fn retry_after_uses_integer_ceiling_and_admission_reacquire_is_terminal_local() {
        assert_eq!(retry_after_seconds_ceil(Duration::from_millis(1_100)), 2);
        assert_eq!(retry_after_seconds_ceil(Duration::ZERO), 1);
        let rounded = provider_pacing_failure(ProviderPacingError::Overloaded(
            Duration::from_millis(1_100),
        ));
        assert_eq!(
            rounded
                .response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("2")
        );
        let failure = admission_reacquire_failure(
            "gateway inflight request limit reached after provider pacing",
        );
        assert_eq!(failure.response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(matches!(failure.kind, AttemptFailureKind::Request));
    }

    #[tokio::test]
    async fn cancelled_waiter_is_removed_and_settings_generation_reclaims_queues() {
        let pacing = Arc::new(ProviderPacing::default());
        let now = Instant::now();
        let slot = {
            let mut state = pacing.state.lock().await;
            reserve_provider_start(&mut state, "provider", Duration::from_secs(1), now)
                .expect("queued pacing slot")
        };
        let reservation = ProviderPacingReservation {
            pacing: Arc::clone(&pacing),
            provider_id: "provider".into(),
            ticket: slot.ticket,
            generation: slot.generation,
            armed: true,
        };
        drop(reservation);
        assert!(pacing
            .state
            .lock()
            .await
            .queues
            .get("provider")
            .is_none_or(|queue| queue.waiters.is_empty()));

        {
            let mut state = pacing.state.lock().await;
            reserve_provider_start(&mut state, "removed-provider", Duration::from_secs(1), now)
                .expect("provider queued before settings replacement");
        }
        let before = pacing.state.lock().await.generation;
        reset_provider_pacing(&pacing).await;
        let state = pacing.state.lock().await;
        assert_eq!(state.generation, before.wrapping_add(1));
        assert!(state.queues.is_empty());
    }
}

#[cfg(test)]
mod routing_attempt_lease_tests {
    use super::*;
    use crate::config::{ProviderCapabilities, ProviderDefinition};

    fn candidate(epoch: u64) -> RouteCandidate {
        RouteCandidate {
            provider: ProviderDefinition::default(),
            upstream_model: "model".into(),
            exposed_model: "model".into(),
            credential: None,
            account_id: None,
            target_key: "provider/model".into(),
            route_id: None,
            failure_threshold: 3,
            quota_threshold_percent: 90,
            input_price_per_million: None,
            output_price_per_million: None,
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            capabilities: ProviderCapabilities::default(),
            routing_epoch: epoch,
            routing_generation: 0,
            session_scope: None,
        }
    }

    fn response_with_body(body: reqwest::Body) -> reqwest::Response {
        axum::http::Response::builder()
            .status(StatusCode::OK)
            .body(body)
            .map(reqwest::Response::from)
            .expect("provider response")
    }

    async fn leased_response(
        body: reqwest::Body,
    ) -> (
        reqwest::Response,
        Arc<Mutex<RoutingRuntime>>,
        RouteCandidate,
    ) {
        let routing = Arc::new(Mutex::new(RoutingRuntime::default()));
        let epoch = routing.lock().await.epoch();
        let candidate = candidate(epoch);
        routing
            .lock()
            .await
            .begin_attempt(&candidate)
            .expect("attempt lease");
        let lease = RoutingAttemptLease(Arc::new(RoutingAttemptLeaseInner {
            routing: Arc::clone(&routing),
            candidate: candidate.clone(),
        }));
        let mut response = response_with_body(body);
        response.extensions_mut().insert(lease);
        (response, routing, candidate)
    }

    async fn assert_attempt_released(
        routing: &Arc<Mutex<RoutingRuntime>>,
        candidate: &RouteCandidate,
    ) {
        for _ in 0..16 {
            if routing.lock().await.active_attempts(candidate) == 0 {
                return;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(routing.lock().await.active_attempts(candidate), 0);
    }

    #[tokio::test]
    async fn buffered_response_and_bounded_reader_hold_attempt_until_body_eof() {
        let (response, routing, candidate) = leased_response(reqwest::Body::from("complete")).await;
        let buffered = buffer_provider_response(response, 1024)
            .await
            .expect("buffered provider response");
        assert_eq!(routing.lock().await.active_attempts(&candidate), 1);

        let rebuilt = buffered.into_response().expect("rebuilt response");
        assert_eq!(routing.lock().await.active_attempts(&candidate), 1);
        assert_eq!(
            read_bounded(rebuilt, 1024).await.expect("body"),
            b"complete".to_vec()
        );
        assert_attempt_released(&routing, &candidate).await;
    }

    #[tokio::test]
    async fn cancelling_guarded_body_releases_attempt_lease() {
        let pending = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();
        let (response, routing, candidate) =
            leased_response(reqwest::Body::wrap_stream(pending)).await;
        let guarded = provider_guard_stream(response, false, false);
        assert_eq!(routing.lock().await.active_attempts(&candidate), 1);
        drop(guarded);
        assert_attempt_released(&routing, &candidate).await;
    }
}

#[cfg(test)]
mod rate_limit_retry_tests {
    use super::*;
    use crate::config::ProviderLimits;

    #[test]
    fn retry_on_429_is_disabled_when_missing_or_defaulted() {
        let defaulted = ProviderLimits::default();
        let deserialized: ProviderLimits =
            serde_json::from_value(json!({})).expect("empty limits use safe defaults");

        assert!(!defaulted.retry_on_429);
        assert!(!deserialized.retry_on_429);
        assert!(!should_retry_rate_limit(
            &deserialized,
            CredentialSource::Environment,
            0,
        ));
    }

    #[test]
    fn retry_on_429_requires_opt_in_and_api_key_equivalent_credentials() {
        let limits = ProviderLimits {
            retry_on_429: true,
            max_429_retries: 2,
            ..ProviderLimits::default()
        };

        assert!(should_retry_rate_limit(
            &limits,
            CredentialSource::Environment,
            0,
        ));
        assert!(should_retry_rate_limit(
            &limits,
            CredentialSource::Keychain,
            1,
        ));
        assert!(!should_retry_rate_limit(
            &limits,
            CredentialSource::OAuth,
            0,
        ));
        assert!(!should_retry_rate_limit(
            &limits,
            CredentialSource::Forward,
            0,
        ));
        assert!(!should_retry_rate_limit(
            &limits,
            CredentialSource::Environment,
            2,
        ));
    }

    #[test]
    fn retry_after_is_not_truncated_to_the_old_five_second_cap() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("30"));

        assert_eq!(rate_limit_retry_delay(&headers, 0), Duration::from_secs(30));
    }

    #[test]
    fn same_key_wait_is_capped_without_shortening_routing_cooldown() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("7200"));

        assert_eq!(
            rate_limit_retry_delay(&headers, 0),
            Duration::from_secs(10 * 60)
        );
        assert_eq!(
            validated_retry_after(&headers).and_then(|value| value.1),
            Some(Duration::from_secs(7_200))
        );
    }
}

#[cfg(test)]
mod final_cache_measurement_tests {
    use super::*;
    #[tokio::test]
    async fn cache_health_buffer_keeps_raw_final_measurement_after_billing_rewrite() {
        let response=axum::http::Response::builder().status(200).body(reqwest::Body::from(json!({"choices":[{"message":{"content":"ok"}}]}).to_string())).unwrap();
        let mut buffered=buffer_provider_response(reqwest::Response::from(response),1_000_000).await.unwrap();
        assert!(!buffered.cache_observation.cache_read_reported);
        let mut rewritten=buffered.value.take().unwrap();
        write_token_usage(&mut rewritten,TokenUsage{input_tokens:100_000,cache_read_reported:true,..Default::default()});
        buffered.bytes=Bytes::from(rewritten.to_string());
        let again=buffer_provider_response(buffered.into_response().unwrap(),1_000_000).await.unwrap();
        assert!(!again.cache_observation.cache_read_reported);
        assert_eq!(again.cache_observation.input_tokens,0);
        assert_eq!(TokenUsage::from_json(again.value.as_ref().unwrap()).input_tokens,100_000);
    }
    #[test]
    fn cache_health_sse_final_measurement_keeps_unknown_and_explicit_zero_distinct() {
        for (protocol,missing,zero) in [
            (ProviderProtocol::ChatCompletions,json!({"usage":{"prompt_tokens":100_000}}),json!({"usage":{"prompt_tokens":100_000,"prompt_tokens_details":{"cached_tokens":0}}})),
            (ProviderProtocol::Responses,json!({"response":{"usage":{"input_tokens":100_000}}}),json!({"response":{"usage":{"input_tokens":100_000,"input_tokens_details":{"cached_tokens":0}}}})),
            (ProviderProtocol::GeminiGenerateContent,json!({"usageMetadata":{"promptTokenCount":100_000}}),json!({"usageMetadata":{"promptTokenCount":100_000,"cachedContentTokenCount":0}})),
        ] {
            let mut state=FinalCacheObservation::default();state.update(protocol,&missing);
            assert_eq!(state.value.input_tokens,100_000);assert!(!state.value.cache_read_reported);
            state.update(protocol,&zero);assert!(state.value.cache_read_reported);assert_eq!(state.value.cached_input_tokens,0);
            state=FinalCacheObservation::default();state.update(protocol,&json!({"choices":[{"delta":{"content":"ok"}}]}));
            assert_eq!(state.value.input_tokens,0);assert!(!state.value.cache_read_reported);
        }
    }
    #[test]
    fn cache_health_anthropic_missing_cache_does_not_become_reported_zero() {
        let mut state=FinalCacheObservation::default();
        state.update(ProviderProtocol::AnthropicMessages,&json!({"type":"message_start","message":{"id":"m","role":"assistant","content":[],"usage":{"input_tokens":100_000,"output_tokens":0}}}));
        state.update(ProviderProtocol::AnthropicMessages,&json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}));
        assert_eq!(state.value.input_tokens,100_000);assert!(!state.value.cache_read_reported);
    }
    #[test]
    fn cache_health_anthropic_sparse_delta_preserves_real_cache_sample() {
        let mut state=FinalCacheObservation::default();
        state.update(ProviderProtocol::AnthropicMessages,&json!({"type":"message_start","message":{"id":"m","role":"assistant","content":[],"usage":{"input_tokens":1000,"cache_read_input_tokens":99_000,"cache_creation_input_tokens":0,"output_tokens":0}}}));
        state.update(ProviderProtocol::AnthropicMessages,&json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}));
        assert_eq!(state.value.input_tokens,100_000);assert_eq!(state.value.cached_input_tokens,99_000);assert!(state.value.cache_read_reported);
    }
}

#[cfg(test)]
mod chat_trailing_usage_tests {
    use super::*;
    #[tokio::test]
    async fn cache_health_chat_guard_adapter_monitor_includes_late_usage_in_same_and_split_chunks() {
        for split in [false,true] {
            let mut settings=GatewaySettings::default();settings.security.dns_pinning=false;
            let settings=Arc::new(RwLock::new(settings));
            let ledger=ObservabilityLedger::new(None);
            let response_state=Arc::new(ResponseStateStore::new(None));
            let state=GatewayState {settings:settings.clone(),client:reqwest::Client::new(),routing:Arc::new(Mutex::new(RoutingRuntime::default())),observability:ledger.clone(),video_jobs:Arc::new(Mutex::new(HashMap::new())),instance_id:"chat-usage-test".into(),response_state:response_state.clone(),memory:Arc::new(MemoryAdmission{settings,inflight:AtomicU32::new(0),reserved_bytes:AtomicU64::new(0),rejected:AtomicU64::new(0)}),pacing:Arc::new(ProviderPacing::default())};
            let mut provider=crate::config::ProviderDefinition::default();provider.id="chat".into();provider.protocol=ProviderProtocol::ChatCompletions;
            let candidate=RouteCandidate{provider,upstream_model:"model".into(),exposed_model:"model".into(),credential:None,account_id:None,target_key:"chat/model".into(),route_id:None,failure_threshold:3,quota_threshold_percent:90,input_price_per_million:None,output_price_per_million:None,context_window:None,max_input_tokens:None,max_output_tokens:None,reasoning_efforts:Vec::new(),default_reasoning_effort:None,capabilities:crate::config::ProviderCapabilities::default(),routing_epoch:0,routing_generation:0,session_scope:None};
            for n in 0..20 {
                let head=format!("data: {}\n\ndata: {}\n\n",json!({"id":"chat-test","choices":[{"index":0,"delta":{"role":"assistant","content":"ok"},"finish_reason":null}]}),json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}));
                let tail=format!("data: {}\n\ndata: [DONE]\n\n",json!({"choices":[],"usage":{"prompt_tokens":100_000,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":0}}}));
                let chunks=if split {vec![head,tail]} else {vec![format!("{head}{tail}")]};
                let source=futures_util::stream::iter(chunks.into_iter().map(|s|Ok::<_,std::io::Error>(Bytes::from(s))));
                let raw=Response::builder().status(200).header(header::CONTENT_TYPE,"text/event-stream").body(reqwest::Body::wrap_stream(source)).unwrap();
                let guarded=guarded_provider_sse(state.clone(),json!({}),candidate.clone(),None,reqwest::Response::from(raw),None,None).unwrap();
                let shared=guarded.extensions().get::<SharedProviderRetryObservation>().unwrap().clone();
                let mut observation=ObservationSeed::for_candidate(ledger.clone(),ObservabilitySettings::default(),format!("chat-{split}-{n}"),true,Instant::now(),1,&candidate);
                observation.request_diagnostic=Some(crate::observability::RequestDiagnostic{client_thread_id:Some("01a13ddb-8d21-7cb2-8f00-23a8a09af079".into()),..Default::default()});
                observation.record_shared_provider_retries(shared);
                let adapted=adapt_successful_response(guarded,&candidate,observation,ResponseToolMap::default(),&response_state,&json!({"model":"model","stream":true,"input":[]}),false,false).await;
                let bytes=to_bytes(adapted.into_body(),1_000_000).await.unwrap();
                assert!(String::from_utf8_lossy(&bytes).contains("response.completed"));
            }
            let row=&ledger.cache_health_summary().sessions[0];
            assert_eq!(row.successful_requests,20);assert_eq!(row.unknown_cache_requests,0);
            assert_eq!(row.rolling_requests,20);assert!(row.active_alert);
        }
    }
}

#[cfg(test)]
mod chat_nonretryable_terminal_tests {
    use super::*;
    #[tokio::test]
    async fn cache_health_chat_empty_length_and_filter_keep_usage_without_retry() {
        for reason in ["length","content_filter","max_tokens"] {
            let settings=Arc::new(RwLock::new(GatewaySettings::default()));
            let state=GatewayState{settings:settings.clone(),client:reqwest::Client::new(),routing:Arc::new(Mutex::new(RoutingRuntime::default())),observability:ObservabilityLedger::new(None),video_jobs:Arc::new(Mutex::new(HashMap::new())),instance_id:"chat-terminal-test".into(),response_state:Arc::new(ResponseStateStore::new(None)),memory:Arc::new(MemoryAdmission{settings,inflight:AtomicU32::new(0),reserved_bytes:AtomicU64::new(0),rejected:AtomicU64::new(0)}),pacing:Arc::new(ProviderPacing::default())};
            let mut provider=crate::config::ProviderDefinition::default();provider.protocol=ProviderProtocol::ChatCompletions;provider.base_url="http://127.0.0.1:9".into();provider.credential.source=CredentialSource::None;
            let candidate=RouteCandidate{capabilities:provider.capabilities.clone(),provider,upstream_model:"model".into(),exposed_model:"model".into(),credential:None,account_id:None,target_key:"test/model".into(),route_id:None,failure_threshold:3,quota_threshold_percent:90,input_price_per_million:None,output_price_per_million:None,context_window:None,max_input_tokens:None,max_output_tokens:None,reasoning_efforts:Vec::new(),default_reasoning_effort:None,routing_epoch:0,routing_generation:0,session_scope:None};
            let raw=format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]}),json!({"choices":[],"usage":{"prompt_tokens":100_000,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":99_000}}}));
            let upstream=Response::builder().status(200).body(reqwest::Body::from(raw)).unwrap();
            let guarded=guarded_provider_sse(state,json!({}),candidate,None,reqwest::Response::from(upstream),None,None).unwrap();let shared=guarded.extensions().get::<SharedProviderRetryObservation>().unwrap().clone();
            let bytes=read_bounded(guarded,1_000_000).await.unwrap();let text=String::from_utf8(bytes).unwrap();assert!(text.contains(reason));assert!(text.contains("100000"));assert!(!text.contains("codetas-recovery"));assert_eq!(shared.0.lock().unwrap().additional_sends,0);
        }
    }
}

#[cfg(test)]
mod raw_anthropic_billing_tests {
    use super::*;
    #[test]
    fn raw_anthropic_retry_unknown_cache_preserves_total_and_does_not_fabricate_cache_report() {
        let unknown=TokenUsage::from_json(&json!({"usage":{"input_tokens":1000,"cache_creation_input_tokens":500,"output_tokens":1}}));
        let known=TokenUsage::from_json(&json!({"usage":{"input_tokens":1000,"cache_read_input_tokens":99_000,"cache_creation_input_tokens":500,"output_tokens":1}}));
        let aggregate=sum_token_usage(unknown,known);assert_eq!(aggregate.input_tokens,102_000);assert!(!aggregate.cache_read_reported);
        let mut value=json!({"type":"message","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{}});write_token_usage(&mut value,aggregate);
        let adapted=anthropic_to_response(&value,"model",&ResponseToolMap::default()).unwrap();let usage=TokenUsage::from_json(&adapted);assert_eq!(usage.input_tokens,102_000);assert!(!usage.cache_read_reported);
    }
    #[test]
    fn chat_raw_cache_extension_is_a_subset_of_prompt_tokens_not_extra_tokens() {
        let value=json!({"usage":{"prompt_tokens":100_000,"cache_read_input_tokens":99_000,"completion_tokens":1}});
        assert_eq!(TokenUsage::from_json(&value).input_tokens,100_000);
        assert_eq!(crate::observability::CacheObservation::from_provider_json(&value).input_tokens,100_000);
    }
}

#[cfg(test)]
mod physical_stream_observation_tests {
    use super::*;
    #[tokio::test]
    async fn known_gemini_failure_before_footer_cannot_become_success_on_client_cancel() {
        let settings=Arc::new(RwLock::new(GatewaySettings::default()));let ledger=ObservabilityLedger::new(None);
        let state=GatewayState{settings:settings.clone(),client:reqwest::Client::new(),routing:Arc::new(Mutex::new(RoutingRuntime::default())),observability:ledger.clone(),video_jobs:Arc::new(Mutex::new(HashMap::new())),instance_id:"physical-observation-test".into(),response_state:Arc::new(ResponseStateStore::new(None)),memory:Arc::new(MemoryAdmission{settings,inflight:AtomicU32::new(0),reserved_bytes:AtomicU64::new(0),rejected:AtomicU64::new(0)}),pacing:Arc::new(ProviderPacing::default())};
        let mut provider=crate::config::ProviderDefinition::default();provider.id="test".into();provider.protocol=ProviderProtocol::GeminiGenerateContent;provider.base_url="http://127.0.0.1:9".into();provider.credential.source=CredentialSource::None;
        let candidate=RouteCandidate{capabilities:provider.capabilities.clone(),provider,upstream_model:"model".into(),exposed_model:"model".into(),credential:None,account_id:None,target_key:"test/model".into(),route_id:None,failure_threshold:3,quota_threshold_percent:90,input_price_per_million:None,output_price_per_million:None,context_window:None,max_input_tokens:None,max_output_tokens:None,reasoning_efforts:Vec::new(),default_reasoning_effort:None,routing_epoch:0,routing_generation:0,session_scope:None};
        let (sender,mut receiver)=mpsc::channel::<Bytes>(4);let source=stream! {while let Some(bytes)=receiver.recv().await {yield Ok::<Bytes,std::io::Error>(bytes);}};
        let first=json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"id":"c","name":"lookup","args":{"x":1}}}]}}],"usageMetadata":{"promptTokenCount":100_000,"candidatesTokenCount":1,"cachedContentTokenCount":0}});
        sender.send(Bytes::from(format!("data: {}\n\n",first))).await.unwrap();
        let raw=Response::builder().status(200).header(header::CONTENT_TYPE,"text/event-stream").body(reqwest::Body::wrap_stream(source)).unwrap();let guarded=guarded_provider_sse(state.clone(),json!({}),candidate.clone(),None,reqwest::Response::from(raw),None,None).unwrap();let shared=guarded.extensions().get::<SharedProviderRetryObservation>().unwrap().clone();
        let mut seed=ObservationSeed::for_candidate(ledger.clone(),ObservabilitySettings::default(),"known-failure".into(),true,Instant::now(),1,&candidate);seed.request_diagnostic=Some(crate::observability::RequestDiagnostic{client_thread_id:Some("01a13ddb-8d21-7cb2-8f00-23a8a09af079".into()),..Default::default()});seed.record_shared_provider_retries(shared.clone());
        let response=adapt_successful_response(guarded,&candidate,seed,ResponseToolMap::default(),&state.response_state,&json!({"input":[],"stream":true}),false,false).await;let mut body=response.into_body().into_data_stream();
        loop {let bytes=tokio::time::timeout(Duration::from_secs(2),body.next()).await.unwrap().unwrap().unwrap();if String::from_utf8_lossy(&bytes).contains("response.function_call_arguments.delta") {break;}}
        sender.send(Bytes::from(format!("data: {}\n\n",json!({"candidates":[{"finishReason":"MAX_TOKENS"}]})))).await.unwrap();
        loop {match tokio::time::timeout(Duration::from_millis(50),body.next()).await {Err(_)=>break,Ok(Some(Ok(bytes)))=>assert!(!String::from_utf8_lossy(&bytes).contains("response.failed")),_=>panic!("unexpected stream termination before footer")}}
        assert!(shared.0.lock().unwrap().terminal_failure);drop(body);assert_eq!(ledger.summary().failed_requests,1);assert_eq!(ledger.summary().successful_requests,0);assert_eq!(ledger.summary().input_tokens,100_000);assert!(ledger.cache_health_summary().sessions.is_empty());
    }
}


pub(crate) fn failure_with_provider_metadata(
    mut failure: AttemptFailure,
    retry: Option<&ProviderRetryObservation>,
    diagnostic: Option<&crate::observability::RequestDiagnostic>,
) -> AttemptFailure {
    if let Some(retry) = retry { failure.response.extensions_mut().insert(retry.clone()); }
    if let Some(diagnostic) = diagnostic { failure.response.extensions_mut().insert(diagnostic.clone()); }
    failure
}


// Consume the rejected body and release its concurrency slot before acquiring
// another lease. Neither refresh retries nor their failures lose known usage.
async fn retire_oauth_response(mut response: reqwest::Response) -> (
    ProviderRetryObservation,
    Option<UpstreamErrorDiagnostic>,
    Option<crate::observability::RequestDiagnostic>,
) {
    let diagnostic = response.extensions().get::<UpstreamErrorDiagnostic>().cloned();
    let request_diagnostic = response.extensions().get::<crate::observability::RequestDiagnostic>().cloned();
    let mut recovery = response.extensions_mut().remove::<ProviderRetryObservation>().unwrap_or_default();
    let lease = take_routing_attempt_lease(&mut response);
    let usage = response_body_usage_from_reqwest(response).await;
    recovery.usage = sum_token_usage(recovery.usage, usage);
    drop(lease);
    recovery.additional_sends = recovery.additional_sends.saturating_add(1);
    recovery.recovery_kinds.push("oauth-refresh".into());
    (recovery, diagnostic, request_diagnostic)
}


#[cfg(test)]
mod review11_retry_tests {
    use super::*;

    fn usage(input: u64) -> TokenUsage {
        TokenUsage { input_tokens: input, output_tokens: 3, total_tokens: input + 3,
            cached_input_tokens: input / 2, cache_creation_input_tokens: 2,
            reasoning_tokens: 1, cache_read_reported: true }
    }
    fn seed(ledger: &ObservabilityLedger) -> ObservationSeed {
        ObservationSeed::without_candidate(ledger.clone(), ObservabilitySettings::default(),
            "review11".into(), "fixture/model", false, Instant::now())
    }

    #[tokio::test]
    async fn failed_empty_retry_accounts_http_and_empty_attempts_exactly_once() {
        let response = empty_completion_retry_failed_response_with_observation(usage(200), 2,
            Some(ProviderRetryObservation { usage: usage(100), additional_sends: 1, ..Default::default() })).unwrap();
        let marker = response.extensions().get::<EmptyCompletionRecoveryFailure>().unwrap().clone();
        let retry = response.extensions().get::<ProviderRetryObservation>().unwrap().clone();
        assert_eq!(marker.usage.input_tokens, 300);
        assert_eq!(retry.additional_sends, 3);
        let body: Value = serde_json::from_slice(&read_bounded(response, 65536).await.unwrap()).unwrap();
        assert_eq!(TokenUsage::from_json(&body).input_tokens, 300);
        // Both the dedicated request failure consumer and generic compact
        // consumers must charge the same total, without a duplicate HTTP retry.
        for dedicated in [false, true] {
            let ledger = ObservabilityLedger::new(None);
            let mut observation = seed(&ledger);
            let mut metadata = retry.clone();
            if dedicated { metadata.usage = TokenUsage::default(); }
            observation.record_provider_retries(&metadata);
            observation.finish(StatusCode::BAD_GATEWAY, Some("empty"), marker.usage.clone());
            let summary = ledger.summary();
            assert_eq!(summary.input_tokens, 300);
            assert_eq!(summary.output_tokens, 6);
            assert_eq!(summary.cached_input_tokens, 150);
            assert_eq!(summary.reasoning_tokens, 2);
        }
    }

    #[tokio::test]
    async fn buffer_read_failure_retains_known_retries_and_wire_diagnostic() {
        for oversized in [false, true] {
            let mut response = if oversized {
                reqwest::Response::from(Response::builder().body(reqwest::Body::from("oversized")).unwrap())
            } else {
                let stream = futures_util::stream::iter(vec![Err::<Bytes, _>(std::io::Error::other("fixture read failure"))]);
                reqwest::Response::from(Response::builder().body(reqwest::Body::wrap_stream(stream)).unwrap())
            };
            response.extensions_mut().insert(ProviderRetryObservation { usage: usage(123), additional_sends: 2, ..Default::default() });
            response.extensions_mut().insert(crate::observability::RequestDiagnostic { wire_bytes: 456, ..Default::default() });
            let failure = match buffer_provider_response(response, 4).await { Err(failure) => failure, Ok(_) => panic!("must fail") };
            let retry = failure.response.extensions().get::<ProviderRetryObservation>().unwrap();
            assert_eq!(retry.usage.input_tokens, 123);
            assert_eq!(retry.additional_sends, 2);
            assert_eq!(failure.response.extensions().get::<crate::observability::RequestDiagnostic>().unwrap().wire_bytes, 456);
        }
    }

    #[test]
    fn sparse_cache_updates_preserve_known_fields_only_within_one_attempt() {
        for (protocol, first, next, zero) in [
            (ProviderProtocol::ChatCompletions,
                json!({"usage":{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":90}}}),
                json!({"usage":{"prompt_tokens":110,"completion_tokens":1}}),
                json!({"usage":{"prompt_tokens_details":{"cached_tokens":0}}})),
            (ProviderProtocol::GeminiGenerateContent,
                json!({"response":{"usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":90}}}),
                json!({"response":{"usageMetadata":{"promptTokenCount":110,"candidatesTokenCount":1}}}),
                json!({"response":{"usageMetadata":{"cachedContentTokenCount":0}}})),
        ] {
            let mut tracker = FinalCacheObservation::default();
            tracker.update(protocol, &first);
            tracker.update(protocol, &next);
            assert_eq!(tracker.value.input_tokens, 110);
            assert_eq!(tracker.value.cached_input_tokens, 90);
            assert!(tracker.value.cache_read_reported);
            tracker.update(protocol, &zero);
            assert_eq!(tracker.value.cached_input_tokens, 0);
            assert!(tracker.value.cache_read_reported);
            let mut another = FinalCacheObservation::default();
            another.update(protocol, &next);
            assert!(!another.value.cache_read_reported);
        }
    }

    #[test]
    fn repaired_compaction_usage_roundtrips_every_counter_without_double_input() {
        let combined = sum_token_usage(usage(100), usage(200));
        let value = json!({"usage": token_usage_json(&combined)});
        let decoded = TokenUsage::from_json(&value);
        assert_eq!(decoded.input_tokens, 300);
        assert_eq!(decoded.output_tokens, 6);
        assert_eq!(decoded.cached_input_tokens, 150);
        assert_eq!(decoded.cache_creation_input_tokens, 4);
        assert_eq!(decoded.reasoning_tokens, 2);
        assert_eq!(decoded.total_tokens, 306);
        assert!(decoded.cache_read_reported);
    }

    #[tokio::test]
    async fn oauth_retirement_releases_lease_and_preserves_rejected_usage() {
        let mut provider = crate::config::ProviderDefinition::default();
        provider.id = "fixture".into(); provider.enabled = true;
        provider.models = vec!["model".into()]; provider.credential.source = CredentialSource::None;
        let mut settings = GatewaySettings::default(); settings.providers = vec![provider];
        let routing = Arc::new(Mutex::new(RoutingRuntime::default()));
        let candidate = routing.lock().await.candidates_for_request(&settings, "fixture/model", false, None).unwrap().remove(0);
        routing.lock().await.begin_attempt(&candidate).unwrap();
        let lease = RoutingAttemptLease(Arc::new(RoutingAttemptLeaseInner { routing: routing.clone(), candidate: candidate.clone() }));
        let weak = Arc::downgrade(&lease.0);
        let mut response = reqwest::Response::from(Response::builder().status(401)
            .body(reqwest::Body::from(json!({"usage":{"input_tokens":20,"output_tokens":3}}).to_string())).unwrap());
        response.extensions_mut().insert(lease);
        response.extensions_mut().insert(ProviderRetryObservation { usage: usage(100), additional_sends: 2, ..Default::default() });
        response.extensions_mut().insert(crate::observability::RequestDiagnostic { wire_bytes: 999, ..Default::default() });
        let (retry, _, diagnostic) = retire_oauth_response(response).await;
        assert!(weak.upgrade().is_none(), "first attempt must be retired before resend");
        routing.lock().await.begin_attempt(&candidate).unwrap();
        routing.lock().await.end_attempt(&candidate);
        assert_eq!(retry.usage.input_tokens, 120);
        assert_eq!(retry.additional_sends, 3);
        assert_eq!(retry.recovery_kinds, ["oauth-refresh"]);
        assert_eq!(diagnostic.unwrap().wire_bytes, 999);
    }
}

#[cfg(test)]
mod regression12_sender_tests {
    use super::*;

    fn candidate_and_state() -> (RouteCandidate, GatewayState) {
        let mut settings = GatewaySettings::default();
        settings.observability.usage_log = true;
        settings.observability.request_log = true;
        settings.providers = vec![serde_json::from_value(json!({
            "id":"regression12", "name":"regression12", "enabled":true,
            "baseUrl":"https://api.anthropic.com/v1", "models":["claude-opus-5-5"],
            "protocol":ProviderProtocol::AnthropicMessages
        })).unwrap()];
        let mut routing = RoutingRuntime::default();
        let candidate = routing.candidates_for_request(
            &settings, "regression12/claude-opus-5-5", false, None).unwrap().remove(0);
        let settings = Arc::new(RwLock::new(settings));
        let state = GatewayState {
            settings: settings.clone(),
            client: reqwest::Client::new(),
            routing: Arc::new(Mutex::new(routing)),
            observability: ObservabilityLedger::new(None),
            video_jobs: Arc::new(Mutex::new(HashMap::new())),
            instance_id: Uuid::new_v4().to_string(),
            response_state: Arc::new(ResponseStateStore::new(None)),
            memory: Arc::new(MemoryAdmission { settings, inflight: AtomicU32::new(0),
                reserved_bytes: AtomicU64::new(0), rejected: AtomicU64::new(0) }),
            pacing: Arc::new(ProviderPacing::default()),
        };
        (candidate, state)
    }
    fn response(status: StatusCode, value: Value) -> reqwest::Response {
        Response::builder().status(status).header(header::CONTENT_TYPE,"application/json")
            .header(header::RETRY_AFTER,"0")
            .body(reqwest::Body::from(value.to_string())).unwrap().into()
    }
    fn seed(state: &GatewayState, candidate: &RouteCandidate, id: &str) -> ObservationSeed {
        let mut settings = ObservabilitySettings::default();
        settings.usage_log = true;
        settings.request_log = true;
        ObservationSeed::for_candidate(state.observability.clone(), settings, id.into(),
            false, Instant::now(), 1, candidate)
    }

    #[tokio::test]
    async fn regression12_image_rejection_preserves_all_billing_once_and_diagnostics() {
        for status in [StatusCode::BAD_REQUEST, StatusCode::PAYLOAD_TOO_LARGE] {
            let (candidate, state) = candidate_and_state();
            let mut upstream = response(status, json!({"error":{"type":"invalid_request_error","message":"too many images"},
                "usage":{"input_tokens":11,"output_tokens":3}}));
            upstream.extensions_mut().insert(SentImageState { omitted_before_send:0, sent_images:2 });
            upstream.extensions_mut().insert(ProviderRetryObservation {
                additional_sends:2, recovery_kinds:vec!["rate-limit".into(),"provider-5xx".into()],
                usage:TokenUsage { input_tokens:37, output_tokens:5, total_tokens:42, ..Default::default() },
                ..Default::default()
            });
            upstream.extensions_mut().insert(crate::observability::RequestDiagnostic {
                wire_model:Some("wire-model".into()), wire_bytes:123, upstream_status_code:status.as_u16(), ..Default::default()
            });
            let failure = require_compaction_on_image_rejection(upstream, status).await.unwrap_err();
            let retry = failure.response.extensions().get::<ProviderRetryObservation>().unwrap();
            assert_eq!(retry.additional_sends, 2);
            assert_eq!(retry.usage.input_tokens, 48);
            assert_eq!(retry.usage.output_tokens, 8);
            assert_eq!(failure.response.extensions().get::<crate::observability::RequestDiagnostic>().unwrap().wire_bytes,123);
            assert!(failure.response.extensions().get::<UpstreamErrorDiagnostic>().is_some());
            let mut observation = seed(&state, &candidate, "image-error");
            observation.record_provider_retries(retry);
            observation.record_upstream_error(&failure.response);
            observation.finish(failure.response.status(), Some("request_rejected"), TokenUsage::default());
            let summary = state.observability.summary();
            assert_eq!(summary.input_tokens, 48);
            assert_eq!(summary.output_tokens, 8);
            assert_eq!(summary.total_tokens, 56);
            assert_eq!(summary.failed_requests, 1);
        }
    }

    #[tokio::test]
    async fn regression12_pacing_failure_keeps_one_prior_physical_attempt() {
        for status in [StatusCode::TOO_MANY_REQUESTS, StatusCode::BAD_GATEWAY] {
            let (mut candidate, state) = candidate_and_state();
            candidate.provider.credential.source = CredentialSource::Environment;
            candidate.credential = None;
            candidate.provider.limits.request_retries = 1;
            candidate.provider.limits.retry_on_429 = true;
            candidate.provider.limits.max_429_retries = 1;
            candidate.provider.limits.request_pacing_ms = 60_000;
            let sends = Arc::new(AtomicU32::new(0));
            let sent = sends.clone();
            let pacing = state.pacing.clone();
            let result = tokio::time::timeout(Duration::from_secs(2), retry_provider_request(&state,&candidate,false,move || {
                let pacing = pacing.clone();
                let sent = sent.clone();
                async move {
                    assert_eq!(sent.fetch_add(1,Ordering::Relaxed),0);
                    tokio::spawn(async move {
                        loop {
                            if pacing.state.lock().await.queues.values().any(|q| !q.waiters.is_empty()) {
                                reset_provider_pacing(&pacing).await;
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                    });
                    Ok(response(status,json!({"usage":{"input_tokens":17,"output_tokens":4}})))
                }
            })).await.unwrap();
            let failure = result.unwrap_err();
            if failure.response.extensions().get::<ProviderRetryObservation>().is_none() {
                let bytes = to_bytes(failure.response.into_body(), 65536).await.unwrap();
                panic!("missing retry evidence after {} sends: {}", sends.load(Ordering::Relaxed), String::from_utf8_lossy(&bytes));
            }
            let retry = failure.response.extensions().get::<ProviderRetryObservation>().unwrap();
            assert_eq!(sends.load(Ordering::Relaxed),1);
            assert_eq!(retry.additional_sends,0);
            assert_eq!(retry.usage.input_tokens,17);
            let mut observation = seed(&state,&candidate,"pacing-error");
            observation.record_provider_retries(retry);
            observation.finish(failure.response.status(),Some("request_rejected"),TokenUsage::default());
            assert_eq!(state.observability.summary().total_tokens,21);
        }
    }

    #[test]
    fn regression12_forward_never_proves_signature_account() {
        let (mut candidate, _) = candidate_and_state();
        candidate.credential = None;
        candidate.provider.credential.source = CredentialSource::Environment;
        let checkpoint = crate::anthropic_compaction::Checkpoint {
            version:1, scope:anthropic_checkpoint_scope(&candidate, "test-auth-binding"), context_hash:"a".repeat(64),
            block:json!({"type":"compaction","content":"visible summary","signature":"opaque-test-signature"}),
            retained:vec![],
        };
        let mut body = json!({"input":[crate::anthropic_compaction::encode(&checkpoint).unwrap()]});
        assert!(preserve_native_anthropic_history(&body,&candidate,Some(&anthropic_checkpoint_scope(&candidate, "test-auth-binding"))));
        for account in [None,Some("configured-label".to_string())] {
            candidate.provider.credential.source = CredentialSource::Forward;
            candidate.account_id = account;
            let mut forwarded = checkpoint.clone();
            forwarded.scope = anthropic_checkpoint_scope(&candidate, "test-auth-binding");
            body["input"] = json!([crate::anthropic_compaction::encode(&forwarded).unwrap()]);
            assert!(!preserve_native_anthropic_history(&body,&candidate,Some(&anthropic_checkpoint_scope(&candidate, "test-auth-binding"))));
            prepare_native_anthropic_history(&mut body,&candidate,Some(&anthropic_checkpoint_scope(&candidate, "test-auth-binding"))).unwrap();
            assert!(!body.to_string().contains("opaque-test-signature"));
            assert!(body.to_string().contains("visible summary"));
        }
    }

    #[tokio::test]
    async fn regression12_wire_model_survives_buffering_and_separates_cache_windows() {
        let (mut candidate, state) = candidate_and_state();
        candidate.provider.google_mode = GoogleMode::CloudCodeAssist;
        let thread = Uuid::new_v4().to_string();
        for index in 0..20 {
            let effort = if index % 2 == 0 { "low" } else { "high" };
            let wire = wire_model_for_request(&candidate,&json!({"reasoning":{"effort":effort}}));
            assert_eq!(wire,format!("claude-opus-5-5-{effort}"));
            let mut upstream = response(StatusCode::OK,json!({"usage":{"input_tokens":100000,"input_tokens_details":{"cached_tokens":0}}}));
            let mut headers = HeaderMap::new();
            headers.insert("session_id",HeaderValue::from_str(&thread).unwrap());
            diagnostics::attach_wire_diagnostic(&mut upstream,&json!({"model":wire}),10,Some(&headers));
            let upstream = buffer_provider_response(upstream,1024).await.unwrap().into_response().unwrap();
            let mut observation = seed(&state,&candidate,&format!("wire-{index}"));
            observation.record_reqwest_upstream_error(&upstream);
            observation.finish(StatusCode::OK,None,TokenUsage::from_json(&json!({"usage":{"input_tokens":100000,"input_tokens_details":{"cached_tokens":0}}})));
        }
        let health = state.observability.cache_health_summary();
        assert_eq!(health.sessions.len(),2);
        for row in health.sessions {
            assert_eq!(row.rolling_requests,10);
            assert!(!row.active_alert);
            assert!(row.model.ends_with("-low") || row.model.ends_with("-high"));
        }
    }
}


#[cfg(test)]
mod review15_provenance_transport_regressions {
    use super::*;
    use super::super::anthropic_compact::ThinkingProvenance;

    fn proof() -> ThinkingProvenance {
        ThinkingProvenance::for_wire(&json!({"messages":[],"system":"fixture"}), Some(&"a".repeat(64)), false)
    }

    #[test]
    fn review15_generation_provenance_never_enters_other_provider_wire() {
        let mut reasoning = json!({"type":"reasoning","summary":[{"type":"summary_text","text":"summary"}],
            "provider_metadata":{"anthropic":{"thinking_blocks":[{"type":"thinking","thinking":"thought","signature":"fixture-signature"}]}}});
        proof().annotate(&mut reasoning);
        let source = json!({"model":"fixture","input":[reasoning,{"type":"message","role":"user","content":"continue"}]});
        for preserve_metadata in [false, true] {
            let chat = responses_to_chat_with_options(&chat_compatible_request(&source, preserve_metadata), "fixture", false).unwrap();
            assert!(!chat.to_string().contains("generation_provenance"));
        }
        let gemini = responses_to_gemini(&source, "fixture").unwrap();
        assert!(!gemini.to_string().contains("generation_provenance"));
        let (kiro, _) = crate::kiro::responses_to_kiro_checked(&source, "fixture", None).unwrap();
        assert!(!kiro.to_string().contains("generation_provenance"));
        let mut responses = source.clone();
        crate::compat::strip_responses_provider_metadata(&mut responses);
        assert!(!responses.to_string().contains("generation_provenance"));
        assert!(source.to_string().contains("generation_provenance"), "wire stripping must not mutate replay source");
    }

    #[tokio::test]
    async fn review15_buffered_response_rebuild_preserves_thinking_provenance() {
        let proof = proof();
        let mut response = reqwest::Response::from(Response::builder().status(200)
            .body(reqwest::Body::from(json!({"type":"message","content":[{"type":"text","text":"nonempty"}]}).to_string())).unwrap());
        response.extensions_mut().insert(proof.clone());
        let buffered = buffer_provider_response(response, 1024).await.unwrap();
        let rebuilt = buffered.into_response().unwrap();
        assert_eq!(rebuilt.extensions().get::<ThinkingProvenance>(), Some(&proof));
    }
}

#[derive(Debug)]
struct ProviderStreamReadFailure { message: &'static str, usage: TokenUsage }
impl std::fmt::Display for ProviderStreamReadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.message) }
}
impl std::error::Error for ProviderStreamReadFailure {}
fn provider_stream_read_failure(message: &'static str, collector: &super::stream::SseUsageCollector) -> std::io::Error {
    std::io::Error::other(ProviderStreamReadFailure { message, usage: collector.usage.clone() })
}


#[cfg(test)]
mod review16_physical_guard_regressions {
    use super::*;
    use super::super::adapt::review16_regressions::{candidate, response, seed};
    pub(super) fn state(ledger: &ObservabilityLedger) -> GatewayState {
        let settings=Arc::new(RwLock::new(GatewaySettings::default()));
        let memory=Arc::new(MemoryAdmission {settings:settings.clone(),inflight:AtomicU32::new(0),reserved_bytes:AtomicU64::new(0),rejected:AtomicU64::new(0)});
        GatewayState {settings,client:reqwest::Client::new(),routing:Arc::new(Mutex::new(RoutingRuntime::default())),observability:ledger.clone(),video_jobs:Arc::new(Mutex::new(HashMap::new())),instance_id:"test".into(),response_state:Arc::new(ResponseStateStore::new(None)),memory,pacing:Arc::new(ProviderPacing::default())}
    }
    #[tokio::test]
    async fn review16_physical_guards_limits_keep_billing_once_and_fail_downstream() {
        for protocol in [ProviderProtocol::Responses,ProviderProtocol::ChatCompletions,ProviderProtocol::AnthropicMessages,ProviderProtocol::GeminiGenerateContent] {
            let mut c=candidate();c.provider.protocol=protocol;
            let prefix=match protocol {
                ProviderProtocol::Responses => sse("response.completed",&json!({"type":"response.completed","response":{"id":"physical","status":"completed","output":[],"usage":{"input_tokens":17,"output_tokens":3}}})),
                ProviderProtocol::AnthropicMessages => sse("message_start",&json!({"type":"message_start","message":{"usage":{"input_tokens":17,"output_tokens":3}}})),
                ProviderProtocol::GeminiGenerateContent => sse("message",&json!({"usageMetadata":{"promptTokenCount":17,"candidatesTokenCount":3},"candidates":[{"finishReason":"STOP"}]})),
                _ => sse("message",&json!({"usage":{"prompt_tokens":17,"completion_tokens":3},"choices":[]})),
            };
            c.provider.limits.max_response_bytes=prefix.len() as u64;
            let ledger=ObservabilityLedger::new(None);let state=state(&ledger);
            let mut upstream=response(vec![Ok(Bytes::from(format!("{prefix}excess")))],StatusCode::OK);
            upstream.extensions_mut().insert(ProviderRetryObservation {usage:TokenUsage {input_tokens:7,total_tokens:7,..Default::default()},additional_sends:1,..Default::default()});
            let request=json!({"input":"test","stream":true,"store":false});
            let guarded=guarded_provider_sse(state,request.clone(),c.clone(),None,upstream,None,None).unwrap();
            let shared=guarded.extensions().get::<SharedProviderRetryObservation>().unwrap().clone();
            let mut observation=seed(&ledger,&c,true);
            observation.record_provider_retries(guarded.extensions().get::<ProviderRetryObservation>().unwrap());
            observation.record_shared_provider_retries(shared.clone());
            let client=adapt_successful_response(guarded,&c,observation,ResponseToolMap::default(),&Arc::new(ResponseStateStore::new(None)),&request,true,false).await;
            let mut source=client.into_body().into_data_stream();let mut text=Vec::new();
            while let Some(chunk)=source.next().await {if let Ok(bytes)=chunk {text.extend_from_slice(&bytes);} else {break;}}
            drop(source);
            assert!(!String::from_utf8_lossy(&text).contains("response.completed"),"{protocol:?}");
            assert!(shared.0.lock().unwrap().terminal_failure);
            assert_eq!(ledger.summary().input_tokens,24,"{protocol:?}");assert_eq!(ledger.summary().output_tokens,3);
            assert_eq!(ledger.summary().successful_requests,0);assert_eq!(ledger.summary().failed_requests,1);
            assert!(ledger.cache_health_summary().sessions.is_empty());
        }
    }
}

pub(crate) async fn reject_oversized_provider_response(response: reqwest::Response, limit: u64, kiro: bool) -> AttemptFailure {
    let retry = response.extensions().get::<ProviderRetryObservation>().cloned();
    let diagnostic = response.extensions().get::<crate::observability::RequestDiagnostic>().cloned();
    let usage = if kiro {
        match super::adapt::read_kiro_bounded(response, limit).await {
            Ok(bytes) => crate::kiro::buffered_usage(&bytes), Err((_, usage)) => usage,
        }
    } else {
        match super::stream::bounded_json_with_usage(response, limit).await {
            Ok(value) => TokenUsage::from_json(&value), Err((_, usage)) => usage,
        }
    };
    let mut failure = failure_with_provider_metadata(AttemptFailure {
        response: error_response(StatusCode::BAD_GATEWAY, "provider_response_too_large", "provider response exceeds the configured limit"),
        kind: AttemptFailureKind::Retryable,
    }, retry.as_ref(), diagnostic.as_ref());
    failure.response.extensions_mut().insert(crate::observability::ReportedUpstreamUsage(usage));
    failure
}


#[cfg(test)]
mod review16_bounded_attempt_regressions {
    use super::*;
    use super::super::adapt::review16_regressions::{candidate,response,metadata};
    #[tokio::test]
    async fn review16_declared_length_rejections_preserve_usage_and_retries_once() {
        for kiro in [false,true] {
            let prefix=if kiro {metadata()} else {sse("response.completed",&json!({"type":"response.completed","response":{"usage":{"input_tokens":17,"output_tokens":3}}})).into_bytes()};
            let limit=prefix.len() as u64;let mut bytes=prefix;bytes.extend_from_slice(b"excess");
            let failure=reject_oversized_provider_response(response(vec![Ok(Bytes::from(bytes))],StatusCode::OK),limit,kiro).await;
            assert_eq!(failure.response.status(),StatusCode::BAD_GATEWAY);
            let usage=response_body_usage(failure.response).await;assert_eq!(usage.input_tokens,17);assert_eq!(usage.output_tokens,3);assert!(!usage.cache_read_reported);
        }
        let ledger=ObservabilityLedger::new(None);let state=super::review16_physical_guard_regressions::state(&ledger);
        let mut c=candidate();c.provider.limits.request_retries=1;c.provider.limits.request_pacing_ms=0;
        let mut settings = GatewaySettings::default(); settings.providers.push(c.provider.clone());
        let target = format!("{}/{}", c.provider.id, c.upstream_model);
        let c = state.routing.lock().await.candidates_scoped(&settings, &target, None).unwrap().remove(0);
        let sends=std::sync::atomic::AtomicU32::new(0);
        let result=retry_provider_request(&state,&c,false,|| {
            let n=sends.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
            async move {
                if n==0 {
                    let prefix=json!({"usage":{"input_tokens":17,"output_tokens":3}}).to_string();let limit=prefix.len() as u64;
                    Err(reject_oversized_provider_response(response(vec![Ok(Bytes::from(format!("{prefix}excess")))],StatusCode::OK),limit,false).await)
                } else { Ok(response(vec![Ok(Bytes::from(json!({"usage":{"input_tokens":11,"output_tokens":2}}).to_string()))],StatusCode::OK)) }
            }
        }).await.unwrap();
        assert_eq!(sends.load(std::sync::atomic::Ordering::Relaxed),2);
        let retry=result.extensions().get::<ProviderRetryObservation>().unwrap().clone();assert_eq!(retry.usage.input_tokens,17);assert_eq!(retry.additional_sends,1);
        let mut seed=super::super::adapt::review16_regressions::seed(&ledger,&c,false);seed.record_provider_retries(&retry);
        let usage=response_body_usage_from_reqwest(result).await;seed.finish(StatusCode::BAD_GATEWAY,Some("fixture-failure"),usage);
        assert_eq!(ledger.summary().input_tokens,28);assert_eq!(ledger.summary().output_tokens,5);assert!(ledger.cache_health_summary().sessions.is_empty());
    }
    #[tokio::test]
    async fn review16_physical_gemini_io_and_invalid_json_keep_earlier_usage() {
        for mode in ["io","json"] {
            let prefix=sse("message",&json!({"usageMetadata":{"promptTokenCount":17,"candidatesTokenCount":3}}));
            let chunks=if mode=="io" {vec![Ok(Bytes::from(prefix)),Err(std::io::Error::other("fixture"))]} else {vec![Ok(Bytes::from(format!("{prefix}data: {{invalid\n\n")))]};
            let mut source=provider_guard_stream_for_protocol(response(chunks,StatusCode::OK),false,ProviderProtocol::GeminiGenerateContent,4096);
            let mut failed=false;
            while let Some(chunk)=source.next().await {
                if let Err(error)=chunk {
                    let failure=error.get_ref().unwrap().downcast_ref::<ProviderStreamReadFailure>().unwrap();
                    assert_eq!(failure.usage.input_tokens,17);assert_eq!(failure.usage.output_tokens,3);failed=true;
                }
            }
            assert!(failed);
        }
    }
}


#[cfg(test)]
mod review17_buffered_retry_regressions {
    use super::*;
    use super::super::adapt::review16_regressions::{candidate, response, seed};

    fn usage(input: u64, output: u64) -> TokenUsage {
        TokenUsage { input_tokens: input, output_tokens: output,
            total_tokens: input.saturating_add(output), ..Default::default() }
    }
    fn first() -> reqwest::Response {
        let mut response = response(vec![Ok(Bytes::from(json!({"status":"completed","output":[],
            "usage":{"input_tokens":13,"output_tokens":2,"input_tokens_details":{"cached_tokens":3},
                "output_tokens_details":{"reasoning_tokens":1}}}).to_string()))], StatusCode::OK);
        response.extensions_mut().insert(ProviderRetryObservation {
            usage: usage(5, 1), additional_sends: 1, known_billing: Some(usage(13, 2)),
            ..Default::default()
        });
        response
    }
    fn final_value(reported: bool) -> Value {
        let mut value = json!({"status":"completed","output":[{"type":"message","role":"assistant",
            "content":[{"type":"output_text","text":"answer"}]}],
            "usage":{"input_tokens":100000,"output_tokens":7,
                "input_tokens_details":{"cache_creation_tokens":1000},
                "output_tokens_details":{"reasoning_tokens":3}}});
        if reported { value["usage"]["input_tokens_details"]["cached_tokens"] = json!(80000); }
        value
    }
    fn nested(response: &mut reqwest::Response) {
        response.extensions_mut().insert(crate::observability::RequestDiagnostic {
            client_thread_id: Some("01a13ddb-8d21-7cb2-8f00-23a8a09af079".into()), ..Default::default()
        });
        response.extensions_mut().insert(ProviderRetryObservation {
            usage: usage(11, 2), additional_sends: 1, known_billing: Some(usage(100000, 7)),
            ..Default::default()
        });
    }

    #[tokio::test(start_paused = true)]
    async fn review17_empty_retry_decode_limit_and_read_failure_bill_once() {
        for mode in ["trailing", "limit", "io", "invalid"] {
            let prefix = final_value(true).to_string();
            let limit = if mode == "limit" { prefix.len() as u64 } else { 8192 };
            let chunks = match mode {
                "io" => vec![Ok(Bytes::from(prefix)), Err(std::io::Error::other("fixture"))],
                "invalid" => vec![Ok(Bytes::from_static(b"not JSON"))],
                _ => vec![Ok(Bytes::from(format!("{prefix} trailing junk")))],
            };
            let mut last = response(chunks, StatusCode::OK);
            nested(&mut last);
            let mut last = Some(last);
            let result = guard_buffered_empty_completion_response(first(), limit, 1,
                || std::future::ready(Ok(last.take().expect("one physical retry")))).await.unwrap();
            assert_eq!(result.status(), StatusCode::BAD_GATEWAY);
            let billed = result.extensions().get::<EmptyCompletionRecoveryFailure>().unwrap().usage.clone();
            let metadata = result.extensions().get::<ProviderRetryObservation>().unwrap().clone();
            let expected_input = if mode == "invalid" { 29 } else { 100029 };
            let expected_output = if mode == "invalid" { 5 } else { 12 };
            assert_eq!(billed.input_tokens, expected_input, "{mode}");
            assert_eq!(billed.output_tokens, expected_output, "{mode}");
            assert_eq!(billed.total_tokens, expected_input + expected_output);
            assert_eq!(metadata.usage.total_tokens, 0);
            assert_eq!(metadata.known_billing.as_ref().unwrap().total_tokens, billed.total_tokens);
            assert_eq!(metadata.additional_sends, 3);
            if mode != "invalid" {
                assert_eq!(billed.cached_input_tokens, 80003);
                assert_eq!(billed.cache_creation_input_tokens, 1000);
                assert_eq!(billed.reasoning_tokens, 4);
            }
            let ledger = ObservabilityLedger::new(None);
            let mut observation = seed(&ledger, &candidate(), false);
            observation.record_provider_retries(&metadata);
            observation.finish(result.status(), Some(EMPTY_COMPLETION_RETRY_FAILED_CODE), billed.clone());
            assert_eq!(ledger.summary().total_tokens, billed.total_tokens);
            assert_eq!(ledger.summary().failed_requests, 1);
            assert!(ledger.cache_health_summary().sessions.is_empty());
            let value: Value = result.json().await.unwrap();
            assert_eq!(TokenUsage::from_json(&value).total_tokens, billed.total_tokens);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn review17_empty_retry_success_cache_is_only_final_physical_attempt() {
        for reported in [false, true] {
            let mut last = response(vec![Ok(Bytes::from(final_value(reported).to_string()))], StatusCode::OK);
            nested(&mut last);
            let mut last = Some(last);
            let result = guard_buffered_empty_completion_response(first(), 8192, 1,
                || std::future::ready(Ok(last.take().unwrap()))).await.unwrap();
            let cache = result.extensions().get::<crate::observability::CacheObservation>().unwrap();
            assert_eq!(cache.input_tokens, 100000);
            assert_eq!(cache.cached_input_tokens, if reported { 80000 } else { 0 });
            assert_eq!(cache.cache_read_reported, reported);
            let ledger = ObservabilityLedger::new(None);
            let mut observation = seed(&ledger, &candidate(), false);
            observation.record_provider_retries(result.extensions().get::<ProviderRetryObservation>().unwrap());
            observation.record_reqwest_upstream_error(&result);
            let value: Value = result.json().await.unwrap();
            observation.finish(StatusCode::OK, None, TokenUsage::from_json(&value));
            assert_eq!(ledger.summary().input_tokens, 100029);
            assert_eq!(ledger.summary().total_tokens, 100041);
            let snapshot = ledger.cache_health_summary();
            assert_eq!(snapshot.sessions.len(), 1);
            let row = &snapshot.sessions[0];
            assert_eq!(row.input_tokens, 100000);
            assert_eq!(row.successful_requests, 1);
            assert_eq!(row.measured_requests, u64::from(reported));
            assert_eq!(row.unknown_cache_requests, u64::from(!reported));
            assert_eq!(row.cache_hit_percent, reported.then_some(80.0));
        }
    }

    #[tokio::test]
    async fn review17_initial_decode_failure_keeps_prefix_without_retry_or_cache_sample() {
        let prefix = final_value(true).to_string();
        let initial = response(vec![Ok(Bytes::from(format!("{prefix} junk")))], StatusCode::OK);
        let result = guard_buffered_empty_completion_response(initial, 8192, 1,
            || std::future::ready(Err(request_failure("unexpected", "must not retry")))).await.unwrap();
        assert!(result.extensions().get::<EmptyCompletionRecoveryFailure>().is_none());
        let (_, billed) = super::super::stream::bounded_json_with_usage(result, 8192).await.unwrap_err();
        assert_eq!(billed.input_tokens, 100000);
        assert_eq!(billed.output_tokens, 7);
    }
}


#[cfg(test)]
pub(super) mod review18_regressions {
    use super::*;
    pub(crate) fn state() -> GatewayState {
        let mut settings = GatewaySettings::default();
        settings.security.dns_pinning = false;
        let settings = Arc::new(tokio::sync::RwLock::new(settings));
        GatewayState {
            settings: settings.clone(), client: reqwest::Client::builder().no_proxy().build().unwrap(),
            routing: Arc::new(Mutex::new(RoutingRuntime::default())),
            observability: ObservabilityLedger::new(None), video_jobs: Arc::new(Mutex::new(HashMap::new())),
            instance_id: Uuid::new_v4().to_string(), response_state: Arc::new(ResponseStateStore::new(None)),
            memory: Arc::new(MemoryAdmission { settings, inflight: std::sync::atomic::AtomicU32::new(0),
                reserved_bytes: std::sync::atomic::AtomicU64::new(0), rejected: std::sync::atomic::AtomicU64::new(0) }),
            pacing: Arc::new(ProviderPacing::default()),
        }
    }
    pub(crate) async fn registered_candidate(state: &GatewayState, c: RouteCandidate) -> RouteCandidate {
        let target = format!("{}/{}", c.provider.id, c.upstream_model);
        let mut provider = c.provider;
        provider.models = vec![c.upstream_model.clone()];
        provider.capabilities = c.capabilities;
        let mut settings = state.settings.write().await;
        settings.providers = vec![provider];
        let mut selected = state.routing.lock().await.candidates_scoped(&settings, &target, None)
            .expect("fixture route must be selectable").remove(0);
        selected.max_input_tokens = c.max_input_tokens;
        selected.max_output_tokens = c.max_output_tokens;
        selected.context_window = c.context_window;
        selected
    }
    #[tokio::test]
    async fn review19_registered_fixture_admits_real_routing_lease() {
        let state = state();
        let mut c = super::super::adapt::review16_regressions::candidate();
        c.provider.credential.source = CredentialSource::None;
        c.provider.credential.reference = None;
        assert!(state.routing.lock().await.begin_attempt(&c).is_err(),
            "the old fabricated fixture must be rejected before transport");
        let c = registered_candidate(&state, c).await;
        let mut routing = state.routing.lock().await;
        routing.begin_attempt(&c).expect("selected fixture must admit a physical send");
        routing.end_attempt(&c);
    }

    #[tokio::test]
    async fn review19_file_reference_budget_preserves_subagent_tool_result() {
        for protocol in [ProviderProtocol::Responses, ProviderProtocol::ChatCompletions,
            ProviderProtocol::AnthropicMessages, ProviderProtocol::GeminiGenerateContent] {
            let mut c = super::super::adapt::review16_regressions::candidate();
            c.provider.protocol = protocol;
            c.max_input_tokens = Some(1000);
            let mut body = json!({"input":[
                {"role":"user","content":"Inspect this image and retain the result"},
                {"type":"custom_tool_call","call_id":"image-call","name":"view_image","input":"fixture.png"},
                {"type":"custom_tool_call_output","call_id":"image-call","output":[
                    {"type":"input_image","file_id":"file-opaque-reference"},
                    {"type":"input_text","text":"important result text ".repeat(1000)}]},
                {"role":"user","content":"Continue"}]});
            let original = body.clone();
            let estimate = super::super::requests::estimate_input_items(body["input"].as_array().unwrap());
            assert_eq!(estimate.image_count, 1);
            assert_eq!(estimate.image_tokens, 8192);
            assert!(estimate.total_tokens > 1000);
            let error = apply_candidate_model_policy(&mut body, &c, true).unwrap_err();
            assert!(matches!(error, CandidatePolicyError::InputBudget(_)));
            assert_eq!(body, original);
            let response = candidate_policy_error_response(&error);
            let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["error"]["code"], "context_length_exceeded");
            c.max_input_tokens = Some(100000);
            apply_candidate_model_policy(&mut body, &c, true).unwrap();
            assert_eq!(body["input"], original["input"]);
        }
    }

    #[test]
    fn review19_reference_estimates_share_rendered_scope_not_data_fields() {
        for part in [json!({"type":"input_image","file_id":"file-opaque"}),
            json!({"type":"image","source":{"type":"file","file_id":"file-opaque"}}),
            json!({"type":"image","source":{"type":"url","url":"https://example.invalid/image"}}),
            json!({"fileData":{"mimeType":"image/png","fileUri":"gs://fixture/image"}}),
            json!({"file_data":{"file_uri":"gs://fixture/image"}}),
            json!({"type":"image_url","image_url":{"url":"https://example.invalid/image"}})] {
            let body = json!({"input":[{"role":"user","content":[part]}]});
            let estimate = super::super::requests::estimate_input_items(body["input"].as_array().unwrap());
            assert_eq!(estimate.image_count, 1, "{part}");
            assert_eq!(estimate.image_tokens, 8192);
            assert_eq!(crate::translate::count_rendered_sent_images(&body), 1);
            assert_eq!(count_translated_input_images(&body), 0);
        }
        let image = json!({"type":"input_image","file_id":"file-opaque"});
        let body = json!({"tools":[{"parameters":{"const":image}}],"metadata":image,"input":[
            {"type":"custom_tool_call","input":image},
            {"type":"function_call","arguments":image},
            {"role":"user","content":"plain text","metadata":image,"provider_metadata":image},
            {"type":"custom_tool_call_output","output":image},
            {"type":"custom_tool_call_output","output":image.to_string()},
            {"type":"custom_tool_call_output","output":[{"file_id":"ordinary-data"}]},
            {"role":"user","content":[{"fileData":{"mimeType":"application/pdf","fileUri":"gs://fixture/doc"}}]}]});
        assert_eq!(super::super::requests::estimate_input_items(body["input"].as_array().unwrap()).image_count, 0);
        assert_eq!(crate::translate::count_rendered_sent_images(&body), 0);
        // Malformed declared images are protected until explicit source validation rejects them.
        // They are not ordinary tool data and must not enter the text-only shrink path.
        let malformed = json!({"input":[{"role":"user","content":[{"type":"input_image","file_id":""}]}]});
        let protected = super::super::requests::estimate_input_items(malformed["input"].as_array().unwrap());
        assert_eq!(protected.image_count, 1);
        assert_eq!(protected.image_tokens, 8192);
        assert_eq!(crate::translate::count_rendered_sent_images(&malformed), 1);
        assert_eq!(count_translated_input_images(&malformed), 0);
        let inline = json!({"input":[{"role":"user","content":[
            {"type":"input_image","image_url":format!("data:image/jpeg;base64,{}", "YWJj".repeat(10000))}]}]});
        let estimate = super::super::requests::estimate_input_items(inline["input"].as_array().unwrap());
        assert_eq!(estimate.image_count, 1);
        assert!(estimate.text_tokens < 100);
        assert_eq!(count_translated_input_images(&inline), 1);
    }

    pub(crate) struct MockTask(tokio::task::JoinHandle<()>);
    impl Drop for MockTask { fn drop(&mut self) { self.0.abort(); } }
    pub(crate) async fn mock(value: Value, status: StatusCode) -> (String, tokio::sync::mpsc::UnboundedReceiver<Value>, MockTask) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let router = axum::Router::new().fallback(move |Json(body): Json<Value>| {
            let tx = tx.clone(); let value = value.clone();
            async move { tx.send(body).unwrap(); (status, [("x-request-id", "review18-fixture")], Json(value)) }
        });
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap(); });
        (format!("http://{address}/v1"), rx, MockTask(task))
    }
    #[tokio::test]
    async fn review18_url_only_send_wrapper_requires_compaction_without_retry() {
        let error = json!({"error":{"type":"invalid_request_error","message":"too many images"},
            "usage":{"prompt_tokens":100000,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":40000}}});
        let (url, mut received, _task) = mock(error, StatusCode::BAD_REQUEST).await;
        let state = state();
        let mut c = super::super::adapt::review16_regressions::candidate();
        c.provider.base_url = url; c.provider.protocol = ProviderProtocol::ChatCompletions;
        c.provider.credential.source = CredentialSource::None; c.provider.credential.reference = None;
        c.capabilities.vision = true;
        let c = registered_candidate(&state, c).await;
        let mut body = json!({"model":"test","input":[{"role":"user","content":[
            {"type":"input_image","image_url":"https://example.invalid/image.jpg"}]}]});
        let original = body["input"].clone();
        let failure = send_candidate(&state, &mut body, &c, None).await.unwrap_err();
        let wire = match received.try_recv() {
            Ok(wire) => wire,
            Err(error) => {
                let status = failure.response.status();
                let bytes = to_bytes(failure.response.into_body(), 65536).await.unwrap();
                panic!("no physical request: {error}; status={status}; body={}", String::from_utf8_lossy(&bytes));
            }
        };
        assert_eq!(crate::translate::count_rendered_sent_images(&wire), 1);
        assert_eq!(count_translated_input_images(&wire), 0);
        assert!(received.try_recv().is_err());
        assert_eq!(body["input"], original);
        let retry = failure.response.extensions().get::<ProviderRetryObservation>().unwrap();
        assert_eq!(retry.additional_sends, 0); assert_eq!(retry.usage.input_tokens, 100000);
        assert_eq!(retry.usage.output_tokens, 3); assert_eq!(retry.usage.cached_input_tokens, 40000);
        let diagnostic = failure.response.extensions().get::<crate::observability::RequestDiagnostic>().unwrap();
        assert_eq!(diagnostic.upstream_request_id.as_deref(), Some("review18-fixture"));
        assert!(failure.response.extensions().get::<UpstreamErrorDiagnostic>().is_some());
        let bytes = to_bytes(failure.response.into_body(), 65536).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "context_length_exceeded");
    }
    #[tokio::test]
    async fn review18_reference_rejection_preserves_retry_usage_and_diagnostics() {
        let mut response = super::super::adapt::review16_regressions::response(vec![Ok(Bytes::from(
            json!({"error":{"message":"too many images"},"usage":{"input_tokens":12,"output_tokens":3}}).to_string()))], StatusCode::BAD_REQUEST);
        response.extensions_mut().insert(SentImageState { omitted_before_send: 0,
            sent_images: crate::translate::count_rendered_sent_images(&json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.invalid/a.jpg"}}]}]})) });
        response.extensions_mut().insert(ProviderRetryObservation { additional_sends: 2,
            recovery_kinds: vec!["provider-5xx".into()], usage: TokenUsage { input_tokens: 8, output_tokens: 2, total_tokens: 10, ..Default::default() }, ..Default::default() });
        response.extensions_mut().insert(crate::observability::RequestDiagnostic { wire_bytes: 1234, ..Default::default() });
        let failure = require_compaction_on_image_rejection(response, StatusCode::BAD_REQUEST).await.unwrap_err();
        let retry = failure.response.extensions().get::<ProviderRetryObservation>().unwrap();
        assert_eq!(retry.additional_sends, 2); assert_eq!(retry.usage.input_tokens, 20); assert_eq!(retry.usage.output_tokens, 5);
        assert_eq!(retry.recovery_kinds, ["provider-5xx"]);
        assert_eq!(failure.response.extensions().get::<crate::observability::RequestDiagnostic>().unwrap().wire_bytes, 1234);
        assert!(failure.response.extensions().get::<UpstreamErrorDiagnostic>().is_some());
    }
}
