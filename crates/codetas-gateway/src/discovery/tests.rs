use super::*;
use crate::config::ProviderProtocol;
use crate::oauth::parse_antigravity_cli_models;

#[test]
fn opendesign_limits_are_read_from_their_nested_fields() {
    let rows = serde_json::json!({"data": [
        {"id": "deepseek-v4.1-flash", "architecture": {"input_modalities": ["text", "image"]},
         "metadata": {"context_limit": 1048000, "context_budget": 328000, "output_budget": 128000}},
        {"id": "deepseek-v4-pro", "architecture": {"input_modalities": ["text"]},
         "metadata": {"context_limit": 1048000, "context_budget": 328000}}
    ]});
    let provider = crate::provider_presets()
        .into_iter()
        .find(|preset| preset.id == "opendesign")
        .unwrap()
        .instantiate(None)
        .unwrap();
    let models = parse_models(&provider, &rows).unwrap();
    let by_id = |id: &str| models.iter().find(|model| model.model_id == id).unwrap();
    let flash = by_id("deepseek-v4.1-flash");
    let pro = by_id("deepseek-v4-pro");
    assert_eq!(flash.context_window, Some(1_048_000));
    assert!(flash.input_modalities.contains(&"image".to_string()));
    assert!(!pro.input_modalities.contains(&"image".to_string()));
    // 328k budgets are raised to 400k; a window under that stays the cap.
    assert_eq!(flash.max_input_tokens, Some(400_000));
    assert_eq!(
        flash.reasoning_efforts,
        ["low", "medium", "high", "xhigh", "max"]
    );
    assert_eq!(flash.default_reasoning_effort.as_deref(), Some("medium"));
    let small = parse_models(
        &provider,
        &serde_json::json!({"data": [{"id": "kimi-k2.7-code",
                "metadata": {"context_limit": 262000, "context_budget": 262000}}]}),
    )
    .unwrap();
    assert_eq!(small[0].max_input_tokens, Some(262_000));
}

#[test]
fn deepai_status_probe_treats_the_identity_error_as_authenticated() {
    // A valid credential answers 400 with this transport error type.
    assert_eq!(
        classify_deepai_probe(400, true),
        (
            true,
            true,
            None,
            "the running DeepAI Code model transport is reachable"
        )
    );
    assert_eq!(
        classify_deepai_probe(200, false),
        (
            true,
            true,
            None,
            "the running DeepAI Code model transport is reachable"
        )
    );
    // An invalid credential answers 403; a bare 400 stays an error.
    assert_eq!(
        classify_deepai_probe(403, false),
        (
            true,
            false,
            Some(403),
            "DeepAI Code is running but the model transport rejected the credential"
        )
    );
    assert_eq!(
        classify_deepai_probe(400, false).2,
        Some(400),
        "a 400 without the transport error type is not success"
    );
    assert!(!classify_deepai_probe(500, false).1);
}

#[test]
fn parses_openai_and_google_model_lists() {
    let mut provider = ProviderDefinition {
        id: "test".into(),
        name: "Test".into(),
        base_url: "https://models.example/v1".into(),
        protocol: ProviderProtocol::Responses,
        ..ProviderDefinition::default()
    };
    provider.discovery.enabled = true;
    let openai = parse_models(
        &provider,
        &serde_json::json!({
            "data": [{"id": "model-a"}]
        }),
    )
    .expect("OpenAI list should parse");
    assert_eq!(openai[0].model_id, "model-a");

    let google = parse_models(&provider, &serde_json::json!({
            "models": [{"name": "models/gemini-a", "displayName": "Gemini A", "inputTokenLimit": 1000}]
        })).expect("Google list should parse");
    assert_eq!(google[0].model_id, "gemini-a");
    assert_eq!(google[0].context_window, Some(1000));
}

