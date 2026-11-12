use super::*;
use crate::config::{ProviderDefinition, ProviderProtocol, RouteTarget};

#[test]
fn selected_api_and_subscription_targets_never_switch_to_the_other_connection() {
    let mut settings = GatewaySettings {
        default_provider: Some("openai-api".into()),
        providers: [
            ("openai", "gpt-test"),
            ("openai-api", "gpt-test"),
            ("anthropic", "claude-test"),
            ("anthropic-apikey", "claude-test"),
        ]
        .into_iter()
        .map(|(id, model)| ProviderDefinition {
            id: id.into(),
            models: vec![model.into()],
            enabled: true,
            ..ProviderDefinition::default()
        })
        .collect(),
        ..GatewaySettings::default()
    };
    for (slug, expected, model) in [
        ("gpt-test", "openai", "gpt-test"),
        ("openai-api/gpt-test", "openai-api", "gpt-test"),
        ("anthropic/claude-test", "anthropic", "claude-test"),
        (
            "anthropic-apikey/claude-test",
            "anthropic-apikey",
            "claude-test",
        ),
    ] {
        let (provider, upstream) = direct_target(&settings, slug).unwrap();
        assert_eq!(provider.id, expected);
        assert_eq!(upstream, model);
        settings
            .providers
            .iter_mut()
            .find(|provider| provider.id == expected)
            .unwrap()
            .enabled = false;
        assert!(direct_target(&settings, slug)
            .unwrap_err()
            .contains("disabled"));
        settings
            .providers
            .iter_mut()
            .find(|provider| provider.id == expected)
            .unwrap()
            .enabled = true;
    }
    // A slash inside an upstream model id still belongs to the default
    // provider when the prefix is not a configured connection.
    let (provider, model) = direct_target(&settings, "vendor/model").unwrap();
    assert_eq!(provider.id, "openai-api");
    assert_eq!(model, "vendor/model");
}

#[test]
fn quota_usage_exposes_only_fresh_observations_and_preserves_real_zero() {
    let mut runtime = RoutingRuntime::default();
    for (key, percent, age) in [
        ("one/model#a", 0, Duration::ZERO),
        ("one/model#b", 83, Duration::ZERO),
        ("two/model", 25, Duration::ZERO),
        ("one/stale#a", 99, QUOTA_SAMPLE_TTL + Duration::from_secs(1)),
    ] {
        runtime.quota_usage_percent.insert(key.into(), percent);
        runtime
            .quota_recorded_at
            .insert(key.into(), Instant::now() - age);
    }
    runtime
        .quota_usage_percent
        .insert("one/unknown#a".into(), 10);
    let rows = runtime.quota_usage();
    assert_eq!(rows.len(), 3);
    let zero = rows
        .iter()
        .find(|row| row.provider_id == "one" && row.account_id.as_deref() == Some("a"))
        .unwrap();
    assert_eq!(zero.used_percent, 0);
    assert_eq!(zero.model_id, "model");
    assert!(rows
        .iter()
        .any(|row| row.provider_id == "two" && row.account_id.is_none()));
    assert!(!rows
        .iter()
        .any(|row| row.model_id == "stale" || row.model_id == "unknown"));
}

fn settings() -> GatewaySettings {
    let providers = ["one", "two"]
        .into_iter()
        .map(|id| ProviderDefinition {
            id: id.into(),
            name: id.into(),
            base_url: format!("https://{id}.example/v1"),
            protocol: ProviderProtocol::Responses,
            default_model: Some("model".into()),
            ..ProviderDefinition::default()
        })
        .collect();
    GatewaySettings {
        default_provider: Some("one".into()),
        providers,
        routes: vec![RouteDefinition {
            id: "reliable".into(),
            name: "Reliable".into(),
            strategy: RouteStrategy::Failover,
            targets: vec![
                RouteTarget {
                    model: "one/model".into(),
                    weight: 1,
                },
                RouteTarget {
                    model: "two/model".into(),
                    weight: 1,
                },
            ],
            ..RouteDefinition::default()
        }],
        ..GatewaySettings::default()
    }
}

fn account(id: &str, priority: i16) -> crate::config::AccountReference {
    crate::config::AccountReference {
        id: id.into(),
        provider_id: "one".into(),
        label: id.into(),
        priority,
        ..crate::config::AccountReference::default()
    }
}

#[test]
fn compaction_quota_outlives_probe_cooldown_and_clears_on_success_or_expiry() {
    let settings = settings();
    let mut runtime = RoutingRuntime::default();
    let candidate = runtime.candidates(&settings, "one/model").unwrap()[0].clone();
    runtime.record_quota_exhausted(&candidate, Some(Duration::from_secs(3600)));
    runtime
        .hard_cooldowns
        .insert(failure_key(&candidate), Instant::now());
    let fresh = runtime.candidates(&settings, "one/model").unwrap()[0].clone();
    assert!(runtime.compaction_quota_exhausted(&fresh));
    runtime.record_success(&fresh, None);
    assert!(!runtime.compaction_quota_exhausted(&fresh));
    runtime.record_quota_exhausted(&fresh, Some(Duration::ZERO));
    assert!(!runtime.compaction_quota_exhausted(&fresh));
    assert!(!RoutingRuntime::default().compaction_quota_exhausted(&fresh));
}

#[test]
fn compaction_quota_groups_only_antigravity_third_party_models_per_account() {
    let mut runtime = RoutingRuntime::default();
    let mut claude = runtime.candidates(&settings(), "one/model").unwrap()[0].clone();
    claude.provider.id = "google-antigravity".into();
    claude.upstream_model = "claude-opus-5-5-high".into();
    claude.target_key = "google-antigravity/claude-opus-5-5-high".into();
    runtime.record_quota_exhausted(&claude, Some(Duration::from_secs(3600)));
    let mut other = claude.clone();
    other.upstream_model = "claude-sonnet-5-5-high".into();
    other.target_key = "google-antigravity/claude-sonnet-5-5-high".into();
    assert!(runtime.compaction_quota_exhausted(&other));
    other.upstream_model = "gemini-3.8-flash".into();
    assert!(!runtime.compaction_quota_exhausted(&other));
    other.upstream_model = "gpt-oss-120b-medium".into();
    assert!(runtime.compaction_quota_exhausted(&other));
    other.account_id = Some("another-account".into());
    assert!(!runtime.compaction_quota_exhausted(&other));
}

#[test]
fn expands_a_failover_route() {
    let mut runtime = RoutingRuntime::default();
    let candidates = runtime.candidates(&settings(), "reliable").expect("route");
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].target_key, "one/model");
    assert_eq!(candidates[1].target_key, "two/model");
}

#[test]
fn dry_run_keeps_policy_and_account_exclusions_visible() {
    let mut settings = settings();
    settings.routes[0].strategy = RouteStrategy::Policy;
    settings.routes[0].policy.required_capabilities = vec!["tools".into()];
    settings.providers[1].capabilities.tools = false;
    settings
        .account_pool
        .accounts
        .push(crate::config::AccountReference {
            id: "paused".into(),
            provider_id: "two".into(),
            label: "Paused".into(),
            paused: true,
            ..crate::config::AccountReference::default()
        });
    let report = RoutingRuntime::default().dry_run(&settings, "reliable", false);
    assert_eq!(report.candidates.len(), 2);
    assert!(report
        .candidates
        .iter()
        .any(|row| row.eligible && row.target == "one/model"));
    let excluded = report
        .candidates
        .iter()
        .find(|row| row.target == "two/model")
        .unwrap();
    assert!(excluded
        .reasons
        .iter()
        .any(|reason| reason == "account paused"));
    assert!(excluded
        .reasons
        .iter()
        .any(|reason| reason.contains("missing capability tools")));
}

