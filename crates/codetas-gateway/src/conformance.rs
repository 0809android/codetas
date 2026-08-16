use crate::{
    anthropic::{anthropic_to_response, responses_to_anthropic},
    compat::ResponsesItemIdRepair,
    config::{
        effective_model_capabilities, CredentialSource, GatewaySettings, ProviderDefinition,
        ProviderProtocol, ProviderTransport,
    },
    gemini::{gemini_to_response, responses_to_gemini},
    routing::RouteCandidate,
    server::{
        apply_provider_request_compatibility, apply_provider_wire_compatibility,
        completion_is_empty, drain_sse_values, empty_completion_retry_enabled,
        model_matches_any, reserve_provider_start, should_retry_rate_limit,
        tolerated_eof_delimiter, ProviderPacingState, ResponsesSnapshotAccumulator,
    },
    translate::responses_to_chat,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ConformanceExpectation {
    Accept,
    Reject,
}

#[derive(Clone, Copy, Debug)]
pub struct ConformanceFixture {
    pub id: &'static str,
    pub expectation: ConformanceExpectation,
    pub description: &'static str,
}

pub const CONFORMANCE_FIXTURES: &[ConformanceFixture] = &[
    ConformanceFixture { id: "basic-text", expectation: ConformanceExpectation::Accept, description: "A minimal text request is accepted." },
    ConformanceFixture { id: "custom-tool-roundtrip", expectation: ConformanceExpectation::Accept, description: "Custom/freeform tools retain their typed payload." },
    ConformanceFixture { id: "tool-search-roundtrip", expectation: ConformanceExpectation::Accept, description: "tool_search calls and outputs retain namespace and identity." },
    ConformanceFixture { id: "mcp-namespace-roundtrip", expectation: ConformanceExpectation::Accept, description: "MCP namespaces survive adapter translation." },
    ConformanceFixture { id: "opaque-metadata-roundtrip", expectation: ConformanceExpectation::Accept, description: "Provider-owned opaque metadata survives the supported adapter path." },
    ConformanceFixture { id: "reasoning-signature-roundtrip", expectation: ConformanceExpectation::Accept, description: "Provider-signed reasoning metadata survives replay." },
    ConformanceFixture { id: "structured-output", expectation: ConformanceExpectation::Accept, description: "JSON schema output is forwarded only when supported." },
    ConformanceFixture { id: "service-tier", expectation: ConformanceExpectation::Accept, description: "Service tier is forwarded only when supported." },
    ConformanceFixture { id: "snapshot-repair", expectation: ConformanceExpectation::Accept, description: "Added items and deltas repair an incomplete terminal snapshot." },
    ConformanceFixture { id: "anthropic-eof", expectation: ConformanceExpectation::Accept, description: "A complete undelimited final Anthropic SSE frame is parsed while truncated JSON fails." },
    ConformanceFixture { id: "terminal-continuation", expectation: ConformanceExpectation::Accept, description: "A terminal tool result gets a continuation guard only for configured models." },
    ConformanceFixture { id: "empty-completion-retry", expectation: ConformanceExpectation::Accept, description: "Empty non-streaming completions retry only for configured models and within the retry budget." },
    ConformanceFixture { id: "request-pacing", expectation: ConformanceExpectation::Accept, description: "Provider start slots are separated while zero pacing stays immediate." },
    ConformanceFixture { id: "retry-429", expectation: ConformanceExpectation::Accept, description: "429 retry policy is explicit, credential-scoped, and respects its attempt limit." },
    ConformanceFixture { id: "malformed-request", expectation: ConformanceExpectation::Reject, description: "A malformed Responses input is rejected by the configured adapter." },
    ConformanceFixture { id: "orphan-tool-output", expectation: ConformanceExpectation::Reject, description: "An orphan tool result is repaired or rejected before upstream delivery." },
    ConformanceFixture { id: "invalid-item-id", expectation: ConformanceExpectation::Reject, description: "Invalid Responses item IDs are repaired before upstream delivery." },
];

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ConformanceStatus {
    Pass,
    Fail,
    Skip,
}

/// Executable, content-free adapter fixtures used by CI. These snapshots are deliberately
/// protocol-scoped: every provider preset is exercised through the adapter selected by its
/// registry protocol rather than merely trusting capability declarations.
#[derive(Clone, Copy, Debug)]
pub struct ProtocolConformanceFixture {
    pub id: &'static str,
    pub expectation: ConformanceExpectation,
    pub request_json: &'static str,
}

pub const PROTOCOL_CONFORMANCE_FIXTURES: &[ProtocolConformanceFixture] = &[
    ProtocolConformanceFixture {
        id: "adapter-basic-text",
        expectation: ConformanceExpectation::Accept,
        request_json: r#"{"model":"fixture-model","input":"hello"}"#,
    },
    ProtocolConformanceFixture {
        id: "adapter-tool-roundtrip",
        expectation: ConformanceExpectation::Accept,
        request_json: r#"{"model":"fixture-model","tools":[{"type":"function","name":"lookup","description":"fixture","parameters":{"type":"object","properties":{"q":{"type":"string"}},"required":["q"]}}],"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"lookup"}]},{"type":"function_call","id":"fc_fixture","call_id":"call_fixture","name":"lookup","arguments":"{\"q\":\"x\"}"},{"type":"function_call_output","call_id":"call_fixture","output":"ok"}]}"#,
    },
    ProtocolConformanceFixture {
        id: "adapter-custom-tool-roundtrip",
        expectation: ConformanceExpectation::Accept,
        request_json: r#"{"model":"fixture-model","tools":[{"type":"custom","name":"exec","description":"fixture","format":{"type":"text"}}],"input":[{"type":"custom_tool_call","id":"ct_fixture","call_id":"call_custom","name":"exec","input":{"command":"pwd"}},{"type":"custom_tool_call_output","call_id":"call_custom","output":"ok"}]}"#,
    },
    ProtocolConformanceFixture {
        id: "adapter-tool-search-roundtrip",
        expectation: ConformanceExpectation::Accept,
        request_json: r#"{"model":"fixture-model","tools":[{"type":"tool_search","namespace":"mcp__fixture"}],"tool_choice":{"type":"tool_search"},"input":[{"type":"tool_search_call","id":"ts_fixture","call_id":"call_search","namespace":"mcp__fixture","arguments":"{\"query\":\"lookup\"}"},{"type":"tool_search_output","call_id":"call_search","output":{"tools":[{"name":"mcp__fixture__lookup"}]}}]}"#,
    },
    ProtocolConformanceFixture {
        id: "adapter-structured-output",
        expectation: ConformanceExpectation::Accept,
        request_json: r#"{"model":"fixture-model","input":"json","text":{"format":{"type":"json_schema","name":"fixture","schema":{"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"]},"strict":true}}}"#,
    },
    ProtocolConformanceFixture {
        id: "adapter-malformed-item",
        expectation: ConformanceExpectation::Reject,
        request_json: r#"{"model":"fixture-model","input":42}"#,
    },
];

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompatibilityResultRow {
    pub provider_id: String,
    pub protocol: ProviderProtocol,
    pub fixture_id: String,
    pub expectation: ConformanceExpectation,
    pub status: ConformanceStatus,
    pub supported: bool,
    pub configured: bool,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompatibilityLabReport {
    pub generated_from_registry_revision: u32,
    pub read_only: bool,
    pub rows: Vec<CompatibilityResultRow>,
}

pub fn compatibility_lab_report(settings: &GatewaySettings) -> CompatibilityLabReport {
    let mut rows = Vec::new();
    for provider in &settings.providers {
        for fixture in CONFORMANCE_FIXTURES {
            let (status, reason) = execute_fixture(settings, provider, fixture.id);
            rows.push(CompatibilityResultRow {
                provider_id: provider.id.clone(),
                protocol: provider.protocol,
                fixture_id: fixture.id.into(),
                expectation: fixture.expectation,
                status,
                supported: status == ConformanceStatus::Pass,
                configured: provider.enabled,
                reason,
            });
        }
    }
    CompatibilityLabReport {
        generated_from_registry_revision: settings.registry_revision,
        read_only: true,
        rows,
    }
}

fn execute_fixture(
    settings: &GatewaySettings,
    provider: &ProviderDefinition,
    fixture: &str,
) -> (ConformanceStatus, String) {
    match run_fixture(settings, provider, fixture) {
        Ok(Some(reason)) => (ConformanceStatus::Pass, reason),
        Ok(None) => (
            ConformanceStatus::Skip,
            "not applicable to the configured protocol or capability".into(),
        ),
        Err(reason) => (ConformanceStatus::Fail, reason),
    }
}

fn run_fixture(
    settings: &GatewaySettings,
    provider: &ProviderDefinition,
    fixture: &str,
) -> Result<Option<String>, String> {
    match fixture {
        "basic-text" => {
            let output = translate_request(provider, &json!({"input": "fixture"}))?;
            if output.is_object() {
                Ok(Some("minimal request passed through the configured adapter".into()))
            } else {
                Err("adapter returned a non-object request".into())
            }
        }
        "custom-tool-roundtrip" => tool_roundtrip(settings, provider, true, false),
        "tool-search-roundtrip" => tool_roundtrip(settings, provider, false, true),
        "mcp-namespace-roundtrip" => mcp_namespace_roundtrip(settings, provider),
        "opaque-metadata-roundtrip" => opaque_metadata_roundtrip(settings, provider),
        "reasoning-signature-roundtrip" => reasoning_signature_roundtrip(provider),
        "structured-output" => structured_output_fixture(settings, provider),
        "service-tier" => service_tier_fixture(settings, provider),
        "snapshot-repair" => snapshot_repair_fixture(provider),
        "anthropic-eof" => anthropic_eof_fixture(provider),
        "terminal-continuation" => terminal_continuation_fixture(settings, provider),
        "empty-completion-retry" => empty_completion_retry_fixture(settings, provider),
        "request-pacing" => request_pacing_fixture(provider),
        "retry-429" => retry_429_fixture(provider),
        "malformed-request" => malformed_request_fixture(provider),
        "orphan-tool-output" => orphan_tool_output_fixture(provider),
        "invalid-item-id" => invalid_item_id_fixture(provider),
        _ => Err(format!("unknown compatibility fixture: {fixture}")),
    }
}

fn fixture_model(provider: &ProviderDefinition) -> &str {
    provider
        .default_model
        .as_deref()
        .or_else(|| provider.models.first().map(String::as_str))
        .unwrap_or("fixture-model")
}

fn translate_request(provider: &ProviderDefinition, request: &Value) -> Result<Value, String> {
    let model = fixture_model(provider);
    if provider.transport == ProviderTransport::Kiro {
        return crate::kiro::responses_to_kiro(request, model, provider.kiro_profile_arn.as_deref())
            .map(|(wire, _)| wire);
    }
    match provider.protocol_for_model(model) {
        ProviderProtocol::Responses => {
            validate_responses_request(request)?;
            let mut output = request.clone();
            output["model"] = Value::String(model.to_string());
            Ok(output)
        }
        ProviderProtocol::ChatCompletions => responses_to_chat(request, model),
        ProviderProtocol::AnthropicMessages => responses_to_anthropic(request, model),
        ProviderProtocol::GeminiGenerateContent => responses_to_gemini(request, model),
    }
}

fn validate_responses_request(request: &Value) -> Result<(), String> {
    let object = request
        .as_object()
        .ok_or("Responses request must be an object")?;
    match object.get("input") {
        Some(Value::String(_)) | None => Ok(()),
        Some(Value::Array(items)) if items.iter().all(Value::is_object) => Ok(()),
        Some(Value::Array(_)) => Err("Responses input items must be objects".into()),
        _ => Err("Responses input must be text or item array".into()),
    }
}

fn fixture_candidate(settings: &GatewaySettings, provider: &ProviderDefinition) -> RouteCandidate {
    fixture_candidate_for_model(settings, provider, fixture_model(provider))
}

fn fixture_candidate_for_model(
    settings: &GatewaySettings,
    provider: &ProviderDefinition,
    model: &str,
) -> RouteCandidate {
    let model = model.to_string();
    let metadata = settings.model_catalog.iter().find(|metadata| {
        metadata.enabled
            && metadata.provider_id == provider.id.as_str()
            && metadata.model_id == model.as_str()
    });
    RouteCandidate {
        capabilities: effective_model_capabilities(provider, metadata, &model),
        provider: provider.clone(),
        upstream_model: model.clone(),
        exposed_model: model.clone(),
        credential: None,
        account_id: None,
        target_key: format!("{}/{}", provider.id, model),
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
    }
}

fn fixture_model_matching<'a>(
    provider: &'a ProviderDefinition,
    configured: &[String],
) -> Option<&'a str> {
    provider
        .default_model
        .as_deref()
        .into_iter()
        .chain(provider.models.iter().map(String::as_str))
        .find(|model| model_matches_any(model, configured))
}

