use crate::config::{CredentialSource, ProviderDefinition, ResponseItemIdRepairSettings};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

mod chat_tools;
mod sanitize;

pub use chat_tools::*;
pub use sanitize::*;

const TOOL_NAME_PREFIX: &str = "cx_";
const REPEATED_FUNCTION_TOOL_LIMIT: usize = 8;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum RepairableItemType {
    Message,
    Reasoning,
}

impl RepairableItemType {
    fn from_item(item: &Value) -> Option<Self> {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => Some(Self::Message),
            Some("reasoning") => Some(Self::Reasoning),
            _ => None,
        }
    }

    fn from_unambiguous_event(event_type: &str) -> Option<Self> {
        match event_type {
            "response.output_text.annotation.added"
            | "response.output_text.delta"
            | "response.output_text.done"
            | "response.refusal.delta"
            | "response.refusal.done" => Some(Self::Message),
            "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_text.delta"
            | "response.reasoning_text.done" => Some(Self::Reasoning),
            _ => None,
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Message => "msg_",
            Self::Reasoning => "rs_",
        }
    }
}

/// A request-local repair map for malformed Responses-compatible providers.
///
/// Output item IDs are normalized by item type. Function-call `call_id` fields
/// retain their upstream meaning and are never rewritten.
pub struct ResponsesItemIdRepair {
    settings: ResponseItemIdRepairSettings,
    exact_ids: HashMap<(usize, String), (RepairableItemType, String)>,
    positional_ids: HashMap<(usize, RepairableItemType), String>,
    item_types: HashMap<usize, RepairableItemType>,
    scope: String,
    repair_invalid_ids: bool,
}

impl ResponsesItemIdRepair {
    pub fn new(settings: &ResponseItemIdRepairSettings) -> Option<Self> {
        Self::new_with_policy(settings, false)
    }

    pub fn new_with_policy(
        settings: &ResponseItemIdRepairSettings,
        repair_invalid_ids: bool,
    ) -> Option<Self> {
        if settings.message.is_empty()
            && settings.reasoning.is_empty()
            && !settings.repair_missing_terminal_ids
            && !repair_invalid_ids
        {
            return None;
        }
        Some(Self {
            settings: settings.clone(),
            exact_ids: HashMap::new(),
            positional_ids: HashMap::new(),
            item_types: HashMap::new(),
            scope: Uuid::new_v4().simple().to_string(),
            repair_invalid_ids,
        })
    }