#[test]
fn routing_never_treats_tool_subcapabilities_as_available_without_tools() {
    let capabilities = ProviderCapabilities {
        tools: false,
        parallel_tools: true,
        custom_tools: true,
        tool_search: true,
        mcp_namespaces: true,
        ..ProviderCapabilities::default()
    };
    for name in [
        "parallelTools",
        "customTools",
        "toolSearch",
        "mcpNamespaces",
    ] {
        assert!(!capability_enabled(&capabilities, name), "{name}");
    }
}

#[test]
fn dry_run_does_not_advance_or_clean_live_runtime_state() {
    let mut settings = settings();
    settings.routes[0].strategy = RouteStrategy::WeightedRoundRobin;
    settings.routes[0].sticky_requests = 1;
    settings.account_pool.accounts = vec![account("a", 0), account("b", 0)];
    settings.account_pool.strategy = AccountPoolStrategy::RoundRobin;
    let mut runtime = RoutingRuntime::default();
    runtime.route_calls.insert("reliable:normal".into(), 3);
    runtime
        .account_calls
        .insert("account:one:priority:0".into(), 5);
    runtime.failures.insert(
        "two/model".into(),
        FailureState {
            hard_key: "two/model".into(),
            consecutive: 2,
            retry_after: Some(Instant::now() - Duration::from_secs(1)),
            ..FailureState::default()
        },
    );
    let before_route_calls = runtime.route_calls.clone();
    let before_account_calls = runtime.account_calls.clone();
    let before_failure = runtime.failures["two/model"].clone();

    let _ = runtime.dry_run(&settings, "reliable", false);

    assert_eq!(runtime.route_calls, before_route_calls);
    assert_eq!(runtime.account_calls, before_account_calls);
    assert_eq!(
        runtime.failures["two/model"].consecutive,
        before_failure.consecutive
    );
    assert_eq!(
        runtime.failures["two/model"].retry_after,
        before_failure.retry_after
    );
}

#[test]
fn dry_run_selection_matches_the_next_real_selection_for_every_strategy() {
    for strategy in [
        RouteStrategy::Failover,
        RouteStrategy::WeightedRoundRobin,
        RouteStrategy::LeastUsage,
        RouteStrategy::Policy,
    ] {
        let mut settings = settings();
        settings.routes[0].strategy = strategy;
        settings.routes[0].sticky_requests = 1;
        settings.routes[0].targets[0].weight = 1;
        settings.routes[0].targets[1].weight = 2;
        let mut runtime = RoutingRuntime::default();
        match strategy {
            RouteStrategy::WeightedRoundRobin => {
                runtime.route_calls.insert("reliable:normal".into(), 1);
            }
            RouteStrategy::LeastUsage => {
                runtime.target_requests.insert("one/model".into(), 10);
            }
            RouteStrategy::Policy => {
                settings.routes[0].policy.quota_weight = 10;
                runtime.quota_usage_percent.insert("one/model".into(), 90);
            }
            RouteStrategy::Failover => {}
        }

        let report = runtime.dry_run(&settings, "reliable", false);
        let next = runtime
            .candidates(&settings, "reliable")
            .expect("next route candidate");

        assert_eq!(
            report.selected.as_deref(),
            next.first().map(|candidate| candidate.target_key.as_str()),
            "{strategy:?}"
        );
        assert_eq!(
            report
                .candidates
                .iter()
                .find(|candidate| candidate.eligible)
                .map(|candidate| candidate.rank),
            Some(
                settings.routes[0]
                    .targets
                    .iter()
                    .position(|target| target.model == next[0].target_key)
                    .expect("configured target")
                    + 1
            ),
            "{strategy:?}"
        );
    }
}

#[test]
fn policy_health_weight_prefers_the_healthier_candidate_and_reports_signal() {
    let mut settings = settings();
    settings.routes[0].strategy = RouteStrategy::Policy;
    settings.routes[0].policy.health_weight = 100;
    settings.routes[0].policy.cost_weight = 0;
    settings.routes[0].policy.quota_weight = 0;
    settings.routes[0].policy.context_weight = 0;
    let mut runtime = RoutingRuntime::default();
    runtime.failures.insert(
        "one/model".into(),
        FailureState {
            hard_key: "one/model".into(),
            consecutive: 2,
            retry_after: None,
            ..FailureState::default()
        },
    );

    let report = runtime.dry_run(&settings, "reliable", false);
    let actual = runtime
        .candidates(&settings, "reliable")
        .expect("policy route");

    assert_eq!(actual[0].target_key, "two/model");
    assert_eq!(report.selected.as_deref(), Some("two/model"));
    let degraded = report
        .candidates
        .iter()
        .find(|candidate| candidate.target == "one/model")
        .expect("degraded candidate");
    assert!(degraded.health_percent < 100);
    assert!(degraded
        .reasons
        .iter()
        .any(|reason| reason.starts_with("health:")));
}

#[test]
fn subagent_uses_requested_model_without_any_roster() {
    let mut settings = settings();
    settings.agents.multi_agent_v2 = true;
    let mut runtime = RoutingRuntime::default();
    for model in ["one/new-model", "new-model", "reliable"] {
        let normal = runtime
            .clone()
            .candidates_for_request(&settings, model, false, None)
            .expect("normal model");
        let subagent = runtime
            .candidates_for_request(&settings, model, true, None)
            .expect("subagent without roster");
        assert_eq!(
            subagent
                .iter()
                .map(|c| (&c.target_key, &c.exposed_model))
                .collect::<Vec<_>>(),
            normal
                .iter()
                .map(|c| (&c.target_key, &c.exposed_model))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn subagent_requested_model_precedes_legacy_rosters_and_fallbacks() {
    let mut settings = settings();
    settings.agents.multi_agent_v2 = true;
    settings.agents.subagent_models = vec!["one/obsolete".into(), "two/selected".into()];
    settings
        .agents
        .subagent_fallback_by_model
        .insert("two/selected".into(), vec!["one/specific".into()]);
    settings.agents.subagent_fallback = vec!["one/common".into(), "one/obsolete".into()];
    let mut runtime = RoutingRuntime::default();
    let candidates = runtime
        .candidates_for_request(&settings, "two/selected", true, Some("child"))
        .expect("requested model with optional fallbacks");
    assert_eq!(
        candidates
            .iter()
            .map(|c| c.target_key.as_str())
            .collect::<Vec<_>>(),
        vec!["two/selected", "one/obsolete", "one/specific", "one/common"]
    );
    assert!(candidates.iter().all(|c| c.exposed_model == "two/selected"));
    assert!(candidates
        .iter()
        .all(|c| c.session_scope.as_deref() == Some("child")));
    let main = runtime
        .candidates_for_request(&settings, "two/selected", false, None)
        .expect("main model unaffected");
    assert_eq!(main.len(), 1);
    assert_eq!(main[0].target_key, "two/selected");
}

#[test]
fn stale_subagent_roster_does_not_block_a_valid_requested_model() {
    let mut settings = settings();
    settings.default_provider = None;
    settings.agents.multi_agent_v2 = true;
    settings.agents.subagent_models = vec!["removed/obsolete".into()];
    let mut runtime = RoutingRuntime::default();
    let candidates = runtime
        .candidates_for_request(&settings, "two/selected", true, None)
        .expect("valid requested model despite stale roster");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].target_key, "two/selected");
}

#[test]
fn model_specific_subagent_fallback_works_without_a_primary_roster() {
    let mut settings = settings();
    settings.default_provider = None;
    settings.agents.multi_agent_v2 = true;
    settings
        .agents
        .subagent_fallback_by_model
        .insert("parent/model".into(), vec!["two/model".into()]);
    let mut runtime = RoutingRuntime::default();

    let candidates = runtime
        .candidates_for_request(&settings, "parent/model", true, None)
        .expect("model-specific subagent fallback");

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].target_key, "two/model");
    assert_eq!(candidates[0].exposed_model, "parent/model");
}

