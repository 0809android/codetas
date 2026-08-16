use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseToolKind {
    Function,
    Custom,
    ToolSearch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponseToolIdentity {
    pub name: String,
    pub namespace: Option<String>,
    pub kind: ResponseToolKind,
}

#[derive(Clone, Debug)]
pub(crate) struct ResponseToolEntry {
    pub(crate) identity: ResponseToolIdentity,
    pub(crate) declaration: Value,
    exposed: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ResponseToolMap {
    pub(crate) entries: BTreeMap<String, ResponseToolEntry>,
    order: Vec<String>,
}

impl ResponseToolMap {
    pub fn identity(&self, wire_name: &str) -> Option<&ResponseToolIdentity> {
        self.entries.get(wire_name).map(|entry| &entry.identity)
    }

    pub fn is_custom(&self, wire_name: &str) -> bool {
        self.identity(wire_name)
            .is_some_and(|identity| identity.kind == ResponseToolKind::Custom)
    }

    pub fn is_tool_search(&self, wire_name: &str) -> bool {
        self.identity(wire_name)
            .is_some_and(|identity| identity.kind == ResponseToolKind::ToolSearch)
    }

    pub fn custom_wire_names(&self) -> BTreeSet<String> {
        self.entries
            .iter()
            .filter_map(|(wire_name, entry)| {
                (entry.identity.kind == ResponseToolKind::Custom).then(|| wire_name.clone())
            })
            .collect()
    }

    pub fn wire_name_for_identity(
        &self,
        name: &str,
        namespace: Option<&str>,
        kind: ResponseToolKind,
    ) -> Option<&str> {
        self.order
            .iter()
            .find(|wire_name| {
                self.entries.get(*wire_name).is_some_and(|entry| {
                    entry.identity.name == name
                        && entry.identity.namespace.as_deref()
                            == namespace.filter(|value| !value.is_empty())
                        && entry.identity.kind == kind
                })
            })
            .map(String::as_str)
    }

    pub fn wire_name(namespace: Option<&str>, name: &str) -> String {
        qualify_response_tool_name(namespace.unwrap_or_default(), name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &ResponseToolIdentity, &Value)> {
        self.order.iter().filter_map(|wire_name| {
            self.entries.get(wire_name).and_then(|entry| entry.exposed.then(|| {
                (
                    wire_name.as_str(),
                    &entry.identity,
                    &entry.declaration,
                )
            }))
        })
    }

    fn insert(&mut self, wire_name: String, entry: ResponseToolEntry) {
        match self.entries.get(&wire_name) {
            None => {
                self.order.push(wire_name.clone());
                self.entries.insert(wire_name, entry);
            }
            Some(existing) if existing.identity == entry.identity => {}
            Some(_) => {
                // Keep the first declaration on the canonical wire name and
                // give later, distinct identities a stable suffix. This makes
                // every exposed name reversible without hiding either tool.
                let suffix = response_tool_identity_suffix(&entry.identity);
                let mut candidate = format!("{wire_name}__{suffix}");
                let mut discriminator = 2_u32;
                loop {
                    match self.entries.get(&candidate) {
                        None => {
                            self.order.push(candidate.clone());
                            self.entries.insert(candidate, entry);
                            break;
                        }
                        Some(existing) if existing.identity == entry.identity => break,
                        Some(_) => {
                            candidate = format!("{wire_name}__{suffix}_{discriminator}");
                            discriminator = discriminator.saturating_add(1);
                        }
                    }
                }
            }
        }
    }
}

pub(crate) fn tool_item(state: &ToolState, status: &str) -> Value {
    let completed = status == "completed";
    let mut item = match state.identity.kind {
        ResponseToolKind::Custom => {
            let mut item = json!({
                "id": state.item_id,
                "type": "custom_tool_call",
                "status": status,
                "call_id": state.call_id,
                "name": state.identity.name,
                "input": if completed {
                    unwrap_custom_tool_arguments(&state.arguments)
                } else {
                    String::new()
                }
            });
            insert_tool_namespace(&mut item, state.identity.namespace.as_deref());
            item
        }
        ResponseToolKind::ToolSearch => json!({
            "id": state.item_id,
            "type": "tool_search_call",
            "status": status,
            "call_id": state.call_id,
            "execution": "client",
            "arguments": if completed {
                arguments_to_value(&state.arguments)
            } else {
                json!({})
            }
        }),
        ResponseToolKind::Function => {
            let mut item = json!({
                "id": state.item_id,
                "type": "function_call",
                "status": status,
                "call_id": state.call_id,
                "name": state.identity.name,
                "arguments": if completed {
                    state.arguments.clone()
                } else {
                    String::new()
                }
            });
            insert_tool_namespace(&mut item, state.identity.namespace.as_deref());
            item
        }
    };
    if let Some(metadata) = &state.provider_metadata {
        item["provider_metadata"] = metadata.clone();
    }
    item
}

pub fn sse(event: &str, payload: &Value) -> String {
    format!(
        "event: {event}\ndata: {}\n\n",
        serde_json::to_string(payload).unwrap_or_else(|_| "{}".into())
    )
}

/// Builds the canonical mapping between the Responses declaration identity and
/// the flat function name exposed to Chat Completions.
///
/// The top-level tool list is authoritative over `additional_tools`; identical
/// duplicates keep the first schema. Distinct identities that collapse onto
/// the same canonical wire name receive a stable suffix so both remain
/// available and independently reversible.
pub fn response_tool_map(body: &Value) -> ResponseToolMap {
    let mut map = ResponseToolMap::default();
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        collect_response_tools(tools, &mut map);
    }
    if let Some(input) = body.get("input").and_then(Value::as_array) {
        for item in input {
            match item.get("type").and_then(Value::as_str) {
                Some("additional_tools" | "tool_search_output") => {
                    if let Some(tools) = item.get("tools").and_then(Value::as_array) {
                        collect_response_tools(tools, &mut map);
                    }
                }
                _ => {}
            }
        }
        for item in input {
            collect_response_history_tool(item, &mut map);
        }
    }
    map
}

pub(crate) fn response_allowed_tool_wire_names(
    choice: &Value,
    tool_map: &ResponseToolMap,
) -> Option<BTreeSet<String>> {
    if choice.get("type").and_then(Value::as_str) != Some("allowed_tools") {
        return None;
    }
    let mut allowed = BTreeSet::new();
    for tool in choice
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(object) = tool.as_object() else {
            continue;
        };
        let wire_name = match object.get("type").and_then(Value::as_str) {
            Some("function" | "custom") => {
                let Some(name) = object.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let namespace = object.get("namespace").and_then(Value::as_str);
                let kind = if object.get("type").and_then(Value::as_str) == Some("custom") {
                    ResponseToolKind::Custom
                } else {
                    ResponseToolKind::Function
                };
                tool_map
                    .wire_name_for_identity(name, namespace, kind)
                    .map(str::to_string)
                    .unwrap_or_else(|| ResponseToolMap::wire_name(namespace, name))
            }
            Some("tool_search") => tool_map
                .wire_name_for_identity("tool_search", None, ResponseToolKind::ToolSearch)
                .unwrap_or("tool_search")
                .to_string(),
            _ => continue,
        };
        if tool_map
            .iter()
            .any(|(exposed_name, _, _)| exposed_name == wire_name.as_str())
        {
            allowed.insert(wire_name);
        }
    }
    Some(allowed)
}

/// Compatibility helper for adapters that only need freeform classification.
pub fn custom_tool_names(body: &Value) -> BTreeSet<String> {
    response_tool_map(body).custom_wire_names()
}

fn collect_response_tools(tools: &[Value], map: &mut ResponseToolMap) {
    for tool in tools {
        let Some(object) = tool.as_object() else {
            continue;
        };
        let tool_type = object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("function");
        if tool_type == "namespace" {
            let namespace = object
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim();
            let Some(children) = object.get("tools").and_then(Value::as_array) else {
                continue;
            };
            // Responses namespaces are one level deep. Nested namespace tools
            // are not representable and are intentionally ignored.
            for child in children {
                collect_response_tool(child, Some(namespace), map);
            }
            continue;
        }
        collect_response_tool(tool, None, map);
    }
}

fn collect_response_tool(tool: &Value, namespace: Option<&str>, map: &mut ResponseToolMap) {
    let Some(object) = tool.as_object() else {
        return;
    };
    let tool_type = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("function");
    let kind = match tool_type {
        "" | "function" => ResponseToolKind::Function,
        "custom" => ResponseToolKind::Custom,
        "tool_search" if namespace.is_none() => ResponseToolKind::ToolSearch,
        _ => return,
    };
    let default_name = (kind == ResponseToolKind::ToolSearch).then_some("tool_search");
    let Some(name) = object
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| tool.pointer("/function/name").and_then(Value::as_str))
        .or(default_name)
        .map(str::trim)
        .filter(|name| !name.is_empty())
    else {
        return;
    };
    let namespace = namespace
        .map(str::trim)
        .filter(|namespace| !namespace.is_empty())
        .map(str::to_string);
    let wire_name = qualify_response_tool_name(namespace.as_deref().unwrap_or_default(), name);
    map.insert(
        wire_name,
        ResponseToolEntry {
            identity: ResponseToolIdentity {
                name: name.to_string(),
                namespace,
                kind,
            },
            declaration: tool.clone(),
            exposed: true,
        },
    );
}

