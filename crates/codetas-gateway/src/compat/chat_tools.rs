use super::*;

pub fn is_zen_chat_endpoint(base_url: &str) -> bool {
    let base = base_url.trim().trim_end_matches('/');
    base == "https://opencode.ai/zen/v1" || base == "https://opencode.ai/zen/go/v1"
}

pub fn is_kimi_chat_endpoint(base_url: &str) -> bool {
    url::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == "api.kimi.com" || host.ends_with(".kimi.com"))
}

pub fn is_xai_chat_endpoint(base_url: &str) -> bool {
    url::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == "api.x.ai" || host == "cli-chat-proxy.grok.com")
}

pub fn is_mistral_chat_endpoint(base_url: &str) -> bool {
    url::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == "api.mistral.ai")
}

/// Mistral answers 400 unless the last message comes from the user or a tool.
/// Chat translation merges a developer message into the leading system message,
/// so an instruction at the tail of a history that ends with the assistant, such
/// as the compaction prompt, would leave the assistant last. Sending it as the
/// user keeps it last. Call this on the wire copy only, never on stored history.
pub fn promote_trailing_instruction_to_user(body: &mut Value) {
    let Some(last) = body
        .get_mut("input")
        .and_then(Value::as_array_mut)
        .and_then(|items| items.last_mut())
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    let is_message = last
        .get("type")
        .and_then(Value::as_str)
        .is_none_or(|kind| kind == "message");
    let is_instruction = matches!(
        last.get("role").and_then(Value::as_str),
        Some("developer" | "system")
    );
    if is_message && is_instruction {
        last.insert("role".into(), Value::String("user".into()));
    }
}

const APPLY_PATCH_FORMAT_GUIDE: &str = "\
Edit files by passing a patch in the `input` string.

This is not a unified diff. Never write `--- a/file`, `+++ b/file` or `@@ -1,3 +1,4 @@` lines. \
The first line must be exactly `*** Begin Patch` and the last line must be exactly `*** End Patch`.

Between them, write one or more of these sections. Paths are relative.

`*** Add File: <path>` creates a file. Every following line of the file starts with `+`.

`*** Delete File: <path>` removes a file. Nothing follows.

`*** Update File: <path>` edits a file. An optional `*** Move to: <path>` line renames it. \
Then come one or more hunks. A hunk starts with `@@`, optionally followed by the enclosing \
function or class. Each of its lines starts with a space (unchanged), `-` (removed) or `+` \
(added). Include about three unchanged lines around each change so it can be located.

Example `input`:

