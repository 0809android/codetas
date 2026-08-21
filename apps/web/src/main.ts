import { invoke } from "@tauri-apps/api/core";
import type {
  GatewayConfiguration,
  GatewayDiagnosticReport,
  GatewayStatus,
  UpdateCheck,
} from "@codetas/core";
import { nextLanguageLabel, t } from "./i18n";
import { state, navigation, type View } from "./state";
import { allModelIds, h, formatNumber, helpTip, providerModelIds, statusDot } from "./format";
import { renderView, renderAccountPoolRow, renderModelRows, renderModelRosterRow, renderRouteTargetRow, hydratePostRenderValues, renderProviderEditor, renderCodexDisconnectConfirmation, syncProviderEditorVisibility } from "./views";
import { handleAction, handleForm, refreshAll, syncMaintenanceJobPolling } from "./actions";
import "./styles.css";

const appRoot = document.querySelector<HTMLDivElement>("#app");
if (!appRoot) throw new Error("CODETAS app root is missing");
const app: HTMLDivElement = appRoot;
let providerEditorSaveTimer: number | null = null;

function queueProviderEditorSave(form: HTMLFormElement): void {
  if (providerEditorSaveTimer != null) window.clearTimeout(providerEditorSaveTimer);
  providerEditorSaveTimer = window.setTimeout(() => {
    providerEditorSaveTimer = null;
    void handleForm(form);
  }, 250);
}

function flushProviderEditorSave(): void {
  if (providerEditorSaveTimer == null) return;
  window.clearTimeout(providerEditorSaveTimer);
  providerEditorSaveTimer = null;
}

type RenderSnapshot = {
  windowX: number;
  windowY: number;
  active: {
    key: string;
    start: number;
    end: number;
    direction: HTMLInputElement["selectionDirection"];
  } | null;
  scrolls: Array<{ key: string; top: number; left: number }>;
};

function controlSnapshotKey(element: HTMLElement): string | null {
  if (element instanceof HTMLInputElement && element.id === "model-search") return "id:model-search";
  if (element instanceof HTMLInputElement && element.hasAttribute("data-model-display-name")) {
    return `display:${element.dataset.providerId ?? ""}:${element.dataset.modelId ?? ""}`;
  }
  if (element instanceof HTMLInputElement && element.dataset.action === "toggle-codex-model") {
    return `model:${element.dataset.providerId ?? ""}:${element.dataset.modelId ?? ""}`;
  }
  if (element instanceof HTMLInputElement && element.dataset.action === "toggle-codex-provider") {
    return `provider:${element.dataset.providerId ?? ""}`;
  }
  if (element.id === "model-display-format") return "id:model-display-format";
  if (element instanceof HTMLInputElement && element.dataset.action === "toggle-maintenance-storage") {
    return `storage:${element.dataset.storageId ?? ""}`;
  }
  if (element.closest("#provider-editor-form") && "name" in element && typeof element.name === "string" && element.name) {
    return `field:${element.name}`;
  }
  if (element instanceof HTMLTextAreaElement && element.dataset.hermesFile) {
    return `hermes-file:${element.dataset.hermesFile}`;
  }
  if (element instanceof HTMLTextAreaElement && element.dataset.contextFile) {
    return `context-file:${element.dataset.contextFile}`;
  }
  return element.id ? `id:${element.id}` : null;
}

function captureRenderSnapshot(): RenderSnapshot {
  const activeElement = document.activeElement;
  let active: RenderSnapshot["active"] = null;
  if (activeElement instanceof HTMLElement) {
    const key = controlSnapshotKey(activeElement);
    if (key) {
      const start = activeElement instanceof HTMLInputElement || activeElement instanceof HTMLTextAreaElement
        ? activeElement.selectionStart ?? activeElement.value.length
        : 0;
      const end = activeElement instanceof HTMLInputElement || activeElement instanceof HTMLTextAreaElement
        ? activeElement.selectionEnd ?? start
        : 0;
      const direction = activeElement instanceof HTMLInputElement || activeElement instanceof HTMLTextAreaElement
        ? activeElement.selectionDirection
        : "none";
      active = { key, start, end, direction };
    }
  }
  const scrolls = [...document.querySelectorAll<HTMLElement>(".workspace, .model-list, .provider-drawer, .drawer-model-list")]
    .map((element) => ({
      key: element.classList.contains("workspace")
        ? "workspace"
        : element.classList.contains("model-list")
          ? "model-list"
          : element.classList.contains("provider-drawer")
            ? "provider-drawer"
            : "drawer-model-list",
      top: element.scrollTop,
      left: element.scrollLeft,
    }))
    .filter((item) => item.top || item.left);
  return { windowX: window.scrollX, windowY: window.scrollY, active, scrolls };
}

