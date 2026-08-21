import { invoke } from "@tauri-apps/api/core";
import { createSyncPlan } from "@codetas/core";
import type {
  AgentMediaTestKind,
  AgentMediaTestResult,
  ClientIntegrationReport,
  CatalogDisplayNameFormat,
  CodexPluginStatus,
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
  HermesEditableFile,
  HermesSyncApplyRequest,
  HermesSyncDirection,
  HermesSyncInventory,
  HermesSyncPolicy,
  HermesSyncPreview,
  HermesSyncPreviewRequest,
  HermesSyncSelection,
  MaintenanceExecuteRequest,
  MaintenanceJob,
  MaintenancePlan,
  MaintenancePreviewInput,
  MaintenanceReport,
  ModelMetadata,
  ObservabilityBreakdown,
  ObservabilityCleanupPreview,
  ObservabilitySummary,
  ObservabilityTrashEntry,
  ObservabilityTrashReport,
  OAuthProviderDescriptor,
  CompatibilityLabReport,
  DebugScope,
  ObservationEvent,
  RouteDryRunReport,
  ProviderConnectionReport,
  ProviderCredential,
  ProviderDefinition,
  ProviderOAuthLaunchReport,
  ProjectInspection,
  ProviderPreset,
  UpdateCheck,
} from "@codetas/core";
import { resolveAgentPreset, type AgentPresetId } from "./agent-presets";
import { nextLanguage, setLanguage, t } from "./i18n";
import { state, type LocalCliScanReport, type DirectApiTarget, type Notice } from "./state";
import { imageGenerationIdentityModelIds, lines, catalogModelEntries, codexPublicModelSlug, h } from "./format";
import { render } from "./main";
import { renderMaintenanceHistory } from "./views";

