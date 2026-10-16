//! Bounded, allocation-independent classification before JSON working-set admission.
//! This is not a general JSON validator. It identifies the exact field path used
//! by request_is_remote_compaction for valid JSON; the normal parser still validates.
pub(crate) fn body_has_compaction_trigger(bytes: impl Iterator<Item = u8>, complete: bool) -> bool {
    #[derive(Default)]
    struct Frame {
        object: bool,
        root: bool,
        input_array: bool,
        input_item: bool,
        phase: u8,
        key: u8,
        trigger: bool,
    }
    let mut stack = Vec::<Frame>::new();
    let mut result = false;
    let mut bytes = bytes;
    while let Some(byte) = bytes.next() {
        match byte {
            b'"' => {
                let mut token = Vec::with_capacity(128);
                token.push(byte);
                let mut capture = true;
                let mut escaped = false;
                let mut ended = false;
                for byte in bytes.by_ref() {
                    if capture {
                        if token.len() < 256 {
                            token.push(byte);
                        } else {
                            capture = false;
                        }
                    }
                    if escaped {
                        escaped = false;
                        continue;
                    }
                    if byte == b'\\' {
                        escaped = true;
                        continue;
                    }
                    if byte == b'"' {
                        ended = true;
                        break;
                    }
                }
                if !ended {
                    return false;
                }
                let token = capture
                    .then(|| serde_json::from_slice::<String>(&token).ok())
                    .flatten();
                if let Some(frame) = stack.last_mut() {
                    if frame.object && frame.phase == 0 {
                        frame.key = match token.as_deref() {
                            Some("input") => 1,
                            Some("type") => 2,
                            _ => 0,
                        };
                        frame.phase = 1;
                    } else if frame.object && frame.phase == 2 {
                        if frame.root && frame.key == 1 {
                            result = false;
                        }
                        if frame.input_item && frame.key == 2 {
                            frame.trigger = token.as_deref() == Some("compaction_trigger");
                        }
                        frame.phase = 3;
                    }
                }
            }
            b'{' | b'[' => {
                if stack.len() >= 128 {
                    return false;
                }
                let input = stack
                    .last()
                    .is_some_and(|f| f.object && f.root && f.key == 1 && f.phase == 2);
                let item = byte == b'{' && stack.last().is_some_and(|f| f.input_array);
                let root = stack.is_empty() && byte == b'{';
                if let Some(frame) = stack.last_mut() {
                    if input {
                        result = false;
                    }
                    if frame.object && frame.phase == 2 {
                        if frame.input_item && frame.key == 2 {
                            frame.trigger = false;
                        }
                        frame.phase = 3;
                    }
                }
                stack.push(Frame {
                    object: byte == b'{',
                    root,
                    input_array: input && byte == b'[',
                    input_item: item,
                    ..Default::default()
                });
            }
            b'}' | b']' => {
                let Some(frame) = stack.pop() else {
                    return false;
                };
                if frame.object != (byte == b'}') {
                    return false;
                }
                if frame.input_array {
                    result = frame.trigger;
                }
                if frame.input_item && frame.trigger {
                    if let Some(parent) = stack.last_mut() {
                        if parent.input_array {
                            parent.trigger = true;
                        }
                    }
                }
            }
            b':' => {
                if let Some(f) = stack.last_mut() {
                    if f.object && f.phase == 1 {
                        f.phase = 2;
                    }
                }
            }
            b',' => {
                if let Some(f) = stack.last_mut() {
                    if f.object {
                        f.phase = 0;
                        f.key = 0;
                    }
                }
            }
            b' ' | b'\t' | b'\r' | b'\n' => {}
            _ => {
                if let Some(f) = stack.last_mut() {
                    if f.object && f.phase == 2 {
                        if f.root && f.key == 1 {
                            result = false;
                        }
                        if f.input_item && f.key == 2 {
                            f.trigger = false;
                        }
                        f.phase = 3;
                    }
                }
            }
        }
    }
    result && (!complete || stack.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classifier_matches_only_root_input_item_type_and_honors_last_duplicate() {
        for (body, expected) in [
            (r#"{"input":[{"type":"compaction_trigger"}]}"#, true),
            (
                r#"{"input":[{"ty\u0070e":"compaction\u005ftrigger"}]}"#,
                true,
            ),
            (
                r#"{"input":[{"type":"message","content":"\"type\":\"compaction_trigger\""}]}"#,
                false,
            ),
            (
                r#"{"metadata":{"input":[{"type":"compaction_trigger"}]},"input":[]}"#,
                false,
            ),
            (
                r#"{"input":[{"type":"compaction_trigger","type":"message"}]}"#,
                false,
            ),
            (
                r#"{"input":[{"type":"compaction_trigger"}],"input":null}"#,
                false,
            ),
            (
                r#"{"input":[{"type":"compaction_trigger"}],"input":[]}"#,
                false,
            ),
            (
                r#"{"input":[{"type":"compaction_trigger"}],"input":[{"type":"message"}]}"#,
                false,
            ),
        ] {
            assert_eq!(
                body_has_compaction_trigger(body.bytes(), true),
                expected,
                "{body}"
            );
        }
        let body = format!(
            "{{\"input\":[{{\"type\":\"message\",\"content\":\"{}\"}}]}}",
            "x".repeat(3 * 1024 * 1024)
        );
        assert!(!body_has_compaction_trigger(body.bytes(), true));
    }
}
