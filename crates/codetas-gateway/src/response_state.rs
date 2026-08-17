use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Cap on stored response entries.
const MAX_STORED_RESPONSES: usize = 2_048;
/// High-water byte cap across all entries. The newest entry is always retained so a
/// single large, but valid, turn cannot immediately break its own continuation.
const MAX_STORED_RESPONSE_BYTES: usize = 256 * 1024 * 1024;
const MAX_PERSISTED_FILE_BYTES: u64 = (MAX_STORED_RESPONSE_BYTES as u64) + (16 * 1024 * 1024);
const PERSISTED_STATE_VERSION: u8 = 1;

#[derive(Clone, Deserialize, Serialize)]
struct StoredResponse {
    created_at_unix_ms: u64,
    items: Vec<Value>,
    size_bytes: usize,
}

#[derive(Deserialize, Serialize)]
struct PersistedResponseState {
    version: u8,
    responses: HashMap<String, StoredResponse>,
}

/// Proxy-internal continuation cache keyed by upstream response id.
///
/// The ChatGPT Codex backend rejects `previous_response_id`, so CODETAS strips it
/// before forwarding (see `sanitize_responses_upstream_request`). The Codex client,
/// however, continues turns by sending `previous_response_id` plus a delta `input`.
/// Without a local cache the upstream would receive only the delta and lose all prior
/// context — which makes a plan-mode model re-propose `update_plan` forever. This store
/// records completed responses (request input + response output) and expands a later
/// request's `previous_response_id` into the full replay.
#[derive(Clone)]
pub struct ResponseStateStore {
    inner: Arc<Mutex<HashMap<String, StoredResponse>>>,
    persistence_path: Option<Arc<PathBuf>>,
    persistence_lock: Arc<Mutex<()>>,
}