export async function withBusy<T>(
  key: string,
  action: () => Promise<T>,
  options: { render?: boolean } = {},
): Promise<T | undefined> {
  const shouldRender = options.render !== false;
  state.busy.add(key);
  if (shouldRender) {
    state.notice = null;
    render();
  }
  try {
    return await action();
  } catch (error) {
    const message = readableError(error);
    state.notice = { tone: "error", text: message };
    if (shouldRender) render();
    else showNotice(message, "error");
    return undefined;
  } finally {
    state.busy.delete(key);
    if (shouldRender) render();
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

function readSelectedStorageIds(): string[] {
  const boxes = [...document.querySelectorAll<HTMLInputElement>("[data-storage-id]")];
  if (!boxes.length) return state.maintenancePreviewInput.deleteStorageIds.filter((id) => id !== "codex-root");
  return boxes
    .filter((input) => input.checked && input.dataset.storageId && input.dataset.storageId !== "codex-root")
    .map((input) => input.dataset.storageId!);
}

function readMaintenancePreviewInput(): MaintenancePreviewInput {
  const retention = document.querySelector<HTMLSelectElement>("#maintenance-retention")?.value ?? "30";
  return {
    logRetentionDays: retention === "never" ? null : Number(retention) as 7 | 30 | 90,
    compactSqlite: document.querySelector<HTMLInputElement>("#maintenance-compact-sqlite")?.checked ?? true,
    repairOrphanPins: document.querySelector<HTMLInputElement>("#maintenance-orphan-pins")?.checked ?? true,
    trashOversizedSessions: document.querySelector<HTMLInputElement>("#maintenance-oversized-sessions")?.checked ?? true,
    disableMcpServers: [...state.maintenancePreviewInput.disableMcpServers],
    deleteStorageIds: readSelectedStorageIds(),
  };
}

const MAINTENANCE_JOB_POLL_MS = 5_000;
let maintenanceJobsRefresh: Promise<MaintenanceJob[]> | null = null;
let maintenanceReportRefresh: Promise<MaintenanceReport> | null = null;
let maintenanceJobPollTimer: number | null = null;
let maintenanceJobPollInFlight = false;

function maintenanceJobIsActive(job: MaintenanceJob): boolean {
  return job.status === "waitingForIdle" || job.status === "running";
}

async function refreshMaintenanceJobs(): Promise<MaintenanceJob[]> {
  const request = maintenanceJobsRefresh ?? invoke<MaintenanceJob[]>("list_codex_maintenance_jobs");
  maintenanceJobsRefresh = request;
  try {
    const jobs = await request;
    state.maintenanceJobs = jobs;
    return jobs;
  } finally {
    if (maintenanceJobsRefresh === request) maintenanceJobsRefresh = null;
  }
}

let maintenanceReportGeneration = 0;

async function refreshMaintenanceReport(force = false): Promise<MaintenanceReport> {
  if (force) {
    maintenanceReportGeneration += 1;
    maintenanceReportRefresh = null;
  }
  const generation = maintenanceReportGeneration;
  const request = maintenanceReportRefresh ?? invoke<MaintenanceReport>("analyze_codex_maintenance");
  maintenanceReportRefresh = request;
  try {
    const report = await request;
    if (generation === maintenanceReportGeneration) state.maintenance = report;
    return report;
  } finally {
    if (maintenanceReportRefresh === request) maintenanceReportRefresh = null;
  }
}

async function refreshMaintenanceReportAfterJobTransition(): Promise<MaintenanceReport> {
  if (maintenanceReportRefresh) {
    try {
      await refreshMaintenanceReport();
    } catch {
      // A fresh post-transition diagnostic is still required if the earlier request failed.
    }
  }
  return refreshMaintenanceReport();
}

export function syncMaintenanceJobPolling(): void {
  const shouldPoll = state.view === "maintenance"
    && state.maintenanceJobs.some(maintenanceJobIsActive);
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
    const beforeJobs = state.maintenanceJobs;
    const before = JSON.stringify(beforeJobs);
    const activeBefore = new Set(beforeJobs.filter(maintenanceJobIsActive).map((job) => job.id));
    const jobs = await refreshMaintenanceJobs();
    const activeAfter = new Set(jobs.filter(maintenanceJobIsActive).map((job) => job.id));
    const terminalTransition = [...activeBefore].some((id) => !activeAfter.has(id));
    let reportUpdated = false;
    if (terminalTransition) {
      try {
        await refreshMaintenanceReportAfterJobTransition();
        reportUpdated = true;
      } catch {
        // History can still update when the optional diagnostic refresh fails.
      }
    }
    if (state.view === "maintenance" && reportUpdated) {
      render();
    } else if (state.view === "maintenance" && JSON.stringify(jobs) !== before) {
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
  await refreshMaintenanceReport();
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
    const configuration = await invoke<GatewayConfiguration>("gateway_configuration");
    const [status, presets, observability, breakdown, service, trashEntries, localClis, directApis, oauthProviders, compatibilityLab, routeDryRuns, hermesProfiles, hermesSyncInventory, hermesEditableFiles, maintenanceJobs, codexPluginStatus] = await Promise.all([
      invoke<GatewayStatus>("provider_gateway_status"),
      invoke<ProviderPreset[]>("list_provider_presets"),
      invoke<ObservabilitySummary>("gateway_observability_summary"),
      invoke<ObservabilityBreakdown>("gateway_observability_breakdown", { sinceMs: 0, maxEvents: 50_000 }),
      invoke<GatewayServiceStatus>("gateway_service_status"),
      invoke<ObservabilityTrashEntry[]>("list_gateway_observability_trash"),
      invoke<LocalCliScanReport>("scan_local_cli_clients", { deep: false }),
      invoke<DirectApiTarget[]>("list_direct_api_targets"),
      invoke<OAuthProviderDescriptor[]>("oauth_provider_registry"),
      configuration.catalog.compatibilityLab
        ? invoke<CompatibilityLabReport>("gateway_compatibility_lab")
        : Promise.resolve(null),
      invoke<RouteDryRunReport[]>("gateway_route_dry_runs"),
      invoke<HermesProfile[]>("list_hermes_profiles"),
      invoke<HermesSyncInventory>("scan_hermes_sync", { projectPath: state.project?.path ?? null }).catch(() => state.hermesSyncInventory),
      invoke<HermesEditableFile[]>("list_hermes_editable_files", { projectPath: state.project?.path ?? null }).catch(() => state.hermesEditableFiles),
      refreshMaintenanceJobs().catch(() => state.maintenanceJobs),
      invoke<CodexPluginStatus>("codex_plugin_status").catch(() => state.codexPluginStatus),
    ]);
    state.status = status;
    state.configuration = configuration;
    state.presets = presets;
    state.directApis = directApis;
    state.oauthProviders = oauthProviders;
    state.compatibilityLab = compatibilityLab;
    state.routeDryRuns = routeDryRuns;
    state.hermesProfiles = hermesProfiles;
    state.hermesSyncInventory = hermesSyncInventory;
    state.hermesEditableFiles = hermesEditableFiles;
    pruneHermesFileDrafts();
    state.observability = observability;
    state.breakdown = breakdown;
    state.service = service;
    state.trashEntries = trashEntries;
    state.localClis = localClis;
    state.maintenanceJobs = maintenanceJobs;
    state.codexPluginStatus = codexPluginStatus;
    if (showNotice) state.notice = { tone: "info", text: t("toast.refreshed") };
  });
}

export async function refreshStatusAndConfig(): Promise<void> {
  const [status, configuration, codexPluginStatus] = await Promise.all([
    invoke<GatewayStatus>("provider_gateway_status"),
    invoke<GatewayConfiguration>("gateway_configuration"),
    invoke<CodexPluginStatus>("codex_plugin_status").catch(() => state.codexPluginStatus),
  ]);
  state.status = status;
  state.configuration = configuration;
  state.codexPluginStatus = codexPluginStatus;
}

type CatalogEntry = ReturnType<typeof catalogModelEntries>[number];

const DEFAULT_MODEL_CAPABILITIES: NonNullable<ProviderDefinition["capabilities"]> = {
  streaming: true,
  tools: true,
  parallelTools: false,
  vision: false,
  audio: false,
  reasoning: false,
  webSearch: false,
  imageGeneration: false,
  videoGeneration: false,
  realtime: false,
  websockets: false,
  statefulResponses: false,
  structuredOutput: false,
  serviceTier: false,
  customTools: false,
  toolSearch: false,
  mcpNamespaces: false,
  providerMetadata: false,
};

function modelAliases(entry: CatalogEntry): string[] {
  return entry.providerId === "openai"
    ? [entry.publicSlug, `openai/${entry.modelId}`, entry.modelId]
    : [entry.publicSlug, `${entry.providerId}/${entry.modelId}`];
}

function setCodexPublication(
  config: GatewayConfiguration,
  targetEntries: CatalogEntry[],
  publish: boolean,
): void {
  const entries = catalogModelEntries(config).filter((entry) => !entry.imageOnly);
  const selected = new Set(config.catalog.selectedModels ?? []);
  // Empty allowlist means "publish all". First uncheck materializes the current set.
  if (selected.size === 0) {
    for (const entry of entries) selected.add(entry.publicSlug);
  }
  for (const entry of targetEntries) {
    const aliases = modelAliases(entry);
    if (publish) {
      for (const alias of aliases) selected.delete(alias);
      selected.add(entry.publicSlug);
    } else {
      for (const alias of aliases) selected.delete(alias);
    }
  }
  // If every non-image model is selected again, collapse back to empty (= all).
  const next = [...selected].sort((left, right) => left.localeCompare(right));
  const allPublished = entries.length === 0 || entries.every((entry) =>
    next.some((item) => item === entry.publicSlug
      || item === entry.qualifiedId
      || (entry.providerId === "openai" && (item === entry.modelId || item === `openai/${entry.modelId}`))));
  config.catalog.selectedModels = allPublished ? [] : next;
}

function createModelMetadata(
  config: GatewayConfiguration,
  entry: CatalogEntry,
  displayName: string | null,
): ModelMetadata {
  const provider = config.providers.find((item) => item.id === entry.providerId);
  const capabilities = structuredClone(provider?.capabilities ?? DEFAULT_MODEL_CAPABILITIES);
  if (provider) {
    // Provider-level image support is only an upper bound. Keep a newly-created
    // metadata row from turning every model in an image-capable provider into an
    // image-only catalog identity.
    capabilities.imageGeneration = imageGenerationIdentityModelIds(config, provider).has(entry.modelId);
  }
  return {
    providerId: entry.providerId,
    modelId: entry.modelId,
    displayName,
    enabled: true,
    contextWindow: entry.contextWindow,
    maxInputTokens: provider?.modelMaxInputTokens?.[entry.modelId] ?? null,
    maxOutputTokens: provider?.modelMaxOutputTokens?.[entry.modelId] ?? null,
    inputModalities: [...(provider?.modelInputModalities?.[entry.modelId] ?? [])],
    reasoningEfforts: [...entry.reasoningEfforts],
    defaultReasoningEffort: provider?.modelDefaultReasoningEfforts?.[entry.modelId] ?? null,
    capabilities,
    inputPricePerMillion: null,
    outputPricePerMillion: null,
  };
}

export async function handleAction(action: string, target: HTMLElement): Promise<void> {
  switch (action) {
    case "dismiss-notice": state.notice = null; render(); return;
    case "toggle-language": setLanguage(nextLanguage()); render(); return;
    case "refresh-all": await refreshAll(true); return;
    case "start-debug-scope":
      await withBusy("debug-scope", async () => {
        state.debugScope = await invoke<DebugScope>("start_gateway_debug_scope", { durationSeconds: 300 });
        state.debugEvents = [];
      });
      return;
    case "refresh-debug-scope":
      if (!state.debugScope) return;
      await withBusy("debug-scope", async () => {
        state.debugEvents = await invoke<ObservationEvent[]>("gateway_debug_events", {
          scopeId: state.debugScope!.id,
          limit: 200,
        });
      });
      return;
    case "stop-debug-scope":
      if (!state.debugScope) return;
      await withBusy("debug-scope", async () => {
        await invoke("stop_gateway_debug_scope", { scopeId: state.debugScope!.id });
        state.debugScope = null;
        state.debugEvents = [];
      });
      return;
    case "refresh-codex-plugin-status":
      await withBusy("codex-plugin-status", async () => {
        state.codexPluginStatus = await invoke<CodexPluginStatus>("codex_plugin_status");
      });
      return;
    case "test-agent-media": {
      const kind = target.dataset.kind as AgentMediaTestKind | undefined;
      if (!kind) return;
      const form = target.closest<HTMLFormElement>("#agents-form");
      if (!form) return;
      const configuration = agentConfigurationFromForm(new FormData(form));
      const prompt = document.querySelector<HTMLInputElement>("#agent-test-prompt")?.value.trim() || null;
      await withBusy("agent-media-test", async () => {
        state.configuration = await invoke<GatewayConfiguration>("save_gateway_configuration", { configuration });
        const result = await invoke<AgentMediaTestResult | null>("run_agent_media_test", { kind, prompt });
        if (!result) return;
        state.agentMediaTest = result;
        notify(t("toast.agentMediaTestDone", { model: result.model, ms: result.durationMs }));
      });
      return;
    }
    case "apply-agent-preset": {
      const presetId = target.dataset.preset as AgentPresetId | undefined;
      if (!presetId || !state.configuration) return;
      const preset = resolveAgentPreset(state.configuration, presetId);
      if (!preset.available) {
        notify(t("toast.agentPresetUnavailable"), "info");
        return;
      }
      await withBusy("agent-preset", async () => {
        state.configuration = await invoke<GatewayConfiguration>("apply_agent_preset_configuration", {
          input: {
            mainModel: preset.mainModel,
            visionModel: preset.visionModel,
            imageModel: preset.imageModel,
          },
        });
        await refreshStatusAndConfig();
        notify(t("toast.agentPresetApplied", { vision: preset.visionModel ?? "—" }));
      });
      return;
    }
    case "start-gateway":
    case "stop-gateway":
      await withBusy("gateway", async () => {
        state.status = await invoke<GatewayStatus>(action === "start-gateway" ? "start_provider_gateway" : "stop_provider_gateway");
        await refreshStatusAndConfig();
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
          refreshMaintenanceReport(),
          refreshMaintenanceJobs().catch(() => state.maintenanceJobs),
        ]);
        state.maintenanceJobs = jobs;
        const tone = report.overallStatus === "critical" ? "error" : report.overallStatus === "healthy" ? "success" : "info";
        notify(t("toast.maintenanceDone"), tone);
      });
      return;
    case "execute-maintenance": {
      const input = readMaintenancePreviewInput();
      state.maintenancePreviewInput = input;
      if (input.deleteStorageIds.length && !window.confirm(t("confirm.maintenanceTrashStorage", { n: String(input.deleteStorageIds.length) }))) {
        return;
      }
      await withBusy("maintenance-execute", async () => {
        await previewMaintenance(input);
        if (state.maintenancePlan) await executeMaintenancePlan(state.maintenancePlan);
      });
      return;
    }
    case "toggle-maintenance-storage": {
      const storageId = target.dataset.storageId;
      if (!storageId) return;
      const input = readMaintenancePreviewInput();
      state.maintenancePreviewInput = input;
      state.maintenancePlan = null;
      return;
    }
    case "select-all-maintenance-storage": {
      const input = readMaintenancePreviewInput();
      const ids = (state.maintenance?.storage ?? []).filter((entry) => entry.bytes > 0 && entry.id !== "codex-root").map((entry) => entry.id);
      input.deleteStorageIds = ids;
      state.maintenancePreviewInput = input;
      state.maintenancePlan = null;
      render();
      return;
    }
    case "clear-maintenance-storage": {
      const input = readMaintenancePreviewInput();
      input.deleteStorageIds = [];
      state.maintenancePreviewInput = input;
      state.maintenancePlan = null;
      render();
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
        await refreshMaintenanceReport();
        notify(waiting ? t("toast.maintenanceWaitingCancelled") : t("toast.maintenanceRolledBack"));
      });
      return;
    }
    case "request-codex-shutdown":
      if (!window.confirm(t("confirm.codexShutdown"))) return;
      await withBusy("codex-shutdown", async () => {
        const result = await invoke<CodexShutdownResult>("request_codex_shutdown");
        await refreshMaintenanceReport();
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
        await refreshMaintenanceReport();
        notify(result.message, result.stopped ? "success" : "info");
      });
      return;
    }
    case "retry-codex-archive": {
      const threadId = target.dataset.threadId;
      if (!threadId) return;
      await withBusy(`retry-archive-${threadId}`, async () => {
        const result = await invoke<CodexArchiveResult>("retry_codex_archive", { threadId });
        await refreshMaintenanceReport();
        notify(result.message, result.archived ? "success" : "info");
      });
      return;
    }
    case "select-disable-mcp": {
      const server = target.dataset.server;
      if (!server) return;
      const input = readMaintenancePreviewInput();
      if (!input.disableMcpServers.includes(server)) {
        input.disableMcpServers.push(server);
      }
      state.maintenancePreviewInput = input;
      state.maintenancePlan = null;
      notify(t("toast.mcpDisableSelected", { name: server }), "info");
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
        const previous = state.configuration;
        const previousIds = new Set(
          previous
            ? catalogModelEntries(previous)
              .filter((entry) => entry.providerId === providerId)
              .map((entry) => entry.modelId)
            : [],
        );
        state.configuration = await invoke<GatewayConfiguration>("refresh_gateway_provider_models", { providerId });
        state.status = await invoke<GatewayStatus>("provider_gateway_status");
        const nextIds = catalogModelEntries(state.configuration)
          .filter((entry) => entry.providerId === providerId)
          .map((entry) => entry.modelId);
        const added = nextIds.filter((id) => !previousIds.has(id));
        const detail = added.length
          ? t("toast.modelsUpdatedWithNew", { id: providerId, n: nextIds.length, added: added.slice(0, 5).join(", ") })
          : t("toast.modelsUpdatedCount", { id: providerId, n: nextIds.length });
        notify(detail);
      });
      return;
    }
    case "toggle-codex-model": {
      const config = state.configuration;
      if (!config) return;
      const providerId = target.dataset.providerId!;
      const modelId = target.dataset.modelId!;
      const input = target as HTMLInputElement;
      const entry = catalogModelEntries(config).find((item) => item.providerId === providerId && item.modelId === modelId);
      if (!entry || entry.imageOnly) return;
      const publish = input.checked;
      await saveConfiguration(config, publish
        ? t("toast.modelPublished", { model: codexPublicModelSlug(providerId, modelId) })
        : t("toast.modelUnpublished", { model: codexPublicModelSlug(providerId, modelId) }), {
          quiet: true,
          mutate: (next) => {
            const current = catalogModelEntries(next).find((item) => item.providerId === providerId && item.modelId === modelId);
            if (!current || current.imageOnly) return;
            setCodexPublication(next, [current], publish);
          },
        });
      return;
    }
    case "toggle-codex-provider": {
      const config = state.configuration;
      if (!config) return;
      const providerId = target.dataset.providerId!;
      const publish = (target as HTMLInputElement).checked;
      const entries = catalogModelEntries(config).filter((entry) => entry.providerId === providerId && !entry.imageOnly);
      if (!entries.length) return;
      await saveConfiguration(config, publish
        ? t("toast.providerPublished", { id: providerId })
        : t("toast.providerUnpublished", { id: providerId }), {
          quiet: true,
          mutate: (next) => {
            const current = catalogModelEntries(next).filter((entry) => entry.providerId === providerId && !entry.imageOnly);
            if (!current.length) return;
            setCodexPublication(next, current, publish);
          },
        });
      return;
    }
    case "save-model-display-name": {
      const config = state.configuration;
      if (!config) return;
      const providerId = target.dataset.providerId!;
      const modelId = target.dataset.modelId!;
      const displayName = (target as HTMLInputElement).value.trim();
      const entry = catalogModelEntries(config).find((item) => item.providerId === providerId && item.modelId === modelId);
      if (!entry) return;
      await saveConfiguration(config, t("toast.modelDisplayNameUpdated"), {
        quiet: true,
        mutate: (next) => {
          const current = catalogModelEntries(next).find((item) => item.providerId === providerId && item.modelId === modelId);
          if (!current) return;
          const metadata = next.modelCatalog.find((item) => item.providerId === providerId && item.modelId === modelId);
          if (metadata) metadata.displayName = displayName || null;
          else if (displayName) next.modelCatalog.push(createModelMetadata(next, current, displayName || null));
        },
      });
      return;
    }
    case "save-model-display-format": {
      const config = state.configuration;
      if (!config) return;
      const value = (target as HTMLSelectElement).value as CatalogDisplayNameFormat;
      const formats: CatalogDisplayNameFormat[] = ["default", "custom", "modelId", "providerModel", "providerIdModel"];
      if (!formats.includes(value)) return;
      await saveConfiguration(config, t("toast.modelDisplayFormatUpdated"), {
        mutate: (next) => {
          next.catalog.displayNameFormat = value;
        },
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
    case "close-provider-editor": {
      const form = document.querySelector<HTMLFormElement>("#provider-editor-form");
      if (form) {
        await saveProviderForm(new FormData(form), { close: true });
      } else {
        if (quietConfigurationSave) await quietConfigurationSave.catch(() => undefined);
        state.editingProviderId = null;
        render();
      }
      return;
    }
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
        state.hermesSyncInventory = await invoke<HermesSyncInventory>("scan_hermes_sync", { projectPath: state.project?.path ?? null });
        await refreshHermesEditableFiles();
        state.hermesSyncPreview = null;
      });
      return;
    }
    case "scan-hermes-sync": {
      await withBusy("hermes-sync", async () => {
        state.hermesSyncInventory = await invoke<HermesSyncInventory>("scan_hermes_sync", { projectPath: state.project?.path ?? null });
        await refreshHermesEditableFiles();
        state.hermesSyncPreview = null;
        notify(state.hermesSyncInventory.installed ? t("toast.hermesScanned", { n: state.hermesSyncInventory.documents.length }) : t("toast.hermesMissing"), state.hermesSyncInventory.installed ? "success" : "info");
      });
      return;
    }
    case "set-hermes-sync-direction": {
      const direction = target.dataset.direction as HermesSyncDirection | undefined;
      if (direction !== "import" && direction !== "export") return;
      state.hermesSyncDirection = direction;
      state.hermesSyncPreview = null;
      render();
      return;
    }
    case "preview-hermes-sync": {
      const request = hermesSyncPreviewRequestFromDom();
      if (!request) return;
      await withBusy("hermes-sync", async () => {
        state.hermesSyncPreview = await invoke<HermesSyncPreview>("preview_hermes_sync", { request });
        notify(t("toast.hermesPreviewReady", { n: state.hermesSyncPreview.items.length }), "info");
      });
      return;
    }
    case "apply-hermes-sync": {
      if (!state.hermesSyncPreview) return;
      const request = hermesSyncApplyRequestFromDom();
      if (!request.items.length) return;
      if (!window.confirm(t("confirm.hermesSyncApply", { n: request.items.length, direction: request.direction === "import" ? t("sync.directionImport") : t("sync.directionExport") }))) return;
      await withBusy("hermes-sync", async () => {
        const report = await invoke<{ written: string[]; skipped: string[]; backups: string[] }>("apply_hermes_sync", { request });
        state.hermesSyncInventory = await invoke<HermesSyncInventory>("scan_hermes_sync", { projectPath: state.project?.path ?? null });
        await refreshHermesEditableFiles();
        state.hermesSyncPreview = null;
        notify(t("toast.hermesApplied", { n: report.written.length }));
      });
      return;
    }
    case "close-hermes-preview":
      state.hermesSyncPreview = null;
      render();
      return;
    case "set-hermes-profile-tab": {
      const tab = target.dataset.tab ?? "all";
      state.hermesProfileTab = tab;
      render();
      return;
    }
    case "save-hermes-file": {
      const id = target.dataset.fileId;
      if (!id) return;
      const editor = document.querySelector<HTMLTextAreaElement>(`textarea[data-hermes-file="${CSS.escape(id)}"]`);
      const content = editor?.value ?? state.hermesFileDrafts[id] ?? "";
      await withBusy(`hermes-file-${id}`, async () => {
        const saved = await invoke<HermesEditableFile>("save_hermes_editable_file", {
          id,
          content,
          projectPath: state.project?.path ?? null,
        });
        state.hermesEditableFiles = state.hermesEditableFiles.map((file) => file.id === saved.id ? saved : file);
        if (!state.hermesEditableFiles.some((file) => file.id === saved.id)) {
          state.hermesEditableFiles = [...state.hermesEditableFiles, saved];
        }
        delete state.hermesFileDrafts[id];
        if (saved.kind === "profileYaml") {
          state.hermesProfiles = await invoke<HermesProfile[]>("list_hermes_profiles");
        }
        notify(t("toast.hermesFileSaved", { label: saved.label }));
      });
      return;
    }
    case "save-context-file": {
      const editor = target.closest("article")?.querySelector<HTMLTextAreaElement>("textarea[data-context-file]");
      const id = editor?.dataset.contextFile;
      if (!id) return;
      const current = state.maintenance?.contextLoad.skills.find((item) => item.id === id)
        ?? state.maintenance?.contextLoad.instructionSources.find((item) => item.id === id);
      if (!current || current.truncated || current.readFailed || !current.writable) return;
      const content = editor?.value ?? state.contextFileDrafts[id] ?? "";
      await withBusy(`context-file-${id}`, async () => {
        const contextLoad = await invoke<MaintenanceReport["contextLoad"]>("save_codex_context_document", {
          input: { id, content },
        });
        if (state.maintenance) state.maintenance = { ...state.maintenance, contextLoad };
        const label = contextLoad.skills.find((item) => item.id === id)?.name
          ?? contextLoad.instructionSources.find((item) => item.id === id)?.label
          ?? id;
        delete state.contextFileDrafts[id];
        notify(t("toast.contextFileSaved", { label }));
        try {
          await refreshMaintenanceReport(true);
        } catch {
          // The file is already saved; keep the returned editor state if a later diagnosis fails.
        }
      });
      return;
    }
    case "toggle-skill-enabled": {
      const id = target.dataset.skillId;
      if (!id) return;
      const enabled = (target as HTMLInputElement).checked;
      await withBusy(`skill-enabled-${id}`, async () => {
        const contextLoad = await invoke<MaintenanceReport["contextLoad"]>("set_codex_skill_enabled", {
          input: { id, enabled },
        });
        if (state.maintenance) state.maintenance = { ...state.maintenance, contextLoad };
        const label = contextLoad.skills.find((item) => item.id === id)?.name ?? id;
        notify(t(enabled ? "toast.skillEnabled" : "toast.skillDisabled", { label }));
        try {
          await refreshMaintenanceReport(true);
        } catch {
          // The config is already saved; keep the returned editor state if a later diagnosis fails.
        }
      });
      return;
    }
    case "reload-context-file": {
      const id = target.closest("article")?.querySelector<HTMLTextAreaElement>("textarea[data-context-file]")?.dataset.contextFile;
      if (!id) return;
      delete state.contextFileDrafts[id];
      await withBusy(`context-file-${id}`, async () => {
        await refreshMaintenanceReport(true);
        delete state.contextFileDrafts[id];
        notify(t("toast.contextFileReloaded"), "info");
      });
      return;
    }
    case "reload-hermes-file": {
      const id = target.dataset.fileId;
      if (!id) return;
      await withBusy(`hermes-file-${id}`, async () => {
        await refreshHermesEditableFiles();
        delete state.hermesFileDrafts[id];
        notify(t("toast.hermesFileReloaded"), "info");
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
      if (document.querySelector(".route-editor")) config.routes = routesFromDom(config);
      config.routes.push({
        id: `route-${config.routes.length + 1}`,
        name: t("route.sampleName"),
        description: t("route.sampleDescription"),
        alias: null,
        strategy: "failover",
        targets: [],
        stickyRequests: 1,
        failureThreshold: 3,
        defaultReasoningEffort: null,
        enabled: true,
        policy: {
          requiredCapabilities: [], healthWeight: 100, costWeight: 100,
          quotaWeight: 100, contextWeight: 0,
          maxInputPricePerMillion: null, maxOutputPricePerMillion: null,
        },
      });
      render();
      return;
    }
    case "remove-route": {
      const index = Number(target.dataset.routeIndex);
      const config = state.configuration;
      if (!config) return;
      config.routes = routesFromDom(config);
      config.routes.splice(index, 1);
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
    case "provider-editor-form": await saveProviderForm(data, { close: false }); return;
    case "agents-form": await saveAgentForm(data); return;
    case "clients-form": await syncClients(data); return;
    case "profiles-form": await saveProfilesForm(data); return;
    case "settings-form": await saveSettingsForm(data); return;
  }
}

function providerFromEditorForm(data: FormData, current: ProviderDefinition): ProviderDefinition {
  const source = String(data.get("credentialSource")) as CredentialSource;
  const reference = String(data.get("credentialReference") ?? "").trim() || null;
  const credentialCommand = source === "oAuth" || source === "command"
    ? current.credential?.command ?? null
    : null;
  const credentialTransport = (source === "none" || source === "forward"
    ? "bearer"
    : String(data.get("credentialTransport") ?? current.credential?.transport ?? "bearer")) as CredentialTransport;
  const provider: ProviderDefinition = structuredClone(current);
  provider.name = String(data.get("name"));
  provider.displayPrefix = String(data.get("displayPrefix") ?? "").trim() || null;
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
      requestRetries: 2,
      streamRetries: 2,
      retryOn429: false,
      max429Retries: 2,
      requestPacingMs: 0,
      emptyCompletionRetries: 1,
      maxRequestBytes: 16 * 1024 ** 2,
      maxResponseBytes: 32 * 1024 ** 2,
    }),
    requestRetries: Number(data.get("requestRetries")),
    streamRetries: Number(data.get("streamRetries")),
  };
  provider.statelessResponses = data.get("statelessResponses") === "on";
  provider.requiresAdjacentResponsesToolResults = data.get("requiresAdjacentResponsesToolResults") === "on";
  provider.stripModelBracketSuffix = data.get("stripModelBracketSuffix") === "on";
  provider.enabled = data.get("enabled") === "on";
  provider.allowPrivateNetwork = data.get("allowPrivateNetwork") === "on";
  provider.apiKeyEnv = source === "environment" ? reference : null;
  provider.credential = {
    source,
    reference: source === "none" || source === "forward" || source === "command" || (source === "oAuth" && credentialCommand)
      ? null
      : reference,
    transport: credentialTransport,
    headerName: credentialTransport === "customHeader"
      ? String(data.get("credentialHeaderName") ?? "").trim() || null
      : null,
    command: credentialCommand,
  };
  return provider;
}

