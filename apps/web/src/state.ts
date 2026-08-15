import type {
  GatewayConfiguration,
  GatewayDiagnosticReport,
  GatewayServiceStatus,
  GatewayStatus,
  HermesProfile,
  MaintenanceJob,
  MaintenancePlan,
  MaintenancePreviewInput,
  MaintenanceReport,
  ObservabilityBreakdown,
  ObservabilityCleanupPreview,
  ObservabilitySummary,
  ObservabilityTrashEntry,
  ProjectInspection,
  ProviderPreset,
  SyncPlan,
} from "@codetas/core";

export type View = "overview" | "maintenance" | "providers" | "routing" | "agents" | "projects" | "clients" | "settings";
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
  status: GatewayStatus | null;
  configuration: GatewayConfiguration | null;
  presets: ProviderPreset[];
  diagnostics: GatewayDiagnosticReport | null;
  maintenance: MaintenanceReport | null;
  maintenancePlan: MaintenancePlan | null;
  maintenanceJobs: MaintenanceJob[];
  maintenancePreviewInput: MaintenancePreviewInput;
  observability: ObservabilitySummary | null;
  breakdown: ObservabilityBreakdown | null;
  cleanupPreview: ObservabilityCleanupPreview | null;
  trashEntries: ObservabilityTrashEntry[];
  service: GatewayServiceStatus | null;
  localClis: LocalCliScanReport | null;
  directApis: DirectApiTarget[];
  hermesProfiles: HermesProfile[];
  providerTestFailed: Set<string>;
  project: ProjectInspection | null;
  syncPlan: SyncPlan | null;
  editingProviderId: string | null;
  confirmingCodexDisconnect: boolean;
  busy: Set<string>;
  notice: Notice | null;
}

export const state: AppState = {
  view: "overview",
  status: null,
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
  },
  observability: null,
  breakdown: null,
  cleanupPreview: null,
  trashEntries: [],
  service: null,
  localClis: null,
  directApis: [],
  hermesProfiles: [],
  providerTestFailed: new Set(),
  project: null,
  syncPlan: null,
  editingProviderId: null,
  confirmingCodexDisconnect: false,
  busy: new Set(),
  notice: null,
};

export const navigation: Array<{ id: View; key: string }> = [
  { id: "overview", key: "nav.overview" },
  { id: "maintenance", key: "nav.maintenance" },
  { id: "providers", key: "nav.providers" },
  { id: "routing", key: "nav.routing" },
  { id: "agents", key: "nav.agents" },
  { id: "projects", key: "nav.projects" },
  { id: "clients", key: "nav.clients" },
  { id: "settings", key: "nav.settings" },
];

export function isBusy(key: string): boolean {
  return state.busy.has(key);
}
