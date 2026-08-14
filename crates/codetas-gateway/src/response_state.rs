use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Cap on stored response entries (mirrors opencodex's MAX_STORED_RESPONSES).
const MAX_STORED_RESPONSES: usize = 1_000;
/// In-memory high-water byte cap across all entries.
const MAX_STORED_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
/// Responses live in the store for at most one hour.
const RESPONSE_TTL: Duration = Duration::from_secs(60 * 60);

struct StoredResponse {
    created_at: Instant,
    items: Vec<Value>,
    size_bytes: usize,
}

/// Proxy-internal continuation cache keyed by upstream response id.
///
/// The ChatGPT Codex backend rejects `previous_response_id`, so CODETAS strips it
/// before forwarding (see `sanitize_responses_upstream_request`). The Codex client,
/// however, continues turns by sending `previous_response_id` plus a delta `input`.
/// Without a local cache the upstream would receive only the delta and lose all prior
/// context — which makes a plan-mode model re-propose `update_plan` forever. This store
/// records completed responses (request input + response output) and expands a later
/// request's `previous_response_id` into the full replay, mirroring opencodex's
/// `rememberResponseState` / `expandPreviousResponseInput`.
#[derive(Clone, Default)]
pub struct ResponseStateStore {
    inner: Arc<Mutex<HashMap<String, StoredResponse>>>,
}

fn input_items(input: &Value) -> Vec<Value> {
    match input {
        Value::Array(items) => items.clone(),
        Value::String(text) => vec![json!({"role": "user", "content": text})],
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    }
}

fn serialized_bytes(value: &Value) -> usize {
    serde_json::to_vec(value).map(|bytes| bytes.len()).unwrap_or(0)
}

impl ResponseStateStore {
    /// Cache a completed upstream response under `response.id` for later replay.
    ///
    /// Mirrors opencodex `rememberResponseState`: only completed (or `max_output_tokens`
    /// incomplete) responses are authoritative replay history, and `store:false` requests
    /// are skipped unless `force` is set (the ChatGPT forward path always forces, because
    /// Codex sends `store:false` on every request yet still chains via `previous_response_id`).
    pub fn remember(&self, request_body: &Value, response: &Value, force: bool) {
        let Some(response_id) = response.get("id").and_then(Value::as_str) else {
            return;
        };
        let Some(output) = response.get("output").and_then(Value::as_array) else {
            return;
        };
        let status = response.get("status").and_then(Value::as_str).unwrap_or("");
        if status == "incomplete" {
            let reason = response
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str);
            if reason != Some("max_output_tokens") {
                return;
            }
        } else if status != "completed" {
            return;
        }
        if request_body.get("store") == Some(&json!(false)) && !force {
            return;
        }
        let mut items = input_items(&request_body.get("input").unwrap_or(&Value::Null));
        items.extend(output.iter().cloned());
        let size_bytes: usize = items
            .iter()
            .map(serialized_bytes)
            .sum::<usize>()
            .min(MAX_STORED_RESPONSE_BYTES);
        let mut inner = self.inner.lock().unwrap();
        Self::prune_locked(&mut inner);
        let entry = StoredResponse {
            created_at: Instant::now(),
            items,
            size_bytes,
        };
        inner.remove(response_id);
        inner.insert(response_id.to_string(), entry);
        if inner.len() > MAX_STORED_RESPONSES {
            Self::evict_oldest_locked(&mut inner, 1);
        }
    }

    /// Expand a request's `previous_response_id` into the full replayed input.
    ///
    /// Returns `true` when the stored items were prepended. When the id is unknown the
    /// body is left untouched (the caller forwards without expansion, as opencodex does).
    pub fn expand_previous_response_input(&self, body: &mut Value) -> bool {
        let Some(previous_id) = body
            .get("previous_response_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return false;
        };
        if previous_id.is_empty() {
            return false;
        }
        let entry = {
            let mut inner = self.inner.lock().unwrap();
            Self::prune_locked(&mut inner);
            inner
                .get(&previous_id)
                .map(|entry| StoredResponse {
                    created_at: entry.created_at,
                    items: entry.items.clone(),
                    size_bytes: entry.size_bytes,
                })
        };
        let Some(entry) = entry else {
            return false;
        };
        let existing = input_items(&body.get("input").unwrap_or(&Value::Null));
        let mut items = entry.items;
        items.extend(existing);
        if let Some(object) = body.as_object_mut() {
            object.insert("input".into(), Value::Array(items));
        }
        true
    }

    /// Find a `custom_tool_call` item with the given `call_id` in the most recently
    /// stored responses. Codex executes client-side tools (`exec`) locally, so a
    /// follow-up HTTP request carries only the `custom_tool_call_output` delta without
    /// the paired call. Replaying the stored call next to its result keeps the pair
    /// intact for the ChatGPT backend (no orphaned-output 400) while preserving the
    /// tool result for the model (no user-message rewrite, no plan-mode loop).
    pub fn find_custom_tool_call(&self, call_id: &str) -> Option<Value> {
        if call_id.is_empty() {
            return None;
        }
        let mut inner = self.inner.lock().unwrap();
        Self::prune_locked(&mut inner);
        let mut entries: Vec<&StoredResponse> = inner.values().collect();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.created_at));
        for entry in entries {
            for item in &entry.items {
                if item.get("type").and_then(Value::as_str) != Some("custom_tool_call") {
                    continue;
                }
                if item.get("call_id").and_then(Value::as_str) == Some(call_id) {
                    return Some(item.clone());
                }
            }
        }
        None
    }

    fn prune_locked(inner: &mut HashMap<String, StoredResponse>) {
        let now = Instant::now();
        let mut expired = Vec::new();
        for (id, entry) in inner.iter() {
            if now.duration_since(entry.created_at) > RESPONSE_TTL {
                expired.push(id.clone());
            }
        }
        for id in expired {
            inner.remove(&id);
        }
    }

    fn evict_oldest_locked(inner: &mut HashMap<String, StoredResponse>, count: usize) {
        let mut ids = inner
            .iter()
            .map(|(id, entry)| (entry.created_at, id.clone()))
            .collect::<Vec<_>>();
        ids.sort();
        for (_, id) in ids.into_iter().take(count) {
            inner.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn remember_and_expand_restores_full_input() {
        let store = ResponseStateStore::default();
        let request = json!({
            "model": "gpt-5.6-sol",
            "store": false,
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
        });
        let response = json!({
            "id": "resp_1",
            "status": "completed",
            "output": [{"type": "function_call", "call_id": "call_1", "name": "update_plan"}]
        });
        // store:false without force is skipped.
        store.remember(&request, &response, false);
        let mut body = json!({
            "previous_response_id": "resp_1",
            "input": [{"type": "function_call_output", "call_id": "call_1", "output": "Plan updated"}]
        });
        assert!(!store.expand_previous_response_input(&mut body));

        // force records despite store:false.
        store.remember(&request, &response, true);
        assert!(store.expand_previous_response_input(&mut body));
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[2]["type"], "function_call_output");
    }

    #[test]
    fn only_completed_responses_are_recorded() {
        let store = ResponseStateStore::default();
        let request = json!({"input": [{"type": "message", "role": "user", "content": "hi"}]});
        store.remember(
            &request,
            &json!({"id": "resp_failed", "status": "failed", "output": []}),
            true,
        );
        store.remember(
            &request,
            &json!({"id": "resp_incomplete", "status": "incomplete", "output": [], "incomplete_details": {"reason": "content_filter"}}),
            true,
        );
        let mut body = json!({"previous_response_id": "resp_failed", "input": []});
        assert!(!store.expand_previous_response_input(&mut body));
        let mut body = json!({"previous_response_id": "resp_incomplete", "input": []});
        assert!(!store.expand_previous_response_input(&mut body));
    }

    #[test]
    fn expansion_of_unknown_id_leaves_body_untouched() {
        let store = ResponseStateStore::default();
        let mut body = json!({"previous_response_id": "resp_unknown", "input": [1, 2, 3]});
        assert!(!store.expand_previous_response_input(&mut body));
        assert_eq!(body["input"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn string_input_is_expanded_as_user_message() {
        let store = ResponseStateStore::default();
        store.remember(
            &json!({"input": "plain string"}),
            &json!({"id": "resp_1", "status": "completed", "output": []}),
            true,
        );
        let mut body = json!({"previous_response_id": "resp_1", "input": []});
        assert!(store.expand_previous_response_input(&mut body));
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"], "plain string");
    }

    #[test]
    fn expansion_prepends_before_delta_input() {
        let store = ResponseStateStore::default();
        store.remember(
            &json!({"input": [{"type": "message", "role": "user", "content": "a"}]}),
            &json!({"id": "resp_1", "status": "completed", "output": [{"type": "reasoning", "id": "rs_1"}]}),
            true,
        );
        let mut body = json!({
            "previous_response_id": "resp_1",
            "input": [{"type": "message", "role": "user", "content": "b"}]
        });
        assert!(store.expand_previous_response_input(&mut body));
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["content"], "a");
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[2]["content"], "b");
    }

    #[test]
    fn remember_replaces_existing_entry_and_evicts_oldest() {
        let store = ResponseStateStore::default();
        store.remember(
            &json!({"input": []}),
            &json!({"id": "resp_1", "status": "completed", "output": [1]}),
            true,
        );
        store.remember(
            &json!({"input": []}),
            &json!({"id": "resp_1", "status": "completed", "output": [2]}),
            true,
        );
        let mut body = json!({"previous_response_id": "resp_1", "input": []});
        assert!(store.expand_previous_response_input(&mut body));
        assert_eq!(body["input"][0], 2);
    }
}