export async function saveProviderForm(data: FormData, options: { close: boolean } = { close: false }): Promise<void> {
  const requestedId = String(data.get("id"));
  await enqueueConfigurationSave(() => withBusy("provider", async () => {
    const form = document.querySelector<HTMLFormElement>("#provider-editor-form");
    const latest = form ? new FormData(form) : data;
    const id = String(latest.get("id") ?? requestedId);
    const current = state.configuration?.providers.find((provider) => provider.id === id);
    if (!current) return;
    const provider = providerFromEditorForm(latest, structuredClone(current));
    state.status = await invoke<GatewayStatus>("upsert_gateway_provider", { input: { provider, makeDefault: false } });
    state.configuration = await invoke<GatewayConfiguration>("gateway_configuration");
    if (options.close) state.editingProviderId = null;
    if (options.close) notify(t("toast.providerUpdated", { id }));
    else showNotice(t("toast.providerUpdated", { id }));
  }, { render: options.close }));
}

export async function saveRoutesFromDom(): Promise<void> {
  const config = state.configuration!;
  await saveConfiguration(config, t("toast.routesSaved"), {
    mutate: (next) => {
      next.routes = routesFromDom(next);
    },
  });
}

function routesFromDom(config: GatewayConfiguration): GatewayConfiguration["routes"] {
  const rows = [...document.querySelectorAll<HTMLElement>(".route-editor")];
  return rows.map((row) => {
    const value = (field: string) => row.querySelector<HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement>(`[data-field="${field}"]`)!;
    const targets = [...row.querySelectorAll<HTMLElement>(".route-target-row")].map((targetRow) => ({
      model: targetRow.querySelector<HTMLSelectElement>('[data-field="targetModel"]')?.value.trim() ?? "",
      weight: Math.max(1, Number(targetRow.querySelector<HTMLInputElement>('[data-field="targetWeight"]')?.value ?? 1)),
    })).filter((target) => target.model);
    return {
      id: value("id").value.trim(), name: value("name").value.trim(), description: value("description").value.trim() || null, alias: value("alias").value.trim() || null,
      strategy: value("strategy").value as GatewayConfiguration["routes"][number]["strategy"], targets,
      stickyRequests: config.routes[Number(row.dataset.routeIndex)]?.stickyRequests ?? 1,
      failureThreshold: config.routes[Number(row.dataset.routeIndex)]?.failureThreshold ?? 3,
      defaultReasoningEffort: value("defaultReasoningEffort").value.trim() || null,
      enabled: (value("enabled") as HTMLInputElement).checked,
      policy: {
        requiredCapabilities: value("requiredCapabilities").value.split(",").map((item) => item.trim()).filter(Boolean),
        healthWeight: Math.max(0, Number(value("healthWeight").value) || 0),
        costWeight: Math.max(0, Number(value("costWeight").value) || 0),
        quotaWeight: Math.max(0, Number(value("quotaWeight").value) || 0),
        contextWeight: Math.max(0, Number(value("contextWeight").value) || 0),
        maxInputPricePerMillion: nullableNumber(value("maxInputPricePerMillion").value),
        maxOutputPricePerMillion: nullableNumber(value("maxOutputPricePerMillion").value),
      },
    };
  });
}

