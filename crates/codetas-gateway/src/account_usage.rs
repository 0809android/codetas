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
const NO_USAGE_DATA: &str = "使用量APIに対応するデータがありません";

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
/// A provider's usage API, with the domains its connections live on. Subscription
/// hosts count: Copilot serves from `githubcopilot.com` and reports on `github.com`.
fn usage_api(provider: &ProviderDefinition) -> Option<(&'static str, &'static [&'static str])> {
    let oauth = matches!(
        provider.credential.source,
        CredentialSource::OAuth | CredentialSource::Forward
    );
    Some(match provider.id.as_str() {
        "openai" if oauth => (
            "https://chatgpt.com/backend-api/wham/usage",
            &["chatgpt.com", "openai.com"],
        ),
        "anthropic" if oauth => (
            "https://api.anthropic.com/api/oauth/usage",
            &["anthropic.com"],
        ),
        "google-antigravity" => (
            "https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
            &["googleapis.com"],
        ),
        "google-gemini-cli" | "gemini-cli" if oauth => (
            "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota",
            &["googleapis.com"],
        ),
        "deepseek" => ("https://api.deepseek.com/user/balance", &["deepseek.com"]),
        "command-code" | "commandcode" => (
            "https://api.commandcode.ai/alpha/billing/credits",
            &["commandcode.ai"],
        ),
        "openrouter" => ("https://openrouter.ai/api/v1/credits", &["openrouter.ai"]),
        "zai" | "z-ai" => ("https://api.z.ai/api/monitor/usage/quota/limit", &["z.ai"]),
        "zhipu-bigmodel" => (
            "https://open.bigmodel.cn/api/monitor/usage/quota/limit",
            &["bigmodel.cn"],
        ),
        "minimax" => (
            "https://api.minimax.io/v1/coding_plan/remains",
            &["minimax.io"],
        ),
        "minimax-cn" => (
            "https://api.minimaxi.com/v1/coding_plan/remains",
            &["minimaxi.com"],
        ),
        "github-copilot" => (
            "https://api.github.com/copilot_internal/user",
            &["githubcopilot.com", "github.com"],
        ),
        "mistral" => (
            "https://api.mistral.ai/v1/billing/budget",
            &["api.mistral.ai"],
        ),
        _ => return None,
    })
}

/// The usage host is fixed, so the credential must be one that vendor already gets.
/// A key meant for a proxy or a dedicated deployment stays there, and a Forward
/// credential is the Codex login, which belongs to OpenAI alone.
fn source(provider: &ProviderDefinition) -> Option<&'static str> {
    if provider.credential.source == CredentialSource::Forward && provider.id != "openai" {
        return None;
    }
    let (endpoint, domains) = usage_api(provider)?;
    let url = url::Url::parse(&provider.base_url).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    domains
        .iter()
        .any(|domain| {
            host.strip_suffix(domain)
                .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('.'))
        })
        .then_some(endpoint)
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
        "mistral" => {
            // A plan has two allowances. A Vibe key spends `vibe_budget` whichever
            // client sends the request (measured live); `api_budget` did not move.
            out.plan = data["vibe_budget"]["payg_enabled"]
                .as_bool()
                .map(mistral_overage_label);
            for (name, budget) in [
                ("Vibe Code", &data["vibe_budget"]),
                ("API", &data["api_budget"]),
            ] {
                // Vibe shows no gauge without a positive allowance. Usage passes
                // 100% only under pay-as-you-go, and the label above says so.
                let Some(allowance) = numeric(&budget["initial_budget"]).filter(|n| *n > 0.0)
                else {
                    continue;
                };
                let currency = budget["currency"].as_str().unwrap_or_default();
                let label = format!("{name} · {allowance:.0} {currency}");
                window(
                    &mut out.windows,
                    label.trim_end(),
                    numeric(&budget["usage_percentage"])
                        .filter(|n| *n >= 0.0)
                        .map(|n| n.min(100.0)),
                    timestamp(&budget["reset_at"], false),
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
        out.error = Some(NO_USAGE_DATA.into());
    }
}

fn mistral_overage_label(payg: bool) -> String {
    if payg {
        "枠を超えると従量課金".into()
    } else {
        "枠を超えると停止".into()
    }
}

fn mistral_plan_title(name: &str) -> String {
    match name.to_ascii_uppercase().as_str() {
        "FREE" => "Free".into(),
        "INDIVIDUAL" => "Pro".into(),
        "EDU" => "Student".into(),
        "TEAM" => "Team".into(),
        _ => name.to_owned(),
    }
}

/// Which billing bucket a Mistral key draws on is the point of showing usage:
/// only a Vibe key (`chat`) spends the plan's monthly Vibe Code allowance.
fn apply_mistral_account(
    out: &mut ProviderAccountUsage,
    subscription: Option<&Value>,
    whoami: Option<&Value>,
) {
    let kind = whoami
        .and_then(|value| value["plan_type"].as_str())
        .map(|kind| kind.trim().to_ascii_lowercase());
    let plan_name = |value: Option<&Value>| {
        value
            .and_then(|value| value["plan_name"].as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(mistral_plan_title)
    };
    let name = match kind.as_deref() {
        Some("api") => Some("APIキー（Vibe Code の枠は使いません）".to_string()),
        Some("mistral_code") => Some("Codestral キー".to_string()),
        _ => plan_name(subscription).or_else(|| plan_name(whoami)),
    };
    // Each allowance has its own pay-as-you-go setting.
    let vibe_overage = out.plan.take();
    let overage = if kind.as_deref() == Some("api") {
        subscription
            .and_then(|value| value["payg_enabled"].as_bool())
            .map(mistral_overage_label)
    } else {
        vibe_overage.or_else(|| {
            subscription
                .and_then(|value| value["vibe_payg_enabled"].as_bool())
                .map(mistral_overage_label)
        })
    };
    let parts: Vec<String> = name.into_iter().chain(overage).collect();
    out.plan = (!parts.is_empty()).then(|| parts.join(" · "));
    // Only when the API answered with no allowance. A failed read keeps its reason.
    if out.error.as_deref() == Some(NO_USAGE_DATA)
        && matches!(kind.as_deref(), Some("api" | "mistral_code"))
    {
        out.error = Some("このキーには Vibe Code の月間枠がありません".into());
    }
}

/// Both reads are extras that the usage bars do not depend on, and the caller
/// waits for them while it holds the provider's cache slot. Vibe's own billing
/// reads use the same limit.
const MISTRAL_ACCOUNT_READ_TIMEOUT: Duration = Duration::from_secs(3);

async fn mistral_account_reads(
    client: &reqwest::Client,
    token: &str,
) -> (Option<Value>, Option<Value>) {
    let read = |url: &'static str| async move {
        let response = client
            .get(url)
            .timeout(MISTRAL_ACCOUNT_READ_TIMEOUT)
            .bearer_auth(token)
            .header("accept", "application/json")
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        read_json(response).await.ok()
    };
    tokio::join!(
        read("https://api.mistral.ai/v1/billing/subscription"),
        read("https://console.mistral.ai/api/vibe/whoami"),
    )
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
            "{}|{}|{}|{:?}|{:?}|{}|{}",
            provider.id,
            provider.base_url,
            endpoint,
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
    let mut req = authorized_get(&client, endpoint, &provider.id, token);
    match provider.id.as_str() {
        "openai" => {
            req = req.header("user-agent", "codex-tui/0.149.1");
            if let Some(id) = identity {
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
                .bearer_auth(token)
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
                .bearer_auth(token)
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
    let (sent, mistral_account) = if provider.id == "mistral" {
        let (sent, reads) = tokio::join!(req.send(), mistral_account_reads(&client, token));
        (sent, Some(reads))
    } else {
        (req.send().await, None)
    };
    match sent {
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
    // A stale copy from the last good read already carries these.
    if let Some((subscription, whoami)) = mistral_account.filter(|_| !result.stale) {
        apply_mistral_account(&mut result, subscription.as_ref(), whoami.as_ref());
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
/// Copilot takes its own scheme, and reqwest appends a second `Authorization`
/// rather than replacing a first one.
fn authorized_get(
    client: &reqwest::Client,
    endpoint: &str,
    provider_id: &str,
    token: &str,
) -> reqwest::RequestBuilder {
    let request = client.get(endpoint).header("accept", "application/json");
    if provider_id != "github-copilot" {
        return request.bearer_auth(token);
    }
    match reqwest::header::HeaderValue::from_str(&format!("token {token}")) {
        Ok(mut value) => {
            value.set_sensitive(true);
            request.header(reqwest::header::AUTHORIZATION, value)
        }
        // Not sendable at all; the read reports the provider's 401.
        Err(_) => request,
    }
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
            let out = fetch_provider_account_usage(
                &provider,
                None,
                Some(("codex-token", "codex-account")),
            )
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