*** Begin Patch
*** Add File: docs/hello.txt
+hello
+world
*** Update File: src/app.py
@@ def greet():
-    print(\"Hi\")
+    print(\"Hello\")
*** Delete File: old.txt
*** End Patch";

/// Codex declares `apply_patch` as a freeform tool whose format is a grammar. Chat
/// has no such tool type, so translation drops the grammar, and Codex's own
/// description says not to wrap the patch in JSON, which is what the translated
/// tool takes. Mistral's models do not know the format: they answered with a
/// unified diff, which is dropped as an invalid call, even with the grammar in the
/// description. With this worked example, all 12 live calls across the plan's four
/// models were valid. Codex's catalog cannot ask for a function-shaped tool
/// instead: 0.159.1 rejects any `apply_patch_tool_type` but `freeform`.
/// Call this on the wire copy only.
pub fn describe_apply_patch_format(body: &mut Value) {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools.iter_mut().filter_map(Value::as_object_mut) {
        let is_freeform_patch = tool.get("type").and_then(Value::as_str) == Some("custom")
            && tool.get("name").and_then(Value::as_str) == Some("apply_patch");
        if is_freeform_patch {
            tool.insert("description".into(), json!(APPLY_PATCH_FORMAT_GUIDE));
        }
    }
}

const RETRY_AFTER_INVALID_TOOL_CALL: &str = "Your previous response had no complete, valid tool \
call, so nothing was run. Continue the task. An `apply_patch` input must start with \
`*** Begin Patch` and end with `*** End Patch`.";

/// The other way a Chat history ends with the assistant, for Mistral's 400: when a
/// response ends `incomplete`, Codex retries with what it already recorded, such as
/// the progress text the gateway sent before it dropped an invalid tool call. Unlike
/// the compaction prompt, no instruction follows it, so one has to be added. A call
/// still waiting for its result is left alone, since only a tool message can follow.
/// Call this on the translated wire copy only, never on stored history.
pub fn continue_after_trailing_assistant(chat: &mut Value) {
    let Some(messages) = chat.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let Some(last) = messages.last() else {
        return;
    };
    let waits_for_tool_result = last
        .get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty());
    if last.get("role").and_then(Value::as_str) != Some("assistant") || waits_for_tool_result {
        return;
    }
    // Each failed attempt leaves its progress text in history, and adjacent assistant
    // messages are joined, so a later retry sees it repeated.
    let progress_only = last
        .get("content")
        .and_then(Value::as_str)
        .is_some_and(|text| {
            !text.trim().is_empty()
                && text
                    .replace(crate::translate::TOOL_PROGRESS_MESSAGE, "")
                    .trim()
                    .is_empty()
        });
    let content = if progress_only {
        RETRY_AFTER_INVALID_TOOL_CALL
    } else {
        "Continue."
    };
    messages.push(json!({"role": "user", "content": content}));
}

pub fn ensure_chat_function_parameters(body: &mut Map<String, Value>) {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools {
        let Some(function) = tool.get_mut("function") else {
            continue;
        };
        let needs_type = !function
            .pointer("/parameters/type")
            .and_then(Value::as_str)
            .is_some_and(|value| value == "object");
        if !needs_type {
            continue;
        }
        let mut parameters = match function.get("parameters") {
            Some(Value::Object(object)) => object.clone(),
            _ => Map::new(),
        };
        parameters.insert("type".into(), Value::String("object".into()));
        if let Some(object) = function.as_object_mut() {
            object.insert("parameters".into(), Value::Object(parameters));
        }
    }
}

pub fn sanitize_kimi_chat_tools(body: &mut Map<String, Value>) {
    ensure_chat_function_parameters(body);
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools {
        let Some(parameters) = tool.pointer_mut("/function/parameters") else {
            continue;
        };
        sanitize_kimi_schema_value(parameters, true);
    }
}

fn sanitize_kimi_schema_value(value: &mut Value, root: bool) {
    match value {
        Value::Array(items) => {
            for item in items {
                sanitize_kimi_schema_value(item, false);
            }
        }
        Value::Object(object) => {
            if !root && object.get("$ref").and_then(Value::as_str).is_some() {
                // Kimi's JSON Schema dialect rejects a `type` sibling on `$ref`.
                // The referenced definition remains authoritative for the type.
                object.remove("type");
            }
            for child in object.values_mut() {
                sanitize_kimi_schema_value(child, false);
            }
        }
        _ => {}
    }
}

pub fn sanitize_xai_chat_tools(body: &mut Map<String, Value>) {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools {
        let Some(function) = tool.get_mut("function").and_then(Value::as_object_mut) else {
            continue;
        };
        let parameters = function.remove("parameters").unwrap_or_else(|| json!({}));
        function.insert("parameters".into(), ensure_root_object_schema(parameters));
    }
}

/// Anthropic's Messages API rejects a tool whose `input_schema` has a
/// composition (`oneOf`/`anyOf`/`allOf`) at the top level:
///
/// ```text
/// tools.83.custom.input_schema: input_schema does not support oneOf, allOf,
/// or anyOf at the top level
/// ```
///
/// Flatten those compositions so the declared properties are still reachable,
/// rather than dropping the tool. Nested compositions are left untouched: only
/// the top level is rejected.
pub fn sanitize_anthropic_input_schemas(body: &mut Map<String, Value>) {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools {
        let Some(schema) = tool.get_mut("input_schema") else {
            continue;
        };
        let original = std::mem::replace(schema, Value::Null);
        *schema = flatten_root_composition(original);
    }
}

/// Flatten only a top-level `oneOf`/`anyOf`/`allOf` into the enclosing object
/// schema, leaving every other keyword exactly as the caller wrote it.
///
/// This is deliberately narrower than `ensure_root_object_schema`: Anthropic
/// only objects to the composition at the top level, so unrelated rewrites
/// (nullable widening, dropping unknown keywords) would change tool semantics
/// for no reason.
fn flatten_root_composition(schema: Value) -> Value {
    let Value::Object(mut object) = schema else {
        return json!({"type": "object"});
    };
    let composition_keys = ["oneOf", "anyOf", "allOf"];
    if !composition_keys
        .iter()
        .any(|key| object.get(*key).is_some_and(Value::is_array))
    {
        return Value::Object(object);
    }

    let mut properties = Map::new();
    if let Some(Value::Object(existing)) = object.get("properties") {
        properties = existing.clone();
    }
    let mut required = Vec::new();
    if let Some(Value::Array(existing)) = object.get("required") {
        required = existing
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
    }
    for key in composition_keys {
        let Some(Value::Array(variants)) = object.get(key) else {
            continue;
        };
        // `allOf` means every branch must hold, so its `required` entries stay
        // required. `oneOf`/`anyOf` are alternatives, so their `required` only
        // applies to the branch that was chosen and must not be hoisted.
        let merge_required = key == "allOf";
        for variant in variants {
            let Some(variant) = variant.as_object() else {
                continue;
            };
            if let Some(Value::Object(props)) = variant.get("properties") {
                for (name, value) in props {
                    properties.insert(name.clone(), value.clone());
                }
            }
            if merge_required {
                if let Some(Value::Array(values)) = variant.get("required") {
                    for name in values.iter().filter_map(Value::as_str) {
                        if !required.iter().any(|existing| existing == name) {
                            required.push(name.to_string());
                        }
                    }
                }
            }
        }
    }
    for key in composition_keys {
        object.remove(key);
    }
    object.insert("type".into(), Value::String("object".into()));
    if !properties.is_empty() {
        object.insert("properties".into(), Value::Object(properties));
    }
    if !required.is_empty() {
        object.insert(
            "required".into(),
            Value::Array(required.into_iter().map(Value::String).collect()),
        );
    }
    Value::Object(object)
}

pub fn sanitize_zen_chat_tools(body: &mut Map<String, Value>) {
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            let Some(function) = tool.get_mut("function").and_then(Value::as_object_mut) else {
                continue;
            };
            let parameters = function.remove("parameters").unwrap_or_else(|| json!({}));
            function.insert("parameters".into(), ensure_root_object_schema(parameters));
        }
    }
}