export async function saveAgentForm(data: FormData): Promise<void> {
  try {
    await saveConfiguration(state.configuration!, t("toast.agentsSaved"), {
      mutate: (next) => applyAgentForm(next, data),
    });
  } catch (error) {
    notify(readableError(error), "error");
    render();
  }
}

function agentConfigurationFromForm(data: FormData): GatewayConfiguration {
  const config = structuredClone(state.configuration!);
  applyAgentForm(config, data);
  return config;
}

function applyAgentForm(config: GatewayConfiguration, data: FormData): void {
  config.agents.multiAgentV2 = data.get("multiAgentV2") === "on";
  config.agents.surfaceMode = String(data.get("surfaceMode")) as GatewayConfiguration["agents"]["surfaceMode"];
  config.agents.maxThreads = Number(data.get("maxThreads"));
  config.agents.effortCap = String(data.get("effortCap") ?? "").trim() || null;
  config.agents.subagentEffortCap = String(data.get("subagentEffortCap") ?? "").trim() || null;
  config.agents.subagentModels = data.getAll("subagentModels").map(String).map((model) => model.trim()).filter(Boolean);
  config.agents.subagentFallback = data.getAll("subagentFallback").map(String).map((model) => model.trim()).filter(Boolean);
  const fallbackMap = String(data.get("subagentFallbackByModel") ?? "{}").trim();
  const parsedFallbackMap = JSON.parse(fallbackMap || "{}") as unknown;
  if (!parsedFallbackMap || Array.isArray(parsedFallbackMap) || typeof parsedFallbackMap !== "object") {
    throw new Error(t("error.modelFallbackMap"));
  }
  config.agents.subagentFallbackByModel = Object.fromEntries(
    Object.entries(parsedFallbackMap).map(([model, fallbacks]) => {
      if (!model.trim() || !Array.isArray(fallbacks) || fallbacks.some((fallback) => typeof fallback !== "string")) {
        throw new Error(t("error.modelFallbackMap"));
      }
      return [model.trim(), fallbacks.map((fallback) => fallback.trim()).filter(Boolean)];
    }),
  );
  config.agents.imageInputMode = String(data.get("imageInputMode") ?? "auto") as GatewayConfiguration["agents"]["imageInputMode"];
  config.agents.videoInputMode = String(data.get("videoInputMode") ?? "auto") as GatewayConfiguration["agents"]["videoInputMode"];
  config.agents.documentInputMode = String(data.get("documentInputMode") ?? "auto") as GatewayConfiguration["agents"]["documentInputMode"];
  config.agents.auxiliaryTimeoutMs = Math.min(600, Math.max(1, Math.round(Number(data.get("auxiliaryTimeoutSeconds") ?? 120) || 120))) * 1000;
  config.agents.videoSampleFrames = Math.min(64, Math.max(1, Math.round(Number(data.get("videoSampleFrames") ?? 8) || 8)));
  config.agents.documentMaxPages = Math.min(100, Math.max(1, Math.round(Number(data.get("documentMaxPages") ?? 12) || 12)));
  config.agents.ocrEnabled = data.get("ocrEnabled") === "on";
  for (const key of ["webSearchModel", "visionModel", "videoInputModel", "documentModel", "imageModel", "videoModel", "liveModel"] as const) {
    config.sidecars[key] = String(data.get(key) ?? "").trim() || null;
  }
}



