use super::*;

#[test]
fn nonstream_gemini_read_is_removed_but_write_is_preserved() {
    let mut read = json!({
        "status": "completed",
        "output": [{
            "type": "function_call",
            "name": "exec",
            "arguments": "{\"cmd\":\"cat README.md\"}"
        }]
    });
    assert!(block_repeated_readonly_tools(&mut read));
    assert_eq!(read["status"], "incomplete");
    assert!(read["output"].as_array().is_some_and(Vec::is_empty));

    let mut write = json!({
        "status": "completed",
        "output": [{
            "type": "function_call",
            "name": "exec",
            "arguments": "{\"cmd\":\"apply_patch < patch.diff\"}"
        }]
    });
    assert!(!block_repeated_readonly_tools(&mut write));
    assert_eq!(write["status"], "completed");
    assert_eq!(write["output"].as_array().map(Vec::len), Some(1));
}
