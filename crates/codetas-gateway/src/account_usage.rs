//! Account-wide quota/balance reads, adapted from kinocode's MIT dsh-usage-bars
//! (lib/sources.js, lib/providers/{copilot,minimax}.js) and dsh-model-routes
//! (lib/agy). These endpoints do not submit inference requests.
use crate::{CredentialSource, ProviderDefinition};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::sync::Mutex;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUsageWindow {
    pub label: String,
    pub used_percent: f64,
    pub resets_at: Option<String>,
    /// None means a shared account window, NOT an independent per-model limit.
    pub model_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAccountUsage {
    pub provider_id: String,
    pub account_id: Option<String>,
    pub status: String,
    pub source: String,
    pub observed_at_ms: Option<i64>,
    pub checked_at_ms: i64,
    pub stale: bool,
    pub plan: Option<String>,
    pub windows: Vec<ProviderUsageWindow>,
    pub balance: Option<String>,
    pub error: Option<String>,
}
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
fn numeric(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str()?.parse().ok())
        .filter(|n| n.is_finite())
}
fn percent(v: &Value) -> Option<f64> {
    numeric(v).filter(|n| (0.0..=100.0).contains(n))
}
fn timestamp(v: &Value, seconds: bool) -> Option<String> {
    if let Some(n) = numeric(v) {
        if n <= 0.0 {
            return None;
        }
        return Some(format!("{}", if seconds { n * 1000.0 } else { n } as i64));
    }
    v.as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

fn window(
    out: &mut Vec<ProviderUsageWindow>,
    label: &str,
    pct: Option<f64>,
    reset: Option<String>,
    model: Option<String>,
) {
    if let Some(used_percent) = pct {
        out.push(ProviderUsageWindow {
            label: label.into(),
            used_percent,
            resets_at: reset,
            model_id: model,
        });
    }
}
fn source(provider: &ProviderDefinition) -> Option<&'static str> {
    let oauth = matches!(
        provider.credential.source,
        CredentialSource::OAuth | CredentialSource::Forward
    );
    match provider.id.as_str() {
        "openai" if oauth => Some("https://chatgpt.com/backend-api/wham/usage"),
        "anthropic" if oauth => Some("https://api.anthropic.com/api/oauth/usage"),
        "google-antigravity" => {
            Some("https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels")
        }
        "google-gemini-cli" | "gemini-cli" if oauth => {
            Some("https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota")
        }
        "deepseek" => Some("https://api.deepseek.com/user/balance"),
        "command-code" | "commandcode" => Some("https://api.commandcode.ai/alpha/billing/credits"),
        "openrouter" => Some("https://openrouter.ai/api/v1/credits"),
        "zai" | "z-ai" => Some("https://api.z.ai/api/monitor/usage/quota/limit"),
        "zhipu-bigmodel" => Some("https://open.bigmodel.cn/api/monitor/usage/quota/limit"),
        "minimax" => Some("https://api.minimax.io/v1/coding_plan/remains"),
        "minimax-cn" => Some("https://api.minimaxi.com/v1/coding_plan/remains"),
        "github-copilot" => Some("https://api.github.com/copilot_internal/user"),
        _ => None,
    }
}
fn empty(
    provider: &ProviderDefinition,
    account_id: Option<String>,
    endpoint: Option<&str>,
) -> ProviderAccountUsage {
    ProviderAccountUsage {
        provider_id: provider.id.clone(),
        account_id,
        status: "error".into(),
        source: endpoint.unwrap_or("").into(),
        observed_at_ms: None,
        checked_at_ms: now_ms(),
        stale: false,
        plan: None,
        windows: vec![],
        balance: None,
        error: None,
    }
}