function pruneHermesFileDrafts(): void {
  const known = new Set(state.hermesEditableFiles.map((file) => file.id));
  for (const id of Object.keys(state.hermesFileDrafts)) {
    if (!known.has(id)) delete state.hermesFileDrafts[id];
  }
}

async function refreshHermesEditableFiles(): Promise<void> {
  state.hermesEditableFiles = await invoke<HermesEditableFile[]>("list_hermes_editable_files", { projectPath: state.project?.path ?? null }).catch(() => state.hermesEditableFiles);
  pruneHermesFileDrafts();
}

function hermesSyncPreviewRequestFromDom(): HermesSyncPreviewRequest | null {
  const form = document.querySelector<HTMLFormElement>("#hermes-sync-form");
  if (!form) return null;
  const items: HermesSyncSelection[] = [...form.querySelectorAll<HTMLElement>("[data-sync-item]")].map((row) => ({
    id: row.dataset.syncItem ?? "",
    selected: row.querySelector<HTMLInputElement>('input[data-sync-selected]')?.checked ?? false,
    policy: (row.querySelector<HTMLSelectElement>("select[data-sync-policy]")?.value ?? "overwrite") as HermesSyncPolicy,
  })).filter((item) => item.id);
  return {
    direction: state.hermesSyncDirection,
    projectPath: state.project?.path ?? null,
    items,
  };
}