fn collect_response_history_tool(item: &Value, map: &mut ResponseToolMap) {
    let Some(object) = item.as_object() else {
        return;
    };
    let kind = match object.get("type").and_then(Value::as_str) {
        Some("function_call") => ResponseToolKind::Function,
        Some("custom_tool_call") => ResponseToolKind::Custom,
        Some("tool_search_call") => ResponseToolKind::ToolSearch,
        _ => return,
    };
    let default_name = (kind == ResponseToolKind::ToolSearch).then_some("tool_search");
    let Some(name) = object
        .get("name")
        .and_then(Value::as_str)
        .or(default_name)
        .map(str::trim)
        .filter(|name| !name.is_empty())
    else {
        return;
    };
    let namespace = object
        .get("namespace")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|namespace| !namespace.is_empty())
        .map(str::to_string);
    let wire_name = qualify_response_tool_name(namespace.as_deref().unwrap_or_default(), name);
    map.insert(
        wire_name,
        ResponseToolEntry {
            identity: ResponseToolIdentity {
                name: name.to_string(),
                namespace,
                kind,
            },
            declaration: item.clone(),
            exposed: false,
        },
    );
}

pub(crate) fn qualify_response_tool_name(namespace: &str, name: &str) -> String {
    let namespace = namespace.trim();
    let name = name.trim();
    if namespace.is_empty() || name.is_empty() {
        return name.to_string();
    }
    if namespace.ends_with("__") {
        format!("{namespace}{name}")
    } else {
        format!("{namespace}__{name}")
    }
}

