import { invoke } from "@tauri-apps/api/core";
import { createSyncPlan } from "@codetas/core";
import type {
  ClientIntegrationReport,
  CodexArchiveResult,
  CodexRestartResult,
  CodexRestoreReport,
  CodexShutdownResult,
  CodexWriterActionResult,
  CredentialSource,
  CredentialTransport,
  ExternalClientIntegrationInput,
  GatewayConfiguration,
  GatewayDiagnosticReport,
  GatewayServiceStatus,
  GatewayStatus,
  HermesProfile,
  MaintenanceExecuteRequest,
  MaintenanceJob,
  MaintenancePlan,
  MaintenancePreviewInput,
  MaintenanceReport,
  ObservabilityBreakdown,
  ObservabilityCleanupPreview,
  ObservabilitySummary,
  ObservabilityTrashEntry,
  ObservabilityTrashReport,
  ProviderConnectionReport,
  ProviderDefinition,
  ProviderOAuthLaunchReport,
  ProjectInspection,
  ProviderPreset,
  UpdateCheck,
} from "@codetas/core";
import { getLanguage, setLanguage, t } from "./i18n";
import { state, type LocalCliScanReport, type DirectApiTarget, type Notice } from "./state";
import { lines } from "./format";
import { render } from "./main";
import { renderMaintenanceHistory } from "./views";