fn structured_output_fixture(
    settings: &GatewaySettings,
    provider: &ProviderDefinition,
) -> Result<Option<String>, String> {
    let request = json!({
        "input": "json",
        "text": {"format": {"type": "json_schema", "name": "fixture",
            "schema": {"type": "object", "properties": {"ok": {"type": "boolean"}}}}}
    });
    let mut wire = translate_request(provider, &request)?;
    let candidate = fixture_candidate(settings, provider);
    let protocol = provider.protocol_for_model(&candidate.upstream_model);
    apply_provider_wire_compatibility(&mut wire, &request, &candidate, protocol)?;
    let present = match protocol {
        ProviderProtocol::Responses => wire.pointer("/text/format").is_some(),
        ProviderProtocol::ChatCompletions => wire.get("response_format").is_some(),
        ProviderProtocol::AnthropicMessages => wire.pointer("/output_config/format").is_some(),
        ProviderProtocol::GeminiGenerateContent => {
            wire.pointer("/generationConfig/responseJsonSchema").is_some()
        }
    };
    if present == candidate.capabilities.structured_output {
        Ok(Some(format!(
            "structured output was {} by the effective model capability",
            if present { "forwarded" } else { "stripped" }
        )))
    } else {
        Err("wire structured-output behavior disagrees with effective capability".into())
    }
}

