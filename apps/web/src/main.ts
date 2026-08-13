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
import { getLanguage, setLanguage, t, type Language } from "./i18n";
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

const navigation: Array<{ id: View; key: string }> = [
  { id: "overview", key: "nav.overview" },
  { id: "providers", key: "nav.providers" },
  { id: "routing", key: "nav.routing" },
  { id: "agents", key: "nav.agents" },
  { id: "projects", key: "nav.projects" },
  { id: "clients", key: "nav.clients" },
  { id: "settings", key: "nav.settings" },
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
        <nav class="nav" aria-label="${t("shell.navLabel")}">
          ${navigation.map((item) => `
            <button class="nav-item ${state.view === item.id ? "selected" : ""}" data-view="${item.id}" type="button">
              <span class="nav-label">${h(t(item.key))}</span>
            </button>
          `).join("")}
        </nav>
        <div class="rail-runtime">
          <div>${statusDot(Boolean(state.status?.running))}<span>${t("shell.gateway")}</span></div>
          <strong>${state.status?.running ? t("runtime.running") : t("runtime.stopped")}</strong>
          <code>${h(state.status?.url ?? t("runtime.notStarted"))}</code>
        </div>
      </aside>
      <main class="workspace">
        <header class="topbar">
          <div>
            <h1>${h(t(activeNav.key))}</h1>
          </div>
          <div class="top-actions">
            <span class="provider-count">${t("shell.providerCount", { n: formatNumber(state.configuration?.providers.length) })}</span>
            <button class="icon-button language-toggle" data-action="toggle-language" aria-label="Language" title="Language" type="button">${getLanguage() === "ja" ? "EN" : "日本語"}</button>
            <button class="icon-button" data-action="refresh-all" aria-label="${t("shell.refresh")}" title="${t("shell.refresh")}" type="button">↻</button>
          </div>
        </header>
        ${state.notice ? `<div class="notice ${state.notice.tone}" role="status"><span>${h(state.notice.text)}</span><button data-action="dismiss-notice" type="button" aria-label="×">×</button></div>` : ""}
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
      <h2>${t("loading.title")}</h2>
      <p>${t("loading.subtitle")}</p>
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
          <span class="eyebrow">${t("overview.eyebrow")}</span>
          <h2>${status.running && status.codexConfigured
            ? t("overview.hero.ready")
            : status.running
              ? t("overview.hero.runningNoCodex")
              : status.codexConfigured
                ? t("overview.hero.stoppedCodex")
                : t("overview.hero.stoppedNoCodex")}</h2>
          <p>${t("overview.hero.explainer")}</p>
          <div class="button-row">
            <button class="primary" data-action="${status.running ? "stop-gateway" : "start-gateway"}" type="button" ${isBusy("gateway") ? "disabled" : ""}>
              ${isBusy("gateway") ? t("overview.hero.working") : status.running ? t("overview.hero.stopGateway") : t("overview.hero.startGateway")}
            </button>
            ${status.codexConfigured
              ? `<button class="text-button" data-action="restore-codex" type="button" ${isBusy("codex") ? "disabled" : ""}>${t("overview.hero.disconnectCodex")}</button>`
              : `<button class="secondary" data-action="install-codex" type="button">${t("overview.hero.connectCodex")}</button>`}
          </div>
        </div>
        <div class="hero-status">
          <div class="status-list">
            <div class="status-row ${status.running ? "ok" : ""}">
              <span class="status-led" aria-hidden="true"></span>
              <div class="status-label"><strong>${t("shell.gateway")}</strong><small>${status.running ? t("runtime.running") : t("runtime.stopped")}</small></div>
              <code>${h(status.url ?? t("runtime.notStarted"))}</code>
            </div>
            <div class="status-row ${status.codexConfigured ? "ok" : ""}">
              <span class="status-led" aria-hidden="true"></span>
              <div class="status-label"><strong>${t("overview.status.codexConnection")}</strong><small>${status.codexConfigured ? t("overview.status.connected") : t("overview.status.notSet")}</small></div>
              <code>${status.codexConfigured ? h(defaultProvider?.name ?? "—") : t("overview.status.needsSetup")}</code>
            </div>
          </div>
          <div class="status-note">${t("overview.status.summary", { provider: defaultProvider?.name ?? "—", n: formatNumber(config.providers.length), routes: formatNumber(config.routes.length) })}</div>
        </div>
      </article>
      <div class="metric-strip">
        <article><span>${t("metric.requests")}</span><strong>${formatNumber(obs?.totalRequests)}</strong><small>${t("metric.success", { n: formatNumber(obs?.successfulRequests) })}</small></article>
        <article><span>${t("metric.tokens")}</span><strong>${formatNumber(obs?.totalTokens)}</strong><small>${t("metric.reasoning", { n: formatNumber(obs?.reasoningTokens) })}</small></article>
        <article><span>${t("metric.models")}</span><strong>${formatNumber(modelCount(config))}</strong><small>${t("metric.routes", { n: formatNumber(config.routes.length) })}</small></article>
        <article><span>${t("metric.storage")}</span><strong>${formatBytes(obs?.storageBytes)}</strong><small>${t("metric.cap", { size: formatBytes(config.observability.maxStorageBytes) })}</small></article>
      </div>
      ${obs?.persistenceError ? `<div class="notice error">${t("metric.recordError", { error: obs.persistenceError })}</div>` : ""}
      <article class="panel diagnostics-panel">
        <header><div><h3>${t("diagnostics.title")}</h3></div><button class="text-button" data-action="run-diagnostics" type="button">${t("diagnostics.rerun")}</button></header>
        ${state.diagnostics ? `
          <div class="diagnostic-score ${errors ? "bad" : warnings ? "warn" : "good"}">
            <strong>${errors ? t("diagnostics.errors", { n: errors }) : warnings ? t("diagnostics.warnings", { n: warnings }) : t("diagnostics.ok")}</strong>
            <span>${t("diagnostics.summary", { passed: state.diagnostics.passed, total: state.diagnostics.checks.length })}</span>
          </div>
          <div class="check-list">${state.diagnostics.checks.slice(0, 5).map((check) => `
            <div><span class="check-mark ${check.level}">${check.level === "pass" ? "✓" : "!"}</span><p><strong>${h(check.summary)}</strong>${check.remediation ? `<small>${h(check.remediation)}</small>` : ""}</p></div>
          `).join("")}</div>
        ` : `<div class="empty-inline">${t("diagnostics.empty")}</div>`}
      </article>
      <article class="panel quick-panel">
        <header><div><h3>${t("setup.title")}</h3></div></header>
        <button class="task-row" data-view="providers" type="button"><b>1</b><span><strong>${t("setup.addConnection")}</strong><small>${t("setup.addConnectionHint")}</small></span><i>→</i></button>
        <button class="task-row" data-action="sync-catalog" type="button"><b>2</b><span><strong>${t("setup.syncCatalog")}</strong><small>${t("setup.syncCatalogHint")}</small></span><i>→</i></button>
        <button class="task-row" data-view="projects" type="button"><b>3</b><span><strong>${t("setup.checkProjects")}</strong><small>${t("setup.checkProjectsHint")}</small></span><i>→</i></button>
      </article>
      <article class="panel usage-map-panel">
        <header><div><h3>${t("usage.title")}</h3></div><span class="legend">${t("usage.events", { n: formatNumber(breakdown?.scannedEvents) })}${breakdown?.truncated ? t("usage.truncated") : ""}</span></header>
        <div class="usage-map-grid">
          <div><h4>${t("usage.providers")}</h4>${renderUsageBars(breakdown?.providers ?? [])}</div>
          <div><h4>${t("usage.surfaces")}</h4>${renderUsageBars(breakdown?.surfaces ?? [])}</div>
          <div><h4>${t("usage.models")}</h4>${renderUsageBars(breakdown?.models.slice(0, 6) ?? [])}</div>
        </div>
      </article>
    </div>`;
}

function renderUsageBars(rows: ObservabilityBreakdown["providers"]): string {
  const max = Math.max(1, ...rows.map((row) => row.requests));
  return rows.slice(0, 6).map((row) => `<div class="usage-bar"><div><strong>${h(row.key)}</strong><span>${formatNumber(row.requests)}</span></div><i><b style="width:${Math.max(2, row.requests / max * 100)}%"></b></i><small>${t("usage.row", { tokens: formatNumber(row.totalTokens), ms: row.requests ? Math.round(row.totalLatencyMs / row.requests) : 0 })}</small></div>`).join("") || `<div class="empty-inline">${t("usage.empty")}</div>`;
}

function renderProviders(): string {
  const config = state.configuration!;
  const availablePresets = state.presets.filter((preset) => !config.providers.some((provider) => provider.id === preset.id));
  return `
    <div class="section-grid providers-layout">
      <section class="panel provider-list-panel">
        <header><div><h2>${t("providers.title", { n: config.providers.length })}</h2></div><span class="legend">${t("providers.legend")}</span></header>
        <div class="provider-list">
          ${config.providers.map(renderProviderCard).join("") || `<div class="empty-state"><strong>${t("providers.empty")}</strong><p>${t("providers.emptyHint")}</p></div>`}
        </div>
        <section class="local-cli-panel">
          <header><div><h3>${t("providers.localCli")}</h3></div><button class="secondary compact" data-action="probe-local-clis" type="button" ${isBusy("local-clis") ? "disabled" : ""}>${isBusy("local-clis") ? t("providers.checking") : t("providers.checkRegistration")}</button></header>
          <p class="local-cli-help">${t("providers.localCliHelp")}</p>
          <div class="local-cli-list">${renderLocalCliRows()}</div>
        </section>
        <section class="local-cli-panel">
          <header><div><h3>${t("providers.directApi")}</h3></div></header>
          <p class="local-cli-help">${t("providers.directApiHelp")}</p>
          <div class="local-cli-list">${renderDirectApiRows()}</div>
        </section>
      </section>
      <aside class="panel add-provider-panel">
        <header><div><h3>${t("add.title")}</h3></div></header>
        <form id="preset-form" class="stack-form">
          <label>${t("add.provider")}
            <select name="presetId" required>
              <option value="">${t("add.select")}</option>
              ${availablePresets.map((preset) => `<option value="${h(preset.id)}">${h(preset.name)}</option>`).join("")}
            </select>
          </label>
          <label>${t("add.customUrl")} <small>${t("add.customUrlHint")}</small><input name="baseUrl" type="url" placeholder="https://api.example.com/v1" /></label>
          <label class="check-control"><input name="makeDefault" type="checkbox" ${config.defaultProvider ? "" : "checked"}/><span>${t("add.makeDefault")}</span></label>
          <button class="primary wide" type="submit" ${isBusy("preset") ? "disabled" : ""}>${isBusy("preset") ? t("add.adding") : t("add.submit")}</button>
        </form>
        <div class="registry-note"><strong>${t("add.remaining", { n: availablePresets.length })}</strong><p>${t("add.remainingHint")}</p></div>
      </aside>
    </div>`;
}

function renderLocalCliRows(): string {
  const rows = state.localClis?.clients ?? [];
  if (!rows.length) return `<div class="empty-inline">${t("providers.emptyLocal")}</div>`;
  return rows.map((client) => {
    const ready = client.probeState === "ready";
    const warning = client.installed && !ready;
    const registered = client.installed && !client.needsCodetasRegistration && Boolean(client.codetasProviderId);
    const label = client.canRegister
      ? t("cli.available")
      : registered
        ? t("cli.registered")
        : client.needsCodetasRegistration
          ? t("cli.import")
          : ready
            ? t("cli.detectedUnsupported")
            : client.installed
              ? t("cli.needsCheck")
              : t("cli.notDetected");
    return `<div class="local-cli-row">
      <div>${statusDot(client.installed, warning || client.needsCodetasRegistration)}<span><strong>${h(client.name)}</strong><code>${h(client.id)}</code></span></div>
      <p>${h(client.message)}${client.registrationHint && client.needsCodetasRegistration ? ` ${h(client.registrationHint)}` : ""}${client.version ? `<small>${h(client.version)}</small>` : ""}</p>
      <span class="chip ${client.canRegister || registered ? "ready" : ""}">${h(label)}</span>
      ${client.needsCodetasRegistration ? `<button class="text-button" data-action="register-local-cli" data-client-id="${h(client.id)}" type="button">${t("cli.use")}</button>` : ""}
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
      <span class="chip ${registered ? "ready" : ""}">${registered ? t("cli.registered") : t("cli.notDetected")}</span>
      ${registered ? "" : `<button class="text-button" data-action="register-direct-api" data-provider-id="${h(target.providerId)}" type="button">${t("cli.register")}</button>`}
    </div>`;
  }).join("") || `<div class="empty-inline">${t("providers.emptyDirect")}</div>`;
}

function renderProviderCard(provider: ProviderDefinition): string {
  const config = state.configuration!;
  const credential = provider.credential;
  const credentialLabel = credential?.source === "environment"
    ? credential.reference ?? provider.apiKeyEnv ?? t("cred.env")
    : credential?.source === "oAuth"
      ? t("cred.login")
      : credential?.source === "forward"
        ? t("cred.codex")
        : credential?.source === "keychain"
          ? t("cred.keychain")
          : credential?.source === "command"
            ? t("cred.command")
            : credential?.source === "none" || !credential?.source
              ? t("cred.none")
              : credential.source;
  return `
    <article class="provider-card ${provider.enabled ? "" : "disabled"}">
      <div class="provider-main">
        <div class="provider-monogram">${h(provider.name.slice(0, 2).toUpperCase())}</div>
        <div><div class="provider-title"><h3>${h(provider.name)}</h3>${config.defaultProvider === provider.id ? `<span class="chip default">${t("card.default")}</span>` : ""}</div><code>${h(provider.id)}</code></div>
      </div>
      <div class="provider-data"><span>${t("card.protocol")}<strong>${h(protocolLabel(provider.protocol))}</strong></span><span>${t("card.models")}<strong>${formatNumber(provider.models.length)}</strong></span><span>${t("card.auth")}<strong>${h(credentialLabel)}</strong></span></div>
      <div class="provider-url"><span>${statusDot(provider.enabled)}</span><code>${h(provider.baseUrl)}</code></div>
      <div class="card-actions">
        <button data-action="test-provider" data-provider-id="${h(provider.id)}" type="button">${t("card.test")}</button>
        ${["github-copilot", "google-vertex", "google-antigravity", "google", "anthropic", "kimi", "xai"].includes(provider.id) ? `<button data-action="oauth-provider" data-provider-id="${h(provider.id)}" type="button">${provider.id === "anthropic" ? t("card.loginClaude") : provider.id === "kimi" ? t("card.loginKimi") : provider.id === "xai" ? t("card.loginGrok") : t("card.loginOAuth")}</button>` : ""}
        <button data-action="refresh-models" data-provider-id="${h(provider.id)}" type="button">${t("card.fetchModels")}</button>
        <button data-action="edit-provider" data-provider-id="${h(provider.id)}" type="button">${t("card.edit")}</button>
        ${config.defaultProvider !== provider.id ? `<button data-action="default-provider" data-provider-id="${h(provider.id)}" type="button">${t("card.makeDefault")}</button>` : ""}
      </div>
    </article>`;
}

function renderRouting(): string {
  const config = state.configuration!;
  const models = allModelIds(config);
  return `
    <div class="section-grid routing-layout">
      <section class="panel routes-panel">
        <header><div><h2>${t("routing.title")}</h2></div><button class="secondary compact" data-action="add-route-row" type="button">${t("routing.addRoute")}</button></header>
        <div id="route-editor-list" class="route-editor-list">
          ${config.routes.map((route, index) => renderRouteEditor(route, index, models)).join("") || `<div class="empty-state"><strong>${t("routing.empty")}</strong><p>${t("routing.emptyHint")}</p></div>`}
        </div>
        <div class="panel-footer"><button class="primary" data-action="save-routes" type="button">${t("routing.save")}</button></div>
      </section>
      <aside class="panel model-index">
        <header><div><h3>${t("routing.models", { n: modelCount(config) })}</h3></div><button class="text-button" data-action="sync-catalog" type="button">${t("routing.syncCodex")}</button></header>
        <div class="model-filter"><input id="model-search" type="search" placeholder="${t("routing.searchModels")}" autocomplete="off" /></div>
        <div id="model-list" class="model-list">${renderModelRows(config, "")}</div>
      </aside>
    </div>`;
}

function renderRouteEditor(route: GatewayConfiguration["routes"][number], index: number, models: string[]): string {
  return `<article class="route-editor" data-route-index="${index}">
    <div class="route-head"><input data-field="name" value="${h(route.name)}" aria-label="${t("route.name")}"/><label class="switch"><input data-field="enabled" type="checkbox" ${route.enabled ? "checked" : ""}/><span></span></label></div>
    <div class="form-grid two"><label>${t("route.id")}<input data-field="id" value="${h(route.id)}" /></label><label>${t("route.alias")}<input data-field="alias" value="${h(route.alias ?? "")}" /></label></div>
    <div class="form-grid two"><label>${t("route.strategy")}<select data-field="strategy"><option value="failover" ${route.strategy === "failover" ? "selected" : ""}>Failover</option><option value="weightedRoundRobin" ${route.strategy === "weightedRoundRobin" ? "selected" : ""}>Weighted round robin</option><option value="leastUsage" ${route.strategy === "leastUsage" ? "selected" : ""}>Least usage</option></select></label><label>${t("route.defaultEffort")}<input data-field="defaultReasoningEffort" value="${h(route.defaultReasoningEffort ?? "")}" placeholder="medium" /></label></div>
    <label>${t("route.targets")} <small>${t("route.targetsHint")}</small><textarea data-field="targets" rows="${Math.max(2, route.targets.length)}">${h(route.targets.map((target) => `${target.model}@${target.weight}`).join("\n"))}</textarea></label>
    <div class="route-foot"><span>${t("route.count", { n: route.targets.length })}</span><button class="danger-link" data-action="remove-route" data-route-index="${index}" type="button">${t("route.remove")}</button></div>
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
    return `<div class="model-row"><div><strong>${h(model)}</strong><code>${h(provider.id)}</code></div><span>${metadata?.contextWindow ? `${formatNumber(metadata.contextWindow / 1000)}k` : "-"}</span><span>${efforts.length ? h(efforts.join(" / ")) : t("route.effortStandard")}</span></div>`;
  }).join("") || `<div class="empty-inline">${t("routing.noModels")}</div>`;
}

