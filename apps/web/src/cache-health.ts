import type { CacheHealthSnapshot, SessionCacheHealth } from "@codetas/core";

const escape = (value: unknown): string => String(value ?? "").replace(/[&<>"']/g,
  (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]!));
const number = (value: number): string => new Intl.NumberFormat("ja-JP").format(value);
const percent = (value: number | null): string => value == null ? "未報告" : `${value.toFixed(1)}%`;
const outstanding = (snapshot: CacheHealthSnapshot): SessionCacheHealth[] => snapshot.sessions.filter((s) => s.activeAlert);
const active = (snapshot: CacheHealthSnapshot): SessionCacheHealth[] => outstanding(snapshot).filter((s) =>
  Date.now() - s.lastEventAtMs <= 30 * 60_000);

export class CacheAlertCursor {
  monitorId = "";
  sequence = 0;
  constructor(saved?: { monitorId: string; sequence: number }) {
    if (saved && typeof saved.monitorId === "string" && Number.isSafeInteger(saved.sequence) && saved.sequence >= 0) {
      this.monitorId = saved.monitorId; this.sequence = saved.sequence;
    }
  }
  take(snapshot: CacheHealthSnapshot): SessionCacheHealth[] {
    if (!snapshot.monitoring) return [];
    if (snapshot.monitorId !== this.monitorId) { this.monitorId = snapshot.monitorId; this.sequence = 0; }
    const rows = active(snapshot).filter((row) => row.alertSequence > this.sequence);
    this.sequence = Math.max(this.sequence, ...snapshot.sessions.map((s) => s.alertSequence));
    return rows;
  }
}

export function renderCacheHealthBanner(snapshot?: CacheHealthSnapshot | null, error?: string | null,
  enabled = true): string {
  if (error) return `<div class="notice error" role="status">キャッシュ監視の更新に失敗しました。状態は未確認です：${escape(error)}</div>`;
  if (snapshot?.persistenceError) return `<div class="notice error" role="status">キャッシュ監視データの保存・取得に問題があります：${escape(snapshot.persistenceError)}</div>`;
  if (!enabled || snapshot?.monitoring === false) return `<div class="cache-health-idle">キャッシュ監視停止中（使用量ログが無効）</div>`;
  if (!snapshot) return `<div class="cache-health-idle">キャッシュ監視：集計データを取得中</div>`;
  const alerts = outstanding(snapshot);
  if (!alerts.length) return `<div class="cache-health-idle">キャッシュ監視有効 · ${number(snapshot.sessions.length)}セッション区分 · 警告なし</div>`;
  return `<div class="notice error cache-health-warning" role="alert"><strong>キャッシュ効率の警告・回復未確認：${number(alerts.length)}セッション</strong>
    <span>${alerts.slice(0, 3).map((row) => `${escape(row.model)}（${escape(row.sessionId.slice(0, 8))}）`).join("、")}
    — 「状態・問題」のセッション別集計を確認してください。</span></div>`;
}