fn service_tier_fixture(
    settings: &GatewaySettings,
    provider: &ProviderDefinition,
) -> Result<Option<String>, String> {
    let request = json!({"input": "tier", "service_tier": "priority"});
    let mut wire = translate_request(provider, &request)?;
    let candidate = fixture_candidate(settings, provider);
    let protocol = provider.protocol_for_model(&candidate.upstream_model);
    apply_provider_wire_compatibility(&mut wire, &request, &candidate, protocol)?;
    let present = wire.get("service_tier").or_else(|| wire.get("serviceTier")).is_some();
    if present == candidate.capabilities.service_tier {
        Ok(Some(format!(
            "service tier was {} by the effective model capability",
            if present { "forwarded" } else { "stripped" }
        )))
    } else {
        Err("wire service-tier behavior disagrees with effective capability".into())
    }
}

fn tool_roundtrip(
    settings: &GatewaySettings,
    provider: &ProviderDefinition,
    custom: bool,
    tool_search: bool,
) -> Result<Option<String>, String> {
    let capabilities = fixture_candidate(settings, provider).capabilities;
    let enabled = if custom {
        capabilities.tools && capabilities.custom_tools
    } else {
        capabilities.tools && capabilities.tool_search
    };
    if !enabled {
        return Ok(None);
    }
    let request = if custom {
        json!({
            "tools": [{"type": "custom", "name": "exec", "format": {"type": "text"}}],
            "input": [
                {"type": "custom_tool_call", "call_id": "call_custom", "name": "exec", "input": "pwd"},
                {"type": "custom_tool_call_output", "call_id": "call_custom", "output": "ok"}
            ]
        })
    } else {
        json!({
            "tools": [{"type": "tool_search", "namespace": "mcp__fixture"}],
            "tool_choice": {"type": "tool_search"},
            "input": [
                {"type": "tool_search_call", "call_id": "call_search", "namespace": "mcp__fixture", "arguments": "{\"query\":\"x\"}"},
                {"type": "tool_search_output", "call_id": "call_search", "output": {"tools": [{"name": "mcp__fixture__lookup"}]}}
            ]
        })
    };
    let serialized = serde_json::to_string(&translate_request(provider, &request)?)
        .map_err(|error| error.to_string())?;
    let identity = if custom { "call_custom" } else { "call_search" };
    if serialized.contains(identity)
        && (!tool_search || serialized.contains("mcp__fixture"))
    {
        Ok(Some("typed tool identity survived adapter translation".into()))
    } else {
        Err("tool identity or namespace was lost during adapter translation".into())
    }
}

