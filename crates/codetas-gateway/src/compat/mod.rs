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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

    fn from_event(event_type: &str) -> Option<Self> {
        match event_type {
            "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.annotation.added"
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
/// Only message and reasoning IDs are eligible. Function-call `id` and
/// `call_id` fields retain their upstream meaning and are never rewritten.
pub struct ResponsesItemIdRepair {
    settings: ResponseItemIdRepairSettings,
    message_ids: HashMap<usize, String>,
    reasoning_ids: HashMap<usize, String>,
    scope: String,
}

impl ResponsesItemIdRepair {
    pub fn new(settings: &ResponseItemIdRepairSettings) -> Option<Self> {
        if settings.message.is_empty()
            && settings.reasoning.is_empty()
            && !settings.repair_missing_terminal_ids
        {
            return None;
        }
        Some(Self {
            settings: settings.clone(),
            message_ids: HashMap::new(),
            reasoning_ids: HashMap::new(),
            scope: Uuid::new_v4().simple().to_string(),
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
            if let Some(item_type) = event
                .get("type")
                .and_then(Value::as_str)
                .and_then(RepairableItemType::from_event)
            {
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
        let mapped = if let Some(existing) = self.mapped_id(item_type, output_index) {
            existing.to_string()
        } else {
            let Some(current) = current.as_deref() else {
                return;
            };
            let mapped = if self.is_placeholder(item_type, current) {
                format!(
                    "{}codetas_{}_{}",
                    item_type.prefix(),
                    self.scope,
                    output_index
                )
            } else if self.settings.repair_missing_terminal_ids {
                current.to_string()
            } else {
                return;
            };
            self.remember_id(item_type, output_index, mapped.clone());
            mapped
        };

        if current.as_deref() == Some(mapped.as_str()) {
            return;
        }
        if current.is_none() && !self.settings.repair_missing_terminal_ids {
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
        let Some(mapped) = self.mapped_id(item_type, output_index) else {
            return;
        };
        let current = event.get("item_id").and_then(Value::as_str);
        if current == Some(mapped)
            || (current.is_none() && !self.settings.repair_missing_terminal_ids)
        {
            return;
        }
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

    fn mapped_id(&self, item_type: RepairableItemType, output_index: usize) -> Option<&str> {
        match item_type {
            RepairableItemType::Message => self.message_ids.get(&output_index),
            RepairableItemType::Reasoning => self.reasoning_ids.get(&output_index),
        }
        .map(String::as_str)
    }

    fn remember_id(&mut self, item_type: RepairableItemType, output_index: usize, value: String) {
        match item_type {
            RepairableItemType::Message => &mut self.message_ids,
            RepairableItemType::Reasoning => &mut self.reasoning_ids,
        }
        .insert(output_index, value);
    }
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
}