export function renderCacheHealthPanel(snapshot?: CacheHealthSnapshot | null): string {
  if (!snapshot) return `<section class="panel" id="cache-health-panel"><h2>セッション別キャッシュ監視</h2><p>集計データを取得中です。</p></section>`;
  const p = snapshot.policy;
  return `<section class="panel cache-health-panel" id="cache-health-panel"><header><h2>セッション別キャッシュ監視</h2>
      <span>${snapshot.monitoring ? "常時集計" : "監視停止中"}</span></header>
    <p class="evidence">入力${number(p.minInputTokens)}トークン以上のキャッシュ報告済み成功応答の直近${p.windowRequests}回で、加重ヒット率が${p.thresholdPercent}%以下で警告。
      ${p.recoveryPercent}%以上で回復扱い。同じ状態では再通知せず、再通知間隔は最低${Math.round(p.cooldownMs / 60_000)}分です。</p>
    <p class="evidence">率＝キャッシュ再利用トークン÷入力トークン。未報告は0%にせず分離します。
      同じチャットでも接続先・モデル・アカウント別に集計します。保存範囲：直近${snapshot.retentionDays}日・最大${snapshot.maxSessions}区分。</p>
    ${snapshot.persistenceError ? `<p class="notice error">${escape(snapshot.persistenceError)}</p>` : ""}
    <div class="cache-health-scroll" id="cache-health-scroll" tabindex="0" aria-label="セッション別キャッシュ集計"><table class="cache-health-table"><thead><tr><th>セッション／モデル</th><th>累積ヒット率（報告分）</th><th>直近ヒット率</th><th>成功応答</th><th>最新入力</th><th>状態</th></tr></thead><tbody>
    ${snapshot.sessions.map((row) => `<tr class="${row.activeAlert ? "cache-health-low" : ""}">
      <td><code title="${escape(row.sessionId)}">${escape(row.sessionId.slice(0, 8))}</code><br>${escape(row.model)}<small>${escape(row.providerId)}${row.accountId ? ` ／ ${escape(row.accountId)}` : ""}</small></td>
      <td>${percent(row.cacheHitPercent)}<small>未報告 ${number(row.unknownCacheRequests)}回</small></td>
      <td>${percent(row.rollingHitPercent)}<small>${row.rollingRequests} / ${p.windowRequests}回</small></td>
      <td>${number(row.successfulRequests)}</td><td>${row.lastInputTokens > 0 ? `${number(row.lastInputTokens)} tokens` : "未報告"}</td>
      <td>${row.activeAlert ? row.rollingRequests < p.windowRequests || row.rollingHitPercent == null ? "回復未確認" : "効率低下・未回復" : row.rollingRequests >= p.windowRequests ? "監視中" : row.measuredRequests ? "サンプル収集中" : "キャッシュ値未報告"}
      <small>${escape(new Date(row.lastEventAtMs).toLocaleString("ja-JP"))}</small></td></tr>`).join("") || `<tr><td colspan="6">まだセッションID付きの使用量記録がありません。設定確認だけでは推論を送りません。</td></tr>`}
    </tbody></table></div></section>`;
}

const panelRenderings = new WeakMap<HTMLElement, string>();

/** Keep a reader's position when live usage changes, and retain unchanged DOM. */
export function updateCacheHealthPanel(panel: HTMLElement, snapshot?: CacheHealthSnapshot | null): HTMLElement {
  const html = renderCacheHealthPanel(snapshot);
  if (panelRenderings.get(panel) === html) return panel;
  const scroll = panel.querySelector<HTMLElement>(".cache-health-scroll");
  const focused = scroll != null && panel.ownerDocument.activeElement === scroll;
  const top = scroll?.scrollTop ?? 0;
  const left = scroll?.scrollLeft ?? 0;
  const template = panel.ownerDocument.createElement("template");
  template.innerHTML = html;
  const next = template.content.firstElementChild as HTMLElement;
  panel.replaceWith(next);
  const restored = next.querySelector<HTMLElement>(".cache-health-scroll");
  if (restored) {
    restored.scrollTop = top; restored.scrollLeft = left;
    if (focused) restored.focus({ preventScroll: true });
  }
  panelRenderings.set(next, html);
  return next;
}

/** Reads only the local status command; never generates requests to a model. */
export function startCacheHealthPolling(fetchSummary: () => Promise<CacheHealthSnapshot>,
  update: (summary: CacheHealthSnapshot | null, error: string | null) => void,
  alert: (rows: SessionCacheHealth[]) => void,
  canNotify: () => boolean = () => true): () => void {
  const key = "codetas-cache-alert-cursor-v1";
  let saved: { monitorId: string; sequence: number } | undefined;
  try { saved = JSON.parse(localStorage.getItem(key) ?? "null") ?? undefined; } catch { /* unavailable storage */ }
  const cursor = new CacheAlertCursor(saved);
  let stopped = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let latest: CacheHealthSnapshot | undefined;
  const poll = async (): Promise<void> => {
    try {
      const summary = await fetchSummary();
      if (stopped) return;
      latest = summary; update(summary, null);
      if (canNotify()) {
        const before = { monitorId: cursor.monitorId, sequence: cursor.sequence };
        const rows = cursor.take(summary);
        try { if (rows.length) alert(rows); }
        catch (error) {
          cursor.monitorId = before.monitorId; cursor.sequence = before.sequence;
          throw error;
        }
        try { localStorage.setItem(key, JSON.stringify({ monitorId: cursor.monitorId, sequence: cursor.sequence })); } catch { /* no persisted cursor */ }
      }
    } catch (error) {
      if (!stopped) update(latest ?? null, error instanceof Error ? error.message : String(error));
    } finally { if (!stopped) timer = setTimeout(() => { void poll(); }, 5_000); }
  };
  void poll();
  return () => { stopped = true; if (timer != null) clearTimeout(timer); };
}