#[test]
fn subagent_rosters_follow_requested_then_model_specific_then_common_order() {
    let mut settings = settings();
    settings.agents.multi_agent_v2 = true;
    settings.agents.subagent_models = vec!["one/primary".into()];
    settings
        .agents
        .subagent_fallback_by_model
        .insert("parent/model".into(), vec!["two/specific".into()]);
    settings.agents.subagent_fallback = vec!["one/common".into()];
    let mut runtime = RoutingRuntime::default();

    let candidates = runtime
        .candidates_for_request(&settings, "parent/model", true, None)
        .expect("subagent rosters");
    let targets = candidates
        .iter()
        .map(|candidate| candidate.target_key.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        targets,
        vec![
            "one/parent/model",
            "one/primary",
            "two/specific",
            "one/common"
        ]
    );
}

#[test]
fn subagent_fallback_respects_cooldown_and_compaction_materialization() {
    let mut settings = settings();
    settings.agents.multi_agent_v2 = true;
    settings.agents.subagent_fallback = vec!["two/model".into()];
    let mut runtime = RoutingRuntime::default();
    let primary = runtime.candidates(&settings, "one/model").expect("primary")[0].clone();
    runtime.record_quota_exhausted(&primary, Some(Duration::from_secs(60)));
    let ordinary = runtime
        .candidates_for_request(&settings, "one/model", true, None)
        .expect("healthy fallback");
    assert_eq!(ordinary.len(), 1);
    assert_eq!(ordinary[0].target_key, "two/model");
    let compact = runtime
        .candidates_for_request_including_cooldown(&settings, "one/model", true, None)
        .expect("compaction may inspect cooled primary");
    assert_eq!(compact[0].target_key, "one/model");
    runtime
        .begin_attempt(&compact[0])
        .expect_err("cooled primary cannot send");
    runtime.record_quota_exhausted(&ordinary[0], Some(Duration::from_secs(60)));
    runtime
        .candidates_for_request(&settings, "one/model", true, None)
        .expect_err("all candidates cooling down must not fail open");
}

#[test]
fn account_strategy_never_promotes_a_lower_priority_tier() {
    let mut settings = settings();
    settings.account_pool.accounts = vec![
        account("high-a", 100),
        account("low", 0),
        account("high-b", 100),
    ];
    settings.account_pool.strategy = AccountPoolStrategy::Quota;
    let mut runtime = RoutingRuntime::default();
    runtime
        .quota_usage_percent
        .insert("one/model#high-a".into(), 50);
    runtime
        .quota_usage_percent
        .insert("one/model#high-b".into(), 10);
    let candidates = runtime
        .candidates(&settings, "one/model")
        .expect("accounts");
    assert_eq!(
        candidates
            .iter()
            .map(|item| item.account_id.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("high-b"), Some("high-a"), Some("low")]
    );

    settings.account_pool.strategy = AccountPoolStrategy::RoundRobin;
    let first = runtime
        .candidates(&settings, "one/model")
        .expect("round robin");
    let second = runtime
        .candidates(&settings, "one/model")
        .expect("round robin");
    assert_eq!(
        first.last().and_then(|item| item.account_id.as_deref()),
        Some("low")
    );
    assert_eq!(
        second.last().and_then(|item| item.account_id.as_deref()),
        Some("low")
    );

    settings
        .account_pool
        .active_accounts
        .insert("one".into(), "low".into());
    let active_low = runtime
        .candidates(&settings, "one/model")
        .expect("active low");
    assert_ne!(active_low[0].account_id.as_deref(), Some("low"));
}

#[test]
fn policy_scoring_cannot_promote_a_lower_account_priority_tier() {
    let mut settings = settings();
    settings.routes[0].strategy = RouteStrategy::Policy;
    settings.routes[0].targets.truncate(1);
    settings.account_pool.accounts = vec![account("high", 100), account("low", 0)];
    settings.account_pool.auto_switch_threshold_percent = 0;
    let mut runtime = RoutingRuntime::default();
    runtime
        .quota_usage_percent
        .insert("one/model#high".into(), 99);
    runtime
        .quota_usage_percent
        .insert("one/model#low".into(), 0);

    let candidates = runtime
        .candidates(&settings, "reliable")
        .expect("policy route");
    assert_eq!(candidates[0].account_id.as_deref(), Some("high"));

    settings.account_pool.accounts[1].pinned = true;
    let pinned = runtime
        .candidates(&settings, "reliable")
        .expect("pinned route");
    assert_eq!(pinned[0].account_id.as_deref(), Some("low"));
}

#[test]
fn policy_score_ranks_providers_before_account_priority_and_dry_run_matches() {
    let mut settings = settings();
    settings.routes[0].strategy = RouteStrategy::Policy;
    settings.routes[0].policy.health_weight = 100;
    settings.routes[0].policy.cost_weight = 0;
    settings.routes[0].policy.quota_weight = 0;
    settings.routes[0].policy.context_weight = 0;
    let mut high = account("high", 100);
    high.provider_id = "one".into();
    let mut low = account("low", 0);
    low.provider_id = "two".into();
    settings.account_pool.accounts = vec![high, low];
    let mut runtime = RoutingRuntime::default();
    runtime.failures.insert(
        "one/model#high".into(),
        FailureState {
            hard_key: "one/model#high".into(),
            consecutive: 2,
            retry_after: None,
            ..FailureState::default()
        },
    );

    let report = runtime.dry_run(&settings, "reliable", false);
    let candidates = runtime
        .candidates(&settings, "reliable")
        .expect("policy route");

    assert_eq!(candidates[0].target_key, "two/model");
    assert_eq!(candidates[0].account_id.as_deref(), Some("low"));
    assert_eq!(report.selected.as_deref(), Some("two/model"));
}

#[test]
fn pin_overrides_priority_until_unavailable_then_returns_to_highest_healthy_tier() {
    let mut settings = settings();
    let mut low = account("low", 0);
    low.pinned = true;
    settings.account_pool.accounts = vec![account("high", 100), low];
    settings.account_pool.auto_switch_threshold_percent = 80;
    let mut runtime = RoutingRuntime::default();
    let pinned = runtime.candidates(&settings, "one/model").expect("pinned");
    assert_eq!(pinned[0].account_id.as_deref(), Some("low"));

    runtime
        .quota_usage_percent
        .insert("one/model#low".into(), 100);
    let fallback = runtime
        .candidates(&settings, "one/model")
        .expect("fallback");
    assert_eq!(fallback[0].account_id.as_deref(), Some("high"));
}

#[test]
fn all_accounts_above_threshold_probe_one_and_keep_the_main_account_sticky() {
    let mut settings = settings();
    settings.account_pool.accounts = vec![account("main", 0), account("backup", 0)];
    settings.account_pool.auto_switch_threshold_percent = 80;
    let mut runtime = RoutingRuntime::default();
    runtime
        .quota_usage_percent
        .insert("one/model#main".into(), 95);
    runtime
        .quota_usage_percent
        .insert("one/model#backup".into(), 85);

    let candidates = runtime
        .candidates(&settings, "one/model")
        .expect("an over-threshold pool must still probe one account");
    // Both stay candidates so a failing probe can fail over in-request.
    assert_eq!(candidates.len(), 2);
    // A 10% sample difference must not move the conversation to another
    // account: that would drop the provider's prompt cache.
    assert_eq!(candidates[0].account_id.as_deref(), Some("main"));

    runtime
        .quota_usage_percent
        .insert("one/model#main".into(), 100);
    let candidates = runtime
        .candidates(&settings, "one/model")
        .expect("an exhausted main account hands over to the backup");
    assert_eq!(candidates[0].account_id.as_deref(), Some("backup"));
}