function hermesSyncApplyRequestFromDom(): HermesSyncApplyRequest {
  const items = [...document.querySelectorAll<HTMLElement>("[data-sync-preview-item]")].flatMap((row) => {
    const id = row.dataset.syncPreviewItem;
    const policy = row.dataset.policy as HermesSyncPolicy | undefined;
    const status = state.hermesSyncPreview?.items.find((item) => item.id === id)?.status;
    const content = row.querySelector<HTMLTextAreaElement>("textarea")?.value;
    if (!id || !policy || policy === "skip" || status === "missingSource" || content == null) return [];
    return [{ id, policy, content }];
  });
  return {
    direction: state.hermesSyncDirection,
    projectPath: state.project?.path ?? null,
    items,
  };
}

export async function saveProfilesForm(data: FormData): Promise<void> {
  const config = structuredClone(state.configuration!);
  config.codex.loadHermesContext = data.get("loadHermesContext") === "on";
  const input: ExternalClientIntegrationInput = {
    claudeCode: config.integrations.claudeCode,
    claudeDesktop: config.integrations.claudeDesktop,
    opencode: config.integrations.opencode,
    grok: config.integrations.grok,
    pi: config.integrations.pi,
    hermes: data.get("hermesIntegration") === "on",
  };
  await withBusy("profiles", async () => {
    state.configuration = await invoke<GatewayConfiguration>("save_gateway_configuration", { configuration: config });
    const report = await invoke<ClientIntegrationReport>("sync_client_integrations", { input });
    state.configuration = await invoke<GatewayConfiguration>("gateway_configuration");
    const hermes = report.clients.find((client) => client.client === "hermes");
    notify(hermes?.enabled ? t("toast.hermesIntegrationOn") : t("toast.hermesIntegrationOff"));
  });
}