export async function withBusy<T>(key: string, action: () => Promise<T>): Promise<T | undefined> {
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

export function readableError(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  return t("error.generic");
}

export function notify(text: string, tone: Notice["tone"] = "success"): void {
  state.notice = { tone, text };
  render();
}

function readMaintenancePreviewInput(): MaintenancePreviewInput {
  const retention = document.querySelector<HTMLSelectElement>("#maintenance-retention")?.value ?? "30";
  return {
    logRetentionDays: retention === "never" ? null : Number(retention) as 7 | 30 | 90,
    compactSqlite: document.querySelector<HTMLInputElement>("#maintenance-compact-sqlite")?.checked ?? true,
    repairOrphanPins: document.querySelector<HTMLInputElement>("#maintenance-orphan-pins")?.checked ?? true,
    disableMcpServers: [...state.maintenancePreviewInput.disableMcpServers],
  };
}

const MAINTENANCE_JOB_POLL_MS = 5_000;
let maintenanceJobsRefresh: Promise<MaintenanceJob[]> | null = null;
let maintenanceJobPollTimer: number | null = null;
let maintenanceJobPollInFlight = false;

async function refreshMaintenanceJobs(): Promise<void> {
  const request = maintenanceJobsRefresh ?? invoke<MaintenanceJob[]>("list_codex_maintenance_jobs");
  maintenanceJobsRefresh = request;
  try {
    state.maintenanceJobs = await request;
  } finally {
    if (maintenanceJobsRefresh === request) maintenanceJobsRefresh = null;
  }
}

export function syncMaintenanceJobPolling(): void {
  const shouldPoll = state.view === "maintenance"
    && state.maintenanceJobs.some((job) => job.status === "waitingForIdle" || job.status === "running");
  if (!shouldPoll) {
    if (maintenanceJobPollTimer != null) window.clearTimeout(maintenanceJobPollTimer);
    maintenanceJobPollTimer = null;
    return;
  }
  if (maintenanceJobPollTimer != null || maintenanceJobPollInFlight) return;
  maintenanceJobPollTimer = window.setTimeout(() => {
    maintenanceJobPollTimer = null;
    void pollMaintenanceJobs();
  }, MAINTENANCE_JOB_POLL_MS);
}

async function pollMaintenanceJobs(): Promise<void> {
  if (state.view !== "maintenance" || maintenanceJobPollInFlight) {
    syncMaintenanceJobPolling();
    return;
  }
  maintenanceJobPollInFlight = true;
  try {
    const before = JSON.stringify(state.maintenanceJobs);
    await refreshMaintenanceJobs();
    if (state.view === "maintenance" && JSON.stringify(state.maintenanceJobs) !== before) {
      const history = document.querySelector<HTMLElement>(".maintenance-history-panel");
      if (history) history.outerHTML = renderMaintenanceHistory();
    }
  } catch {
    // Background refresh errors must not interrupt the active screen.
  } finally {
    maintenanceJobPollInFlight = false;
    syncMaintenanceJobPolling();
  }
}

async function previewMaintenance(input: MaintenancePreviewInput): Promise<void> {
  state.maintenancePreviewInput = input;
  state.maintenancePlan = await invoke<MaintenancePlan>("preview_codex_maintenance", { request: input });
}

async function executeMaintenancePlan(plan: MaintenancePlan): Promise<void> {
  const enabled = plan.actions.filter((item) => !item.blockedReason);
  if (!enabled.length) {
    notify(t("maintenance.optimize.noExecutable"), "info");
    return;
  }
  const request: MaintenanceExecuteRequest = {
    planId: plan.id,
    actionIds: enabled.map((item) => item.id),
  };
  const job = await invoke<MaintenanceJob>("execute_codex_maintenance", { request });
  state.maintenancePlan = null;
  await refreshMaintenanceJobs();
  state.maintenance = await invoke<MaintenanceReport>("analyze_codex_maintenance");
  if (job.status === "completed") {
    notify(t("toast.maintenanceExecuted"));
  } else if (job.status === "waitingForIdle") {
    notify(t(job.error ? "toast.maintenanceQueuedWithErrors" : "toast.maintenanceQueued"), job.error ? "error" : "info");
  } else {
    notify(t("toast.maintenanceExecutionIncomplete"), "error");
  }
}

export async function refreshAll(showNotice = false): Promise<void> {
  await withBusy("refresh", async () => {
    const [status, configuration, presets, observability, breakdown, service, trashEntries, localClis, directApis, hermesProfiles] = await Promise.all([
      invoke<GatewayStatus>("provider_gateway_status"),
      invoke<GatewayConfiguration>("gateway_configuration"),
      invoke<ProviderPreset[]>("list_provider_presets"),
      invoke<ObservabilitySummary>("gateway_observability_summary"),
      invoke<ObservabilityBreakdown>("gateway_observability_breakdown", { sinceMs: 0, maxEvents: 50_000 }),
      invoke<GatewayServiceStatus>("gateway_service_status"),
      invoke<ObservabilityTrashEntry[]>("list_gateway_observability_trash"),
      invoke<LocalCliScanReport>("scan_local_cli_clients", { deep: false }),
      invoke<DirectApiTarget[]>("list_direct_api_targets"),
      invoke<HermesProfile[]>("list_hermes_profiles"),
    ]);
    state.status = status;
    state.configuration = configuration;
    state.presets = presets;
    state.directApis = directApis;
    state.hermesProfiles = hermesProfiles;
    state.observability = observability;
    state.breakdown = breakdown;
    state.service = service;
    state.trashEntries = trashEntries;
    state.localClis = localClis;
    if (showNotice) state.notice = { tone: "info", text: t("toast.refreshed") };
  });
}

export async function refreshStatusAndConfig(): Promise<void> {
  const [status, configuration] = await Promise.all([
    invoke<GatewayStatus>("provider_gateway_status"),
    invoke<GatewayConfiguration>("gateway_configuration"),
  ]);
  state.status = status;
  state.configuration = configuration;
}

export async function handleAction(action: string, target: HTMLElement): Promise<void> {
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
    case "run-maintenance":
      await withBusy("maintenance", async () => {
        const [report, jobs] = await Promise.all([
          invoke<MaintenanceReport>("analyze_codex_maintenance"),
          invoke<MaintenanceJob[]>("list_codex_maintenance_jobs").catch(() => state.maintenanceJobs),
        ]);
        state.maintenance = report;
        state.maintenanceJobs = jobs;
        const tone = report.overallStatus === "critical" ? "error" : report.overallStatus === "healthy" ? "success" : "info";
        notify(t("toast.maintenanceDone"), tone);
      });
      return;
    case "preview-maintenance": {
      const input = readMaintenancePreviewInput();
      await withBusy("maintenance-preview", async () => {
        await previewMaintenance(input);
        notify(t("toast.maintenancePreviewed"), "info");
      });
      return;
    }
    case "quick-maintenance": {
      const input = readMaintenancePreviewInput();
      await withBusy("maintenance-quick", async () => {
        await previewMaintenance(input);
        if (state.maintenancePlan) await executeMaintenancePlan(state.maintenancePlan);
      });
      return;
    }
    case "execute-maintenance": {
      const plan = state.maintenancePlan;
      if (!plan) return;
      await withBusy("maintenance-execute", async () => {
        await executeMaintenancePlan(plan);
      });
      return;
    }
    case "refresh-maintenance-jobs":
      await withBusy("maintenance-jobs", async () => {
        await refreshMaintenanceJobs();
      });
      return;
    case "rollback-maintenance": {
      const jobId = target.dataset.jobId;
      const waiting = target.dataset.jobStatus === "waitingForIdle";
      if (!jobId || (!waiting && !window.confirm(t("confirm.maintenanceRollback", { id: jobId })))) return;
      await withBusy(`maintenance-rollback-${jobId}`, async () => {
        await invoke<MaintenanceJob>("rollback_codex_maintenance_job", { request: { jobId, cancelOnly: waiting } });
        await refreshMaintenanceJobs();
        state.maintenance = await invoke<MaintenanceReport>("analyze_codex_maintenance");
        notify(waiting ? t("toast.maintenanceWaitingCancelled") : t("toast.maintenanceRolledBack"));
      });
      return;
    }
    case "request-codex-shutdown":
      if (!window.confirm(t("confirm.codexShutdown"))) return;
      await withBusy("codex-shutdown", async () => {
        const result = await invoke<CodexShutdownResult>("request_codex_shutdown");
        notify(result.message, result.stopped ? "success" : "info");
      });
      return;
    case "restart-codex":
      await withBusy("codex-restart", async () => {
        const result = await invoke<CodexRestartResult>("restart_codex");
        notify(result.message, result.started ? "success" : "info");
      });
      return;
    case "terminate-codex-writer": {
      const pid = Number(target.dataset.pid);
      const expectedStartedAt = target.dataset.startedAt ?? "";
      const threadId = target.dataset.threadId ?? "";
      if (!Number.isSafeInteger(pid) || pid <= 0 || !window.confirm(t("confirm.terminateWriter", { pid }))) return;
      await withBusy(`terminate-writer-${pid}`, async () => {
        const result = await invoke<CodexWriterActionResult>("terminate_codex_writer", { input: { pid, expectedStartedAt: expectedStartedAt || null, threadId } });
        state.maintenance = await invoke<MaintenanceReport>("analyze_codex_maintenance");
        notify(result.message, result.stopped ? "success" : "info");
      });
      return;
    }
    case "retry-codex-archive": {
      const threadId = target.dataset.threadId;
      if (!threadId) return;
      await withBusy(`retry-archive-${threadId}`, async () => {
        const result = await invoke<CodexArchiveResult>("retry_codex_archive", { threadId });
        state.maintenance = await invoke<MaintenanceReport>("analyze_codex_maintenance");
        notify(result.message, result.archived ? "success" : "info");
      });
      return;
    }
    case "preview-disable-mcp": {
      const server = target.dataset.server;
      if (!server) return;
      const input: MaintenancePreviewInput = {
        logRetentionDays: null,
        compactSqlite: false,
        repairOrphanPins: false,
        disableMcpServers: [server],
      };
      await withBusy("maintenance-preview", async () => {
        await previewMaintenance(input);
        notify(t("toast.mcpDisablePreview", { name: server }), "info");
      });
      return;
    }
    case "clear-maintenance-mcp":
      state.maintenancePreviewInput.disableMcpServers = [];
      state.maintenancePlan = null;
      render();
      return;
    case "export-maintenance": {
      const report = state.maintenance;
      if (!report) return;
      const payload = JSON.stringify(report, null, 2);
      const blob = new Blob([payload], { type: "application/json" });
      const url = URL.createObjectURL(blob);
      const link = document.createElement("a");
      link.href = url;
      link.download = `codetas-codex-health-${new Date(report.generatedAtMs).toISOString().slice(0, 10)}.json`;
      document.body.appendChild(link);
      link.click();
      link.remove();
      URL.revokeObjectURL(url);
      notify(t("toast.maintenanceExported"), "info");
      return;
    }
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
      state.confirmingCodexDisconnect = true;
      render();
      return;
    case "cancel-restore-codex":
      state.confirmingCodexDisconnect = false;
      render();
      return;
    case "confirm-restore-codex":
      state.confirmingCodexDisconnect = false;
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
        if (report.reachable) state.providerTestFailed.delete(providerId);
        else state.providerTestFailed.add(providerId);
        let detail: string;
        if (report.reachable) {
          if (report.status === 200) detail = t("toast.testReachable");
          else if (report.status === 401 || report.status === 403) detail = t("toast.testCredRejected");
          else detail = t("toast.testEndpointStatus", { status: report.status ?? "?" });
        } else {
          detail = `${t("toast.testFailed")}（${report.message}）`;
        }
        notify(`${providerId}: ${detail} (${report.latencyMs}ms)`, report.reachable ? "success" : "error");
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
        state.syncPlan = state.project ? createSyncPlan(state.project, { context: true, skills: true, mcp: true }) : null;
      });
      return;
    }
    case "convert-hermes-profiles": {
      if (!state.hermesProfiles.length) return;
      await withBusy("hermes-profiles", async () => {
        const report = await invoke<{ created: string[]; skipped: string[] }>("convert_hermes_profiles", { profileNames: state.hermesProfiles.map((profile) => profile.name) });
        state.hermesProfiles = await invoke<HermesProfile[]>("list_hermes_profiles");
        const suffix = report.skipped.length ? `（${t("toast.skipped", { items: report.skipped.join(", ") })}）` : "";
        notify(t("toast.profilesConverted", { n: report.created.length }) + suffix, report.skipped.length ? "info" : "success");
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

export async function handleForm(form: HTMLFormElement): Promise<void> {
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
    case "clients-form": await syncClients(data); return;
    case "settings-form": await saveSettingsForm(data); return;
  }
}

export async function saveProviderForm(data: FormData): Promise<void> {
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

export async function saveRoutesFromDom(): Promise<void> {
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

export async function saveAgentForm(data: FormData): Promise<void> {
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

export async function syncClients(data: FormData): Promise<void> {
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

export async function saveSettingsForm(data: FormData): Promise<void> {
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

export async function saveConfiguration(config: GatewayConfiguration, message: string): Promise<void> {
  await withBusy("configuration", async () => {
    state.configuration = await invoke<GatewayConfiguration>("save_gateway_configuration", { configuration: config });
    state.status = await invoke<GatewayStatus>("provider_gateway_status");
    notify(message);
  });
}
