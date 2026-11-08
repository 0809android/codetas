import type { GatewayConfiguration, ObservabilityBreakdown, ProviderOAuthAccount, ProviderAccountUsage } from "@codetas/core";
import { h } from "./format";
import { t } from "./i18n";

export function usageAccountLabels(
  config: GatewayConfiguration, providerId: string, accounts: ProviderOAuthAccount[], snapshot: ObservabilityBreakdown | null,
): Map<string | null, string> {
  const labels = new Map<string | null, string>();
  for (const account of accounts) labels.set(account.accountId, account.label);
  for (const account of config.accountPool?.accounts ?? []) {
    if (account.providerId === providerId && !labels.has(account.id)) labels.set(account.id, account.label || account.id);
  }
  for (const row of [...(snapshot?.modelAccounts ?? []), ...(snapshot?.quotas ?? [])]) {
    if (row.providerId === providerId && !labels.has(row.accountId)) {
      labels.set(row.accountId, row.accountId === null ? t("usageInline.unattributed") : t("usageInline.pastAccount", { id: row.accountId }));
    }
  }
  if (!labels.size) labels.set(null, t("usageInline.defaultCredential"));
  return labels;
}

/**
 * The pool whose quota a model draws from, as reported by the gateway.
 * The UI lists family ids (`claude-opus-5-5`) while the quota API reports
 * effort variants (`claude-opus-5-5-high`), so fall back to a prefix match.
 * Several ids are tried in order (wire id, then the displayed id) because a
 * configured wire id can be absent from the upstream list.
 *
 * A preset model can be absent upstream entirely (`gemini-3.7-flash`), so the
 * last resort uses the vendor's own family token. This decides which pool to
 * LOOK FOR, never which number to show: if that pool was not reported, the row
 * says so instead of borrowing the other family's bars.
 */
function poolForModel(record: ProviderAccountUsage, modelIds: string[]): string | null {
  const pools = record.modelPools;
  if (pools) {
    for (const modelId of modelIds) {
      if (!modelId) continue;
      if (pools[modelId]) return pools[modelId];
      const matches = new Set(Object.entries(pools)
        .filter(([id]) => id.startsWith(modelId + "-"))
        .map(([, pool]) => pool));
      if (matches.size === 1) return [...matches][0]!;
      // Ambiguous here; a later (more specific) id may still resolve it.
    }
  }
  // Last resort: the family token upstream uses for these two pools.
  for (const modelId of modelIds) {
    if (!modelId) continue;
    const family = /gemini/i.test(modelId) ? "gemini" : /claude|gpt/i.test(modelId) ? "claude-gpt" : null;
    if (family) return family;
  }
  return null;
}

