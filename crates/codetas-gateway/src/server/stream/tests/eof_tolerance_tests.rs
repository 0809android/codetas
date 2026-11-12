use super::*;

#[test]
fn anthropic_eof_tolerance_emits_a_complete_undelimited_final_frame() {
    let mut pending = br#"data: {"type":"message_stop"}"#.to_vec();
    let delimiter =
        tolerated_eof_delimiter(&pending, true, false).expect("synthetic EOF delimiter");

    let values = drain_sse_values(&mut pending, delimiter).expect("complete final frame");

    assert_eq!(values, vec![json!({"type": "message_stop"})]);
    assert!(pending.is_empty());
}

#[test]
fn anthropic_eof_tolerance_rejects_truncated_final_json() {
    let mut pending = br#"data: {"type":"content_block_delta""#.to_vec();
    let delimiter =
        tolerated_eof_delimiter(&pending, true, false).expect("synthetic EOF delimiter");

    assert!(drain_sse_values(&mut pending, delimiter).is_err());
}

#[test]
fn ordinary_streams_do_not_synthesize_an_eof_delimiter() {
    let pending = br#"data: {"type":"message_stop"}"#.to_vec();
    assert!(tolerated_eof_delimiter(&pending, false, false).is_none());
    assert!(tolerated_eof_delimiter(&pending, true, true).is_none());
}