/// Parse only known fields. Missing/malformed/null values never become zero.
fn parse(provider: &str, data: &Value, out: &mut ProviderAccountUsage) {
    match provider {
        "openai" => {
            out.plan = data["plan_type"].as_str().map(str::to_owned);
            for (label, key) in [("5h", "primary_window"), ("7d", "secondary_window")] {
                let w = &data["rate_limit"][key];
                // Prefer actual server window duration over a guessed label.
                let label = numeric(&w["limit_window_seconds"])
                    .map(|n| format!("{}h", n / 3600.0))
                    .unwrap_or(label.into());
                window(
                    &mut out.windows,
                    &label,
                    percent(&w["used_percent"]),
                    timestamp(&w["reset_at"], true),
                    None,
                );
            }
        }
        "anthropic" => {
            for (label, key, model) in [
                ("5h", "five_hour", None),
                ("7d", "seven_day", None),
                ("7d Opus", "seven_day_opus", Some("claude-opus")),
                ("7d Sonnet", "seven_day_sonnet", Some("claude-sonnet")),
            ] {
                let w = &data[key];
                window(
                    &mut out.windows,
                    label,
                    percent(&w["utilization"]),
                    timestamp(&w["resets_at"], false),
                    model.map(str::to_owned),
                );
            }
        }
        "google-antigravity" => {
            if let Some(models) = data["models"].as_object() {
                for (id, entry) in models {
                    window(
                        &mut out.windows,
                        id,
                        numeric(&entry["quotaInfo"]["remainingFraction"])
                            .filter(|n| (0.0..=1.0).contains(n))
                            .map(|n| (1.0 - n) * 100.0),
                        timestamp(&entry["quotaInfo"]["resetTime"], false),
                        Some(id.clone()),
                    );
                }
            }
        }
        "google-gemini-cli" | "gemini-cli" => {
            if let Some(buckets) = data["buckets"].as_array() {
                for bucket in buckets {
                    if let Some(id) = bucket["modelId"].as_str() {
                        window(
                            &mut out.windows,
                            id,
                            numeric(&bucket["remainingFraction"])
                                .filter(|n| (0.0..=1.0).contains(n))
                                .map(|n| (1.0 - n) * 100.0),
                            timestamp(&bucket["resetTime"], false),
                            Some(id.into()),
                        );
                    }
                }
            }
        }
        "deepseek" => {
            if let Some(balances) = data["balance_infos"].as_array() {
                let values: Vec<_> = balances
                    .iter()
                    .filter_map(|b| {
                        let value = numeric(&b["total_balance"])?;
                        let currency = b["currency"].as_str()?;
                        Some(format!("{value:.4} {currency}"))
                    })
                    .collect();
                if !values.is_empty() {
                    out.balance = Some(values.join(" / "));
                }
            }
        }
        "command-code" | "commandcode" => {
            for (label, key) in [("5h", "fiveHour"), ("7d", "weekly")] {
                let w = &data["windowLimits"][key];
                let pct = numeric(&w["cap"])
                    .filter(|n| *n > 0.0)
                    .zip(numeric(&w["used"]))
                    .map(|(cap, used)| (used / cap * 100.0).clamp(0.0, 100.0));
                window(
                    &mut out.windows,
                    label,
                    pct,
                    timestamp(&w["resetAt"], false),
                    None,
                );
            }
            if let Some(left) = numeric(&data["credits"]["monthlyCredits"]) {
                out.balance = Some(format!("{left:.4} USD (monthly credits)"));
            }
        }
        "openrouter" => {
            if let (Some(total), Some(used)) = (
                numeric(&data["data"]["total_credits"]),
                numeric(&data["data"]["total_usage"]),
            ) {
                out.balance = Some(format!("{:.4} USD", total - used));
                if total > 0.0 {
                    window(
                        &mut out.windows,
                        "credits",
                        Some((used / total * 100.0).clamp(0.0, 100.0)),
                        None,
                        None,
                    );
                }
            }
        }
        "zai" | "z-ai" | "zhipu-bigmodel" => {
            out.plan = data["data"]["level"].as_str().map(str::to_owned);
            if let Some(limits) = data["data"]["limits"].as_array() {
                for limit in limits {
                    window(
                        &mut out.windows,
                        limit["type"].as_str().unwrap_or("quota"),
                        percent(&limit["percentage"]),
                        timestamp(&limit["nextResetTime"], false),
                        None,
                    );
                }
            }
        }
        "minimax" | "minimax-cn" => {
            // current_*_usage_count is REMAINING, despite its name.
            if numeric(&data["base_resp"]["status_code"]).is_some_and(|n| n != 0.0) {
                return;
            }
            if let Some(models) = data["model_remains"]
                .as_array()
                .or_else(|| data["data"]["model_remains"].as_array())
            {
                for model in models {
                    let id = model["model_name"].as_str().map(str::to_owned);
                    for (label, total, remaining, reset) in [
                        (
                            "5h",
                            "current_interval_total_count",
                            "current_interval_usage_count",
                            "end_time",
                        ),
                        (
                            "7d",
                            "current_weekly_total_count",
                            "current_weekly_usage_count",
                            "weekly_end_time",
                        ),
                    ] {
                        let pct = numeric(&model[total])
                            .filter(|n| *n > 0.0)
                            .zip(numeric(&model[remaining]))
                            .map(|(cap, left)| ((cap - left) / cap * 100.0).clamp(0.0, 100.0));
                        let seconds = numeric(&model[reset]).is_some_and(|n| n < 1e12);
                        window(
                            &mut out.windows,
                            label,
                            pct,
                            timestamp(&model[reset], seconds),
                            id.clone(),
                        );
                    }
                }
            }
        }
        "github-copilot" => {
            out.plan = data["copilot_plan"]
                .as_str()
                .or_else(|| data["copilotPlan"].as_str())
                .map(str::to_owned);
            let qs = data
                .get("quota_snapshots")
                .or_else(|| data.get("quotaSnapshots"))
                .unwrap_or(&Value::Null);
            for (label, key, alt) in [
                ("premium", "premium_interactions", "premiumInteractions"),
                ("chat", "chat", "chat"),
            ] {
                let q = qs.get(key).or_else(|| qs.get(alt)).unwrap_or(&Value::Null);
                if q["unlimited"] == true {
                    continue;
                }
                let p = percent(
                    q.get("percent_remaining")
                        .or_else(|| q.get("percentRemaining"))
                        .unwrap_or(&Value::Null),
                )
                .map(|n| 100.0 - n);
                window(
                    &mut out.windows,
                    label,
                    p,
                    timestamp(
                        data.get("quota_reset_date")
                            .or_else(|| data.get("quotaResetDate"))
                            .unwrap_or(&Value::Null),
                        false,
                    ),
                    None,
                );
            }
        }
        _ => {}
    }
    if !out.windows.is_empty() || out.balance.is_some() {
        out.status = "ok".into();
        out.observed_at_ms = Some(now_ms());
        out.error = None;
    } else {
        out.error = Some("使用量APIに対応するデータがありません".into());
    }
}

