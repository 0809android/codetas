use super::*;

/// Mistral types assistant `content` as `str | list[chunk]`. A model asked for
/// `reasoning_effort` answers in the list form, mixing `thinking` and `text`
/// chunks in one delta, and it may switch forms mid-stream. Returns the chunks
/// in order, under the field the translators read for each.
fn content_list_parts(chunks: &[Value]) -> Vec<(&'static str, String)> {
    let mut parts = Vec::new();
    for chunk in chunks {
        match chunk.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = chunk.get("text").and_then(Value::as_str) {
                    parts.push(("content", text.to_string()));
                }
            }
            Some("thinking") => {
                let text = match chunk.get("thinking") {
                    Some(Value::String(text)) => text.clone(),
                    Some(Value::Array(items)) => items
                        .iter()
                        .filter_map(|item| {
                            item.as_str()
                                .or_else(|| item.get("text").and_then(Value::as_str))
                        })
                        .collect(),
                    _ => String::new(),
                };
                parts.push(("reasoning_content", text));
            }
            _ => {}
        }
    }
    parts.retain(|(_, text)| !text.is_empty());
    parts
}

/// Splits a chunk whose delta content is a list into ordered string deltas.
/// Everything else on the chunk (tool calls, finish reason, usage) stays on a
/// final chunk, so reasoning still precedes the text that follows it.
pub(crate) fn split_chunk_content_list(chunk: &Value) -> Option<Vec<Value>> {
    let list = chunk.pointer("/choices/0/delta/content")?.as_array()?;
    let mut parts: Vec<Value> = content_list_parts(list)
        .into_iter()
        .map(|(field, text)| json!({"choices": [{"delta": {field: text}}]}))
        .collect();
    let mut rest = chunk.clone();
    rest.pointer_mut("/choices/0/delta")
        .and_then(Value::as_object_mut)?
        .remove("content");
    parts.push(rest);
    Some(parts)
}