function renderAgents(): string {
  const config = state.configuration!;
  const options = allModelIds(config);
  return `
    <form id="agents-form" class="agent-layout">
      <section class="panel agent-core-panel">
        <header><div><h2>${t("agents.parallel")}</h2></div><label class="switch labeled"><input name="multiAgentV2" type="checkbox" ${config.agents.multiAgentV2 ? "checked" : ""}/><span></span><b>${config.agents.multiAgentV2 ? t("agents.on") : t("agents.off")}</b></label></header>
        <div class="agent-topology">
          <div class="agent-main"><span>${t("agents.main")}</span><strong>${h(config.defaultProvider ?? t("agents.default"))}</strong><small>${t("agents.effort")} ${h(config.agents.effortCap ?? t("agents.default"))}</small></div>
          <div class="agent-branches">${Array.from({ length: Math.min(config.agents.maxThreads, 6) }, (_, index) => `<span style="--i:${index}">A${index + 1}</span>`).join("")}</div>
        </div>
        <div class="form-grid three">
          <label>${t("agents.surface")}<select name="surfaceMode"><option value="v1" ${config.agents.surfaceMode === "v1" ? "selected" : ""}>v1 compatible</option><option value="default" ${config.agents.surfaceMode === "default" ? "selected" : ""}>Default</option><option value="v2" ${config.agents.surfaceMode === "v2" ? "selected" : ""}>v2 native</option></select></label>
          <label>${t("agents.maxThreads")}<input name="maxThreads" type="number" min="1" max="64" value="${config.agents.maxThreads}" /></label>
          <label>${t("agents.mainEffort")}<input name="effortCap" value="${h(config.agents.effortCap ?? "")}" placeholder="high" /></label>
        </div>
        <label>${t("agents.subagents")} <small>${t("agents.subagentsHint")}</small><textarea name="subagentModels" rows="4">${h(config.agents.subagentModels.join("\n"))}</textarea></label>
        <label>${t("agents.fallback")} <small>${t("agents.fallbackHint")}</small><textarea name="subagentFallback" rows="3">${h(config.agents.subagentFallback.join("\n"))}</textarea></label>
      </section>
      <aside class="panel sidecar-panel">
        <header><div><h3>${t("agents.sidecar")}</h3></div></header>
        ${renderModelSelect("webSearchModel", "Web search", config.sidecars.webSearchModel, options)}
        ${renderModelSelect("visionModel", "Vision", config.sidecars.visionModel, options)}
        ${renderModelSelect("imageModel", "Image", config.sidecars.imageModel, options)}
        ${renderModelSelect("videoModel", "Video", config.sidecars.videoModel, options)}
        ${renderModelSelect("liveModel", "Realtime", config.sidecars.liveModel, options)}
        <button class="primary wide" type="submit">${t("agents.save")}</button>
      </aside>
    </form>`;
}