/// Flatten a schema whose top level is a `oneOf`/`anyOf`/`allOf` composition
/// into a single `object` schema.
///
/// Anthropic's `input_schema` rejects a top-level composition with
/// `input_schema does not support oneOf, allOf, or anyOf at the top level`,
/// and the Zen/xAI chat endpoints need the same shape, so both callers share
/// this normalization.
pub(crate) fn ensure_root_object_schema(schema: Value) -> Value {
    let Value::Object(mut object) = sanitize_zen_schema_value(schema) else {
        return json!({"type": "object"});
    };
    let has_composition = ["oneOf", "anyOf", "allOf"]
        .iter()
        .any(|key| object.get(*key).is_some_and(Value::is_array));
    if !has_composition {
        if object.get("type").and_then(Value::as_str) != Some("object") {
            object.insert("type".into(), Value::String("object".into()));
        }
        return Value::Object(object);
    }

    let mut properties = Map::new();
    if let Some(Value::Object(existing)) = object.get("properties") {
        properties = existing.clone();
    }
    let mut required = Vec::new();
    if let Some(Value::Array(existing)) = object.get("required") {
        required = existing
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
    }
    for key in ["oneOf", "anyOf", "allOf"] {
        let Some(Value::Array(variants)) = object.get(key) else {
            continue;
        };
        let merge_required = key == "allOf";
        for variant in variants {
            let Some(variant) = variant.as_object() else {
                continue;
            };
            if let Some(Value::Object(props)) = variant.get("properties") {
                for (name, value) in props {
                    properties.insert(name.clone(), value.clone());
                }
            }
            if merge_required {
                if let Some(Value::Array(values)) = variant.get("required") {
                    for name in values.iter().filter_map(Value::as_str) {
                        if !required.iter().any(|existing| existing == name) {
                            required.push(name.to_string());
                        }
                    }
                }
            }
        }
    }
    object.remove("oneOf");
    object.remove("anyOf");
    object.remove("allOf");
    object.insert("type".into(), Value::String("object".into()));
    if !properties.is_empty() {
        object.insert("properties".into(), Value::Object(properties));
    }
    if !required.is_empty() {
        object.insert(
            "required".into(),
            Value::Array(required.into_iter().map(Value::String).collect()),
        );
    }
    Value::Object(object)
}

