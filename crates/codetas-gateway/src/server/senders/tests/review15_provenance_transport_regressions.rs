use super::super::anthropic_compact::ThinkingProvenance;
use super::*;

fn proof() -> ThinkingProvenance {
    ThinkingProvenance::for_wire(
        &json!({"messages":[],"system":"fixture"}),
        Some(&"a".repeat(64)),
        false,
    )
}

#[test]
fn review15_generation_provenance_never_enters_other_provider_wire() {
    let mut reasoning = json!({"type":"reasoning","summary":[{"type":"summary_text","text":"summary"}],
            "provider_metadata":{"anthropic":{"thinking_blocks":[{"type":"thinking","thinking":"thought","signature":"fixture-signature"}]}}});
    proof().annotate(&mut reasoning);
    let source = json!({"model":"fixture","input":[reasoning,{"type":"message","role":"user","content":"continue"}]});
    for preserve_metadata in [false, true] {
        let chat = responses_to_chat_with_options(
            &chat_compatible_request(&source, preserve_metadata),
            "fixture",
            false,
        )
        .unwrap();
        assert!(!chat.to_string().contains("generation_provenance"));
    }
    let gemini = responses_to_gemini(&source, "fixture").unwrap();
    assert!(!gemini.to_string().contains("generation_provenance"));
    let (kiro, _) = crate::kiro::responses_to_kiro_checked(&source, "fixture", None).unwrap();
    assert!(!kiro.to_string().contains("generation_provenance"));
    let mut responses = source.clone();
    crate::compat::strip_responses_provider_metadata(&mut responses);
    assert!(!responses.to_string().contains("generation_provenance"));
    assert!(
        source.to_string().contains("generation_provenance"),
        "wire stripping must not mutate replay source"
    );
}

#[tokio::test]
async fn review15_buffered_response_rebuild_preserves_thinking_provenance() {
    let proof = proof();
    let mut response = reqwest::Response::from(
        Response::builder()
            .status(200)
            .body(reqwest::Body::from(
                json!({"type":"message","content":[{"type":"text","text":"nonempty"}]}).to_string(),
            ))
            .unwrap(),
    );
    response.extensions_mut().insert(proof.clone());
    let buffered = buffer_provider_response(response, 1024).await.unwrap();
    let rebuilt = buffered.into_response().unwrap();
    assert_eq!(
        rebuilt.extensions().get::<ThinkingProvenance>(),
        Some(&proof)
    );
}