function renderModelSelect(name: string, label: string, selected: string | null, models: string[]): string {
  return `<label>${h(label)}<select name="${h(name)}"><option value="">${t("agents.unused")}</option>${models.map((model) => `<option value="${h(model)}" ${selected === model ? "selected" : ""}>${h(model)}</option>`).join("")}</select></label>`;
}

function renderProjects(): string {
  const project = state.project;
  const plan = state.syncPlan;
  return `
    <div class="project-layout">
      <section class="project-intro">
        <h2>${t("projects.tagline")}</h2>
        <p>${t("projects.intro")}</p>
        <button class="primary" data-action="pick-project" type="button">${t("projects.pick")}</button>
      </section>
      <section class="panel project-inspector">
        ${project ? `
          <header><div><h3>${h(project.name)}</h3></div><span class="chip ready">${t("projects.readonly")}</span></header>
          <code class="project-path">${h(project.path)}</code>
          <div class="source-grid">
            ${sourceTile("Context", project.contextFile, "CX")}
            ${sourceTile("Skills", project.skillsDirectory, `${project.skillsCount}`)}
            ${sourceTile("MCP", project.mcpFile, "MC")}
            ${sourceTile("Codex", project.codexConfigFile ?? project.agentsFile, "CD")}
          </div>
          <form id="sync-plan-form" class="sync-options">
            <label class="check-control"><input name="context" type="checkbox" checked/><span>${t("projects.context")}</span></label>
            <label class="check-control"><input name="skills" type="checkbox" checked/><span>${t("projects.skills")}</span></label>
            <label class="check-control"><input name="mcp" type="checkbox" checked/><span>${t("projects.mcp")}</span></label>
            <button class="secondary" type="submit">${t("projects.makePlan")}</button>
          </form>
        ` : `<div class="project-empty"><div class="scan-symbol"><span></span></div><strong>${t("projects.empty")}</strong><p>${t("projects.emptyHint")}</p></div>`}
      </section>
      ${plan ? `<section class="panel sync-plan-panel"><header><div><h3>${t("projects.planCount", { n: plan.actions.length })}</h3></div><span class="chip ready">${t("projects.planSafe")}</span></header><div class="plan-flow">${plan.actions.map((action) => `<div><span class="plan-kind">${h(action.category)}</span><p><strong>${h(action.summary)}</strong><small>${h(action.source)} → ${h(action.target)}</small></p><em class="${action.compatibility}">${h(action.compatibility)}</em></div>`).join("") || `<div class="empty-inline">${t("projects.planEmpty")}</div>`}</div>${plan.warnings.length ? `<div class="warning-box">${plan.warnings.map((warning) => `<p>${h(warning)}</p>`).join("")}</div>` : ""}<p class="plan-note">${t("projects.planNote")}</p></section>` : ""}
    </div>`;
}

