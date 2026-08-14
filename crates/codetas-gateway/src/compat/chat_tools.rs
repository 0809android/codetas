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
        function.insert(
            "parameters".into(),
            ensure_zen_root_object_schema(parameters),
        );
    }
}

pub fn sanitize_zen_chat_tools(body: &mut Map<String, Value>) {
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            let Some(function) = tool.get_mut("function").and_then(Value::as_object_mut) else {
                continue;
            };
            let parameters = function.remove("parameters").unwrap_or_else(|| json!({}));
            function.insert(
                "parameters".into(),
                ensure_zen_root_object_schema(parameters),
            );
        }
    }
}

fn ensure_zen_root_object_schema(schema: Value) -> Value {
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

