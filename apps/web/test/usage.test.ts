import assert from "node:assert/strict";
import test from "node:test";
import type { GatewayConfiguration, ModelAccountUsageRow, ObservabilityBreakdown, ProviderAccountUsage } from "@codetas/core";
import { build } from "esbuild";
import { fileURLToPath } from "node:url";
// Bundle the browser modules exactly as Vite does; Node's native TS runner
// does not resolve their extensionless imports.
const bundle = await build({
  stdin: { contents: 'export * from "./usage"; export { setLanguage } from "./i18n";',
    resolveDir: fileURLToPath(new URL("../src", import.meta.url)), loader: "ts" },
  bundle: true, write: false, platform: "node", format: "esm",
});
const { lowQuotaStatus, lowQuotaWarnings, renderUsageWarningBanner, renderModelUsage, renderAccountUsage, usageAccountLabels, renderProviderUsage, setLanguage } =
  await import("data:text/javascript," + encodeURIComponent(bundle.outputFiles[0]!.text));
setLanguage("ja");
const row = (accountId: string | null, extra: Partial<ModelAccountUsageRow> = {}): ModelAccountUsageRow => ({
  providerId: "one", modelId: "model", accountId, key: "one/model", lastEventAtMs: Date.now(),
  measuredRequests: 1, costedRequests: 1, requests: 1, successfulRequests: 1, failedRequests: 0,
  inputTokens: 100, outputTokens: 20, cachedInputTokens: 30, reasoningTokens: 0,
  totalTokens: 120, estimatedCostUsd: 0.01, totalLatencyMs: 1, maxLatencyMs: 1, ...extra,
});
const snapshot = {
  generatedAtMs: Date.now(), sinceMs: 0, truncated: false, scannedEvents: 4,
  daily: [], models: [], providers: [], surfaces: [],
  modelAccounts: [row("a"), row("b", { inputTokens: 900 }), row(null), row("a", { providerId: "two", inputTokens: 800 })],
  quotas: [{ providerId: "one", modelId: "model", accountId: "a", usedPercent: 0, observedAtMs: Date.now() }],
} satisfies ObservabilityBreakdown;
const config = { observability: { usageLog: true }, accountPool: { accounts: [
  { id: "a", providerId: "one", label: "Account A" }, { id: "b", providerId: "one", label: "<img src=x>" },
] } } as unknown as GatewayConfiguration;

test("model usage lists each account, including unassigned history, and escapes labels", () => {
  assert.equal(usageAccountLabels(config, "one", [], snapshot).get(null), "アカウント未特定");
  const model = renderModelUsage(config, snapshot, "one", "model", []);
  assert.match(model, /Account A/); assert.match(model, /アカウント未特定/);
  assert.match(model, /&lt;img src=x&gt;/); assert.doesNotMatch(model, /<img/);
  assert.doesNotMatch(model, /保持ログ|50,000|usage-local-details/);
});
const direct: ProviderAccountUsage = {
  providerId: "one", accountId: "a", status: "ok", source: "https://api.anthropic.com/api/oauth/usage",
  observedAtMs: Date.now(), checkedAtMs: Date.now(), stale: false, plan: "<img src=x>", balance: null, error: null,
  windows: [
    { label: "5h", usedPercent: 12.5, resetsAt: "2026-11-04T01:00:00Z", modelId: null },
    { label: "Opus", usedPercent: 90, resetsAt: null, modelId: "claude-opus" },
  ],
};
test("direct account data is separate from CODETAS logs and respects account/model scope", () => {
  const data = [direct, { ...direct, accountId: "b", windows: [{ ...direct.windows[0]!, usedPercent: 88 }] }];
  const html = renderProviderUsage(data, "one", "a", "claude-sonnet-5-5");
  assert.match(html, /接続先のアカウント使用量/); assert.doesNotMatch(html, /CODETAS外の利用を含む/); assert.match(html, /12.5%/); assert.match(html, /共有枠/);
  assert.match(html, /リセット予定/); assert.match(html, /api.anthropic.com/);
  assert.doesNotMatch(html, /90%|88%|<img/); assert.match(html, /&lt;img/);
  assert.match(renderProviderUsage(data, "one", "a", "claude-opus-5-5"), /90%/);
  assert.doesNotMatch(renderProviderUsage(data, "another", "a"), /12.5%/);
  const model = renderModelUsage({ ...config, observability: { ...config.observability, usageLog: false } }, snapshot, "one", "model", [], data);
  assert.match(model, /12.5%/);
});
test("direct unavailable and stale data cannot look like live zero usage", () => {
  assert.match(renderProviderUsage(null, "one", "a"), /取得待ち/);
  assert.match(renderProviderUsage([], "one", "a"), /未取得/);
  const failed = renderProviderUsage([{ ...direct, stale: true, status: "error", error: "HTTP 429" }], "one", "a");
  assert.match(failed, /最新ではありません/); assert.match(failed, /HTTP 429/);
  const unsupported = renderProviderUsage([{ ...direct, windows: [], observedAtMs: null, status: "unsupported", error: "未対応" }], "one", "a");
  assert.match(unsupported, /未対応/); assert.doesNotMatch(unsupported, /0%/);
});
test("default direct credential is not relabelled as an unidentified historical account", () => {
  const html = renderModelUsage(config, snapshot, "one", "model", [], [{ ...direct, accountId: null }]);
  assert.match(html, /接続の既定認証/); assert.match(html, /アカウント未特定/);
  assert.equal((html.match(/12.5%/g) ?? []).length, 1);
});