function restoreRenderSnapshot(snapshot: RenderSnapshot): void {
  window.scrollTo(snapshot.windowX, snapshot.windowY);
  for (const item of snapshot.scrolls) {
    const element = document.querySelector<HTMLElement>(
      item.key === "workspace"
        ? ".workspace"
        : item.key === "model-list"
          ? ".model-list"
          : item.key === "provider-drawer"
            ? ".provider-drawer"
            : ".drawer-model-list",
    );
    if (element) {
      element.scrollTop = item.top;
      element.scrollLeft = item.left;
    }
  }
  if (!snapshot.active) return;
  const selectors: Record<string, string> = {
    "id:model-search": "#model-search",
    "id:model-display-format": "#model-display-format",
  };
  let target: HTMLElement | null = null;
  if (snapshot.active.key.startsWith("display:")) {
    const [, providerId, modelId] = snapshot.active.key.split(":");
    target = document.querySelector(`[data-model-display-name][data-provider-id="${CSS.escape(providerId ?? "")}"][data-model-id="${CSS.escape(modelId ?? "")}"]`);
  } else if (snapshot.active.key.startsWith("model:")) {
    const [, providerId, modelId] = snapshot.active.key.split(":");
    target = document.querySelector(`[data-action="toggle-codex-model"][data-provider-id="${CSS.escape(providerId ?? "")}"][data-model-id="${CSS.escape(modelId ?? "")}"]`);
  } else if (snapshot.active.key.startsWith("provider:")) {
    const providerId = snapshot.active.key.slice("provider:".length);
    target = document.querySelector(`[data-action="toggle-codex-provider"][data-provider-id="${CSS.escape(providerId)}"]`);
  } else if (snapshot.active.key.startsWith("storage:")) {
    const storageId = snapshot.active.key.slice("storage:".length);
    target = document.querySelector(`[data-action="toggle-maintenance-storage"][data-storage-id="${CSS.escape(storageId)}"]`);
  } else if (snapshot.active.key.startsWith("field:")) {
    const name = snapshot.active.key.slice("field:".length);
    target = document.querySelector(`#provider-editor-form [name="${CSS.escape(name)}"]`);
  } else if (snapshot.active.key.startsWith("hermes-file:")) {
    const id = snapshot.active.key.slice("hermes-file:".length);
    target = document.querySelector(`textarea[data-hermes-file="${CSS.escape(id)}"]`);
  } else if (snapshot.active.key.startsWith("context-file:")) {
    const id = snapshot.active.key.slice("context-file:".length);
    target = document.querySelector(`textarea[data-context-file="${CSS.escape(id)}"]`);
  } else {
    target = document.querySelector(selectors[snapshot.active.key] ?? `#${CSS.escape(snapshot.active.key.slice(3))}`);
  }
  if (!(target instanceof HTMLElement)) return;
  target.focus({ preventScroll: true });
  if (target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement) {
    try {
      target.setSelectionRange(snapshot.active.start, snapshot.active.end, snapshot.active.direction ?? undefined);
    } catch {
      // Non-text inputs do not support a selection range.
    }
  }
}

function persistHermesFileDrafts(): void {
  for (const editor of document.querySelectorAll<HTMLTextAreaElement>("textarea[data-hermes-file]")) {
    const id = editor.dataset.hermesFile;
    if (!id) continue;
    const file = state.hermesEditableFiles.find((item) => item.id === id);
    if (!file || editor.value === file.content) delete state.hermesFileDrafts[id];
    else state.hermesFileDrafts[id] = editor.value;
  }
}

function persistContextFileDrafts(): void {
  const context = state.maintenance?.contextLoad;
  for (const editor of document.querySelectorAll<HTMLTextAreaElement>("textarea[data-context-file]")) {
    const id = editor.dataset.contextFile;
    if (!id) continue;
    const current = context?.skills.find((item) => item.id === id)?.content
      ?? context?.instructionSources.find((item) => item.id === id)?.content;
    const focused = document.activeElement === editor;
    if (current == null || editor.value === current) delete state.contextFileDrafts[id];
    else if (state.contextFileDrafts[id] != null || focused) state.contextFileDrafts[id] = editor.value;
  }
}

