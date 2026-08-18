use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use serde_json::{json, Value};

const PREFIX: &str = "codetas1:";
const LEGACY_PREFIX: &str = "ocx1:";
const MAX_SUMMARY_BYTES: usize = 2 * 1024 * 1024;
const MIN_USABLE_SUMMARY_CHARS: usize = 80;

/// Mirrors OpenCodex / Codex local compact prompt: a handoff, not a new plan.
pub(crate) const COMPACT_PROMPT: &str = "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.\n\nInclude:\n- Current progress and key decisions made\n- Important context, constraints, or user preferences\n- What remains to be done (clear next steps)\n- Any critical data, examples, or references needed to continue\n- File paths already inspected and findings that must not be rediscovered\n\nBe concise, structured, and focused on helping the next LLM seamlessly continue the work. Do not emit tool calls, markup fences, or tokenizer sentinels such as <|eos|> or <file_end>.";

/// Mirrors OpenCodex SUMMARY_PREFIX / Codex compact.rs framing.
pub(crate) const SUMMARY_PREFIX: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";

pub(crate) const OPAQUE_COMPACTION_NOTE: &str =
    "[earlier conversation was compacted; the summary is stored in a format this model cannot read]";

pub(crate) fn model_summary_is_usable(summary: &str) -> Result<(), String> {
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        return Err("compaction summary is empty".into());
    }
    if trimmed.contains("<|eos|>")
        || trimmed.contains("<file_end>")
        || trimmed.contains("<tool_call>")
        || trimmed.contains("</tool_call>")
    {
        return Err("compaction summary contains leaked control tokens".into());
    }
    if trimmed.chars().count() < MIN_USABLE_SUMMARY_CHARS {
        return Err("compaction summary is too short to be a usable handoff".into());
    }
    Ok(())
}

pub(crate) fn encode_summary(summary: &str) -> Result<String, String> {
    if summary.trim().is_empty() {
        return Err("compaction summary is empty".into());
    }
    if summary.len() > MAX_SUMMARY_BYTES {
        return Err("compaction summary exceeds the CODETAS limit".into());
    }
    let payload = serde_json::to_vec(&json!({"version": 1, "summary": summary}))
        .map_err(|_| "compaction summary cannot be encoded".to_string())?;
    Ok(format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(payload)))
}

pub(crate) fn decode_summary(item: &Value) -> Result<Option<String>, String> {
    match item.get("type").and_then(Value::as_str) {
        Some("compaction" | "compaction_summary" | "context_compaction") => {}
        _ => return Ok(None),
    }
    let encrypted = item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .ok_or("compaction item requires encrypted_content")?;
    if let Some(encoded) = encrypted.strip_prefix(PREFIX) {
        let payload = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| "CODETAS compaction envelope is not valid base64")?;
        if payload.len() > MAX_SUMMARY_BYTES {
            return Err("CODETAS compaction envelope exceeds the size limit".into());
        }
        let value: Value = serde_json::from_slice(&payload)
            .map_err(|_| "CODETAS compaction envelope is invalid")?;
        if value.get("version").and_then(Value::as_u64) != Some(1) {
            return Err("CODETAS compaction envelope version is unsupported".into());
        }
        let summary = value
            .get("summary")
            .and_then(Value::as_str)
            .filter(|summary| !summary.trim().is_empty())
            .ok_or("CODETAS compaction envelope has no summary")?;
        return Ok(Some(format!(
            "{SUMMARY_PREFIX}\n\n<codetas_compaction_summary>\n{summary}\n</codetas_compaction_summary>"
        )));
    }
    if let Some(encoded) = encrypted.strip_prefix(LEGACY_PREFIX) {
        let payload = STANDARD
            .decode(encoded)
            .or_else(|_| URL_SAFE_NO_PAD.decode(encoded))
            .map_err(|_| "Compaction envelope is not valid base64")?;
        if payload.len() > MAX_SUMMARY_BYTES {
            return Err("Compaction envelope exceeds the size limit".into());
        }
        let summary = String::from_utf8(payload)
            .map_err(|_| "Compaction envelope is not valid UTF-8")?;
        if summary.trim().is_empty() {
            return Err("Compaction envelope has no summary".into());
        }
        return Ok(Some(format!(
            "{SUMMARY_PREFIX}\n\n<codetas_compaction_summary>\n{summary}\n</codetas_compaction_summary>"
        )));
    }
    Err("translated providers can consume only CODETAS compaction envelopes".into())
}

pub(crate) fn request_is_remote_compaction(body: &Value) -> bool {
    body.get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction_trigger"))
        })
}

pub(crate) fn compaction_item_count(response: &Value) -> usize {
    response
        .get("output")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    matches!(
                        item.get("type").and_then(Value::as_str),
                        Some("compaction" | "compaction_summary" | "context_compaction")
                    )
                })
                .count()
        })
        .unwrap_or(0)
}

pub(crate) fn response_output_text(response: &Value) -> String {
    let mut output = String::new();
    let Some(items) = response.get("output").and_then(Value::as_array) else {
        return output;
    };
    for item in items {
        let Some(parts) = item.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in parts {
            if matches!(
                part.get("type").and_then(Value::as_str),
                Some("output_text" | "text")
            ) {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    output.push_str(text);
                }
            }
        }
    }
    output
}