fn mcp_namespace_roundtrip(
    settings: &GatewaySettings,
    provider: &ProviderDefinition,
) -> Result<Option<String>, String> {
    let capabilities = fixture_candidate(settings, provider).capabilities;
    if !(capabilities.tools && capabilities.mcp_namespaces) {
        return Ok(None);
    }
    let request = json!({
        "tools": [{"type": "namespace", "name": "calendar", "tools": [{
            "type": "function", "name": "create_event", "parameters": {"type": "object"}
        }]}],
        "input": [{"type": "function_call", "call_id": "call_mcp", "namespace": "calendar",
            "name": "create_event", "arguments": "{}"}]
    });
    let serialized = serde_json::to_string(&translate_request(provider, &request)?)
        .map_err(|error| error.to_string())?;
    if serialized.contains("calendar") && serialized.contains("create_event") {
        Ok(Some("MCP namespace and function identity survived translation".into()))
    } else {
        Err("MCP namespace was lost during adapter translation".into())
    }
}

fn opaque_metadata_roundtrip(
    settings: &GatewaySettings,
    provider: &ProviderDefinition,
) -> Result<Option<String>, String> {
    if !fixture_candidate(settings, provider)
        .capabilities
        .provider_metadata
    {
        return Ok(None);
    }
    let request = json!({"input": [{
        "type": "message", "role": "assistant", "content": "fixture",
        "provider_metadata": {"fixture": {"opaque": "signed"}}
    }]});
    let translated = translate_request(provider, &request)?;
    let serialized = serde_json::to_string(&translated).map_err(|error| error.to_string())?;
    if serialized.contains("signed") {
        Ok(Some("provider-owned opaque metadata survived the adapter path".into()))
    } else {
        Err("provider-owned opaque metadata was dropped".into())
    }
}