function sourceTile(label: string, path: string | null, monogram: string): string {
  return `<article class="source-tile ${path ? "found" : "missing"}"><span>${h(monogram)}</span><div><strong>${h(label)}</strong><small>${path ? h(path.split(/[\\/]/).at(-1)) : t("projects.notFound")}</small></div></article>`;
}

function renderClients(): string {
  const config = state.configuration!;
  const clients: Array<[keyof ExternalClientIntegrationInput, string, string]> = [
    ["claudeCode", "Claude Code", t("client.claudeCode")],
    ["claudeDesktop", "Claude Desktop", t("client.claudeDesktop")],
    ["opencode", "OpenCode", t("client.opencode")],
    ["grok", "Grok", t("client.grok")],
    ["pi", "Pi", t("client.pi")],
  ];
  return `
    <div class="clients-layout">
      <form id="clients-form" class="panel client-list-panel">
        <header><div><h2>${t("clients.title")}</h2></div></header>
        ${clients.map(([key, name, detail]) => `<label class="client-row"><span class="client-glyph">${h(name.slice(0, 2))}</span><span><strong>${h(name)}</strong><small>${h(detail)}</small></span><input name="${key}" type="checkbox" ${config.integrations[key] ? "checked" : ""}/></label>`).join("")}
        <div class="panel-footer"><button class="primary" type="submit">${t("clients.generate")}</button></div>
      </form>
      <aside class="panel service-panel">
        <header><div><h3>${t("service.title")}</h3></div>${statusDot(Boolean(state.service?.running))}</header>
        <div class="service-state"><strong>${state.service?.running ? t("service.running") : state.service?.installed ? t("service.stopped") : t("service.notInstalled")}</strong><p>${h(state.service?.message ?? t("service.loading"))}</p>${state.service?.installed ? `<small>${h(state.service.supervisor)} · ${h(state.service.restartPolicy)}</small>` : ""}</div>
        <div class="stack-actions">
          ${state.service?.installed
            ? `${state.service.running ? `<button class="secondary wide" data-action="restart-service" type="button">${t("service.restart")}</button><button class="text-button wide" data-action="stop-service" type="button">${t("service.stop")}</button>` : `<button class="secondary wide" data-action="start-service" type="button">${t("service.start")}</button>`}<button class="danger-link wide" data-action="uninstall-service" type="button">${t("service.uninstall")}</button>`
            : `<button class="primary wide" data-action="install-service" type="button">${t("service.install")}</button>`}
        </div>
      </aside>
    </div>`;
}