fn response_tool_identity_suffix(identity: &ResponseToolIdentity) -> String {
    // FNV-1a is deliberately simple and stable across processes/toolchains.
    let mut hash = 0xcbf29ce484222325_u64;
    let kind = match identity.kind {
        ResponseToolKind::Function => "function",
        ResponseToolKind::Custom => "custom",
        ResponseToolKind::ToolSearch => "tool_search",
    };
    for byte in kind
        .bytes()
        .chain([0])
        .chain(
            identity
                .namespace
                .as_deref()
                .unwrap_or_default()
                .bytes(),
        )
        .chain([0])
        .chain(identity.name.bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:08x}", hash as u32)
}

pub(crate) fn insert_tool_namespace(item: &mut Value, namespace: Option<&str>) {
    if let Some(namespace) = namespace.filter(|value| !value.is_empty()) {
        item["namespace"] = Value::String(namespace.to_string());
    }
}

pub(crate) fn unwrap_custom_tool_arguments(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| value.get("input").cloned())
        .map(|input| match input {
            Value::String(text) => text,
            other => serde_json::to_string(&other).unwrap_or_default(),
        })
        .unwrap_or_else(|| arguments.to_string())
}

pub(crate) fn arguments_to_value(arguments: &str) -> Value {
    serde_json::from_str(arguments).unwrap_or_else(|_| json!({"input": arguments}))
}

pub(crate) fn response_tool_parameters(tool: &Value) -> Option<Value> {
    tool.get("parameters")
        .or_else(|| tool.get("input_schema"))
        .or_else(|| tool.get("inputSchema"))
        .or_else(|| tool.pointer("/function/parameters"))
        .cloned()
}

pub(crate) fn default_tool_search_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Search query for tools to load."
            },
            "limit": {
                "type": "number",
                "description": "Maximum number of tools to return."
            }
        },
        "required": ["query"]
    })
}

pub(crate) fn tool_search_output_to_text(item: &Value, tool_map: &ResponseToolMap) -> String {
    let dynamic_body = json!({
        "tools": item.get("tools").cloned().unwrap_or_else(|| json!([]))
    });
    let dynamic_tool_map = response_tool_map(&dynamic_body);
    let names = dynamic_tool_map
        .iter()
        .map(|(wire_name, identity, _)| {
            tool_map
                .wire_name_for_identity(
                    &identity.name,
                    identity.namespace.as_deref(),
                    identity.kind,
                )
                .unwrap_or(wire_name)
                .to_string()
        })
        .collect::<Vec<_>>();
    if names.is_empty() {
        "Tool search returned no tools.".to_string()
    } else {
        format!("Tool search loaded: {}", names.join(", "))
    }
}

pub(crate) fn chat_usage_to_responses(usage: Option<&Value>) -> Value {
    let prompt = usage
        .and_then(|value| value.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion = usage
        .and_then(|value| value.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .and_then(|value| value.pointer("/prompt_tokens_details/cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = usage
        .and_then(|value| value.pointer("/completion_tokens_details/reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = usage
        .and_then(|value| value.get("total_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(prompt + completion);
    json!({
        "input_tokens": prompt,
        "input_tokens_details": {"cached_tokens": cached},
        "output_tokens": completion,
        "output_tokens_details": {"reasoning_tokens": reasoning},
        "total_tokens": total
    })
}

pub(crate) fn response_id() -> String {
    format!("resp_{}", Uuid::new_v4().simple())
}

pub(crate) fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}