pub(crate) fn validate_local_compactions(body: &Value) -> Result<(), String> {
    let Some(items) = body.get("input").and_then(Value::as_array) else {
        return Ok(());
    };
    for item in items {
        let item_type = item.get("type").and_then(Value::as_str);
        if !matches!(item_type, Some("compaction" | "compaction_summary" | "context_compaction")) {
            continue;
        }
        let local = item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .is_some_and(|value| value.starts_with(PREFIX) || value.starts_with(LEGACY_PREFIX));
        if !local {
            continue;
        }
        decode_summary(item)
            .map(|_| ())
            .map_err(|error| format!("invalid local compaction envelope: {error}"))?;
    }
    Ok(())
}

pub(crate) fn expand_local_compactions(body: &mut Value) {
    expand_compaction_items(body, false);
}

/// Expand local envelopes and replace OpenAI-opaque compaction blobs with a
/// note. Used on translated / synthetic hops that cannot decrypt `gAAAAA`.
pub(crate) fn expand_translated_compactions(body: &mut Value) {
    expand_compaction_items(body, true);
}

fn expand_compaction_items(body: &mut Value, rewrite_opaque: bool) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    for item in items {
        let kind = item.get("type").and_then(Value::as_str);
        if !matches!(
            kind,
            Some("compaction" | "compaction_summary" | "context_compaction")
        ) {
            continue;
        }
        let encrypted = item.get("encrypted_content").and_then(Value::as_str);
        let local = encrypted
            .is_some_and(|value| value.starts_with(PREFIX) || value.starts_with(LEGACY_PREFIX));
        if !local {
            if rewrite_opaque {
                *item = json!({
                    "type": "message",
                    "role": "developer",
                    "content": [{"type": "input_text", "text": OPAQUE_COMPACTION_NOTE}]
                });
            }
            continue;
        }
        match decode_summary(item) {
            Ok(Some(summary)) => {
                *item = json!({
                    "type": "message",
                    "role": "developer",
                    "content": [{"type": "input_text", "text": summary}]
                });
            }
            Ok(None) => {}
            Err(error) => {
                crate::debug::log(&format!(
                    "invalid local compaction envelope replaced with a marker: {error}"
                ));
                *item = json!({
                    "type": "message",
                    "role": "developer",
                    "content": [{
                        "type": "input_text",
                        "text": "[invalid local compaction summary omitted]"
                    }]
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_local_compaction_envelopes_before_translation() {
        let valid = encode_summary("keep this summary").expect("valid envelope");
        assert!(validate_local_compactions(&json!({
            "input": [{"type": "compaction", "encrypted_content": valid}]
        }))
        .is_ok());
        assert!(validate_local_compactions(&json!({
            "input": [{"type": "compaction", "encrypted_content": "codetas1:not-base64"}]
        }))
        .is_err());
        assert!(validate_local_compactions(&json!({
            "input": [{"type": "reasoning", "encrypted_content": "codetas1:opaque"}]
        }))
        .is_ok());
    }

    #[test]
    fn malformed_local_compaction_is_not_silently_dropped() {
        let mut body = json!({
            "input": [{"type": "compaction", "encrypted_content": "codetas1:not-base64"}]
        });
        expand_local_compactions(&mut body);
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(
            body["input"][0]["content"][0]["text"],
            "[invalid local compaction summary omitted]"
        );
    }

    #[test]
    fn expand_local_compactions_leaves_native_trigger_and_non_local_items() {
        let encrypted = encode_summary("keep this summary").expect("valid envelope");
        let mut body = json!({
            "input": [
                {"type": "compaction", "encrypted_content": encrypted},
                {"type": "compaction", "encrypted_content": "gAAAAABopaque"},
                {"type": "message", "role": "user", "content": "continue"},
                {"type": "compaction_trigger", "id": "trigger_1"}
            ]
        });
        expand_translated_compactions(&mut body);
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "developer");
        assert!(
            body["input"][0]["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("keep this summary")
                    && text.contains("avoid duplicating work"))
        );
        assert_eq!(body["input"][1]["type"], "message");
        assert_eq!(
            body["input"][1]["content"][0]["text"],
            OPAQUE_COMPACTION_NOTE
        );
        assert_eq!(body["input"][2]["role"], "user");
        assert_eq!(body["input"][3]["type"], "compaction_trigger");
        assert!(!body["input"][0].to_string().contains("codetas1:"));
    }

    #[test]
    fn rejects_leaked_control_tokens_and_tiny_handoffs() {
        assert!(model_summary_is_usable("隠れ敵の撃破描画と被ダメ位置のずれ原因を、関連関数から特定します。\n<file_end><|eos|>").is_err());
        assert!(model_summary_is_usable("<tool_call>sed -n '1,20p' file.js</tool_call>").is_err());
        assert!(model_summary_is_usable("too short").is_err());
        assert!(model_summary_is_usable(
            "User wants attack cues removed and chapter-two hidden enemies to despawn. \
             renderer.js#drawEnemyTelegraph still emits 攻撃がくる. Next: delete that string \
             and skip defeated silt/mud burrowers in the draw loop."
        )
        .is_ok());
    }

    #[test]
    fn detects_remote_compaction_trigger() {
        assert!(request_is_remote_compaction(&json!({
            "input": [
                {"type": "message", "role": "user", "content": []},
                {"type": "compaction_trigger"}
            ]
        })));
        assert!(!request_is_remote_compaction(&json!({
            "input": [{"type": "message", "role": "user", "content": []}]
        })));
    }

    #[test]
    fn counts_only_compaction_output_items() {
        assert_eq!(
            compaction_item_count(&json!({
                "output": [
                    {"type": "reasoning"},
                    {"type": "message"}
                ]
            })),
            0
        );
        assert_eq!(
            compaction_item_count(&json!({
                "output": [{"type": "compaction", "encrypted_content": "codetas1:x"}]
            })),
            1
        );
    }
}