struct Cached {
    until: i64,
    value: ProviderAccountUsage,
}
type Slot = Arc<Mutex<Option<Cached>>>;
fn slots() -> &'static Mutex<HashMap<String, Slot>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// borrowed_login is used ONLY for Codex Forward auth, never an API account.
/// Neither credentials nor response bodies are returned to the frontend/logs.
pub async fn fetch_provider_account_usage(
    provider: &ProviderDefinition,
    account_id: Option<String>,
    borrowed_login: Option<(&str, &str)>,
) -> ProviderAccountUsage {
    let borrowed_login =
        borrowed_login.filter(|_| provider.credential.source == CredentialSource::Forward);
    let endpoint = source(provider);
    let mut result = empty(provider, account_id, endpoint);
    if !provider.enabled {
        result.status = "disabled".into();
        result.error = Some("接続が無効です".into());
        return result;
    }
    let Some(endpoint) = endpoint else {
        result.status = "unsupported".into();
        result.error = Some("この接続のアカウント使用量APIは未対応です".into());
        return result;
    };
    // Forward must not resolve to an API key or an unrelated stored OAuth login.
    let token = if matches!(provider.credential.source, CredentialSource::Forward) {
        borrowed_login
            .map(|(token, _)| token.to_owned())
            .ok_or_else(|| "Codexのログイン情報がありません".to_string())
    } else {
        resolve_secret(provider).await
    };
    let token = match token {
        Ok(token) => token,
        Err(_) => {
            result.error =
                Some("認証情報を取得できません。接続のログイン状態を確認してください".into());
            return result;
        }
    };
    let identity = borrowed_login.map(|(_, id)| id.to_owned()).or_else(|| {
        provider
            .credential
            .reference
            .as_deref()
            .and_then(crate::oauth::oauth_account_id)
    });
    fetch_with_token(provider, result, endpoint, &token, identity.as_deref()).await
}