#[test]
fn a_cooling_account_does_not_reject_a_request_another_over_threshold_account_can_serve() {
    let mut settings = settings();
    settings.account_pool.accounts = vec![account("main", 0), account("backup", 0)];
    settings.account_pool.auto_switch_threshold_percent = 80;
    let mut runtime = RoutingRuntime::default();
    runtime
        .quota_usage_percent
        .insert("one/model#backup".into(), 85);
    runtime.hard_cooldowns.insert(
        "one/model#main".into(),
        Instant::now() + Duration::from_secs(5),
    );
    let candidates = runtime
        .candidates(&settings, "one/model")
        .expect("the backup account must keep the task running");
    assert_eq!(candidates[0].account_id.as_deref(), Some("backup"));
}

#[test]
fn stale_quota_samples_do_not_exclude_an_account_forever() {
    let mut settings = settings();
    settings.account_pool.accounts = vec![account("main", 0), account("backup", 0)];
    settings.account_pool.auto_switch_threshold_percent = 80;
    let mut runtime = RoutingRuntime::default();
    runtime
        .quota_usage_percent
        .insert("one/model#main".into(), 100);
    runtime.quota_recorded_at.insert(
        "one/model#main".into(),
        Instant::now()
            .checked_sub(QUOTA_SAMPLE_TTL + Duration::from_secs(1))
            .expect("monotonic clock supports the subtraction"),
    );
    runtime
        .quota_usage_percent
        .insert("one/model#backup".into(), 10);

    assert_eq!(runtime.effective_quota_usage("one/model#main"), 0);
    let candidates = runtime
        .candidates(&settings, "one/model")
        .expect("stale sample must be probeable");
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.account_id.as_deref() == Some("main")),
        "the main account must be routable again once its sample expires"
    );
}

#[test]
fn unavailable_high_priority_accounts_fall_through_to_the_next_tier() {
    let mut settings = settings();
    let mut paused = account("paused", 100);
    paused.paused = true;
    settings.account_pool.accounts = vec![
        paused,
        account("cooling", 100),
        account("quota", 100),
        account("low", 0),
    ];
    settings.account_pool.auto_switch_threshold_percent = 80;
    let mut runtime = RoutingRuntime::default();
    runtime.failures.insert(
        "one/model#cooling".into(),
        FailureState {
            hard_key: "one/model#cooling".into(),
            consecutive: 0,
            retry_after: Some(Instant::now() + Duration::from_secs(60)),
            ..FailureState::default()
        },
    );
    runtime
        .quota_usage_percent
        .insert("one/model#quota".into(), 100);
    let candidates = runtime
        .candidates(&settings, "one/model")
        .expect("lower tier");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].account_id.as_deref(), Some("low"));
}

#[test]
fn transient_429_quota_expires_without_overwriting_persisted_usage() {
    let mut settings = settings();
    settings.account_pool.accounts = vec![account("only", 100)];
    settings.account_pool.auto_switch_threshold_percent = 80;
    let mut runtime = RoutingRuntime::default();
    let candidate = runtime
        .candidates(&settings, "one/model")
        .expect("single account")[0]
        .clone();
    runtime.record_success(&candidate, Some(25));
    runtime.record_quota_exhausted(&candidate, Some(Duration::ZERO));

    let recovered = runtime
        .candidates(&settings, "one/model")
        .expect("expired transient 429 must recover");
    assert_eq!(recovered[0].account_id.as_deref(), Some("only"));
    assert_eq!(
        runtime.effective_quota_usage("one/model#only"),
        25,
        "the real provider quota survives the transient exclusion"
    );

    let recovered_candidate = recovered[0].clone();
    runtime.record_success(&recovered_candidate, Some(100));
    runtime.record_quota_exhausted(&recovered_candidate, Some(Duration::ZERO));
    // A pool with no account below the threshold still probes the least
    // used one instead of failing the request outright.
    let probed = runtime
        .candidates(&settings, "one/model")
        .expect("over-threshold pool probes its least-used account");
    assert_eq!(probed[0].account_id.as_deref(), Some("only"));
    assert_eq!(runtime.effective_quota_usage("one/model#only"), 100);
}

#[test]
fn transient_429_fails_over_then_readmits_the_recovered_account() {
    let mut settings = settings();
    settings.account_pool.accounts = vec![account("primary", 100), account("backup", 100)];
    settings.account_pool.auto_switch_threshold_percent = 80;
    let mut runtime = RoutingRuntime::default();
    let primary = runtime
        .candidates(&settings, "one/model")
        .expect("account pool")[0]
        .clone();
    runtime.record_quota_exhausted(&primary, Some(Duration::from_secs(60)));

    let failover = runtime.candidates(&settings, "one/model").expect("backup");
    assert_eq!(failover.len(), 1);
    assert_eq!(failover[0].account_id.as_deref(), Some("backup"));

    let key = failure_key(&primary);
    runtime
        .hard_cooldowns
        .insert(key.clone(), Instant::now() - Duration::from_secs(1));
    let recovered = runtime
        .candidates(&settings, "one/model")
        .expect("recovered pool");
    assert!(recovered
        .iter()
        .any(|candidate| candidate.account_id.as_deref() == Some("primary")));
    assert!(!runtime.transient_quota_exhausted.contains(&key));
}

#[test]
fn hard_429_cooldown_survives_stale_success_and_failure_and_keeps_max_deadline() {
    let mut settings = settings();
    settings.account_pool.accounts = vec![account("only", 100)];
    let mut runtime = RoutingRuntime::default();
    let old_attempt = runtime
        .candidates(&settings, "one/model")
        .expect("old attempt")[0]
        .clone();
    let key = failure_key(&old_attempt);

    runtime.record_quota_exhausted(&old_attempt, Some(Duration::from_secs(7_200)));
    let first_deadline = runtime.hard_cooldowns[&key];
    let remaining = first_deadline.saturating_duration_since(Instant::now());
    assert!(remaining > Duration::from_secs(4));
    assert!(remaining <= MAX_HARD_RETRY_AFTER_COOLDOWN);

    runtime.record_success(&old_attempt, Some(0));
    runtime.record_failure(&old_attempt);
    runtime.record_quota_exhausted(&old_attempt, Some(Duration::from_secs(60)));

    assert_eq!(runtime.hard_cooldowns[&key], first_deadline);
    assert!(runtime.candidates(&settings, "one/model").is_err());
    assert!(runtime.transient_quota_exhausted.contains(&key));
    assert!(!runtime.failures.contains_key(&key));
}

#[test]
fn delayed_real_429_is_hard_evidence_after_the_old_generation_expires() {
    let mut settings = settings();
    settings.account_pool.accounts = vec![account("only", 100)];
    let mut runtime = RoutingRuntime::default();
    let old_attempt = runtime
        .candidates(&settings, "one/model")
        .expect("old attempt")[0]
        .clone();
    let key = failure_key(&old_attempt);
    runtime.record_quota_exhausted(&old_attempt, Some(Duration::from_secs(1)));
    runtime
        .hard_cooldowns
        .insert(key.clone(), Instant::now() - Duration::from_secs(1));
    let recovered = runtime
        .candidates(&settings, "one/model")
        .expect("recovered generation")[0]
        .clone();
    runtime.record_success(&recovered, None);

    runtime.record_quota_exhausted(&old_attempt, Some(Duration::from_secs(7_200)));

    let remaining = runtime.hard_cooldowns[&key].saturating_duration_since(Instant::now());
    assert!(remaining > Duration::from_secs(4));
    assert!(remaining <= MAX_HARD_RETRY_AFTER_COOLDOWN);
    assert!(runtime.transient_quota_exhausted.contains(&key));
    assert!(runtime.candidates(&settings, "one/model").is_err());
    assert_ne!(
        runtime.routing_generation(&key),
        old_attempt.routing_generation,
        "late hard evidence advances the generation fence"
    );
}

