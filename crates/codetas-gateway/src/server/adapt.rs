use super::*;

pub(crate) async fn adapt_successful_response(
    upstream: reqwest::Response,
    candidate: &RouteCandidate,
    observation: ObservationSeed,
    tool_map: ResponseToolMap,
    response_state: &Arc<ResponseStateStore>,
    request_body: &Value,
    record_eligible: bool,
    codex_client: bool,
) -> Response<Body> {
    if candidate.provider.transport == ProviderTransport::Kiro {
        return kiro_response(upstream, candidate, observation, Arc::clone(response_state), request_body.clone(), record_eligible).await;
    }
    // Some Responses-compatible upstreams omit or rewrite the SSE content type even
    // after accepting `stream: true`. Trust the already-validated request intent as
    // well as the response header so an SSE body is not parsed as ordinary JSON.
    let streaming = candidate.capabilities.streaming
        && (observation.streaming
            || upstream
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.contains("text/event-stream")));
    let limit = candidate.provider.limits.max_response_bytes;
    let idle_timeout = Duration::from_millis(candidate.provider.limits.stream_idle_timeout_ms);
    let protocol = candidate
        .provider
        .protocol_for_model(&candidate.upstream_model);
    // Translated protocols cannot forward Responses `previous_response_id`, so their
    // generated Responses history must also be cached locally. Codex sends `store:false`
    // while still chaining turns, which requires forced recording on these paths.
    // A missed local expand is not itself a reason to checkpoint a truncated
    // delta: stateful Responses keep `previous_response_id` and recover upstream.
    let force_record = protocol != ProviderProtocol::Responses
        || candidate.provider.credential.source == CredentialSource::Forward
        || candidate.provider.stateless_responses;
    let should_record = record_eligible && force_record;
    let progress_policy = if codex_client {
        ToolProgressPolicy::from_request(request_body)
    } else {
        ToolProgressPolicy::default()
    };
    let state_for_stream = Arc::clone(response_state);
    let body_for_stream = request_body.clone();
    match protocol {
        ProviderProtocol::Responses if streaming => {
            if let Some(repair) = ResponsesItemIdRepair::new_with_policy(
                &candidate.provider.response_item_id_repair,
                candidate.provider.repair_invalid_response_item_ids,
            ) {
                repairing_responses_stream(
                    upstream,
                    limit,
                    idle_timeout,
                    observation,
                    repair,
                    state_for_stream,
                    body_for_stream,
                    should_record,
                    candidate.provider.responses_snapshot_repair,
                )
            } else {
                passthrough_stream(
                    upstream,
                    limit,
                    idle_timeout,
                    observation,
                    state_for_stream,
                    body_for_stream,
                    should_record,
                    candidate.provider.responses_snapshot_repair,
                )
            }
        }
        ProviderProtocol::Responses => {
            responses_json_response(
                upstream,
                limit,
                observation,
                ResponsesItemIdRepair::new_with_policy(
                    &candidate.provider.response_item_id_repair,
                    candidate.provider.repair_invalid_response_item_ids,
                ),
                Arc::clone(response_state),
                request_body.clone(),
                should_record,
                candidate.provider.responses_snapshot_repair,
            )
            .await
        }
        ProviderProtocol::ChatCompletions if streaming => translated_stream_response(
            upstream,
            candidate.exposed_model.clone(),
            StreamAdapter::Chat,
            limit,
            idle_timeout,
            observation,
            false,
            tool_map,
            progress_policy,
            state_for_stream,
            body_for_stream,
            should_record,
            false,
            repeated_readonly_inspect_guard_active(request_body),
        ),
        ProviderProtocol::ChatCompletions => {
            chat_json_response(
                upstream,
                &candidate.exposed_model,
                limit,
                observation,
                tool_map,
                Arc::clone(response_state),
                request_body.clone(),
                should_record,
                repeated_readonly_inspect_guard_active(request_body),
            )
            .await
        }
        ProviderProtocol::AnthropicMessages if streaming => translated_stream_response(
            upstream,
            candidate.exposed_model.clone(),
            StreamAdapter::Anthropic {
                state: AnthropicStreamState::default(),
                subscription_oauth: uses_anthropic_subscription_oauth(&candidate.provider),
            },
            limit,
            idle_timeout,
            observation,
            candidate.provider.escape_builtin_tool_names,
            tool_map,
            progress_policy,
            state_for_stream,
            body_for_stream,
            should_record,
            model_matches_any(
                &candidate.upstream_model,
                &candidate.provider.anthropic_eof_tolerance_models,
            ),
            repeated_readonly_inspect_guard_active(request_body),
        ),
        ProviderProtocol::AnthropicMessages => {
            let subscription_oauth = uses_anthropic_subscription_oauth(&candidate.provider);
            let block_repeated_readonly_inspect =
                repeated_readonly_inspect_guard_active(request_body);
            adapted_json_response(
                upstream,
                &candidate.exposed_model,
                move |value, exposed_model, tool_map| {
                    let mut response = anthropic_to_response_with_oauth(
                        value,
                        exposed_model,
                        tool_map,
                        subscription_oauth,
                    )?;
                    if block_repeated_readonly_inspect {
                        block_repeated_readonly_tools(&mut response);
                    }
                    Ok(response)
                },
                limit,
                observation,
                candidate.provider.escape_builtin_tool_names,
                tool_map,
                Arc::clone(response_state),
                request_body.clone(),
                should_record,
            )
            .await
        }
        // Gemini's GenerateContent stream already carries explicit candidate/tool
        // lifecycle events. Do not add the generic Responses progress placeholder:
        // `Still working…` becomes assistant history and is replayed on the next
        // tool turn, which can make a translated provider loop. Native Responses
        // is left untouched because it has its own lifecycle and replay rules.
        ProviderProtocol::GeminiGenerateContent if streaming => translated_stream_response(
            upstream,
            candidate.exposed_model.clone(),
            StreamAdapter::Gemini,
            limit,
            idle_timeout,
            observation,
            false,
            tool_map,
            progress_policy,
            state_for_stream,
            body_for_stream,
            should_record,
            false,
            repeated_readonly_inspect_guard_active(request_body),
        ),
        ProviderProtocol::GeminiGenerateContent => {
            let block_repeated_readonly_inspect =
                repeated_readonly_inspect_guard_active(request_body);
            adapted_json_response(
                upstream,
                &candidate.exposed_model,
                move |value, exposed_model, tool_map| {
                    let mut response = gemini_to_response(value, exposed_model, tool_map)?;
                    if block_repeated_readonly_inspect {
                        block_repeated_readonly_tools(&mut response);
                    }
                    Ok(response)
                },
                limit,
                observation,
                false,
                tool_map,
                Arc::clone(response_state),
                request_body.clone(),
                should_record,
            )
            .await
        }
    }
}