function renderSettings(): string {
  const config = state.configuration!;
  return `
    <form id="settings-form" class="settings-layout">
      <section class="panel settings-section">
        <header><div><h2>${t("settings.gateway")}</h2></div></header>
        <div class="form-grid two"><label>${t("settings.host")}<input name="host" value="${h(config.runtime.host)}" /></label><label>${t("settings.port")}<input name="port" type="number" min="1" max="65535" value="${config.runtime.port}" /></label></div>
        <label>${t("settings.shutdownTimeout")}<input name="shutdownTimeoutMs" type="number" min="100" max="300000" value="${config.runtime.shutdownTimeoutMs}" /></label>
        <label class="check-control"><input name="dynamicPortFallback" type="checkbox" ${config.runtime.dynamicPortFallback !== false ? "checked" : ""}/><span>${t("settings.dynamicPort")}</span></label>
        <label class="check-control"><input name="autoStart" type="checkbox" ${config.runtime.autoStart ? "checked" : ""}/><span>${t("settings.autoStart")}</span></label>
        <label class="check-control"><input name="autoSyncCatalog" type="checkbox" ${config.codex.autoSyncCatalog ? "checked" : ""}/><span>${t("settings.autoSyncCatalog")}</span></label>
      </section>
      <section class="panel settings-section">
        <header><div><h2>${t("settings.security")}</h2></div></header>
        <label class="check-control"><input name="requireLocalToken" type="checkbox" ${config.security.requireLocalToken ? "checked" : ""}/><span>${t("settings.requireToken")}</span></label>
        <label class="check-control"><input name="dnsPinning" type="checkbox" ${config.security.dnsPinning ? "checked" : ""}/><span>${t("settings.dnsPinning")}</span></label>
        <label class="check-control"><input name="allowRemote" type="checkbox" ${config.security.allowRemote ? "checked" : ""}/><span>${t("settings.allowRemote")}</span></label>
        <label>${t("settings.cors")} <small>${t("settings.corsHint")}</small><textarea name="corsAllowOrigins" rows="4">${h(config.security.corsAllowOrigins.join("\n"))}</textarea></label>
      </section>
      <section class="panel settings-section">
        <header><div><h2>${t("settings.records")}</h2></div></header>
        <label class="check-control"><input name="requestLog" type="checkbox" ${config.observability.requestLog ? "checked" : ""}/><span>${t("settings.requestLog")}</span></label>
        <label class="check-control"><input name="usageLog" type="checkbox" ${config.observability.usageLog ? "checked" : ""}/><span>${t("settings.usageLog")}</span></label>
        <div class="form-grid two"><label>${t("settings.retentionDays")}<input name="retentionDays" type="number" min="1" max="3650" value="${config.observability.retentionDays}" /></label><label>${t("settings.maxStorageMb")}<input name="maxStorageMb" type="number" min="1" max="10240" value="${Math.max(1, Math.round(config.observability.maxStorageBytes / 1024 ** 2))}" /></label></div>
        <div class="form-grid two"><label>${t("settings.trashRetentionDays")}<input name="trashRetentionDays" type="number" min="1" max="365" value="${config.observability.trashRetentionDays}" /></label><label>${t("settings.maxTrashMb")}<input name="maxTrashMb" type="number" min="1" max="10240" value="${Math.max(1, Math.round(config.observability.maxTrashBytes / 1024 ** 2))}" /></label></div>
      </section>
      <section class="panel settings-section update-section">
        <header><div><h2>${t("settings.updates")}</h2></div><button class="text-button" data-action="check-update" type="button">${t("settings.checkUpdate")}</button></header>
        <label class="check-control"><input name="updateAutoCheck" type="checkbox" ${config.updates.autoCheck ? "checked" : ""}/><span>${t("settings.autoCheck")}</span></label>
        <label>${t("settings.manifestUrl")}<input name="manifestUrl" type="url" value="${h(config.updates.manifestUrl ?? "")}" placeholder="https://releases.example/latest.signed.json" /></label>
        <label>${t("settings.manifestKey")}<input name="publicKeyBase64" value="${h(config.updates.publicKeyBase64 ?? "")}" autocomplete="off" /></label>
        <label>${t("settings.installerEndpoint")}<input name="installerEndpoint" type="url" value="${h(config.updates.installerEndpoint ?? "")}" placeholder="https://releases.example/latest.json" /></label>
        <label>${t("settings.installerKey")}<input name="installerPublicKey" value="${h(config.updates.installerPublicKey ?? "")}" autocomplete="off" /></label>
        <p class="field-note">${t("settings.updateNote")}</p>
      </section>
      <section class="panel settings-section storage-section">
        <header><div><h2>${t("settings.storage")}</h2></div><button class="text-button" data-action="preview-cleanup" type="button">${t("settings.previewCleanup")}</button></header>
        ${state.cleanupPreview ? `<div class="storage-preview"><strong>${state.cleanupPreview.files.length} files / ${formatBytes(state.cleanupPreview.totalBytesBefore - state.cleanupPreview.bytesAfter)}</strong><p>${formatBytes(state.cleanupPreview.totalBytesBefore)} → ${formatBytes(state.cleanupPreview.bytesAfter)}</p>${state.cleanupPreview.files.length ? `<button class="secondary wide" data-action="trash-cleanup" type="button">${t("settings.moveToTrash")}</button>` : ""}</div>` : `<p class="storage-copy">${t("settings.storageCopy")}</p>`}
        <div class="trash-list"><h4>${t("settings.trashHistory")}</h4>${state.trashEntries.map((entry) => `<div><span><strong>${new Date(entry.createdAtMs).toLocaleString("ja-JP")}</strong><small>${entry.files} files · ${formatBytes(entry.bytes)}</small></span><button class="text-button" data-action="restore-trash" data-transaction-id="${h(entry.transactionId)}" type="button">${t("settings.restore")}</button></div>`).join("") || `<p>${t("settings.trashEmpty")}</p>`}</div>
      </section>
      <section class="panel settings-section advanced-config">
        <header><div><h2>${t("settings.json")}</h2></div><button class="text-button" data-action="copy-config" type="button">${t("settings.copy")}</button></header>
        <p>${t("settings.jsonNote")}</p>
        <textarea id="advanced-json" spellcheck="false" rows="18" aria-label="${t("settings.json")}"></textarea>
      </section>
      <div class="settings-savebar"><span>${t("settings.saveNote")}</span><button class="primary" type="submit">${t("settings.save")}</button></div>
    </form>`;
}

