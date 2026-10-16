//! Persistent diagnostics must never contain prompts, tool arguments or credentials.
//! One bounded exception: `HeadChange` keeps a short instruction prefix and tool
//! identifiers when a thread's request head changes, to identify helper requests.
use super::*;
use crate::observability::{ClientKind, DynamicToolStats, HeadChange, RequestDiagnostic};
use sha2::{Digest, Sha256};

pub(super) fn client_thread_id(headers: Option<&HeaderMap>) -> Option<String> {
    headers.and_then(|headers| ["session_id", "session-id", "thread-id"].iter().find_map(|name| {
        headers.get(*name).and_then(|v| v.to_str().ok())
            .and_then(|v| uuid::Uuid::parse_str(v).ok()).map(|id| id.to_string())
    }))
}

pub(super) fn attach_wire_diagnostic(
    response: &mut reqwest::Response, body: &Value, bytes: usize, headers: Option<&HeaderMap>,
) {
    let mut diagnostic = wire_request_diagnostic(body, bytes, headers);
    record_response_headers(&mut diagnostic, response);
    response.extensions_mut().insert(diagnostic);
}

pub(super) async fn send_with_wire_diagnostic(
    request: reqwest::RequestBuilder,
    serialized: Vec<u8>,
    body: &Value,
    headers: Option<&HeaderMap>,
) -> Result<reqwest::Response, reqwest::Error> {
    let bytes = serialized.len();
    let (client, request) = request.body(serialized).build_split();
    let request = request?;
    let binding = resolved_auth_binding(&request);
    let mut response = client.execute(request).await?;
    attach_wire_diagnostic(&mut response, body, bytes, headers);
    if let Some(diagnostic) = response.extensions_mut().get_mut::<RequestDiagnostic>() {
        diagnostic.auth_binding = Some(binding);
    }
    Ok(response)
}

pub(super) fn wire_request_diagnostic(
    body: &Value,
    bytes: usize,
    headers: Option<&HeaderMap>,
) -> RequestDiagnostic {
    let replayed = headers
        .and_then(|headers| headers.get("x-codetas-ws-history-replayed"))
        .and_then(|value| match value.as_bytes() {
            b"true" => Some(true),
            b"false" => Some(false),
            _ => None,
        });
    RequestDiagnostic {
        wire_model: body.get("model")
            .or_else(|| body.pointer("/conversationState/currentMessage/userInputMessage/modelId"))
            .and_then(Value::as_str).map(str::to_string),
        client_thread_id: client_thread_id(headers),
        wire_bytes: bytes as u64,
        input_items: body
            .get("input")
            .or_else(|| body.get("messages"))
            .and_then(Value::as_array)
            .map_or(0, |items| items.len() as u64),
        tool_count: body
            .get("tools")
            .and_then(Value::as_array)
            .map_or(0, |tools| tools.len() as u64),
        store: body.get("store").and_then(Value::as_bool),
        has_previous_response_id: body
            .get("previous_response_id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty()),
        client_transport: if replayed.is_some() {
            "websocket"
        } else {
            "http"
        }
        .into(),
        upstream_transport: "http".into(),
        websocket_history_replayed: replayed,
        head_change: client_thread_id(headers).and_then(|thread| track_head_change(&thread, body)),
        client_kind: client_kind(headers),
        ..Default::default()
    }
}

/// Codex marks helper and sub-agent requests with headers. Only identifier-shaped
/// values are kept; the turn metadata is reduced to three fields and its key names.
pub(super) fn client_kind(headers: Option<&HeaderMap>) -> Option<ClientKind> {
    let headers = headers?;
    let text = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    let mut kind = ClientKind {
        subagent: text("x-openai-subagent").and_then(identifier),
        ..Default::default()
    };
    if let Some(metadata) = text("x-codex-turn-metadata")
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .filter(Value::is_object)
    {
        let field = |key: &str| metadata.get(key).and_then(Value::as_str).and_then(identifier);
        kind.subagent_kind = field("subagent_kind");
        kind.thread_source = field("thread_source");
        kind.turn_trigger = field("turn_trigger");
        kind.turn_metadata_keys = metadata
            .as_object()
            .into_iter()
            .flat_map(|object| object.keys())
            .filter_map(|key| identifier(key))
            .take(24)
            .collect();
    }
    (kind.subagent.is_some()
        || kind.subagent_kind.is_some()
        || kind.thread_source.is_some()
        || kind.turn_trigger.is_some()
        || !kind.turn_metadata_keys.is_empty())
    .then_some(kind)
}

/// Counts the history items that deliver tools after the base `tools` list. The
/// wire tool count is the base list plus these, so it changes with the history.
pub(super) fn dynamic_tool_stats(source: &Value) -> Option<DynamicToolStats> {
    let items = source.get("input").and_then(Value::as_array)?;
    let mut stats = DynamicToolStats::default();
    for item in items {
        if matches!(
            item.get("type").and_then(Value::as_str),
            Some("additional_tools" | "tool_search_output")
        ) {
            stats.items += 1;
            stats.tools += item
                .get("tools")
                .and_then(Value::as_array)
                .map_or(0, |tools| tools.len() as u64);
        }
    }
    Some(stats)
}

const HEAD_TRACKED_THREADS: usize = 256;
const HEAD_NAME_LIMIT: usize = 24;
const INSTRUCTION_HEAD_CHARS: usize = 120;

struct HeadSnapshot {
    tools: std::collections::BTreeSet<String>,
    instruction_hash: String,
}

static HEAD_SNAPSHOTS: std::sync::Mutex<std::collections::VecDeque<(String, HeadSnapshot)>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

fn tool_names(body: &Value) -> std::collections::BTreeSet<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            tool.get("name")
                .or_else(|| tool.pointer("/function/name"))
                .and_then(Value::as_str)
        })
        .map(|name| identifier(name).unwrap_or_else(|| "(other)".into()))
        .collect()
}

fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .filter(|text| !text.trim().is_empty())
            .last()
            .map(str::to_string),
        _ => None,
    }
}

/// The instruction text of the final wire body, whichever protocol shape it uses.
fn instruction_text(body: &Value) -> Option<String> {
    if let Some(text) = body.get("instructions").and_then(text_of) {
        return Some(text);
    }
    if let Some(text) = body.get("system").and_then(text_of) {
        return Some(text);
    }
    body.get("messages")
        .and_then(Value::as_array)?
        .iter()
        .find(|message| matches!(message.get("role").and_then(Value::as_str), Some("system" | "developer")))
        .and_then(|message| message.get("content"))
        .and_then(text_of)
}

fn instruction_prefix(text: &str) -> String {
    let head: String = text
        .chars()
        .take(INSTRUCTION_HEAD_CHARS)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let head = head.trim().to_string();
    // A prefix is never allowed to carry something that looks like a credential.
    if ["sk-", "eyJ", "Bearer ", "api_key", "token="].iter().any(|marker| head.contains(marker)) {
        "(redacted)".into()
    } else {
        head
    }
}

/// Compares this request's head with the previous request of the same thread.
/// Returns `None` when nothing changed, so a steady conversation adds no data.
fn track_head_change(thread_id: &str, body: &Value) -> Option<HeadChange> {
    let tools = tool_names(body);
    let instruction = instruction_text(body);
    let instruction_hash = instruction
        .as_deref()
        .map(|text| format!("{:x}", Sha256::digest(text.as_bytes()))[..16].to_string())
        .unwrap_or_default();
    let head = instruction.as_deref().map(instruction_prefix).filter(|head| !head.is_empty());
    let mut snapshots = HEAD_SNAPSHOTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = snapshots
        .iter()
        .position(|(id, _)| id == thread_id)
        .and_then(|index| snapshots.remove(index))
        .map(|(_, snapshot)| snapshot);
    snapshots.push_back((
        thread_id.to_string(),
        HeadSnapshot { tools: tools.clone(), instruction_hash: instruction_hash.clone() },
    ));
    while snapshots.len() > HEAD_TRACKED_THREADS {
        snapshots.pop_front();
    }
    let Some(previous) = previous else {
        return Some(HeadChange { first_in_thread: true, instruction_head: head, ..Default::default() });
    };
    let added: Vec<String> = tools.difference(&previous.tools).cloned().collect();
    let removed: Vec<String> = previous.tools.difference(&tools).cloned().collect();
    let instruction_changed = previous.instruction_hash != instruction_hash;
    if added.is_empty() && removed.is_empty() && !instruction_changed {
        return None;
    }
    Some(HeadChange {
        first_in_thread: false,
        instruction_changed,
        instruction_head: instruction_changed.then_some(head).flatten(),
        tools_added_total: added.len() as u64,
        tools_removed_total: removed.len() as u64,
        tools_added: added.into_iter().take(HEAD_NAME_LIMIT).collect(),
        tools_removed: removed.into_iter().take(HEAD_NAME_LIMIT).collect(),
    })
}