#[test]
fn replaced_runtime_epoch_rejects_late_attempt_outcomes_for_the_same_key() {
    let mut settings = settings();
    settings.account_pool.accounts = vec![account("only", 100)];
    let mut old_runtime = RoutingRuntime::default();
    let old_candidate = old_runtime
        .candidates(&settings, "one/model")
        .expect("old runtime candidate")[0]
        .clone();
    old_runtime
        .begin_attempt(&old_candidate)
        .expect("old runtime attempt");

    let mut replacement = RoutingRuntime::default();
    let current_candidate = replacement
        .candidates(&settings, "one/model")
        .expect("replacement runtime candidate")[0]
        .clone();
    replacement
        .begin_attempt(&current_candidate)
        .expect("replacement runtime attempt");
    let key = failure_key(&current_candidate);
    let generation = replacement.routing_generation(&key);

    assert_ne!(old_candidate.routing_epoch, current_candidate.routing_epoch);
    assert!(replacement.begin_attempt(&old_candidate).is_err());
    replacement.end_attempt(&old_candidate);
    replacement.record_success(&old_candidate, Some(100));
    replacement.record_failure(&old_candidate);
    replacement.record_quota_exhausted(&old_candidate, Some(Duration::from_secs(7_200)));

    assert_eq!(replacement.active_attempts(&current_candidate), 1);
    assert_eq!(replacement.routing_generation(&key), generation);
    assert!(!replacement.hard_cooldowns.contains_key(&key));
    assert!(!replacement.failures.contains_key(&key));
    assert!(!replacement.quota_usage_percent.contains_key(&key));
    replacement.end_attempt(&current_candidate);
}

#[test]
fn routing_key_capacity_fails_closed_before_a_new_direct_target_is_returned() {
    let mut settings = settings();
    settings.routes.clear();
    settings.account_pool.accounts.clear();
    let mut runtime = RoutingRuntime::default();
    for index in 0..MAX_ROUTING_RUNTIME_KEYS {
        let model = format!("one/arbitrary-{index}");
        let candidates = runtime
            .candidates(&settings, &model)
            .expect("routing key within capacity");
        assert_eq!(candidates.len(), 1);
        runtime
            .begin_attempt(&candidates[0])
            .expect("live attempt reserves routing key capacity");
    }
    let error = runtime
        .candidates(&settings, "one/arbitrary-overflow")
        .expect_err("new routing key must fail closed before upstream send");
    assert!(error.contains("routing runtime key capacity reached before sending"));
    assert_eq!(runtime.routing_generations.len(), MAX_ROUTING_RUNTIME_KEYS);
}

#[test]
fn expired_hard_key_reclaims_all_linked_runtime_maps_and_capacity() {
    let mut runtime = RoutingRuntime::default();
    for index in 0..MAX_ROUTING_RUNTIME_KEYS {
        let key = format!("provider/model-{index}");
        runtime.routing_generations.insert(
            key.clone(),
            RoutingGenerationState {
                generation: 7,
                active_leases: 0,
            },
        );
        runtime.transient_quota_exhausted.insert(key.clone());
        runtime
            .hard_cooldowns
            .insert(key, Instant::now() - Duration::from_secs(1));
    }
    assert_eq!(runtime.reserve_routing_key("provider/fresh"), Ok(0));
    assert!(runtime.routing_generations.is_empty());
    assert!(runtime.transient_quota_exhausted.is_empty());
    assert!(runtime.hard_cooldowns.is_empty());
}

#[test]
fn active_hard_cooldowns_are_never_evicted_to_admit_a_new_key() {
    let mut runtime = RoutingRuntime::default();
    let deadline = Instant::now() + Duration::from_secs(7_200);
    for index in 0..MAX_ROUTING_RUNTIME_KEYS {
        let key = format!("provider/model-{index}");
        runtime.routing_generations.insert(
            key.clone(),
            RoutingGenerationState {
                generation: 1,
                active_leases: 0,
            },
        );
        runtime.transient_quota_exhausted.insert(key.clone());
        runtime.hard_cooldowns.insert(key, deadline);
    }
    let first = "provider/model-0";
    let error = runtime
        .reserve_routing_key("provider/overflow")
        .expect_err("active hard cooldown capacity must fail closed");
    assert!(error.contains("capacity reached before sending"));
    assert_eq!(runtime.routing_generations.len(), MAX_ROUTING_RUNTIME_KEYS);
    assert_eq!(runtime.hard_cooldowns.len(), MAX_ROUTING_RUNTIME_KEYS);
    assert_eq!(
        runtime.transient_quota_exhausted.len(),
        MAX_ROUTING_RUNTIME_KEYS
    );
    assert_eq!(runtime.hard_cooldowns.get(first), Some(&deadline));
}

#[test]
fn inactive_soft_failures_and_generations_are_reclaimed_before_capacity_rejection() {
    let mut settings = settings();
    settings.routes.clear();
    settings.account_pool.accounts.clear();
    let mut runtime = RoutingRuntime::default();
    for index in 0..MAX_ROUTING_RUNTIME_KEYS {
        let candidate = runtime
            .candidates(&settings, &format!("one/failed-{index}"))
            .expect("soft failure candidate")[0]
            .clone();
        runtime
            .begin_attempt(&candidate)
            .expect("soft failure lease");
        runtime.record_failure(&candidate);
        runtime.end_attempt(&candidate);
    }
    assert!(runtime.candidates(&settings, "one/overflow").is_err());
    for failure in runtime.failures.values_mut() {
        failure.last_activity = Instant::now() - SOFT_FAILURE_RETENTION - Duration::from_secs(1);
    }
    assert!(runtime.candidates(&settings, "one/fresh").is_ok());
    assert!(runtime.routing_generations.is_empty());
    assert!(runtime.failures.is_empty());
}

#[test]
fn live_attempt_lease_survives_cleanup_beyond_the_old_five_minute_timeout() {
    let mut settings = settings();
    settings.routes.clear();
    settings.account_pool.accounts.clear();
    let mut runtime = RoutingRuntime::default();
    let candidate = runtime
        .candidates(&settings, "one/long-running")
        .expect("long running candidate")[0]
        .clone();
    runtime
        .begin_attempt(&candidate)
        .expect("live attempt lease");
    let key = failure_key(&candidate);

    runtime.cleanup_inactive_routing_keys(Instant::now() + Duration::from_secs(6 * 60));

    assert_eq!(runtime.routing_generations[&key].active_leases, 1);
    runtime.record_quota_exhausted(&candidate, Some(Duration::from_secs(7_200)));
    assert!(runtime.hard_cooldowns.contains_key(&key));
    assert_ne!(
        runtime.routing_generation(&key),
        candidate.routing_generation
    );
}

#[test]
fn direct_provider_and_all_cooled_route_targets_do_not_fail_open() {
    let mut direct_settings = settings();
    direct_settings.routes.clear();
    let mut runtime = RoutingRuntime::default();
    let direct = runtime
        .candidates(&direct_settings, "one/model")
        .expect("direct provider")[0]
        .clone();
    runtime.record_quota_exhausted(&direct, Some(Duration::from_secs(7_200)));
    let direct_error = runtime
        .candidates(&direct_settings, "one/model")
        .expect_err("direct cooled target must be excluded");
    assert!(direct_error.contains("cooling down"));

    let route_settings = settings();
    let mut route_runtime = RoutingRuntime::default();
    let candidates = route_runtime
        .candidates(&route_settings, "reliable")
        .expect("route candidates");
    for candidate in &candidates {
        route_runtime.record_quota_exhausted(candidate, Some(Duration::from_secs(7_200)));
    }
    let route_error = route_runtime
        .candidates(&route_settings, "reliable")
        .expect_err("all cooled route targets must not be probed");
    assert!(route_error.contains("all targets cooling down"));
}

