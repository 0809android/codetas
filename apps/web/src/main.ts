import { invoke } from "@tauri-apps/api/core";
import type {
  GatewayConfiguration,
  GatewayDiagnosticReport,
  GatewayStatus,
  UpdateCheck,
} from "@codetas/core";
import { getLanguage, t } from "./i18n";
import { state, navigation, type View } from "./state";
import { h, formatNumber, statusDot } from "./format";
import { renderView, renderModelRows, hydratePostRenderValues, renderProviderEditor, renderCodexDisconnectConfirmation } from "./views";
import { handleAction, handleForm, refreshAll, syncMaintenanceJobPolling } from "./actions";
import "./styles.css";

const appRoot = document.querySelector<HTMLDivElement>("#app");
if (!appRoot) throw new Error("CODETAS app root is missing");
const app: HTMLDivElement = appRoot;

export function render(): void {
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
    state.view = view;
    state.notice = null;
    render();
    return;
  }
  const action = target.dataset.action;
  if (!action) return;
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
  if (target.id === "model-search" && state.configuration) {
    const list = document.querySelector("#model-list");
    if (list) list.innerHTML = renderModelRows(state.configuration, target.value);
  }
});

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