pub(crate) async fn candidate_response_value(
    upstream: reqwest::Response,
    candidate: &RouteCandidate,
    limit: u64,
) -> Result<Value, (String, TokenUsage)> {
    if candidate.provider.transport != ProviderTransport::Kiro {
        return super::stream::bounded_json_with_usage(upstream, limit).await;
    }
    let context = upstream.extensions().get::<KiroRequestContext>().cloned();
    let bytes = read_kiro_bounded(upstream, limit).await?;
    let context = context.ok_or_else(|| ("Kiro response lost its request context".to_string(), crate::kiro::buffered_usage(&bytes)))?;
    kiro_buffered_value(&bytes, &context, &candidate.exposed_model)
}

pub(crate) async fn kiro_response(
    upstream: reqwest::Response,
    candidate: &RouteCandidate,
    observation: ObservationSeed,
    response_state: Arc<ResponseStateStore>,
    request_body: Value,
    force_record: bool,
) -> Response<Body> {
    let status = upstream.status();
    let context = upstream.extensions().get::<KiroRequestContext>().cloned();
    if let Some(context) = context.as_ref().filter(|context| context.streaming && context.completion_tool.is_none()) {
        return kiro_incremental_response(upstream, candidate, observation, context.clone(), response_state, request_body, force_record);
    }
    let bytes = match read_kiro_bounded(upstream, candidate.provider.limits.max_response_bytes).await {
        Ok(bytes) => bytes,
        Err((message, usage)) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("invalid_provider_response"),
                usage,
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            );
        }
    };
    let Some(context) = context else {
        observation.finish(StatusCode::BAD_GATEWAY, Some("invalid_provider_response"), crate::kiro::buffered_usage(&bytes));
        return error_response(StatusCode::BAD_GATEWAY, "invalid_provider_response", "Kiro response lost its request context");
    };
    let value = match kiro_buffered_value(&bytes, &context, &candidate.exposed_model) {
        Ok(value) => value,
        Err((message, usage)) => {
            observation.finish(
                StatusCode::BAD_GATEWAY,
                Some("response_translation_error"),
                usage,
            );
            return error_response(
                StatusCode::BAD_GATEWAY,
                "invalid_provider_response",
                &message,
            );
        }
    };
    response_state.remember(&request_body, &value, force_record);
    observation.finish(
        status,
        response_terminal_failure_category(&value),
        TokenUsage::from_json(&value),
    );
    if !context.streaming {
        return json_response(status, value);
    }
    let mut encoded = String::new();
    for event in websocket_json_response_events(&value) {
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message");
        encoded.push_str(&sse(event_type, &event));
    }
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from(encoded))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "gateway_error",
                "failed to build Kiro stream response",
            )
        })
}