export function render(): void {
  if (state.view === "routing") state.view = "providers";
  persistHermesFileDrafts();
  persistContextFileDrafts();
  const snapshot = captureRenderSnapshot();
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
            <div class="page-heading">
              <h1>${h(t(activeNav.key))}</h1>
              ${helpTip(t(`nav.help.${activeNav.id}`))}
            </div>
          </div>
          <div class="top-actions">
            <span class="provider-count">${t("shell.providerCount", { n: formatNumber(state.configuration?.providers.length) })}</span>
            <button class="icon-button language-toggle" data-action="toggle-language" aria-label="Language" title="Language" type="button">${h(nextLanguageLabel())}</button>
            <button class="icon-button" data-action="refresh-all" aria-label="${t("shell.refresh")}" title="${t("shell.refresh")}" type="button">↻</button>
          </div>
        </header>
        ${state.notice ? `<div class="notice ${state.notice.tone}" role="status"><span>${h(state.notice.text)}</span><button data-action="dismiss-notice" type="button" aria-label="×">×</button></div>` : ""}
        <section class="view" aria-live="polite">${renderView()}</section>
      </main>
      ${state.editingProviderId ? renderProviderEditor() : ""}
      ${state.confirmingCodexDisconnect ? renderCodexDisconnectConfirmation() : ""}
    </div>
  `;
  hydratePostRenderValues();
  restoreRenderSnapshot(snapshot);
  syncMaintenanceJobPolling();
}

document.addEventListener("click", (event) => {
  const target = (event.target as HTMLElement).closest<HTMLElement>("[data-view], [data-action]");
  if (!target) return;
  const view = target.dataset.view as View | undefined;
  if (view) {
    state.view = view === "routing" ? "providers" : view;
    state.notice = null;
    render();
    return;
  }
  const action = target.dataset.action;
  if (!action) return;
  if (target.matches('input[data-action="toggle-codex-model"], input[data-action="toggle-codex-provider"], input[data-action="toggle-maintenance-storage"], input[data-action="toggle-skill-enabled"]')) return;
  if (action === "add-route-target" && state.configuration) {
    const editor = target.closest<HTMLElement>(".route-editor");
    const list = editor?.querySelector<HTMLElement>(".route-target-list");
    if (!editor || !list) return;
    list.insertAdjacentHTML("beforeend", renderRouteTargetRow({ model: "", weight: 1 }, providerModelIds(state.configuration)));
    updateRouteTargetCount(editor);
    return;
  }
  if (action === "remove-route-target") {
    const editor = target.closest<HTMLElement>(".route-editor");
    target.closest(".route-target-row")?.remove();
    if (editor) updateRouteTargetCount(editor);
    return;
  }
  if (action === "add-account-pool-row" && state.configuration) {
    const list = document.querySelector<HTMLElement>(".account-pool-list");
    const providerId = state.configuration.providers[0]?.id ?? "";
    if (!list || !providerId) return;
    const usedIds = new Set([...list.querySelectorAll<HTMLInputElement>('[data-account-field="id"]')].map((input) => input.value.trim()));
    let suffix = 1;
    while (usedIds.has(`account-${suffix}`)) suffix += 1;
    list.insertAdjacentHTML("beforeend", renderAccountPoolRow({
      id: `account-${suffix}`,
      providerId, label: "", enabled: true, priority: 0, paused: false,
      pauseUntilUnix: null, pinned: false,
      credential: { source: "environment", reference: "", transport: "bearer", headerName: null, command: null },
    }, state.configuration));
    return;
  }
  if (action === "remove-account-pool-row") {
    target.closest(".account-pool-row")?.remove();
    return;
  }
  if (action === "add-model-roster-row" && state.configuration) {
    const name = target.dataset.name;
    const roster = target.closest<HTMLElement>(".model-roster-field")?.querySelector<HTMLElement>(".model-roster");
    if (!name || !roster) return;
    roster.insertAdjacentHTML("beforeend", renderModelRosterRow(name, null, allModelIds(state.configuration)));
    return;
  }
  if (action === "remove-model-roster-row") {
    target.closest(".model-roster-row")?.remove();
    return;
  }
  if (action === "move-model-row-up" || action === "move-model-row-down") {
    const row = target.closest<HTMLElement>(".route-target-row, .model-roster-row");
    if (!row?.parentElement) return;
    if (action === "move-model-row-up" && row.previousElementSibling) {
      row.parentElement.insertBefore(row, row.previousElementSibling);
    } else if (action === "move-model-row-down" && row.nextElementSibling) {
      row.parentElement.insertBefore(row.nextElementSibling, row);
    }
    return;
  }
  if (action === "close-provider-editor" && (event.target as HTMLElement).closest("[data-stop-close]") && target.classList.contains("drawer-scrim")) return;
  if (action === "close-provider-editor") flushProviderEditorSave();
  if (action === "cancel-restore-codex" && (event.target as HTMLElement).closest("[data-stop-confirmation-close]") && target.classList.contains("confirmation-scrim")) return;
  void handleAction(action, target);
});

document.addEventListener("submit", (event) => {
  event.preventDefault();
  const form = event.target as HTMLFormElement;
  void handleForm(form);
});

document.addEventListener("input", (event) => {
  const target = event.target as HTMLInputElement;
  if (target.matches("[data-model-filter]")) {
    filterModelSelect(target);
    return;
  }
  if (target.id === "model-search" && state.configuration) {
    state.modelSearchQuery = target.value;
    const list = document.querySelector("#model-list");
    if (list) {
      list.innerHTML = renderModelRows(state.configuration, target.value);
      hydratePostRenderValues();
    }
  }
  if (target.matches('#provider-editor-form [name="baseUrl"]')) {
    const form = target.closest<HTMLFormElement>("#provider-editor-form");
    if (form) syncProviderEditorVisibility(form);
  }
  if (target.matches("textarea[data-hermes-file]")) {
    const id = target.dataset.hermesFile;
    if (!id) return;
    const file = state.hermesEditableFiles.find((item) => item.id === id);
    if (!file || target.value === file.content) delete state.hermesFileDrafts[id];
    else state.hermesFileDrafts[id] = target.value;
  }
  if (target.matches("textarea[data-context-file]")) {
    const id = target.dataset.contextFile;
    if (!id) return;
    const current = state.maintenance?.contextLoad.skills.find((item) => item.id === id)?.content
      ?? state.maintenance?.contextLoad.instructionSources.find((item) => item.id === id)?.content;
    if (current == null || target.value === current) delete state.contextFileDrafts[id];
    else state.contextFileDrafts[id] = target.value;
  }
});

document.addEventListener("focusout", (event) => {
  const target = event.target as HTMLElement;
  const form = target.closest<HTMLFormElement>("#provider-editor-form");
  if (!form || target.matches("[data-action], [data-model-display-name]")) return;
  if (!target.matches("input, textarea, select")) return;
  const next = event.relatedTarget as Node | null;
  if (next && form.contains(next)) return;
  queueProviderEditorSave(form);
});

document.addEventListener("change", (event) => {
  const target = event.target as HTMLElement;
  if (target.matches('[data-action="toggle-codex-model"], [data-action="toggle-codex-provider"], [data-action="toggle-maintenance-storage"], [data-action="toggle-skill-enabled"]')) {
    void handleAction(target.dataset.action!, target);
    return;
  }
  if (target.matches("[data-model-display-name]")) {
    void handleAction("save-model-display-name", target);
    return;
  }
  if (target.matches("#model-display-format")) {
    void handleAction("save-model-display-format", target);
    return;
  }
  if (target.closest("#provider-editor-form")) {
    const form = target.closest<HTMLFormElement>("#provider-editor-form");
    if (!form) return;
    syncProviderEditorVisibility(form);
    if (!target.matches("[data-action], [data-model-display-name]")) {
      queueProviderEditorSave(form);
    }
  }
});

function filterModelSelect(search: HTMLInputElement): void {
  const picker = search.closest<HTMLElement>(".searchable-model-select");
  const select = picker?.querySelector<HTMLSelectElement>("select");
  if (!picker || !select) return;
  const query = search.value.trim().toLocaleLowerCase();
  let matches = 0;
  for (const option of select.options) {
    const visible = !option.value || !query || option.text.toLocaleLowerCase().includes(query);
    option.hidden = !visible;
    option.style.display = visible ? "" : "none";
    if (visible && option.value) matches += 1;
  }
  const empty = picker.querySelector<HTMLElement>(".model-filter-empty");
  if (empty) empty.hidden = matches > 0;
}

function updateRouteTargetCount(editor: HTMLElement): void {
  const count = editor.querySelectorAll(".route-target-row").length;
  const label = editor.querySelector<HTMLElement>("[data-route-target-count]");
  if (label) label.textContent = t("route.count", { n: count });
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
