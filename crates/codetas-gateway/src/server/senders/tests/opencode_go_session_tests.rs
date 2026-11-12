use super::*;

#[test]
fn session_is_scoped_to_go_and_stable_per_thread() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "session_id",
        "01a13ddb-8d21-7cb2-8f00-23a8a09af079".parse().unwrap(),
    );
    let base = "https://opencode.ai/zen/go/v1/";
    let session = opencode_go_session(base, Some(&headers)).unwrap();
    assert_eq!(
        Some(session.clone()),
        opencode_go_session(base, Some(&headers))
    );
    assert!(!session.contains("01a13ddb"));
    headers.insert(
        "session_id",
        "01a13ddb-8d21-7cb2-8f00-23a8a09af080".parse().unwrap(),
    );
    assert_ne!(Some(session), opencode_go_session(base, Some(&headers)));
    assert!(opencode_go_session("https://opencode.ai/zen/v1", Some(&headers)).is_none());
    assert_ne!(
        opencode_go_session(base, None),
        opencode_go_session(base, None)
    );
}