export async function syncClients(data: FormData): Promise<void> {
  const input: ExternalClientIntegrationInput = {
    claudeCode: data.get("claudeCode") === "on", claudeDesktop: data.get("claudeDesktop") === "on",
    opencode: data.get("opencode") === "on", grok: data.get("grok") === "on", pi: data.get("pi") === "on",
    hermes: data.get("hermes") === "on",
  };
  await withBusy("clients", async () => {
    const report = await invoke<ClientIntegrationReport>("sync_client_integrations", { input });
    state.configuration = await invoke<GatewayConfiguration>("gateway_configuration");
    notify(t("toast.clientsGenerated", { n: report.clients.filter((client) => client.enabled).length }));
  });
}

export async function saveSettingsForm(data: FormData): Promise<void> {
  const raw = document.querySelector<HTMLTextAreaElement>("#advanced-json")?.value;
  let parsed: GatewayConfiguration | null = null;
  if (raw) {
    try {
      parsed = JSON.parse(raw) as GatewayConfiguration;
    } catch {
      notify(t("error.jsonParse"), "error");
      return;
    }
  }
  await saveConfiguration(state.configuration!, t("toast.settingsSaved"), {
    mutate: (next) => applySettingsForm(next, data, parsed),
  });
}

function applySettingsForm(config: GatewayConfiguration, data: FormData, parsed: GatewayConfiguration | null): void {
  if (parsed) Object.assign(config, parsed);
  config.runtime.host = String(data.get("host"));
  config.runtime.port = Number(data.get("port"));
  config.runtime.shutdownTimeoutMs = Number(data.get("shutdownTimeoutMs"));
  config.runtime.memoryBudgetBytes = Number(data.get("memoryBudgetMb")) * 1024 ** 2;
  config.runtime.maxInflightRequests = Number(data.get("maxInflightRequests"));
  config.runtime.dynamicPortFallback = data.get("dynamicPortFallback") === "on";
  config.runtime.autoStart = data.get("autoStart") === "on";
  config.codex.autoSyncCatalog = data.get("autoSyncCatalog") === "on";
  config.catalog.compatibilityLab = data.get("compatibilityLab") === "on";
  config.catalog.selectedModels = lines(data.get("selectedModels"));
  config.catalog.modelPickerOrder = lines(data.get("modelPickerOrder"));
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
  config.accountPool.accounts = [...document.querySelectorAll<HTMLElement>(".account-pool-row")].map((row) => {
    const field = (name: string) => row.querySelector<HTMLInputElement | HTMLSelectElement>(`[data-account-field="${name}"]`)!;
    const source = field("source").value as GatewayConfiguration["accountPool"]["accounts"][number]["credential"]["source"];
    const reference = source === "none" || source === "forward" ? null : field("reference").value.trim() || null;
    const existing = config.accountPool.accounts.find((account) =>
      account.id === row.dataset.originalAccountId
        && account.providerId === row.dataset.originalProviderId);
    return {
      id: field("id").value.trim(),
      providerId: field("providerId").value,
      label: field("label").value.trim(),
      credential: mergeAccountCredential(existing?.credential, source, reference),
      enabled: (field("enabled") as HTMLInputElement).checked,
      priority: Number(field("priority").value) || 0,
      paused: (field("paused") as HTMLInputElement).checked,
      pauseUntilUnix: nullableNumber(field("pauseUntilUnix").value),
      pinned: (field("pinned") as HTMLInputElement).checked,
    };
  }).filter((account) => account.id && account.providerId && account.label);
  const nowUnix = Math.floor(Date.now() / 1000);
  const currentlyPaused = (account: GatewayConfiguration["accountPool"]["accounts"][number]) =>
    account.paused && (account.pauseUntilUnix === null || account.pauseUntilUnix > nowUnix);
  const pinnedProviders = new Set<string>();
  for (const account of config.accountPool.accounts) {
    account.pinned = account.pinned && account.enabled && !currentlyPaused(account) && !pinnedProviders.has(account.providerId);
    if (account.pinned) pinnedProviders.add(account.providerId);
  }
  const usableAccounts = new Set(config.accountPool.accounts
    .filter((account) => account.enabled && !currentlyPaused(account))
    .map((account) => `${account.providerId}\u0000${account.id}`));
  config.accountPool.activeAccounts = Object.fromEntries(Object.entries(config.accountPool.activeAccounts)
    .filter(([providerId, accountId]) => usableAccounts.has(`${providerId}\u0000${accountId}`)));
}