// IDs and error identifiers only: reject arbitrary prose, oversized values and secrets.
fn identifier(value: &str) -> Option<String> {
    (!value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte))
        && !value.starts_with("sk-")
        && !value.starts_with("eyJ"))
    .then(|| value.to_string())
}

fn error_identifier(value: &str) -> Option<String> {
    // Error fields are not inherently trusted: some providers put echoed input
    // there too. Unknown values remain distinguishable by the error fingerprint.
    matches!(
        value,
        "server_overloaded"
            | "overloaded_error"
            | "server_error"
            | "internal_error"
            | "internal_server_error"
            | "rate_limit_exceeded"
            | "rate_limit_error"
            | "insufficient_quota"
            | "quota_exceeded"
            | "invalid_request_error"
            | "invalid_request"
            | "authentication_error"
            | "permission_error"
            | "context_length_exceeded"
            | "context_window_exceeded"
            | "max_output_tokens"
            | "content_filter"
            | "model_not_found"
            | "empty_response"
            | "invalid_tool_call"
            | "empty_completion_retry_failed"
    )
    .then(|| value.to_string())
}

pub(super) fn record_response_headers(
    diagnostic: &mut RequestDiagnostic,
    response: &reqwest::Response,
) {
    diagnostic.upstream_status_code = response.status().as_u16();
    diagnostic.upstream_request_id = [
        "x-request-id",
        "x-requestid",
        "x-msh-request-id",
        "x-kimi-request-id",
    ]
    .iter()
    .find_map(|name| {
        response
            .headers()
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .and_then(identifier)
    });
}