impl Default for ResponseStateStore {
    fn default() -> Self {
        Self::new(None)
    }
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
    pub(crate) fn new(persistence_path: Option<PathBuf>) -> Self {
        let mut inner = persistence_path
            .as_deref()
            .and_then(|path| match load_persisted_state(path) {
                Ok(responses) => Some(responses),
                Err(error) => {
                    crate::debug::log(&format!(
                        "cannot load response continuation state from {}: {error}",
                        path.display()
                    ));
                    None
                }
            })
            .unwrap_or_default();
        Self::enforce_limits_locked(&mut inner);
        Self {
            inner: Arc::new(Mutex::new(inner)),
            persistence_path: persistence_path.map(Arc::new),
            persistence_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Cache a completed upstream response under `response.id` for later replay.
    ///
    /// Only completed (or `max_output_tokens`
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
        let size_bytes = items.iter().map(serialized_bytes).sum::<usize>();
        // Serialize mutation and persistence together so concurrent completions cannot
        // write an older snapshot after a newer one.
        let _persistence_guard = self
            .persistence_path
            .as_ref()
            .map(|_| self.persistence_lock.lock().unwrap());
        let snapshot = {
            let mut inner = self.inner.lock().unwrap();
            let entry = StoredResponse {
                created_at_unix_ms: unix_millis(),
                items,
                size_bytes,
            };
            inner.remove(response_id);
            inner.insert(response_id.to_string(), entry);
            Self::enforce_limits_locked(&mut inner);
            self.persistence_path
                .as_ref()
                .map(|_| inner.clone())
        };
        if let (Some(path), Some(responses)) = (self.persistence_path.as_deref(), snapshot) {
            if let Err(error) = persist_state(path, responses) {
                crate::debug::log(&format!(
                    "cannot persist response continuation state to {}: {error}",
                    path.display()
                ));
            }
        }
    }

    /// Expand a request's `previous_response_id` into the full replayed input.
    ///
    /// Returns `true` when the stored items were prepended. When the id is unknown the
    /// body is left untouched (the caller forwards without expansion).
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
            let inner = self.inner.lock().unwrap();
            inner
                .get(&previous_id)
                .map(|entry| StoredResponse {
                    created_at_unix_ms: entry.created_at_unix_ms,
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

    /// Find a tool call only inside the exact continuation entry that was replayed.
    /// Call IDs are not conversation identifiers, so lookup must never scan the
    /// global cache.
    pub fn find_tool_call_in_response(
        &self,
        response_id: &str,
        call_id: &str,
        item_type: &str,
    ) -> Option<Value> {
        if response_id.is_empty() || call_id.is_empty() {
            return None;
        }
        let inner = self.inner.lock().unwrap();
        let entry = inner.get(response_id)?;
        entry.items.iter().find_map(|item| {
            (item.get("type").and_then(Value::as_str) == Some(item_type)
                && item.get("call_id").and_then(Value::as_str) == Some(call_id))
                .then(|| item.clone())
        })
    }

    fn enforce_limits_locked(inner: &mut HashMap<String, StoredResponse>) {
        while inner.len() > MAX_STORED_RESPONSES
            || (inner.len() > 1 && stored_bytes(inner) > MAX_STORED_RESPONSE_BYTES)
        {
            Self::evict_oldest_locked(inner, 1);
        }
    }

    fn evict_oldest_locked(inner: &mut HashMap<String, StoredResponse>, count: usize) {
        let mut ids = inner
            .iter()
            .map(|(id, entry)| (entry.created_at_unix_ms, id.clone()))
            .collect::<Vec<_>>();
        ids.sort();
        for (_, id) in ids.into_iter().take(count) {
            inner.remove(&id);
        }
    }
}

fn stored_bytes(inner: &HashMap<String, StoredResponse>) -> usize {
    inner
        .values()
        .fold(0_usize, |total, entry| total.saturating_add(entry.size_bytes))
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn load_persisted_state(path: &Path) -> Result<HashMap<String, StoredResponse>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HashMap::new())
        }
        Err(error) => return Err(error.to_string()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("state path is not a regular file".into());
    }
    if metadata.len() > MAX_PERSISTED_FILE_BYTES {
        return Err("state file exceeds the configured limit".into());
    }
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    let mut persisted: PersistedResponseState =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if persisted.version != PERSISTED_STATE_VERSION {
        return Err(format!("unsupported state version {}", persisted.version));
    }
    for entry in persisted.responses.values_mut() {
        entry.size_bytes = entry.items.iter().map(serialized_bytes).sum();
    }
    Ok(persisted.responses)
}

fn persist_state(
    path: &Path,
    responses: HashMap<String, StoredResponse>,
) -> Result<(), String> {
    let content = serde_json::to_vec(&PersistedResponseState {
        version: PERSISTED_STATE_VERSION,
        responses,
    })
    .map_err(|error| error.to_string())?;
    let parent = path
        .parent()
        .ok_or_else(|| "state path has no parent directory".to_string())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = parent.join(format!(
        ".response-state-{}-{}.tmp",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(|error| error.to_string())?;
    if let Err(error) = file.write_all(&content).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    if let Err(first_error) = fs::rename(&temporary, path) {
        if path.exists() {
            fs::remove_file(path).map_err(|error| error.to_string())?;
            fs::rename(&temporary, path).map_err(|error| error.to_string())?;
        } else {
            let _ = fs::remove_file(&temporary);
            return Err(first_error.to_string());
        }
    }
    Ok(())
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
    fn tool_call_lookup_is_scoped_to_one_response() {
        let store = ResponseStateStore::default();
        store.remember(
            &json!({"input": []}),
            &json!({
                "id": "resp_other",
                "status": "completed",
                "output": [{"type": "custom_tool_call", "call_id": "call_reused"}]
            }),
            true,
        );
        assert!(store
            .find_tool_call_in_response("resp_other", "call_reused", "custom_tool_call")
            .is_some());
        assert!(store
            .find_tool_call_in_response("resp_missing", "call_reused", "custom_tool_call")
            .is_none());
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

    #[test]
    fn persisted_state_survives_store_recreation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("response-state.json");
        {
            let store = ResponseStateStore::new(Some(path.clone()));
            store.remember(
                &json!({"input": [{"type": "message", "role": "user", "content": "before restart"}]}),
                &json!({"id": "resp_persisted", "status": "completed", "output": [{"type": "function_call", "call_id": "call_persisted", "name": "lookup", "arguments": "{}"}]}),
                true,
            );
        }

        let restored = ResponseStateStore::new(Some(path));
        let mut body = json!({
            "previous_response_id": "resp_persisted",
            "input": [{"type": "function_call_output", "call_id": "call_persisted", "output": "ok"}]
        });
        assert!(restored.expand_previous_response_input(&mut body));
        assert_eq!(body["input"][0]["content"], "before restart");
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["type"], "function_call_output");
    }

}
