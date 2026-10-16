//! The image boundary is rendered history, never schemas, arguments or metadata.
use serde_json::Value;
use std::collections::HashSet;

#[derive(Clone, Copy)]
enum Scope {
    Root,
    History,
    Content,
    ToolOutput,
    ToolPart,
    FunctionResponse,
    KiroState,
    KiroTurn,
    KiroUser,
}
fn tag(value: &Value) -> u8 {
    match value.get("type").and_then(Value::as_str) {
        Some("function_call_output" | "custom_tool_call_output") => 1,
        Some("tool_result") => 2,
        Some(
            "function_call" | "custom_tool_call" | "tool_search_call" | "tool_search_output" | "local_shell_call"
            | "additional_tools" | "reasoning" | "compaction" | "compaction_summary"
            | "context_compaction" | "tool_use" | "thinking" | "redacted_thinking",
        ) => 3,
        _ => 0,
    }
}
fn child(scope: Scope, kind: u8, key: &str) -> Option<Scope> {
    match scope {
        Scope::Root if kind == 3 => None,
        Scope::Root if kind == 1 && key == "output" => Some(Scope::ToolOutput),
        Scope::Root => match key {
            "input" | "messages" | "contents" => Some(Scope::History),
            "content" => Some(Scope::Content),
            "request" => Some(Scope::Root),
            "conversationState" => Some(Scope::KiroState),
            _ => None,
        },
        Scope::History => {
            if kind == 3 {
                None
            } else if kind == 1 {
                if key == "output" {
                    Some(Scope::ToolOutput)
                } else {
                    None
                }
            } else {
                match key {
                    "content" | "parts" => Some(Scope::Content),
                    _ => None,
                }
            }
        }
        Scope::Content => {
            if kind == 3 {
                None
            } else if kind == 2 && key == "content" {
                Some(Scope::Content)
            } else if key == "functionResponse" {
                Some(Scope::FunctionResponse)
            } else {
                None
            }
        }
        Scope::ToolOutput | Scope::ToolPart => None,
        Scope::FunctionResponse => {
            if key == "parts" {
                Some(Scope::Content)
            } else {
                None
            }
        }
        Scope::KiroState => {
            if matches!(key, "history" | "currentMessage") {
                Some(Scope::KiroTurn)
            } else {
                None
            }
        }
        Scope::KiroTurn => {
            if key == "userInputMessage" {
                Some(Scope::KiroUser)
            } else {
                None
            }
        }
        Scope::KiroUser => {
            if key == "images" {
                Some(Scope::Content)
            } else {
                None
            }
        }
    }
}
fn walk<'a>(value: &'a Value, scope: Scope, out: &mut Vec<&'a Value>, sent: bool) {
    if matches!(scope, Scope::ToolPart)
        && !matches!(value.get("type").and_then(Value::as_str), Some("input_image" | "image_url")) { return; }
    if !matches!(scope, Scope::ToolOutput) && (super::image_history::is_scoped_image_leaf(value)
        || sent && (super::image_history::is_reference_image_leaf(value)
            || matches!(value.get("type").and_then(Value::as_str), Some("input_image" | "image_url")))) {
        out.push(value);
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                walk(
                    item,
                    if matches!(scope, Scope::Root) {
                        Scope::History
                    } else if matches!(scope, Scope::ToolOutput) {
                        Scope::ToolPart
                    } else {
                        scope
                    },
                    out,
                    sent,
                );
            }
        }
        Value::Object(map) => {
            let kind = tag(value);
            for (key, value) in map {
                if let Some(next) = child(scope, kind, key) {
                    walk(value, next, out, sent);
                }
            }
        }
        _ => {}
    }
}
fn walk_mut<'a>(value: &'a mut Value, scope: Scope, out: &mut Vec<&'a mut Value>) {
    if matches!(scope, Scope::ToolPart)
        && !matches!(value.get("type").and_then(Value::as_str), Some("input_image" | "image_url")) { return; }
    if !matches!(scope, Scope::ToolOutput) && super::image_history::is_scoped_image_leaf(value) {
        out.push(value);
        return;
    }
    let kind = tag(value);
    match value {
        Value::Array(items) => {
            for item in items {
                walk_mut(
                    item,
                    if matches!(scope, Scope::Root) {
                        Scope::History
                    } else if matches!(scope, Scope::ToolOutput) {
                        Scope::ToolPart
                    } else {
                        scope
                    },
                    out,
                );
            }
        }
        Value::Object(map) => {
            for (key, value) in map {
                if let Some(next) = child(scope, kind, key) {
                    walk_mut(value, next, out);
                }
            }
        }
        _ => {}
    }
}
pub(crate) fn rendered_image_nodes(value: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    walk(value, Scope::Root, &mut out, false);
    out
}
/// Count physically rendered images, including references without inline bytes.
pub(crate) fn count_rendered_sent_images(value: &Value) -> usize {
    let mut out = Vec::new();
    walk(value, Scope::Root, &mut out, true);
    out.len()
}
pub(crate) fn rendered_image_nodes_mut(value: &mut Value) -> Vec<&mut Value> {
    let mut out = Vec::new();
    walk_mut(value, Scope::Root, &mut out);
    out
}
pub(crate) fn rendered_input_image_addresses(items: &[Value]) -> HashSet<usize> {
    let mut out = Vec::new();
    for item in items {
        walk(item, Scope::History, &mut out, true);
    }
    out.into_iter()
        .filter_map(|v| v.as_object())
        .map(|map| map as *const _ as usize)
        .collect()
}