fn reasoning_signature_roundtrip(
    provider: &ProviderDefinition,
) -> Result<Option<String>, String> {
    let model = fixture_model(provider);
    if provider.transport == ProviderTransport::Kiro {
        let request = json!({
            "tool_choice": "none",
            "input": [
                {"type": "message", "role": "user", "content": "first"},
                {"type": "message", "role": "assistant", "content": "answer"},
                {"type": "reasoning", "provider_metadata": {"kiro": {"redacted_reasoning": ["opaque-kiro"]}}},
                {"type": "message", "role": "user", "content": "next"}
            ]
        });
        let (wire, _) = crate::kiro::responses_to_kiro(&request, model, None)?;
        return if serde_json::to_string(&wire)
            .map_err(|error| error.to_string())?
            .contains("opaque-kiro")
        {
            Ok(Some("Kiro redacted reasoning was replayed".into()))
        } else {
            Err("Kiro redacted reasoning was lost".into())
        };
    }
    match provider.protocol_for_model(model) {
        ProviderProtocol::AnthropicMessages => {
            let response = anthropic_to_response(
                &json!({"content": [{"type": "thinking", "thinking": "fixture", "signature": "signed-anthropic"}], "stop_reason": "end_turn"}),
                model,
                &Default::default(),
            )?;
            let replay = responses_to_anthropic(
                &json!({"input": response["output"].clone()}),
                model,
            )?;
            if serde_json::to_string(&replay)
                .map_err(|error| error.to_string())?
                .contains("signed-anthropic")
            {
                Ok(Some("Anthropic signed thinking survived normalize and replay".into()))
            } else {
                Err("Anthropic signed thinking was lost".into())
            }
        }
        ProviderProtocol::GeminiGenerateContent => {
            let response = gemini_to_response(
                &json!({"candidates": [{"content": {"parts": [{
                    "thoughtSignature": "signed-gemini",
                    "functionCall": {"id": "call_sig", "name": "lookup", "args": {}}
                }]}, "finishReason": "STOP"}]}),
                model,
                &Default::default(),
            )?;
            let replay = responses_to_gemini(
                &json!({"input": response["output"].clone()}),
                model,
            )?;
            if replay.pointer("/contents/0/parts/0/thoughtSignature")
                == Some(&Value::String("signed-gemini".into()))
            {
                Ok(Some("Gemini thought signature survived normalize and replay".into()))
            } else {
                Err("Gemini thought signature was lost".into())
            }
        }
        _ => Ok(None),
    }
}

fn snapshot_repair_fixture(provider: &ProviderDefinition) -> Result<Option<String>, String> {
    if !provider.responses_snapshot_repair {
        return Ok(None);
    }
    let mut snapshot = ResponsesSnapshotAccumulator::default();
    snapshot.observe(&json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"id": "msg_fixture", "type": "message", "status": "in_progress",
            "role": "assistant", "content": [{"type": "output_text", "text": ""}]}
    }));
    let delta = json!({
        "type": "response.output_text.delta", "output_index": 0,
        "item_id": "msg_fixture", "content_index": 0, "delta": "repaired"
    });
    let mut injected = snapshot.injected_events_before(&delta);
    snapshot.observe(&delta);
    let mut terminal = json!({
        "type": "response.completed",
        "response": {"id": "resp_fixture", "status": "completed"}
    });
    injected.extend(snapshot.closing_events_before_terminal(&terminal));
    snapshot.repair_terminal_event(&mut terminal);
    if terminal.pointer("/response/output/0/content/0/text")
        == Some(&Value::String("repaired".into()))
        && injected.iter().filter_map(|event| event.get("type").and_then(Value::as_str)).eq([
            "response.content_part.added",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
        ])
    {
        Ok(Some("terminal snapshot emitted canonical closing events before reconstruction".into()))
    } else {
        Err("terminal snapshot repair did not reconstruct streamed text".into())
    }
}

