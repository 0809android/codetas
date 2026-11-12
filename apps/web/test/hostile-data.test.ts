import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";

// Bundle the browser modules exactly as Vite does; Node's native TS runner
// does not resolve their extensionless imports.
const bundle = await build({
  stdin: {
    contents: 'export * from "./views"; export { state } from "./state"; export { setLanguage } from "./i18n";',
    resolveDir: fileURLToPath(new URL("../src", import.meta.url)),
    loader: "ts",
  },
  bundle: true, write: false, platform: "node", format: "esm", logLevel: "silent",
});
const { state, renderView, renderOverview, renderProviderEditor, setLanguage } =
  await import("data:text/javascript," + encodeURIComponent(bundle.outputFiles[0]!.text));
setLanguage("ja");

// Provider names, model ids, upstream error bodies and backend messages all reach
// the management window as data. Every one of them must arrive escaped: the
// window's IPC can rewrite the gateway configuration.
const PAYLOAD = `"'><img src=x onerror=alert(1)>`;
const RAW_TAG = "<img src=x";
const ESCAPED_TAG = "&lt;img src=x";

// Enum-like fields steer rendering and would only hide sections if tainted.
const ENUM_KEYS = new Set([
  "protocol", "transport", "source", "googleMode", "strategy", "channel", "kind", "mode", "status", "view",
  "anthropicCacheTtl", "reasoningEffort", "imageInputMode", "videoInputMode", "documentInputMode", "apiKeyEnv",
]);

function taint(value: unknown, key: string): unknown {
  if (typeof value === "string") return ENUM_KEYS.has(key) ? value : value + PAYLOAD;
  if (Array.isArray(value)) return value.map((item) => taint(item, key));
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.entries(value).map(([name, item]) => [name, taint(item, name)]));
  }
  return value;
}

function hostileState(): void {
  const fixture = JSON.parse(readFileSync(new URL("./fixtures/gateway.json", import.meta.url), "utf8"));
  state.configuration = taint(fixture.configuration, "");
  state.status = taint(fixture.status, "");
  const base = state.configuration.providers[0];
  for (const [suffix, source] of [["oauth", "oAuth"], ["cmd", "command"], ["fwd", "forward"], ["kc", "keychain"]]) {
    const extra = structuredClone(base);
    extra.id = `${base.id}-${suffix}`;
    extra.credential = {
      ...extra.credential, source, reference: `ref${PAYLOAD}`,
      command: source === "command"
        ? { program: `/bin/x${PAYLOAD}`, args: [PAYLOAD], cwd: null, timeoutMs: 1000, refreshIntervalMs: 0 }
        : null,
    };
    state.configuration.providers.push(extra);
  }
  const providerId = base.id;
  const event = (n: number) => ({
    ledgerSequence: n, timestampMs: Date.now() - n * 1000, requestId: `req${n}${PAYLOAD}`, providerId,
    upstreamModel: `up${PAYLOAD}`, upstreamEndpoint: `https://e.invalid/${PAYLOAD}`, exposedModel: `${providerId}/m${PAYLOAD}`,
    routeId: `route${PAYLOAD}`, accountId: `acct${PAYLOAD}`, statusCode: n % 2 ? 429 : 500, outcome: "failure",
    failureCategory: `cat${PAYLOAD}`, latencyMs: 10, attempts: 2, candidateOrdinal: 0, sendCount: 1,
    recoveryKinds: [`kind${PAYLOAD}`], recoveryKind: `kind${PAYLOAD}`, attemptOnly: false, shadow: false,
    clientThreadId: `thread${PAYLOAD}`, totalTokens: 10, inputTokens: 5, outputTokens: 5, cachedInputTokens: 0,
    usage: { inputTokens: 5, outputTokens: 5, totalTokens: 10, cachedInputTokens: 0, reasoningTokens: 0 },
    upstreamError: { statusCode: 429, errorMessage: PAYLOAD, errorType: PAYLOAD, errorCode: PAYLOAD, retryAfter: PAYLOAD },
  });
  state.recentEvents = [event(1), event(2), event(3)];
  state.recentEventsError = PAYLOAD;
  state.sessionTitles = { [`thread${PAYLOAD}`]: PAYLOAD };
  state.diagnostics = { checks: [{ id: `check${PAYLOAD}`, level: "error", summary: PAYLOAD, remediation: PAYLOAD }], passed: 0, warnings: 0, errors: 1 };
  state.routeDryRuns = [{ requestedModel: PAYLOAD, selected: PAYLOAD, candidates: [{ rank: 1, target: PAYLOAD, accountId: PAYLOAD, eligible: false, healthPercent: 50, score: 1, reasons: [PAYLOAD] }] }];
  state.providerAccounts = { [providerId]: [{ accountId: `a${PAYLOAD}`, label: PAYLOAD, email: PAYLOAD, storeKey: PAYLOAD, main: true, enabled: true, expiresAtMs: Date.now() + 1e6, source: PAYLOAD }] };
  state.observability = { totalRequests: 3, successfulRequests: 1, failedRequests: 2, inputTokens: 15, outputTokens: 15, totalTokens: 30, storageBytes: 0, persistenceError: PAYLOAD };
  state.breakdown = {
    scannedEvents: 3, truncated: false, models: [], surfaces: [], daily: [],
    providers: [{ key: PAYLOAD, providerId: PAYLOAD, requests: 1, successfulRequests: 1, failedRequests: 0, inputTokens: 1, outputTokens: 1, cachedInputTokens: 0, reasoningTokens: 0, totalTokens: 2, estimatedCostUsd: 0, totalLatencyMs: 1, maxLatencyMs: 1, measuredRequests: 1, costedRequests: 1 }],
  };
  state.codexPluginStatus = { installed: true, enabled: true, mcpHealthy: false, gatewayConnected: false, gatewayReachable: false, message: PAYLOAD };
}

function assertEscaped(label: string, html: string): void {
  const raw = html.indexOf(RAW_TAG);
  assert.equal(raw, -1, `${label} rendered a raw tag: …${html.slice(Math.max(0, raw - 120), raw + 40)}`);
}

test("every page escapes hostile configuration, log and backend text", () => {
  hostileState();
  let rendered = 0;
  for (const view of ["connections", "status", "settings"]) {
    state.view = view;
    const html = renderView();
    assertEscaped(view, html);
    rendered += html.split(ESCAPED_TAG).length - 1;
  }
  const overview = renderOverview();
  assertEscaped("overview", overview);
  assert.ok(overview.includes(ESCAPED_TAG), "the overview must actually show the hostile persistence error");
  // Guard against a fixture that silently stops reaching the renderers.
  assert.ok(rendered > 50, `only ${rendered} hostile strings reached the pages`);
});

test("the provider editor escapes hostile provider fields", () => {
  hostileState();
  for (const provider of state.configuration.providers) {
    state.editingProviderId = provider.id;
    const html = renderProviderEditor();
    assertEscaped(`editor for ${provider.id}`, html);
    assert.ok(html.includes(ESCAPED_TAG));
  }
  state.editingProviderId = null;
});
