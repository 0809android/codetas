import { invoke } from "@tauri-apps/api/core";
import { createSyncPlan } from "@codetas/core";
import type {
  ClientIntegrationReport,
  CodexRestoreReport,
  CredentialSource,
  CredentialTransport,
  ExternalClientIntegrationInput,
  GatewayConfiguration,
  GatewayDiagnosticReport,
  GatewayServiceStatus,
  GatewayStatus,
  ObservabilitySummary,
  ObservabilityBreakdown,
  ObservabilityCleanupPreview,
  ObservabilityTrashEntry,
  ObservabilityTrashReport,
  ProjectInspection,
  ProviderDefinition,
  ProviderPreset,
  ProviderConnectionReport,
  ProviderOAuthLaunchReport,
  SyncPlan,
  UpdateCheck,
} from "@codetas/core";
import "./styles.css";

type View = "overview" | "providers" | "routing" | "agents" | "projects" | "clients" | "settings";
type Notice = { tone: "success" | "error" | "info"; text: string };
type LocalCliStatus = {
  id: string;
  name: string;
  installed: boolean;
  executable: string | null;
  version: string | null;
  probeState: string;
  message: string;
  canRegister: boolean;
  needsCodetasRegistration: boolean;
  codetasProviderId: string | null;
  registrationHint: string;
};
type LocalCliScanReport = { deep: boolean; clients: LocalCliStatus[] };
type DirectApiTarget = { providerId: string; name: string; hint: string };

interface AppState {
  view: View;
  status: GatewayStatus | null;
  configuration: GatewayConfiguration | null;
  presets: ProviderPreset[];
  diagnostics: GatewayDiagnosticReport | null;
  observability: ObservabilitySummary | null;
  breakdown: ObservabilityBreakdown | null;
  cleanupPreview: ObservabilityCleanupPreview | null;
  trashEntries: ObservabilityTrashEntry[];
  service: GatewayServiceStatus | null;
  localClis: LocalCliScanReport | null;
  directApis: DirectApiTarget[];
  project: ProjectInspection | null;
  syncPlan: SyncPlan | null;
  editingProviderId: string | null;
  busy: Set<string>;
  notice: Notice | null;
}

const state: AppState = {
  view: "overview",
  status: null,
  configuration: null,
  presets: [],
  diagnostics: null,
  observability: null,
  breakdown: null,
  cleanupPreview: null,
  trashEntries: [],
  service: null,
  localClis: null,
  directApis: [],
  project: null,
  syncPlan: null,
  editingProviderId: null,
  busy: new Set(),
  notice: null,
};

const appRoot = document.querySelector<HTMLDivElement>("#app");
if (!appRoot) throw new Error("CODETAS app root is missing");
const app: HTMLDivElement = appRoot;

const navigation: Array<{ id: View; label: string }> = [
  { id: "overview", label: "概要" },
  { id: "providers", label: "接続" },
  { id: "routing", label: "ルーティング" },
  { id: "agents", label: "エージェント" },
  { id: "projects", label: "Hermes 同期" },
  { id: "clients", label: "クライアント" },
  { id: "settings", label: "設定" },
];