#[test]
fn compaction_can_materialize_a_cooled_direct_target_without_sending() {
    let mut settings = settings();
    settings.routes.clear();
    let mut runtime = RoutingRuntime::default();
    let direct = runtime
        .candidates(&settings, "one/model")
        .expect("direct provider")[0]
        .clone();
    runtime.record_quota_exhausted(&direct, Some(Duration::from_secs(7_200)));
    runtime
        .candidates(&settings, "one/model")
        .expect_err("ordinary routing must still reject a cooled target");

    let candidates = runtime
        .candidates_for_request_including_cooldown(&settings, "one/model", false, None)
        .expect("compaction may inspect a cooled target");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].target_key, "one/model");
    assert!(runtime.candidate_is_cooling(&candidates[0]));
    runtime
        .begin_attempt(&candidates[0])
        .expect_err("a cooled target must still refuse an upstream send");
}

fn image_provider(id: &str, source: CredentialSource) -> ProviderDefinition {
    let mut provider = ProviderDefinition {
        id: id.into(),
        name: id.into(),
        base_url: if id == "openai" {
            "https://chatgpt.com/backend-api/codex".into()
        } else {
            "https://api.openai.com/v1".into()
        },
        models: vec!["gpt-5.6".into()],
        image_generation_models: vec!["imagegen-2".into(), "gpt-image-2".into()],
        credential: ProviderCredential {
            source,
            ..ProviderCredential::default()
        },
        capabilities: ProviderCapabilities {
            image_generation: true,
            ..ProviderCapabilities::default()
        },
        ..ProviderDefinition::default()
    };
    provider
        .model_wire_ids
        .insert("imagegen-2".into(), "gpt-image-2".into());
    provider
}

#[test]
fn built_in_imagegen_alias_uses_codex_forward_and_real_wire_model() {
    let mut settings = GatewaySettings {
        default_provider: Some("chat-only".into()),
        providers: vec![
            image_provider("openai", CredentialSource::Forward),
            ProviderDefinition {
                id: "chat-only".into(),
                name: "Chat only".into(),
                enabled: true,
                ..ProviderDefinition::default()
            },
        ],
        ..GatewaySettings::default()
    };
    settings.providers[1].capabilities.image_generation = false;
    let candidates = RoutingRuntime::default()
        .candidates_for_image_generation(&settings, None, Some("imagegen-2"), None)
        .expect("Codex image route");

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].provider.id, "openai");
    assert_eq!(candidates[0].upstream_model, "imagegen-2");
    assert_eq!(
        candidates[0]
            .provider
            .wire_model_id(&candidates[0].upstream_model),
        "gpt-image-2"
    );
}

#[test]
fn fully_qualified_image_sidecar_overrides_codex_forward_with_api_key_provider() {
    let settings = GatewaySettings {
        providers: vec![
            image_provider("openai", CredentialSource::Forward),
            image_provider("openai-api", CredentialSource::Environment),
        ],
        ..GatewaySettings::default()
    };
    let candidates = RoutingRuntime::default()
        .candidates_for_image_generation(
            &settings,
            Some("openai-api/gpt-image-2"),
            Some("imagegen-2"),
            None,
        )
        .expect("explicit image sidecar");

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].provider.id, "openai-api");
    assert_eq!(candidates[0].upstream_model, "gpt-image-2");
    assert_eq!(
        candidates[0].provider.credential.source,
        CredentialSource::Environment
    );
}

#[test]
fn image_routing_never_falls_back_to_chat_default_provider() {
    let settings = GatewaySettings {
        default_provider: Some("opencode-go".into()),
        providers: vec![ProviderDefinition {
            id: "opencode-go".into(),
            name: "OpenCode Go".into(),
            models: vec!["imagegen-2".into()],
            capabilities: ProviderCapabilities {
                image_generation: false,
                ..ProviderCapabilities::default()
            },
            ..ProviderDefinition::default()
        }],
        ..GatewaySettings::default()
    };
    let candidates = RoutingRuntime::default()
        .candidates_for_image_generation(&settings, None, Some("imagegen-2"), None)
        .expect("unconfigured image route");

    assert!(candidates.is_empty());
    let explicit_unknown = RoutingRuntime::default()
        .candidates_for_image_generation(
            &settings,
            None,
            Some("missing-provider/gpt-image-2"),
            None,
        )
        .expect("unknown explicit image provider");
    assert!(explicit_unknown.is_empty());
}

#[test]
fn image_only_models_are_rejected_by_normal_responses_routing() {
    let settings = GatewaySettings {
        default_provider: Some("openai-api".into()),
        providers: vec![image_provider("openai-api", CredentialSource::Environment)],
        ..GatewaySettings::default()
    };

    for model in [
        "openai-api/imagegen-2",
        "openai-api/gpt-image-2",
        "imagegen-2",
        "gpt-image-2",
    ] {
        let error = RoutingRuntime::default()
            .candidates(&settings, model)
            .expect_err("image-only model must not use Responses routing");
        assert!(error.contains("image-generation-only"));
    }

    let image = RoutingRuntime::default()
        .candidates_for_image_generation(
            &settings,
            Some("openai-api/gpt-image-2"),
            Some("imagegen-2"),
            None,
        )
        .expect("image endpoint route");
    assert_eq!(image.len(), 1);
    assert_eq!(image[0].upstream_model, "gpt-image-2");
}

#[test]
fn image_only_routes_are_rejected_normally_but_work_for_image_requests() {
    let settings = GatewaySettings {
        providers: vec![image_provider("openai-api", CredentialSource::Environment)],
        routes: vec![RouteDefinition {
            id: "image-route".into(),
            name: "Image route".into(),
            alias: Some("img-route".into()),
            targets: vec![RouteTarget {
                model: "openai-api/gpt-image-2".into(),
                weight: 1,
            }],
            enabled: true,
            ..RouteDefinition::default()
        }],
        ..GatewaySettings::default()
    };

    assert!(RoutingRuntime::default()
        .candidates(&settings, "img-route")
        .is_err());
    let image = RoutingRuntime::default()
        .candidates_for_image_generation(&settings, Some("img-route"), Some("imagegen-2"), None)
        .expect("image route");
    assert_eq!(image.len(), 1);
    assert_eq!(image[0].upstream_model, "gpt-image-2");
}

#[test]
fn unavailable_image_identity_never_falls_back_into_normal_routes() {
    let mut provider = image_provider("openai-api", CredentialSource::Environment);
    provider.capabilities.image_generation = false;
    let settings = GatewaySettings {
        providers: vec![provider],
        routes: vec![RouteDefinition {
            id: "unavailable-image".into(),
            name: "Unavailable image".into(),
            targets: vec![RouteTarget {
                model: "openai-api/gpt-image-2".into(),
                weight: 1,
            }],
            enabled: true,
            ..RouteDefinition::default()
        }],
        ..GatewaySettings::default()
    };

    assert!(RoutingRuntime::default()
        .candidates(&settings, "unavailable-image")
        .is_err());
    assert!(RoutingRuntime::default()
        .candidates_for_image_generation(
            &settings,
            Some("unavailable-image"),
            Some("gpt-image-2"),
            None,
        )
        .is_err());
}