pub(super) fn stream_failure_diagnostic(
    response: &Value,
    request: Option<&RequestDiagnostic>,
) -> Option<UpstreamErrorDiagnostic> {
    if !matches!(
        response.get("status").and_then(Value::as_str),
        Some("failed" | "incomplete")
    ) {
        return None;
    }
    let error = response
        .get("error")
        .filter(|value| value.is_object())
        .or_else(|| {
            response
                .get("incomplete_details")
                .filter(|value| value.is_object())
        });
    let code = error
        .and_then(|error| error.get("code").or_else(|| error.get("reason")))
        .and_then(Value::as_str)
        .and_then(error_identifier);
    let error_type = error
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .and_then(error_identifier);
    // Provider messages can echo user input or credentials. Persist only recognized
    // exact boilerplate, never an arbitrary message or response body.
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .filter(|message| {
            matches!(
                *message,
                "Selected model is at capacity. Please try a different model."
                    | "Our servers are currently overloaded. Please try again later."
                    | "The server is currently overloaded. Please try again later."
            )
        })
        .map(str::to_string);
    let bytes = serde_json::to_vec(&error).unwrap_or_default();
    Some(UpstreamErrorDiagnostic {
        status_code: request.map_or(200, |request| request.upstream_status_code),
        content_type: Some("text/event-stream".into()),
        request_id: request.and_then(|request| request.upstream_request_id.clone()),
        retry_after: None,
        error_type,
        error_code: code,
        error_message: message,
        body_bytes: bytes.len() as u64,
        body_sha256: format!("{:x}", Sha256::digest(&bytes)),
        body_truncated: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn capacity_diagnostic_survives_sse_and_usage_only_persistence() {
        // Exercise both native SSE adapters. Diagnostics must be committed before
        // yielding the terminal frame because Codex can close immediately after it.
        for repair in [false, true] {
            let directory =
                std::env::temp_dir().join(format!("codetas-capacity-{}", uuid::Uuid::new_v4()));
            let ledger = ObservabilityLedger::new(Some(directory.clone()));
            let settings = ObservabilitySettings {
                request_log: false,
                usage_log: true,
                ..Default::default()
            };
            let terminal = json!({"type":"response.failed", "response":{"id":"resp_capacity", "status":"failed", "error":{"code":"server_overloaded", "message":"Selected model is at capacity. Please try a different model."}, "output":[]}});
            let upstream = Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .header("x-request-id", "req_capacity")
                .body(reqwest::Body::from(sse("response.failed", &terminal)))
                .unwrap();
            let mut upstream = reqwest::Response::from(upstream);
            let mut diagnostic =
                wire_request_diagnostic(&json!({"input":[], "store":false}), 456, None);
            record_response_headers(&mut diagnostic, &upstream);
            upstream.extensions_mut().insert(diagnostic);
            let mut observation = ObservationSeed::without_candidate(
                ledger.clone(),
                settings,
                "local_request".into(),
                "gpt-6.1-sol",
                true,
                Instant::now(),
            );
            observation.record_reqwest_upstream_error(&upstream);
            let response = super::super::stream::passthrough_stream(
                upstream,
                65536,
                Duration::from_secs(1),
                observation,
                Arc::new(ResponseStateStore::new(None)),
                json!({"input":[]}),
                false,
                repair,
            );
            let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
            assert!(String::from_utf8_lossy(&bytes).contains("server_overloaded"));
            ledger.flush().await;
            let events = crate::observability::read_recent_observability_events(&directory, 0, 10);
            assert_eq!(events.len(), 1);
            let event = &events[0];
            assert_eq!(
                event.failure_category.as_deref(),
                Some("provider_completion_failed")
            );
            assert_eq!(event.status_code, 502);
            let request = event.request_diagnostic.as_ref().unwrap();
            assert_eq!(request.wire_bytes, 456);
            assert_eq!(request.upstream_status_code, 200);
            let error = event.upstream_error.as_ref().unwrap();
            assert_eq!(error.error_code.as_deref(), Some("server_overloaded"));
            assert_eq!(error.request_id.as_deref(), Some("req_capacity"));
            // Historical ledgers without the new field must still deserialize.
            let mut old = serde_json::to_value(event).unwrap();
            old.as_object_mut().unwrap().remove("requestDiagnostic");
            let old: ObservationEvent = serde_json::from_value(old).unwrap();
            assert!(old.request_diagnostic.is_none());
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn capacity_diagnostic_records_wire_shape_without_content() {
        let secret = "Bearer secret-prompt-and-tool-arguments";
        let body = json!({"store": false, "input": [{"content": secret}], "tools": [{"description": secret}], "previous_response_id": "resp_secret"});
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer secret"));
        headers.insert(
            "x-codetas-ws-history-replayed",
            HeaderValue::from_static("true"),
        );
        headers.insert(
            "session_id",
            HeaderValue::from_static("01a13d21-e1e8-7fa3-ac98-e5893baf792a"),
        );
        let diagnostic = wire_request_diagnostic(&body, 1234, Some(&headers));
        assert_eq!(diagnostic.wire_bytes, 1234);
        assert_eq!(diagnostic.input_items, 1);
        assert_eq!(diagnostic.tool_count, 1);
        assert_eq!(diagnostic.store, Some(false));
        assert!(diagnostic.has_previous_response_id);
        assert_eq!(diagnostic.websocket_history_replayed, Some(true));
        assert_eq!(
            diagnostic.client_thread_id.as_deref(),
            Some("01a13d21-e1e8-7fa3-ac98-e5893baf792a")
        );
        let serialized = serde_json::to_string(&diagnostic).unwrap();
        assert!(!serialized.contains("secret"));
    }

    #[test]
    fn head_change_reports_tool_name_diff_and_instruction_prefix_only_on_change() {
        let thread = "01a1433c-0000-7000-8000-000000000001";
        let mut headers = HeaderMap::new();
        headers.insert("session_id", HeaderValue::from_static(thread));
        let main = json!({
            "instructions": "You are Codex, based on GPT-5.\nWork carefully.",
            "tools": [{"name": "exec"}, {"name": "view_image"}, {"name": "list_projects"}],
            "input": [{"content": "private conversation text"}]
        });

        // First request: instruction prefix only, no tool name dump.
        let first = wire_request_diagnostic(&main, 1, Some(&headers)).head_change.expect("first");
        assert!(first.first_in_thread);
        assert_eq!(first.instruction_head.as_deref(), Some("You are Codex, based on GPT-5. Work carefully."));
        assert!(first.tools_added.is_empty() && first.tools_removed.is_empty());

        // Identical head: nothing is recorded.
        assert!(wire_request_diagnostic(&main, 1, Some(&headers)).head_change.is_none());

        // A helper request with other instructions and fewer tools.
        let helper = json!({
            "instructions": "Pick the skills that match the request.",
            "tools": [{"name": "exec"}, {"name": "view_image"}],
            "input": [{"content": "private conversation text"}]
        });
        let change = wire_request_diagnostic(&helper, 1, Some(&headers)).head_change.expect("helper");
        assert!(change.instruction_changed && !change.first_in_thread);
        assert_eq!(change.instruction_head.as_deref(), Some("Pick the skills that match the request."));
        assert_eq!(change.tools_removed, ["list_projects"]);
        assert_eq!(change.tools_removed_total, 1);
        assert!(change.tools_added.is_empty());

        // Returning to the main head reports the tool coming back.
        let back = wire_request_diagnostic(&main, 1, Some(&headers)).head_change.expect("back");
        assert_eq!(back.tools_added, ["list_projects"]);

        // Conversation content never reaches the persisted diagnostic.
        let serialized = serde_json::to_string(&back).unwrap();
        assert!(!serialized.contains("private conversation text"));

        // Requests without a client thread are not tracked.
        assert!(wire_request_diagnostic(&main, 1, None).head_change.is_none());
    }

    #[test]
    fn head_change_bounds_and_redacts_what_it_records() {
        let thread = "01a1433c-0000-7000-8000-000000000002";
        let mut headers = HeaderMap::new();
        headers.insert("session_id", HeaderValue::from_static(thread));
        let secret = json!({"instructions": "Use Bearer sk-live-secret for everything", "tools": []});
        let change = wire_request_diagnostic(&secret, 1, Some(&headers)).head_change.expect("first");
        assert_eq!(change.instruction_head.as_deref(), Some("(redacted)"));

        let long = "x".repeat(500);
        let many: Vec<Value> = (0..60).map(|i| json!({"name": format!("tool_{i}")})).collect();
        let wide = json!({"system": [{"type": "text", "text": "identity"}, {"type": "text", "text": long}], "tools": many});
        let change = wire_request_diagnostic(&wide, 1, Some(&headers)).head_change.expect("wide");
        assert_eq!(change.instruction_head.as_deref().map(str::len), Some(120));
        assert_eq!(change.tools_added_total, 60);
        assert_eq!(change.tools_added.len(), 24);

        // Names that are not plain identifiers are masked, not stored.
        let odd = json!({"system": "same", "tools": [{"name": "has space and text"}]});
        let change = wire_request_diagnostic(&odd, 1, Some(&headers)).head_change.expect("odd");
        assert!(change.tools_added.iter().all(|name| name == "(other)" || !name.contains(' ')));
    }

    #[test]
    fn client_kind_keeps_identifiers_only() {
        let mut headers = HeaderMap::new();
        headers.insert("x-openai-subagent", HeaderValue::from_static("memory_consolidation"));
        headers.insert(
            "x-codex-turn-metadata",
            HeaderValue::from_static(
                r#"{"thread_source":"user","turn_trigger":"queue","subagent_kind":"review","cwd":"/secret path/project","prompt":"do not store"}"#,
            ),
        );
        let kind = client_kind(Some(&headers)).expect("kind");
        assert_eq!(kind.subagent.as_deref(), Some("memory_consolidation"));
        assert_eq!(kind.thread_source.as_deref(), Some("user"));
        assert_eq!(kind.turn_trigger.as_deref(), Some("queue"));
        assert_eq!(kind.subagent_kind.as_deref(), Some("review"));
        assert!(kind.turn_metadata_keys.iter().any(|key| key == "cwd"));
        let serialized = serde_json::to_string(&kind).unwrap();
        assert!(!serialized.contains("secret"));
        assert!(!serialized.contains("do not store"));

        // Values that are not identifiers are dropped, and no headers means no record.
        let mut odd = HeaderMap::new();
        odd.insert("x-openai-subagent", HeaderValue::from_static("has space"));
        assert!(client_kind(Some(&odd)).is_none());
        assert!(client_kind(Some(&HeaderMap::new())).is_none());
        assert!(client_kind(None).is_none());

        // Not JSON: nothing is guessed from it.
        let mut raw = HeaderMap::new();
        raw.insert("x-codex-turn-metadata", HeaderValue::from_static("not json"));
        assert!(client_kind(Some(&raw)).is_none());
    }

    #[test]
    fn dynamic_tool_stats_counts_history_delivered_tools() {
        let none = json!({"input": [{"type": "message"}], "tools": [{"name": "a"}]});
        let stats = dynamic_tool_stats(&none).expect("stats");
        assert_eq!((stats.items, stats.tools), (0, 0));
        let some = json!({"input": [
            {"type": "additional_tools", "tools": [{"name": "x"}, {"name": "y"}]},
            {"type": "message"},
            {"type": "tool_search_output", "tools": [{"name": "z"}]}
        ]});
        let stats = dynamic_tool_stats(&some).expect("stats");
        assert_eq!((stats.items, stats.tools), (2, 3));
        assert!(dynamic_tool_stats(&json!({"messages": []})).is_none());
    }

    #[test]
    fn capacity_diagnostic_preserves_code_and_actual_http_status() {
        let response = json!({"status":"failed", "error":{"code":"server_overloaded", "type":"server_error", "message":"Selected model is at capacity. Please try a different model."}, "output":[{"content":"secret"}]});
        let request = RequestDiagnostic {
            upstream_status_code: 200,
            upstream_request_id: Some("req_123".into()),
            ..Default::default()
        };
        let diagnostic = stream_failure_diagnostic(&response, Some(&request)).unwrap();
        assert_eq!(diagnostic.status_code, 200);
        assert_eq!(diagnostic.error_code.as_deref(), Some("server_overloaded"));
        assert_eq!(diagnostic.request_id.as_deref(), Some("req_123"));
        assert!(diagnostic.error_message.is_some());
        assert!(!serde_json::to_string(&diagnostic)
            .unwrap()
            .contains("secret"));
        assert!(stream_failure_diagnostic(&json!({"status":"completed"}), None).is_none());
    }

    #[test]
    fn capacity_diagnostic_never_persists_arbitrary_provider_message() {
        let diagnostic = stream_failure_diagnostic(&json!({"status":"failed", "error":{"code":"sk-secret", "type":"Bearer credential", "message":"prompt echoed: confidential"}}), None).unwrap();
        assert!(diagnostic.error_code.is_none());
        assert!(diagnostic.error_type.is_none());
        assert!(diagnostic.error_message.is_none());
        assert_eq!(diagnostic.body_sha256.len(), 64);
    }
}

#[cfg(test)]
mod special_transport_cache_tests {
    use super::*;
    #[tokio::test]
    async fn kiro_and_copilot_wire_sends_reach_session_monitoring() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let app = Router::new().route("/", post(|| async { Json(json!({
            "usage": {"prompt_tokens": 100_000, "completion_tokens": 1}
        })) }));
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let ledger = ObservabilityLedger::new(None);
        let mut headers = HeaderMap::new();
        headers.insert("session_id", HeaderValue::from_static("01a13ddb-8d21-7cb2-8f00-23a8a09af079"));
        for (provider, body) in [
            ("kiro", json!({"conversationState": {"currentMessage": {"userInputMessage": {"content": "test"}}}})),
            ("copilot", json!({"model": "model", "messages": [{"role": "user", "content": "test"}]})),
        ] {
            for n in 0..20 {
                let bytes = serde_json::to_vec(&body).unwrap(); let size = bytes.len();
                let response = send_with_wire_diagnostic(client.post(&url), bytes, &body, Some(&headers)).await.unwrap();
                let diagnostic = response.extensions().get::<crate::observability::RequestDiagnostic>().unwrap();
                assert_eq!(diagnostic.client_thread_id.as_deref(), Some("01a13ddb-8d21-7cb2-8f00-23a8a09af079"));
                assert_eq!(diagnostic.wire_bytes, size as u64);
                assert_eq!(diagnostic.upstream_status_code, 200);
                let mut observation = ObservationSeed::without_candidate(ledger.clone(), ObservabilitySettings::default(),
                    format!("{provider}-{n}"), "test/model", false, Instant::now());
                observation.provider_id = Some(provider.into()); observation.upstream_model = Some("model".into());
                observation.send_count = 1; observation.record_reqwest_upstream_error(&response);
                let value: Value = response.json().await.unwrap();
                observation.finish(StatusCode::OK, None, TokenUsage::from_json(&value));
            }
        }
        let rows = ledger.cache_health_summary().sessions;
        assert_eq!(rows.len(), 2);
        for row in rows { assert_eq!(row.successful_requests, 20); assert_eq!(row.unknown_cache_requests, 20); }
        task.abort();
    }
}

/// Hash stable authentication transport, never request-specific signatures/nonces.
/// Unknown custom auth headers remain binding inputs; raw values never leave here.
pub(super) fn resolved_auth_binding(request: &reqwest::Request) -> String {
    let mut fields = Vec::<(String, Vec<u8>)>::new();
    for (name, value) in request.headers() {
        let name = name.as_str();
        if matches!(name, "content-type" | "content-length" | "accept" | "accept-encoding" | "user-agent"
            | "anthropic-version" | "anthropic-beta" | "amz-sdk-request" | "amz-sdk-invocation-id"
            | "x-amz-date" | "date" | "traceparent" | "tracestate" | "session_id" | "session-id" | "thread-id"
            | "x-codex-turn-state" | "x-codex-turn-metadata" | "x-codex-installation-id"
            | "x-codetas-ws-history-replayed" | "x-requestid" | "x-correlation-id")
            || name.contains("request-id") || name.contains("nonce") || name.contains("signature") { continue; }
        let bytes = if name == "authorization" {
            stable_authorization(value.as_bytes())
        } else { value.as_bytes().to_vec() };
        fields.push((name.to_string(), bytes));
    }
    for (key, value) in request.url().query_pairs() {
        let lower = key.to_ascii_lowercase();
        if lower.contains("signature") || lower.contains("nonce") || matches!(lower.as_str(), "x-amz-date" | "x-amz-expires" | "alt" | "api-version") { continue; }
        let value = if lower == "x-amz-credential" { value.split('/').next().unwrap_or("").as_bytes().to_vec() } else { value.as_bytes().to_vec() };
        fields.push((format!("query:{key}"), value));
    }
    fields.sort();
    let mut hash = Sha256::new();
    hash.update(b"codetas-resolved-auth-v1\0");
    // Authority separates identical credentials used against different backends.
    hash.update(request.url().origin().ascii_serialization().as_bytes());
    for (name, value) in fields {
        for bytes in [name.as_bytes(), value.as_slice()] {
            hash.update((bytes.len() as u64).to_be_bytes()); hash.update(bytes);
        }
    }
    format!("{:x}", hash.finalize())
}
fn stable_authorization(value: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(value) else { return value.to_vec(); };
    if text.starts_with("AWS4-HMAC-SHA256 ") || text.starts_with("AWS4-ECDSA-P256-SHA256 ") {
        if let Some(id) = text.split("Credential=").nth(1).and_then(|s| s.split('/').next()) {
            return format!("{} {id}", text.split_whitespace().next().unwrap()).into_bytes();
        }
    }
    value.to_vec()
}

#[cfg(test)]
mod review14_auth_regressions {
    use super::*;
    #[test]
    fn review14_auth_binding_is_secret_free_stable_and_rotates_with_auth() {
        let client=reqwest::Client::new();
        for name in ["authorization","x-api-key","api-key","custom-auth","x-goog-api-key"] {
            let a=client.post("http://127.0.0.1:1").header(name,"fixture-account-a").header("amz-sdk-invocation-id","nonce-one").build().unwrap();
            let b=client.post("http://127.0.0.1:1").header(name,"fixture-account-a").header("amz-sdk-invocation-id","nonce-two").build().unwrap();
            let c=client.post("http://127.0.0.1:1").header(name,"fixture-account-b").build().unwrap();
            assert_eq!(resolved_auth_binding(&a),resolved_auth_binding(&b));
            assert_ne!(resolved_auth_binding(&a),resolved_auth_binding(&c));
            let digest=resolved_auth_binding(&a); assert_eq!(digest.len(),64); assert!(digest.bytes().all(|b|b.is_ascii_hexdigit()));
        }
        let signed=|date:&str,sig:&str|client.post("http://127.0.0.1:1").header("authorization",format!("AWS4-HMAC-SHA256 Credential=account/{date}/region/service/aws4_request, SignedHeaders=host, Signature={sig}")).header("x-amz-date",date).build().unwrap();
        assert_eq!(resolved_auth_binding(&signed("20261016","one")),resolved_auth_binding(&signed("20261017","two")));
        let query=|key:&str|client.post("http://127.0.0.1:1").query(&[("key",key)]).build().unwrap();
        assert_ne!(resolved_auth_binding(&query("a")),resolved_auth_binding(&query("b")));
    }
    #[test]
    fn review14_legacy_diagnostic_defaults_binding() {
        let mut json=serde_json::to_value(RequestDiagnostic::default()).unwrap();
        json.as_object_mut().unwrap().remove("authBinding");
        assert!(serde_json::from_value::<RequestDiagnostic>(json).unwrap().auth_binding.is_none());
    }
}