export function mergeAccountCredential(
  existing: ProviderCredential | undefined,
  source: ProviderCredential["source"],
  reference: string | null,
): ProviderCredential {
  if (existing?.source === source) {
    if (source === "command") return structuredClone(existing);
    return {
      ...structuredClone(existing),
      reference: source === "none" || source === "forward" ? null : reference,
    };
  }
  return {
    source,
    reference: source === "none" || source === "forward" ? null : reference,
    transport: "bearer",
    headerName: null,
    command: null,
  };
}

function nullableNumber(value: string): number | null {
  const trimmed = value.trim();
  if (!trimmed) return null;
  const parsed = Number(trimmed);
  return Number.isFinite(parsed) ? parsed : null;
}

let quietConfigurationSave: Promise<void> | null = null;

export async function saveConfiguration(
  config: GatewayConfiguration,
  message: string,
  options: { quiet?: boolean; mutate?: (config: GatewayConfiguration) => void } = {},
): Promise<void> {
  await enqueueConfigurationSave(() => withBusy("configuration", async () => {
    const next = structuredClone(state.configuration ?? config);
    if (options.mutate) options.mutate(next);
    const configuration = await invoke<GatewayConfiguration>("save_gateway_configuration", { configuration: next });
    state.configuration = configuration;
    [state.status, state.compatibilityLab, state.routeDryRuns] = await Promise.all([
      invoke<GatewayStatus>("provider_gateway_status"),
      configuration.catalog.compatibilityLab
        ? invoke<CompatibilityLabReport>("gateway_compatibility_lab")
        : Promise.resolve(null),
      invoke<RouteDryRunReport[]>("gateway_route_dry_runs"),
    ]);
    if (options.quiet) {
      patchPublishedModelControls(configuration);
      showNotice(message);
    } else {
      notify(message);
    }
  }, { render: !options.quiet }));
}

async function enqueueConfigurationSave(run: () => Promise<unknown>): Promise<void> {
  const current = (quietConfigurationSave ?? Promise.resolve())
    .catch(() => undefined)
    .then(run)
    .then(() => undefined);
  quietConfigurationSave = current;
  try {
    await current;
  } finally {
    if (quietConfigurationSave === current) quietConfigurationSave = null;
  }
}

function showNotice(text: string, tone: Notice["tone"] = "success"): void {
  state.notice = { tone, text };
  const html = `<div class="notice ${tone}" role="status"><span>${h(text)}</span><button data-action="dismiss-notice" type="button" aria-label="×">×</button></div>`;
  const existing = document.querySelector(".notice");
  if (existing) existing.outerHTML = html;
  else document.querySelector(".workspace")?.insertAdjacentHTML("afterbegin", html);
}

function patchPublishedModelControls(config: GatewayConfiguration): void {
  const entries = catalogModelEntries(config);
  for (const input of document.querySelectorAll<HTMLInputElement>('[data-action="toggle-codex-model"]')) {
    const entry = entries.find((item) => item.providerId === input.dataset.providerId && item.modelId === input.dataset.modelId);
    if (!entry || entry.imageOnly) continue;
    input.checked = entry.published;
    const row = input.closest(".model-row, .drawer-model-row");
    if (row) {
      row.classList.toggle("published", entry.published);
      row.classList.toggle("unpublished", !entry.published);
    }
    const label = row?.querySelector(".model-meta small, .drawer-model-identity small");
    if (label) label.textContent = entry.published ? t("routing.published") : t("routing.unpublished");
  }
  for (const input of document.querySelectorAll<HTMLInputElement>('[data-action="toggle-codex-provider"]')) {
    const providerId = input.dataset.providerId;
    if (!providerId) continue;
    const publishable = entries.filter((entry) => entry.providerId === providerId && !entry.imageOnly);
    const publishedCount = publishable.filter((entry) => entry.published).length;
    const allPublished = publishable.length > 0 && publishedCount === publishable.length;
    const mixed = publishedCount > 0 && !allPublished;
    input.checked = allPublished;
    input.indeterminate = mixed;
    if (mixed) input.dataset.mixed = "true";
    else delete input.dataset.mixed;
    const countLabel = t("routing.publishedCount", { n: publishedCount, total: publishable.length });
    const drawerCount = input.closest(".drawer-model-toolbar")?.querySelector("p");
    if (drawerCount) drawerCount.textContent = countLabel;
    const headerCount = input.closest(".model-provider-header")?.querySelector("small");
    if (headerCount) {
      headerCount.textContent = `${t("routing.providerGroup", { n: entries.filter((entry) => entry.providerId === providerId).length })} · ${countLabel}`;
    }
    const status = input.closest(".model-provider-header")?.querySelector(":scope > span");
    if (status) status.textContent = mixed ? "—" : allPublished ? t("routing.providerPublished") : t("routing.unpublished");
  }
  const total = document.querySelector(".model-index-sub");
  if (total) {
    const visible = entries.filter((entry) => !entry.imageOnly);
    total.textContent = t("routing.publishedCount", {
      n: visible.filter((entry) => entry.published).length,
      total: visible.length,
    });
  }
}