fn orphan_tool_output_fixture(provider: &ProviderDefinition) -> Result<Option<String>, String> {
    if !provider.responses_snapshot_repair {
        return Ok(None);
    }
    let mut snapshot = ResponsesSnapshotAccumulator::default();
    snapshot.observe(&json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"id": "out_orphan", "type": "function_call_output",
            "call_id": "missing", "output": "unsafe"}
    }));
    let mut terminal = json!({
        "type": "response.completed",
        "response": {"id": "resp_orphan", "status": "completed"}
    });
    snapshot.closing_events_before_terminal(&terminal);
    snapshot.repair_terminal_event(&mut terminal);
    if terminal
        .pointer("/response/output")
        .is_none()
    {
        Ok(Some("open non-injectable tool output blocked snapshot reconstruction".into()))
    } else {
        Err("orphan tool output survived snapshot repair".into())
    }
}

fn invalid_item_id_fixture(provider: &ProviderDefinition) -> Result<Option<String>, String> {
    if provider.protocol_for_model(fixture_model(provider)) != ProviderProtocol::Responses {
        return Ok(None);
    }
    let Some(mut repair) = ResponsesItemIdRepair::new_with_policy(
        &provider.response_item_id_repair,
        provider.repair_invalid_response_item_ids,
    ) else {
        return Ok(None);
    };
    let mut event = json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"id": "invalid id", "type": "message", "role": "assistant", "content": []}
    });
    repair.repair_event(&mut event);
    let repaired = event.pointer("/item/id").and_then(Value::as_str).unwrap_or_default();
    if repaired.starts_with("msg_") && repaired != "invalid id" {
        Ok(Some("invalid Responses item ID was repaired by the configured policy".into()))
    } else {
        Err("invalid Responses item ID was not repaired".into())
    }
}

fn anthropic_eof_fixture(provider: &ProviderDefinition) -> Result<Option<String>, String> {
    if provider.protocol_for_model(fixture_model(provider)) != ProviderProtocol::AnthropicMessages
        || !model_matches_any(
            fixture_model(provider),
            &provider.anthropic_eof_tolerance_models,
        )
    {
        return Ok(None);
    }
    let mut complete = br#"data: {"type":"message_stop"}"#.to_vec();
    let delimiter = tolerated_eof_delimiter(&complete, true, false)
        .ok_or("EOF tolerance did not schedule a final delimiter")?;
    let values = drain_sse_values(&mut complete, delimiter)?;
    let mut truncated = br#"data: {"type":"message_stop""#.to_vec();
    let truncated_delimiter = tolerated_eof_delimiter(&truncated, true, false)
        .ok_or("EOF tolerance did not inspect truncated input")?;
    if values.len() == 1 && drain_sse_values(&mut truncated, truncated_delimiter).is_err() {
        Ok(Some("complete EOF frame parsed and truncated JSON rejected".into()))
    } else {
        Err("Anthropic EOF tolerance accepted an incomplete frame".into())
    }
}

fn terminal_continuation_fixture(
    settings: &GatewaySettings,
    provider: &ProviderDefinition,
) -> Result<Option<String>, String> {
    let Some(model) = fixture_model_matching(
        provider,
        &provider.terminal_continuation_guard_models,
    ) else {
        return Ok(None);
    };
    let candidate = fixture_candidate_for_model(settings, provider, model);
    let mut terminal = json!({"input": [
        {"type": "function_call", "call_id": "call_guard", "name": "lookup", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "call_guard", "output": "ok"}
    ]});
    apply_provider_request_compatibility(
        &mut terminal,
        &candidate,
        provider.protocol_for_model(model),
        false,
    );
    let guarded = terminal
        .pointer("/input")
        .and_then(Value::as_array)
        .and_then(|items| items.last())
        .is_some_and(|item| {
            item.get("role").and_then(Value::as_str) == Some("developer")
        });
    let mut ordinary = json!({"input": [{
        "type": "message", "role": "user", "content": "continue"
    }]});
    apply_provider_request_compatibility(
        &mut ordinary,
        &candidate,
        provider.protocol_for_model(model),
        false,
    );
    let ordinary_guarded = ordinary
        .pointer("/input")
        .and_then(Value::as_array)
        .and_then(|items| items.last())
        .is_some_and(|item| {
            item.get("role").and_then(Value::as_str) == Some("developer")
        });
    if guarded && !ordinary_guarded {
        Ok(Some("terminal tool result was guarded without changing ordinary history".into()))
    } else {
        Err("terminal continuation guard did not match runtime behavior".into())
    }
}

