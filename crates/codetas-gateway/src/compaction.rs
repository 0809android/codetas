use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use serde_json::{json, Value};

const PREFIX: &str = "codetas1:";
const LEGACY_PREFIX: &str = "ocx1:";
const MAX_SUMMARY_BYTES: usize = 2 * 1024 * 1024;

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
            "<codetas_compaction_summary>\n{}\n</codetas_compaction_summary>",
            summary
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
            "<codetas_compaction_summary>\n{}\n</codetas_compaction_summary>",
            summary
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
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    for item in items {
        let local = item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .is_some_and(|value| value.starts_with(PREFIX) || value.starts_with(LEGACY_PREFIX));
        if !local {
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