/// The non-streaming counterpart of [`split_chunk_content_list`].
pub(crate) fn flatten_message_content_list(
    message: &Map<String, Value>,
) -> Option<Map<String, Value>> {
    let list = message.get("content")?.as_array()?;
    let mut content = String::new();
    let mut reasoning = String::new();
    for (field, text) in content_list_parts(list) {
        if field == "content" {
            content.push_str(&text);
        } else {
            reasoning.push_str(&text);
        }
    }
    let mut flattened = message.clone();
    flattened.insert("content".into(), Value::String(content));
    if !reasoning.is_empty() {
        flattened.insert("reasoning_content".into(), Value::String(reasoning));
    }
    Some(flattened)
}

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
            .filter(|&(_wire_name, entry)| entry.identity.kind == ResponseToolKind::Custom)
            .map(|(wire_name, _entry)| wire_name.clone())
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
            self.entries.get(wire_name).and_then(|entry| {
                entry
                    .exposed
                    .then_some((wire_name.as_str(), &entry.identity, &entry.declaration))
            })
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
                let mut candidate = bound_response_tool_name(&format!("{wire_name}__{suffix}"));
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
                            candidate = bound_response_tool_name(&format!(
                                "{wire_name}__{suffix}_{discriminator}"
                            ));
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
                    unwrap_custom_tool_arguments(&state.identity.name, &state.arguments)
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
    let mut expanded;
    let body = if body
        .get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(crate::anthropic_compaction::is_item))
    {
        expanded = body.clone();
        let _ = crate::anthropic_compaction::expand_retained(&mut expanded);
        &expanded
    } else {
        body
    };
    let mut map = ResponseToolMap::default();
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        collect_response_tools(tools, &mut map);
    }
    if let Some(input) = body.get("input").and_then(Value::as_array) {
        for item in input {
            if let Some("additional_tools" | "tool_search_output") =
                item.get("type").and_then(Value::as_str)
            {
                if let Some(tools) = item.get("tools").and_then(Value::as_array) {
                    collect_response_tools(tools, &mut map);
                }
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
    let full_name = if namespace.is_empty() || name.is_empty() {
        name.to_string()
    } else if namespace.ends_with("__") {
        format!("{namespace}{name}")
    } else {
        format!("{namespace}__{name}")
    };
    bound_response_tool_name(&full_name)
}

const MAX_RESPONSE_TOOL_NAME_BYTES: usize = 64;

fn bound_response_tool_name(name: &str) -> String {
    if name.len() <= MAX_RESPONSE_TOOL_NAME_BYTES {
        return name.to_string();
    }

    // Keep the prefix recognizable while making the alias deterministic and
    // reversible through ResponseToolMap. Chat-compatible providers commonly
    // enforce the OpenAI function-name limit of 64 bytes.
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in name.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let suffix = format!("__{hash:016x}");
    let prefix_budget = MAX_RESPONSE_TOOL_NAME_BYTES.saturating_sub(suffix.len());
    let prefix = name
        .char_indices()
        .take_while(|(index, character)| index + character.len_utf8() <= prefix_budget)
        .map(|(_, character)| character)
        .collect::<String>();
    format!("{prefix}{suffix}")
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
        .chain(identity.namespace.as_deref().unwrap_or_default().bytes())
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

pub(crate) fn unwrap_custom_tool_arguments(name: &str, arguments: &str) -> String {
    unwrap_named_custom_tool_arguments(name, arguments)
}

fn unwrap_named_custom_tool_arguments(name: &str, arguments: &str) -> String {
    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(map)) => {
            let chosen = if name == "apply_patch" {
                map.get("input")
                    .cloned()
                    .or_else(|| map.get("patch").cloned())
            } else {
                map.get("input").cloned()
            };
            match chosen {
                Some(Value::String(text)) => text,
                Some(other) => serde_json::to_string(&other).unwrap_or_default(),
                // apply_patch wrappers without a patch body are empty, not "{}".
                None if name == "apply_patch" => String::new(),
                None => arguments.to_string(),
            }
        }
        Ok(Value::String(text)) => text,
        _ => partial_named_custom_tool_argument(name, arguments)
            .unwrap_or_else(|| arguments.to_string()),
    }
}

/// Some Chat Completions providers stream a JSON wrapper around a custom
/// tool's freeform input, but terminate the tool call before the wrapper's
/// closing quote/braces arrive. Recover the string value without treating the
/// incomplete wrapper itself as the tool input.
fn partial_named_custom_tool_argument(name: &str, arguments: &str) -> Option<String> {
    let key = if name == "apply_patch" {
        ["input", "patch"]
            .into_iter()
            .find(|key| partial_json_string_value(arguments, key).is_some())?
    } else {
        "input"
    };
    partial_json_string_value(arguments, key)
}

fn partial_json_string_value(arguments: &str, key: &str) -> Option<String> {
    let marker = format!("\"{key}\"");
    let start = arguments.find(&marker)? + marker.len();
    let rest = arguments[start..].trim_start();
    let value = rest.strip_prefix(':')?.trim_start().strip_prefix('"')?;
    let candidate = format!("\"{value}\"");
    serde_json::from_str(&candidate).ok()
}

/// Custom tools are freeform text. An announced name plus a finish marker is
/// not enough: empty `apply_patch` input still reaches Codex and is executed.
pub(crate) fn custom_tool_input_is_actionable(name: &str, arguments: &str) -> bool {
    if name != "apply_patch"
        && serde_json::from_str::<Value>(arguments)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .is_some_and(|object| object.contains_key("patch") && !object.contains_key("input"))
    {
        return false;
    }
    let input = unwrap_custom_tool_arguments(name, arguments);
    if input.trim().is_empty() {
        return false;
    }
    if name == "apply_patch" {
        return apply_patch_input_is_complete(&input);
    }
    true
}

/// Empty-completion retry looks at one SSE/Chat event, not accumulated args.
/// A non-empty apply_patch fragment is a real attempt even before End Patch.
pub(crate) fn custom_tool_input_is_present(name: &str, arguments: &str) -> bool {
    !unwrap_custom_tool_arguments(name, arguments)
        .trim()
        .is_empty()
}

/// Whether a finished custom call may go to Codex, which runs what it receives.
/// Without the `tool_calls` finish, only a closed wrapper or a patch's end marker
/// shows that the input was not cut off.
pub(crate) fn custom_tool_arguments_are_final(
    name: &str,
    arguments: &str,
    finished_with_tool_calls: bool,
) -> bool {
    let trimmed = arguments.trim();
    // `{}`/`null` means the provider announced the name but never sent the input.
    if trimmed == "{}" || trimmed == "null" || !custom_tool_input_is_actionable(name, arguments) {
        return false;
    }
    finished_with_tool_calls
        || name == "apply_patch"
        || serde_json::from_str::<Value>(trimmed).is_ok_and(|value| value.is_object())
}

/// How a provider ended the turn, as far as its tool calls can be trusted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TurnEnd {
    /// The provider's own statement that the calls are complete.
    ToolCalls,
    /// An ordinary end that says nothing about any call.
    Normal,
    /// A limit or a filter, which can stop anywhere.
    CutOff,
    Failed,
}