fn empty_completion_retry_fixture(
    settings: &GatewaySettings,
    provider: &ProviderDefinition,
) -> Result<Option<String>, String> {
    let configured = provider
        .empty_completion_retry_models
        .iter()
        .chain(&provider.terminal_continuation_guard_models)
        .cloned()
        .collect::<Vec<_>>();
    let Some(model) = fixture_model_matching(provider, &configured) else {
        return Ok(None);
    };
    let candidate = fixture_candidate_for_model(settings, provider, model);
    let empty = json!({"output": []});
    let non_empty = json!({"output": [{"type": "message", "content": [{
        "type": "output_text", "text": "ok"
    }]}]});
    let enabled = empty_completion_retry_enabled(&candidate, false, 0);
    let streaming_disabled = empty_completion_retry_enabled(&candidate, true, 0);
    let exhausted = empty_completion_retry_enabled(
        &candidate,
        false,
        provider.limits.empty_completion_retries,
    );
    if enabled
        && !streaming_disabled
        && !exhausted
        && completion_is_empty(&empty)
        && !completion_is_empty(&non_empty)
    {
        Ok(Some("empty completion detection and retry budget matched runtime policy".into()))
    } else {
        Err("empty completion retry did not match runtime behavior".into())
    }
}

fn request_pacing_fixture(provider: &ProviderDefinition) -> Result<Option<String>, String> {
    let mut pacing = ProviderPacingState::default();
    let now = Instant::now();
    let interval = Duration::from_millis(provider.limits.request_pacing_ms);
    let first = reserve_provider_start(&mut pacing, &provider.id, interval, now);
    let second = reserve_provider_start(&mut pacing, &provider.id, interval, now);
    if interval.is_zero() {
        if first == now && second == now {
            Ok(Some("disabled pacing leaves both starts immediate".into()))
        } else {
            Err("zero pacing unexpectedly delayed a request".into())
        }
    } else if first == now && second >= now + interval {
        Ok(Some(format!(
            "concurrent starts were separated by at least {} ms",
            provider.limits.request_pacing_ms
        )))
    } else {
        Err("provider pacing did not reserve distinct start slots".into())
    }
}

fn retry_429_fixture(provider: &ProviderDefinition) -> Result<Option<String>, String> {
    let limits = &provider.limits;
    let source = provider.credential.source;
    let first = should_retry_rate_limit(limits, source, 0);
    let exhausted = should_retry_rate_limit(limits, source, limits.max_429_retries);
    let key_credential = matches!(
        source,
        CredentialSource::Environment | CredentialSource::Keychain | CredentialSource::Command
    );
    if first == (limits.retry_on_429 && key_credential && limits.max_429_retries > 0)
        && !exhausted
    {
        Ok(Some(
            "429 retry opt-in, credential class, and attempt boundary were enforced".into(),
        ))
    } else {
        Err("429 retry decision disagrees with configured policy".into())
    }
}