pub(crate) fn kiro_incremental_response(
    mut upstream: reqwest::Response,
    candidate: &RouteCandidate,
    observation: ObservationSeed,
    context: KiroRequestContext,
    response_state: Arc<ResponseStateStore>,
    request_body: Value,
    force_record: bool,
) -> Response<Body> {
    let status = upstream.status();
    let limit = candidate.provider.limits.max_response_bytes;
    let idle_timeout = Duration::from_millis(candidate.provider.limits.stream_idle_timeout_ms);
    let routing_attempt_lease = take_routing_attempt_lease(&mut upstream);
    let mut source = upstream.bytes_stream();
    let mut decoder = KiroStreamDecoder::new(context, candidate.exposed_model.clone());
    let output = stream! {
        let _routing_attempt_lease = routing_attempt_lease;
        let mut completion = StreamObservation::new(observation);
        let mut usage = TokenUsage::default();
        let mut received = 0_u64;
        let mut collector = crate::kiro::BoundedUsageCollector::default();
        let mut failure = None;
        let created = decoder.created_event();
        let created_type = created.get("type").and_then(Value::as_str).unwrap_or("response.created");
        yield Ok::<Bytes, std::io::Error>(Bytes::from(sse(created_type, &created)));
        loop {
            let chunk = match tokio::time::timeout(idle_timeout, source.next()).await {
                Ok(Some(Ok(chunk))) => chunk,
                Ok(Some(Err(_))) => {
                    failure = Some("upstream_stream_error");
                    usage=collector.usage();
                    completion.finish(StatusCode::BAD_GATEWAY,failure,usage.clone());
                    yield Err(std::io::Error::other("Kiro provider stream failed"));
                    break;
                }
                Ok(None) => break,
                Err(_) => {
                    failure = Some("stream_idle_timeout");
                    usage=collector.usage();
                    completion.finish(StatusCode::BAD_GATEWAY,failure,usage.clone());
                    yield Err(std::io::Error::other("Kiro provider stream exceeded the idle timeout"));
                    break;
                }
            };
            let take = chunk.len().min(usize::try_from(limit.saturating_sub(received)).unwrap_or(usize::MAX));
            collector.push(&chunk[..take]);
            received = received.saturating_add(chunk.len() as u64);
            if received > limit {
                failure = Some("provider_response_too_large");
                    usage=collector.usage();
                    completion.finish(StatusCode::BAD_GATEWAY,failure,usage.clone());
                yield Err(std::io::Error::other("Kiro provider stream exceeded the response limit"));
                break;
            }
            match decoder.push(&chunk) {
                Ok(events) => {
                    usage=collector.usage();completion.record_usage(&usage);
                    for event in events {
                        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("message");
                        yield Ok(Bytes::from(sse(event_type, &event)));
                    }
                }
                Err(_) => {
                    failure = Some("response_translation_error");
                    usage=collector.usage();
                    completion.finish(StatusCode::BAD_GATEWAY,failure,usage.clone());
                    yield Err(std::io::Error::other("Kiro provider stream was invalid"));
                    break;
                }
            }
        }
        if failure.is_none() {
            match decoder.finish() {
                Ok((events, response)) => {
                    response_state.remember(&request_body, &response, force_record);
                    usage = TokenUsage::from_json(&response);
                    completion.finish(status,response_terminal_failure_category(&response),usage.clone());
                    for event in events {
                        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("message");
                        yield Ok(Bytes::from(sse(event_type, &event)));
                    }
                }
                Err(_) => {
                    failure = Some("response_translation_error");
                    usage=collector.usage();
                    completion.finish(StatusCode::BAD_GATEWAY,failure,usage.clone());
                    yield Err(std::io::Error::other("Kiro provider stream ended before a valid response"));
                }
            }
        }
        completion.finish(
            if failure.is_some() { StatusCode::BAD_GATEWAY } else { status },
            failure,
            usage,
        );
    };
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(output))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "gateway_error",
                "failed to build incremental Kiro stream response",
            )
        })
}

#[cfg(test)]
mod kiro_json_lifecycle_tests {
    use super::*;