    pub fn repair_response(&mut self, response: &mut Value) {
        let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) else {
            return;
        };
        for (output_index, item) in output.iter_mut().enumerate() {
            self.repair_output_item(output_index, item);
        }
    }

    pub fn repair_event(&mut self, event: &mut Value) {
        let output_index = event
            .get("output_index")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok());

        if let Some(output_index) = output_index {
            if let Some(item) = event.get_mut("item") {
                self.repair_output_item(output_index, item);
            }
            if let Some(item_type) = self.event_item_type(output_index, event) {
                self.repair_event_item_id(output_index, item_type, event);
            }
        }
        if let Some(response) = event.get_mut("response") {
            self.repair_response(response);
        }
    }

    fn repair_output_item(&mut self, output_index: usize, item: &mut Value) {
        let Some(item_type) = RepairableItemType::from_item(item) else {
            return;
        };
        let current = item.get("id").and_then(Value::as_str).map(str::to_string);
        self.item_types.insert(output_index, item_type);
        let mapped = match current.as_deref() {
            Some(current) => {
                if let Some((mapped_type, mapped)) =
                    self.exact_ids.get(&(output_index, current.to_string()))
                {
                    if *mapped_type != item_type {
                        return;
                    }
                    mapped.clone()
                } else if self
                    .positional_ids
                    .get(&(output_index, item_type))
                    .is_some_and(|mapped| mapped == current)
                {
                    return;
                } else if self.is_placeholder(item_type, current)
                    || (self.repair_invalid_ids && !valid_item_id(item_type, current))
                {
                    let mapped = self.synthetic_id(item_type, output_index);
                    self.exact_ids.insert(
                        (output_index, current.to_string()),
                        (item_type, mapped.clone()),
                    );
                    self.positional_ids
                        .insert((output_index, item_type), mapped.clone());
                    mapped
                } else if self.settings.repair_missing_terminal_ids {
                    self.positional_ids
                        .entry((output_index, item_type))
                        .or_insert_with(|| current.to_string());
                    return;
                } else {
                    return;
                }
            }
            None if self.settings.repair_missing_terminal_ids => {
                let synthetic = self.synthetic_id(item_type, output_index);
                self.positional_ids
                    .entry((output_index, item_type))
                    .or_insert(synthetic)
                    .clone()
            }
            None => return,
        };

        if current.as_deref() == Some(mapped.as_str()) {
            return;
        }
        if let Some(object) = item.as_object_mut() {
            object.insert("id".into(), Value::String(mapped));
        }
    }

    fn repair_event_item_id(
        &self,
        output_index: usize,
        item_type: RepairableItemType,
        event: &mut Value,
    ) {
        let current = event.get("item_id").and_then(Value::as_str);
        let mapped = match current {
            Some(current) => self
                .exact_ids
                .get(&(output_index, current.to_string()))
                .filter(|(mapped_type, _)| *mapped_type == item_type)
                .map(|(_, mapped)| mapped.as_str()),
            None if self.settings.repair_missing_terminal_ids => self
                .positional_ids
                .get(&(output_index, item_type))
                .map(String::as_str),
            None => None,
        };
        let Some(mapped) = mapped else { return };
        if current == Some(mapped) { return; }
        if let Some(object) = event.as_object_mut() {
            object.insert("item_id".into(), Value::String(mapped.to_string()));
        }
    }

    fn is_placeholder(&self, item_type: RepairableItemType, value: &str) -> bool {
        match item_type {
            RepairableItemType::Message => self.settings.message.iter().any(|id| id == value),
            RepairableItemType::Reasoning => self.settings.reasoning.iter().any(|id| id == value),
        }
    }

    fn event_item_type(&self, output_index: usize, event: &Value) -> Option<RepairableItemType> {
        let event_type = event.get("type").and_then(Value::as_str)?;
        if matches!(
            event_type,
            "response.content_part.added" | "response.content_part.done"
        ) {
            if let Some(raw_id) = event.get("item_id").and_then(Value::as_str) {
                if let Some((item_type, _)) =
                    self.exact_ids.get(&(output_index, raw_id.to_string()))
                {
                    return Some(*item_type);
                }
            }
            return self.item_types.get(&output_index).copied();
        }
        RepairableItemType::from_unambiguous_event(event_type)
    }

    fn synthetic_id(&self, item_type: RepairableItemType, output_index: usize) -> String {
        format!(
            "{}codetas_{}_{}",
            item_type.prefix(),
            self.scope,
            output_index
        )
    }
}

fn valid_item_id(item_type: RepairableItemType, value: &str) -> bool {
    value.starts_with(item_type.prefix())
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

pub fn escape_anthropic_tool_names(body: &mut Value) -> Result<(), String> {
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            escape_name_field(tool, "name")?;
        }
    }
    if let Some(choice) = body.get_mut("tool_choice") {
        escape_name_field(choice, "name")?;
    }
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) else {
                continue;
            };
            for part in parts {
                if part.get("type").and_then(Value::as_str) == Some("tool_use") {
                    escape_name_field(part, "name")?;
                }
            }
        }
    }
    Ok(())
}

pub fn restore_anthropic_tool_names(value: &mut Value) {
    if let Some(blocks) = value.get_mut("content").and_then(Value::as_array_mut) {
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                restore_name_field(block, "name");
            }
        }
    }
}

pub fn restore_anthropic_stream_tool_names(value: &mut Value) {
    if value.get("type").and_then(Value::as_str) == Some("content_block_start") {
        if let Some(block) = value.get_mut("content_block") {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                restore_name_field(block, "name");
            }
        }
    }
}

fn escape_name_field(value: &mut Value, field: &str) -> Result<(), String> {
    let Some(name) = value.get(field).and_then(Value::as_str) else {
        return Ok(());
    };
    let escaped = format!("{TOOL_NAME_PREFIX}{name}");
    if escaped.len() > 64 {
        return Err(format!(
            "tool name {name} is too long for reversible provider escaping"
        ));
    }
    if let Some(object) = value.as_object_mut() {
        object.insert(field.into(), Value::String(escaped));
    }
    Ok(())
}