/// Admission visits rendered history only. Strings are always text.
#[derive(Default)]
pub(super) struct AdmissionScope {
    pub nodes: HashSet<usize>,
    pub images: HashSet<usize>,
}
#[derive(Clone, Copy)]
enum AdmissionPosition {
    Rendered(Scope),
    ToolParts,
    Caption,
}
pub(super) fn admission_scope(value: &Value) -> AdmissionScope {
    fn visit(value: &Value, position: AdmissionPosition, out: &mut AdmissionScope) {
        // A standalone tool-output object is serialized as data, not rendered
        // as a content block. Do not admit it (or any of its fields) as an image.
        if matches!(position, AdmissionPosition::Rendered(Scope::ToolOutput)) && value.is_object() {
            return;
        }
        let id = value as *const _ as usize;
        out.nodes.insert(id);
        if value.is_string() { return; }
        if super::image_history::is_scoped_image_leaf(value) {
            return;
        }
        match value {
            Value::Array(items) => {
                let next = match position {
                    AdmissionPosition::Rendered(Scope::Root) => {
                        AdmissionPosition::Rendered(Scope::History)
                    }
                    AdmissionPosition::Rendered(Scope::ToolOutput) => AdmissionPosition::ToolParts,
                    _ => position,
                };
                for item in items {
                    visit(item, next, out);
                }
            }
            Value::Object(map) => {
                let kind = tag(value);
                for (key, value) in map {
                    let next = match position {
                        AdmissionPosition::Caption => None,
                        AdmissionPosition::ToolParts => {
                            if key == "text" && kind == 0 {
                                Some(AdmissionPosition::Caption)
                            } else {
                                child(Scope::Content, kind, key).map(AdmissionPosition::Rendered)
                            }
                        }
                        AdmissionPosition::Rendered(scope) => {
                            if matches!(scope, Scope::Content) && key == "text" && kind == 0 {
                                Some(AdmissionPosition::Caption)
                            } else {
                                child(scope, kind, key).map(AdmissionPosition::Rendered)
                            }
                        }
                    };
                    if let Some(next) = next {
                        visit(value, next, out);
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = AdmissionScope::default();
    // Counting and rewriting share the exact rendered-image boundary.
    out.images = rendered_image_nodes(value).into_iter().map(|v| v as *const _ as usize).collect();
    visit(value, AdmissionPosition::Rendered(Scope::Root), &mut out);
    out
}


#[cfg(test)]
mod review18_regressions {
    use super::*;
    use serde_json::json;
    #[test]
    fn review18_sent_images_include_references_only_in_rendered_slots() {
        let image = json!({"type":"input_image","image_url":"https://example.invalid/a.jpg"});
        let body = json!({"input":[{"role":"user","content":[image]},
            {"type":"function_call","arguments":image},
            {"type":"custom_tool_call","input":image},
            {"type":"function_call_output","output":image},
            {"type":"custom_tool_call_output","output":image.to_string()},
            {"type":"function_call_output","output":[image,{"type":"text","text":image.to_string()}]}],
            "tools":[{"parameters":{"const":image}}],"metadata":image});
        assert_eq!(count_rendered_sent_images(&body), 2);
        assert_eq!(super::super::count_translated_input_images(&body), 0);
        for body in [
            json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.invalid/a"}}]}]}),
            json!({"messages":[{"role":"user","content":[{"type":"tool_result","content":[{"type":"image","source":{"type":"url","url":"https://example.invalid/a"}}]}]}]}),
            json!({"request":{"contents":[{"parts":[{"fileData":{"fileUri":"gs://fixture/a"}}]}]}}),
            json!({"contents":[{"parts":[{"file_data":{"file_uri":"gs://fixture/a","mime_type":"image/png"}}]}]}),
            json!({"input":[{"role":"user","content":[{"type":"input_image","file_id":"file-fixture"}]}]}),
        ] { assert_eq!(count_rendered_sent_images(&body), 1, "{body}"); assert_eq!(super::super::count_translated_input_images(&body), 0); }
        assert_eq!(count_rendered_sent_images(&json!({"contents":[{"parts":[{"fileData":{"fileUri":"gs://fixture/a","mimeType":"video/mp4"}}]}]})), 0);
        let inline = json!({"input":[{"role":"user","content":[{"type":"input_image","image_url":"data:image/jpeg;base64,YWJj"}]}]});
        assert_eq!(count_rendered_sent_images(&inline), 1);
        assert_eq!(super::super::count_translated_input_images(&inline), 1);
    }
}

/// Reject opaque images before a translator can silently discard their source.
/// Responses passthrough does not call this validator.
pub(crate) fn validate_translated_image_references(body: &Value) -> Result<(), String> {
    let rendered = crate::compaction::rendered_compaction_view(body)?;
    let mut images = Vec::new();
    walk(&rendered, Scope::Root, &mut images, true);
    if images.into_iter().any(|image| super::image_history::is_reference_image_leaf(image)
        || matches!(image.get("type").and_then(Value::as_str), Some("input_image" | "image_url"))
            && image.get("image_url").and_then(|url| url.as_str().or_else(|| url.get("url").and_then(Value::as_str)))
                .or_else(|| image.get("url").and_then(Value::as_str)).is_none_or(str::is_empty)) {
        return Err("unsupported image reference for this translated provider; supply a supported image URL or inline image".into());
    }
    Ok(())
}