#[test]
fn mistral_discovery_keeps_only_agent_capable_models_with_their_vision() {
    let provider = crate::provider_presets()
        .into_iter()
        .find(|preset| preset.id == "mistral")
        .unwrap()
        .instantiate(None)
        .unwrap();
    let models = parse_models(
            &provider,
            &serde_json::json!({"data": [
                {"id": "mistral-medium-latest", "max_context_length": 262144,
                    "capabilities": {"completion_chat": true, "function_calling": true, "vision": true}},
                {"id": "text-only-model",
                    "capabilities": {"completion_chat": true, "function_calling": true, "vision": false}},
                {"id": "mistral-embed",
                    "capabilities": {"completion_chat": false, "function_calling": false}},
                {"id": "chat-without-tools",
                    "capabilities": {"completion_chat": true, "function_calling": false}}
            ]}),
        )
        .unwrap();
    let ids: Vec<_> = models.iter().map(|model| model.model_id.as_str()).collect();
    assert_eq!(ids, ["mistral-medium-latest", "text-only-model"]);
    assert_eq!(models[0].context_window, Some(262_144));
    assert!(models[0].input_modalities.contains(&"image".to_string()));
    assert!(!models[1].input_modalities.contains(&"image".to_string()));
}

#[test]
fn mistral_catalog_row_brings_its_label_and_reasoning_levels() {
    let provider = crate::provider_presets()
        .into_iter()
        .find(|preset| preset.id == "mistral")
        .unwrap()
        .instantiate(None)
        .unwrap();
    assert!(is_mistral_catalog(&provider));
    let models = parse_models(
        &provider,
        &serde_json::json!({"models": [{
            "id": "mistral-next", "label": "Mistral Next", "max_context_length": 1000000,
            "parameters": {"reasoning": ["none", "low", "high"]},
            "capabilities": {"completion_chat": true, "function_calling": true, "vision": true}
        }]}),
    )
    .unwrap();
    assert_eq!(models[0].display_name.as_deref(), Some("Mistral Next"));
    assert_eq!(models[0].reasoning_efforts, ["low", "high"]);
    assert_eq!(models[0].default_reasoning_effort.as_deref(), Some("high"));
    assert_eq!(models[0].context_window, Some(1_000_000));
}

#[test]
fn mistral_catalog_efforts_keep_only_what_settings_accept() {
    let efforts = mistral_catalog_efforts(&serde_json::json!({
        "parameters": {"reasoning": ["None", "LOW", "low", " High ", "extreme", 3, "max"]}
    }));
    assert_eq!(efforts, ["low", "high", "max"]);
    // The filter is the validator's own check, so what it keeps cannot fail the refresh.
    assert!(efforts
        .iter()
        .all(|effort| crate::config::is_reasoning_effort(effort)));
}

#[test]
fn mistral_default_effort_is_high_or_else_the_top_level() {
    let levels = |efforts: &[&str]| efforts.iter().map(|e| e.to_string()).collect::<Vec<_>>();
    assert_eq!(
        mistral_default_effort(&levels(&["low", "high", "max"])).as_deref(),
        Some("high")
    );
    assert_eq!(
        mistral_default_effort(&levels(&["low", "medium"])).as_deref(),
        Some("medium")
    );
    assert_eq!(mistral_default_effort(&[]), None);
}

#[test]
fn only_an_unreachable_mistral_catalog_falls_back_to_the_full_list() {
    let failed = |status| {
        Err(ModelDiscoveryError::Status {
            status,
            message: String::new(),
        })
    };
    assert!(mistral_catalog_is_out_of_reach(&failed(403)));
    assert!(mistral_catalog_is_out_of_reach(&failed(404)));
    for status in [401, 429, 500, 503] {
        assert!(!mistral_catalog_is_out_of_reach(&failed(status)));
    }
    assert!(!mistral_catalog_is_out_of_reach(&Ok(Vec::new())));
}

#[test]
fn label_names_a_model_only_for_mistral() {
    let provider = ProviderDefinition {
        id: "other".into(),
        name: "Other".into(),
        base_url: "https://api.example.com/v1".into(),
        protocol: ProviderProtocol::ChatCompletions,
        ..ProviderDefinition::default()
    };
    let models = parse_models(
        &provider,
        &serde_json::json!({"data": [{"id": "model-a", "label": "free"}]}),
    )
    .unwrap();
    assert_eq!(models[0].display_name, None);
}