#[test]
fn normal_mixed_routes_drop_image_targets_and_keep_chat_targets() {
    let mut chat = image_provider("chat", CredentialSource::Environment);
    chat.image_generation_models.clear();
    chat.capabilities.image_generation = false;
    let settings = GatewaySettings {
        providers: vec![
            image_provider("openai-api", CredentialSource::Environment),
            chat,
        ],
        routes: vec![RouteDefinition {
            id: "mixed".into(),
            name: "Mixed".into(),
            targets: vec![
                RouteTarget {
                    model: "openai-api/gpt-image-2".into(),
                    weight: 1,
                },
                RouteTarget {
                    model: "chat/gpt-5.6".into(),
                    weight: 1,
                },
            ],
            enabled: true,
            ..RouteDefinition::default()
        }],
        ..GatewaySettings::default()
    };

    let normal = RoutingRuntime::default()
        .candidates(&settings, "mixed")
        .expect("normal route");
    assert_eq!(normal.len(), 1);
    assert_eq!(normal[0].target_key, "chat/gpt-5.6");

    let image = RoutingRuntime::default()
        .candidates_for_image_generation(&settings, Some("mixed"), Some("imagegen-2"), None)
        .expect("image route");
    assert_eq!(image.len(), 1);
    assert_eq!(image[0].target_key, "openai-api/gpt-image-2");
}

#[test]
fn mixed_route_cursors_are_independent_for_normal_and_image_requests() {
    let mut normal_one = image_provider("normal-one", CredentialSource::Environment);
    normal_one.image_generation_models.clear();
    normal_one.capabilities.image_generation = false;
    let mut normal_two = normal_one.clone();
    normal_two.id = "normal-two".into();
    normal_two.name = "normal-two".into();
    let settings = GatewaySettings {
        providers: vec![
            image_provider("images", CredentialSource::Environment),
            normal_one,
            normal_two,
        ],
        routes: vec![RouteDefinition {
            id: "mixed-cursor".into(),
            name: "Mixed cursor".into(),
            strategy: RouteStrategy::WeightedRoundRobin,
            sticky_requests: 1,
            targets: vec![
                RouteTarget {
                    model: "images/gpt-image-2".into(),
                    weight: 1,
                },
                RouteTarget {
                    model: "normal-one/gpt-5.6".into(),
                    weight: 1,
                },
                RouteTarget {
                    model: "normal-two/gpt-5.6".into(),
                    weight: 1,
                },
            ],
            enabled: true,
            ..RouteDefinition::default()
        }],
        ..GatewaySettings::default()
    };
    let mut runtime = RoutingRuntime::default();

    let normal_first = runtime
        .candidates(&settings, "mixed-cursor")
        .expect("normal first");
    let image = runtime
        .candidates_for_image_generation(&settings, Some("mixed-cursor"), Some("gpt-image-2"), None)
        .expect("image request");
    let normal_second = runtime
        .candidates(&settings, "mixed-cursor")
        .expect("normal second");

    assert_eq!(normal_first[0].target_key, "normal-one/gpt-5.6");
    assert_eq!(image[0].target_key, "images/gpt-image-2");
    assert_eq!(normal_second[0].target_key, "normal-two/gpt-5.6");
}

#[test]
fn image_sidecar_pseudo_model_is_never_a_normal_responses_model() {
    let settings = GatewaySettings {
        sidecars: crate::config::SidecarSettings {
            image_model: Some("openai-api/gpt-image-2".into()),
            ..crate::config::SidecarSettings::default()
        },
        providers: vec![image_provider("openai-api", CredentialSource::Environment)],
        ..GatewaySettings::default()
    };

    let error = RoutingRuntime::default()
        .candidates(&settings, "codetas-sidecar/image")
        .expect_err("image sidecar must not enter normal routing");
    assert!(error.contains("image-generation-only"));
}

#[test]
fn disabled_model_level_image_capability_excludes_provider() {
    let settings = GatewaySettings {
        providers: vec![image_provider("openai", CredentialSource::Forward)],
        model_catalog: vec![crate::config::ModelMetadata {
            provider_id: "openai".into(),
            model_id: "imagegen-2".into(),
            capabilities: ProviderCapabilities {
                image_generation: false,
                ..ProviderCapabilities::default()
            },
            ..crate::config::ModelMetadata::default()
        }],
        ..GatewaySettings::default()
    };
    let candidates = RoutingRuntime::default()
        .candidates_for_image_generation(&settings, None, Some("imagegen-2"), None)
        .expect("model-level exclusion");

    assert!(candidates.is_empty());
}

#[test]
fn soft_failures_are_scoped_per_session_and_do_not_bleed_across_sessions() {
    let mut settings = settings();
    settings.routes.clear();
    let mut runtime = RoutingRuntime::default();
    let session_a = Some("session-a");
    let session_b = Some("session-b");

    // Three soft failures from session A push the target into a 60s
    // soft cooldown for session A only. Soft-avoid (30s) also fires per
    // failure; we clear it between iterations so this test can isolate the
    // session-scoped soft-cooldown behavior.
    for _ in 0..3 {
        runtime.clear_soft_avoid("one/model");
        runtime.clear_session_avoid(session_a);
        let candidate = runtime
            .candidates_scoped(&settings, "one/model", session_a)
            .expect("session a candidate")[0]
            .clone();
        assert_eq!(candidate.session_scope.as_deref(), session_a);
        runtime.record_failure(&candidate);
    }
    let err_a = runtime
        .candidates_scoped(&settings, "one/model", session_a)
        .expect_err("session a must observe its own soft cooldown");
    assert!(err_a.contains("cooling down"));

    // Soft-avoid (30s, credential-wide) is a separate layer from the
    // session-scoped soft cooldown. Clear it so we can assert the cooldown
    // itself does not bleed to session B / un-scoped resolution.
    runtime.clear_soft_avoid("one/model");
    // Session B and un-scoped resolution stay available: a soft failure in
    // one session must not cool the same provider target globally.
    assert!(runtime
        .candidates_scoped(&settings, "one/model", session_b)
        .is_ok());
    assert!(runtime.candidates(&settings, "one/model").is_ok());

    // Session A's failures live under its session bucket, never under the
    // global credential key.
    assert!(runtime.failures.contains_key("session-a::one/model"));
    assert!(!runtime.failures.contains_key("one/model"));
}

#[test]
fn session_scoped_soft_cooldown_is_hard_quota_aware() {
    let mut settings = settings();
    settings.routes.clear();
    let mut runtime = RoutingRuntime::default();
    let candidate = runtime
        .candidates_scoped(&settings, "one/model", Some("session-a"))
        .expect("candidate")[0]
        .clone();

    // A real 429 from the provider globally hard-cools the credential
    // target for every session, overriding session-scoped soft state.
    runtime.record_quota_exhausted(&candidate, Some(Duration::from_secs(7_200)));
    assert!(runtime
        .candidates_scoped(&settings, "one/model", Some("session-a"))
        .is_err());
    assert!(runtime
        .candidates_scoped(&settings, "one/model", Some("session-b"))
        .is_err());
    assert!(runtime.candidates(&settings, "one/model").is_err());
}

