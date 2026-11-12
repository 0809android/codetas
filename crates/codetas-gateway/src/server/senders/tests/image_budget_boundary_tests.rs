use super::*;

#[tokio::test]
async fn image_size_rejections_require_compaction_without_a_reduced_history_retry() {
    for status in [StatusCode::BAD_REQUEST, StatusCode::PAYLOAD_TOO_LARGE] {
        let upstream = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(reqwest::Body::from(
                r#"{"error":{"message":"too many images"}}"#,
            ))
            .unwrap();
        let mut upstream = reqwest::Response::from(upstream);
        upstream
            .extensions_mut()
            .insert(SentImageState { sent_images: 5 });
        // This handler accepts neither a client nor a mutable request body:
        // it cannot prune history or issue another upstream request.
        let failure = require_compaction_on_image_rejection(upstream, status)
            .await
            .expect_err("image budget rejection must require a compaction boundary");
        assert_eq!(failure.response.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(failure.response.into_body(), 16 * 1024)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "context_length_exceeded");
        assert_eq!(value["error"]["type"], "invalid_request_error");
        assert_eq!(value["error"]["param"], "input");
    }
}

#[tokio::test]
async fn unrelated_provider_errors_are_not_relabelled_as_image_limits() {
    for message in [
        "invalid tool schema",
        "invalid image format",
        "unsupported image dimensions",
    ] {
        let text = json!({"error": {"message": message}}).to_string();
        let upstream = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::CONTENT_TYPE, "application/json")
            .body(reqwest::Body::from(text.clone()))
            .unwrap();
        let mut upstream = reqwest::Response::from(upstream);
        upstream
            .extensions_mut()
            .insert(SentImageState { sent_images: 5 });
        let response = require_compaction_on_image_rejection(upstream, StatusCode::BAD_REQUEST)
            .await
            .expect("unrelated error remains unchanged");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            read_bounded(response, 16 * 1024).await.unwrap(),
            text.as_bytes()
        );
    }
}

#[tokio::test]
async fn local_image_budget_uses_the_same_context_window_error_contract() {
    let failure = image_history_failure(crate::translate::ImageHistoryError::BudgetExceeded {
        required: 8192,
        limit: 4096,
    });
    assert_eq!(failure.response.status(), StatusCode::BAD_REQUEST);
    let bytes = to_bytes(failure.response.into_body(), 16 * 1024)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["error"]["code"], "context_length_exceeded");
    assert_eq!(value["error"]["param"], "input");
}