/// The one rule for a finished custom call, in every translator. `length` and
/// filters can cut anywhere, which only a patch's end marker can rule out. A failed
/// turn delivers none, since Codex would run what it receives.
pub(crate) fn custom_call_is_deliverable(name: &str, arguments: &str, end: TurnEnd) -> bool {
    match end {
        TurnEnd::Failed => false,
        TurnEnd::CutOff if name != "apply_patch" => false,
        _ => custom_tool_arguments_are_final(name, arguments, end == TurnEnd::ToolCalls),
    }
}

/// The custom calls a non-stream translator left out, and what they do to the turn.
/// An empty patch only fails a turn that has nothing else to show for itself.
#[derive(Default)]
pub(crate) struct UndeliveredCustomCalls {
    invalid: bool,
    empty_patch: bool,
}

impl UndeliveredCustomCalls {
    pub(crate) fn note(&mut self, name: &str, arguments: &str) {
        if name == "apply_patch" && !custom_tool_input_is_present(name, arguments) {
            self.empty_patch = true;
        } else {
            self.invalid = true;
        }
    }

    pub(crate) fn invalidate_turn(&self, output: &[Value], text: &str) -> bool {
        self.invalid
            || (self.empty_patch
                && (text.trim().is_empty() || is_placeholder_progress_text(text))
                && !output.iter().any(|item| {
                    matches!(
                        item.get("type").and_then(Value::as_str),
                        Some("function_call" | "custom_tool_call" | "tool_search_call")
                    )
                }))
    }
}

/// What a provider sent as a call's `function.arguments`.
pub(crate) enum ChatToolArguments<'a> {
    Absent,
    /// The text to accumulate. An object is serialized, since some providers send
    /// the arguments as JSON rather than as a string that encodes it.
    Text(std::borrow::Cow<'a, str>),
    /// Any other JSON value, which cannot be a tool's arguments.
    Invalid,
}

pub(crate) fn chat_tool_call_arguments(call: &Value) -> ChatToolArguments<'_> {
    match call.pointer("/function/arguments") {
        None | Some(Value::Null) => ChatToolArguments::Absent,
        Some(Value::String(text)) => ChatToolArguments::Text(text.into()),
        Some(object @ Value::Object(_)) => ChatToolArguments::Text(object.to_string().into()),
        Some(_) => ChatToolArguments::Invalid,
    }
}

/// Whether `incoming` can only be the call that produced `prior` again: an echo
/// without arguments, or the same object with more keys. Anything else is another
/// call, because a provider may send several calls to one tool. `{}` is a whole call
/// to a tool without parameters, not the start of a longer one.
pub(crate) fn arguments_update_same_call(prior: &Value, incoming: &Value) -> bool {
    let (Some(prior), Some(incoming)) = (prior.as_object(), incoming.as_object()) else {
        return prior == incoming;
    };
    incoming.is_empty()
        || (!prior.is_empty()
            && prior
                .iter()
                .all(|(key, value)| incoming.get(key) == Some(value)))
}

/// A translated turn with neither an answer nor a call would end silently if it
/// reached Codex as `completed`. Translators stay as they are for callers that
/// inspect the raw result, such as the compaction summarizer and its repair step.
pub(crate) fn mark_empty_response_incomplete(response: &mut Value) {
    if response.get("status").and_then(Value::as_str) != Some("completed") {
        return;
    }
    let Some(output) = response.get("output").and_then(Value::as_array) else {
        return;
    };
    let has_result = output
        .iter()
        .any(|item| match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => false,
            Some("message") => item
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|parts| {
                    parts.iter().any(|part| {
                        part.get("text")
                            .and_then(Value::as_str)
                            .is_some_and(|text| {
                                !text.trim().is_empty() && !is_placeholder_progress_text(text)
                            })
                    })
                }),
            _ => true,
        });
    if !has_result {
        response["status"] = json!("incomplete");
        response["completed_at"] = Value::Null;
        response["incomplete_details"] = json!({"reason": "empty_response"});
    }
}

