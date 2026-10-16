import type {
  AgentMediaTestResult,
  CodexPluginStatus,
  GatewayConfiguration,
  GatewayDiagnosticReport,
  GatewayServiceStatus,
  GatewayStatus,
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
  ProviderPreset,
  ProviderConnectionReport,
} from "@codetas/core";

export type View = "connections" | "models" | "status" | "settings";
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
  diagnosedAt: number | null;
  providerReports: Record<string, { report: ProviderConnectionReport; checkedAt: number }>;
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
  cacheHealth: import("@codetas/core").CacheHealthSnapshot | null;
  breakdown: ObservabilityBreakdown | null;
  debugScope: DebugScope | null;
  debugEvents: ObservationEvent[];
  recentEvents: ObservationEvent[];
  recentEventsError: string | null;
  cleanupPreview: ObservabilityCleanupPreview | null;
  trashEntries: ObservabilityTrashEntry[];
  service: GatewayServiceStatus | null;
  localClis: LocalCliScanReport | null;
  directApis: DirectApiTarget[];
  oauthProviders: OAuthProviderDescriptor[];
  compatibilityLab: CompatibilityLabReport | null;
  routeDryRuns: RouteDryRunReport[];
  providerTestFailed: Set<string>;
  editingProviderId: string | null;
  confirmingCodexDisconnect: boolean;
  modelSearchQuery: string;
  busy: Set<string>;
  notice: Notice | null;
}

export const state: AppState = {
  view: "connections",
  diagnosedAt: null,
  providerReports: {},
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
  cacheHealth: null,
  breakdown: null,
  debugScope: null,
  debugEvents: [],
  recentEvents: [],
  recentEventsError: null,
  cleanupPreview: null,
  trashEntries: [],
  service: null,
  localClis: null,
  directApis: [],
  oauthProviders: [],
  compatibilityLab: null,
  routeDryRuns: [],
  providerTestFailed: new Set(),
  editingProviderId: null,
  confirmingCodexDisconnect: false,
  modelSearchQuery: "",
  busy: new Set(),
  notice: null,
};

export const navigation: Array<{ id: View; key: string }> = [
  { id: "connections", key: "nav.connections" },
  { id: "models", key: "nav.models" },
  { id: "status", key: "nav.status" },
  { id: "settings", key: "nav.settings" },
];

export function normalizeView(value: string): View {
  if (value === "providers" || value === "clients") return "connections";
  if (value === "agents" || value === "routing") return "models";
  if (value === "overview") return "status";
  if (value === "maintenance" || value === "projects") return "settings";
  return navigation.some((entry) => entry.id === value) ? value as View : "connections";
}

export function isBusy(key: string): boolean {
  return state.busy.has(key);
}
