import assert from "node:assert/strict";
import test from "node:test";
import { CacheAlertCursor, renderCacheHealthBanner, renderCacheHealthPanel, startCacheHealthPolling } from "../src/cache-health.ts";
import type { CacheHealthSnapshot, SessionCacheHealth } from "@codetas/core";

const session = (extra: Partial<SessionCacheHealth> = {}): SessionCacheHealth => ({
  sessionId: "01a13ddb-8d21-7cb2-8f00-23a8a09af079", providerId: "test", model: "test/model",
  successfulRequests: 20, measuredRequests: 20, unknownCacheRequests: 0,
  inputTokens: 2_000_000, measuredInputTokens: 2_000_000, cachedInputTokens: 1_800_000,
  lastInputTokens: 100_000, firstEventAtMs: Date.now() - 60_000, lastEventAtMs: Date.now(),
  cacheHitPercent: 90, rollingHitPercent: 90, rollingRequests: 20,
  activeAlert: true, alertSequence: 1, lastAlertAtMs: Date.now(), ...extra,
});
const snapshot = (sessions = [session()]): CacheHealthSnapshot => ({
  monitorId: "monitor", monitoring: true,
  policy: { minInputTokens: 100_000, windowRequests: 20, thresholdPercent: 90, recoveryPercent: 95, cooldownMs: 1_800_000 },
  sessions, persistenceError: null, retentionDays: 7, maxSessions: 128,
});

test("cache alerts are emitted once per persisted episode, not per poll or app launch", () => {
  const cursor = new CacheAlertCursor();
  assert.equal(cursor.take(snapshot()).length, 1);
  assert.equal(cursor.take(snapshot()).length, 0);
  const restored = new CacheAlertCursor({ monitorId: cursor.monitorId, sequence: cursor.sequence });
  assert.equal(restored.take(snapshot()).length, 0);
  assert.equal(restored.take(snapshot([session({ activeAlert: false })])).length, 0);
  assert.equal(restored.take(snapshot([session({ alertSequence: 2 })])).length, 1);
  assert.equal(restored.take({ ...snapshot(), monitorId: "new-monitor" }).length, 1);
});
test("disabled and inactive historical sessions do not create new notifications", () => {
  const cursor = new CacheAlertCursor();
  assert.equal(cursor.take({ ...snapshot(), monitoring: false }).length, 0);
  assert.equal(cursor.take(snapshot([session({ lastEventAtMs: Date.now() - 31 * 60_000 })])).length, 0);
});
test("cache alert banner escapes model identifiers and remains visible outside the status screen", () => {
  const html = renderCacheHealthBanner(snapshot([session({ model: '<img src=x onerror="alert(1)">' })]));
  assert.match(html, /role="alert"/);
  assert.match(html, /&lt;img/);
  assert.doesNotMatch(html, /<img/);
  assert.match(html, /キャッシュ効率.*警告/);
});
test("missing cache readings are displayed as unknown, never a fabricated zero percent", () => {
  const html = renderCacheHealthPanel(snapshot([session({
    cacheHitPercent: null, rollingHitPercent: null, measuredRequests: 0, unknownCacheRequests: 20,
    activeAlert: false, alertSequence: 0, rollingRequests: 0,
  })]));
  assert.match(html, /未報告/);
  assert.doesNotMatch(html, />0\.0%</);
  assert.match(html, /キャッシュ値未報告/);
});
test("cache policy and monitoring persistence failures are visible without opening debug logs", () => {
  const html = renderCacheHealthPanel({ ...snapshot(), persistenceError: "checkpoint failed" });
  assert.match(html, /100,000/); assert.match(html, /90%以下/); assert.match(html, /95%以上/);
  assert.match(html, /checkpoint failed/); assert.match(html, /最大128区分/);
});
test("poll failures are reported even before the first successful status fetch", async () => {
  let stop = () => {};
  await new Promise<void>((resolve) => {
    stop = startCacheHealthPolling(async () => { throw new Error("offline"); }, (summary, error) => {
      assert.equal(summary, null); assert.equal(error, "offline"); stop(); resolve();
    }, () => assert.fail("no alert from an unavailable status API"));
  });
});
test("stopping a pending poll prevents later updates and notifications", async () => {
  let finish!: (value: never) => void;
  let updates = 0;
  const stop = startCacheHealthPolling(() => new Promise((resolve) => { finish = resolve; }),
    () => { updates += 1; }, () => { updates += 1; });
  stop(); finish(snapshot() as never);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(updates, 0);
});

test("corrupt cache checkpoints are not presented as a healthy monitor", () => {
  const html = renderCacheHealthBanner({ ...snapshot([]), persistenceError: "unavailable" });
  assert.match(html, /問題/); assert.doesNotMatch(html, /警告なし/);
});


test("an unresolved warning is not hidden as healthy when samples become unavailable or stale", () => {
  const html = renderCacheHealthBanner(snapshot([session({ rollingRequests: 0, rollingHitPercent: null,
    lastEventAtMs: Date.now() - 31 * 60_000 })]));
  assert.match(html, /回復未確認/); assert.doesNotMatch(html, /警告なし/);
});

test("boot-time notification waits for configuration readiness without consuming its cursor", async () => {
  const original = globalThis.setTimeout;
  const originalClear = globalThis.clearTimeout;
  let next: (() => void) | undefined;
  globalThis.setTimeout = ((callback: () => void) => { next = callback; return 1; }) as unknown as typeof setTimeout;
  globalThis.clearTimeout = (() => {}) as typeof clearTimeout;
  let ready = false;
  let notified = 0;
  const stop = startCacheHealthPolling(async () => snapshot(), () => {}, rows => { notified += rows.length; }, () => ready);
  try {
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(notified, 0);
    ready = true; next!();
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(notified, 1);
    next!(); await new Promise(resolve => setImmediate(resolve));
    assert.equal(notified, 1);
  } finally { stop(); globalThis.setTimeout = original; globalThis.clearTimeout = originalClear; }
});


test("main polling and full renders use scroll-preserving cache panel paths", async () => {
  const { readFileSync } = await import("node:fs");
  const main = readFileSync(new URL("../src/main.ts", import.meta.url), "utf8");
  assert.match(main, /updateCacheHealthPanel\(panel,\s*current\)/);
  assert.doesNotMatch(main, /panel\.outerHTML\s*=/);
  assert.ok((main.match(/cache-health-scroll/g) ?? []).length >= 5,
    "full render snapshots must include the cache table's scroll container");
});


test("eligible sample policy does not require consecutive requests", () => {
  const html = renderCacheHealthPanel(snapshot());
  assert.match(html, /キャッシュ報告済み成功応答の直近20回/);
  assert.doesNotMatch(html, /20回続き/);
});
