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

export function render(): void {
  if (state.view === "routing") state.view = "providers";
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
    const list = document.querySelector("#model-list");
    if (list) list.innerHTML = renderModelRows(state.configuration, target.value);
  }
  if (target.matches('#provider-editor-form [name="baseUrl"]')) {
    const form = target.closest<HTMLFormElement>("#provider-editor-form");
    if (form) syncProviderEditorVisibility(form);
  }
});

document.addEventListener("change", (event) => {
  const target = event.target as HTMLElement;
  if (target.matches('[data-action="toggle-codex-model"]')) {
    void handleAction("toggle-codex-model", target as HTMLElement);
    return;
  }
  if (!target.matches("#provider-editor-form select")) return;
  const form = target.closest<HTMLFormElement>("#provider-editor-form");
  if (form) syncProviderEditorVisibility(form);
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
