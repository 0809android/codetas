import type {
  AgentMediaTestResult,
  CodexPluginStatus,
  GatewayConfiguration,
  GatewayDiagnosticReport,
  GatewayServiceStatus,
  GatewayStatus,
  HermesProfile,
  HermesEditableFile,
  HermesSyncDirection,
  HermesSyncInventory,
  HermesSyncPreview,
  MaintenanceJob,
  MaintenancePlan,
  MaintenancePreviewInput,
  MaintenanceReport,
  ObservabilityBreakdown,
  ObservabilityCleanupPreview,
  ObservabilitySummary,
  OAuthProviderDescriptor,
  CompatibilityLabReport,
  DebugScope,
  ObservationEvent,
  RouteDryRunReport,
  ObservabilityTrashEntry,
  ProjectInspection,
  ProviderPreset,
  SyncPlan,
} from "@codetas/core";

export type View = "overview" | "maintenance" | "bots" | "providers" | "routing" | "agents" | "projects" | "clients" | "settings";
export type BotMessage = { role: "user" | "assistant"; content: string };
export type Bot = {
  id: string;
  name: string;
  model: string | null;
  instructions: string;
  messages: BotMessage[];
  createdAt: number;
  updatedAt: number;
  collapsed: boolean;
};
export type Notice = { tone: "success" | "error" | "info"; text: string };
export type LocalCliStatus = {
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
export type LocalCliScanReport = { deep: boolean; clients: LocalCliStatus[] };
export type DirectApiTarget = { providerId: string; name: string; hint: string };

export interface AppState {
  view: View;
  bots: Bot[];
  botInputs: Record<string, string>;
  botSending: Set<string>;
  botAborts: Record<string, AbortController>;
  status: GatewayStatus | null;
  codexPluginStatus: CodexPluginStatus | null;
  agentMediaTest: AgentMediaTestResult | null;
  configuration: GatewayConfiguration | null;
  presets: ProviderPreset[];
  diagnostics: GatewayDiagnosticReport | null;
  maintenance: MaintenanceReport | null;
  maintenancePlan: MaintenancePlan | null;
  maintenanceJobs: MaintenanceJob[];
  maintenancePreviewInput: MaintenancePreviewInput;
  observability: ObservabilitySummary | null;
  breakdown: ObservabilityBreakdown | null;
  debugScope: DebugScope | null;
  debugEvents: ObservationEvent[];
  cleanupPreview: ObservabilityCleanupPreview | null;
  trashEntries: ObservabilityTrashEntry[];
  service: GatewayServiceStatus | null;
  localClis: LocalCliScanReport | null;
  directApis: DirectApiTarget[];
  oauthProviders: OAuthProviderDescriptor[];
  compatibilityLab: CompatibilityLabReport | null;
  routeDryRuns: RouteDryRunReport[];
  hermesProfiles: HermesProfile[];
  hermesSyncInventory: HermesSyncInventory | null;
  hermesSyncPreview: HermesSyncPreview | null;
  hermesSyncDirection: HermesSyncDirection;
  hermesProfileTab: string;
  hermesEditableFiles: HermesEditableFile[];
  hermesFileDrafts: Record<string, string>;
  contextFileDrafts: Record<string, string>;
  providerTestFailed: Set<string>;
  project: ProjectInspection | null;
  syncPlan: SyncPlan | null;
  editingProviderId: string | null;
  confirmingCodexDisconnect: boolean;
  modelSearchQuery: string;
  busy: Set<string>;
  notice: Notice | null;
}

export const state: AppState = {
  view: "overview",
  bots: [],
  botInputs: {},
  botSending: new Set(),
  botAborts: {},
  status: null,
  codexPluginStatus: null,
  agentMediaTest: null,
  configuration: null,
  presets: [],
  diagnostics: null,
  maintenance: null,
  maintenancePlan: null,
  maintenanceJobs: [],
  maintenancePreviewInput: {
    logRetentionDays: 30,
    compactSqlite: true,
    repairOrphanPins: true,
    disableMcpServers: [],
    deleteStorageIds: [],
    trashOversizedSessions: true,
  },
  observability: null,
  breakdown: null,
  debugScope: null,
  debugEvents: [],
  cleanupPreview: null,
  trashEntries: [],
  service: null,
  localClis: null,
  directApis: [],
  oauthProviders: [],
  compatibilityLab: null,
  routeDryRuns: [],
  hermesProfiles: [],
  hermesSyncInventory: null,
  hermesSyncPreview: null,
  hermesSyncDirection: "import",
  hermesProfileTab: "all",
  hermesEditableFiles: [],
  hermesFileDrafts: {},
  contextFileDrafts: {},
  providerTestFailed: new Set(),
  project: null,
  syncPlan: null,
  editingProviderId: null,
  confirmingCodexDisconnect: false,
  modelSearchQuery: "",
  busy: new Set(),
  notice: null,
};

export const navigation: Array<{ id: View; key: string }> = [
  { id: "overview", key: "nav.overview" },
  { id: "maintenance", key: "nav.maintenance" },
  { id: "bots", key: "nav.bots" },
  { id: "providers", key: "nav.providers" },
  { id: "agents", key: "nav.agents" },
  { id: "projects", key: "nav.projects" },
  { id: "clients", key: "nav.clients" },
  { id: "settings", key: "nav.settings" },
];

const BOTS_KEY = "codetas.bots.v2";
const LEGACY_CHAT_SESSIONS_KEY = "codetas.chatSessions.v1";

function isBotMessage(value: unknown): value is BotMessage {
  if (!value || typeof value !== "object") return false;
  const item = value as Record<string, unknown>;
  return (item.role === "user" || item.role === "assistant") && typeof item.content === "string";
}

function isBot(value: unknown): value is Bot {
  if (!value || typeof value !== "object") return false;
  const item = value as Record<string, unknown>;
  return typeof item.id === "string"
    && typeof item.name === "string"
    && (item.model === null || typeof item.model === "string")
    && typeof item.instructions === "string"
    && Array.isArray(item.messages)
    && item.messages.every(isBotMessage)
    && typeof item.createdAt === "number"
    && typeof item.updatedAt === "number"
    && typeof item.collapsed === "boolean";
}

function botsFromLegacySessions(raw: string | null): Bot[] {
  if (!raw) return [];
  try {
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed.flatMap((item) => {
      if (!item || typeof item !== "object") return [];
      const session = item as Record<string, unknown>;
      if (typeof session.id !== "string" || typeof session.title !== "string" || !Array.isArray(session.messages)) return [];
      const now = Date.now();
      const bot: Bot = {
        id: session.id,
        name: session.title,
        model: null,
        instructions: "",
        messages: session.messages.filter(isBotMessage),
        createdAt: typeof session.createdAt === "number" ? session.createdAt : now,
        updatedAt: typeof session.updatedAt === "number" ? session.updatedAt : now,
        collapsed: false,
      };
      return [bot];
    });
  } catch {
    return [];
  }
}

export function loadBots(): Bot[] {
  try {
    const raw = localStorage.getItem(BOTS_KEY);
    if (raw !== null) {
      const parsed: unknown = JSON.parse(raw);
      return Array.isArray(parsed) ? parsed.filter(isBot) : [];
    }
    const legacy = botsFromLegacySessions(localStorage.getItem(LEGACY_CHAT_SESSIONS_KEY));
    if (legacy.length) localStorage.setItem(BOTS_KEY, JSON.stringify(legacy));
    return legacy;
  } catch {
    return [];
  }
}

export function saveBots(bots: Bot[]): void {
  try {
    localStorage.setItem(BOTS_KEY, JSON.stringify(bots));
  } catch {
    // Bot sessions are a convenience feature and must never break the UI.
  }
}


export function botExists(botId: string): boolean {
  return state.bots.some((item) => item.id === botId);
}

export function createBotRecord(name: string, model: string | null): Bot {
  const now = Date.now();
  return {
    id: globalThis.crypto?.randomUUID?.() ?? `${now}-${Math.random().toString(16).slice(2)}`,
    name,
    model,
    instructions: "",
    messages: [],
    createdAt: now,
    updatedAt: now,
    collapsed: false,
  };
}

export function deleteBot(botId: string): void {
  state.botAborts[botId]?.abort();
  state.bots = state.bots.filter((item) => item.id !== botId);
  delete state.botInputs[botId];
  delete state.botAborts[botId];
  state.botSending.delete(botId);
  saveBots(state.bots);
}

export function abortBot(botId: string): void {
  state.botAborts[botId]?.abort();
}

export function persistActiveBot(botId: string): void {
  if (botExists(botId)) saveBots(state.bots);
}

export function beginBotTurn(bot: Bot, message: string): void {
  state.botInputs[bot.id] = "";
  bot.messages.push({ role: "user", content: message });
  bot.messages.push({ role: "assistant", content: "" });
  bot.updatedAt = Date.now();
  state.botSending.add(bot.id);
  saveBots(state.bots);
}

export function appendBotDelta(bot: Bot, delta: string): void {
  if (!botExists(bot.id)) return;
  const streaming = bot.messages[bot.messages.length - 1];
  if (streaming && streaming.role === "assistant") streaming.content += delta;
}

export function finishBotReply(bot: Bot, reply: string, fallback: string): void {
  if (!botExists(bot.id)) return;
  const target = bot.messages[bot.messages.length - 1];
  if (target && target.role === "assistant") {
    if (!target.content.trim()) target.content = reply || fallback;
    else if (!reply.startsWith(target.content)) target.content = reply || target.content;
  }
  bot.updatedAt = Date.now();
}

export function finishBotError(bot: Bot, detail: string, aborted: boolean): void {
  if (!botExists(bot.id)) return;
  const target = bot.messages[bot.messages.length - 1];
  if (target && target.role === "assistant" && !target.content.trim()) {
    target.content = detail;
  } else if (!aborted) {
    bot.messages.push({ role: "assistant", content: detail });
  }
  bot.updatedAt = Date.now();
}

export function endBotTurn(botId: string): void {
  delete state.botAborts[botId];
  state.botSending.delete(botId);
  persistActiveBot(botId);
}

export function copyTextFromBotMessage(target: HTMLElement): string {
  return target.closest(".chat-message")?.querySelector("p")?.textContent ?? "";
}
export function isBusy(key: string): boolean {
  return state.busy.has(key);
}