#[test]
fn discovered_xai_models_inherit_context_when_the_api_omits_it() {
    let mut provider = ProviderDefinition {
        id: "xai".into(),
        name: "xAI".into(),
        base_url: "https://api.x.ai/v1".into(),
        protocol: ProviderProtocol::ChatCompletions,
        ..ProviderDefinition::default()
    };
    provider
        .model_context_windows
        .insert("grok-4.6".into(), 500_000);
    let models = parse_models(
        &provider,
        &serde_json::json!({
            "data": [
                {"id": "grok-4.6"},
                {"id": "grok-4.7"},
                {"id": "grok-4.8", "context_length": 512_000},
                {"id": "grok-imagine-image"}
            ]
        }),
    )
    .expect("xAI list should parse");
    let window = |id: &str| {
        models
            .iter()
            .find(|model| model.model_id == id)
            .and_then(|model| model.context_window)
    };
    assert_eq!(window("grok-4.6"), Some(500_000));
    assert_eq!(window("grok-4.7"), Some(500_000));
    assert_eq!(window("grok-4.8"), Some(512_000));
    assert_eq!(window("grok-imagine-image"), None);
}

#[test]
fn provider_image_capability_is_scoped_to_explicit_image_models() {
    let mut provider = ProviderDefinition {
        id: "xai".into(),
        name: "xAI".into(),
        base_url: "https://models.example/v1".into(),
        protocol: ProviderProtocol::Responses,
        ..ProviderDefinition::default()
    };
    provider.capabilities.image_generation = true;
    provider.image_generation_models = vec!["image-model".into()];

    let models = parse_models(
        &provider,
        &serde_json::json!({
            "data": [{"id": "chat-model"}, {"id": "image-model"}]
        }),
    )
    .expect("model list should parse");

    assert!(!models[0].capabilities.image_generation);
    assert!(models[1].capabilities.image_generation);
}

#[tokio::test]
async fn codex_online_status_errors_are_not_cached_or_leaked() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for status in [401, 403, 429, 500, 302] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!(
            "http://{}/models?client_version=0.159.1",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let count = socket.read(&mut request).await.unwrap();
            let headers = String::from_utf8_lossy(&request[..count]).to_ascii_lowercase();
            assert!(headers.contains("authorization: bearer test-token"));
            assert!(headers.contains("chatgpt-account-id: test-account"));
            assert!(headers.contains("client_version=0.159.1"));
            socket.write_all(format!("HTTP/1.1 {status} Error\r\nContent-Length: 12\r\nLocation: http://127.0.0.1:1/secret\r\nConnection: close\r\n\r\nsecret-token").as_bytes()).await.unwrap();
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let error = fetch_codex_model_list(
            &client,
            url,
            &ProviderDefinition::default(),
            "test-token",
            "test-account",
            "0.159.1",
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, ModelDiscoveryError::Status { status: actual, .. } if actual == status)
        );
        assert!(!error.to_string().contains("secret-token"));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn a_failed_discovery_keeps_the_status_but_not_the_response_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    for status in [401, 403, 404, 500] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let _ = socket.read(&mut request).await.unwrap();
            socket.write_all(format!("HTTP/1.1 {status} Error\r\nContent-Length: 12\r\nConnection: close\r\n\r\nsecret-token").as_bytes()).await.unwrap();
        });
        let mut provider = ProviderDefinition {
            id: "custom".into(),
            name: "Custom".into(),
            base_url,
            protocol: ProviderProtocol::ChatCompletions,
            allow_private_network: true,
            ..ProviderDefinition::default()
        };
        provider.discovery.enabled = true;
        let error = discover_provider_models(&provider).await.unwrap_err();
        assert!(
            matches!(error, ModelDiscoveryError::Status { status: actual, .. } if actual == status)
        );
        assert!(!error.to_string().contains("secret-token"));
        server.await.unwrap();
    }
}