fn malformed_request_fixture(provider: &ProviderDefinition) -> Result<Option<String>, String> {
    match translate_request(provider, &json!({"input": 42})) {
        Err(_) => Ok(Some("malformed input was rejected by the configured adapter".into())),
        Ok(_) => Err("configured adapter accepted malformed input".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GatewaySettings;
    use crate::registry::provider_presets;
    use serde_json::Value;

    #[test]
    fn every_registry_provider_has_positive_and_negative_fixture_rows() {
        let providers = provider_presets()
            .into_iter()
            .filter_map(|preset| preset.instantiate(preset.requires_custom_url.then_some("https://fixture.invalid/v1")).ok())
            .collect::<Vec<_>>();
        let settings = GatewaySettings { providers, ..GatewaySettings::default() };
        let report = compatibility_lab_report(&settings);
        for provider in &settings.providers {
            let rows = report.rows.iter().filter(|row| row.provider_id == provider.id).collect::<Vec<_>>();
            assert!(rows.iter().any(|row| row.expectation == ConformanceExpectation::Accept));
            assert!(rows.iter().any(|row| {
                row.expectation == ConformanceExpectation::Reject
                    && row.status == ConformanceStatus::Pass
            }));
            assert!(
                rows.iter().all(|row| row.status != ConformanceStatus::Fail),
                "{} has failing pure fixtures: {:?}",
                provider.id,
                rows.iter()
                    .filter(|row| row.status == ConformanceStatus::Fail)
                    .map(|row| (&row.fixture_id, &row.reason))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn compatibility_rows_report_actual_fixture_failures_not_declarations() {
        let provider = ProviderDefinition {
            id: "misconfigured".into(),
            name: "Misconfigured".into(),
            base_url: "https://fixture.invalid/v1".into(),
            protocol: ProviderProtocol::ChatCompletions,
            capabilities: crate::config::ProviderCapabilities {
                service_tier: true,
                ..crate::config::ProviderCapabilities::default()
            },
            chat_service_tier: None,
            ..ProviderDefinition::default()
        };
        let report = compatibility_lab_report(&GatewaySettings {
            providers: vec![provider],
            ..GatewaySettings::default()
        });
        let service = report
            .rows
            .iter()
            .find(|row| row.fixture_id == "service-tier")
            .expect("service tier row");

        assert_eq!(service.status, ConformanceStatus::Fail);
        assert!(!service.supported);
    }

    #[test]
    fn every_provider_preset_executes_positive_and_negative_adapter_fixtures() {
        for preset in provider_presets() {
            let provider = preset
                .instantiate(preset.requires_custom_url.then_some("https://fixture.invalid/v1"))
                .expect("fixture provider must instantiate");
            let settings = GatewaySettings {
                providers: vec![provider.clone()],
                ..GatewaySettings::default()
            };
            let capabilities = fixture_candidate(&settings, &provider).capabilities;
            let mut accepted = 0;
            let mut rejected = 0;
            for fixture in PROTOCOL_CONFORMANCE_FIXTURES {
                if (fixture.id == "adapter-custom-tool-roundtrip"
                    && !(capabilities.tools && capabilities.custom_tools))
                    || (fixture.id == "adapter-tool-search-roundtrip"
                        && !(capabilities.tools && capabilities.tool_search))
                    || (fixture.id == "adapter-structured-output"
                        && !capabilities.structured_output)
                {
                    continue;
                }
                let request: Value = serde_json::from_str(fixture.request_json).expect("fixture JSON");
                let result = translate_request(&provider, &request);
                match fixture.expectation {
                    ConformanceExpectation::Accept => {
                        assert!(result.is_ok(), "{} failed {}: {:?}", provider.id, fixture.id, result.as_ref().err());
                        validate_fixture_output(fixture.id, &provider, result.as_ref().expect("accepted fixture"))
                            .unwrap_or_else(|error| panic!("{} failed {} output: {error}", provider.id, fixture.id));
                        accepted += 1;
                    }
                    ConformanceExpectation::Reject => {
                        assert!(result.is_err(), "{} unexpectedly accepted {}", provider.id, fixture.id);
                        rejected += 1;
                    }
                }
            }
            assert!(accepted > 0 && rejected > 0, "{} lacks both fixture classes", provider.id);
        }
    }

    fn validate_fixture_output(
        fixture: &str,
        provider: &ProviderDefinition,
        output: &Value,
    ) -> Result<(), String> {
        let serialized = serde_json::to_string(output).map_err(|error| error.to_string())?;
        match fixture {
            "adapter-basic-text" => {
                if provider.transport == ProviderTransport::Kiro {
                    if serialized.contains("hello") {
                        return Ok(());
                    }
                    return Err("Kiro wire request lost basic text".into());
                }
                let protocol = provider.protocol_for_model(fixture_model(provider));
                let path = match protocol {
                    ProviderProtocol::Responses => "/input",
                    ProviderProtocol::ChatCompletions | ProviderProtocol::AnthropicMessages => "/messages/0",
                    ProviderProtocol::GeminiGenerateContent => "/contents/0",
                };
                output.pointer(path).ok_or_else(|| format!("missing translated text at {path}"))?;
            }
            "adapter-tool-roundtrip" if !serialized.contains("call_fixture") => {
                return Err("function call identity was lost".into());
            }
            "adapter-custom-tool-roundtrip" if !serialized.contains("call_custom") => {
                return Err("custom tool identity was lost".into());
            }
            "adapter-tool-search-roundtrip"
                if !serialized.contains("call_search") || !serialized.contains("mcp__fixture") =>
            {
                return Err("tool_search identity or MCP namespace was lost".into());
            }
            "adapter-structured-output" => {
                let protocol = provider.protocol_for_model(fixture_model(provider));
                let path = match protocol {
                    ProviderProtocol::Responses => "/text/format/schema",
                    ProviderProtocol::ChatCompletions => "/response_format/json_schema/schema",
                    ProviderProtocol::AnthropicMessages => "/output_config/format/schema",
                    ProviderProtocol::GeminiGenerateContent => "/generationConfig/responseJsonSchema",
                };
                output.pointer(path).ok_or_else(|| format!("structured output was lost at {path}"))?;
            }
            _ => {}
        }
        Ok(())
    }
}