test("Antigravity quotas match known effort variants without merging other models", () => {
  const data = [{ ...direct, providerId: "google-antigravity", windows: [
    { label: "Argon high", usedPercent: 25, resetsAt: null, modelId: "gemini-4-argon-high" },
    { label: "Another model", usedPercent: 75, resetsAt: null, modelId: "gemini-3.8-flash-high" },
  ] }];
  const html = renderProviderUsage(data, "google-antigravity", "a", "gemini-4-argon");
  assert.match(html, /25%/); assert.doesNotMatch(html, /75%/);
});

test("low quota warns only for fresh windows with under 10% remaining", () => {
  const window = (usedPercent: number, label = "5時間") => ({ label, usedPercent, resetsAt: "1893456000000", modelId: null });
  const make = (windows: ProviderAccountUsage["windows"], extra: Partial<ProviderAccountUsage> = {}): ProviderAccountUsage[] =>
    [{ ...direct, accountId: null, windows, ...extra }];
  const withProviders = { ...config, providers: [{ id: "one", name: "One" }] } as unknown as GatewayConfiguration;
  const warn = (usage: ProviderAccountUsage[]) => lowQuotaWarnings(withProviders, usage, {}, null);
  assert.equal(warn(make([window(90)])).length, 0);
  const low = warn(make([window(90.1), window(40, "週")]));
  assert.equal(low.length, 1); assert.match(low[0]!.text, /5時間：残り 9\.9%/);
  assert.equal(warn(make([window(95)], { stale: true })).length, 0);
  assert.equal(warn(make([window(95)], { observedAtMs: Date.now() - 11 * 60_000 })).length, 0);
  assert.equal(warn(make([window(95)], { status: "error" })).length, 0);
  assert.equal(warn(make([window(Number.NaN), window(101)])).length, 0);
  assert.match(renderUsageWarningBanner(low), /data-action="dismiss-usage-warning"/);
  assert.equal(renderUsageWarningBanner([]), "");
});

test("low quota status keeps drifting reset times out of the warning identity and only counts fresh readings as recovered", () => {
  const withProviders = { ...config, providers: [{ id: "one", name: "One" }] } as unknown as GatewayConfiguration;
  const read = (usedPercent: number, resetsAt: string, extra: Partial<ProviderAccountUsage> = {}) => lowQuotaStatus(withProviders,
    [{ ...direct, accountId: null, windows: [{ label: "7d", usedPercent, resetsAt, modelId: null }], ...extra }], {}, null);
  const first = read(95, "1893456000000"), second = read(95, "1893456001234");
  assert.equal(first.low[0]!.key, second.low[0]!.key); assert.equal(first.low[0]!.version, second.low[0]!.version);
  assert.deepEqual([...read(50, "1893456000000").healthy], [first.low[0]!.key]);
  assert.equal(read(50, "1893456000000", { stale: true }).healthy.size, 0);
  assert.equal(read(50, "1893456000000", { status: "error" }).healthy.size, 0);
});