fn sanitize_zen_schema_value(value: Value) -> Value {
    match value {
        Value::Array(items) => {
            Value::Array(items.into_iter().map(sanitize_zen_schema_value).collect())
        }
        Value::Object(object) => {
            let mut out = Map::new();
            for (key, child) in object {
                if key == "encrypted" {
                    continue;
                }
                if key == "required" && child.as_array().is_some_and(Vec::is_empty) {
                    continue;
                }
                if key == "type" {
                    if let Value::Array(types) = child {
                        let nullable = types.iter().any(|entry| entry.as_str() == Some("null"));
                        if let Some(first) = types
                            .iter()
                            .find(|entry| entry.as_str() != Some("null"))
                            .cloned()
                        {
                            out.insert("type".into(), first);
                        }
                        if nullable {
                            out.insert("nullable".into(), Value::Bool(true));
                        }
                        continue;
                    }
                    out.insert(key, child);
                    continue;
                }
                if matches!(key.as_str(), "properties" | "$defs" | "definitions") {
                    if let Value::Object(map) = child {
                        let cleaned = map
                            .into_iter()
                            .map(|(name, value)| (name, sanitize_zen_schema_value(value)))
                            .collect();
                        out.insert(key, Value::Object(cleaned));
                        continue;
                    }
                }
                out.insert(key, sanitize_zen_schema_value(child));
            }
            Value::Object(out)
        }
        other => other,
    }
}

pub(crate) fn ensure_function_parameters_object(tool: &mut Value) {
    if tool.get("type").and_then(Value::as_str) != Some("function") {
        return;
    }
    let needs_type = !tool
        .pointer("/parameters/type")
        .and_then(Value::as_str)
        .is_some_and(|value| value == "object");
    if !needs_type {
        return;
    }
    let mut parameters = match tool.get("parameters") {
        Some(Value::Object(object)) => object.clone(),
        _ => Map::new(),
    };
    parameters.insert("type".into(), Value::String("object".into()));
    if let Some(object) = tool.as_object_mut() {
        object.insert("parameters".into(), Value::Object(parameters));
    }
}

#[cfg(test)]
mod anthropic_input_schema_tests {
    use super::*;

    fn tools_body(tools: Value) -> Map<String, Value> {
        let mut body = Map::new();
        body.insert("tools".into(), tools);
        body
    }

    #[test]
    fn flattens_a_top_level_composition_anthropic_rejects() {
        // Anthropic answers this shape with 400 and
        // "input_schema does not support oneOf, allOf, or anyOf at the top level".
        let mut body = tools_body(json!([{
            "name": "custom_apply_patch",
            "input_schema": {
                "oneOf": [
                    {"type": "object", "properties": {"patch": {"type": "string"}}, "required": ["patch"]},
                    {"type": "object", "properties": {"note": {"type": "string"}}, "required": ["note"]}
                ]
            }
        }]));

        sanitize_anthropic_input_schemas(&mut body);

        let schema = &body["tools"][0]["input_schema"];
        assert!(schema.get("oneOf").is_none());
        assert!(schema.get("anyOf").is_none());
        assert!(schema.get("allOf").is_none());
        assert_eq!(schema["type"], "object");
        // Both branches stay reachable instead of losing a tool.
        assert_eq!(schema["properties"]["patch"]["type"], "string");
        assert_eq!(schema["properties"]["note"]["type"], "string");
        // `oneOf` branches are alternatives, so their `required` must not be hoisted.
        assert!(schema.get("required").is_none());
    }

    #[test]
    fn hoists_required_only_for_all_of() {
        let mut body = tools_body(json!([{
            "name": "tool",
            "input_schema": {
                "allOf": [
                    {"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]},
                    {"type": "object", "properties": {"b": {"type": "string"}}, "required": ["b"]}
                ]
            }
        }]));

        sanitize_anthropic_input_schemas(&mut body);

        let schema = &body["tools"][0]["input_schema"];
        assert_eq!(schema["type"], "object");
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required should be present")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(required, vec!["a", "b"]);
    }

    #[test]
    fn leaves_plain_object_schemas_byte_identical() {
        // A schema without a top-level composition must survive untouched,
        // including keywords this crate does not model.
        let original = json!({
            "type": "object",
            "properties": {
                "cmd": {"type": "string", "description": "shell command"},
                "nested": {"oneOf": [{"type": "string"}, {"type": "number"}]}
            },
            "required": ["cmd"],
            "additionalProperties": false,
            "$defs": {"unused": {"type": "null"}}
        });
        let mut body = tools_body(json!([{"name": "exec_command", "input_schema": original}]));

        sanitize_anthropic_input_schemas(&mut body);

        assert_eq!(body["tools"][0]["input_schema"], original);
    }

    #[test]
    fn ignores_tools_without_an_input_schema() {
        let mut body = tools_body(json!([{"name": "server_side_tool", "type": "web_search"}]));
        sanitize_anthropic_input_schemas(&mut body);
        assert_eq!(body["tools"][0]["name"], "server_side_tool");
        assert!(body["tools"][0].get("input_schema").is_none());
    }
}