    #[test]
    fn bounded_kiro_json_uses_the_canonical_message_lifecycle() {
        let response = json!({
            "id": "resp_kiro", "status": "completed", "output": [{
                "id": "msg_kiro", "type": "message", "status": "completed",
                "role": "assistant", "content": [{"type": "output_text",
                    "text": "hello", "annotations": []}]
            }]
        });

        let types = websocket_json_response_events(&response)
            .into_iter()
            .filter_map(|event| {
                event
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect::<Vec<_>>();

        let expected = [
            "response.created",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        assert_eq!(types, expected);
    }
}

#[cfg(test)]
mod kiro_terminal_observation_tests {
    use super::*;
    fn frame(kind:&str,payload:Value)->Vec<u8> {
        let mut headers=Vec::new();for (name,value) in [(":message-type","event"),(":event-type",kind)] {headers.push(name.len() as u8);headers.extend_from_slice(name.as_bytes());headers.push(7);headers.extend_from_slice(&(value.len() as u16).to_be_bytes());headers.extend_from_slice(value.as_bytes());}
        let payload=payload.to_string();let total=16+headers.len()+payload.len();let mut result=Vec::new();result.extend_from_slice(&(total as u32).to_be_bytes());result.extend_from_slice(&(headers.len() as u32).to_be_bytes());result.extend_from_slice(&crc32fast::hash(&result).to_be_bytes());result.extend(headers);result.extend_from_slice(payload.as_bytes());result.extend_from_slice(&crc32fast::hash(&result).to_be_bytes());result
    }
    #[tokio::test]
    async fn cache_health_kiro_terminal_is_recorded_before_client_closes_body() {
        let mut provider=crate::config::ProviderDefinition::default();provider.id="kiro-test".into();provider.enabled=true;provider.models=vec!["model".into()];provider.credential.source=CredentialSource::None;let mut settings=GatewaySettings::default();settings.providers=vec![provider];let candidate=RoutingRuntime::default().candidates_for_request(&settings,"kiro-test/model",false,None).unwrap().remove(0);
        let ledger=ObservabilityLedger::new(None);
        for n in 0..20 {
            let mut bytes=frame("assistantResponseEvent",json!({"content":"ok"}));bytes.extend(frame("metadataEvent",json!({"tokenUsage":{"uncachedInputTokens":1000,"cacheReadInputTokens":99_000,"outputTokens":1},"stopReason":"end_turn"})));
            let upstream=Response::builder().status(200).body(reqwest::Body::from(bytes)).unwrap();
            let mut seed=ObservationSeed::for_candidate(ledger.clone(),ObservabilitySettings::default(),format!("kiro-terminal-{n}"),true,Instant::now(),1,&candidate);seed.request_diagnostic=Some(crate::observability::RequestDiagnostic{client_thread_id:Some("01a13ddb-8d21-7cb2-8f00-23a8a09af079".into()),..Default::default()});
            let response=kiro_incremental_response(reqwest::Response::from(upstream),&candidate,seed,KiroRequestContext{conversation_id:"fixture".into(),response_id:format!("kiro-{n}"),streaming:true,tool_names:Default::default(),completion_tool:None},Arc::new(ResponseStateStore::new(None)),json!({"input":"fixture"}),true);
            let mut body=response.into_body().into_data_stream();while let Some(chunk)=body.next().await {if String::from_utf8_lossy(&chunk.unwrap()).contains("response.completed") {break;}}drop(body);
        }
        let row=&ledger.cache_health_summary().sessions[0];assert_eq!(row.successful_requests,20);assert_eq!(row.measured_requests,20);assert_eq!(row.unknown_cache_requests,0);assert_eq!(row.input_tokens,2_000_000);
    }
    #[tokio::test]
    async fn kiro_reported_usage_survives_exception_truncation_and_early_cancel() {
        let mut provider=crate::config::ProviderDefinition::default();provider.id="kiro-test".into();provider.enabled=true;provider.models=vec!["model".into()];provider.credential.source=CredentialSource::None;let mut settings=GatewaySettings::default();settings.providers=vec![provider];let candidate=RoutingRuntime::default().candidates_for_request(&settings,"kiro-test/model",false,None).unwrap().remove(0);
        for case in 0..3 {
            let ledger=ObservabilityLedger::new(None);let mut bytes=frame("metadataEvent",json!({"tokenUsage":{"uncachedInputTokens":1000,"cacheReadInputTokens":99_000,"outputTokens":1},"stopReason":"end_turn"}));
            if case==0 {bytes.extend(frame("invalidStateEvent",json!({"reason":"fixture"})));}
            else if case==1 {bytes.extend([0,1,2]);}
            else {bytes.extend(frame("assistantResponseEvent",json!({"content":"partial text"})));}
            let raw=Response::builder().status(200).body(reqwest::Body::from(bytes)).unwrap();let seed=ObservationSeed::for_candidate(ledger.clone(),ObservabilitySettings::default(),format!("kiro-fail-{case}"),true,Instant::now(),1,&candidate);
            let response=kiro_incremental_response(reqwest::Response::from(raw),&candidate,seed,KiroRequestContext{conversation_id:"fixture".into(),response_id:"fixture".into(),streaming:true,tool_names:Default::default(),completion_tool:None},Arc::new(ResponseStateStore::new(None)),json!({"input":"fixture"}),true);let mut body=response.into_body().into_data_stream();
            while let Some(chunk)=body.next().await {match chunk {Err(_)=>break,Ok(bytes)=>if case==2 && String::from_utf8_lossy(&bytes).contains("response.output_text.delta") {break;}}}drop(body);
            assert_eq!(ledger.summary().failed_requests,1);assert_eq!(ledger.summary().input_tokens,100_000);assert!(ledger.cache_health_summary().sessions.is_empty());
        }
    }

}

fn kiro_buffered_value(bytes: &[u8], context: &KiroRequestContext, model: &str)
    -> Result<Value, (String, TokenUsage)>
{
    kiro_eventstream_to_response(bytes, context, model)
        .map_err(|message| (message, crate::kiro::buffered_usage(bytes)))
}

#[cfg(test)]
mod review13_kiro_billing_tests {
    use super::*;
    fn frame(kind: &str, value: Value) -> Vec<u8> {
        let mut headers = Vec::new();
        for (key, value) in [(":message-type", "event"), (":event-type", kind)] {
            headers.push(key.len() as u8); headers.extend_from_slice(key.as_bytes());
            headers.push(7); headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            headers.extend_from_slice(value.as_bytes());
        }
        let payload = value.to_string().into_bytes();
        let mut bytes = ((16 + headers.len() + payload.len()) as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&crc32fast::hash(&bytes).to_be_bytes());
        bytes.extend(headers); bytes.extend(payload);
        bytes.extend_from_slice(&crc32fast::hash(&bytes).to_be_bytes()); bytes
    }
    #[tokio::test]
    async fn review13_buffered_kiro_failure_retains_billing_once() {
        for streaming in [false, true] {
            let (_, context) = crate::kiro::responses_to_kiro(&json!({"input":"hi","stream":streaming,
                "tools":[{"type":"function","name":"tool","parameters":{"type":"object"}}]}), "fixture", None).unwrap();
            assert!(context.completion_tool.is_some());
            for malformed in [false, true] {
                let mut bytes = frame("metadataEvent", json!({"tokenUsage":{
                    "uncachedInputTokens":20,"cacheReadInputTokens":80,"outputTokens":7}}));
                if malformed { bytes.extend_from_slice(&[0, 1, 2]); }
                else { bytes.extend(frame("invalidStateEvent", json!({}))); }
                let (_, usage) = kiro_buffered_value(&bytes, &context, "fixture").unwrap_err();
                assert_eq!(usage.input_tokens,100); assert_eq!(usage.output_tokens,7);
                assert_eq!(usage.cached_input_tokens,80); assert!(usage.cache_read_reported);
                let ledger = ObservabilityLedger::new(None);
                ObservationSeed::without_candidate(ledger.clone(), ObservabilitySettings::default(),
                    "kiro-failure".into(), "fixture", streaming, Instant::now())
                    .finish(StatusCode::BAD_GATEWAY, Some("response_translation_error"), usage);
                let summary = ledger.summary();
                assert_eq!(summary.failed_requests,1); assert_eq!(summary.successful_requests,0);
                assert_eq!(summary.input_tokens,100); assert_eq!(summary.output_tokens,7);
            }
        }
    }
}

#[cfg(test)]
mod review13_buffered_kiro_regressions {
    use super::*;
    fn frame(kind: &str, payload: &[u8]) -> Vec<u8> {
        let mut headers = Vec::new();
        for (name, value) in [(":message-type", "event"), (":event-type", kind)] {
            headers.push(name.len() as u8); headers.extend_from_slice(name.as_bytes());
            headers.push(7); headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            headers.extend_from_slice(value.as_bytes());
        }
        let total = 16 + headers.len() + payload.len();
        let mut out = Vec::new();
        out.extend_from_slice(&(total as u32).to_be_bytes());
        out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        out.extend_from_slice(&crc32fast::hash(&out).to_be_bytes());
        out.extend(headers); out.extend_from_slice(payload);
        out.extend_from_slice(&crc32fast::hash(&out).to_be_bytes());
        out
    }
    #[tokio::test]
    async fn review13_buffered_kiro_translation_failure_records_usage_once() {
        for streaming in [false, true] {
            for truncated in [false, true] {
                let mut candidate = super::super::anthropic_compact::review13_resolved_auth_regressions::candidate();
                candidate.provider.transport = ProviderTransport::Kiro;
                let context = KiroRequestContext {
                    conversation_id: "test".into(), response_id: "test".into(), streaming,
                    completion_tool: streaming.then(|| "final".into()), tool_names: Default::default(),
                };
                let mut bytes = frame("metadataEvent", br#"{"tokenUsage":{"uncachedInputTokens":20,"cacheReadInputTokens":80,"outputTokens":7}}"#);
                if truncated { bytes.extend_from_slice(&[0,1,2]); }
                else { bytes.extend(frame("toolUseEvent", b"invalid JSON")); }
                let mut upstream = reqwest::Response::from(Response::builder().status(200)
                    .body(reqwest::Body::from(bytes)).unwrap());
                upstream.extensions_mut().insert(context);
                let ledger = ObservabilityLedger::new(None);
                let observation = ObservationSeed::for_candidate(ledger.clone(), ObservabilitySettings::default(),
                    "r13-kiro".into(), streaming, Instant::now(), 1, &candidate);
                let response = kiro_response(upstream, &candidate, observation, Arc::new(ResponseStateStore::new(None)), json!({"input":"fixture"}), true).await;
                assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
                let summary = ledger.summary();
                assert_eq!(summary.total_requests, 1);
                assert_eq!(summary.failed_requests, 1);
                assert_eq!(summary.successful_requests, 0);
                assert_eq!(summary.input_tokens, 100);
                assert_eq!(summary.cached_input_tokens, 80);
                assert_eq!(summary.output_tokens, 7);
                assert_eq!(summary.total_tokens, 107);
            }
        }
    }
}

/// Recover validated metadata from only the accepted prefix, including the
/// bounded part of an oversized chunk. No excess bytes enter either buffer.
pub(crate) async fn read_kiro_bounded(mut upstream: reqwest::Response, limit: u64) -> Result<Vec<u8>, (String, TokenUsage)> {
    let _lease = take_routing_attempt_lease(&mut upstream);
    let mut source = upstream.bytes_stream();
    let mut bytes = Vec::new();
    let mut collector = crate::kiro::BoundedUsageCollector::default();
    while let Some(chunk) = source.next().await {
        let chunk = chunk.map_err(|_| ("Kiro response body read failed".into(), collector.usage()))?;
        let remaining = limit.saturating_sub(bytes.len() as u64);
        let take = chunk.len().min(usize::try_from(remaining).unwrap_or(usize::MAX));
        collector.push(&chunk[..take]);
        bytes.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            return Err(("Kiro response exceeds the configured limit".into(), collector.usage()));
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod review15_bounded_kiro_regressions {
    use super::*;
    fn frame(tokens: Value) -> Vec<u8> {
        let name = b":event-type";
        let value = b"metadataEvent";
        let mut headers = vec![name.len() as u8]; headers.extend(name); headers.push(7);
        headers.extend((value.len() as u16).to_be_bytes()); headers.extend(value);
        let payload = json!({"tokenUsage":tokens}).to_string().into_bytes();
        let total = 16 + headers.len() + payload.len();
        let mut bytes = Vec::new(); bytes.extend((total as u32).to_be_bytes());
        bytes.extend((headers.len() as u32).to_be_bytes());
        bytes.extend(crc32fast::hash(&bytes).to_be_bytes());
        bytes.extend(headers); bytes.extend(payload);
        bytes.extend(crc32fast::hash(&bytes).to_be_bytes()); bytes
    }
    fn response(chunks: Vec<Result<Bytes, std::io::Error>>) -> reqwest::Response {
        axum::http::Response::builder().status(200)
            .body(reqwest::Body::wrap_stream(futures_util::stream::iter(chunks))).unwrap().into()
    }
    #[tokio::test]
    async fn review15_bounded_kiro_prefix_usage_survives_io_limit_and_split_frames() {
        let metadata = frame(json!({"uncachedInputTokens":100,"cacheReadInputTokens":900,"outputTokens":7}));
        for mode in 0..4 {
            let limit = metadata.len() as u64 + 5;
            let chunks = match mode {
                0 => vec![Ok(Bytes::from(metadata.clone())), Err(std::io::Error::other("fixture"))],
                1 => { let mut all = metadata.clone(); all.extend(vec![0; 4096]); vec![Ok(Bytes::from(all))] },
                2 => vec![Ok(Bytes::copy_from_slice(&metadata[..7])), Ok(Bytes::copy_from_slice(&metadata[7..])), Err(std::io::Error::other("fixture"))],
                _ => vec![Ok(Bytes::from(metadata.clone())), Ok(Bytes::from(vec![0;4096]))],
            };
            let (_, usage) = read_kiro_bounded(response(chunks), limit).await.unwrap_err();
            assert_eq!(usage.input_tokens, 1000); assert_eq!(usage.output_tokens, 7);
            assert_eq!(usage.cached_input_tokens, 900); assert!(usage.cache_read_reported);
        }
        let (_, usage) = read_kiro_bounded(response(vec![Ok(Bytes::from(metadata.clone()))]), metadata.len() as u64 - 1).await.unwrap_err();
        assert_eq!(usage.total_tokens, 0);
    }
    #[test]
    fn review15_invalid_usage_is_atomic_and_crc_required() {
        let good = frame(json!({"uncachedInputTokens":100,"outputTokens":7}));
        let mut collector = crate::kiro::BoundedUsageCollector::default(); collector.push(&good);
        collector.push(&frame(json!({"uncachedInputTokens":999,"outputTokens":-1})));
        assert_eq!(collector.usage().input_tokens, 100);
        assert_eq!(collector.usage().output_tokens, 7);
        let mut corrupt = good; *corrupt.last_mut().unwrap() ^= 1;
        assert_eq!(crate::kiro::buffered_usage(&corrupt).total_tokens, 0);
    }
}


#[cfg(test)]
pub(super) mod review16_regressions {
    use super::*;
    pub(crate) fn candidate() -> RouteCandidate {
        super::super::anthropic_compact::review13_resolved_auth_regressions::candidate()
    }
    pub(crate) fn seed(ledger: &ObservabilityLedger, candidate: &RouteCandidate, streaming: bool) -> ObservationSeed {
        let mut seed = ObservationSeed::for_candidate(ledger.clone(), ObservabilitySettings { request_log: true, usage_log: true, ..Default::default() }, "review16".into(), streaming, Instant::now(), 1, candidate);
        seed.request_diagnostic = Some(crate::observability::RequestDiagnostic { client_thread_id: Some("01a13ddb-8d21-7cb2-8f00-23a8a09af079".into()), ..Default::default() });
        seed
    }
    pub(crate) fn response(chunks: Vec<Result<Bytes, std::io::Error>>, status: StatusCode) -> reqwest::Response {
        axum::http::Response::builder().status(status).body(reqwest::Body::wrap_stream(futures_util::stream::iter(chunks))).unwrap().into()
    }
    pub(crate) fn frame(kind: &str, value: Value) -> Vec<u8> {
        let name = b":event-type";
        let mut headers = vec![name.len() as u8]; headers.extend_from_slice(name); headers.push(7);
        headers.extend_from_slice(&(kind.len() as u16).to_be_bytes()); headers.extend_from_slice(kind.as_bytes());
        let payload = serde_json::to_vec(&value).unwrap();
        let total = 16 + headers.len() + payload.len();
        let mut bytes = Vec::new(); bytes.extend_from_slice(&(total as u32).to_be_bytes()); bytes.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&crc32fast::hash(&bytes).to_be_bytes()); bytes.extend(headers); bytes.extend(payload);
        bytes.extend_from_slice(&crc32fast::hash(&bytes).to_be_bytes()); bytes
    }
    pub(crate) fn metadata() -> Vec<u8> {
        frame("metadataEvent", json!({"tokenUsage":{"uncachedInputTokens":17,"outputTokens":3},"stopReason":"end_turn"}))
    }
    fn kiro_candidate() -> RouteCandidate {
        let mut c = candidate(); c.provider.transport = ProviderTransport::Kiro; c
    }
    #[tokio::test]
    async fn review16_kiro_http_json_buffered_and_incremental_replay_images_and_answer() {
        for mode in 0..3 {
            let c = kiro_candidate(); let ledger = ObservabilityLedger::new(None);
            let store = Arc::new(ResponseStateStore::new(None));
            let mut request = json!({"model":"test","store":false,"stream":mode != 0,"input":[{"role":"user","content":[{"type":"input_text","text":"describe image"},{"type":"input_image","image_url":"data:image/jpeg;base64,YWJj"}]}]});
            if mode == 1 { request["tools"] = json!([{"type":"function","name":"lookup","parameters":{"type":"object"}}]); }
            let (_, context) = crate::kiro::responses_to_kiro(&request, "test", None).unwrap();
            let id = context.response_id.clone();
            let mut bytes = if let Some(name) = &context.completion_tool {
                frame("toolUseEvent", json!({"toolUseId":"final","name":name,"input":"{\"answer\":\"image answer\"}","stop":true}))
            } else { frame("assistantResponseEvent", json!({"content":"image answer"})) };
            bytes.extend(metadata());
            let split = bytes.len()/2;
            let mut upstream = response(vec![Ok(Bytes::copy_from_slice(&bytes[..split])), Ok(Bytes::copy_from_slice(&bytes[split..]))], StatusCode::OK);
            upstream.extensions_mut().insert(context);
            let client = adapt_successful_response(upstream, &c, seed(&ledger, &c, mode != 0), response_tool_map(&request), &store, &request, true, false).await;
            let bytes = to_bytes(client.into_body(), 1024 * 1024).await.unwrap();
            let value = if mode == 0 { serde_json::from_slice::<Value>(&bytes).unwrap() } else {
                let values = super::super::stream::drain_sse_values(&mut Vec::new(), &bytes).unwrap();
                let terminal = values.iter().find(|v| matches!(v["type"].as_str(), Some("response.completed" | "response.incomplete"))).unwrap();
                let response = terminal["response"].clone();
                let done: Vec<_> = values.iter().filter(|v| v["type"] == "response.output_item.done").map(|v|v["item"].clone()).collect();
                assert_eq!(response["output"], json!(done)); response
            };
            assert_eq!(value["id"], id); assert_eq!(value["status"], "completed");
            let mut next = json!({"model":"test","store":false,"previous_response_id":id,"input":[{"role":"user","content":"continue"}]});
            assert!(store.expand_previous_response_input_with_hint(&mut next, None).expanded());
            assert_eq!(count_translated_input_images(&next), 1);
            assert!(next.to_string().contains("image answer"));
            let (wire, _) = crate::kiro::responses_to_kiro(&next, "test", None).unwrap();
            assert_eq!(count_translated_input_images(&wire), 1); assert!(wire.to_string().contains("image answer"));
        }
    }
    #[tokio::test]
    async fn review16_kiro_incremental_limit_crc_json_and_io_keep_prefix_usage() {
        for failure in ["limit", "crc", "json", "io"] {
            let mut c = kiro_candidate(); let ledger = ObservabilityLedger::new(None);
            let store = Arc::new(ResponseStateStore::new(None));
            let request = json!({"store":false,"stream":true,"input":"test"});
            let (_, context) = crate::kiro::responses_to_kiro(&request,"test",None).unwrap();
            let id = context.response_id.clone();
            let mut prefix = metadata(); let mut suffix = frame("assistantResponseEvent",json!({"content":"answer"}));
            if failure == "crc" { *suffix.last_mut().unwrap() ^= 1; }
            if failure == "json" { suffix = frame("assistantResponseEvent",json!(false)); }
            let chunks = if failure == "io" { vec![Ok(Bytes::from(prefix)), Err(std::io::Error::other("fixture"))] } else {
                if failure == "limit" { c.provider.limits.max_response_bytes = prefix.len() as u64; }
                prefix.extend(suffix); vec![Ok(Bytes::from(prefix))]
            };
            let mut upstream = response(chunks,StatusCode::OK); upstream.extensions_mut().insert(context);
            let client = adapt_successful_response(upstream,&c,seed(&ledger,&c,true),ResponseToolMap::default(),&store,&request,true,false).await;
            assert!(to_bytes(client.into_body(),1024*1024).await.is_err());
            assert_eq!(ledger.summary().input_tokens,17, "{failure}"); assert_eq!(ledger.summary().output_tokens,3);
            assert_eq!(ledger.summary().successful_requests,0); assert_eq!(ledger.summary().failed_requests,1);
            assert!(ledger.cache_health_summary().sessions.is_empty());
            let mut next=json!({"previous_response_id":id,"input":"next"});
            assert!(!store.expand_previous_response_input_with_hint(&mut next,None).expanded());
        }
    }
}


#[cfg(test)]
mod review18_regressions {
    use super::*;
    use super::review16_regressions::{candidate, seed, response};
    #[tokio::test]
    async fn review18_anthropic_json_and_stream_refusal_keep_billing_out_of_cache_window() {
        for reason in ["refusal", "content_filter", "max_tokens", "end_turn", "stop_sequence", "pause_turn", "tool_use"] {
            for streaming in [false, true] {
                let mut c = candidate(); c.capabilities.streaming = true;
                c.input_price_per_million = Some(2.0); c.output_price_per_million = Some(3.0);
                let ledger = ObservabilityLedger::new(None);
                let store = Arc::new(ResponseStateStore::new(None));
                let request = json!({"model":"test","stream":streaming,"input":"test","tools":[{"type":"function","name":"lookup"}]});
                let usage = json!({"input_tokens":20000,"cache_read_input_tokens":80000,"output_tokens":7});
                let content = if reason == "tool_use" {
                    json!([{"type":"tool_use","id":"call-fixture","name":"lookup","input":{"q":"test"}}])
                } else { json!([{"type":"text","text":"fixture answer"}]) };
                let payload = if streaming {
                    let mut events = vec![json!({"type":"message_start","message":{"usage":usage}})];
                    if reason == "tool_use" {
                        events.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call-fixture","name":"lookup","input":{}}}));
                        events.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"q\":\"test\"}"}}));
                    } else {
                        events.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}));
                        events.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"fixture answer"}}));
                    }
                    events.extend([json!({"type":"content_block_stop","index":0}),
                        json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":7}}),
                        json!({"type":"message_stop"})]);
                    events.iter().map(|v| sse(v["type"].as_str().unwrap(),v)).collect::<String>()
                } else { json!({"type":"message","content":content,"stop_reason":reason,"usage":usage}).to_string() };
                let upstream = response(vec![Ok(Bytes::from(payload))],StatusCode::OK);
                let client = adapt_successful_response(upstream,&c,seed(&ledger,&c,streaming),response_tool_map(&request),&store,&request,true,false).await;
                let bytes = to_bytes(client.into_body(), 1024*1024).await.unwrap();
                let value = if streaming {
                    super::super::stream::drain_sse_values(&mut Vec::new(),&bytes).unwrap().into_iter()
                        .find(|v| matches!(v["type"].as_str(), Some("response.completed" | "response.incomplete"))).unwrap()["response"].clone()
                } else { serde_json::from_slice::<Value>(&bytes).unwrap() };
                let failed = matches!(reason,"refusal" | "content_filter" | "max_tokens");
                assert_eq!(value["status"], if failed {"incomplete"} else {"completed"}, "{reason}/{streaming}");
                if failed { assert_eq!(value["incomplete_details"]["reason"], if reason=="max_tokens" {"max_output_tokens"} else {"content_filter"}); }
                let summary = ledger.summary();
                assert_eq!(summary.input_tokens,100000); assert_eq!(summary.cached_input_tokens,80000);
                assert_eq!(summary.output_tokens,7); assert!((summary.estimated_cost_usd-0.200021).abs()<1e-10);
                assert_eq!(summary.failed_requests,u64::from(failed));
                assert_eq!(summary.successful_requests,u64::from(!failed));
                let health = ledger.cache_health_summary();
                if failed { assert!(health.sessions.is_empty(), "{reason}/{streaming}"); }
                else { assert_eq!(health.sessions[0].rolling_requests,1); }
            }
        }
    }
}
