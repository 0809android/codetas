//! Account-wide quota/balance reads, adapted from kinocode's MIT dsh-usage-bars
//! (lib/sources.js, lib/providers/{copilot,minimax}.js) and dsh-model-routes
//! (lib/agy). These endpoints do not submit inference requests.
use crate::{CredentialSource, ProviderDefinition};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
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
    /// The shared pool this window belongs to, when the provider reports pools
    /// instead of per-model quotas (Antigravity: Gemini vs Claude/GPT). The
    /// frontend must not guess this from the model name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
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
    /// Model id → shared pool key, for providers that report pools instead of
    /// per-model quotas. Lets the frontend pick a model's windows without
    /// guessing from the model name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_pools: BTreeMap<String, String>,
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
            pool: None,
        });
    }
}
/// A window that belongs to a shared pool rather than to one model.
fn pool_window(
    out: &mut Vec<ProviderUsageWindow>,
    label: &str,
    pct: Option<f64>,
    reset: Option<String>,
    pool: &str,
) {
    if let Some(used_percent) = pct {
        out.push(ProviderUsageWindow {
            label: label.into(),
            used_percent,
            resets_at: reset,
            model_id: None,
            pool: Some(pool.into()),
        });
    }
}
/// Map each Antigravity model id to the shared pool its quota comes from.
/// Upstream names the provider per model, so this never depends on the model id
/// containing "gemini". A model without `apiProvider` is left out rather than
/// guessed: an unmapped model shows the account-wide list instead of a
/// possibly wrong pool.
fn antigravity_model_pools(data: &Value) -> BTreeMap<String, String> {
    let mut pools = BTreeMap::new();
    for (id, entry) in data["models"].as_object().into_iter().flatten() {
        let pool = match entry["apiProvider"].as_str() {
            Some("API_PROVIDER_GOOGLE_GEMINI" | "API_PROVIDER_INTERNAL") => "gemini",
            Some("API_PROVIDER_ANTHROPIC_VERTEX" | "API_PROVIDER_OPENAI_VERTEX") => "claude-gpt",
            // Unknown or missing provider: not guessed into a pool.
            _ => continue,
        };
        pools.insert(id.clone(), pool.into());
    }
    pools
}
/// Record which shared pool each model draws from.
///
/// This never fills windows. Per-model `quotaInfo` carries a reset time but no
/// window length, so a bar built from it cannot say whether it is the 5-hour or
/// the weekly limit — exactly the ambiguity this change set removed. Windows
/// come only from `retrieveUserQuotaSummary`; when that fails the read stays
/// failed instead of inventing a window.
fn apply_antigravity_models(result: &mut ProviderAccountUsage, models: &Value) {
    result.model_pools = antigravity_model_pools(models);
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
            "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary",
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
        // The endpoint is only the host's marker: usage is read by `fetch_opendesign_usage`.
        "opendesign" | "open-design" => (
            "https://amr-api.open-design.ai/api/v1/workspaces/current",
            &["open-design.ai"],
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
        model_pools: BTreeMap::new(),
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
            // Antigravity の使用枠はモデル単位ではなく、Gemini 系と
            // Claude / GPT-OSS 系の2系統で共有されている。上流も
            // retrieveUserQuotaSummary で「系統 × 5時間/週」の4枠を返すので、
            // モデルごとに増やさず、その4枠だけを表示する。
            // 同じ系統が複数グループに分かれて返ることがある。系統+期間ごとに
            // 1枠へまとめ、いちばん減っている値を採る(残量を多く見せない)。
            let mut pools: Vec<(String, String)> = Vec::new();
            let mut cells: Vec<(String, u8, String, f64, Option<String>)> = Vec::new();
            for group in data["groups"].as_array().into_iter().flatten() {
                // 系統名は上流の表示名から決める。未知の系統が増えたときに
                // Claude/GPT へ混ぜると、別の枠を同じ枠として見せてしまう。
                let name = group["displayName"].as_str().unwrap_or_default();
                let lower = name.to_ascii_lowercase();
                // `bucketId` is the stable machine name (`gemini-5h`, `3p-5h`)
                // while `displayName` is prose that upstream may reword. The id
                // wins; the display name is the fallback, so a wording change
                // cannot silently merge two pools.
                let ids = group["buckets"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|bucket| bucket["bucketId"].as_str())
                    .map(str::to_ascii_lowercase)
                    .collect::<Vec<_>>();
                let id_says = |needle: &str| ids.iter().any(|id| id.starts_with(needle));
                // The key is what the frontend matches models against, so it
                // stays stable even if the display name is reworded upstream.
                // `displayName` is consulted only when the group carries no id
                // at all: once ids exist they are authoritative, and a prose
                // mention of "Gemini" must not pull an unknown id into that
                // pool. Otherwise a future third pool would merge into Gemini.
                let no_ids = ids.is_empty();
                let group_key = if id_says("gemini") || (no_ids && lower.contains("gemini")) {
                    "gemini".to_string()
                } else if id_says("3p")
                    || (no_ids && (lower.contains("claude") || lower.contains("gpt")))
                {
                    "claude-gpt".to_string()
                } else if !no_ids {
                    // Ids exist but none is recognised: keep them apart.
                    format!("group:{}", ids[0])
                } else if !name.trim().is_empty() {
                    // Each unknown group keeps its own key so two of them can
                    // never be mistaken for the same pool.
                    format!("group:{}", name.trim())
                } else {
                    continue;
                };
                let title_for = |key: &str| match key {
                    "gemini" => "Gemini".to_string(),
                    "claude-gpt" => "Claude/GPT".to_string(),
                    // An unknown pool shows the group's own name, not its key.
                    _ if !name.trim().is_empty() => name.trim().to_string(),
                    other => other.strip_prefix("group:").unwrap_or(other).to_string(),
                };
                // 上流は週枠を先に返すことがあるため、表示順は 5時間→週で固定する。
                for bucket in group["buckets"].as_array().into_iter().flatten() {
                    let (rank, period) = match bucket["window"].as_str().unwrap_or("") {
                        "5h" => (0, "5h"),
                        "weekly" => (1, "7d"),
                        "" => continue,
                        other => (2, other),
                    };
                    let Some(remaining) =
                        numeric(&bucket["remainingFraction"]).filter(|n| (0.0..=1.0).contains(n))
                    else {
                        continue;
                    };
                    // A bucket's own id is the most precise signal. Deriving the
                    // key per bucket means a group that ever mixes pools cannot
                    // assign one family's quota to the other.
                    let bucket_id = bucket["bucketId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    let key = if bucket_id.starts_with("gemini") {
                        "gemini".to_string()
                    } else if bucket_id.starts_with("3p") {
                        "claude-gpt".to_string()
                    } else {
                        group_key.clone()
                    };
                    if !pools.iter().any(|(existing, _)| existing == &key) {
                        pools.push((key.clone(), title_for(&key)));
                    }
                    let used = (1.0 - remaining) * 100.0;
                    let reset = timestamp(&bucket["resetTime"], false);
                    match cells
                        .iter_mut()
                        .find(|(pool, _, window, ..)| pool == &key && window == period)
                    {
                        Some(cell) if used > cell.3 => {
                            cell.3 = used;
                            cell.4 = reset;
                        }
                        Some(_) => {}
                        None => cells.push((key.clone(), rank, period.to_string(), used, reset)),
                    }
                }
            }
            // Keep a stable order regardless of how upstream groups the pools:
            // Gemini, then Claude/GPT, then any unknown group in first-seen order.
            pools.sort_by_key(|(key, _)| match key.as_str() {
                "gemini" => 0,
                "claude-gpt" => 1,
                _ => 2,
            });
            for (key, title) in pools {
                let mut rows: Vec<_> = cells.iter().filter(|cell| cell.0 == key).collect();
                rows.sort_by_key(|cell| cell.1);
                for (_, _, period, used, reset) in rows {
                    pool_window(
                        &mut out.windows,
                        &format!("{title} {period}"),
                        Some(*used),
                        reset.clone(),
                        &key,
                    );
                }
            }
            // No model-level fallback here: this response is the pool summary
            // and carries no `models`. Windows come only from this endpoint;
            // `apply_antigravity_models` adds the model→pool map, never bars.
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
    if matches!(provider.id.as_str(), "opendesign" | "open-design") {
        return fetch_opendesign_usage(provider, result).await;
    }
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
    // Sent concurrently with the summary so the extra read adds no latency.
    let mut model_list: Option<reqwest::RequestBuilder> = None;
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
                        .clone()
                        .map(|project| json!({"project":project}))
                        .unwrap_or(json!({})),
                );
            if agy {
                // The summary reports pools, not which models are in them. Read
                // the model list once so the UI can pick a model's pool without
                // guessing from its name.
                model_list = Some(
                    client
                    .post("https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels")
                    .bearer_auth(token)
                    .header("user-agent", crate::oauth::antigravity_user_agent())
                    .json(
                        &project
                            .clone()
                            .map(|project| json!({"project":project}))
                            .unwrap_or(json!({})),
                    ),
                );
            }
        }
        _ => {}
    }
    let mut ttl = 300_000;
    let (sent, model_list, mistral_account) = if provider.id == "mistral" {
        let (sent, reads) = tokio::join!(req.send(), mistral_account_reads(&client, token));
        (sent, None, Some(reads))
    } else if let Some(models) = model_list {
        let (sent, models) = tokio::join!(req.send(), models.send());
        (sent, models.ok(), None)
    } else {
        (req.send().await, None, None)
    };
    let model_list = match model_list {
        Some(response) if response.status().is_success() => read_json(response).await.ok(),
        _ => None,
    };
    match sent {
        Ok(response) => {
            if response.status().is_success() {
                match read_json(response).await {
                    Ok(data) => {
                        parse(&provider.id, &data, &mut result);
                        if let Some(models) = model_list {
                            apply_antigravity_models(&mut result, &models);
                        }
                    }
                    Err(_) => result.error = Some("使用量APIの応答を解析できません".into()),
                }
            } else {
                let status = response.status().as_u16();
                result.error = Some(if status == 401 || status == 403 {
                    format!("HTTP {status}: 再ログインまたは使用量の参照権限が必要です")
                } else {
                    format!("使用量API: HTTP {status}")
                });
                // The summary failed but the model list may have succeeded; the
                // pool map is still worth keeping for the model rows.
                if let Some(models) = model_list.as_ref() {
                    apply_antigravity_models(&mut result, models);
                }
                if status == 429 {
                    ttl = response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<i64>().ok())
                        .unwrap_or(600)
                        .clamp(600, 3600)
                        * 1000;
                    // The previous good read is more useful than an empty one,
                    // but it must stay internally consistent: its windows and
                    // its pool map come from the same response, so the fresh map
                    // is not merged into it.
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
                Some("使用量APIへの接続に失敗しました（タイムアウトまたは通信エラー）".into());
            if let Some(models) = model_list.as_ref() {
                apply_antigravity_models(&mut result, models);
            }
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

const OPENDESIGN_API: &str = "https://amr-api.open-design.ai";

/// OpenDesign bills per workspace, and only the account's control key (kept next
/// to the model key in the login) can read it. A plain API key has no way to.
async fn fetch_opendesign_usage(
    provider: &ProviderDefinition,
    mut result: ProviderAccountUsage,
) -> ProviderAccountUsage {
    let control = provider
        .credential
        .reference
        .as_deref()
        .filter(|_| provider.credential.source == CredentialSource::OAuth)
        .and_then(|key| {
            crate::oauth::stored_session(&crate::oauth::auth_store_path(), key)
                .ok()
                .flatten()
        })
        .map(|session| session.refresh)
        .filter(|key| key.starts_with("vela_ctrl_"));
    let Some(control) = control else {
        result.status = "unsupported".into();
        result.error = Some(
            "OpenDesign の使用量はログインしたアカウントだけ表示できます。接続の「OpenDesign でログイン」を使ってください"
                .into(),
        );
        return result;
    };
    let key = format!(
        "{:x}",
        Sha256::digest(format!("opendesign|{:?}|{}", result.account_id, control))
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
        Ok(client) => client,
        Err(_) => {
            result.error = Some("使用量APIへの接続を準備できません".into());
            return result;
        }
    };
    match read_opendesign(&client, &control).await {
        Ok((preflight, workspace_id)) => {
            parse_opendesign_preflight(&preflight, &workspace_id, &mut result)
        }
        Err(error) => result.error = Some(error),
    }
    let ttl = if result.status == "ok" {
        300_000
    } else {
        60_000
    };
    *guard = Some(Cached {
        until: now_ms() + ttl,
        value: result.clone(),
    });
    result
}

async fn opendesign_get(
    client: &reqwest::Client,
    control: &str,
    path: &str,
    workspace_id: Option<&str>,
) -> Result<Value, String> {
    let mut request = client
        .get(format!("{OPENDESIGN_API}{path}"))
        .header("accept", "application/json")
        .bearer_auth(control);
    if let Some(id) = workspace_id {
        request = request.header("x-vela-workspace-id", id);
    }
    let response = request.send().await.map_err(|_| {
        "使用量APIへの接続に失敗しました（タイムアウトまたは通信エラー）".to_string()
    })?;
    let status = response.status().as_u16();
    if matches!(status, 401 | 403) {
        return Err(format!(
            "HTTP {status}: OpenDesign のログインが失効しています。再ログインしてください"
        ));
    }
    if !(200..300).contains(&status) {
        return Err(format!("使用量API: HTTP {status}"));
    }
    read_json(response)
        .await
        .map_err(|_| "使用量APIの応答を解析できません".to_string())
}

/// The workspace the key belongs to, then that workspace's plan windows and wallet.
async fn read_opendesign(
    client: &reqwest::Client,
    control: &str,
) -> Result<(Value, String), String> {
    let current = opendesign_get(client, control, "/api/v1/workspaces/current", None).await?;
    let workspace_id = opendesign_workspace_id(&current)
        .ok_or_else(|| "OpenDesign のワークスペースを特定できません".to_string())?;
    let encoded: String = url::form_urlencoded::byte_serialize(workspace_id.as_bytes()).collect();
    let preflight = opendesign_get(
        client,
        control,
        &format!("/api/v1/workspaces/{encoded}/billing/preflight"),
        Some(&workspace_id),
    )
    .await?;
    Ok((preflight, workspace_id))
}

fn opendesign_workspace_id(current: &Value) -> Option<String> {
    [
        &current["workspaceId"],
        &current["activeWorkspaceId"],
        &current["workspace"]["workspaceId"],
        &current["workspace"]["id"],
        &current["id"],
    ]
    .into_iter()
    .find_map(|value| value.as_str())
    .map(str::trim)
    .filter(|id| !id.is_empty())
    .map(str::to_owned)
}

/// Credits arrive as digit strings that can exceed an f64's exact range, so they
/// are compared as parsed floats only after the shape is checked.
fn credit_amount(value: &Value) -> Option<f64> {
    let text = value.as_str()?;
    (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse::<f64>().ok())
        .flatten()
        .filter(|n| n.is_finite())
}

fn opendesign_window_label(seconds: i64) -> String {
    match seconds {
        18_000 => "5h".into(),
        604_800 => "7d".into(),
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3_600 == 0 => format!("{}h", s / 3_600),
        s => format!("{}m", s / 60),
    }
}

fn parse_opendesign_preflight(data: &Value, workspace_id: &str, out: &mut ProviderAccountUsage) {
    // A body naming another workspace is not this account's usage.
    if data["workspaceId"]
        .as_str()
        .is_some_and(|id| id != workspace_id)
    {
        out.error = Some("OpenDesign の応答が別のワークスペースのものでした".into());
        return;
    }
    let plan = &data["codingPlan"];
    out.plan = plan["tier"].as_str().map(|tier| {
        let mut chars = tier.chars();
        chars
            .next()
            .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
            .unwrap_or_default()
    });
    let mut windows = plan["windows"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|w| {
            let seconds = w["durationSeconds"].as_i64().filter(|s| *s > 0)?;
            let limit = credit_amount(&w["limitCredits"]).filter(|n| *n > 0.0)?;
            let used = credit_amount(&w["usedCredits"])?;
            Some((
                seconds,
                used,
                limit,
                w["resetsAt"].as_str().map(str::to_owned),
            ))
        })
        .collect::<Vec<_>>();
    windows.sort_by_key(|(seconds, ..)| *seconds);
    for (seconds, used, limit, resets_at) in windows {
        window(
            &mut out.windows,
            &opendesign_window_label(seconds),
            Some((used / limit * 100.0).clamp(0.0, 100.0)),
            resets_at.filter(|value| !value.trim().is_empty()),
            None,
        );
    }
    if let Some(balance) = data["balanceUsd"]
        .as_str()
        .and_then(|text| text.parse::<f64>().ok())
        .filter(|n| n.is_finite())
    {
        out.balance = Some(format!("{balance:.4} USD (wallet)"));
    }
    if !out.windows.is_empty() || out.balance.is_some() {
        out.status = "ok".into();
        out.observed_at_ms = Some(now_ms());
        out.error = None;
    } else if plan["eligible"] == json!(false) {
        out.error = Some("有効なOpenDesignのサブスクリプションがありません".into());
    } else {
        out.error = Some(NO_USAGE_DATA.into());
    }
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
mod tests;
