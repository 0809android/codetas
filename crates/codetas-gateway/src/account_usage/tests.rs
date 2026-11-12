use super::*;
fn parsed(provider: &str, value: Value) -> ProviderAccountUsage {
    let mut out = ProviderAccountUsage {
        provider_id: provider.into(),
        account_id: Some("a".into()),
        status: "error".into(),
        source: "test".into(),
        observed_at_ms: None,
        checked_at_ms: 0,
        stale: false,
        plan: None,
        windows: vec![],
        model_pools: BTreeMap::new(),
        balance: None,
        error: None,
    };
    parse(provider, &value, &mut out);
    out
}
#[test]
fn codex_shared_windows_and_reset_are_real_values() {
    let out = parsed(
        "openai",
        json!({"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":0,"reset_at":1800000000,"limit_window_seconds":18000},"secondary_window":{"used_percent":34}}}),
    );
    assert_eq!(out.status, "ok");
    assert_eq!(out.windows.len(), 2);
    assert_eq!(out.windows[0].used_percent, 0.0);
    assert_eq!(out.windows[0].resets_at.as_deref(), Some("1800000000000"));
    assert!(out.windows[0].model_id.is_none());
}
#[test]
fn numeric_reset_strings_use_the_same_units_as_numbers() {
    assert_eq!(
        timestamp(&json!("1800000000"), true).as_deref(),
        Some("1800000000000")
    );
    assert_eq!(timestamp(&json!(null), true), None);
}
#[test]
fn opendesign_windows_come_from_credits_by_duration() {
    let mut out = parsed("opendesign", json!({}));
    out.status = "error".into();
    parse_opendesign_preflight(
        &json!({
            "workspaceId": "w1",
            "balanceUsd": "1.5",
            "codingPlan": {
                "eligible": true,
                "tier": "pro",
                "windows": [
                    {"durationSeconds": 604800, "usedCredits": "250", "limitCredits": "1000", "resetsAt": "2026-11-12T00:00:00Z"},
                    {"durationSeconds": 18000, "usedCredits": "0", "limitCredits": "100", "resetsAt": null}
                ]
            }
        }),
        "w1",
        &mut out,
    );
    assert_eq!(out.status, "ok");
    assert_eq!(out.plan.as_deref(), Some("Pro"));
    assert_eq!(out.windows[0].label, "5h");
    assert_eq!(out.windows[0].used_percent, 0.0);
    assert_eq!(out.windows[1].label, "7d");
    assert_eq!(out.windows[1].used_percent, 25.0);
    assert_eq!(
        out.windows[1].resets_at.as_deref(),
        Some("2026-11-12T00:00:00Z")
    );
    assert!(out.balance.unwrap().contains("1.5000 USD"));
}
#[test]
fn opendesign_rejects_another_workspace_and_bad_credits() {
    let mut out = parsed("opendesign", json!({}));
    parse_opendesign_preflight(
        &json!({"workspaceId": "other", "codingPlan": {"windows": []}}),
        "w1",
        &mut out,
    );
    assert_eq!(out.status, "error");
    assert!(out.windows.is_empty());
    let mut out = parsed("opendesign", json!({}));
    parse_opendesign_preflight(
        &json!({"codingPlan": {"eligible": true, "windows": [
            {"durationSeconds": 18000, "usedCredits": "-1", "limitCredits": "100"},
            {"durationSeconds": 18000, "usedCredits": "1", "limitCredits": "0"}
        ]}}),
        "w1",
        &mut out,
    );
    assert_eq!(out.status, "error");
    assert!(out.windows.is_empty());
}
#[test]
fn missing_and_null_are_not_zero() {
    for data in [
        json!({}),
        json!({"five_hour":{"utilization":null}}),
        json!({"five_hour":{"utilization":101}}),
    ] {
        let out = parsed("anthropic", data);
        assert_eq!(out.status, "error");
        assert!(out.windows.is_empty());
    }
}
#[test]
fn claude_preserves_distinct_model_families() {
    let out = parsed(
        "anthropic",
        json!({"five_hour":{"utilization":4},"seven_day_opus":{"utilization":99,"resets_at":"2026-11-04T01:00:00Z"}}),
    );
    assert_eq!(out.windows.len(), 2);
    assert_eq!(out.windows[1].model_id.as_deref(), Some("claude-opus"));
}
#[test]
fn minimax_count_is_remaining_not_consumed() {
    let out = parsed(
        "minimax",
        json!({"model_remains":[{"model_name":"MiniMax-M2","current_interval_total_count":100,"current_interval_usage_count":80}]}),
    );
    assert_eq!(out.windows[0].used_percent, 20.0);
}
#[test]
fn mistral_budgets_are_two_shared_monthly_windows_with_their_resets() {
    // The shape of a live Pro plan response.
    let out = parsed(
        "mistral",
        json!({
            "vibe_budget":{"usage_percentage":40.5,"initial_budget":300.0,"currency":"USD","reset_at":"2026-12-01T00:00:00Z","payg_enabled":false},
            "api_budget":{"usage_percentage":0.0,"initial_budget":30.0,"currency":"USD","reset_at":"2026-12-01T00:00:00Z","payg_enabled":false},
            "usage_percentage":0.0,"initial_budget":30.0,"currency":"USD","reset_at":"2026-12-01T00:00:00Z"
        }),
    );
    assert_eq!(out.status, "ok");
    assert_eq!(out.windows.len(), 2);
    assert_eq!(out.windows[0].label, "Vibe Code · 300 USD");
    assert_eq!(out.windows[0].used_percent, 40.5);
    assert!(out.windows[0].model_id.is_none());
    assert_eq!(
        out.windows[0].resets_at.as_deref(),
        Some("2026-12-01T00:00:00Z")
    );
    assert_eq!(out.windows[1].label, "API · 30 USD");
    assert_eq!(out.windows[1].used_percent, 0.0);
    assert_eq!(out.plan.as_deref(), Some("枠を超えると停止"));
}
#[test]
fn mistral_usage_past_the_allowance_stays_on_the_gauge() {
    let out = parsed(
        "mistral",
        json!({"vibe_budget":{"usage_percentage":123.0,"initial_budget":30,"payg_enabled":true}}),
    );
    assert_eq!(out.windows[0].used_percent, 100.0);
    assert_eq!(out.plan.as_deref(), Some("枠を超えると従量課金"));
}
#[test]
fn mistral_without_an_allowance_is_not_zero_percent() {
    for data in [
        json!({"vibe_budget":null}),
        json!({"vibe_budget":{"usage_percentage":0,"initial_budget":0}}),
        json!({"vibe_budget":{"usage_percentage":null,"initial_budget":30}}),
    ] {
        let out = parsed("mistral", data);
        assert_eq!(out.status, "error");
        assert!(out.windows.is_empty());
    }
}
#[test]
fn mistral_plan_names_the_billing_bucket_of_the_key() {
    let mut vibe = parsed(
        "mistral",
        json!({"vibe_budget":{"usage_percentage":1,"initial_budget":30,"payg_enabled":false}}),
    );
    apply_mistral_account(
        &mut vibe,
        Some(&json!({"plan_name":"INDIVIDUAL"})),
        Some(&json!({"plan_type":"CHAT","plan_name":"FREE"})),
    );
    assert_eq!(vibe.plan.as_deref(), Some("Pro · 枠を超えると停止"));

    let mut api = parsed("mistral", json!({"vibe_budget":null}));
    apply_mistral_account(
        &mut api,
        Some(&json!({"plan_name":"PRO","vibe_payg_enabled":false,"payg_enabled":true})),
        Some(&json!({"plan_type":"api","plan_name":"Workspace"})),
    );
    let plan = api.plan.unwrap();
    assert!(plan.starts_with("APIキー"));
    assert!(plan.ends_with("枠を超えると従量課金"));
    assert!(api.error.unwrap().contains("月間枠がありません"));

    let mut unread = parsed("mistral", json!({"vibe_budget":null}));
    apply_mistral_account(&mut unread, None, None);
    assert!(unread.plan.is_none());
}
#[test]
fn mistral_key_kind_does_not_hide_why_a_read_failed() {
    let mut failed = parsed("mistral", json!({"vibe_budget":null}));
    failed.error = Some("使用量API: HTTP 500".into());
    apply_mistral_account(&mut failed, None, Some(&json!({"plan_type":"api"})));
    assert_eq!(failed.error.as_deref(), Some("使用量API: HTTP 500"));
}
#[tokio::test]
async fn mistral_usage_is_only_read_from_the_official_host() {
    let mut provider = crate::provider_presets()
        .into_iter()
        .find(|p| p.id == "mistral")
        .unwrap()
        .instantiate(None)
        .unwrap();
    assert!(source(&provider).is_some());
    provider.base_url = "https://proxy.example/v1".into();
    assert!(source(&provider).is_none());
    provider.enabled = true;
    let out = fetch_provider_account_usage(&provider, None, None).await;
    assert_eq!(out.status, "unsupported");
}
#[test]
fn every_preset_with_a_usage_api_keeps_it_and_loses_it_behind_a_proxy() {
    for id in [
        "openai",
        "anthropic",
        "google-antigravity",
        "deepseek",
        "openrouter",
        "zai",
        "zhipu-bigmodel",
        "minimax",
        "minimax-cn",
        "github-copilot",
        "mistral",
    ] {
        let mut provider = crate::provider_presets()
            .into_iter()
            .find(|p| p.id == id)
            .unwrap_or_else(|| panic!("no preset for {id}"))
            .instantiate(None)
            .unwrap();
        if id != "openai" {
            provider.credential = crate::oauth_session_credential(id);
        }
        assert!(source(&provider).is_some(), "{id} lost its usage read");
        for elsewhere in [
            "https://proxy.example/v1",
            "https://evil-deepseek.com/v1",
            "https://api.mistral.ai.proxy.example/v1",
        ] {
            provider.base_url = elsewhere.into();
            assert!(source(&provider).is_none(), "{id} at {elsewhere}");
        }
    }
}
#[test]
fn providers_without_a_preset_keep_their_usage_read_on_their_own_hosts() {
    for (id, base_url) in [
        ("command-code", "https://api.commandcode.ai/v1"),
        ("commandcode", "https://api.commandcode.ai/v1"),
        ("google-gemini-cli", "https://cloudcode-pa.googleapis.com"),
        ("gemini-cli", "https://cloudcode-pa.googleapis.com"),
        ("z-ai", "https://api.z.ai/api/coding/paas/v4"),
        ("github-copilot", "https://api.individual.githubcopilot.com"),
        ("openai", "https://api.openai.com/v1"),
    ] {
        let mut provider = ProviderDefinition {
            id: id.into(),
            base_url: base_url.into(),
            credential: crate::oauth_session_credential(id),
            ..ProviderDefinition::default()
        };
        assert!(source(&provider).is_some(), "{id} at {base_url}");
        provider.base_url = "https://proxy.example/v1".into();
        assert!(source(&provider).is_none(), "{id} behind a proxy");
    }
}
#[tokio::test]
async fn the_codex_login_only_reaches_openai() {
    for id in [
        "anthropic",
        "google-antigravity",
        "deepseek",
        "openrouter",
        "zai",
        "minimax",
        "github-copilot",
        "mistral",
    ] {
        let mut provider = crate::provider_presets()
            .into_iter()
            .find(|p| p.id == id)
            .unwrap()
            .instantiate(None)
            .unwrap();
        provider.enabled = true;
        provider.credential = crate::config::ProviderCredential {
            source: CredentialSource::Forward,
            ..crate::config::ProviderCredential::default()
        };
        let out =
            fetch_provider_account_usage(&provider, None, Some(("codex-token", "codex-account")))
                .await;
        assert_eq!(out.status, "unsupported", "{id}");
    }
    let mut codex = crate::provider_presets()
        .into_iter()
        .find(|p| p.id == "openai")
        .unwrap()
        .instantiate(None)
        .unwrap();
    assert!(source(&codex).is_some());
    codex.base_url = "https://proxy.example/backend-api/codex".into();
    assert!(source(&codex).is_none());
}
#[test]
fn copilot_usage_sends_one_sensitive_authorization_header() {
    let client = reqwest::Client::new();
    let copilot = authorized_get(
        &client,
        "https://api.github.com/copilot_internal/user",
        "github-copilot",
        "secret",
    )
    .build()
    .unwrap();
    let values: Vec<_> = copilot
        .headers()
        .get_all(reqwest::header::AUTHORIZATION)
        .iter()
        .collect();
    assert_eq!(values.len(), 1);
    assert_eq!(values[0], "token secret");
    assert!(values[0].is_sensitive());

    let other = authorized_get(&client, "https://api.deepseek.com", "deepseek", "secret")
        .build()
        .unwrap();
    let values: Vec<_> = other
        .headers()
        .get_all(reqwest::header::AUTHORIZATION)
        .iter()
        .collect();
    assert_eq!(values.len(), 1);
    assert_eq!(values[0], "Bearer secret");
}
#[test]
fn google_reports_two_pools_by_five_hour_and_weekly_window() {
    let out = parsed(
        "google-antigravity",
        json!({"groups":[
            {"displayName":"Gemini Models","buckets":[
                {"window":"5h","remainingFraction":0.25,"resetTime":"2026-11-04T01:00:00Z"},
                {"window":"weekly","remainingFraction":0.5,"resetTime":"2026-11-10T01:00:00Z"}
            ]},
            {"displayName":"Claude and GPT models","buckets":[
                {"window":"5h","remainingFraction":0.9},
                {"window":"weekly","remainingFraction":1}
            ]}
        ]}),
    );
    let labels: Vec<_> = out.windows.iter().map(|w| w.label.as_str()).collect();
    assert_eq!(
        labels,
        ["Gemini 5h", "Gemini 7d", "Claude/GPT 5h", "Claude/GPT 7d"]
    );
    assert_eq!(out.windows[0].used_percent, 75.0);
    assert_eq!(
        out.windows[0].resets_at.as_deref(),
        Some("2026-11-04T01:00:00Z")
    );
    assert_eq!(out.windows[1].used_percent, 50.0);
    assert!((out.windows[2].used_percent - 10.0).abs() < 1e-9);
    assert_eq!(out.windows[3].used_percent, 0.0);
    assert!(out.windows.iter().all(|w| w.model_id.is_none()));
    let pools: Vec<_> = out.windows.iter().map(|w| w.pool.as_deref()).collect();
    assert_eq!(
        pools,
        [
            Some("gemini"),
            Some("gemini"),
            Some("claude-gpt"),
            Some("claude-gpt")
        ]
    );
}
#[test]
fn google_model_list_records_pools_without_inventing_windows() {
    // Per-model quotaInfo has a reset time but no window length, so it
    // cannot say whether it is the 5-hour or the weekly limit. Only the
    // summary may produce bars.
    let mut out = parsed("google-antigravity", json!({"groups":[]}));
    apply_antigravity_models(
        &mut out,
        &json!({"models":{
            "gemini-3-pro-low":{"apiProvider":"API_PROVIDER_GOOGLE_GEMINI","quotaInfo":{"remainingFraction":0.9}},
            "gemini-3-pro-high":{"apiProvider":"API_PROVIDER_GOOGLE_GEMINI","quotaInfo":{"remainingFraction":0.4}},
            "claude-sonnet":{"apiProvider":"API_PROVIDER_ANTHROPIC_VERTEX","quotaInfo":{"remainingFraction":0.75}},
            "gpt-oss-120b-medium":{"apiProvider":"API_PROVIDER_OPENAI_VERTEX","quotaInfo":{"remainingFraction":0.25}}
        }}),
    );
    assert!(out.windows.is_empty());
    assert_eq!(out.status, "error");
    assert_eq!(
        out.model_pools.get("gemini-3-pro-high").map(String::as_str),
        Some("gemini")
    );
    assert_eq!(
        out.model_pools.get("claude-sonnet").map(String::as_str),
        Some("claude-gpt")
    );
    assert_eq!(
        out.model_pools
            .get("gpt-oss-120b-medium")
            .map(String::as_str),
        Some("claude-gpt")
    );
}
#[test]
fn google_model_list_does_not_guess_pools_without_provider_fields() {
    let mut out = parsed("google-antigravity", json!({"groups":[]}));
    apply_antigravity_models(
        &mut out,
        &json!({"models":{
            "gemini-3-pro-high":{"quotaInfo":{"remainingFraction":0.4}}
        }}),
    );
    assert!(out.windows.is_empty());
    assert_eq!(out.status, "error");
    assert!(out.model_pools.is_empty());
}
#[test]
fn google_model_list_never_replaces_summary_windows() {
    let models = json!({"models":{
        "gemini-3-pro-high":{"apiProvider":"API_PROVIDER_GOOGLE_GEMINI","quotaInfo":{"remainingFraction":0.4}},
        "claude-sonnet":{"apiProvider":"API_PROVIDER_ANTHROPIC_VERTEX","quotaInfo":{"remainingFraction":0.75}}
    }});
    let mut partial = parsed(
        "google-antigravity",
        json!({"groups":[{"displayName":"Gemini Models","buckets":[
            {"window":"5h","remainingFraction":0.5}
        ]}]}),
    );
    apply_antigravity_models(&mut partial, &models);
    assert_eq!(partial.status, "ok");
    assert_eq!(partial.windows.len(), 1);
    assert_eq!(partial.windows[0].label, "Gemini 5h");
    assert_eq!(partial.windows[0].used_percent, 50.0);
    // The map is still recorded so model rows can pick their pool.
    assert_eq!(partial.model_pools.len(), 2);
}
#[test]
fn google_keeps_the_pool_map_when_the_summary_fails() {
    let models = json!({"models":{
        "gemini-3.8-flash-high":{"apiProvider":"API_PROVIDER_GOOGLE_GEMINI"},
        "claude-opus-5-5-high":{"apiProvider":"API_PROVIDER_ANTHROPIC_VERTEX"}
    }});
    let mut failed = parsed("google-antigravity", json!({}));
    failed.error = Some("使用量API: HTTP 500".into());
    apply_antigravity_models(&mut failed, &models);
    // The map survives for the model rows, but the read is still a failure.
    assert_eq!(failed.status, "error");
    assert_eq!(failed.error.as_deref(), Some("使用量API: HTTP 500"));
    assert!(failed.windows.is_empty());
    assert_eq!(failed.model_pools.len(), 2);
}
#[test]
fn google_skips_buckets_without_a_usable_fraction() {
    let out = parsed(
        "google-antigravity",
        json!({"groups":[{"displayName":"Gemini Models","buckets":[
            {"window":"5h","remainingFraction":null},
            {"window":"weekly","remainingFraction":1.5},
            {"window":"","remainingFraction":0.5}
        ]}]}),
    );
    assert!(out.windows.is_empty());
}
#[test]
fn google_orders_five_hour_before_weekly_and_keeps_unknown_pools_apart() {
    let out = parsed(
        "google-antigravity",
        json!({"groups":[
            {"displayName":"Gemini Models","buckets":[
                {"window":"weekly","remainingFraction":0.5},
                {"window":"5h","remainingFraction":0.25},
                {"window":"5h","remainingFraction":0.9}
            ]},
            {"displayName":"Claude and GPT models","buckets":[
                {"window":"5h","remainingFraction":0.9}
            ]},
            {"displayName":"Future Models","buckets":[
                {"window":"5h","remainingFraction":0.5}
            ]}
        ]}),
    );
    let labels: Vec<_> = out.windows.iter().map(|w| w.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "Gemini 5h",
            "Gemini 7d",
            "Claude/GPT 5h",
            "Future Models 5h"
        ]
    );
    // The duplicate 5h bucket is ignored instead of overwriting the first.
    assert_eq!(out.windows[0].used_percent, 75.0);
    // An unknown pool keeps its own key instead of being merged with Claude/GPT.
    assert_eq!(out.windows[3].pool.as_deref(), Some("group:Future Models"));
}
#[test]
fn google_orders_pools_stably_regardless_of_upstream_group_order() {
    // Claude/GPT is returned first upstream; Gemini must still lead.
    let out = parsed(
        "google-antigravity",
        json!({"groups":[
            {"displayName":"Claude and GPT models","buckets":[
                {"window":"5h","remainingFraction":0.5}
            ]},
            {"displayName":"Gemini Models","buckets":[
                {"window":"5h","remainingFraction":0.5}
            ]}
        ]}),
    );
    let labels: Vec<_> = out.windows.iter().map(|w| w.label.as_str()).collect();
    assert_eq!(labels, ["Gemini 5h", "Claude/GPT 5h"]);
}
#[test]
fn google_identifies_pools_by_bucket_id_not_display_name() {
    // Upstream may reword the prose. `bucketId` is the stable machine name,
    // so a renamed group must still land in the right pool.
    let out = parsed(
        "google-antigravity",
        json!({"groups":[
            {"displayName":"Google models","buckets":[
                {"bucketId":"gemini-5h","window":"5h","remainingFraction":0.5}
            ]},
            {"displayName":"Third-party models","buckets":[
                {"bucketId":"3p-5h","window":"5h","remainingFraction":0.25}
            ]}
        ]}),
    );
    let pools: Vec<_> = out
        .windows
        .iter()
        .map(|w| (w.label.as_str(), w.pool.as_deref()))
        .collect();
    assert_eq!(
        pools,
        [
            ("Gemini 5h", Some("gemini")),
            ("Claude/GPT 5h", Some("claude-gpt"))
        ]
    );
}
#[test]
fn google_keeps_an_unknown_id_out_of_gemini_even_if_the_name_says_gemini() {
    // A third pool whose display name mentions Gemini must not merge into
    // the Gemini pool: the id is authoritative once it exists.
    let out = parsed(
        "google-antigravity",
        json!({"groups":[
            {"displayName":"Gemini Models","buckets":[
                {"bucketId":"gemini-5h","window":"5h","remainingFraction":0.5}
            ]},
            {"displayName":"Gemini preview","buckets":[
                {"bucketId":"preview-5h","window":"5h","remainingFraction":0.25}
            ]}
        ]}),
    );
    let pools: Vec<_> = out
        .windows
        .iter()
        .map(|w| (w.label.as_str(), w.pool.as_deref()))
        .collect();
    assert_eq!(
        pools,
        [
            ("Gemini 5h", Some("gemini")),
            ("Gemini preview 5h", Some("group:preview-5h"))
        ]
    );
}
#[test]
fn google_does_not_duplicate_bars_when_two_groups_share_a_pool() {
    let out = parsed(
        "google-antigravity",
        json!({"groups":[
            {"displayName":"Gemini Models","buckets":[
                {"window":"5h","remainingFraction":0.5}
            ]},
            {"displayName":"Gemini Experimental","buckets":[
                {"window":"5h","remainingFraction":0.9}
            ]}
        ]}),
    );
    let labels: Vec<_> = out.windows.iter().map(|w| w.label.as_str()).collect();
    assert_eq!(labels, ["Gemini 5h"]);
    // Two groups share one pool: keep the most-used value, never the
    // friendlier one (0.9 remaining would read as only 10% used).
    assert_eq!(out.windows[0].used_percent, 50.0);
}
#[test]
fn google_maps_models_to_pools_from_the_upstream_provider_fields() {
    let pools = antigravity_model_pools(&json!({"models":{
        "gemini-3-flash":{"apiProvider":"API_PROVIDER_GOOGLE_GEMINI"},
        "claude-opus-5-5-high":{"apiProvider":"API_PROVIDER_ANTHROPIC_VERTEX"},
        "gpt-oss-120b-medium":{"apiProvider":"API_PROVIDER_OPENAI_VERTEX"},
        "chat_20706":{"apiProvider":"API_PROVIDER_INTERNAL"},
        "mystery-model":{"apiProvider":"API_PROVIDER_SOMETHING_NEW"},
        "renamed-model":{"apiProvider":"API_PROVIDER_GOOGLE_GEMINI"},
        "legacy-gemini":{"apiProvider":null}
    }}));
    assert_eq!(
        pools.get("gemini-3-flash").map(String::as_str),
        Some("gemini")
    );
    assert_eq!(
        pools.get("renamed-model").map(String::as_str),
        Some("gemini")
    );
    assert_eq!(
        pools.get("claude-opus-5-5-high").map(String::as_str),
        Some("claude-gpt")
    );
    assert_eq!(
        pools.get("gpt-oss-120b-medium").map(String::as_str),
        Some("claude-gpt")
    );
    assert_eq!(pools.get("chat_20706").map(String::as_str), Some("gemini"));
    // Unknown or missing providers are not guessed into a pool.
    assert!(!pools.contains_key("mystery-model"));
    assert!(!pools.contains_key("legacy-gemini"));
}
#[test]
fn command_code_has_real_window_and_monthly_balance() {
    let out = parsed(
        "command-code",
        json!({"windowLimits":{"fiveHour":{"cap":100,"used":25,"resetAt":1800000000000_i64}},"credits":{"monthlyCredits":12.5}}),
    );
    assert_eq!(out.windows[0].used_percent, 25.0);
    assert!(out.balance.unwrap().contains("12.5000 USD"));
}
#[test]
fn balance_needs_valid_numeric_fields() {
    assert_eq!(
        parsed(
            "openrouter",
            json!({"data":{"total_credits":10,"total_usage":3}})
        )
        .balance
        .as_deref(),
        Some("7.0000 USD")
    );
    assert!(parsed(
        "deepseek",
        json!({"balance_infos":[{"currency":"USD","total_balance":null}]})
    )
    .balance
    .is_none());
}
async fn mock_usage(
    status: u16,
    body: &'static str,
) -> (
    String,
    Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    use axum::{
        http::{HeaderMap, StatusCode},
        routing::get,
        Router,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    let count = Arc::new(AtomicUsize::new(0));
    let hits = count.clone();
    let app = Router::new().route(
        "/usage",
        get(move |headers: HeaderMap| {
            let hits = hits.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer test-token");
                assert_eq!(headers["chatgpt-account-id"], "remote-account");
                hits.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::from_u16(status).unwrap(),
                    [("retry-after", "1200")],
                    body,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/usage", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (endpoint, count, server)
}
#[tokio::test]
async fn requests_are_authenticated_cached_and_isolated_per_account() {
    use std::sync::atomic::Ordering;
    let (endpoint, hits, server) = mock_usage(
        200,
        r#"{"rate_limit":{"primary_window":{"used_percent":42}}}"#,
    )
    .await;
    let provider = crate::provider_presets()
        .into_iter()
        .find(|p| p.id == "openai")
        .unwrap()
        .instantiate(None)
        .unwrap();
    for account in ["a", "a", "b"] {
        let out = fetch_with_token(
            &provider,
            empty(&provider, Some(account.into()), Some(&endpoint)),
            &endpoint,
            "test-token",
            Some("remote-account"),
        )
        .await;
        assert_eq!(out.status, "ok");
        assert_eq!(out.account_id.as_deref(), Some(account));
        assert_eq!(out.windows[0].used_percent, 42.0);
        assert!(!serde_json::to_string(&out).unwrap().contains("test-token"));
    }
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    server.abort();
}
#[tokio::test]
async fn rate_limit_is_backed_off_and_response_body_is_never_an_error_message() {
    use std::sync::atomic::Ordering;
    let (endpoint, hits, server) = mock_usage(429, "private-server-body").await;
    let provider = crate::provider_presets()
        .into_iter()
        .find(|p| p.id == "openai")
        .unwrap()
        .instantiate(None)
        .unwrap();
    for _ in 0..2 {
        let out = fetch_with_token(
            &provider,
            empty(&provider, None, Some(&endpoint)),
            &endpoint,
            "test-token",
            Some("remote-account"),
        )
        .await;
        assert_eq!(out.status, "error");
        assert!(out.windows.is_empty());
        assert!(out.observed_at_ms.is_none());
        assert!(!out.error.unwrap().contains("private-server-body"));
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    server.abort();
}
#[tokio::test]
async fn api_and_disabled_accounts_do_not_borrow_codex_subscription_login() {
    let mut provider = crate::provider_presets()
        .into_iter()
        .find(|p| p.id == "openai")
        .unwrap()
        .instantiate(None)
        .unwrap();
    provider.credential.source = CredentialSource::Environment;
    let out =
        fetch_provider_account_usage(&provider, None, Some(("test-token", "remote-account"))).await;
    assert_eq!(out.status, "unsupported");
    provider.enabled = false;
    let out = fetch_provider_account_usage(&provider, None, None).await;
    assert_eq!(out.status, "disabled");
}
