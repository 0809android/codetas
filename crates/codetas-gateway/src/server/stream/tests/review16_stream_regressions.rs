use super::super::adapt::review16_regressions::{candidate, response, seed};
use super::*;
#[tokio::test]
async fn review16_translated_limits_recover_split_usage_prefix_without_completed() {
    for protocol in [
        ProviderProtocol::ChatCompletions,
        ProviderProtocol::AnthropicMessages,
        ProviderProtocol::GeminiGenerateContent,
    ] {
        let mut c = candidate();
        c.provider.protocol = protocol;
        let prefix = match protocol {
            ProviderProtocol::AnthropicMessages => {
                sse(
                    "message_start",
                    &json!({"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":5}}}),
                ) + &sse(
                    "message_delta",
                    &json!({"type":"message_delta","usage":{"cache_creation_input_tokens":2,"output_tokens":3},"delta":{"stop_reason":"end_turn"}}),
                )
            }
            ProviderProtocol::GeminiGenerateContent => sse(
                "message",
                &json!({"usageMetadata":{"promptTokenCount":17,"candidatesTokenCount":3},"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"answer"}]}}]}),
            ),
            _ => sse(
                "message",
                &json!({"choices":[{"delta":{"content":"answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":17,"completion_tokens":3}}),
            ),
        };
        c.provider.limits.max_response_bytes = prefix.len() as u64;
        let split = prefix.len() / 2;
        let chunks = vec![
            Ok(Bytes::copy_from_slice(&prefix.as_bytes()[..split])),
            Ok(Bytes::from(format!("{}{}", &prefix[split..], "excess"))),
        ];
        let ledger = ObservabilityLedger::new(None);
        let store = Arc::new(ResponseStateStore::new(None));
        let client = adapt_successful_response(
            response(chunks, StatusCode::OK),
            &c,
            seed(&ledger, &c, true),
            ResponseToolMap::default(),
            &store,
            &json!({"store":false,"stream":true,"input":"test"}),
            true,
            false,
        )
        .await;
        let bytes = to_bytes(client.into_body(), 1024 * 1024).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("response.failed"));
        assert!(!text.contains("response.completed"));
        assert_eq!(ledger.summary().input_tokens, 17, "{protocol:?}");
        assert_eq!(ledger.summary().output_tokens, 3);
        assert_eq!(ledger.summary().successful_requests, 0);
        assert_eq!(ledger.summary().failed_requests, 1);
        assert!(ledger.cache_health_summary().sessions.is_empty());
    }
}
#[tokio::test]
async fn review16_passthrough_and_repair_limits_account_but_never_send_terminal() {
    for repair_ids in [false, true] {
        for snapshot_repair in [false, true] {
            let c = candidate();
            let ledger = ObservabilityLedger::new(None);
            let store = Arc::new(ResponseStateStore::new(None));
            let prefix = sse(
                "response.completed",
                &json!({"type":"response.completed","response":{"id":"limit","status":"completed","output":[],"usage":{"input_tokens":17,"output_tokens":3}}}),
            );
            let upstream = response(
                vec![Ok(Bytes::from(format!("{prefix}excess")))],
                StatusCode::OK,
            );
            let request = json!({"store":false,"input":"test"});
            let client = if repair_ids {
                repairing_responses_stream(
                    upstream,
                    prefix.len() as u64,
                    Duration::from_secs(1),
                    seed(&ledger, &c, true),
                    ResponsesItemIdRepair::new_with_policy(&Default::default(), true).unwrap(),
                    store.clone(),
                    request,
                    true,
                    snapshot_repair,
                )
            } else {
                passthrough_stream(
                    upstream,
                    prefix.len() as u64,
                    Duration::from_secs(1),
                    seed(&ledger, &c, true),
                    store.clone(),
                    request,
                    true,
                    snapshot_repair,
                )
            };
            let mut source = client.into_body().into_data_stream();
            let mut text = Vec::new();
            let mut failed = false;
            while let Some(chunk) = source.next().await {
                match chunk {
                    Ok(bytes) => text.extend_from_slice(&bytes),
                    Err(_) => {
                        failed = true;
                        break;
                    }
                }
            }
            drop(source);
            assert!(failed);
            assert!(!String::from_utf8_lossy(&text).contains("response.completed"));
            assert_eq!(ledger.summary().input_tokens, 17);
            assert_eq!(ledger.summary().successful_requests, 0);
            assert!(ledger.cache_health_summary().sessions.is_empty());
            let mut next = json!({"previous_response_id":"limit","input":"next"});
            assert!(!store
                .expand_previous_response_input_with_hint(&mut next, None)
                .expanded());
        }
    }
}
#[tokio::test]
async fn review16_json_read_limit_io_and_invalid_suffix_keep_known_usage() {
    let prefix = json!({"usage":{"input_tokens":17,"output_tokens":3}}).to_string();
    for mode in ["limit", "io", "json"] {
        let chunks = if mode == "io" {
            vec![
                Ok(Bytes::from(prefix.clone())),
                Err(std::io::Error::other("fixture")),
            ]
        } else {
            vec![Ok(Bytes::from(format!("{prefix}invalid")))]
        };
        let limit = if mode == "limit" {
            prefix.len() as u64
        } else {
            4096
        };
        let (_, usage) = bounded_json_with_usage(response(chunks, StatusCode::OK), limit)
            .await
            .unwrap_err();
        assert_eq!(usage.input_tokens, 17);
        assert_eq!(usage.output_tokens, 3);
        assert!(!usage.cache_read_reported);
    }
}