fn apply_patch_input_is_complete(input: &str) -> bool {
    let mut saw_begin = false;
    let mut last_nonempty = None;
    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "*** Begin Patch" {
            saw_begin = true;
        }
        last_nonempty = Some(trimmed);
    }
    saw_begin && last_nonempty == Some("*** End Patch")
}

pub(crate) fn is_placeholder_progress_text(text: &str) -> bool {
    matches!(
        text.trim(),
        "Still working…" | "Still working..." | "Still working."
    )
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
    let cached = usage.and_then(|value| {
        value
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .or_else(|| value.get("cache_read_input_tokens").and_then(Value::as_u64))
    });
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
        "input_tokens_details": cached.map(|n| json!({"cached_tokens": n})).unwrap_or_else(|| json!({})),
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

#[cfg(test)]
mod custom_tool_input_tests {
    use super::{
        custom_tool_input_is_actionable, custom_tool_input_is_present, qualify_response_tool_name,
        unwrap_custom_tool_arguments,
    };

    #[test]
    fn empty_and_whitespace_custom_input_is_not_actionable() {
        assert!(!custom_tool_input_is_actionable("exec", ""));
        assert!(!custom_tool_input_is_actionable("exec", "   \n"));
        assert!(!custom_tool_input_is_actionable("apply_patch", ""));
        assert!(!custom_tool_input_is_actionable(
            "apply_patch",
            r#"{"input":""}"#
        ));
    }

    #[test]
    fn apply_patch_requires_end_marker() {
        let truncated = "*** Begin Patch\n*** Add File: docs/a.html\n+ok\n";
        let complete = "*** Begin Patch\n*** Add File: docs/a.html\n+ok\n*** End Patch\n";
        let end_only = "*** End Patch\n";
        assert!(!custom_tool_input_is_actionable("apply_patch", truncated));
        assert!(!custom_tool_input_is_actionable("apply_patch", end_only));
        assert!(!custom_tool_input_is_actionable("apply_patch", "{}"));
        assert!(custom_tool_input_is_present("apply_patch", truncated));
        assert!(!custom_tool_input_is_present("apply_patch", ""));
        assert!(!custom_tool_input_is_present("apply_patch", "{}"));
        assert!(custom_tool_input_is_actionable("apply_patch", complete));
        let wrapped = serde_json::json!({"input": complete}).to_string();
        assert!(custom_tool_input_is_actionable("apply_patch", &wrapped));
        assert_eq!(
            unwrap_custom_tool_arguments("apply_patch", &wrapped),
            complete
        );
        let patch_key = serde_json::json!({"patch": complete}).to_string();
        assert!(custom_tool_input_is_actionable("apply_patch", &patch_key));
        assert!(!custom_tool_input_is_actionable("exec", &patch_key));
        assert_eq!(
            unwrap_custom_tool_arguments("apply_patch", &patch_key),
            complete
        );
        assert_eq!(unwrap_custom_tool_arguments("exec", &patch_key), patch_key);
    }

    #[test]
    fn non_patch_custom_tools_only_need_non_empty_input() {
        assert!(custom_tool_input_is_actionable("exec", "pwd"));
        assert!(custom_tool_input_is_actionable(
            "exec",
            r#"{"input":"pwd"}"#
        ));
    }

    #[test]
    fn long_names_are_bounded_with_stable_aliases() {
        let first = qualify_response_tool_name(
            "mcp__codex_apps__codex_document_control___",
            "get_document_tool_schemas",
        );
        let second = qualify_response_tool_name(
            "mcp__codex_apps__codex_document_control___",
            "get_document_tool_schemas",
        );
        assert_eq!(first, second);
        assert_eq!(first.len(), 64);
        assert!(first.starts_with("mcp__codex_apps__codex_document_control"));
    }
}