/** Provider-side quota includes use outside CODETAS, unlike the log totals. */
export function renderProviderUsage(
  usage: ProviderAccountUsage[] | null | undefined, providerId: string, accountId: string | null, modelId?: string,
  displayModelId?: string,
): string {
  const record = usage?.find(row => row.providerId === providerId && row.accountId === accountId);
  const title = `<strong class="provider-usage-title">${h(t("usageDirect.title"))}</strong>`;
  if (!record) return `<div class="provider-usage">${title}<span class="usage-unavailable">${h(t(usage == null ? "usageDirect.loading" : "usageDirect.unavailable"))}</span></div>`;
  const fresh = record.observedAtMs !== null && Date.now() - record.observedAtMs < 10 * 60_000;
  const stale = record.stale || !fresh;
  // A window tagged with a pool is a shared account window even if a provider
  // also fills in `modelId`, so it is never narrowed away by the model filter.
  const applicable = record.windows.filter(w => !modelId || w.pool !== undefined
    || w.modelId === null || w.modelId === modelId
    || (["claude-opus", "claude-sonnet"].includes(w.modelId) && modelId.startsWith(w.modelId + "-")));
  // Antigravity shares one pool per model family, so a Gemini model does not
  // need the Claude/GPT bars next to it (and the reverse). The gateway reports
  // which pool each model belongs to; never guess it from the model name.
  const pool = providerId === "google-antigravity" && modelId
    ? poolForModel(record, [modelId, displayModelId ?? ""])
    : null;
  // Match on the pool tag alone. Requiring `modelId === null` here would drop a
  // window that carries both, which then reads as "this pool was not reported".
  const pooled = pool === null ? [] : applicable.filter(w => w.pool === pool);
  // A payload written before pools carried their key has windows but no `pool`
  // tags at all. That is missing information, not a missing pool, so fall back
  // to the account-wide list instead of claiming the pool was never reported.
  // Requiring EVERY window to be untagged matters: one tagged window proves the
  // payload carries keys, so a mixed payload must not leak the other pool.
  const legacy = applicable.length > 0 && applicable.every(w => w.pool === undefined);
  // A resolved pool with no windows means that pool was not reported (for
  // example the account has no access to it). Show nothing rather than the
  // other pool's bars, which would read as this model's remaining quota.
  const shown = pool === null || legacy ? applicable : pooled;
  // Which "no bars" message is accurate. Only claim the pool was never
  // reported when it truly produced no window; a window that exists but has an
  // unusable percentage is a different problem.
  const noBarsMessage = pool !== null && !legacy && pooled.length === 0
    ? "usageDirect.noPoolWindow"
    : "usageDirect.noModelWindow";
  const date = (value: string): string => {
    const parsed = /^\d+$/.test(value) ? new Date(Number(value)) : new Date(value);
    return Number.isNaN(parsed.getTime()) ? t("usageDirect.resetUnknown") : parsed.toLocaleString();
  };
  const bars = shown.filter(w => Number.isFinite(w.usedPercent) && w.usedPercent >= 0 && w.usedPercent <= 100).map(w => {
    const percent = Math.round(w.usedPercent * 10) / 10;
    return `<div class="provider-usage-window"><span>${h(w.label)}${w.modelId === null ? ` · ${h(t("usageDirect.shared"))}` : ""}
      <b>${percent}%</b>${stale ? ` · ${h(t("usageDirect.stale"))}` : ""}</span>
      <progress max="100" value="${percent}" aria-label="${h(w.label)}"></progress>
      ${w.resetsAt ? `<small>${h(t("usageDirect.reset", { time: date(w.resetsAt) }))}</small>` : ""}</div>`;
  }).join("");
  let source = "";
  try { source = new URL(record.source).hostname; } catch { /* unsupported */ }
  return `<div class="provider-usage${stale ? " provider-usage-stale" : ""}">${title}
    ${record.plan ? `<small>${h(record.plan)}</small>` : ""}${bars}
    ${record.balance !== null ? `<span>${h(t("usageDirect.balance"))}: <b>${h(record.balance)}</b>${stale ? ` · ${h(t("usageDirect.stale"))}` : ""}</span>` : ""}
    ${record.error ? `<span class="usage-unavailable">${h(record.error)}</span>` : ""}
    ${!bars && record.balance === null && !record.error ? `<span class="usage-unavailable">${h(t(noBarsMessage))}</span>` : ""}
    ${record.observedAtMs !== null ? `<small>${h(t("usageDirect.observed", { time: new Date(record.observedAtMs).toLocaleString() }))}${source ? ` · ${h(source)}` : ""}</small>` : ""}
  </div>`;
}

export function renderModelUsage(
  config: GatewayConfiguration, snapshot: ObservabilityBreakdown | null,
  providerId: string, modelId: string, accounts: ProviderOAuthAccount[], direct?: ProviderAccountUsage[] | null,
): string {
  const labels = usageAccountLabels(config, providerId, accounts, snapshot);
  const directModelId = config.providers?.find(provider => provider.id === providerId)?.modelWireIds?.[modelId] ?? modelId;
  const defaultDirect = direct?.some(row => row.providerId === providerId && row.accountId === null);
  const separateDefault = defaultDirect && labels.get(null) !== t("usageInline.defaultCredential");
  return `<section class="inline-model-usage" aria-label="${h(t("usageInline.title"))}">
    ${separateDefault ? `<div class="model-account-usage"><strong>${h(t("usageInline.defaultCredential"))}</strong>${renderProviderUsage(direct, providerId, null, directModelId, modelId)}</div>` : ""}
    ${[...labels].map(([accountId, label]) => `<div class="model-account-usage"><strong>${h(label)}</strong>
      ${accountId !== null || !separateDefault ? renderProviderUsage(direct, providerId, accountId, directModelId, modelId) : ""}</div>`).join("")}
  </section>`;
}