function renderProviderEditor(): string {
  const provider = state.configuration?.providers.find((item) => item.id === state.editingProviderId);
  if (!provider) return "";
  const credential = provider.credential ?? {
    source: "none", reference: null, transport: "bearer", headerName: null, command: null,
  };
  return `<div class="drawer-scrim" data-action="close-provider-editor"><aside class="provider-drawer" role="dialog" aria-modal="true" aria-labelledby="provider-editor-title" data-stop-close>
    <header><div><h2 id="provider-editor-title">${h(provider.name)}</h2></div><button class="icon-button" data-action="close-provider-editor" type="button" aria-label="${t("drawer.close")}">×</button></header>
    <form id="provider-editor-form" class="drawer-form">
      <input name="id" type="hidden" value="${h(provider.id)}" />
      <div class="form-grid two"><label>${t("drawer.displayName")}<input name="name" value="${h(provider.name)}" required /></label><label>${t("drawer.defaultModel")}<input name="defaultModel" value="${h(provider.defaultModel ?? "")}" /></label></div>
      <label>${t("drawer.baseUrl")}<input name="baseUrl" type="url" value="${h(provider.baseUrl)}" required /></label>
      <div class="form-grid two"><label>${t("drawer.protocol")}<select name="protocol"><option value="responses" ${provider.protocol === "responses" ? "selected" : ""}>Responses</option><option value="chatCompletions" ${provider.protocol === "chatCompletions" ? "selected" : ""}>Chat Completions</option><option value="anthropicMessages" ${provider.protocol === "anthropicMessages" ? "selected" : ""}>Anthropic Messages</option><option value="geminiGenerateContent" ${provider.protocol === "geminiGenerateContent" ? "selected" : ""}>Gemini generateContent</option></select></label><label>${t("drawer.transport")}<select name="providerTransport"><option value="standard" ${provider.transport === "standard" || !provider.transport ? "selected" : ""}>Standard HTTP / SSE</option><option value="kiro" ${provider.transport === "kiro" ? "selected" : ""}>Kiro event-stream</option><option value="githubCopilot" ${provider.transport === "githubCopilot" ? "selected" : ""}>GitHub Copilot exchange</option></select></label></div>
      <div class="form-grid two"><label>${t("drawer.googleMode")}<select name="googleMode"><option value="aiStudio" ${provider.googleMode === "aiStudio" || !provider.googleMode ? "selected" : ""}>AI Studio</option><option value="vertex" ${provider.googleMode === "vertex" ? "selected" : ""}>Vertex</option><option value="cloudCodeAssist" ${provider.googleMode === "cloudCodeAssist" ? "selected" : ""}>Cloud Code Assist</option></select></label><label>${t("drawer.location")}<input name="location" value="${h(provider.location ?? "")}" /></label></div>
      <label>${t("drawer.project")}<input name="project" value="${h(provider.project ?? "")}" /></label>
      <div class="form-grid two"><label>${t("drawer.azureDeployment")}<input name="azureDeployment" value="${h(provider.azureDeployment ?? "")}" /></label><label>${t("drawer.azureApiVersion")}<input name="azureApiVersion" value="${h(provider.azureApiVersion ?? "")}" placeholder="2025-04-01-preview" /></label></div>
      <label>${t("drawer.kiroArn")} <small>${t("drawer.kiroArnHint")}</small><input name="kiroProfileArn" value="${h(provider.kiroProfileArn ?? "")}" /></label>
      <div class="form-grid two"><label>${t("drawer.responsesPath")}<input name="responsesPath" value="${h(provider.responsesPath ?? "")}" placeholder="/responses" /></label><label>${t("drawer.realtimeWs")}<input name="realtimeWsBaseUrl" type="url" value="${h(provider.realtimeWsBaseUrl ?? "")}" /></label></div>
      <div class="form-grid two"><label>${t("drawer.requestRetries")}<input name="requestRetries" type="number" min="0" max="10" value="${provider.limits?.requestRetries ?? 2}" /></label><label>${t("drawer.streamRetries")}<input name="streamRetries" type="number" min="0" max="10" value="${provider.limits?.streamRetries ?? 2}" /></label></div>
      <div class="toggle-pair"><label class="check-control"><input name="statelessResponses" type="checkbox" ${provider.statelessResponses ? "checked" : ""}/><span>${t("drawer.stateless")}</span></label><label class="check-control"><input name="stripModelBracketSuffix" type="checkbox" ${provider.stripModelBracketSuffix ? "checked" : ""}/><span>${t("drawer.stripSuffix")}</span></label></div>
      <div class="drawer-rule"></div>
      <div class="form-grid two"><label>${t("drawer.credential")}<select name="credentialSource"><option value="none" ${credential.source === "none" ? "selected" : ""}>${t("cred.none")}</option><option value="environment" ${credential.source === "environment" ? "selected" : ""}>${t("cred.env")}</option><option value="keychain" ${credential.source === "keychain" ? "selected" : ""}>${t("cred.keychain")}</option><option value="oAuth" ${credential.source === "oAuth" ? "selected" : ""}>${t("cred.login")}</option><option value="command" ${credential.source === "command" ? "selected" : ""}>${t("cred.command")}</option><option value="forward" ${credential.source === "forward" ? "selected" : ""}>${t("cred.codex")}</option></select></label><label>${t("drawer.credentialTransport")}<select name="credentialTransport"><option value="bearer" ${credential.transport === "bearer" ? "selected" : ""}>Bearer</option><option value="xApiKey" ${credential.transport === "xApiKey" ? "selected" : ""}>x-api-key</option><option value="customHeader" ${credential.transport === "customHeader" ? "selected" : ""}>Custom header</option></select></label></div>
      <label>${t("drawer.credentialRef")} <small>${t("drawer.credentialRefHint")}</small><input name="credentialReference" value="${h(credential.reference ?? provider.apiKeyEnv ?? "")}" autocomplete="off" /></label>
      <div class="toggle-pair"><label class="check-control"><input name="enabled" type="checkbox" ${provider.enabled ? "checked" : ""}/><span>${t("drawer.enable")}</span></label><label class="check-control"><input name="allowPrivateNetwork" type="checkbox" ${provider.allowPrivateNetwork ? "checked" : ""}/><span>${t("drawer.allowPrivate")}</span></label></div>
      <div class="drawer-actions"><button class="danger-link" data-action="remove-provider" data-provider-id="${h(provider.id)}" type="button">${t("drawer.remove")}</button><button class="primary" type="submit">${t("drawer.save")}</button></div>
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
  return t("error.generic");
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
    if (showNotice) state.notice = { tone: "info", text: t("toast.refreshed") };
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
    case "toggle-language": setLanguage(getLanguage() === "ja" ? "en" : "ja"); render(); return;
    case "refresh-all": await refreshAll(true); return;
    case "start-gateway":
    case "stop-gateway":
      await withBusy("gateway", async () => {
        state.status = await invoke<GatewayStatus>(action === "start-gateway" ? "start_provider_gateway" : "stop_provider_gateway");
        notify(action === "start-gateway" ? t("toast.gatewayStarted") : t("toast.gatewayStopped"));
      });
      return;
    case "run-diagnostics":
      await withBusy("diagnostics", async () => {
        state.diagnostics = await invoke<GatewayDiagnosticReport>("gateway_diagnostics");
        notify(t("toast.diagnosticsDone"), state.diagnostics.errors ? "error" : "success");
      });
      return;
    case "probe-local-clis":
      await withBusy("local-clis", async () => {
        state.localClis = await invoke<LocalCliScanReport>("scan_local_cli_clients", { deep: true });
        const ready = state.localClis.clients.filter((client) => client.probeState === "ready").length;
        const needsRegistration = state.localClis.clients.filter((client) => client.needsCodetasRegistration).length;
        notify(
          needsRegistration
            ? t("toast.cliReadyImport", { ready, n: needsRegistration, use: t("cli.use") })
            : t("toast.cliReady", { ready }),
          "info",
        );
      });
      return;
    case "register-direct-api": {
      const providerId = target.dataset.providerId!;
      if (!window.confirm(t("confirm.registerDirect", { id: providerId }))) return;
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
      if (!window.confirm(t("confirm.registerLocal"))) return;
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
        notify(t("toast.codexUpdated", { path }));
      });
      return;
    case "restore-codex":
      if (!window.confirm(t("confirm.restoreCodex"))) return;
      await withBusy("codex", async () => {
        const report = await invoke<CodexRestoreReport>("restore_codex_gateway_config");
        await refreshStatusAndConfig();
        const suffix = report.conflicts.length ? t("toast.conflictSuffix", { items: report.conflicts.join(" / ") }) : "";
        notify(report.restored ? t("toast.codexRestored", { suffix }) : t("toast.restoreNone", { suffix }), report.conflicts.length ? "info" : "success");
      });
      return;
    case "sync-catalog":
      await withBusy("catalog", async () => {
        const path = await invoke<string>("sync_codex_model_catalog");
        await refreshStatusAndConfig();
        notify(t("toast.catalogSynced", { path }));
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
        notify(t("toast.modelsUpdated", { id: providerId }));
      });
      return;
    }
    case "default-provider": {
      const providerId = target.dataset.providerId!;
      await withBusy("provider", async () => {
        state.status = await invoke<GatewayStatus>("set_default_gateway_provider", { providerId });
        state.configuration = await invoke<GatewayConfiguration>("gateway_configuration");
        notify(t("toast.defaultSet", { id: providerId }));
      });
      return;
    }
    case "edit-provider": state.editingProviderId = target.dataset.providerId ?? null; render(); return;
    case "close-provider-editor": state.editingProviderId = null; render(); return;
    case "remove-provider": {
      const providerId = target.dataset.providerId!;
      if (!window.confirm(t("confirm.removeProvider", { id: providerId }))) return;
      await withBusy("provider", async () => {
        state.status = await invoke<GatewayStatus>("remove_gateway_provider", { providerId });
        state.configuration = await invoke<GatewayConfiguration>("gateway_configuration");
        state.editingProviderId = null;
        notify(t("toast.providerRemoved", { id: providerId }));
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
      config.routes.push({ id: `route-${config.routes.length + 1}`, name: t("route.name"), alias: null, strategy: "failover", targets: [], stickyRequests: 1, failureThreshold: 3, defaultReasoningEffort: null, enabled: true });
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
        notify(t("toast.serviceInstalled"));
      });
      return;
    case "start-service":
      await withBusy("service", async () => {
        state.service = await invoke<GatewayServiceStatus>("start_gateway_service");
        await refreshStatusAndConfig();
        notify(t("toast.serviceStarted"));
      });
      return;
    case "restart-service":
      await withBusy("service", async () => {
        state.service = await invoke<GatewayServiceStatus>("restart_gateway_service");
        await refreshStatusAndConfig();
        notify(t("toast.serviceRestarted"));
      });
      return;
    case "stop-service":
      await withBusy("service", async () => {
        state.service = await invoke<GatewayServiceStatus>("stop_gateway_service");
        await refreshStatusAndConfig();
        notify(t("toast.serviceStopped"), "info");
      });
      return;
    case "uninstall-service":
      if (!window.confirm(t("confirm.uninstallService"))) return;
      await withBusy("service", async () => {
        await invoke("uninstall_gateway_service");
        await refreshAll();
        notify(t("toast.serviceUninstalled"));
      });
      return;
    case "copy-config": {
      const textarea = document.querySelector<HTMLTextAreaElement>("#advanced-json");
      if (textarea) await navigator.clipboard.writeText(textarea.value);
      notify(t("toast.configCopied"), "info");
      return;
    }
    case "check-update":
      await withBusy("update", async () => {
        const check = await invoke<UpdateCheck>("check_for_codetas_update");
        if (!check.updateAvailable) {
          notify(t("toast.upToDate", { v: check.currentVersion }), "info");
          return;
        }
        if (!window.confirm(t("confirm.update", { v: check.manifest.version }))) {
          notify(t("toast.updateAvailable", { v: check.manifest.version }), "info");
          return;
        }
        state.notice = { tone: "info", text: t("toast.updateInstalling") };
        render();
        await invoke("install_codetas_update");
      });
      return;
    case "preview-cleanup":
      await withBusy("storage", async () => {
        state.cleanupPreview = await invoke<ObservabilityCleanupPreview>("preview_gateway_observability_cleanup");
        notify(state.cleanupPreview.files.length ? t("toast.cleanupPreview", { n: state.cleanupPreview.files.length }) : t("toast.cleanupNone"), "info");
      });
      return;
    case "trash-cleanup":
      if (!window.confirm(t("confirm.trash"))) return;
      await withBusy("storage", async () => {
        const report = await invoke<ObservabilityTrashReport | null>("trash_gateway_observability_cleanup");
        state.cleanupPreview = null;
        state.trashEntries = await invoke<ObservabilityTrashEntry[]>("list_gateway_observability_trash");
        state.observability = await invoke<ObservabilitySummary>("gateway_observability_summary");
        state.breakdown = await invoke<ObservabilityBreakdown>("gateway_observability_breakdown", { sinceMs: 0, maxEvents: 50_000 });
        notify(report ? t("toast.trashed", { n: report.files }) : t("toast.trashNone"), "info");
      });
      return;
    case "restore-trash": {
      const transactionId = target.dataset.transactionId!;
      await withBusy("storage", async () => {
        const report = await invoke<ObservabilityTrashReport>("restore_gateway_observability_trash", { transactionId });
        state.trashEntries = await invoke<ObservabilityTrashEntry[]>("list_gateway_observability_trash");
        state.observability = await invoke<ObservabilitySummary>("gateway_observability_summary");
        state.breakdown = await invoke<ObservabilityBreakdown>("gateway_observability_breakdown", { sinceMs: 0, maxEvents: 50_000 });
        notify(t("toast.restored", { n: report.files }));
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
        notify(t("toast.providerAdded", { id: presetId }));
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
    notify(t("toast.providerUpdated", { id }));
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
  await saveConfiguration({ ...config, routes }, t("toast.routesSaved"));
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
  await saveConfiguration(config, t("toast.agentsSaved"));
}

async function syncClients(data: FormData): Promise<void> {
  const input: ExternalClientIntegrationInput = {
    claudeCode: data.get("claudeCode") === "on", claudeDesktop: data.get("claudeDesktop") === "on",
    opencode: data.get("opencode") === "on", grok: data.get("grok") === "on", pi: data.get("pi") === "on",
  };
  await withBusy("clients", async () => {
    const report = await invoke<ClientIntegrationReport>("sync_client_integrations", { input });
    state.configuration = await invoke<GatewayConfiguration>("gateway_configuration");
    notify(t("toast.clientsGenerated", { n: report.clients.filter((client) => client.enabled).length }));
  });
}

async function saveSettingsForm(data: FormData): Promise<void> {
  const raw = document.querySelector<HTMLTextAreaElement>("#advanced-json")?.value;
  let config: GatewayConfiguration;
  try {
    config = raw ? JSON.parse(raw) as GatewayConfiguration : structuredClone(state.configuration!);
  } catch {
    notify(t("error.jsonParse"), "error");
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
  await saveConfiguration(config, t("toast.settingsSaved"));
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
          text: t("toast.updateAvailable", { v: check.manifest.version }),
        };
      }
    } catch {
      // An unavailable update service must not interfere with local gateway startup.
    }
  }
  render();
});