#[test]
fn codex_online_list_imports_visible_models_and_efforts() {
    let provider = ProviderDefinition {
        id: "openai".into(),
        ..ProviderDefinition::default()
    };
    let value = serde_json::json!({"models": [
        {"slug": "gpt-6.1-sol", "visibility": "list", "display_name": "GPT-6.1-Sol",
         "context_window": 272000, "input_modalities": ["text", "image"],
         "supported_reasoning_levels": [{"effort": "low"}, {"effort": "high"}],
         "default_reasoning_level": "low"},
        {"slug": "hidden-model", "visibility": "hide"},
        {"slug": "", "visibility": "list"}
    ]});
    let models = parse_codex_model_list(&provider, &value).unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].provider_id, "openai");
    assert_eq!(models[0].model_id, "gpt-6.1-sol");
    assert_eq!(models[0].context_window, Some(272000));
    assert_eq!(models[0].input_modalities, ["text", "image"]);
    assert_eq!(models[0].reasoning_efforts, ["low", "high"]);
    assert_eq!(models[0].default_reasoning_effort.as_deref(), Some("low"));
    for invalid in [serde_json::json!({}), serde_json::json!({"models": []})] {
        assert!(parse_codex_model_list(&provider, &invalid).is_err());
    }
}

#[test]
fn antigravity_claude_55_variants_have_separate_effort_metadata() {
    let mut output = String::new();
    for family in ["opus", "sonnet"] {
        for effort in ["low", "medium", "high"] {
            output.push_str(&format!(
                "claude-{family}-5-5-{effort}\tClaude {family} 5.5 ({effort})\n"
            ));
        }
    }
    output.push_str("claude-opus-4-6-thinking\tClaude Opus 4.6 (Thinking)\n");
    output.push_str("gpt-oss-120b-medium\tGPT-OSS 120B (Medium)\n");
    let rows = parse_antigravity_cli_models(&output);
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[2].0, "claude-opus-4-6-thinking");
    assert_eq!(rows[3].0, "gpt-oss-120b-medium");
    // Existing saved providers may not yet have registry effort metadata.
    let models = antigravity_models_from_cli_rows(&ProviderDefinition::default(), &rows);
    for family in ["opus", "sonnet"] {
        let model = models
            .iter()
            .find(|m| m.model_id == format!("claude-{family}-5-5"))
            .unwrap();
        assert_eq!(model.reasoning_efforts, ["low", "medium", "high"]);
        assert_eq!(model.default_reasoning_effort.as_deref(), Some("medium"));
        assert_eq!(
            model.display_name.as_deref(),
            Some(format!("Claude {family} 5.5").as_str())
        );
    }
}

#[test]
fn antigravity_cli_models_collapse_effort_variants() {
    let rows = parse_antigravity_cli_models(
        "Fetching available models...\n\
gemini-3.6-flash-high\tGemini 3.6 Flash (High)\n\
gemini-3.6-flash-medium\tGemini 3.6 Flash (Medium)\n\
gemini-3.6-flash-low\tGemini 3.6 Flash (Low)\n\
claude-sonnet-4-6\tClaude Sonnet 4.6 (Thinking)\n",
    );
    assert_eq!(
        rows.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        ["gemini-3.6-flash", "claude-sonnet-4-6"]
    );

    let preserved = parse_antigravity_cli_models("gpt-oss-120b-medium\tGPT-OSS 120B (Medium)\n");
    assert_eq!(preserved[0].0, "gpt-oss-120b-medium");
    let mut provider = ProviderDefinition {
        id: "google-antigravity".into(),
        name: "Google Antigravity".into(),
        base_url: "https://daily-cloudcode-pa.googleapis.com".into(),
        protocol: ProviderProtocol::GeminiGenerateContent,
        ..ProviderDefinition::default()
    };
    // Image input is on by default; start from a text-only provider.
    provider.capabilities.vision = false;
    provider
        .model_context_windows
        .insert("gemini-3.6-flash".into(), 1_048_576);
    provider.model_reasoning_efforts.insert(
        "gemini-3.6-flash".into(),
        vec!["low".into(), "medium".into(), "high".into()],
    );
    let models = antigravity_models_from_cli_rows(&provider, &rows);
    let flash = models
        .iter()
        .find(|model| model.model_id == "gemini-3.6-flash")
        .unwrap();
    assert_eq!(flash.context_window, Some(1_048_576));
    assert_eq!(flash.reasoning_efforts, ["low", "medium", "high"]);
    assert_eq!(flash.input_modalities, ["text"]);

    provider.capabilities.vision = true;
    let vision = antigravity_models_from_cli_rows(&provider, &rows)
        .into_iter()
        .find(|model| model.model_id == "gemini-3.6-flash")
        .unwrap();
    assert_eq!(vision.input_modalities, ["text", "image"]);
}