#[test]
fn success_in_one_session_only_clears_that_sessions_soft_failures() {
    let mut settings = settings();
    settings.routes.clear();
    let mut runtime = RoutingRuntime::default();
    for _ in 0..3 {
        runtime.clear_soft_avoid("one/model");
        runtime.clear_session_avoid(Some("session-a"));
        let candidate = runtime
            .candidates_scoped(&settings, "one/model", Some("session-a"))
            .expect("session a candidate")[0]
            .clone();
        runtime.record_failure(&candidate);
    }
    runtime.clear_soft_avoid("one/model");
    runtime.clear_session_avoid(Some("session-a"));
    assert!(runtime
        .candidates_scoped(&settings, "one/model", Some("session-a"))
        .is_err());

    // A success (e.g. a follow-up send that succeeded in session B) must
    // not clear session A's soft failure count.
    let session_b_candidate = runtime
        .candidates_scoped(&settings, "one/model", Some("session-b"))
        .expect("session b candidate")[0]
        .clone();
    runtime.record_success(&session_b_candidate, None);
    assert!(runtime
        .candidates_scoped(&settings, "one/model", Some("session-a"))
        .is_err());
    assert!(runtime.failures.contains_key("session-a::one/model"));
    assert!(!runtime.failures.contains_key("session-b::one/model"));
}

#[test]
fn transient_failure_does_not_drop_the_requested_target() {
    let settings = settings();
    let mut runtime = RoutingRuntime::default();

    let candidate = runtime
        .candidates(&settings, "one/model")
        .expect("candidate")[0]
        .clone();
    runtime.record_failure(&candidate);
    assert!(runtime.soft_avoids.contains_key("one/model"));

    let failover = runtime
        .candidates_scoped(&settings, "reliable", Some("session-a"))
        .expect("soft-avoid must not hide the primary target");
    assert!(failover.iter().any(|item| item.target_key == "one/model"));
    assert!(failover.iter().any(|item| item.target_key == "two/model"));
}

#[test]
fn sole_soft_avoided_target_fail_opens_instead_of_rejecting() {
    let mut settings = settings();
    settings.routes.clear();
    let mut runtime = RoutingRuntime::default();
    let candidate = runtime
        .candidates(&settings, "one/model")
        .expect("candidate")[0]
        .clone();
    runtime.record_failure(&candidate);
    assert!(runtime.soft_avoids.contains_key("one/model"));

    // OpenCodex: soft-avoid never throws when there is no alternate.
    // A 400 here is what killed Codex compact on xai/grok-4.6.
    let recovered = runtime
        .candidates_scoped(&settings, "one/model", Some("session-a"))
        .expect("sole avoided target must fail-open");
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].target_key, "one/model");
    assert!(runtime.candidates(&settings, "one/model").is_ok());
}

#[test]
fn account_pool_soft_avoid_skips_only_when_another_account_is_healthy() {
    let mut settings = settings();
    settings.routes.clear();
    settings.account_pool.accounts = vec![account("primary", 100), account("secondary", 50)];
    let mut runtime = RoutingRuntime::default();
    let primary = runtime
        .candidates(&settings, "one/model")
        .expect("pool")
        .into_iter()
        .find(|candidate| candidate.account_id.as_deref() == Some("primary"))
        .expect("primary account");
    runtime.record_failure(&primary);

    let remaining = runtime
        .candidates(&settings, "one/model")
        .expect("soft-avoid must not hide a pool account");
    assert!(remaining
        .iter()
        .any(|candidate| candidate.account_id.as_deref() == Some("primary")));
    assert!(remaining
        .iter()
        .any(|candidate| candidate.account_id.as_deref() == Some("secondary")));
}

#[test]
fn soft_avoid_is_cleared_on_success() {
    let settings = settings();
    let mut runtime = RoutingRuntime::default();
    let candidate = runtime
        .candidates(&settings, "one/model")
        .expect("candidate")[0]
        .clone();
    runtime.record_failure(&candidate);
    assert!(runtime
        .candidates_scoped(&settings, "reliable", Some("session-a"))
        .expect("still routable")
        .iter()
        .any(|item| item.target_key == "one/model"));

    // 2xx proves health: soft-avoid is cleared and the target is routable.
    runtime.record_success(&candidate, None);
    assert!(!runtime.soft_avoids.contains_key("one/model"));
    let failover = runtime
        .candidates(&settings, "reliable")
        .expect("both targets after success");
    assert!(failover.iter().any(|item| item.target_key == "one/model"));
    assert!(failover.iter().any(|item| item.target_key == "two/model"));
}

#[test]
fn session_avoid_defers_only_the_session_that_failed_the_target() {
    let settings = settings();
    let mut runtime = RoutingRuntime::default();
    let session_a = Some("session-a");
    let session_b = Some("session-b");

    // Session A fails the target and is now deferred from it. Soft-avoid
    // (credential-wide) fires first; clear it so the test isolates the
    // per-session deferral.
    let a = runtime
        .candidates_scoped(&settings, "one/model", session_a)
        .expect("session a candidate")[0]
        .clone();
    runtime.record_failure(&a);
    runtime.clear_soft_avoid("one/model");

    // Session-avoid is bookkeeping only: the failed target stays selectable.
    assert!(runtime
        .candidates_scoped(&settings, "one/model", session_a)
        .is_ok());
    let still_primary = runtime
        .candidates_scoped(&settings, "reliable", session_a)
        .expect("session a failover");
    assert!(still_primary
        .iter()
        .any(|item| item.target_key == "one/model"));

    // Session B / un-scoped are not deferred by A's failure.
    runtime.clear_soft_avoid("one/model");
    let session_b_route = runtime
        .candidates_scoped(&settings, "reliable", session_b)
        .expect("session b failover");
    assert!(session_b_route
        .iter()
        .any(|item| item.target_key == "one/model"));
    assert!(runtime.candidates(&settings, "one/model").is_ok());
}

#[test]
fn concurrent_sends_to_the_same_target_are_not_capped() {
    let mut settings = settings();
    settings.routes.clear();
    let mut runtime = RoutingRuntime::default();
    let candidate = runtime
        .candidates(&settings, "one/model")
        .expect("candidate")[0]
        .clone();

    for _ in 0..8 {
        runtime
            .begin_attempt(&candidate)
            .expect("concurrent lease must be granted");
    }
    assert_eq!(runtime.active_attempts(&candidate), 8);
    runtime.end_attempt(&candidate);
    assert_eq!(runtime.active_attempts(&candidate), 7);
}

#[test]
fn success_on_one_target_does_not_clear_session_avoid_of_another() {
    let settings = settings(); // route "reliable" targets one/model, two/model
    let mut runtime = RoutingRuntime::default();
    let session = Some("session-a");

    // Session A fails target `two/model`: session_avoids[session-a] -> two/model.
    let two = runtime
        .candidates_scoped(&settings, "two/model", session)
        .expect("two candidate")[0]
        .clone();
    runtime.record_failure(&two);
    runtime.clear_soft_avoid(&failure_key(&two));
    assert!(runtime.is_session_avoided(session, &failure_key(&two)));

    // Session A then succeeds on a *different* target `one/model`. This must
    // not lift the avoidance of `two/model`.
    runtime.clear_soft_avoid("one/model");
    let one = runtime
        .candidates_scoped(&settings, "one/model", session)
        .expect("one candidate")[0]
        .clone();
    runtime.record_success(&one, None);
    assert!(
        runtime.is_session_avoided(session, &failure_key(&two)),
        "success on one target must not clear avoidance of another"
    );
}

#[test]
fn session_avoids_keep_multiple_failed_targets() {
    let settings = settings();
    let mut runtime = RoutingRuntime::default();
    let session = Some("session-a");

    let one = runtime
        .candidates_scoped(&settings, "one/model", session)
        .expect("one candidate")[0]
        .clone();
    let two = runtime
        .candidates_scoped(&settings, "two/model", session)
        .expect("two candidate")[0]
        .clone();

    runtime.record_failure(&one);
    runtime.record_failure(&two);
    runtime.clear_soft_avoid(&failure_key(&one));
    runtime.clear_soft_avoid(&failure_key(&two));

    assert!(runtime.is_session_avoided(session, &failure_key(&one)));
    assert!(runtime.is_session_avoided(session, &failure_key(&two)));
}