async fn fetch_with_token(
    provider: &ProviderDefinition,
    mut result: ProviderAccountUsage,
    endpoint: &str,
    token: &str,
    identity: Option<&str>,
) -> ProviderAccountUsage {
    // Include credential identity, settings and logical account to avoid reusing
    // a previous login's result after an account switch or credential rotation.
    let key = format!(
        "{:x}",
        Sha256::digest(format!(
            "{}|{:?}|{:?}|{}|{}",
            format!("{}|{}|{}", provider.id, provider.base_url, endpoint),
            result.account_id,
            provider.credential,
            identity.unwrap_or(""),
            token
        ))
    );
    let slot = {
        let mut slots = slots().lock().await;
        if slots.len() > 512 {
            slots.clear();
        }
        slots
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    };
    let mut guard = slot.lock().await;
    if let Some(cached) = guard.as_ref().filter(|c| c.until > now_ms()) {
        return cached.value.clone();
    }
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            result.error = Some("使用量APIへの接続を準備できません".into());
            return result;
        }
    };
    let mut req = client
        .get(endpoint)
        .bearer_auth(&token)
        .header("accept", "application/json");
    match provider.id.as_str() {
        "openai" => {
            req = req.header("user-agent", "codex-tui/0.149.1");
            if let Some(id) = identity.as_deref() {
                req = req.header("chatgpt-account-id", id);
            }
        }
        "anthropic" => {
            req = req
                .header("anthropic-beta", "oauth-2025-04-20")
                .header("user-agent", "claude-code/2.1.0");
        }
        "github-copilot" => {
            req = req
                .header("authorization", format!("token {token}"))
                .header("editor-version", "vscode/1.96.2")
                .header("editor-plugin-version", "copilot-chat/0.26.7")
                .header("user-agent", "GitHubCopilotChat/0.26.7")
                .header("x-github-api-version", "2025-04-01");
        }
        "google-antigravity" | "google-gemini-cli" | "gemini-cli" => {
            // Obtain the project for THIS credential, never borrow the active
            // local language server's identity for a different pool account.
            let agy = provider.id == "google-antigravity";
            let load = client
                .post(if agy {
                    "https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist"
                } else {
                    "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist"
                })
                .bearer_auth(&token)
                .header("user-agent", crate::oauth::antigravity_user_agent())
                .json(&json!({"metadata":{"ideType":if agy {"ANTIGRAVITY"} else {"GEMINI_CLI"}}}))
                .send()
                .await;
            let project = match load {
                Ok(r) if r.status().is_success() => read_json(r).await.ok().and_then(|v| {
                    v["cloudaicompanionProject"]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| {
                            v["cloudaicompanionProject"]["id"]
                                .as_str()
                                .map(str::to_owned)
                        })
                }),
                _ => None,
            };
            req = client
                .post(endpoint)
                .bearer_auth(&token)
                .header("user-agent", crate::oauth::antigravity_user_agent())
                .json(
                    &project
                        .map(|project| json!({"project":project}))
                        .unwrap_or(json!({})),
                );
        }
        _ => {}
    }
    let mut ttl = 300_000;
    match req.send().await {
        Ok(response) => {
            if response.status().is_success() {
                match read_json(response).await {
                    Ok(data) => parse(&provider.id, &data, &mut result),
                    Err(_) => result.error = Some("使用量APIの応答を解析できません".into()),
                }
            } else {
                let status = response.status().as_u16();
                result.error = Some(if status == 401 || status == 403 {
                    format!("HTTP {status}: 再ログインまたは使用量の参照権限が必要です")
                } else {
                    format!("使用量API: HTTP {status}")
                });
                if status == 429 {
                    ttl = response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<i64>().ok())
                        .unwrap_or(600)
                        .clamp(600, 3600)
                        * 1000;
                    if let Some(old) = guard.as_ref().filter(|c| c.value.observed_at_ms.is_some()) {
                        let mut stale = old.value.clone();
                        stale.stale = true;
                        stale.status = "error".into();
                        stale.error = result.error.clone();
                        stale.checked_at_ms = now_ms();
                        result = stale;
                    }
                }
            }
        }
        Err(_) => {
            result.error =
                Some("使用量APIへの接続に失敗しました（タイムアウトまたは通信エラー）".into())
        }
    }
    if result.status != "ok" && ttl == 300_000 {
        ttl = 60_000;
    }
    *guard = Some(Cached {
        until: now_ms() + ttl,
        value: result.clone(),
    });
    result
}
async fn resolve_secret(provider: &ProviderDefinition) -> Result<String, String> {
    // Resolve just the selected account credential. Never forward configured
    // request headers/environment headers to a different billing endpoint.
    let mut auth_provider = provider.clone();
    auth_provider.headers.clear();
    auth_provider.env_headers.clear();
    auth_provider.credential.transport = crate::CredentialTransport::Bearer;
    let headers = crate::auth::resolve_provider_headers(&auth_provider, None).await?;
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "missing credential".into())
}
async fn read_json(mut response: reqwest::Response) -> Result<Value, ()> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
            return Err(());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| ())
}

#[cfg(test)]
mod tests {
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
    fn google_keeps_model_identity() {
        let out = parsed(
            "google-antigravity",
            json!({"models":{"claude-sonnet":{"quotaInfo":{"remainingFraction":0.75}}}}),
        );
        assert_eq!(out.windows[0].model_id.as_deref(), Some("claude-sonnet"));
        assert_eq!(out.windows[0].used_percent, 25.0);
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
            fetch_provider_account_usage(&provider, None, Some(("test-token", "remote-account")))
                .await;
        assert_eq!(out.status, "unsupported");
        provider.enabled = false;
        let out = fetch_provider_account_usage(&provider, None, None).await;
        assert_eq!(out.status, "disabled");
    }
}
