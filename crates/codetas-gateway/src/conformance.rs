use crate::config::{GatewaySettings, ProviderDefinition, ProviderProtocol};
use serde::Serialize;

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
    ConformanceFixture { id: "tool-roundtrip", expectation: ConformanceExpectation::Accept, description: "Function calls and outputs retain call identity." },
    ConformanceFixture { id: "parallel-tool-roundtrip", expectation: ConformanceExpectation::Accept, description: "Parallel calls remain adjacent to their results." },
    ConformanceFixture { id: "custom-tool-roundtrip", expectation: ConformanceExpectation::Accept, description: "Custom/freeform tools retain their typed payload." },
    ConformanceFixture { id: "tool-search-roundtrip", expectation: ConformanceExpectation::Accept, description: "tool_search calls and outputs retain namespace and identity." },
    ConformanceFixture { id: "reasoning-signature-roundtrip", expectation: ConformanceExpectation::Accept, description: "Provider-signed reasoning metadata survives replay." },
    ConformanceFixture { id: "structured-output", expectation: ConformanceExpectation::Accept, description: "JSON schema output is forwarded only when supported." },
    ConformanceFixture { id: "service-tier", expectation: ConformanceExpectation::Accept, description: "Service tier is forwarded only when supported." },
    ConformanceFixture { id: "orphan-tool-output", expectation: ConformanceExpectation::Reject, description: "An orphan tool result is repaired or rejected before upstream delivery." },
    ConformanceFixture { id: "invalid-item-id", expectation: ConformanceExpectation::Reject, description: "Invalid Responses item IDs are repaired before upstream delivery." },
    ConformanceFixture { id: "unsupported-structured-output", expectation: ConformanceExpectation::Reject, description: "Unsupported structured output is removed deterministically." },
    ConformanceFixture { id: "unsafe-provider-metadata", expectation: ConformanceExpectation::Reject, description: "Unowned or secret provider metadata is never persisted." },
];

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
            let (supported, reason) = fixture_support(provider, fixture.id);
            rows.push(CompatibilityResultRow {
                provider_id: provider.id.clone(),
                protocol: provider.protocol,
                fixture_id: fixture.id.into(),
                expectation: fixture.expectation,
                supported,
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

fn fixture_support(provider: &ProviderDefinition, fixture: &str) -> (bool, String) {
    let supported = match fixture {
        "basic-text" => true,
        "tool-roundtrip" => provider.capabilities.tools,
        "parallel-tool-roundtrip" => provider.capabilities.parallel_tools,
        "custom-tool-roundtrip" => provider.capabilities.custom_tools,
        "tool-search-roundtrip" => provider.capabilities.tool_search,
        "reasoning-signature-roundtrip" => provider.capabilities.reasoning,
        "structured-output" => provider.capabilities.structured_output,
        "service-tier" => provider.capabilities.service_tier,
        "orphan-tool-output" | "invalid-item-id" | "unsafe-provider-metadata" => true,
        "unsupported-structured-output" => !provider.capabilities.structured_output
            || !provider.no_structured_output_models.is_empty(),
        _ => false,
    };
    let protocol = match provider.protocol {
        ProviderProtocol::Responses => "Responses",
        ProviderProtocol::ChatCompletions => "Chat Completions",
        ProviderProtocol::AnthropicMessages => "Anthropic Messages",
        ProviderProtocol::GeminiGenerateContent => "Gemini generateContent",
    };
    (
        supported,
        if supported {
            format!("declared compatible on the {protocol} adapter")
        } else {
            format!("not declared by the provider's {protocol} capability contract")
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{anthropic::responses_to_anthropic, gemini::responses_to_gemini, translate::responses_to_chat};
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
            assert!(rows.iter().any(|row| row.expectation == ConformanceExpectation::Reject));
        }
    }

    #[test]
    fn every_provider_preset_executes_positive_and_negative_adapter_fixtures() {
        for preset in provider_presets() {
            let provider = preset
                .instantiate(preset.requires_custom_url.then_some("https://fixture.invalid/v1"))
                .expect("fixture provider must instantiate");
            let mut accepted = 0;
            let mut rejected = 0;
            for fixture in PROTOCOL_CONFORMANCE_FIXTURES {
                let request: Value = serde_json::from_str(fixture.request_json).expect("fixture JSON");
                let result = match provider.protocol {
                    ProviderProtocol::Responses => validate_responses_fixture(&request).map(|_| request.clone()),
                    ProviderProtocol::ChatCompletions => responses_to_chat(&request, "fixture-model"),
                    ProviderProtocol::AnthropicMessages => responses_to_anthropic(&request, "fixture-model"),
                    ProviderProtocol::GeminiGenerateContent => responses_to_gemini(&request, "fixture-model"),
                };
                match fixture.expectation {
                    ConformanceExpectation::Accept => {
                        assert!(result.is_ok(), "{} failed {}: {:?}", provider.id, fixture.id, result.as_ref().err());
                        validate_fixture_output(fixture.id, provider.protocol, result.as_ref().expect("accepted fixture"))
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

    fn validate_responses_fixture(request: &Value) -> Result<(), String> {
        let object = request.as_object().ok_or("Responses request must be an object")?;
        match object.get("input") {
            Some(Value::String(_)) | None => Ok(()),
            Some(Value::Array(items)) if items.iter().all(Value::is_object) => Ok(()),
            Some(Value::Array(_)) => Err("Responses input items must be objects".into()),
            _ => Err("Responses input must be text or item array".into()),
        }
    }

    fn validate_fixture_output(
        fixture: &str,
        protocol: ProviderProtocol,
        output: &Value,
    ) -> Result<(), String> {
        let serialized = serde_json::to_string(output).map_err(|error| error.to_string())?;
        match fixture {
            "adapter-basic-text" => {
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
