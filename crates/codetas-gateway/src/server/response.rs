use super::*;

pub(crate) fn validated_retry_after(headers: &HeaderMap) -> Option<(HeaderValue, Option<Duration>)> {
    const MAX_COOLDOWN: Duration = Duration::from_secs(10 * 60);
    let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
    if value.is_empty() || value.len() > 128 {
        return None;
    }
    let header_value = HeaderValue::from_str(value).ok()?;
    if value == "0" {
        return Some((header_value, None));
    }
    if value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        let seconds = value.parse::<f64>().ok()?;
        if !seconds.is_finite() || seconds <= 0.0 {
            return None;
        }
        let millis = (seconds * 1_000.0).ceil().clamp(1.0, 600_000.0) as u64;
        return Some((
            header_value,
            Some(Duration::from_millis(millis).min(MAX_COOLDOWN)),
        ));
    }
    let retry_at = httpdate::parse_http_date(value).ok()?;
    let delay = retry_at.duration_since(SystemTime::now()).ok()?;
    if delay.is_zero() {
        return None;
    }
    Some((header_value, Some(delay.min(MAX_COOLDOWN))))
}

pub(crate) async fn upstream_error(
    upstream: reqwest::Response,
    retry_after: Option<&HeaderValue>,
) -> Response<Body> {
    let status = upstream.status();
    // Drain a bounded body, but never reflect provider text. Some upstreams
    // echo prompts, headers, or credentials in diagnostic responses.
    let body = read_bounded(upstream, 64 * 1024).await.unwrap_or_default();
    if status.as_u16() == 400 {
        record_upstream_error(status, &body);
    }
    let mut response = error_response(
        status,
        "provider_error",
        &format!("provider request failed with HTTP {}", status.as_u16()),
    );
    if let Some(retry_after) = retry_after {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, retry_after.clone());
    }
    response
}

/// Temporary opt-in diagnostic: appends bounded upstream 400 bodies to a local
/// file when CODETAS_LOG_UPSTREAM_ERRORS is set. Never affects responses.
pub(crate) fn record_upstream_error(status: reqwest::StatusCode, body: &[u8]) {
    if std::env::var_os("CODETAS_LOG_UPSTREAM_ERRORS").is_none() {
        return;
    }
    let Ok(text) = std::str::from_utf8(body) else {
        return;
    };
    let mut text: String = text.chars().take(2048).collect();
    if text.trim().is_empty() {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    text.push('\n');
    let entry = format!("[{now}] HTTP {} {text}", status.as_u16());
    let path = std::env::temp_dir().join("codetas-upstream-errors.log");
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        let _ = file.write_all(entry.as_bytes());
    }
}

pub(crate) async fn bounded_json(upstream: reqwest::Response, limit: u64) -> Result<Value, String> {
    let bytes = read_bounded(upstream, limit).await?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("provider returned invalid JSON: {error}"))
}

pub(crate) async fn read_bounded(upstream: reqwest::Response, limit: u64) -> Result<Vec<u8>, String> {
    let mut stream = upstream.bytes_stream();
    let mut output = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("provider response failed: {error}"))?;
        if output.len() as u64 + chunk.len() as u64 > limit {
            return Err("provider response exceeded the configured limit".into());
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

pub(crate) fn json_response(status: StatusCode, value: Value) -> Response<Body> {
    let body = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

pub(crate) fn error_response(status: StatusCode, code: &str, message: &str) -> Response<Body> {
    json_response(
        status,
        json!({
            "error": {
                "type": "codetas_gateway_error",
                "code": code,
                "message": message
            }
        }),
    )
}

#[cfg(test)]
mod subagent_shrink_tests {
    use super::*;
    use crate::config::ProviderDefinition;

    fn candidate(context: u64) -> RouteCandidate {
        RouteCandidate {
            provider: ProviderDefinition::default(),
            upstream_model: "kimi/k3".into(),
            exposed_model: "kimi/k3".into(),
            credential: None,
            account_id: None,
            target_key: "test".into(),
            route_id: None,
            failure_threshold: 0,
            quota_threshold_percent: 0,
            input_price_per_million: None,
            output_price_per_million: None,
            context_window: Some(context),
            max_input_tokens: None,
            max_output_tokens: None,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
        }
    }

    fn input_with(count: usize) -> Value {
        let mut items = Vec::new();
        for i in 0..count {
            items.push(json!({"type": "reasoning", "id": format!("rs_{i}"), "summary": "x".repeat(50)}));
            items.push(json!({"type": "custom_tool_call_output", "call_id": format!("c_{i}"), "output": "y".repeat(50)}));
        }
        json!({"input": items})
    }

    #[test]
    fn shrinks_when_over_budget() {
        // 大きな input を小さい context に
        let mut body = input_with(20);
        let cand = candidate(300);
        let changed = shrink_subagent_input(&mut body, &cand, 0).unwrap();
        assert!(changed);
        assert!(estimate_input_items_tokens(body["input"].as_array().unwrap()) <= 300);
    }

    #[test]
    fn keeps_within_budget_untouched() {
        let mut body = input_with(2);
        let cand = candidate(200_000);
        let changed = shrink_subagent_input(&mut body, &cand, 0).unwrap();
        assert!(!changed);
        assert_eq!(body["input"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn drops_reasoning_before_tool_outputs() {
        let mut body = input_with(10);
        let cand = candidate(4_000);
        // 半分程度まで縮小される
        shrink_subagent_input(&mut body, &cand, 0).unwrap();
        let items = body["input"].as_array().unwrap();
        let reasoning_left = items.iter().filter(|i| i.get("type").and_then(Value::as_str) == Some("reasoning")).count();
        let outputs_left = items.iter().filter(|i| i.get("type").and_then(Value::as_str) == Some("custom_tool_call_output")).count();
        // reasoning を優先的に落とす
        assert!(reasoning_left <= outputs_left);
    }
}