export function renderAccountUsage(
  providerId: string, accountId: string, direct?: ProviderAccountUsage[] | null,
): string {
  return `<div class="account-usage">${renderProviderUsage(direct, providerId, accountId)}</div>`;
}

export interface LowQuotaWarning { key: string; version: string; text: string; }

/** Remaining quota below this share of the window triggers a warning. */
export const LOW_QUOTA_REMAINING_PERCENT = 10;

/** Fresh, successfully read provider windows whose remaining share fell under the threshold. */
export function lowQuotaStatus(
  config: GatewayConfiguration, usage: ProviderAccountUsage[] | null | undefined,
  accountsByProvider: Record<string, ProviderOAuthAccount[]>, snapshot: ObservabilityBreakdown | null, now = Date.now(),
): { low: LowQuotaWarning[]; healthy: Set<string> } {
  const warnings: LowQuotaWarning[] = [];
  // Windows read fresh and still comfortably above the threshold: only these count
  // as "recovered". A missing or stale reading says nothing either way.
  const healthy = new Set<string>();
  for (const record of usage ?? []) {
    if (record.status !== "ok" || record.stale || record.observedAtMs === null || now - record.observedAtMs >= 10 * 60_000) continue;
    const provider = config.providers.find(item => item.id === record.providerId);
    if (!provider) continue;
    const account = record.accountId === null ? null
      : usageAccountLabels(config, record.providerId, accountsByProvider[record.providerId] ?? [], snapshot).get(record.accountId) ?? record.accountId;
    for (const window of record.windows) {
      if (!Number.isFinite(window.usedPercent) || window.usedPercent < 0 || window.usedPercent > 100) continue;
      const remaining = Math.round((100 - window.usedPercent) * 10) / 10;
      const key = ["quota", record.providerId, record.accountId ?? "", window.modelId ?? "", window.label].join("|");
      if (remaining >= LOW_QUOTA_REMAINING_PERCENT) { healthy.add(key); continue; }
      const reset = window.resetsAt === null ? null
        : new Date(/^\d+$/.test(window.resetsAt) ? Number(window.resetsAt) : window.resetsAt);
      warnings.push({
        key,
        // The reset time of some providers drifts between reads, so it must not identify a warning.
        version: "",
        text: `${provider.name}${account ? ` / ${account}` : ""} — ${window.label}：残り ${remaining}%${reset && !Number.isNaN(reset.getTime()) ? `（リセット ${reset.toLocaleString()}）` : ""}`,
      });
    }
  }
  return { low: warnings, healthy };
}

export function lowQuotaWarnings(
  config: GatewayConfiguration, usage: ProviderAccountUsage[] | null | undefined,
  accountsByProvider: Record<string, ProviderOAuthAccount[]>, snapshot: ObservabilityBreakdown | null, now = Date.now(),
): LowQuotaWarning[] {
  return lowQuotaStatus(config, usage, accountsByProvider, snapshot, now).low;
}

export function renderUsageWarningBanner(warnings: LowQuotaWarning[]): string {
  if (!warnings.length) return "";
  return `<div class="notice error usage-low-warning" role="alert"><div><strong>利用枠の残りが${LOW_QUOTA_REMAINING_PERCENT}%未満です</strong>
    <ul>${warnings.map(w => `<li><span>${h(w.text)}</span> <button class="text-button" data-action="dismiss-usage-warning" data-warning-key="${h(w.key)}" data-warning-version="${h(w.version)}" type="button">消す</button></li>`).join("")}</ul></div></div>`;
}