function h(value: unknown): string {
  return String(value ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#039;");
}

function isBusy(key: string): boolean {
  return state.busy.has(key);
}

function formatNumber(value: number | null | undefined): string {
  return new Intl.NumberFormat("ja-JP", { maximumFractionDigits: 1 }).format(value ?? 0);
}

function formatBytes(value: number | null | undefined): string {
  const bytes = value ?? 0;
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
}

function statusDot(active: boolean, warning = false): string {
  return `<span class="status-dot ${active ? (warning ? "warning" : "active") : "idle"}" aria-hidden="true"></span>`;
}

function protocolLabel(protocol: string | undefined): string {
  switch (protocol) {
    case "responses":
      return "Responses";
    case "chatCompletions":
      return "Chat";
    case "anthropicMessages":
      return "Messages";
    case "geminiGenerateContent":
      return "Gemini";
    default:
      return protocol || "-";
  }
}

function render(): void {
  const activeNav = navigation.find((item) => item.id === state.view) ?? navigation[0]!;
  app.innerHTML = `
    <div class="app-shell">
      <aside class="side-rail">
        <div class="brand-lockup">
          <img src="/codetas-mark.svg" width="42" height="42" alt="" />
          <div><strong>CODETAS</strong><span>Codexに、できることを足す。</span></div>
        </div>
        <nav class="nav" aria-label="メインナビゲーション">
          ${navigation.map((item) => `
            <button class="nav-item ${state.view === item.id ? "selected" : ""}" data-view="${item.id}" type="button">
              <span class="nav-label">${h(item.label)}</span>
            </button>
          `).join("")}
        </nav>
        <div class="rail-runtime">
          <div>${statusDot(Boolean(state.status?.running))}<span>Gateway</span></div>
          <strong>${state.status?.running ? "稼働中" : "停止中"}</strong>
          <code>${h(state.status?.url ?? "未起動")}</code>
        </div>
      </aside>
      <main class="workspace">
        <header class="topbar">
          <div>
            <h1>${h(activeNav.label)}</h1>
          </div>
          <div class="top-actions">
            <span class="provider-count">${formatNumber(state.configuration?.providers.length)} 件の接続</span>
            <button class="icon-button" data-action="refresh-all" aria-label="状態を更新" title="状態を更新" type="button">↻</button>
          </div>
        </header>
        ${state.notice ? `<div class="notice ${state.notice.tone}" role="status"><span>${h(state.notice.text)}</span><button data-action="dismiss-notice" type="button" aria-label="閉じる">×</button></div>` : ""}
        <section class="view" aria-live="polite">${renderView()}</section>
      </main>
      ${state.editingProviderId ? renderProviderEditor() : ""}
    </div>
  `;
  hydratePostRenderValues();
}

function renderView(): string {
  if (!state.configuration || !state.status) return renderLoading();
  switch (state.view) {
    case "overview": return renderOverview();
    case "providers": return renderProviders();
    case "routing": return renderRouting();
    case "agents": return renderAgents();
    case "projects": return renderProjects();
    case "clients": return renderClients();
    case "settings": return renderSettings();
  }
}

function renderLoading(): string {
  return `
    <div class="loading-stage">
      <div class="loading-path"><i></i><i></i><i></i><i></i></div>
      <h2>CODETAS を読み込んでいます</h2>
      <p>ローカル設定と Gateway の状態を確認しています。</p>
    </div>`;
}

function renderOverview(): string {
  const config = state.configuration!;
  const status = state.status!;
  const obs = state.observability;
  const errors = state.diagnostics?.errors ?? 0;
  const warnings = state.diagnostics?.warnings ?? 0;
  const breakdown = state.breakdown;
  const defaultProvider = config.providers.find((provider) => provider.id === config.defaultProvider);
  return `
    <div class="overview-grid">
      <article class="hero-console">
        <div class="hero-copy">
          <span class="eyebrow">状態</span>
          <h2>${status.running && status.codexConfigured
            ? "準備完了です。Codex が CODETAS を利用しています。"
            : status.running
              ? "Gateway は稼働中。Codex への接続が未設定です。"
              : status.codexConfigured
                ? "Gateway が停止しています。起動してください。"
                : "Gateway を起動して、Codex に接続してください。"}</h2>
          <p>CODETAS は「Gateway（ローカルサーバー）」と「Codex への接続設定」の2つで動きます。右側の状態でそれぞれを確認できます。</p>
          <div class="button-row">
            <button class="primary" data-action="${status.running ? "stop-gateway" : "start-gateway"}" type="button" ${isBusy("gateway") ? "disabled" : ""}>
              ${isBusy("gateway") ? "処理中…" : status.running ? "Gateway を停止" : "Gateway を起動"}
            </button>
            ${status.codexConfigured
              ? `<button class="text-button" data-action="restore-codex" type="button" ${isBusy("codex") ? "disabled" : ""}>Codex 接続を解除</button>`
              : `<button class="secondary" data-action="install-codex" type="button">Codex に接続</button>`}
          </div>
        </div>
        <div class="hero-status">
          <div class="status-list">
            <div class="status-row ${status.running ? "ok" : ""}">
              <span class="status-led" aria-hidden="true"></span>
              <div class="status-label"><strong>Gateway</strong><small>${status.running ? "稼働中" : "停止中"}</small></div>
              <code>${h(status.url ?? "未起動")}</code>
            </div>
            <div class="status-row ${status.codexConfigured ? "ok" : ""}">
              <span class="status-led" aria-hidden="true"></span>
              <div class="status-label"><strong>Codex 接続</strong><small>${status.codexConfigured ? "接続済み" : "未設定"}</small></div>
              <code>${status.codexConfigured ? h(defaultProvider?.name ?? "—") : "設定が必要"}</code>
            </div>
          </div>
          <div class="status-note">既定の接続: ${h(defaultProvider?.name ?? "—")} · ${formatNumber(config.providers.length)} 件 · ルーティング ${formatNumber(config.routes.length)} 本</div>
        </div>
      </article>
      <div class="metric-strip">
        <article><span>リクエスト</span><strong>${formatNumber(obs?.totalRequests)}</strong><small>${formatNumber(obs?.successfulRequests)} 成功</small></article>
        <article><span>トークン</span><strong>${formatNumber(obs?.totalTokens)}</strong><small>${formatNumber(obs?.reasoningTokens)} 推論</small></article>
        <article><span>モデル</span><strong>${formatNumber(modelCount(config))}</strong><small>${formatNumber(config.routes.length)} ルーティング</small></article>
        <article><span>記録容量</span><strong>${formatBytes(obs?.storageBytes)}</strong><small>上限 ${formatBytes(config.observability.maxStorageBytes)}</small></article>
      </div>
      ${obs?.persistenceError ? `<div class="notice error">記録: ${h(obs.persistenceError)}</div>` : ""}
      <article class="panel diagnostics-panel">
        <header><div><h3>診断</h3></div><button class="text-button" data-action="run-diagnostics" type="button">再診断</button></header>
        ${state.diagnostics ? `
          <div class="diagnostic-score ${errors ? "bad" : warnings ? "warn" : "good"}">
            <strong>${errors ? `${errors}件のエラー` : warnings ? `${warnings}件の確認事項` : "問題は見つかりません"}</strong>
            <span>${state.diagnostics.passed} 件通過 / ${state.diagnostics.checks.length} 件検査</span>
          </div>
          <div class="check-list">${state.diagnostics.checks.slice(0, 5).map((check) => `
            <div><span class="check-mark ${check.level}">${check.level === "pass" ? "✓" : "!"}</span><p><strong>${h(check.summary)}</strong>${check.remediation ? `<small>${h(check.remediation)}</small>` : ""}</p></div>
          `).join("")}</div>
        ` : `<div class="empty-inline">診断を実行すると、接続と設定の状態を確認できます。</div>`}
      </article>
      <article class="panel quick-panel">
        <header><div><h3>セットアップ</h3></div></header>
        <button class="task-row" data-view="providers" type="button"><b>1</b><span><strong>接続を追加</strong><small>OpenAI / Anthropic / Google など</small></span><i>→</i></button>
        <button class="task-row" data-action="sync-catalog" type="button"><b>2</b><span><strong>モデル一覧を同期</strong><small>Codex のモデル選択に反映</small></span><i>→</i></button>
        <button class="task-row" data-view="projects" type="button"><b>3</b><span><strong>Hermes プロジェクトを確認</strong><small>.hermes を読み取り専用で利用</small></span><i>→</i></button>
      </article>
      <article class="panel usage-map-panel">
        <header><div><h3>接続別の利用状況</h3></div><span class="legend">${formatNumber(breakdown?.scannedEvents)} 件${breakdown?.truncated ? "（上限）" : ""}</span></header>
        <div class="usage-map-grid">
          <div><h4>接続</h4>${renderUsageBars(breakdown?.providers ?? [])}</div>
          <div><h4>画面</h4>${renderUsageBars(breakdown?.surfaces ?? [])}</div>
          <div><h4>モデル</h4>${renderUsageBars(breakdown?.models.slice(0, 6) ?? [])}</div>
        </div>
      </article>
    </div>`;
}

function renderUsageBars(rows: ObservabilityBreakdown["providers"]): string {
  const max = Math.max(1, ...rows.map((row) => row.requests));
  return rows.slice(0, 6).map((row) => `<div class="usage-bar"><div><strong>${h(row.key)}</strong><span>${formatNumber(row.requests)}</span></div><i><b style="width:${Math.max(2, row.requests / max * 100)}%"></b></i><small>${formatNumber(row.totalTokens)} tokens · ${row.requests ? Math.round(row.totalLatencyMs / row.requests) : 0}ms avg</small></div>`).join("") || `<div class="empty-inline">記録された利用はまだありません。</div>`;
}

function renderProviders(): string {
  const config = state.configuration!;
  const availablePresets = state.presets.filter((preset) => !config.providers.some((provider) => provider.id === preset.id));
  return `
    <div class="section-grid providers-layout">
      <section class="panel provider-list-panel">
        <header><div><h2>${config.providers.length} 件の接続</h2></div><span class="legend">ログイン情報は別ファイルに保存します</span></header>
        <div class="provider-list">
          ${config.providers.map(renderProviderCard).join("") || `<div class="empty-state"><strong>まだ接続がありません</strong><p>右側のプリセットから最初のプロバイダーを追加してください。</p></div>`}
        </div>
        <section class="local-cli-panel">
          <header><div><h3>ローカル CLI の検出</h3></div><button class="secondary compact" data-action="probe-local-clis" type="button" ${isBusy("local-clis") ? "disabled" : ""}>${isBusy("local-clis") ? "確認中…" : "登録確認"}</button></header>
          <p class="local-cli-help">既存の CLI ログインをそのまま使えます。「登録確認」で認証状態を確認し、取り込んだログインは CODETAS が保持して期限が近づくと自動更新します。</p>
          <div class="local-cli-list">${renderLocalCliRows()}</div>
        </section>
        <section class="local-cli-panel">
          <header><div><h3>直接 API 接続</h3></div></header>
          <p class="local-cli-help">公式 API キーで登録する接続です。OpenCode Go 経由の同名モデルとは別に扱います。</p>
          <div class="local-cli-list">${renderDirectApiRows()}</div>
        </section>
      </section>
      <aside class="panel add-provider-panel">
        <header><div><h3>接続を追加</h3></div></header>
        <form id="preset-form" class="stack-form">
          <label>プロバイダー
            <select name="presetId" required>
              <option value="">選択してください</option>
              ${availablePresets.map((preset) => `<option value="${h(preset.id)}">${h(preset.name)}</option>`).join("")}
            </select>
          </label>
          <label>カスタムURL <small>必要なプリセットのみ</small><input name="baseUrl" type="url" placeholder="https://api.example.com/v1" /></label>
          <label class="check-control"><input name="makeDefault" type="checkbox" ${config.defaultProvider ? "" : "checked"}/><span>既定の接続にする</span></label>
          <button class="primary wide" type="submit" ${isBusy("preset") ? "disabled" : ""}>${isBusy("preset") ? "追加中…" : "接続を追加"}</button>
        </form>
        <div class="registry-note"><strong>あと ${availablePresets.length} 件追加できます</strong><p>ローカル接続（Ollama など）もこの一覧から選べます。</p></div>
      </aside>
    </div>`;
}

function renderLocalCliRows(): string {
  const rows = state.localClis?.clients ?? [];
  if (!rows.length) return `<div class="empty-inline">ローカルCLIを確認しています。</div>`;
  return rows.map((client) => {
    const ready = client.probeState === "ready";
    const warning = client.installed && !ready;
    const registered = client.installed && !client.needsCodetasRegistration && Boolean(client.codetasProviderId);
    const label = client.canRegister
      ? "利用可能"
      : registered
        ? "登録済み"
        : client.needsCodetasRegistration
          ? "ログインを取り込む"
          : ready
            ? "検出済み（未対応）"
            : client.installed
              ? "要確認"
              : "未検出";
    return `<div class="local-cli-row">
      <div>${statusDot(client.installed, warning || client.needsCodetasRegistration)}<span><strong>${h(client.name)}</strong><code>${h(client.id)}</code></span></div>
      <p>${h(client.message)}${client.registrationHint && client.needsCodetasRegistration ? ` ${h(client.registrationHint)}` : ""}${client.version ? `<small>${h(client.version)}</small>` : ""}</p>
      <span class="chip ${client.canRegister || registered ? "ready" : ""}">${h(label)}</span>
      ${client.needsCodetasRegistration ? `<button class="text-button" data-action="register-local-cli" data-client-id="${h(client.id)}" type="button">使う</button>` : ""}
    </div>`;
  }).join("");
}

function renderDirectApiRows(): string {
  const config = state.configuration;
  return (state.directApis ?? []).map((target) => {
    const existing = config?.providers.find((provider) => provider.id === target.providerId);
    const registered = Boolean(existing?.enabled);
    return `<div class="local-cli-row">
      <div>${statusDot(registered)}<span><strong>${h(target.name)}</strong><code>${h(target.providerId)}</code></span></div>
      <p>${h(target.hint)}</p>
      <span class="chip ${registered ? "ready" : ""}">${registered ? "登録済み" : "未登録"}</span>
      ${registered ? "" : `<button class="text-button" data-action="register-direct-api" data-provider-id="${h(target.providerId)}" type="button">CODETAS に登録</button>`}
    </div>`;
  }).join("") || `<div class="empty-inline">直接APIの候補がありません。</div>`;
}

function renderProviderCard(provider: ProviderDefinition): string {
  const config = state.configuration!;
  const credential = provider.credential;
  const credentialLabel = credential?.source === "environment"
    ? credential.reference ?? provider.apiKeyEnv ?? "環境変数"
    : credential?.source === "oAuth"
      ? "ログイン"
      : credential?.source === "forward"
        ? "Codex ログイン"
        : credential?.source === "keychain"
          ? "Keychain"
          : credential?.source === "command"
            ? "コマンド"
            : credential?.source === "none" || !credential?.source
              ? "なし"
              : credential.source;
  return `
    <article class="provider-card ${provider.enabled ? "" : "disabled"}">
      <div class="provider-main">
        <div class="provider-monogram">${h(provider.name.slice(0, 2).toUpperCase())}</div>
        <div><div class="provider-title"><h3>${h(provider.name)}</h3>${config.defaultProvider === provider.id ? '<span class="chip default">既定</span>' : ""}</div><code>${h(provider.id)}</code></div>
      </div>
      <div class="provider-data"><span>方式<strong>${h(protocolLabel(provider.protocol))}</strong></span><span>モデル<strong>${formatNumber(provider.models.length)}</strong></span><span>認証<strong>${h(credentialLabel)}</strong></span></div>
      <div class="provider-url"><span>${statusDot(provider.enabled)}</span><code>${h(provider.baseUrl)}</code></div>
      <div class="card-actions">
        <button data-action="test-provider" data-provider-id="${h(provider.id)}" type="button">接続確認</button>
        ${["github-copilot", "google-vertex", "google-antigravity", "google", "anthropic", "kimi", "xai"].includes(provider.id) ? `<button data-action="oauth-provider" data-provider-id="${h(provider.id)}" type="button">${provider.id === "anthropic" ? "Claude でログイン" : provider.id === "kimi" ? "Kimi でログイン" : provider.id === "xai" ? "Grok でログイン" : "OAuth ログイン"}</button>` : ""}
        <button data-action="refresh-models" data-provider-id="${h(provider.id)}" type="button">モデル取得</button>
        <button data-action="edit-provider" data-provider-id="${h(provider.id)}" type="button">編集</button>
        ${config.defaultProvider !== provider.id ? `<button data-action="default-provider" data-provider-id="${h(provider.id)}" type="button">既定にする</button>` : ""}
      </div>
    </article>`;
}

function renderRouting(): string {
  const config = state.configuration!;
  const models = allModelIds(config);
  return `
    <div class="section-grid routing-layout">
      <section class="panel routes-panel">
        <header><div><h2>ルーティング</h2></div><button class="secondary compact" data-action="add-route-row" type="button">経路を追加</button></header>
        <div id="route-editor-list" class="route-editor-list">
          ${config.routes.map((route, index) => renderRouteEditor(route, index, models)).join("") || `<div class="empty-state"><strong>直接接続を使用中です</strong><p>経路を追加すると、複数モデル間のフェイルオーバーや重み付き分散を構成できます。</p></div>`}
        </div>
        <div class="panel-footer"><button class="primary" data-action="save-routes" type="button">経路を保存</button></div>
      </section>
      <aside class="panel model-index">
        <header><div><h3>${modelCount(config)} 件のモデル</h3></div><button class="text-button" data-action="sync-catalog" type="button">Codex へ同期</button></header>
        <div class="model-filter"><input id="model-search" type="search" placeholder="モデル ID を検索" autocomplete="off" /></div>
        <div id="model-list" class="model-list">${renderModelRows(config, "")}</div>
      </aside>
    </div>`;
}

function renderRouteEditor(route: GatewayConfiguration["routes"][number], index: number, models: string[]): string {
  return `<article class="route-editor" data-route-index="${index}">
    <div class="route-head"><input data-field="name" value="${h(route.name)}" aria-label="経路名"/><label class="switch"><input data-field="enabled" type="checkbox" ${route.enabled ? "checked" : ""}/><span></span></label></div>
    <div class="form-grid two"><label>ID<input data-field="id" value="${h(route.id)}" /></label><label>公開名<input data-field="alias" value="${h(route.alias ?? "")}" placeholder="任意" /></label></div>
    <div class="form-grid two"><label>方式<select data-field="strategy"><option value="failover" ${route.strategy === "failover" ? "selected" : ""}>Failover</option><option value="weightedRoundRobin" ${route.strategy === "weightedRoundRobin" ? "selected" : ""}>Weighted round robin</option><option value="leastUsage" ${route.strategy === "leastUsage" ? "selected" : ""}>Least usage</option></select></label><label>既定 effort<input data-field="defaultReasoningEffort" value="${h(route.defaultReasoningEffort ?? "")}" placeholder="medium" /></label></div>
    <label>対象 <small>1 行に provider/model@weight</small><textarea data-field="targets" rows="${Math.max(2, route.targets.length)}">${h(route.targets.map((target) => `${target.model}@${target.weight}`).join("\n"))}</textarea></label>
    <div class="route-foot"><span>${route.targets.length} 件の対象</span><button class="danger-link" data-action="remove-route" data-route-index="${index}" type="button">削除</button></div>
    <datalist id="model-options-${index}">${models.map((model) => `<option value="${h(model)}"></option>`).join("")}</datalist>
  </article>`;
}

function renderModelRows(config: GatewayConfiguration, query: string): string {
  const normalized = query.trim().toLowerCase();
  return config.providers.flatMap((provider) => {
    const ids = [...new Set([...(provider.models ?? []), ...(provider.defaultModel ? [provider.defaultModel] : [])])];
    return ids.map((model) => ({ provider, model }));
  }).filter(({ provider, model }) => `${provider.id}/${model}`.toLowerCase().includes(normalized)).slice(0, 160).map(({ provider, model }) => {
    const metadata = config.modelCatalog.find((item) => item.providerId === provider.id && item.modelId === model);
    const efforts = metadata?.reasoningEfforts ?? provider.modelReasoningEfforts?.[model] ?? [];
    return `<div class="model-row"><div><strong>${h(model)}</strong><code>${h(provider.id)}</code></div><span>${metadata?.contextWindow ? `${formatNumber(metadata.contextWindow / 1000)}k` : "-"}</span><span>${efforts.length ? h(efforts.join(" / ")) : "標準"}</span></div>`;
  }).join("") || `<div class="empty-inline">一致するモデルがありません。</div>`;
}

function renderAgents(): string {
  const config = state.configuration!;
  const options = allModelIds(config);
  return `
    <form id="agents-form" class="agent-layout">
      <section class="panel agent-core-panel">
        <header><div><h2>並列実行</h2></div><label class="switch labeled"><input name="multiAgentV2" type="checkbox" ${config.agents.multiAgentV2 ? "checked" : ""}/><span></span><b>${config.agents.multiAgentV2 ? "オン" : "オフ"}</b></label></header>
        <div class="agent-topology">
          <div class="agent-main"><span>メイン</span><strong>${h(config.defaultProvider ?? "未設定")}</strong><small>effort ${h(config.agents.effortCap ?? "既定")}</small></div>
          <div class="agent-branches">${Array.from({ length: Math.min(config.agents.maxThreads, 6) }, (_, index) => `<span style="--i:${index}">A${index + 1}</span>`).join("")}</div>
        </div>
        <div class="form-grid three">
          <label>表示モード<select name="surfaceMode"><option value="v1" ${config.agents.surfaceMode === "v1" ? "selected" : ""}>v1 compatible</option><option value="default" ${config.agents.surfaceMode === "default" ? "selected" : ""}>Default</option><option value="v2" ${config.agents.surfaceMode === "v2" ? "selected" : ""}>v2 native</option></select></label>
          <label>最大スレッド<input name="maxThreads" type="number" min="1" max="64" value="${config.agents.maxThreads}" /></label>
          <label>メイン effort<input name="effortCap" value="${h(config.agents.effortCap ?? "")}" placeholder="high" /></label>
        </div>
        <label>サブエージェントモデル <small>1 行に 1 モデル。上のモデルを優先します。</small><textarea name="subagentModels" rows="4">${h(config.agents.subagentModels.join("\n"))}</textarea></label>
        <label>フォールバック <small>1 行に 1 モデル。</small><textarea name="subagentFallback" rows="3">${h(config.agents.subagentFallback.join("\n"))}</textarea></label>
      </section>
      <aside class="panel sidecar-panel">
        <header><div><h3>サイドカー</h3></div></header>
        ${renderModelSelect("webSearchModel", "Web search", config.sidecars.webSearchModel, options)}
        ${renderModelSelect("visionModel", "Vision", config.sidecars.visionModel, options)}
        ${renderModelSelect("imageModel", "Image", config.sidecars.imageModel, options)}
        ${renderModelSelect("videoModel", "Video", config.sidecars.videoModel, options)}
        ${renderModelSelect("liveModel", "Realtime", config.sidecars.liveModel, options)}
        <button class="primary wide" type="submit">エージェント設定を保存</button>
      </aside>
    </form>`;
}

function renderModelSelect(name: string, label: string, selected: string | null, models: string[]): string {
  return `<label>${h(label)}<select name="${h(name)}"><option value="">未使用</option>${models.map((model) => `<option value="${h(model)}" ${selected === model ? "selected" : ""}>${h(model)}</option>`).join("")}</select></label>`;
}

function renderProjects(): string {
  const project = state.project;
  const plan = state.syncPlan;
  return `
    <div class="project-layout">
      <section class="project-intro">
        <h2>.hermes を壊さず、Codex で活かす。</h2>
        <p>プロジェクトの指示・スキル・MCP 設定を読み取り専用で検出します。元のファイルは変更せず、適用前に変換内容を確認できます。</p>
        <button class="primary" data-action="pick-project" type="button">プロジェクトを選ぶ</button>
      </section>
      <section class="panel project-inspector">
        ${project ? `
          <header><div><h3>${h(project.name)}</h3></div><span class="chip ready">読み取り専用</span></header>
          <code class="project-path">${h(project.path)}</code>
          <div class="source-grid">
            ${sourceTile("Context", project.contextFile, "CX")}
            ${sourceTile("Skills", project.skillsDirectory, `${project.skillsCount}`)}
            ${sourceTile("MCP", project.mcpFile, "MC")}
            ${sourceTile("Codex", project.codexConfigFile ?? project.agentsFile, "CD")}
          </div>
          <form id="sync-plan-form" class="sync-options">
            <label class="check-control"><input name="context" type="checkbox" checked/><span>プロジェクト指示</span></label>
            <label class="check-control"><input name="skills" type="checkbox" checked/><span>スキル</span></label>
            <label class="check-control"><input name="mcp" type="checkbox" checked/><span>MCP 接続</span></label>
            <button class="secondary" type="submit">同期プランを作る</button>
          </form>
        ` : `<div class="project-empty"><div class="scan-symbol"><span></span></div><strong>プロジェクトを選択してください</strong><p>.hermes、HERMES.md、AGENTS.md、SKILL.md、MCP 設定を探索します。</p></div>`}
      </section>
      ${plan ? `<section class="panel sync-plan-panel"><header><div><h3>${plan.actions.length} 件の同期項目</h3></div><span class="chip ready">元ファイルは変更しません</span></header><div class="plan-flow">${plan.actions.map((action) => `<div><span class="plan-kind">${h(action.category)}</span><p><strong>${h(action.summary)}</strong><small>${h(action.source)} → ${h(action.target)}</small></p><em class="${action.compatibility}">${h(action.compatibility)}</em></div>`).join("") || `<div class="empty-inline">同期できる項目が選択されていません。</div>`}</div>${plan.warnings.length ? `<div class="warning-box">${plan.warnings.map((warning) => `<p>${h(warning)}</p>`).join("")}</div>` : ""}<p class="plan-note">CODETAS プラグインの SessionStart フックが、このプランと同じ読み取り専用ルールでプロジェクトのコンテキストを Codex へ渡します。</p></section>` : ""}
    </div>`;
}

function sourceTile(label: string, path: string | null, monogram: string): string {
  return `<article class="source-tile ${path ? "found" : "missing"}"><span>${h(monogram)}</span><div><strong>${h(label)}</strong><small>${path ? h(path.split(/[\\/]/).at(-1)) : "見つかりません"}</small></div></article>`;
}

function renderClients(): string {
  const config = state.configuration!;
  const clients: Array<[keyof ExternalClientIntegrationInput, string, string]> = [
    ["claudeCode", "Claude Code", "Anthropic Messages 互換 Gateway で起動"],
    ["claudeDesktop", "Claude Desktop", "MCP とモデルファミリー設定を生成"],
    ["opencode", "OpenCode", "一時設定で CODETAS へ接続"],
    ["grok", "Grok", "CODETAS のモデルフェンスを適用"],
    ["pi", "Pi", "所有マーカー付き設定を出力"],
  ];
  return `
    <div class="clients-layout">
      <form id="clients-form" class="panel client-list-panel">
        <header><div><h2>同じ接続を他のアプリでも</h2></div></header>
        ${clients.map(([key, name, detail]) => `<label class="client-row"><span class="client-glyph">${h(name.slice(0, 2))}</span><span><strong>${h(name)}</strong><small>${h(detail)}</small></span><input name="${key}" type="checkbox" ${config.integrations[key] ? "checked" : ""}/></label>`).join("")}
        <div class="panel-footer"><button class="primary" type="submit">選択した連携を生成</button></div>
      </form>
      <aside class="panel service-panel">
        <header><div><h3>常駐サービス</h3></div>${statusDot(Boolean(state.service?.running))}</header>
        <div class="service-state"><strong>${state.service?.running ? "起動中" : state.service?.installed ? "停止中" : "未登録"}</strong><p>${h(state.service?.message ?? "状態を読み込んでいます")}</p>${state.service?.installed ? `<small>${h(state.service.supervisor)} · ${h(state.service.restartPolicy)}</small>` : ""}</div>
        <div class="stack-actions">
          ${state.service?.installed
            ? `${state.service.running ? `<button class="secondary wide" data-action="restart-service" type="button">再起動</button><button class="text-button wide" data-action="stop-service" type="button">停止</button>` : `<button class="secondary wide" data-action="start-service" type="button">サービスを起動</button>`}<button class="danger-link wide" data-action="uninstall-service" type="button">自動起動を解除</button>`
            : `<button class="primary wide" data-action="install-service" type="button">ログイン時に自動起動</button>`}
        </div>
      </aside>
    </div>`;
}

function renderSettings(): string {
  const config = state.configuration!;
  return `
    <form id="settings-form" class="settings-layout">
      <section class="panel settings-section">
        <header><div><h2>Gateway</h2></div></header>
        <div class="form-grid two"><label>Host<input name="host" value="${h(config.runtime.host)}" /></label><label>Port<input name="port" type="number" min="1" max="65535" value="${config.runtime.port}" /></label></div>
        <label>終了猶予（ms）<input name="shutdownTimeoutMs" type="number" min="100" max="300000" value="${config.runtime.shutdownTimeoutMs}" /></label>
        <label class="check-control"><input name="dynamicPortFallback" type="checkbox" ${config.runtime.dynamicPortFallback !== false ? "checked" : ""}/><span>使用中のポートを自動回避する</span></label>
        <label class="check-control"><input name="autoStart" type="checkbox" ${config.runtime.autoStart ? "checked" : ""}/><span>アプリ起動時に Gateway を開始</span></label>
        <label class="check-control"><input name="autoSyncCatalog" type="checkbox" ${config.codex.autoSyncCatalog ? "checked" : ""}/><span>所有権を確認して Codex のモデル一覧を自動同期</span></label>
      </section>
      <section class="panel settings-section">
        <header><div><h2>セキュリティ</h2></div></header>
        <label class="check-control"><input name="requireLocalToken" type="checkbox" ${config.security.requireLocalToken ? "checked" : ""}/><span>ローカル接続にもトークンを要求</span></label>
        <label class="check-control"><input name="dnsPinning" type="checkbox" ${config.security.dnsPinning ? "checked" : ""}/><span>接続先 DNS を固定して再検証</span></label>
        <label class="check-control"><input name="allowRemote" type="checkbox" ${config.security.allowRemote ? "checked" : ""}/><span>リモート端末からの接続を許可</span></label>
        <label>CORS 許可 Origin <small>1 行に 1 件</small><textarea name="corsAllowOrigins" rows="4">${h(config.security.corsAllowOrigins.join("\n"))}</textarea></label>
      </section>
      <section class="panel settings-section">
        <header><div><h2>記録</h2></div></header>
        <label class="check-control"><input name="requestLog" type="checkbox" ${config.observability.requestLog ? "checked" : ""}/><span>リクエスト結果を記録</span></label>
        <label class="check-control"><input name="usageLog" type="checkbox" ${config.observability.usageLog ? "checked" : ""}/><span>トークンと概算費用を記録</span></label>
        <div class="form-grid two"><label>保持日数<input name="retentionDays" type="number" min="1" max="3650" value="${config.observability.retentionDays}" /></label><label>上限（MB）<input name="maxStorageMb" type="number" min="1" max="10240" value="${Math.max(1, Math.round(config.observability.maxStorageBytes / 1024 ** 2))}" /></label></div>
        <div class="form-grid two"><label>ごみ箱保持日数<input name="trashRetentionDays" type="number" min="1" max="365" value="${config.observability.trashRetentionDays}" /></label><label>ごみ箱上限（MB）<input name="maxTrashMb" type="number" min="1" max="10240" value="${Math.max(1, Math.round(config.observability.maxTrashBytes / 1024 ** 2))}" /></label></div>
      </section>
      <section class="panel settings-section update-section">
        <header><div><h2>アップデート</h2></div><button class="text-button" data-action="check-update" type="button">更新を確認</button></header>
        <label class="check-control"><input name="updateAutoCheck" type="checkbox" ${config.updates.autoCheck ? "checked" : ""}/><span>起動時に更新を確認</span></label>
        <label>署名済み manifest URL<input name="manifestUrl" type="url" value="${h(config.updates.manifestUrl ?? "")}" placeholder="https://releases.example/latest.signed.json" /></label>
        <label>Manifest 公開鍵<input name="publicKeyBase64" value="${h(config.updates.publicKeyBase64 ?? "")}" autocomplete="off" /></label>
        <label>Tauri installer endpoint<input name="installerEndpoint" type="url" value="${h(config.updates.installerEndpoint ?? "")}" placeholder="https://releases.example/latest.json" /></label>
        <label>Installer 公開鍵<input name="installerPublicKey" value="${h(config.updates.installerPublicKey ?? "")}" autocomplete="off" /></label>
        <p class="field-note">manifest 署名・Tauri 署名・URL・バージョン・サイズ・SHA-256 がすべて一致した更新だけを適用します。</p>
      </section>
      <section class="panel settings-section storage-section">
        <header><div><h2>観測ストレージ</h2></div><button class="text-button" data-action="preview-cleanup" type="button">整理対象を確認</button></header>
        ${state.cleanupPreview ? `<div class="storage-preview"><strong>${state.cleanupPreview.files.length} files / ${formatBytes(state.cleanupPreview.totalBytesBefore - state.cleanupPreview.bytesAfter)}</strong><p>${formatBytes(state.cleanupPreview.totalBytesBefore)} → ${formatBytes(state.cleanupPreview.bytesAfter)}</p>${state.cleanupPreview.files.length ? '<button class="secondary wide" data-action="trash-cleanup" type="button">専用ごみ箱へ移動</button>' : ""}</div>` : `<p class="storage-copy">保持期限と容量上限から対象を計算します。削除はせず、復元できる CODETAS 専用のごみ箱へ移します。Gateway 停止中のみ実行できます。</p>`}
        <div class="trash-list"><h4>復元できる履歴</h4>${state.trashEntries.map((entry) => `<div><span><strong>${new Date(entry.createdAtMs).toLocaleString("ja-JP")}</strong><small>${entry.files} files · ${formatBytes(entry.bytes)}</small></span><button class="text-button" data-action="restore-trash" data-transaction-id="${h(entry.transactionId)}" type="button">復元</button></div>`).join("") || `<p>ごみ箱は空です。</p>`}</div>
      </section>
      <section class="panel settings-section advanced-config">
        <header><div><h2>設定JSON</h2></div><button class="text-button" data-action="copy-config" type="button">コピー</button></header>
        <p>画面にない互換スイッチやモデル別メタデータも編集できます。保存時に Rust 側で完全に検証します。</p>
        <textarea id="advanced-json" spellcheck="false" rows="18" aria-label="Gateway設定JSON"></textarea>
      </section>
      <div class="settings-savebar"><span>変更は一括で保存され、起動中の Gateway に反映されます。</span><button class="primary" type="submit">設定を保存</button></div>
    </form>`;
}

function renderProviderEditor(): string {
  const provider = state.configuration?.providers.find((item) => item.id === state.editingProviderId);
  if (!provider) return "";
  const credential = provider.credential ?? {
    source: "none", reference: null, transport: "bearer", headerName: null, command: null,
  };
  return `<div class="drawer-scrim" data-action="close-provider-editor"><aside class="provider-drawer" role="dialog" aria-modal="true" aria-labelledby="provider-editor-title" data-stop-close>
    <header><div><h2 id="provider-editor-title">${h(provider.name)}</h2></div><button class="icon-button" data-action="close-provider-editor" type="button" aria-label="閉じる">×</button></header>
    <form id="provider-editor-form" class="drawer-form">
      <input name="id" type="hidden" value="${h(provider.id)}" />
      <div class="form-grid two"><label>表示名<input name="name" value="${h(provider.name)}" required /></label><label>既定モデル<input name="defaultModel" value="${h(provider.defaultModel ?? "")}" /></label></div>
      <label>Base URL<input name="baseUrl" type="url" value="${h(provider.baseUrl)}" required /></label>
      <div class="form-grid two"><label>API protocol<select name="protocol"><option value="responses" ${provider.protocol === "responses" ? "selected" : ""}>Responses</option><option value="chatCompletions" ${provider.protocol === "chatCompletions" ? "selected" : ""}>Chat Completions</option><option value="anthropicMessages" ${provider.protocol === "anthropicMessages" ? "selected" : ""}>Anthropic Messages</option><option value="geminiGenerateContent" ${provider.protocol === "geminiGenerateContent" ? "selected" : ""}>Gemini generateContent</option></select></label><label>Native transport<select name="providerTransport"><option value="standard" ${provider.transport === "standard" || !provider.transport ? "selected" : ""}>Standard HTTP / SSE</option><option value="kiro" ${provider.transport === "kiro" ? "selected" : ""}>Kiro event-stream</option><option value="githubCopilot" ${provider.transport === "githubCopilot" ? "selected" : ""}>GitHub Copilot exchange</option></select></label></div>
      <div class="form-grid two"><label>Google mode<select name="googleMode"><option value="aiStudio" ${provider.googleMode === "aiStudio" || !provider.googleMode ? "selected" : ""}>AI Studio</option><option value="vertex" ${provider.googleMode === "vertex" ? "selected" : ""}>Vertex</option><option value="cloudCodeAssist" ${provider.googleMode === "cloudCodeAssist" ? "selected" : ""}>Cloud Code Assist</option></select></label><label>Location<input name="location" value="${h(provider.location ?? "")}" /></label></div>
      <label>Project ID<input name="project" value="${h(provider.project ?? "")}" /></label>
      <div class="form-grid two"><label>Azure deployment<input name="azureDeployment" value="${h(provider.azureDeployment ?? "")}" /></label><label>Azure API version<input name="azureApiVersion" value="${h(provider.azureApiVersion ?? "")}" placeholder="2025-04-01-preview" /></label></div>
      <label>Kiro profile ARN <small>Enterprise profileだけ。API key / Builder IDでは空欄</small><input name="kiroProfileArn" value="${h(provider.kiroProfileArn ?? "")}" /></label>
      <div class="form-grid two"><label>Responses path<input name="responsesPath" value="${h(provider.responsesPath ?? "")}" placeholder="/responses" /></label><label>Realtime WebSocket URL<input name="realtimeWsBaseUrl" type="url" value="${h(provider.realtimeWsBaseUrl ?? "")}" /></label></div>
      <div class="form-grid two"><label>通常リクエスト再試行<input name="requestRetries" type="number" min="0" max="10" value="${provider.limits?.requestRetries ?? 2}" /></label><label>ストリーム開始再試行<input name="streamRetries" type="number" min="0" max="10" value="${provider.limits?.streamRetries ?? 2}" /></label></div>
      <div class="toggle-pair"><label class="check-control"><input name="statelessResponses" type="checkbox" ${provider.statelessResponses ? "checked" : ""}/><span>上流 Responses を stateless 化</span></label><label class="check-control"><input name="stripModelBracketSuffix" type="checkbox" ${provider.stripModelBracketSuffix ? "checked" : ""}/><span>モデル末尾の [variant] を除去</span></label></div>
      <div class="drawer-rule"></div>
      <div class="form-grid two"><label>資格情報<select name="credentialSource"><option value="none" ${credential.source === "none" ? "selected" : ""}>なし</option><option value="environment" ${credential.source === "environment" ? "selected" : ""}>環境変数</option><option value="keychain" ${credential.source === "keychain" ? "selected" : ""}>Keychain参照</option><option value="oAuth" ${credential.source === "oAuth" ? "selected" : ""}>OAuth（CODETASが保持）</option><option value="command" ${credential.source === "command" ? "selected" : ""}>コマンド参照</option><option value="forward" ${credential.source === "forward" ? "selected" : ""}>呼び出し元から転送</option></select></label><label>Transport<select name="credentialTransport"><option value="bearer" ${credential.transport === "bearer" ? "selected" : ""}>Bearer</option><option value="xApiKey" ${credential.transport === "xApiKey" ? "selected" : ""}>x-api-key</option><option value="customHeader" ${credential.transport === "customHeader" ? "selected" : ""}>Custom header</option></select></label></div>
      <label>参照名 <small>値ではなく環境変数名・Keychain ID・broker ID</small><input name="credentialReference" value="${h(credential.reference ?? provider.apiKeyEnv ?? "")}" autocomplete="off" /></label>
      <div class="toggle-pair"><label class="check-control"><input name="enabled" type="checkbox" ${provider.enabled ? "checked" : ""}/><span>接続を有効化</span></label><label class="check-control"><input name="allowPrivateNetwork" type="checkbox" ${provider.allowPrivateNetwork ? "checked" : ""}/><span>プライベートネットワークを許可</span></label></div>
      <div class="drawer-actions"><button class="danger-link" data-action="remove-provider" data-provider-id="${h(provider.id)}" type="button">接続を削除</button><button class="primary" type="submit">変更を保存</button></div>
    </form>
  </aside></div>`;
}

function hydratePostRenderValues(): void {
  const advanced = document.querySelector<HTMLTextAreaElement>("#advanced-json");
  if (advanced && state.configuration) advanced.value = JSON.stringify(state.configuration, null, 2);
}

function modelCount(config: GatewayConfiguration): number {
  return allModelIds(config).length;
}

function allModelIds(config: GatewayConfiguration): string[] {
  const ids = new Set<string>();
  for (const provider of config.providers) {
    for (const model of provider.models ?? []) ids.add(`${provider.id}/${model}`);
    if (provider.defaultModel) ids.add(`${provider.id}/${provider.defaultModel}`);
  }
  for (const model of config.modelCatalog) ids.add(`${model.providerId}/${model.modelId}`);
  return [...ids].sort((left, right) => left.localeCompare(right));
}

function lines(value: FormDataEntryValue | null): string[] {
  return String(value ?? "").split(/\r?\n|,/).map((item) => item.trim()).filter(Boolean);
}

async function withBusy<T>(key: string, action: () => Promise<T>): Promise<T | undefined> {
  state.busy.add(key);
  state.notice = null;
  render();
  try {
    return await action();
  } catch (error) {
    state.notice = { tone: "error", text: readableError(error) };
    return undefined;
  } finally {
    state.busy.delete(key);
    render();
  }
}

function readableError(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  return "操作を完了できませんでした。設定と接続状態を確認してください。";
}

function notify(text: string, tone: Notice["tone"] = "success"): void {
  state.notice = { tone, text };
  render();
}

async function refreshAll(showNotice = false): Promise<void> {
  await withBusy("refresh", async () => {
    const [status, configuration, presets, observability, breakdown, service, trashEntries, localClis, directApis] = await Promise.all([
      invoke<GatewayStatus>("provider_gateway_status"),
      invoke<GatewayConfiguration>("gateway_configuration"),
      invoke<ProviderPreset[]>("list_provider_presets"),
      invoke<ObservabilitySummary>("gateway_observability_summary"),
      invoke<ObservabilityBreakdown>("gateway_observability_breakdown", { sinceMs: 0, maxEvents: 50_000 }),
      invoke<GatewayServiceStatus>("gateway_service_status"),
      invoke<ObservabilityTrashEntry[]>("list_gateway_observability_trash"),
      invoke<LocalCliScanReport>("scan_local_cli_clients", { deep: false }),
      invoke<DirectApiTarget[]>("list_direct_api_targets"),
    ]);
    state.status = status;
    state.configuration = configuration;
    state.presets = presets;
    state.directApis = directApis;
    state.observability = observability;
    state.breakdown = breakdown;
    state.service = service;
    state.trashEntries = trashEntries;
    state.localClis = localClis;
    if (showNotice) state.notice = { tone: "info", text: "最新の状態に更新しました。" };
  });
}

async function refreshStatusAndConfig(): Promise<void> {
  const [status, configuration] = await Promise.all([
    invoke<GatewayStatus>("provider_gateway_status"),
    invoke<GatewayConfiguration>("gateway_configuration"),
  ]);
  state.status = status;
  state.configuration = configuration;
}

document.addEventListener("click", (event) => {
  const target = (event.target as HTMLElement).closest<HTMLElement>("[data-view], [data-action]");
  if (!target) return;
  const view = target.dataset.view as View | undefined;
  if (view) {
    state.view = view;
    state.notice = null;
    render();
    return;
  }
  const action = target.dataset.action;
  if (!action) return;
  if (action === "close-provider-editor" && (event.target as HTMLElement).closest("[data-stop-close]") && target.classList.contains("drawer-scrim")) return;
  void handleAction(action, target);
});

document.addEventListener("submit", (event) => {
  event.preventDefault();
  const form = event.target as HTMLFormElement;
  void handleForm(form);
});

document.addEventListener("input", (event) => {
  const target = event.target as HTMLInputElement;
  if (target.id === "model-search" && state.configuration) {
    const list = document.querySelector("#model-list");
    if (list) list.innerHTML = renderModelRows(state.configuration, target.value);
  }
});

async function handleAction(action: string, target: HTMLElement): Promise<void> {
  switch (action) {
    case "dismiss-notice": state.notice = null; render(); return;
    case "refresh-all": await refreshAll(true); return;
    case "start-gateway":
    case "stop-gateway":
      await withBusy("gateway", async () => {
        state.status = await invoke<GatewayStatus>(action === "start-gateway" ? "start_provider_gateway" : "stop_provider_gateway");
        notify(action === "start-gateway" ? "Gatewayを起動しました。" : "Gatewayを停止しました。");
      });
      return;
    case "run-diagnostics":
      await withBusy("diagnostics", async () => {
        state.diagnostics = await invoke<GatewayDiagnosticReport>("gateway_diagnostics");
        notify("診断が完了しました。", state.diagnostics.errors ? "error" : "success");
      });
      return;
    case "probe-local-clis":
      await withBusy("local-clis", async () => {
        state.localClis = await invoke<LocalCliScanReport>("scan_local_cli_clients", { deep: true });
        const ready = state.localClis.clients.filter((client) => client.probeState === "ready").length;
        const needsRegistration = state.localClis.clients.filter((client) => client.needsCodetasRegistration).length;
        notify(
          needsRegistration
            ? `${ready} 件の CLI で非対話実行を確認しました。${needsRegistration} 件は「使う」で取り込みます。`
            : `${ready} 件の CLI で非対話実行を確認しました。`,
          "info",
        );
      });
      return;
    case "register-direct-api": {
      const providerId = target.dataset.providerId!;
      if (!window.confirm(`${providerId} を CODETAS に登録します。API キーは接続の編集画面で Keychain または環境変数参照として保存します。続行しますか？`)) return;
      await withBusy("local-clis", async () => {
        const report = await invoke<{ providerId: string; credentialNeeded: boolean; message: string }>("register_codetas_provider", { providerId });
        await refreshStatusAndConfig();
        if (report.credentialNeeded) state.editingProviderId = report.providerId;
        notify(report.message, report.credentialNeeded ? "info" : "success");
      });
      return;
    }
    case "register-local-cli": {
      const clientId = target.dataset.clientId!;
      if (!window.confirm("既存の CLI ログインがあれば取り込み、なければブラウザでログインします。続行しますか？")) return;
      await withBusy("local-clis", async () => {
        const report = await invoke<{ providerId: string; credentialNeeded: boolean; message: string }>("register_local_cli_in_codetas", { clientId });
        await refreshStatusAndConfig();
        if (report.credentialNeeded) {
          state.editingProviderId = report.providerId;
        }
        notify(report.message, report.credentialNeeded ? "info" : "success");
      });
      return;
    }
    case "install-codex":
      await withBusy("codex", async () => {
        const path = await invoke<string>("install_codex_gateway_config", { input: { model: null } });
        await refreshStatusAndConfig();
        notify(`Codex 接続を更新しました: ${path}`);
      });
      return;
    case "restore-codex":
      if (!window.confirm("Codex の CODETAS 接続を解除して、接続前の設定へ戻しますか？Codex のセッションは削除しません。")) return;
      await withBusy("codex", async () => {
        const report = await invoke<CodexRestoreReport>("restore_codex_gateway_config");
        await refreshStatusAndConfig();
        const suffix = report.conflicts.length ? `（確認事項: ${report.conflicts.join(" / ")}）` : "";
        notify(report.restored ? `Codex を接続前の設定へ戻しました。${suffix}` : `復元対象はありませんでした。${suffix}`, report.conflicts.length ? "info" : "success");
      });
      return;
    case "sync-catalog":
      await withBusy("catalog", async () => {
        const path = await invoke<string>("sync_codex_model_catalog");
        await refreshStatusAndConfig();
        notify(`モデル一覧を同期しました: ${path}`);
      });
      return;
    case "test-provider": {
      const providerId = target.dataset.providerId!;
      await withBusy(`test:${providerId}`, async () => {
        const report = await invoke<ProviderConnectionReport>("test_gateway_provider", { providerId });
        notify(report.reachable ? `${providerId}: ${report.message} (${report.latencyMs}ms)` : `${providerId}: ${report.message}`, report.reachable ? "success" : "error");
      });
      return;
    }
    case "oauth-provider": {
      const providerId = target.dataset.providerId!;
      await withBusy("oauth", async () => {
        const report = await invoke<ProviderOAuthLaunchReport>("launch_provider_oauth_broker", {
          input: { providerId, broker: null },
        });
        state.configuration = await invoke<GatewayConfiguration>("gateway_configuration");
        notify(report.instructions, "info");
      });
      return;
    }
    case "refresh-models": {
      const providerId = target.dataset.providerId!;
      await withBusy(`models:${providerId}`, async () => {
        state.configuration = await invoke<GatewayConfiguration>("refresh_gateway_provider_models", { providerId });
        state.status = await invoke<GatewayStatus>("provider_gateway_status");
        notify(`${providerId} のモデル一覧を更新しました。`);
      });
      return;
    }
    case "default-provider": {
      const providerId = target.dataset.providerId!;
      await withBusy("provider", async () => {
        state.status = await invoke<GatewayStatus>("set_default_gateway_provider", { providerId });
        state.configuration = await invoke<GatewayConfiguration>("gateway_configuration");
        notify(`${providerId} を既定の接続にしました。`);
      });
      return;
    }
    case "edit-provider": state.editingProviderId = target.dataset.providerId ?? null; render(); return;
    case "close-provider-editor": state.editingProviderId = null; render(); return;
    case "remove-provider": {
      const providerId = target.dataset.providerId!;
      if (!window.confirm(`${providerId} を CODETAS 設定から削除しますか？資格情報の実体は削除しません。`)) return;
      await withBusy("provider", async () => {
        state.status = await invoke<GatewayStatus>("remove_gateway_provider", { providerId });
        state.configuration = await invoke<GatewayConfiguration>("gateway_configuration");
        state.editingProviderId = null;
        notify(`${providerId} を削除しました。`);
      });
      return;
    }
    case "pick-project": {
      await withBusy("project", async () => {
        const path = await invoke<string | null>("pick_project");
        if (!path) return;
        state.project = await invoke<ProjectInspection>("inspect_project", { path });
        state.syncPlan = null;
      });
      return;
    }
    case "add-route-row": {
      const config = state.configuration!;
      config.routes.push({ id: `route-${config.routes.length + 1}`, name: "新しい経路", alias: null, strategy: "failover", targets: [], stickyRequests: 1, failureThreshold: 3, defaultReasoningEffort: null, enabled: true });
      render();
      return;
    }
    case "remove-route": {
      const index = Number(target.dataset.routeIndex);
      state.configuration?.routes.splice(index, 1);
      render();
      return;
    }
    case "save-routes": await saveRoutesFromDom(); return;
    case "install-service":
      await withBusy("service", async () => {
        await invoke("install_gateway_service", { input: { installShim: true } });
        await refreshAll();
        notify("常駐サービスを登録しました。");
      });
      return;
    case "start-service":
      await withBusy("service", async () => {
        state.service = await invoke<GatewayServiceStatus>("start_gateway_service");
        await refreshStatusAndConfig();
        notify("常駐サービスを起動しました。");
      });
      return;
    case "restart-service":
      await withBusy("service", async () => {
        state.service = await invoke<GatewayServiceStatus>("restart_gateway_service");
        await refreshStatusAndConfig();
        notify("常駐サービスを監督下で再起動しました。");
      });
      return;
    case "stop-service":
      await withBusy("service", async () => {
        state.service = await invoke<GatewayServiceStatus>("stop_gateway_service");
        await refreshStatusAndConfig();
        notify("常駐サービスを停止しました。", "info");
      });
      return;
    case "uninstall-service":
      if (!window.confirm("常駐サービスと起動 shim を解除しますか？Gateway 設定は残ります。")) return;
      await withBusy("service", async () => {
        await invoke("uninstall_gateway_service");
        await refreshAll();
        notify("常駐サービスを解除しました。");
      });
      return;
    case "copy-config": {
      const textarea = document.querySelector<HTMLTextAreaElement>("#advanced-json");
      if (textarea) await navigator.clipboard.writeText(textarea.value);
      notify("設定JSONをコピーしました。", "info");
      return;
    }
    case "check-update":
      await withBusy("update", async () => {
        const check = await invoke<UpdateCheck>("check_for_codetas_update");
        if (!check.updateAvailable) {
          notify(`CODETAS ${check.currentVersion} は最新です。`, "info");
          return;
        }
        if (!window.confirm(`CODETAS ${check.manifest.version} をダウンロードして適用しますか？Gateway は適用直前に安全に停止します。`)) {
          notify(`CODETAS ${check.manifest.version} を利用できます。`, "info");
          return;
        }
        state.notice = { tone: "info", text: "署名済み更新を検証して適用しています。アプリは完了後に再起動します。" };
        render();
        await invoke("install_codetas_update");
      });
      return;
    case "preview-cleanup":
      await withBusy("storage", async () => {
        state.cleanupPreview = await invoke<ObservabilityCleanupPreview>("preview_gateway_observability_cleanup");
        notify(state.cleanupPreview.files.length ? `${state.cleanupPreview.files.length} 件の整理対象があります。` : "現在の保持ポリシーで整理対象はありません。", "info");
      });
      return;
    case "trash-cleanup":
      if (!window.confirm("整理対象を CODETAS 専用のごみ箱へ移しますか？後から復元できます。")) return;
      await withBusy("storage", async () => {
        const report = await invoke<ObservabilityTrashReport | null>("trash_gateway_observability_cleanup");
        state.cleanupPreview = null;
        state.trashEntries = await invoke<ObservabilityTrashEntry[]>("list_gateway_observability_trash");
        state.observability = await invoke<ObservabilitySummary>("gateway_observability_summary");
        state.breakdown = await invoke<ObservabilityBreakdown>("gateway_observability_breakdown", { sinceMs: 0, maxEvents: 50_000 });
        notify(report ? `${report.files} 件を専用ごみ箱へ移しました。` : "整理対象はありませんでした。", "info");
      });
      return;
    case "restore-trash": {
      const transactionId = target.dataset.transactionId!;
      await withBusy("storage", async () => {
        const report = await invoke<ObservabilityTrashReport>("restore_gateway_observability_trash", { transactionId });
        state.trashEntries = await invoke<ObservabilityTrashEntry[]>("list_gateway_observability_trash");
        state.observability = await invoke<ObservabilitySummary>("gateway_observability_summary");
        state.breakdown = await invoke<ObservabilityBreakdown>("gateway_observability_breakdown", { sinceMs: 0, maxEvents: 50_000 });
        notify(`${report.files} 件の記録を復元しました。`);
      });
      return;
    }
  }
}

async function handleForm(form: HTMLFormElement): Promise<void> {
  const data = new FormData(form);
  switch (form.id) {
    case "preset-form": {
      const presetId = String(data.get("presetId") ?? "");
      if (!presetId) return;
      await withBusy("preset", async () => {
        state.status = await invoke<GatewayStatus>("install_provider_preset", { input: { presetId, baseUrl: String(data.get("baseUrl") ?? "").trim() || null, makeDefault: data.get("makeDefault") === "on" } });
        state.configuration = await invoke<GatewayConfiguration>("gateway_configuration");
        notify(`${presetId} を追加しました。資格情報の参照を編集してください。`);
      });
      return;
    }
    case "provider-editor-form": await saveProviderForm(data); return;
    case "agents-form": await saveAgentForm(data); return;
    case "sync-plan-form": {
      if (!state.project) return;
      state.syncPlan = createSyncPlan(state.project, { context: data.get("context") === "on", skills: data.get("skills") === "on", mcp: data.get("mcp") === "on" });
      render();
      return;
    }
    case "clients-form": await syncClients(data); return;
    case "settings-form": await saveSettingsForm(data); return;
  }
}

async function saveProviderForm(data: FormData): Promise<void> {
  const config = state.configuration!;
  const id = String(data.get("id"));
  const current = config.providers.find((provider) => provider.id === id);
  if (!current) return;
  const source = String(data.get("credentialSource")) as CredentialSource;
  const reference = String(data.get("credentialReference") ?? "").trim() || null;
  const provider: ProviderDefinition = structuredClone(current);
  provider.name = String(data.get("name"));
  provider.baseUrl = String(data.get("baseUrl"));
  provider.defaultModel = String(data.get("defaultModel") ?? "").trim() || null;
  provider.protocol = String(data.get("protocol")) as ProviderDefinition["protocol"];
  provider.transport = String(data.get("providerTransport")) as ProviderDefinition["transport"];
  provider.googleMode = String(data.get("googleMode")) as ProviderDefinition["googleMode"];
  provider.project = String(data.get("project") ?? "").trim() || null;
  provider.location = String(data.get("location") ?? "").trim() || null;
  provider.azureDeployment = String(data.get("azureDeployment") ?? "").trim() || null;
  provider.azureApiVersion = String(data.get("azureApiVersion") ?? "").trim() || null;
  provider.kiroProfileArn = String(data.get("kiroProfileArn") ?? "").trim() || null;
  provider.responsesPath = String(data.get("responsesPath") ?? "").trim() || null;
  provider.realtimeWsBaseUrl = String(data.get("realtimeWsBaseUrl") ?? "").trim() || null;
  provider.limits = {
    ...(current.limits ?? {
      connectTimeoutMs: 15_000,
      requestTimeoutMs: 300_000,
      streamIdleTimeoutMs: 300_000,
      maxRequestBytes: 16 * 1024 ** 2,
      maxResponseBytes: 32 * 1024 ** 2,
    }),
    requestRetries: Number(data.get("requestRetries")),
    streamRetries: Number(data.get("streamRetries")),
  };
  provider.statelessResponses = data.get("statelessResponses") === "on";
  provider.stripModelBracketSuffix = data.get("stripModelBracketSuffix") === "on";
  provider.enabled = data.get("enabled") === "on";
  provider.allowPrivateNetwork = data.get("allowPrivateNetwork") === "on";
  provider.apiKeyEnv = source === "environment" ? reference : null;
  provider.credential = {
    source,
    reference: source === "none" || source === "forward" ? null : reference,
    transport: String(data.get("credentialTransport")) as CredentialTransport,
    headerName:
      String(data.get("credentialTransport")) === "customHeader"
        ? current.credential?.headerName ?? null
        : null,
    command:
      source === "oAuth" || source === "command"
        ? current.credential?.command ?? null
        : null,
  };
  await withBusy("provider", async () => {
    state.status = await invoke<GatewayStatus>("upsert_gateway_provider", { input: { provider, makeDefault: false } });
    state.configuration = await invoke<GatewayConfiguration>("gateway_configuration");
    state.editingProviderId = null;
    notify(`${id} を更新しました。`);
  });
}

async function saveRoutesFromDom(): Promise<void> {
  const config = state.configuration!;
  const rows = [...document.querySelectorAll<HTMLElement>(".route-editor")];
  const routes = rows.map((row) => {
    const value = (field: string) => row.querySelector<HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement>(`[data-field="${field}"]`)!;
    const targets = lines(value("targets").value).map((entry) => {
      const match = entry.match(/^(.*?)(?:@(\d+))?$/);
      return { model: match?.[1]?.trim() ?? entry, weight: Math.max(1, Number(match?.[2] ?? 1)) };
    });
    return {
      id: value("id").value.trim(), name: value("name").value.trim(), alias: value("alias").value.trim() || null,
      strategy: value("strategy").value as GatewayConfiguration["routes"][number]["strategy"], targets,
      stickyRequests: config.routes[Number(row.dataset.routeIndex)]?.stickyRequests ?? 1,
      failureThreshold: config.routes[Number(row.dataset.routeIndex)]?.failureThreshold ?? 3,
      defaultReasoningEffort: value("defaultReasoningEffort").value.trim() || null,
      enabled: (value("enabled") as HTMLInputElement).checked,
    };
  });
  await saveConfiguration({ ...config, routes }, "モデル経路を保存しました。");
}

async function saveAgentForm(data: FormData): Promise<void> {
  const config = structuredClone(state.configuration!);
  config.agents.multiAgentV2 = data.get("multiAgentV2") === "on";
  config.agents.surfaceMode = String(data.get("surfaceMode")) as GatewayConfiguration["agents"]["surfaceMode"];
  config.agents.maxThreads = Number(data.get("maxThreads"));
  config.agents.effortCap = String(data.get("effortCap") ?? "").trim() || null;
  config.agents.subagentModels = lines(data.get("subagentModels"));
  config.agents.subagentFallback = lines(data.get("subagentFallback"));
  for (const key of ["webSearchModel", "visionModel", "imageModel", "videoModel", "liveModel"] as const) {
    config.sidecars[key] = String(data.get(key) ?? "").trim() || null;
  }
  await saveConfiguration(config, "エージェント設定を保存しました。");
}

async function syncClients(data: FormData): Promise<void> {
  const input: ExternalClientIntegrationInput = {
    claudeCode: data.get("claudeCode") === "on", claudeDesktop: data.get("claudeDesktop") === "on",
    opencode: data.get("opencode") === "on", grok: data.get("grok") === "on", pi: data.get("pi") === "on",
  };
  await withBusy("clients", async () => {
    const report = await invoke<ClientIntegrationReport>("sync_client_integrations", { input });
    state.configuration = await invoke<GatewayConfiguration>("gateway_configuration");
        notify(`${report.clients.filter((client) => client.enabled).length} 件のクライアント連携を生成しました。`);
  });
}

async function saveSettingsForm(data: FormData): Promise<void> {
  const raw = document.querySelector<HTMLTextAreaElement>("#advanced-json")?.value;
  let config: GatewayConfiguration;
  try {
    config = raw ? JSON.parse(raw) as GatewayConfiguration : structuredClone(state.configuration!);
  } catch {
    notify("設定JSONを解析できません。構文を確認してください。", "error");
    return;
  }
  config.runtime.host = String(data.get("host"));
  config.runtime.port = Number(data.get("port"));
  config.runtime.shutdownTimeoutMs = Number(data.get("shutdownTimeoutMs"));
  config.runtime.dynamicPortFallback = data.get("dynamicPortFallback") === "on";
  config.runtime.autoStart = data.get("autoStart") === "on";
  config.codex.autoSyncCatalog = data.get("autoSyncCatalog") === "on";
  config.security.requireLocalToken = data.get("requireLocalToken") === "on";
  config.security.dnsPinning = data.get("dnsPinning") === "on";
  config.security.allowRemote = data.get("allowRemote") === "on";
  config.security.corsAllowOrigins = lines(data.get("corsAllowOrigins"));
  config.observability.requestLog = data.get("requestLog") === "on";
  config.observability.usageLog = data.get("usageLog") === "on";
  config.observability.retentionDays = Number(data.get("retentionDays"));
  config.observability.maxStorageBytes = Number(data.get("maxStorageMb")) * 1024 ** 2;
  config.observability.trashRetentionDays = Number(data.get("trashRetentionDays"));
  config.observability.maxTrashBytes = Number(data.get("maxTrashMb")) * 1024 ** 2;
  config.updates.autoCheck = data.get("updateAutoCheck") === "on";
  config.updates.manifestUrl = String(data.get("manifestUrl") ?? "").trim() || null;
  config.updates.publicKeyBase64 = String(data.get("publicKeyBase64") ?? "").trim() || null;
  config.updates.installerEndpoint = String(data.get("installerEndpoint") ?? "").trim() || null;
  config.updates.installerPublicKey = String(data.get("installerPublicKey") ?? "").trim() || null;
  await saveConfiguration(config, "Gateway設定を保存しました。");
}

async function saveConfiguration(config: GatewayConfiguration, message: string): Promise<void> {
  await withBusy("configuration", async () => {
    state.configuration = await invoke<GatewayConfiguration>("save_gateway_configuration", { configuration: config });
    state.status = await invoke<GatewayStatus>("provider_gateway_status");
    notify(message);
  });
}

void refreshAll().then(async () => {
  try {
    state.diagnostics = await invoke<GatewayDiagnosticReport>("gateway_diagnostics");
  } catch {
    // Overview remains usable when optional diagnostics cannot be collected.
  }
  if (state.configuration?.updates.autoCheck) {
    try {
      const check = await invoke<UpdateCheck>("check_for_codetas_update");
      if (check.updateAvailable) {
        state.notice = {
          tone: "info",
          text: `CODETAS ${check.manifest.version} を利用できます。設定画面から二重署名を確認して適用できます。`,
        };
      }
    } catch {
      // An unavailable update service must not interfere with local gateway startup.
    }
  }
  render();
});
