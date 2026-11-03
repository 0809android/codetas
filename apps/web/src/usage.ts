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

/** Provider-side quota includes use outside CODETAS, unlike the log totals. */
export function renderProviderUsage(
  usage: ProviderAccountUsage[] | null | undefined, providerId: string, accountId: string | null, modelId?: string,
): string {
  const record = usage?.find(row => row.providerId === providerId && row.accountId === accountId);
  const title = `<strong class="provider-usage-title">${h(t("usageDirect.title"))}</strong>`;
  if (!record) return `<div class="provider-usage">${title}<span class="usage-unavailable">${h(t(usage == null ? "usageDirect.loading" : "usageDirect.unavailable"))}</span></div>`;
  const fresh = record.observedAtMs !== null && Date.now() - record.observedAtMs < 10 * 60_000;
  const stale = record.stale || !fresh;
  const applicable = record.windows.filter(w => !modelId || w.modelId === null || w.modelId === modelId
    || (["claude-opus", "claude-sonnet"].includes(w.modelId) && modelId.startsWith(w.modelId + "-"))
    || (providerId === "google-antigravity" && ["-high", "-medium", "-mid", "-low", "-extra-low"].some(suffix => w.modelId === modelId + suffix)));
  const date = (value: string): string => {
    const parsed = /^\d+$/.test(value) ? new Date(Number(value)) : new Date(value);
    return Number.isNaN(parsed.getTime()) ? t("usageDirect.resetUnknown") : parsed.toLocaleString();
  };
  const bars = applicable.filter(w => Number.isFinite(w.usedPercent) && w.usedPercent >= 0 && w.usedPercent <= 100).map(w => {
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
    ${!bars && record.balance === null && !record.error ? `<span class="usage-unavailable">${h(t("usageDirect.noModelWindow"))}</span>` : ""}
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
    ${separateDefault ? `<div class="model-account-usage"><strong>${h(t("usageInline.defaultCredential"))}</strong>${renderProviderUsage(direct, providerId, null, directModelId)}</div>` : ""}
    ${[...labels].map(([accountId, label]) => `<div class="model-account-usage"><strong>${h(label)}</strong>
      ${accountId !== null || !separateDefault ? renderProviderUsage(direct, providerId, accountId, directModelId) : ""}</div>`).join("")}
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
export function lowQuotaWarnings(
  config: GatewayConfiguration, usage: ProviderAccountUsage[] | null | undefined,
  accountsByProvider: Record<string, ProviderOAuthAccount[]>, snapshot: ObservabilityBreakdown | null, now = Date.now(),
): LowQuotaWarning[] {
  const warnings: LowQuotaWarning[] = [];
  for (const record of usage ?? []) {
    if (record.status !== "ok" || record.stale || record.observedAtMs === null || now - record.observedAtMs >= 10 * 60_000) continue;
    const provider = config.providers.find(item => item.id === record.providerId);
    if (!provider) continue;
    const account = record.accountId === null ? null
      : usageAccountLabels(config, record.providerId, accountsByProvider[record.providerId] ?? [], snapshot).get(record.accountId) ?? record.accountId;
    for (const window of record.windows) {
      if (!Number.isFinite(window.usedPercent) || window.usedPercent < 0 || window.usedPercent > 100) continue;
      const remaining = Math.round((100 - window.usedPercent) * 10) / 10;
      if (remaining >= LOW_QUOTA_REMAINING_PERCENT) continue;
      const reset = window.resetsAt === null ? null
        : new Date(/^\d+$/.test(window.resetsAt) ? Number(window.resetsAt) : window.resetsAt);
      warnings.push({
        key: ["quota", record.providerId, record.accountId ?? "", window.modelId ?? "", window.label].join("|"),
        version: window.resetsAt ?? "",
        text: `${provider.name}${account ? ` / ${account}` : ""} — ${window.label}：残り ${remaining}%${reset && !Number.isNaN(reset.getTime()) ? `（リセット ${reset.toLocaleString()}）` : ""}`,
      });
    }
  }
  return warnings;
}

export function renderUsageWarningBanner(warnings: LowQuotaWarning[]): string {
  if (!warnings.length) return "";
  return `<div class="notice error usage-low-warning" role="alert"><div><strong>利用枠の残りが${LOW_QUOTA_REMAINING_PERCENT}%未満です</strong>
    <ul>${warnings.map(w => `<li><span>${h(w.text)}</span> <button class="text-button" data-action="dismiss-usage-warning" data-warning-key="${h(w.key)}" data-warning-version="${h(w.version)}" type="button">消す</button></li>`).join("")}</ul></div></div>`;
}
