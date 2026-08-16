import type {
ExternalClientIntegrationInput,
  GatewayConfiguration,
  MaintenanceFinding,
  MaintenanceFileLock,
  MaintenanceJob,
  MaintenanceProcessInfo,
  MaintenanceSeverity,
  ObservabilityBreakdown,
  ProviderDefinition,
} from "@codetas/core";
import { resolveAgentPreset, type AgentPresetId } from "./agent-presets";
import { t } from "./i18n";
import { state, isBusy } from "./state";
import { allModelIds, formatBytes, formatNumber, h, helpTip, imageModelIds, modelCount, protocolLabel, providerModelIds, statusDot } from "./format";

export function renderView(): string {
  if (!state.configuration || !state.status) return renderLoading();
  switch (state.view) {
    case "overview": return renderOverview();
    case "maintenance": return renderMaintenance();
    case "providers": return renderProviders();
    case "routing": return renderRouting();
    case "agents": return renderAgents();
    case "projects": return renderProjects();
    case "clients": return renderClients();
    case "settings": return renderSettings();
  }
}

export function renderLoading(): string {
  return `
    <div class="loading-stage">
      <div class="loading-path"><i></i><i></i><i></i><i></i></div>
      <h2>${t("loading.title")}</h2>
      <p>${t("loading.subtitle")}</p>
    </div>`;
}