fn restore_name_field(value: &mut Value, field: &str) {
    let Some(name) = value.get(field).and_then(Value::as_str) else {
        return;
    };
    let Some(restored) = name.strip_prefix(TOOL_NAME_PREFIX) else {
        return;
    };
    let restored = restored.to_string();
    if let Some(object) = value.as_object_mut() {
        object.insert(field.into(), Value::String(restored));
    }
}

pub fn is_codetas_repaired_item_id(value: &str) -> bool {
    value.starts_with("msg_codetas_") || value.starts_with("rs_codetas_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderDefinition;

    fn chatgpt_provider() -> ProviderDefinition {
        ProviderDefinition {
            id: "openai".into(),
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            credential: crate::config::ProviderCredential {
                source: CredentialSource::Forward,
                ..crate::config::ProviderCredential::default()
            },
            ..ProviderDefinition::default()
        }
    }

    fn api_key_provider() -> ProviderDefinition {
        ProviderDefinition {
            id: "openai-apikey".into(),
            base_url: "https://api.openai.com/v1".into(),
            ..ProviderDefinition::default()
        }
    }

    #[test]
    fn chatgpt_forward_strips_fields_that_400_the_codex_backend() {
        let provider = chatgpt_provider();
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "previous_response_id": "resp_123",
            "metadata": {"session": "abc"},
            "max_output_tokens": 4096,
            "input": [{
                "type": "message",
                "id": "msg_1",
                "role": "user",
                "content": [{"type": "input_text", "text": "hi"}]
            }]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
        assert!(body.get("previous_response_id").is_none());
        assert!(body.get("metadata").is_none());
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(body["input"][0]["id"], "msg_1");
    }

    fn repeated_function_history(count: usize) -> Value {
        let mut input = vec![json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "Review the changes"}]
        })];
        for index in 0..count {
            let call_id = format!("call_{index}");
            input.push(json!({
                "type": "function_call",
                "call_id": call_id,
                "name": "update_plan",
                "arguments": "{\"plan\":[]}"
            }));
            input.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": "Plan updated"
            }));
        }
        json!({
            "tools": [
                {"type": "function", "name": "update_plan", "parameters": {}},
                {"type": "function", "name": "exec_command", "parameters": {}}
            ],
            "tool_choice": {"type": "function", "name": "update_plan"},
            "input": input
        })
    }

    #[test]
    fn repeated_function_loop_removes_only_the_repeated_tool() {
        let mut body = repeated_function_history(REPEATED_FUNCTION_TOOL_LIMIT);
        assert_eq!(
            guard_repeated_function_tool_loop(&mut body).as_deref(),
            Some("update_plan")
        );
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["tools"][0]["name"], "exec_command");
        assert!(body.get("tool_choice").is_none());
        let warning = body["input"].as_array().unwrap().last().unwrap();
        assert_eq!(warning["role"], "user");
        assert!(warning["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Do not call that tool again"));
    }

    #[test]
    fn repeated_function_loop_allows_normal_retries() {
        let mut body = repeated_function_history(REPEATED_FUNCTION_TOOL_LIMIT - 1);
        assert_eq!(guard_repeated_function_tool_loop(&mut body), None);
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
        assert!(body.get("tool_choice").is_some());
    }

    #[test]
    fn repeated_function_loop_resets_after_new_user_input() {
        let mut body = repeated_function_history(REPEATED_FUNCTION_TOOL_LIMIT);
        body["input"].as_array_mut().unwrap().push(json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "Try the plan again"}]
        }));
        assert_eq!(guard_repeated_function_tool_loop(&mut body), None);
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn repeated_function_loop_allows_different_arguments() {
        let mut body = repeated_function_history(REPEATED_FUNCTION_TOOL_LIMIT);
        for (index, item) in body["input"].as_array_mut().unwrap().iter_mut().enumerate() {
            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                item["arguments"] = Value::String(format!("{{\"step\":{index}}}"));
            }
        }
        assert_eq!(guard_repeated_function_tool_loop(&mut body), None);
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn chatgpt_forward_empties_raw_reasoning_content() {
        let provider = chatgpt_provider();
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "input": [{
                "type": "reasoning",
                "id": "rs_1",
                "content": [{"type": "reasoning_text", "text": "internal thoughts"}],
                "encrypted_content": "ocxr1:AAAA"
            }]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
        assert_eq!(body["input"][0]["content"], json!([]));
        assert!(body["input"][0].get("encrypted_content").is_none());
        assert_eq!(body["input"][0]["id"], "rs_1");
    }

    #[test]
    fn compact_forward_matches_open_codex_reasoning_sanitization() {
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "reasoning": {"effort": "high", "summary": "auto"},
            "input": [
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "content": [{"type": "reasoning_text", "text": "internal thoughts"}],
                    "encrypted_content": "codetas1:AAAA"
                },
                {
                    "type": "reasoning",
                    "id": "rs_2",
                    "content": [{"type": "reasoning_text", "text": "opaque summary"}],
                    "encrypted_content": "gAAAAABopaque"
                },
                {"type": "message", "role": "user", "content": []}
            ]
        });

        sanitize_compact_request(&mut body);

        assert!(body.get("reasoning").is_none());
        assert_eq!(body["input"][0]["content"], json!([]));
        assert!(body["input"][0].get("encrypted_content").is_none());
        assert_eq!(body["input"][1]["content"], json!([]));
        assert_eq!(body["input"][1]["encrypted_content"], "gAAAAABopaque");
        assert_eq!(body["input"][2]["role"], "user");
    }

    #[test]
    fn compact_forward_expands_local_compaction_envelopes() {
        let encrypted = crate::compaction::encode_summary("prior local summary")
            .expect("valid envelope");
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "reasoning": {"effort": "high"},
            "input": [
                {"type": "compaction", "encrypted_content": encrypted},
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "content": [{"type": "reasoning_text", "text": "internal thoughts"}],
                    "encrypted_content": "codetas1:AAAA"
                },
                {"type": "message", "role": "user", "content": "continue"}
            ]
        });

        sanitize_compact_request(&mut body);

        assert!(body.get("reasoning").is_none());
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "assistant");
        assert!(
            body["input"][0]["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("prior local summary"))
        );
        assert_eq!(body["input"][1]["content"], json!([]));
        assert!(body["input"][1].get("encrypted_content").is_none());
        assert_eq!(body["input"][2]["role"], "user");
        assert!(!serde_json::to_string(&body).unwrap().contains("codetas1:"));
    }

    #[test]
    fn compact_forward_does_not_change_body_without_reasoning_items() {
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "input": [{"type": "message", "role": "user", "content": "keep"}]
        });
        let original = body.clone();

        sanitize_compact_request(&mut body);

        assert_eq!(body, original);
    }

    #[test]
    fn compact_trigger_preserves_top_level_reasoning_but_sanitizes_input() {
        let encrypted = crate::compaction::encode_summary("prior local summary")
            .expect("valid envelope");
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "reasoning": {"effort": "high", "summary": "auto"},
            "stream": true,
            "store": false,
            "input": [
                {"type": "compaction", "encrypted_content": encrypted},
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "content": [{"type": "reasoning_text", "text": "internal thoughts"}],
                    "encrypted_content": "ocxr1:AAAA"
                },
                {"type": "compaction_trigger", "id": "trigger_1"}
            ]
        });

        sanitize_compact_trigger_request(&mut body);

        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "assistant");
        assert!(
            body["input"][0]["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("prior local summary"))
        );
        assert_eq!(body["input"][1]["content"], json!([]));
        assert!(body["input"][1].get("encrypted_content").is_none());
        assert_eq!(body["input"][2]["type"], "compaction_trigger");
        assert!(!serde_json::to_string(&body).unwrap().contains("codetas1:"));
    }

    #[test]
    fn compact_sanitizers_repair_orphaned_tool_outputs_without_cross_conversation_lookup() {
        for sanitize in [
            sanitize_compact_request as fn(&mut Value),
            sanitize_compact_trigger_request as fn(&mut Value),
        ] {
            let mut body = json!({
                "reasoning": {"effort": "high"},
                "input": [{
                    "type": "custom_tool_call_output",
                    "call_id": "call_missing",
                    "output": "pwd=/tmp"
                }]
            });
            sanitize(&mut body);
            assert_eq!(body["input"][0]["type"], "message");
            assert!(body["input"][0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("pwd=/tmp"));
        }
    }

    #[test]
    fn responses_sanitizer_expands_local_compaction_envelopes() {
        let encrypted = crate::compaction::encode_summary("prior local summary")
            .expect("valid envelope");
        for provider in [chatgpt_provider(), api_key_provider()] {
            let mut body = json!({
                "model": "gpt-5.6-sol",
                "input": [
                    {"type": "compaction", "encrypted_content": encrypted},
                    {
                        "type": "reasoning",
                        "id": "rs_1",
                        "content": [{"type": "reasoning_text", "text": "internal thoughts"}],
                        "encrypted_content": "codetas1:AAAA"
                    },
                    {"type": "message", "role": "user", "content": "continue"}
                ]
            });
            sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
            assert_eq!(body["input"][0]["type"], "message");
            assert_eq!(body["input"][0]["role"], "assistant");
            assert!(
                body["input"][0]["content"][0]["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("prior local summary"))
            );
            assert_eq!(body["input"][1]["content"], json!([]));
            assert!(body["input"][1].get("encrypted_content").is_none());
            assert_eq!(body["input"][2]["role"], "user");
            assert!(!serde_json::to_string(&body).unwrap().contains("codetas1:"));
            assert!(!serde_json::to_string(&body).unwrap().contains("ocx1:"));
        }
    }

    #[test]
    fn responses_sanitizer_expands_legacy_ocx1_compaction_envelopes() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode("legacy local summary");
        let encrypted = format!("ocx1:{encoded}");
        let provider = api_key_provider();
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "input": [
                {"type": "compaction", "encrypted_content": encrypted},
                {"type": "message", "role": "user", "content": "continue"}
            ]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "assistant");
        assert!(
            body["input"][0]["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("legacy local summary"))
        );
        assert_eq!(body["input"][1]["role"], "user");
        assert!(!serde_json::to_string(&body).unwrap().contains("ocx1:"));
        assert!(!serde_json::to_string(&body).unwrap().contains("codetas1:"));
    }

    #[test]
    fn responses_sanitizer_leaves_native_compaction_payloads_intact() {
        let provider = chatgpt_provider();
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "input": [
                {"type": "compaction", "encrypted_content": "gAAAAABopaque"},
                {"type": "message", "role": "user", "content": "continue"}
            ]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
        assert_eq!(body["input"][0]["type"], "compaction");
        assert_eq!(body["input"][0]["encrypted_content"], "gAAAAABopaque");
        assert_eq!(body["input"][1]["role"], "user");
    }

    #[test]
    fn chatgpt_forward_repairs_orphaned_tool_output_after_replay_miss() {
        let provider = chatgpt_provider();
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "previous_response_id": "resp_missing",
            "input": [{
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "pwd=/tmp"
            }]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
        assert!(body.get("previous_response_id").is_none());
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "user");
        assert!(body["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("pwd=/tmp"));
    }

    #[test]
    fn chatgpt_forward_repairs_orphaned_tool_search_output() {
        let provider = chatgpt_provider();
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "input": [{
                "type": "tool_search_output",
                "call_id": "call_missing",
                "tools": [{"name": "exec_command"}]
            }]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "user");
        assert!(body["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("exec_command"));
    }

    #[test]
    fn chatgpt_forward_keeps_paired_tool_search_output() {
        let provider = chatgpt_provider();
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "input": [
                {
                    "type": "tool_search_call",
                    "call_id": "call_1",
                    "arguments": {"query": "shell"}
                },
                {
                    "type": "tool_search_output",
                    "call_id": "call_1",
                    "tools": [{"name": "exec_command"}]
                }
            ]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
        assert_eq!(body["input"][0]["type"], "tool_search_call");
        assert_eq!(body["input"][1]["type"], "tool_search_output");
    }

    #[test]
    fn api_key_provider_repairs_orphaned_tool_search_output_without_previous_response() {
        let provider = api_key_provider();
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "input": [{
                "type": "tool_search_output",
                "call_id": "call_missing",
                "tools": [{"name": "exec_command"}]
            }]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
        assert_eq!(body["input"][0]["type"], "message");
        assert!(body["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("exec_command"));
    }

    #[test]
    fn api_key_provider_keeps_remote_tool_output_with_previous_response() {
        let provider = api_key_provider();
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "previous_response_id": "resp_123",
            "input": [{
                "type": "tool_search_output",
                "call_id": "call_remote",
                "tools": [{"name": "exec_command"}]
            }]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
        assert_eq!(body["previous_response_id"], "resp_123");
        assert_eq!(body["input"][0]["type"], "tool_search_output");
    }

    #[test]
    fn chatgpt_forward_shortens_oversized_call_ids() {
        let provider = chatgpt_provider();
        let long_id = format!("call_{}", "x".repeat(80));
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "input": [
                {"type": "function_call", "call_id": long_id.clone(), "name": "exec", "arguments": "{}"},
                {"type": "function_call_output", "call_id": long_id, "output": "ok"}
            ]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
        let first = body["input"][0]["call_id"].as_str().unwrap();
        let second = body["input"][1]["call_id"].as_str().unwrap();
        assert_eq!(first, second);
        assert!(first.len() <= 64);
        assert!(first.starts_with("call_cdt_"));
    }

    #[test]
    fn store_false_strips_item_ids() {
        let provider = chatgpt_provider();
        let mut body = json!({
            "store": false,
            "input": [{"type": "message", "id": "msg_abc", "role": "user", "content": []}]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.6-sol");
        assert!(body["input"][0].get("id").is_none());
    }

    #[test]
    fn api_key_providers_keep_stateful_fields() {
        let provider = api_key_provider();
        let mut body = json!({
            "previous_response_id": "resp_123",
            "metadata": {"k": "v"},
            "max_output_tokens": 128,
            "input": [{
                "type": "reasoning",
                "id": "rs_1",
                "content": [{"type": "reasoning_text", "text": "keep summaries only"}]
            }]
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.4");
        assert_eq!(body["previous_response_id"], "resp_123");
        assert_eq!(body["metadata"]["k"], "v");
        assert_eq!(body["max_output_tokens"], 128);
        assert_eq!(body["input"][0]["content"], json!([]));
    }

    #[test]
    fn host_detectors_cover_kimi_and_xai() {
        assert!(is_kimi_chat_endpoint("https://api.kimi.com/coding/v1"));
        assert!(is_xai_chat_endpoint("https://api.x.ai/v1"));
        assert!(!is_kimi_chat_endpoint("https://opencode.ai/zen/go/v1"));
        assert!(is_zen_chat_endpoint("https://opencode.ai/zen/go/v1"));
    }

    #[test]
    fn chat_function_parameters_gain_object_type() {
        let mut body = json!({
            "tools": [{
                "type": "function",
                "function": {"name": "exec", "parameters": {"properties": {"cmd": {"type": "string"}}}}
            }]
        });
        ensure_chat_function_parameters(body.as_object_mut().unwrap());
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn kimi_tools_drop_redundant_types_beside_nested_refs() {
        let mut body = json!({
            "tools": [{
                "type": "function",
                "function": {
                    "name": "automation_update",
                    "parameters": {
                        "oneOf": [{"$ref": "#/$defs/action"}],
                        "$defs": {
                            "identifier": {"type": "string"},
                            "action": {
                                "type": "object",
                                "properties": {
                                    "id": {
                                        "$ref": "#/$defs/identifier",
                                        "type": "string"
                                    }
                                }
                            }
                        }
                    }
                }
            }]
        });

        sanitize_kimi_chat_tools(body.as_object_mut().unwrap());

        let parameters = &body["tools"][0]["function"]["parameters"];
        assert_eq!(parameters["type"], "object");
        assert_eq!(parameters["$defs"]["identifier"]["type"], "string");
        assert_eq!(
            parameters["$defs"]["action"]["properties"]["id"]["$ref"],
            "#/$defs/identifier"
        );
        assert!(parameters["$defs"]["action"]["properties"]["id"]
            .get("type")
            .is_none());
    }

    #[test]
    fn zen_tools_flatten_oneof_and_drop_encrypted() {
        let mut body = json!({
            "tools": [{
                "type": "function",
                "function": {
                    "name": "exec",
                    "parameters": {
                        "oneOf": [
                            {"properties": {"command": {"type": "string"}}, "required": ["command"]},
                            {"properties": {"script": {"type": ["string", "null"]}}}
                        ],
                        "encrypted": true,
                        "required": []
                    }
                }
            }]
        });
        let object = body.as_object_mut().unwrap();
        sanitize_zen_chat_tools(object);
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
        assert!(body["tools"][0]["function"]["parameters"]
            .get("oneOf")
            .is_none());
        assert!(body["tools"][0]["function"]["parameters"]
            .get("encrypted")
            .is_none());
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["properties"]["command"]["type"],
            "string"
        );
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["properties"]["script"]["nullable"],
            true
        );
    }

    #[test]
    fn spark_drops_hosted_image_generation() {
        let provider = chatgpt_provider();
        let mut body = json!({
            "model": "gpt-5.3-codex-spark",
            "tools": [
                {"type": "image_generation"},
                {"type": "function", "name": "exec", "parameters": {}}
            ],
            "input": []
        });
        sanitize_responses_upstream_request(&mut body, &provider, "gpt-5.3-codex-spark");
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["parameters"]["type"], "object");
    }

    #[test]
    fn invalid_response_item_ids_are_repaired_by_type_and_stay_stable() {
        let mut repair = ResponsesItemIdRepair::new_with_policy(
            &ResponseItemIdRepairSettings::default(), true,
        ).expect("repair enabled");
        let mut response = json!({"output": [
            {"type": "message", "id": "bad id", "content": []},
            {"type": "function_call", "id": "call", "call_id": "call_1", "name": "x", "arguments": "{}"},
            {"type": "tool_search_call", "arguments": {}}
        ]});
        repair.repair_response(&mut response);
        assert!(response["output"][0]["id"].as_str().unwrap().starts_with("msg_"));
        assert_eq!(response["output"][1]["id"], "call");
        assert!(response["output"][2].get("id").is_none());
        let first = response.clone();
        repair.repair_response(&mut response);
        assert_eq!(response, first);
        assert_eq!(response["output"][1]["call_id"], "call_1");
    }

    #[test]
    fn response_item_id_repair_uses_exact_raw_ids_and_only_positional_missing_fallback() {
        let settings = ResponseItemIdRepairSettings {
            message: vec!["placeholder".into()],
            reasoning: vec!["reasoning-placeholder".into()],
            repair_missing_terminal_ids: true,
        };
        let mut repair =
            ResponsesItemIdRepair::new_with_policy(&settings, true).expect("repair enabled");
        let mut added = json!({
            "type": "response.output_item.added", "output_index": 0,
            "item": {"type": "reasoning", "id": "reasoning-placeholder"}
        });
        repair.repair_event(&mut added);
        let repaired = added["item"]["id"].as_str().unwrap().to_string();

        let mut exact = json!({
            "type": "response.reasoning_text.delta", "output_index": 0,
            "item_id": "reasoning-placeholder", "delta": "thought"
        });
        repair.repair_event(&mut exact);
        assert_eq!(exact["item_id"], repaired);

        let mut foreign = json!({
            "type": "response.reasoning_text.delta", "output_index": 0,
            "item_id": "foreign-invalid-id", "delta": "foreign"
        });
        repair.repair_event(&mut foreign);
        assert_eq!(foreign["item_id"], "foreign-invalid-id");

        let mut missing = json!({
            "type": "response.content_part.done", "output_index": 0,
            "part": {"type": "reasoning_text", "text": "thought"}
        });
        repair.repair_event(&mut missing);
        assert_eq!(missing["item_id"], repaired);

        let mut valid_added = json!({
            "type": "response.output_item.added", "output_index": 1,
            "item": {"type": "message", "id": "msg_provider"}
        });
        repair.repair_event(&mut valid_added);
        let mut valid_missing = json!({
            "type": "response.output_text.done", "output_index": 1, "text": "answer"
        });
        repair.repair_event(&mut valid_missing);
        assert_eq!(valid_missing["item_id"], "msg_provider");

        let mut function = json!({
            "type": "response.output_item.done", "output_index": 2,
            "item": {"type": "function_call", "id": "bad function id",
                "call_id": "call_1", "name": "lookup", "arguments": "{}"}
        });
        repair.repair_event(&mut function);
        assert_eq!(function["item"]["id"], "bad function id");
    }
}