export function renderOverview(): string {
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
          <p>${t("overview.hero.explainer")}</p>
        </div>
        <div class="hero-status">
          <div class="status-list">
            <div class="status-row ${status.running ? "ok" : ""}">
              <span class="status-led" aria-hidden="true"></span>
              <div class="status-label"><strong>${t("shell.gateway")}</strong><small>${status.running ? t("runtime.running") : t("runtime.stopped")}</small></div>
              <code>${h(status.url ?? t("runtime.notStarted"))}</code>
              <button class="text-button status-action" data-action="${status.running ? "stop-gateway" : "start-gateway"}" type="button" ${isBusy("gateway") ? "disabled" : ""}>
                ${isBusy("gateway") ? t("overview.hero.working") : status.running ? t("overview.hero.stopGateway") : t("overview.hero.startGateway")}
              </button>
            </div>
            <div class="status-row ${status.codexConfigured ? "ok" : ""}">
              <span class="status-led" aria-hidden="true"></span>
              <div class="status-label"><strong>${t("overview.status.codexConnection")}</strong><small>${status.codexConfigured ? t("overview.status.connected") : t("overview.status.notSet")}</small></div>
              <code>${status.codexConfigured ? h(defaultProvider?.name ?? "—") : t("overview.status.needsSetup")}</code>
              ${status.codexConfigured
                ? `<button class="text-button status-action" data-action="restore-codex" type="button" ${isBusy("codex") ? "disabled" : ""}>${t("overview.hero.disconnectCodex")}</button>`
                : `<button class="secondary status-action" data-action="install-codex" type="button">${t("overview.hero.connectCodex")}</button>`}
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

function maintenanceStatusLabel(status: MaintenanceSeverity): string {
  return t(`maintenance.status.${status}`);
}

function displayNumber(value: number | null | undefined): string {
  return value == null ? "—" : formatNumber(value);
}

function displayBytes(value: number | null | undefined): string {
  return value == null ? "—" : formatBytes(value);
}

function maintenanceStatusMark(status: MaintenanceSeverity): string {
  if (status === "healthy") return "✓";
  if (status === "critical") return "!";
  if (status === "attention") return "△";
  return "?";
}

function maintenanceMetric(label: string, value: string, note: string, status: MaintenanceSeverity): string {
  return `<article class="maintenance-metric ${status}">
    <span>${h(label)}</span>
    <strong>${h(value)}</strong>
    <small>${h(note)}</small>
  </article>`;
}

function renderMaintenanceFinding(finding: MaintenanceFinding): string {
  const details = [
    ...finding.technicalDetails,
    ...finding.affectedPaths.map((path) => `path: ${path}`),
    ...finding.affectedThreadIds.map((id) => `thread: ${id}`),
    `confidence: ${finding.confidence}`,
    `Codex shutdown required: ${finding.requiresCodexShutdown ? "yes" : "no"}`,
  ];
  return `<details class="maintenance-finding ${finding.severity}">
    <summary>
      <span class="maintenance-mark">${maintenanceStatusMark(finding.severity)}</span>
      <span><strong>${h(finding.title)}</strong><small>${h(finding.summary)}</small></span>
      <em>${h(maintenanceStatusLabel(finding.severity))}</em>
    </summary>
    <div class="maintenance-details">
      ${finding.estimatedReclaimableBytes != null ? `<p><b>${t("maintenance.reclaimable")}</b> ${h(formatBytes(finding.estimatedReclaimableBytes))}</p>` : ""}
      ${details.length ? `<ul>${details.map((detail) => `<li><code>${h(detail)}</code></li>`).join("")}</ul>` : `<p>${t("maintenance.noTechnicalDetails")}</p>`}
    </div>
  </details>`;
}

function renderMaintenanceJob(job: MaintenanceJob): string {
  const rollbackBusy = isBusy(`maintenance-rollback-${job.id}`);
  return `<details class="maintenance-job ${job.status}">
    <summary><span class="maintenance-job-status">${h(t(`maintenance.jobStatus.${job.status}`))}</span><strong>${h(new Date(job.createdAtMs).toLocaleString())}</strong><span>${h(formatBytes(job.reclaimedBytes))}</span></summary>
    <div>
      <p><code>${h(job.id)}</code>${job.finishedAtMs == null ? "" : ` · ${h(new Date(job.finishedAtMs).toLocaleString())}`}</p>
      ${job.error ? `<p class="maintenance-job-error">${h(job.error)}</p>` : ""}
      <p>${t("maintenance.history.actions", { n: formatNumber(job.actionIds.length) })}</p>
      <ul class="maintenance-job-actions">${job.actionIds.map((id) => `<li><code>${h(id)}</code></li>`).join("")}</ul>
      ${job.rollbackAvailable ? `<button class="secondary compact" data-action="rollback-maintenance" data-job-id="${h(job.id)}" data-job-status="${h(job.status)}" type="button" ${rollbackBusy ? "disabled" : ""}>${rollbackBusy ? t("maintenance.rollbackRunning") : job.status === "waitingForIdle" ? t("maintenance.cancelWaiting") : t("maintenance.rollback")}</button>` : `<span class="legend">${h(t("maintenance.rollbackStatus.notAvailable"))}</span>`}
    </div>
  </details>`;
}

function maintenanceThreadStatus(status: string | null): string {
  if (status === "notLoaded") return t("maintenance.threadStatus.notLoaded");
  if (status === "active") return t("maintenance.threadStatus.active");
  if (status === "idle") return t("maintenance.threadStatus.idle");
  return t("maintenance.threadStatus.unknown");
}

function maintenanceSessionTitle(lock: MaintenanceFileLock): string {
  const previewLine = lock.preview?.split(/\r?\n/, 1)[0]?.trim();
  return lock.threadName?.trim() || previewLine || lock.threadId || t("maintenance.unknownTask");
}

function renderMaintenanceSession(lock: MaintenanceFileLock): string {
  const title = maintenanceSessionTitle(lock);
  const tooltipId = `maintenance-preview-${lock.threadId ?? lock.process.pid}`;
  const canTerminateWriter = lock.process.name.toLowerCase() === "codex" && lock.process.terminal != null;
  const updated = lock.updatedAtMs == null ? t("maintenance.updatedUnknown") : new Date(lock.updatedAtMs).toLocaleString();
  return `<article class="maintenance-session-card">
    <span class="maintenance-mark attention">!</span>
    <div class="maintenance-session-copy">
      <div class="maintenance-session-heading">
        <span class="maintenance-session-title" tabindex="0" ${lock.preview ? `aria-describedby="${h(tooltipId)}"` : ""}>
          <strong>${h(title)}</strong>
          ${lock.preview ? `<span class="maintenance-session-preview" id="${h(tooltipId)}" role="tooltip">${h(lock.preview)}</span>` : ""}
        </span>
        <span class="maintenance-session-status">${h(maintenanceThreadStatus(lock.threadStatus))}</span>
      </div>
      <p>${t(lock.threadStatus === "notLoaded" ? "maintenance.sessionReason.notLoaded" : "maintenance.sessionReason.open")}</p>
      <small>${t("maintenance.sessionUpdated", { date: updated })}</small>
      <details class="maintenance-session-technical">
        <summary>${t("maintenance.technicalDetails")}</summary>
        <code>${h(lock.threadId ?? t("maintenance.unknownTask"))}</code>
        <code>${h(lock.path)}</code>
        ${lock.cwd ? `<code>${h(lock.cwd)}</code>` : ""}
        <code>${h(lock.process.name)} · PID ${h(lock.process.pid)} · parent ${h(lock.process.parentName ?? "—")} (${h(lock.process.parentPid ?? "—")}) · ${h(lock.process.startedAt ?? "started —")} · ${h(lock.process.terminal ?? "terminal —")}</code>
      </details>
    </div>
    <div class="maintenance-lock-actions">
      ${canTerminateWriter ? `<button class="secondary compact" data-action="terminate-codex-writer" data-pid="${h(lock.process.pid)}" data-started-at="${h(lock.process.startedAt ?? "")}" data-thread-id="${h(lock.threadId ?? "")}" title="${h(!lock.threadId || !lock.process.startedAt ? t("maintenance.writerIdentityMissing") : "")}" type="button" ${!lock.threadId || !lock.process.startedAt || isBusy(`terminate-writer-${lock.process.pid}`) ? "disabled" : ""}>${t("maintenance.terminateWriter")}</button>` : ""}
      ${lock.threadId ? `<button class="text-button" data-action="retry-codex-archive" data-thread-id="${h(lock.threadId)}" type="button" ${isBusy(`retry-archive-${lock.threadId}`) ? "disabled" : ""}>${t("maintenance.retryArchive")}</button>` : ""}
    </div>
  </article>`;
}

function renderOrphanProcess(process: MaintenanceProcessInfo): string {
  return `<article class="maintenance-orphan-card">
    <span class="maintenance-mark attention">!</span>
    <p><strong>${t("maintenance.orphanCandidate")}</strong><small>${t("maintenance.orphanReason")}</small></p>
    <code>${h(process.name)} · PID ${h(process.pid)} · parent ${h(process.parentName ?? "—")} (${h(process.parentPid ?? "—")}) · ${h(process.startedAt ?? "started —")}</code>
  </article>`;
}

function renderMaintenanceOptimizer(): string {
  const input = state.maintenancePreviewInput;
  const maintenanceBusy = isBusy("maintenance-execute");
  return `<section class="panel maintenance-optimize-panel">
    <header><div><span class="eyebrow">${t("maintenance.optimize.eyebrow")}</span><h3>${t("maintenance.optimize.title")}</h3></div><span class="chip ready">${t("maintenance.optimize.diagnosed")}</span></header>
    <div class="maintenance-optimize-intro"><p>${t("maintenance.optimize.copy")}</p></div>
    <div class="maintenance-optimize-controls">
      <label><span>${t("maintenance.optimize.retention")}</span><select id="maintenance-retention">
        <option value="7" ${input.logRetentionDays === 7 ? "selected" : ""}>${t("maintenance.optimize.days7")}</option>
        <option value="30" ${input.logRetentionDays === 30 ? "selected" : ""}>${t("maintenance.optimize.days30")}</option>
        <option value="90" ${input.logRetentionDays === 90 ? "selected" : ""}>${t("maintenance.optimize.days90")}</option>
        <option value="never" ${input.logRetentionDays == null ? "selected" : ""}>${t("maintenance.optimize.never")}</option>
      </select></label>
      <label class="maintenance-check"><input id="maintenance-compact-sqlite" type="checkbox" ${input.compactSqlite ? "checked" : ""}><span>${t("maintenance.optimize.compactDb")}</span></label>
      <label class="maintenance-check"><input id="maintenance-orphan-pins" type="checkbox" ${input.repairOrphanPins ? "checked" : ""}><span>${t("maintenance.optimize.orphanPins")}</span></label>
      ${input.disableMcpServers.length ? `<div class="maintenance-mcp-selection"><b>${t("maintenance.optimize.disableMcp")}</b>${input.disableMcpServers.map((name) => `<code>${h(name)}</code>`).join("")}<button class="text-button" data-action="clear-maintenance-mcp" type="button">${t("maintenance.optimize.clearMcp")}</button></div>` : ""}
    </div>
    <div class="maintenance-optimize-submit"><p><strong>${t("maintenance.optimize.readyTitle")}</strong><span>${t("maintenance.optimize.readyCopy")}</span></p><button class="primary" data-action="execute-maintenance" type="button" ${maintenanceBusy ? "disabled" : ""}>${maintenanceBusy ? t("maintenance.optimize.executing") : t("maintenance.optimize.execute")}</button></div>
  </section>`;
}

export function renderMaintenanceHistory(): string {
  return `<section class="panel maintenance-history-panel"><header><div><h3>${t("maintenance.history.title")}</h3></div><button class="secondary compact" data-action="refresh-maintenance-jobs" type="button" ${isBusy("maintenance-jobs") ? "disabled" : ""}>${t("maintenance.history.refresh")}</button></header><div class="maintenance-jobs">${state.maintenanceJobs.map(renderMaintenanceJob).join("") || `<div class="maintenance-inline-ok"><span>—</span>${t("maintenance.history.empty")}</div>`}</div></section>`;
}

export function renderMaintenance(): string {
  const report = state.maintenance;
  if (!report) {
    const empty = `<section class="maintenance-empty">
      <div class="maintenance-orbit" aria-hidden="true"><i></i><span>R/O</span></div>
      <span class="eyebrow">CODEX DOCTOR</span>
      <h2>${t("maintenance.empty.title")}</h2>
      <p>${t("maintenance.empty.copy")}</p>
      <div class="maintenance-safety"><strong>${t("maintenance.readOnly")}</strong><span>${t("maintenance.empty.safety")}</span></div>
      <button class="primary" data-action="run-maintenance" type="button" ${isBusy("maintenance") ? "disabled" : ""}>${isBusy("maintenance") ? t("maintenance.running") : t("maintenance.run")}</button>
    </section>`;
    return state.maintenanceJobs.length
      ? `<div class="maintenance-dashboard">${empty}${renderMaintenanceHistory()}</div>`
      : empty;
  }

  const session = report.storage.find((entry) => entry.id === "sessions");
  const archive = report.storage.find((entry) => entry.id === "archives");
  const freeStatus: MaintenanceSeverity = (report.system.diskFreeBytes ?? Number.MAX_SAFE_INTEGER) < 10 * 1024 ** 3
    ? "critical"
    : (report.system.diskFreeBytes ?? Number.MAX_SAFE_INTEGER) < 25 * 1024 ** 3 ? "attention" : "healthy";
  const dbStatus: MaintenanceSeverity = (report.sqlite.reclaimableBytes ?? 0) >= 2 * 1024 ** 3
    ? "critical"
    : (report.sqlite.reclaimableBytes ?? 0) >= 512 * 1024 ** 2 ? "attention" : report.sqlite.available ? "healthy" : "unknown";
  const taskStatus: MaintenanceSeverity = report.fileLocks.length || report.orphanProcesses.length ? "attention" : "healthy";
  const mcpAttention = report.mcp.some((item) => item.status !== "healthy") || (report.mcpMaxStartupMs ?? 0) >= 10_000;

  return `<div class="maintenance-dashboard">
    <section class="maintenance-command ${report.overallStatus}">
      <div class="maintenance-spine" aria-hidden="true"><i></i><i></i><i></i><i></i></div>
      <div class="maintenance-command-copy">
        <span class="eyebrow">CODEX DOCTOR · ${t("maintenance.readOnly")}</span>
        <h2>${t("maintenance.hero", { status: maintenanceStatusLabel(report.overallStatus) })}</h2>
        <p>${h(report.privacyNote)}</p>
        <small>${t("maintenance.generated", { date: new Date(report.generatedAtMs).toLocaleString(), ms: formatNumber(report.durationMs) })}</small>
      </div>
      <div class="maintenance-command-actions">
        <button class="primary" data-action="run-maintenance" type="button" ${isBusy("maintenance") ? "disabled" : ""}>${isBusy("maintenance") ? t("maintenance.running") : t("maintenance.runAgain")}</button>
        <button class="secondary" data-action="export-maintenance" type="button">${t("maintenance.export")}</button>
      </div>
    </section>

    ${renderMaintenanceOptimizer()}

    <section class="maintenance-metrics">
      ${maintenanceMetric(t("maintenance.diskFree"), displayBytes(report.system.diskFreeBytes), report.system.diskUsedPercent == null ? "—" : t("maintenance.diskUsed", { n: formatNumber(report.system.diskUsedPercent) }), freeStatus)}
      ${maintenanceMetric(t("maintenance.logDatabase"), formatBytes(report.sqlite.physicalBytes), t("maintenance.dbReclaim", { size: displayBytes(report.sqlite.reclaimableBytes) }), dbStatus)}
      ${maintenanceMetric(t("maintenance.tasks"), displayNumber(session?.fileCount), t("maintenance.taskStorage", { active: displayBytes(session?.bytes), archive: displayBytes(archive?.bytes) }), taskStatus)}
      ${maintenanceMetric("MCP", report.mcpMaxStartupMs == null ? "—" : `${(report.mcpMaxStartupMs / 1000).toFixed(1)}s`, t("maintenance.mcpIssues", { n: formatNumber(report.mcp.filter((item) => item.status !== "healthy").length) }), mcpAttention ? "attention" : "healthy")}
    </section>

    <section class="maintenance-layout">
      <article class="panel maintenance-findings-panel">
        <header><div><h3>${t("maintenance.findings")}</h3></div><span class="legend">${t("maintenance.findingCount", { n: formatNumber(report.findings.length) })}</span></header>
        <div class="maintenance-findings">
          ${report.findings.length ? report.findings.map(renderMaintenanceFinding).join("") : `<div class="maintenance-clear"><b>✓</b><strong>${t("maintenance.allClear")}</strong><p>${t("maintenance.allClearCopy")}</p></div>`}
        </div>
      </article>

      <article class="panel maintenance-db-panel">
        <header><div><h3>${t("maintenance.sqliteTitle")}</h3></div><span class="chip ${report.sqlite.available ? "ready" : ""}">${report.sqlite.available ? "SQLite" : t("maintenance.unavailable")}</span></header>
        <div class="maintenance-db-gauge">
          <div style="--reclaim:${report.sqlite.physicalBytes ? Math.min(100, (report.sqlite.reclaimableBytes ?? 0) / report.sqlite.physicalBytes * 100) : 0}%"><i></i></div>
          <p><strong>${h(formatBytes(report.sqlite.physicalBytes))}</strong><span>${t("maintenance.physicalSize")}</span></p>
          <p><strong>${h(formatBytes(report.sqlite.reclaimableBytes))}</strong><span>${t("maintenance.reclaimable")}</span></p>
        </div>
        <div class="maintenance-facts">
          <span>page_size <b>${h(displayNumber(report.sqlite.pageSize))}</b></span>
          <span>page_count <b>${h(displayNumber(report.sqlite.pageCount))}</b></span>
          <span>freelist <b>${h(displayNumber(report.sqlite.freelistCount))}</b></span>
          <span>journal <b>${h(report.sqlite.journalMode ?? "—")}</b></span>
          <span>${t("maintenance.liveData")} <b>${report.sqlite.estimatedLiveBytes == null ? "—" : h(formatBytes(report.sqlite.estimatedLiveBytes))}</b></span>
          <span>query <b>${report.sqlite.queryDurationMs == null ? "—" : `${h(formatNumber(report.sqlite.queryDurationMs))} ms`}</b></span>
        </div>
        <p class="maintenance-distinction">${t("maintenance.sqliteDistinction")}</p>
      </article>

      <article class="panel maintenance-storage-panel">
        <header><div><h3>${t("maintenance.storageTitle")}</h3></div><span class="legend">${t("maintenance.readOnly")}</span></header>
        <div class="maintenance-storage-list">${report.storage.map((entry) => `<div>
          <span class="maintenance-mark ${entry.status}">${maintenanceStatusMark(entry.status)}</span>
          <p><strong>${h(entry.label)}</strong><code>${h(entry.path)}</code></p>
          <span><b>${h(formatBytes(entry.bytes))}</b><small>${entry.fileCount == null ? "—" : t("maintenance.filesAndDirs", { files: formatNumber(entry.fileCount), dirs: formatNumber(entry.directoryCount) })}${entry.id === "worktrees" && entry.topLevelDirectoryCount != null ? ` · ${t("maintenance.worktrees", { n: formatNumber(entry.topLevelDirectoryCount) })}` : ""}${entry.scanTruncated ? " +" : ""}${entry.recent24hModifiedBytes == null ? "" : ` · ${t("maintenance.recent24h", { size: formatBytes(entry.recent24hModifiedBytes) })}`}</small></span>
        </div>`).join("")}</div>
      </article>

      <article class="panel maintenance-process-panel">
        <header><div><h3>${t("maintenance.processTitle")}</h3></div><span class="legend">${t("maintenance.processSummary", { sessions: formatNumber(report.fileLocks.length), orphans: formatNumber(report.orphanProcesses.length) })}</span></header>
        ${report.fileLocks.some((lock) => lock.process.terminal == null && lock.process.parentName === "ChatGPT") ? `<div class="maintenance-shared-writer"><p><strong>${t("maintenance.sharedWriterTitle")}</strong><span>${t("maintenance.sharedWriterCopy")}</span></p><button class="secondary compact" data-action="request-codex-shutdown" type="button" ${isBusy("codex-shutdown") ? "disabled" : ""}>${isBusy("codex-shutdown") ? t("maintenance.shutdownRunning") : t("maintenance.shutdown")}</button></div>` : ""}
        <div class="maintenance-session-list">
          ${report.fileLocks.length ? report.fileLocks.map(renderMaintenanceSession).join("") : `<div class="maintenance-inline-ok"><span>✓</span>${t("maintenance.noLocks")}</div>`}
        </div>
        ${report.orphanProcesses.length ? `<section class="maintenance-orphan-section"><h4>${t("maintenance.orphanTitle")}</h4>${report.orphanProcesses.map(renderOrphanProcess).join("")}</section>` : ""}
      </article>

      <article class="panel maintenance-mcp-panel">
        <header><div><h3>MCP</h3></div><span class="legend">${report.mcpMaxStartupMs == null ? "—" : t("maintenance.maxStartup", { seconds: (report.mcpMaxStartupMs / 1000).toFixed(1) })}</span></header>
        <div class="maintenance-table">${report.mcp.map((item) => `<div><span class="maintenance-mark ${item.status}">${maintenanceStatusMark(item.status)}</span><strong>${h(item.name)}<small>${item.enabled ? t("maintenance.mcpEnabled") : t("maintenance.mcpDisabled")}${item.startupMs == null ? "" : ` · ${h(formatNumber(item.startupMs))} ms`}</small></strong><span>${t("maintenance.errors", { n: formatNumber(item.errorCount) })}</span><span>${t("maintenance.authErrors", { n: formatNumber(item.authErrorCount) })}</span>${item.disableCandidate ? `<button class="secondary compact" data-action="select-disable-mcp" data-server="${h(item.name)}" type="button">${t("maintenance.selectDisable")}</button>` : ""}</div>`).join("") || `<div class="maintenance-inline-ok"><span>—</span>${t("maintenance.noMcp")}</div>`}</div>
      </article>

      <article class="panel maintenance-git-panel">
        <header><div><h3>Git</h3></div><span class="legend">${t("maintenance.repositories", { n: formatNumber(report.git.length) })}</span></header>
        <div class="maintenance-git-safety">${t("maintenance.gitNoAutoEdit")}</div><div class="maintenance-git-list">${report.git.map((repo) => `<details><summary><span class="maintenance-mark ${repo.status}">${maintenanceStatusMark(repo.status)}</span><code>${h(repo.path)}</code><b>${h(displayNumber(repo.changedFiles))} files</b></summary><div><p>${h(repo.note)}</p><dl><div><dt>diff</dt><dd>${repo.estimatedDiffBytes == null ? "—" : h(formatBytes(repo.estimatedDiffBytes))}</dd></div><div><dt>untracked</dt><dd>${h(displayNumber(repo.untrackedFiles))}</dd></div><div><dt>upstream</dt><dd>${repo.upstreamConfigured === null ? "—" : repo.upstreamConfigured ? "OK" : "NG"}</dd></div><div><dt>origin</dt><dd>${repo.originConfigured === null ? "—" : repo.originConfigured ? "OK" : "NG"}</dd></div></dl>${repo.generatedFileCandidates.length ? `<p><b>${t("maintenance.generatedCandidates")}</b></p><ul>${repo.generatedFileCandidates.map((path) => `<li><code>${h(path)}</code></li>`).join("")}</ul>` : ""}${repo.gitignoreCandidates.length ? `<p><b>${t("maintenance.gitignoreCandidates")}</b></p><ul>${repo.gitignoreCandidates.map((path) => `<li><code>${h(path)}</code></li>`).join("")}</ul>` : ""}</div></details>`).join("") || `<div class="maintenance-inline-ok"><span>—</span>${t("maintenance.noRepositories")}</div>`}</div>
      </article>
    </section>

    ${renderMaintenanceHistory()}

    <footer class="maintenance-system-strip">
      <span><b>Memory free</b> ${report.system.memoryFreePercent == null ? "—" : `${h(formatNumber(report.system.memoryFreePercent))}%`}</span>
      <span><b>Swap</b> ${h(displayBytes(report.system.swapUsedBytes))} / ${h(displayBytes(report.system.swapTotalBytes))}</span>
      <span><b>${t("maintenance.partial")}</b> ${h(formatNumber(report.partialFailures.length))}</span>
    </footer>
  </div>`;
}

export function renderUsageBars(rows: ObservabilityBreakdown["providers"]): string {
  const max = Math.max(1, ...rows.map((row) => row.requests));
  return rows.slice(0, 6).map((row) => `<div class="usage-bar"><div><strong>${h(row.key)}</strong><span>${formatNumber(row.requests)}</span></div><i><b style="width:${Math.max(2, row.requests / max * 100)}%"></b></i><small>${t("usage.row", { tokens: formatNumber(row.totalTokens), ms: row.requests ? Math.round(row.totalLatencyMs / row.requests) : 0 })}</small></div>`).join("") || `<div class="empty-inline">${t("usage.empty")}</div>`;
}

export function renderProviders(): string {
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

export function renderLocalCliRows(): string {
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

export function renderDirectApiRows(): string {
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

export function renderProviderCard(provider: ProviderDefinition): string {
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
        ${state.oauthProviders.some((item) => item.id === provider.id || item.aliases.includes(provider.id)) && (["oAuth", "command"].includes(provider.credential?.source ?? "") ? state.providerTestFailed.has(provider.id) : true) ? `<button data-action="oauth-provider" data-provider-id="${h(provider.id)}" type="button">${provider.id === "anthropic" ? t("card.loginClaude") : provider.id === "kimi" ? t("card.loginKimi") : provider.id === "xai" ? t("card.loginGrok") : t("card.loginOAuth")}</button>` : ""}
        <button data-action="refresh-models" data-provider-id="${h(provider.id)}" type="button">${t("card.fetchModels")}</button>
        <button data-action="edit-provider" data-provider-id="${h(provider.id)}" type="button">${t("card.edit")}</button>
        ${config.defaultProvider !== provider.id ? `<button data-action="default-provider" data-provider-id="${h(provider.id)}" type="button">${t("card.makeDefault")}</button>` : ""}
      </div>
    </article>`;
}

export function renderRouting(): string {
  const config = state.configuration!;
  const models = providerModelIds(config);
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
        <section class="compatibility-lab"><h3>${t("routing.dryRun")}</h3>${state.routeDryRuns.map((report) => `<div class="dry-run-row"><strong>${h(report.requestedModel)}</strong><small>${report.selected ? t("routing.selected", { model: report.selected }) : t("routing.noCandidate")}</small>${report.candidates.map((candidate) => `<code class="${candidate.eligible ? "eligible" : "excluded"}">#${candidate.rank} ${h(candidate.target)}${candidate.accountId ? `#${h(candidate.accountId)}` : ""} health=${candidate.healthPercent}%: ${candidate.eligible ? candidate.score : h(candidate.reasons.join(", "))}</code>`).join("")}</div>`).join("") || `<p>${t("routing.noDryRun")}</p>`}</section>
        ${config.catalog.compatibilityLab && state.compatibilityLab ? `<section class="compatibility-lab"><h3>${t("routing.compatibilityLab")}</h3><p>${t("routing.readOnly")}</p><div class="compatibility-table">${state.compatibilityLab.rows.map((row) => `<div><code>${h(row.providerId)}</code><span>${h(row.fixtureId)}</span><b class="${row.status}">${row.status === "pass" ? "✓" : row.status === "fail" ? "✕" : "—"}</b><small>${h(row.reason)}</small></div>`).join("")}</div></section>` : ""}
      </aside>
    </div>`;
}

export function renderRouteEditor(route: GatewayConfiguration["routes"][number], index: number, models: string[]): string {
  return `<article class="route-editor" data-route-index="${index}">
    <div class="route-head"><input data-field="name" value="${h(route.name)}" aria-label="${t("route.name")}"/><label class="switch"><input data-field="enabled" type="checkbox" ${route.enabled ? "checked" : ""}/><span></span></label></div>
    <label class="route-description">${labelWithHelp(t("route.description"), t("route.descriptionHelp"))}<textarea data-field="description" rows="2" maxlength="1000" placeholder="${h(t("route.descriptionPlaceholder"))}">${h(route.description ?? "")}</textarea></label>
    <div class="form-grid two"><label>${t("route.id")}<input data-field="id" value="${h(route.id)}" /></label><label>${t("route.alias")}<input data-field="alias" value="${h(route.alias ?? "")}" /></label></div>
    <div class="form-grid two"><label>${t("route.strategy")}<select data-field="strategy"><option value="failover" ${route.strategy === "failover" ? "selected" : ""}>Failover</option><option value="weightedRoundRobin" ${route.strategy === "weightedRoundRobin" ? "selected" : ""}>Weighted round robin</option><option value="leastUsage" ${route.strategy === "leastUsage" ? "selected" : ""}>Least usage</option><option value="policy" ${route.strategy === "policy" ? "selected" : ""}>Policy</option></select></label><label>${labelWithHelp(t("route.defaultEffort"), t("effort.help"))}${renderReasoningEffortSelect({ dataField: "defaultReasoningEffort", selected: route.defaultReasoningEffort })}</label></div>
    <details class="route-policy" ${route.strategy === "policy" ? "open" : ""}><summary>${t("route.policy")}</summary>
      <label>${t("route.requiredCapabilities")}<input data-field="requiredCapabilities" value="${h(route.policy.requiredCapabilities.join(", "))}" placeholder="vision, tools" /></label>
      <div class="form-grid four"><label>${t("route.healthWeight")}<input data-field="healthWeight" type="number" min="0" max="65535" value="${route.policy.healthWeight}" /></label><label>${t("route.costWeight")}<input data-field="costWeight" type="number" min="0" max="65535" value="${route.policy.costWeight}" /></label><label>${t("route.quotaWeight")}<input data-field="quotaWeight" type="number" min="0" max="65535" value="${route.policy.quotaWeight}" /></label><label>${t("route.contextWeight")}<input data-field="contextWeight" type="number" min="0" max="65535" value="${route.policy.contextWeight}" /></label></div>
      <div class="form-grid two"><label>${t("route.maxInputCost")}<input data-field="maxInputPricePerMillion" type="number" min="0" step="0.0001" value="${route.policy.maxInputPricePerMillion ?? ""}" /></label><label>${t("route.maxOutputCost")}<input data-field="maxOutputPricePerMillion" type="number" min="0" step="0.0001" value="${route.policy.maxOutputPricePerMillion ?? ""}" /></label></div>
    </details>
    <div class="route-target-field">
      <div class="field-heading"><span>${t("route.targets")}</span><small>${t("route.targetsHint")}</small></div>
      <div class="route-target-list">${route.targets.map((target) => renderRouteTargetRow(target, models)).join("")}</div>
      <button class="secondary compact add-row-button" data-action="add-route-target" type="button">${t("route.addTarget")}</button>
    </div>
    <div class="route-foot"><span data-route-target-count>${t("route.count", { n: route.targets.length })}</span><button class="danger-link" data-action="remove-route" data-route-index="${index}" type="button">${t("route.remove")}</button></div>
  </article>`;
}

export function renderRouteTargetRow(target: { model: string; weight: number }, models: string[]): string {
  return `<div class="route-target-row">
    ${renderSearchableModelSelect({ selected: target.model, models, dataField: "targetModel", allowEmpty: false })}
    <label class="route-weight">${t("route.weight")}<input data-field="targetWeight" type="number" min="1" max="65535" value="${Math.max(1, target.weight)}" /></label>
    ${renderModelRowActions("remove-route-target")}
  </div>`;
}

export function renderModelRows(config: GatewayConfiguration, query: string): string {
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

export function renderAgents(): string {
    const config = state.configuration!;
    const options = allModelIds(config);
    const imageOptions = imageModelIds(config);
  const presets = (["deepseek-gpt", "kimi-gpt", "current-gpt"] as AgentPresetId[]).map((id) => resolveAgentPreset(config, id));
  const plugin = state.codexPluginStatus;
  const pluginReady = Boolean(plugin?.installed && plugin.enabled && plugin.mcpHealthy && plugin.gatewayConnected && plugin.gatewayReachable && state.status?.running);
  return `
    <form id="agents-form" class="agent-layout">
      <section class="panel agent-core-panel">
        <header><div><h2>${t("agents.parallel")}</h2></div><label class="switch labeled"><input name="multiAgentV2" type="checkbox" ${config.agents.multiAgentV2 ? "checked" : ""}/><span></span><b>${config.agents.multiAgentV2 ? t("agents.on") : t("agents.off")}</b></label></header>
        <div class="agent-topology">
          <div class="agent-main"><span>${t("agents.main")}</span><strong>${h(config.defaultProvider ?? t("agents.default"))}</strong><small>${t("agents.effort")} ${h(config.agents.effortCap ?? t("agents.default"))}</small></div>
          <div class="agent-branches">${Array.from({ length: Math.min(config.agents.maxThreads, 6) }, (_, index) => `<span style="--i:${index}">A${index + 1}</span>`).join("")}</div>
        </div>
        <div class="form-grid four">
          <label>${t("agents.surface")}<select name="surfaceMode"><option value="v1" ${config.agents.surfaceMode === "v1" ? "selected" : ""}>v1 compatible</option><option value="default" ${config.agents.surfaceMode === "default" ? "selected" : ""}>Default</option><option value="v2" ${config.agents.surfaceMode === "v2" ? "selected" : ""}>v2 native</option></select></label>
          <label>${t("agents.maxThreads")}<input name="maxThreads" type="number" min="1" max="64" value="${config.agents.maxThreads}" /></label>
          <label>${labelWithHelp(t("agents.mainEffort"), t("effort.help"))}${renderReasoningEffortSelect({ name: "effortCap", selected: config.agents.effortCap })}</label>
          <label>${labelWithHelp(t("agents.subagentEffort"), t("effort.help"))}${renderReasoningEffortSelect({ name: "subagentEffortCap", selected: config.agents.subagentEffortCap })}</label>
        </div>
        ${renderModelRoster("subagentModels", t("agents.subagents"), t("agents.subagentsHint"), config.agents.subagentModels, options)}
        ${renderModelRoster("subagentFallback", t("agents.fallback"), t("agents.fallbackHint"), config.agents.subagentFallback, options)}
        <label>${t("agents.modelFallbackMap")}<small>${t("agents.modelFallbackMapHint")}</small><textarea name="subagentFallbackByModel" rows="6" spellcheck="false">${h(JSON.stringify(config.agents.subagentFallbackByModel, null, 2))}</textarea></label>
        <section class="agent-presets">
          <div class="agent-section-heading"><div><h3>${t("agents.presets")}</h3><p>${t("agents.presetsHint")}</p></div>${helpTip(t("agents.help.presets"))}</div>
          <div class="preset-choice-grid">
            ${presets.map((preset) => renderAgentPreset(preset)).join("")}
          </div>
        </section>
        <section class="agent-input-fallback">
          <div class="agent-section-heading"><div><h3>${t("agents.inputFallback")}</h3><p>${t("agents.inputFallbackHint")}</p></div></div>
          <div class="form-grid three">
            ${renderInputModeSelect("imageInputMode", t("agents.imageInputMode"), config.agents.imageInputMode, t("agents.help.imageMode"))}
            ${renderInputModeSelect("videoInputMode", t("agents.videoInputMode"), config.agents.videoInputMode, t("agents.help.videoMode"))}
            ${renderInputModeSelect("documentInputMode", t("agents.documentInputMode"), config.agents.documentInputMode, t("agents.help.documentMode"))}
          </div>
          <div class="form-grid three">
            <label>${labelWithHelp(t("agents.auxiliaryTimeout"), t("agents.help.timeout"))}<input name="auxiliaryTimeoutSeconds" type="number" min="1" max="600" value="${Math.round(config.agents.auxiliaryTimeoutMs / 1000)}" /></label>
            <label>${labelWithHelp(t("agents.videoFrames"), t("agents.help.videoFrames"))}<input name="videoSampleFrames" type="number" min="1" max="64" value="${config.agents.videoSampleFrames}" /></label>
            <label>${labelWithHelp(t("agents.documentPages"), t("agents.help.documentPages"))}<input name="documentMaxPages" type="number" min="1" max="100" value="${config.agents.documentMaxPages}" /></label>
          </div>
          <label class="checkbox-row"><input name="ocrEnabled" type="checkbox" ${config.agents.ocrEnabled ? "checked" : ""}/><span>${labelWithHelp(t("agents.ocr"), t("agents.help.ocr"))}</span></label>
        </section>
        <section class="agent-media-tests">
          <div class="agent-section-heading"><div><h3>${t("agents.tests")}</h3><p>${t("agents.testsHint")}</p></div>${helpTip(t("agents.help.tests"))}</div>
          <label>${t("agents.testPrompt")}<input id="agent-test-prompt" type="text" placeholder="${t("agents.testPromptPlaceholder")}" /></label>
          <div class="agent-test-buttons">
            ${renderMediaTestButton("image", t("agents.testImage"))}
            ${renderMediaTestButton("video", t("agents.testVideo"))}
            ${renderMediaTestButton("document", t("agents.testDocument"))}
            ${renderMediaTestButton("imageGeneration", t("agents.testImageGeneration"))}
          </div>
          ${renderAgentTestResult()}
        </section>
      </section>
      <aside class="panel sidecar-panel">
        <header><div><h3>${t("agents.sidecar")}</h3></div></header>
        <div class="plugin-connection ${pluginReady ? "ready" : "needs-attention"}" ${plugin?.healthDetail ? `title="${h(plugin.healthDetail)}"` : ""}>
          <div class="plugin-connection-title">${statusDot(pluginReady, Boolean(plugin?.installed && plugin.enabled && (!plugin.mcpHealthy || !plugin.gatewayConnected || !plugin.gatewayReachable || !state.status?.running)))}<span><strong>${t("agents.codexPlugin")}</strong><small>${pluginStatusLabel()}</small></span>${helpTip(t("agents.help.plugin"))}</div>
          <button class="text-button compact" data-action="refresh-codex-plugin-status" type="button" ${isBusy("codex-plugin-status") ? "disabled" : ""}>${t("agents.recheck")}</button>
        </div>
        ${renderModelSelect("webSearchModel", "Web search", config.sidecars.webSearchModel, options)}
        ${renderModelSelect("visionModel", t("agents.visionModel"), config.sidecars.visionModel, options, t("agents.help.visionModel"))}
        ${renderModelSelect("videoInputModel", t("agents.videoInputModel"), config.sidecars.videoInputModel, options, t("agents.help.videoModel"))}
        ${renderModelSelect("documentModel", t("agents.documentModel"), config.sidecars.documentModel, options, t("agents.help.documentModel"))}
        ${renderModelSelect("imageModel", t("agents.imageModel"), config.sidecars.imageModel, imageOptions, t("agents.help.imageModel"))}
        ${renderModelSelect("videoModel", t("agents.videoModel"), config.sidecars.videoModel, options)}
        ${renderModelSelect("liveModel", "Realtime", config.sidecars.liveModel, options)}
        <button class="primary wide" type="submit">${t("agents.save")}</button>
      </aside>
    </form>`;
}

function labelWithHelp(label: string, help: string): string {
  return `<span class="field-label">${h(label)}${helpTip(help)}</span>`;
}

const REASONING_EFFORTS = ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"] as const;

type ReasoningEffortSelectOptions = {
  name?: string;
  dataField?: string;
  selected: string | null;
};

function renderReasoningEffortSelect({ name, dataField, selected }: ReasoningEffortSelectOptions): string {
  const attributes = [
    name ? `name="${h(name)}"` : "",
    dataField ? `data-field="${h(dataField)}"` : "",
  ].filter(Boolean).join(" ");
  return `<select ${attributes}><option value="">${t("effort.default")}</option>${REASONING_EFFORTS.map((effort) => `<option value="${effort}" ${selected === effort ? "selected" : ""}>${t(`effort.${effort}`)}</option>`).join("")}</select>`;
}

function renderInputModeSelect(name: string, label: string, selected: "auto" | "native" | "text", help: string): string {
  return `<label>${labelWithHelp(label, help)}<select name="${h(name)}"><option value="auto" ${selected === "auto" ? "selected" : ""}>${t("agents.modeAuto")}</option><option value="native" ${selected === "native" ? "selected" : ""}>${t("agents.modeNative")}</option><option value="text" ${selected === "text" ? "selected" : ""}>${t("agents.modeText")}</option></select></label>`;
}

export function renderModelSelect(name: string, label: string, selected: string | null, models: string[], help?: string): string {
  return `<label>${help ? labelWithHelp(label, help) : h(label)}${renderSearchableModelSelect({ name, selected, models, allowEmpty: true })}</label>`;
}

export type SearchableModelSelectOptions = {
  name?: string;
  dataField?: string;
  selected: string | null;
  models: string[];
  allowEmpty: boolean;
};

export function renderSearchableModelSelect({ name, dataField, selected, models, allowEmpty }: SearchableModelSelectOptions): string {
  const value = selected ?? "";
  const options = [...new Set([...(value ? [value] : []), ...models])];
  const selectAttributes = [
    name ? `name="${h(name)}"` : "",
    dataField ? `data-field="${h(dataField)}"` : "",
  ].filter(Boolean).join(" ");
  return `<div class="searchable-model-select">
    <input type="search" data-model-filter placeholder="${h(t("modelPicker.search"))}" aria-label="${h(t("modelPicker.search"))}" autocomplete="off" />
    <select ${selectAttributes} aria-label="${h(t("modelPicker.choose"))}">
      ${allowEmpty ? `<option value="">${t("agents.unused")}</option>` : `<option value="" disabled ${value ? "" : "selected"}>${t("modelPicker.choose")}</option>`}
      ${options.map((model) => `<option value="${h(model)}" ${value === model ? "selected" : ""}>${h(model)}</option>`).join("")}
    </select>
    <small class="model-filter-empty" hidden>${t("modelPicker.noMatches")}</small>
  </div>`;
}

export function renderModelRosterRow(name: string, selected: string | null, models: string[]): string {
  return `<div class="model-roster-row">
    ${renderSearchableModelSelect({ name, selected, models, allowEmpty: false })}
    ${renderModelRowActions("remove-model-roster-row")}
  </div>`;
}

function renderModelRowActions(removeAction: string): string {
  return `<div class="model-row-actions">
    <button class="text-button row-order-button" data-action="move-model-row-up" type="button" aria-label="${t("modelPicker.moveUp")}">↑</button>
    <button class="text-button row-order-button" data-action="move-model-row-down" type="button" aria-label="${t("modelPicker.moveDown")}">↓</button>
    <button class="danger-link remove-row-button" data-action="${h(removeAction)}" type="button" aria-label="${t("modelPicker.remove")}">×</button>
  </div>`;
}

function renderModelRoster(name: string, label: string, hint: string, values: string[], models: string[]): string {
  return `<div class="model-roster-field">
    <div class="field-heading"><span>${h(label)}</span><small>${h(hint)}</small></div>
    <div class="model-roster" data-name="${h(name)}">${values.map((model) => renderModelRosterRow(name, model, models)).join("")}</div>
    <button class="secondary compact add-row-button" data-action="add-model-roster-row" data-name="${h(name)}" type="button">${t("modelPicker.add")}</button>
  </div>`;
}

function renderAgentPreset(preset: ReturnType<typeof resolveAgentPreset>): string {
  const title = t(`agents.preset.${preset.id}`);
  const main = preset.mainModel ?? t("agents.currentMainModel");
  const vision = preset.visionModel ?? t("agents.modelNotFound");
  return `<button class="agent-preset-card" data-action="apply-agent-preset" data-preset="${preset.id}" type="button" ${!preset.available || isBusy("agent-preset") ? "disabled" : ""}>
    <strong>${h(title)}</strong>
    <span>${h(main)}</span>
    <small>Vision: ${h(vision)}</small>
  </button>`;
}

function renderMediaTestButton(kind: "image" | "video" | "document" | "imageGeneration", label: string): string {
  return `<button class="secondary" data-action="test-agent-media" data-kind="${kind}" type="button" ${isBusy("agent-media-test") ? "disabled" : ""}>${h(label)}</button>`;
}

function renderAgentTestResult(): string {
  const result = state.agentMediaTest;
  if (!result) return "";
  const source = result.sourcePath?.split(/[\\/]/).at(-1);
  return `<article class="agent-test-result" aria-live="polite">
    ${result.previewDataUrl ? `<img src="${h(result.previewDataUrl)}" alt="${t("agents.testPreviewAlt")}" />` : ""}
    <div><strong>${h(result.model)}</strong><small>${source ? `${h(source)} · ` : ""}${formatNumber(result.durationMs)} ms</small><p>${h(result.summary)}</p></div>
  </article>`;
}

function pluginStatusLabel(): string {
  const plugin = state.codexPluginStatus;
  if (!plugin) return t("agents.pluginChecking");
  if (!plugin.installed) return t("agents.pluginNotInstalled");
  if (!plugin.enabled) return t("agents.pluginDisabled");
  if (!plugin.mcpHealthy) return t("agents.pluginMcpUnavailable");
  if (!plugin.gatewayConnected) return t("agents.pluginGatewayMissing");
  if (!state.status?.running) return t("agents.pluginGatewayStopped");
  if (!plugin.gatewayReachable) return t("agents.pluginGatewayUnreachable");
  return t("agents.pluginConnected");
}

export function renderProjects(): string {
  const project = state.project;
  const plan = state.syncPlan;
  return `
    <div class="project-layout">
      <section class="project-intro">
        <h2>${t("projects.title")}</h2>
        <p>${t("projects.intro")}</p>
        <button class="primary" data-action="pick-project" type="button">${t("projects.register")}</button>
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
        ` : `<div class="project-empty"><div class="scan-symbol"><span></span></div><strong>${t("projects.empty")}</strong><p>${t("projects.emptyHint")}</p></div>`}
      </section>
      ${plan ? `<section class="panel sync-plan-panel"><header><div><h3>${t("projects.planCount", { n: plan.actions.length })}</h3></div><span class="chip ready">${t("projects.planSafe")}</span></header><div class="plan-flow">${plan.actions.map((action) => `<div><span class="plan-kind">${h(action.category)}</span><p><strong>${h(action.summary)}</strong><small>${h(action.source)} → ${h(action.target)}</small></p><em class="${action.compatibility}">${h(action.compatibility)}</em></div>`).join("") || `<div class="empty-inline">${t("projects.planEmpty")}</div>`}</div>${plan.warnings.length ? `<div class="warning-box">${plan.warnings.map((warning) => `<p>${h(warning)}</p>`).join("")}</div>` : ""}<p class="plan-note">${t("projects.planNote")}</p></section>` : ""}
      <section class="panel profile-convert-panel">
        <header><div><h3>${t("profiles.title")}</h3></div><button class="secondary compact" data-action="convert-hermes-profiles" type="button" ${isBusy("hermes-profiles") || !state.hermesProfiles.length ? "disabled" : ""}>${isBusy("hermes-profiles") ? t("profiles.converting") : t("profiles.convertAll")}</button></header>
        <p class="profile-help">${t("profiles.help")}</p>
        ${state.hermesProfiles.length ? `<div class="profile-list">${state.hermesProfiles.map((profile) => `<div class="profile-row"><div class="profile-monogram">${h(profile.name.slice(0, 2).toUpperCase())}</div><div class="profile-label"><strong>${h(profile.displayName ?? profile.name)}</strong><code>${h(profile.name)}</code></div><small>${h(compactProfileDescription(profile.description))}</small></div>`).join("")}</div>` : `<div class="empty-inline">${t("profiles.empty")}</div>`}
      </section>
    </div>`;
}

export function compactProfileDescription(description: string): string {
  const single = description.split(/\s+/).join(" ");
  return single.length > 120 ? `${single.slice(0, 120)}…` : single;
}

export function sourceTile(label: string, path: string | null, monogram: string): string {
  return `<article class="source-tile ${path ? "found" : "missing"}"><span>${h(monogram)}</span><div><strong>${h(label)}</strong><small>${path ? h(path.split(/[\\/]/).at(-1)) : t("projects.notFound")}</small></div></article>`;
}

export function renderClients(): string {
  const config = state.configuration!;
  const clients: Array<[keyof ExternalClientIntegrationInput, string, string]> = [
    ["claudeCode", "Claude Code", t("client.claudeCode")],
    ["claudeDesktop", "Claude Desktop", t("client.claudeDesktop")],
    ["opencode", "OpenCode", t("client.opencode")],
    ["grok", "Grok", t("client.grok")],
    ["pi", "Pi", t("client.pi")],
    ["hermes", "Hermes", t("client.hermes")],
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

export function renderSettings(): string {
  const config = state.configuration!;
  return `
    <form id="settings-form" class="settings-layout">
      <section class="panel settings-section">
        <header><div><h2>${t("settings.gateway")}</h2></div></header>
        <div class="form-grid two"><label>${t("settings.host")}<input name="host" value="${h(config.runtime.host)}" /></label><label>${t("settings.port")}<input name="port" type="number" min="1" max="65535" value="${config.runtime.port}" /></label></div>
        <label>${t("settings.shutdownTimeout")}<input name="shutdownTimeoutMs" type="number" min="100" max="300000" value="${config.runtime.shutdownTimeoutMs}" /></label>
        <div class="form-grid two"><label>${t("settings.memoryBudget")}<input name="memoryBudgetMb" type="number" min="64" max="65536" value="${Math.round(config.runtime.memoryBudgetBytes / 1024 ** 2)}" /></label><label>${t("settings.maxInflight")}<input name="maxInflightRequests" type="number" min="1" max="4096" value="${config.runtime.maxInflightRequests}" /></label></div>
        <label class="check-control"><input name="dynamicPortFallback" type="checkbox" ${config.runtime.dynamicPortFallback !== false ? "checked" : ""}/><span>${t("settings.dynamicPort")}</span></label>
        <label class="check-control"><input name="autoStart" type="checkbox" ${config.runtime.autoStart ? "checked" : ""}/><span>${t("settings.autoStart")}</span></label>
        <label class="check-control"><input name="autoSyncCatalog" type="checkbox" ${config.codex.autoSyncCatalog ? "checked" : ""}/><span>${t("settings.autoSyncCatalog")}</span></label>
        <label class="check-control"><input name="compatibilityLab" type="checkbox" ${config.catalog.compatibilityLab ? "checked" : ""}/><span>${t("settings.compatibilityLab")}</span></label>
        <label>${t("settings.selectedModels")}<small>${t("settings.onePerLine")}</small><textarea name="selectedModels" rows="5">${h(config.catalog.selectedModels.join("\n"))}</textarea></label>
        <label>${t("settings.modelPickerOrder")}<small>${t("settings.onePerLine")}</small><textarea name="modelPickerOrder" rows="5">${h(config.catalog.modelPickerOrder.join("\n"))}</textarea></label>
      </section>
      <section class="panel settings-section">
        <header><div><h2>${t("settings.keyPool")}</h2><p>${t("settings.keyPoolHint")}</p></div><button class="secondary compact" data-action="add-account-pool-row" type="button">${t("settings.addAccount")}</button></header>
        <div class="account-pool-list">${config.accountPool.accounts.map((account) => renderAccountPoolRow(account, config)).join("")}</div>
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

export function renderAccountPoolRow(
  account: GatewayConfiguration["accountPool"]["accounts"][number],
  config: GatewayConfiguration,
): string {
  const providers = config.providers.map((provider) => `<option value="${h(provider.id)}" ${provider.id === account.providerId ? "selected" : ""}>${h(provider.name)} (${h(provider.id)})</option>`).join("");
  return `<article class="account-pool-row" data-original-account-id="${h(account.id)}" data-original-provider-id="${h(account.providerId)}">
    <div class="form-grid four"><label>${t("settings.accountId")}<input data-account-field="id" value="${h(account.id)}" /></label><label>${t("settings.accountProvider")}<select data-account-field="providerId">${providers}</select></label><label>${t("settings.accountLabel")}<input data-account-field="label" value="${h(account.label)}" /></label><label>${t("settings.accountPriority")}<input data-account-field="priority" type="number" min="-32768" max="32767" value="${account.priority}" /></label></div>
    <div class="form-grid three"><label>${t("settings.credentialSource")}<select data-account-field="source"><option value="environment" ${account.credential.source === "environment" ? "selected" : ""}>Environment</option><option value="keychain" ${account.credential.source === "keychain" ? "selected" : ""}>Keychain</option><option value="oAuth" ${account.credential.source === "oAuth" ? "selected" : ""}>OAuth</option><option value="command" ${account.credential.source === "command" ? "selected" : "disabled"}>Command (${t("settings.preserved")})</option><option value="forward" ${account.credential.source === "forward" ? "selected" : ""}>Forward</option><option value="none" ${account.credential.source === "none" ? "selected" : ""}>None</option></select></label><label>${t("settings.credentialReference")}<input data-account-field="reference" value="${h(account.credential.reference ?? "")}" autocomplete="off" /></label><label>${t("settings.pauseUntil")}<input data-account-field="pauseUntilUnix" type="number" min="0" value="${account.pauseUntilUnix ?? ""}" /></label></div>
    <div class="form-grid four"><label class="check-control"><input data-account-field="enabled" type="checkbox" ${account.enabled ? "checked" : ""}/><span>${t("settings.accountEnabled")}</span></label><label class="check-control"><input data-account-field="paused" type="checkbox" ${account.paused ? "checked" : ""}/><span>${t("settings.accountPaused")}</span></label><label class="check-control"><input data-account-field="pinned" type="checkbox" ${account.pinned ? "checked" : ""}/><span>${t("settings.accountPinned")}</span></label><button class="danger-link" data-action="remove-account-pool-row" type="button">${t("route.remove")}</button></div>
  </article>`;
}

export function renderProviderEditor(): string {
  const provider = state.configuration?.providers.find((item) => item.id === state.editingProviderId);
  if (!provider) return "";
  const credential = provider.credential ?? {
    source: "none", reference: null, transport: "bearer", headerName: null, command: null,
  };
  return `<div class="drawer-scrim" data-action="close-provider-editor"><aside class="provider-drawer" role="dialog" aria-modal="true" aria-labelledby="provider-editor-title" data-stop-close>
    <header><div><h2 id="provider-editor-title">${h(provider.name)}</h2></div><button class="icon-button" data-action="close-provider-editor" type="button" aria-label="${t("drawer.close")}">×</button></header>
    <form id="provider-editor-form" class="drawer-form" data-provider-id="${h(provider.id)}" data-realtime-capable="${provider.capabilities?.realtime ? "true" : "false"}" data-has-credential-command="${credential.command ? "true" : "false"}">
      <input name="id" type="hidden" value="${h(provider.id)}" />
      <div class="form-grid two"><label>${t("drawer.displayName")}<input name="name" value="${h(provider.name)}" required /></label><label>${t("drawer.defaultModel")}${renderSearchableModelSelect({ name: "defaultModel", selected: provider.defaultModel, models: provider.models, allowEmpty: true })}</label></div>
      <label>${t("drawer.baseUrl")}<input name="baseUrl" type="url" value="${h(provider.baseUrl)}" required /></label>
      <div class="form-grid two"><label>${t("drawer.protocol")}<select name="protocol"><option value="responses" ${provider.protocol === "responses" ? "selected" : ""}>Responses</option><option value="chatCompletions" ${provider.protocol === "chatCompletions" ? "selected" : ""}>Chat Completions</option><option value="anthropicMessages" ${provider.protocol === "anthropicMessages" ? "selected" : ""}>Anthropic Messages</option><option value="geminiGenerateContent" ${provider.protocol === "geminiGenerateContent" ? "selected" : ""}>Gemini generateContent</option></select></label><label>${t("drawer.transport")}<select name="providerTransport"><option value="standard" ${provider.transport === "standard" || !provider.transport ? "selected" : ""}>Standard HTTP / SSE</option><option value="kiro" ${provider.transport === "kiro" ? "selected" : ""}>Kiro event-stream</option><option value="githubCopilot" ${provider.transport === "githubCopilot" ? "selected" : ""}>GitHub Copilot exchange</option></select></label></div>
      <fieldset class="drawer-field-group" data-provider-section="google" hidden>
        <legend>${t("drawer.googleSettings")}</legend>
        <label>${t("drawer.googleMode")}<select name="googleMode"><option value="aiStudio" ${provider.googleMode === "aiStudio" || !provider.googleMode ? "selected" : ""}>AI Studio</option><option value="vertex" ${provider.googleMode === "vertex" ? "selected" : ""}>Vertex</option><option value="cloudCodeAssist" ${provider.googleMode === "cloudCodeAssist" ? "selected" : ""}>Cloud Code Assist</option></select></label>
        <div class="form-grid two" data-google-project-fields><label>${t("drawer.project")}<input name="project" value="${h(provider.project ?? "")}" /></label><label>${t("drawer.location")}<input name="location" value="${h(provider.location ?? "")}" /></label></div>
      </fieldset>
      <fieldset class="drawer-field-group" data-provider-section="azure" hidden>
        <legend>${t("drawer.azureSettings")}</legend>
        <div class="form-grid two"><label>${t("drawer.azureDeployment")}<input name="azureDeployment" value="${h(provider.azureDeployment ?? "")}" /></label><label>${t("drawer.azureApiVersion")}<input name="azureApiVersion" value="${h(provider.azureApiVersion ?? "")}" placeholder="2025-04-01-preview" /></label></div>
      </fieldset>
      <fieldset class="drawer-field-group" data-provider-section="kiro" hidden>
        <legend>${t("drawer.kiroSettings")}</legend>
        <label>${t("drawer.kiroArn")} <small>${t("drawer.kiroArnHint")}</small><input name="kiroProfileArn" value="${h(provider.kiroProfileArn ?? "")}" /></label>
      </fieldset>
      <fieldset class="drawer-field-group" data-provider-section="responses" hidden>
        <legend>${t("drawer.responsesSettings")}</legend>
        <label>${t("drawer.responsesPath")}<input name="responsesPath" value="${h(provider.responsesPath ?? "")}" placeholder="/responses" /></label>
        <label class="check-control"><input name="statelessResponses" type="checkbox" ${provider.statelessResponses ? "checked" : ""}/><span>${t("drawer.stateless")}</span></label>
        <label class="check-control"><input name="requiresAdjacentResponsesToolResults" type="checkbox" ${provider.requiresAdjacentResponsesToolResults ? "checked" : ""}/><span>${t("drawer.adjacentToolResults")}</span></label>
      </fieldset>
      <fieldset class="drawer-field-group" data-provider-section="realtime" hidden>
        <legend>${t("drawer.realtimeSettings")}</legend>
        <label>${t("drawer.realtimeWs")}<input name="realtimeWsBaseUrl" type="url" value="${h(provider.realtimeWsBaseUrl ?? "")}" /></label>
      </fieldset>
      <fieldset class="drawer-field-group">
        <legend>${t("drawer.requestSettings")}</legend>
        <div class="form-grid two"><label>${t("drawer.requestRetries")}<input name="requestRetries" type="number" min="0" max="10" value="${provider.limits?.requestRetries ?? 2}" /></label><label>${t("drawer.streamRetries")}<input name="streamRetries" type="number" min="0" max="10" value="${provider.limits?.streamRetries ?? 2}" /></label></div>
        <label class="check-control"><input name="stripModelBracketSuffix" type="checkbox" ${provider.stripModelBracketSuffix ? "checked" : ""}/><span>${t("drawer.stripSuffix")}</span></label>
      </fieldset>
      <fieldset class="drawer-field-group">
        <legend>${t("drawer.credentialSettings")}</legend>
        <div class="form-grid two"><label>${t("drawer.credential")}<select name="credentialSource"><option value="none" ${credential.source === "none" ? "selected" : ""}>${t("cred.none")}</option><option value="environment" ${credential.source === "environment" ? "selected" : ""}>${t("cred.env")}</option><option value="keychain" ${credential.source === "keychain" ? "selected" : ""}>${t("cred.keychain")}</option><option value="oAuth" ${credential.source === "oAuth" ? "selected" : ""}>${t("cred.login")}</option><option value="command" ${credential.source === "command" ? "selected" : ""} ${credential.command ? "" : "disabled"}>${t("cred.command")}</option><option value="forward" ${credential.source === "forward" ? "selected" : ""}>${t("cred.codex")}</option></select></label><label data-credential-transport-field>${t("drawer.credentialTransport")}<select name="credentialTransport"><option value="bearer" ${credential.transport === "bearer" ? "selected" : ""}>Bearer</option><option value="xApiKey" ${credential.transport === "xApiKey" ? "selected" : ""}>x-api-key</option><option value="customHeader" ${credential.transport === "customHeader" ? "selected" : ""}>Custom header</option></select></label></div>
        <label data-credential-reference-field><span data-credential-reference-label>${t("drawer.credentialRef")}</span><small data-credential-reference-hint>${t("drawer.credentialRefHint")}</small><input name="credentialReference" value="${h(credential.reference ?? provider.apiKeyEnv ?? "")}" autocomplete="off" /></label>
        <label data-credential-header-field hidden>${t("drawer.credentialHeaderName")}<small>${t("drawer.credentialHeaderNameHint")}</small><input name="credentialHeaderName" value="${h(credential.headerName ?? "")}" placeholder="X-API-Key" pattern="[A-Za-z0-9!#$%&'*+.^_|~-]+" autocomplete="off" /></label>
        <p class="drawer-managed-note" data-credential-command-note hidden>${t("drawer.credentialCommandManaged")}</p>
      </fieldset>
      <div class="toggle-pair"><label class="check-control"><input name="enabled" type="checkbox" ${provider.enabled ? "checked" : ""}/><span>${t("drawer.enable")}</span></label><label class="check-control"><input name="allowPrivateNetwork" type="checkbox" ${provider.allowPrivateNetwork ? "checked" : ""}/><span>${t("drawer.allowPrivate")}</span></label></div>
      <div class="drawer-actions"><button class="danger-link" data-action="remove-provider" data-provider-id="${h(provider.id)}" type="button">${t("drawer.remove")}</button><button class="primary" type="submit">${t("drawer.save")}</button></div>
    </form>
  </aside></div>`;
}

export function renderCodexDisconnectConfirmation(): string {
  return `<div class="confirmation-scrim" data-action="cancel-restore-codex">
    <section class="confirmation-dialog" role="dialog" aria-modal="true" aria-labelledby="codex-disconnect-title" data-stop-confirmation-close>
      <h2 id="codex-disconnect-title">${t("confirm.restoreCodexTitle")}</h2>
      <p>${t("confirm.restoreCodex")}</p>
      <div class="confirmation-actions">
        <button class="secondary" data-action="cancel-restore-codex" type="button">${t("confirm.cancel")}</button>
        <button class="danger-button" data-action="confirm-restore-codex" type="button">${t("confirm.disconnect")}</button>
      </div>
    </section>
  </div>`;
}

export function syncProviderEditorVisibility(form: HTMLFormElement): void {
  const value = (name: string) => form.querySelector<HTMLInputElement | HTMLSelectElement>(`[name="${name}"]`)?.value.trim() ?? "";
  const toggleSection = (name: string, visible: boolean) => {
    const section = form.querySelector<HTMLElement>(`[data-provider-section="${name}"]`);
    if (section) section.hidden = !visible;
  };
  const protocol = value("protocol");
  const transport = value("providerTransport");
  const baseUrl = value("baseUrl").toLowerCase();
  const providerId = (form.dataset.providerId ?? "").toLowerCase();
  const azure = providerId.includes("azure")
    || baseUrl.includes("azure")
    || Boolean(value("azureDeployment") || value("azureApiVersion"));
  toggleSection("google", protocol === "geminiGenerateContent");
  toggleSection("azure", azure);
  toggleSection("kiro", transport === "kiro");
  toggleSection("responses", protocol === "responses");
  toggleSection("realtime", form.dataset.realtimeCapable === "true" || Boolean(value("realtimeWsBaseUrl")));

  const googleProjectFields = form.querySelector<HTMLElement>("[data-google-project-fields]");
  if (googleProjectFields) googleProjectFields.hidden = value("googleMode") === "aiStudio";

  const source = value("credentialSource");
  const credentialTransport = value("credentialTransport");
  const hasCommand = form.dataset.hasCredentialCommand === "true";
  const usesManagedCommand = hasCommand && (source === "command" || source === "oAuth");
  const noCredentialFields = source === "none" || source === "forward";
  const transportField = form.querySelector<HTMLElement>("[data-credential-transport-field]");
  const referenceField = form.querySelector<HTMLElement>("[data-credential-reference-field]");
  const headerField = form.querySelector<HTMLElement>("[data-credential-header-field]");
  const commandNote = form.querySelector<HTMLElement>("[data-credential-command-note]");
  if (transportField) transportField.hidden = noCredentialFields;
  if (referenceField) referenceField.hidden = noCredentialFields || source === "command" || usesManagedCommand;
  if (headerField) headerField.hidden = noCredentialFields || credentialTransport !== "customHeader";
  if (commandNote) commandNote.hidden = !usesManagedCommand;

  const referenceInput = form.querySelector<HTMLInputElement>('[name="credentialReference"]');
  const headerInput = form.querySelector<HTMLInputElement>('[name="credentialHeaderName"]');
  if (referenceInput) referenceInput.required = Boolean(referenceField && !referenceField.hidden);
  if (headerInput) headerInput.required = Boolean(headerField && !headerField.hidden);
  const label = form.querySelector<HTMLElement>("[data-credential-reference-label]");
  const hint = form.querySelector<HTMLElement>("[data-credential-reference-hint]");
  const credentialCopy = source === "environment"
    ? { label: "drawer.credentialRef.environment", hint: "drawer.credentialRefHint.environment", placeholder: "OPENAI_API_KEY" }
    : source === "keychain"
      ? { label: "drawer.credentialRef.keychain", hint: "drawer.credentialRefHint.keychain", placeholder: "service/account" }
      : { label: "drawer.credentialRef.oauth", hint: "drawer.credentialRefHint.oauth", placeholder: "provider-or-broker-id" };
  if (label) label.textContent = t(credentialCopy.label);
  if (hint) hint.textContent = t(credentialCopy.hint);
  if (referenceInput) {
    referenceInput.placeholder = credentialCopy.placeholder;
    if (source === "environment") referenceInput.pattern = "[A-Za-z_][A-Za-z0-9_]*";
    else referenceInput.removeAttribute("pattern");
  }
}

export function hydratePostRenderValues(): void {
  const advanced = document.querySelector<HTMLTextAreaElement>("#advanced-json");
  if (advanced && state.configuration) advanced.value = JSON.stringify(state.configuration, null, 2);
  const providerForm = document.querySelector<HTMLFormElement>("#provider-editor-form");
  if (providerForm) syncProviderEditorVisibility(providerForm);
}
