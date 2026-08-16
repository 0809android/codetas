export type Compatibility = "ready" | "review" | "missing";

export interface ProjectInspection {
  id: string;
  name: string;
  path: string;
  contextFile: string | null;
  agentsFile: string | null;
  skillsDirectory: string | null;
  skillsCount: number;
  mcpFile: string | null;
  codexConfigFile: string | null;
  warnings: string[];
  inspectedAt: string;
}

export interface SyncPreference {
  context: boolean;
  skills: boolean;
  mcp: boolean;
}

export type SyncCategory = "context" | "skills" | "mcp";

export interface SyncAction {
  id: string;
  category: SyncCategory;
  source: string;
  target: string;
  summary: string;
  compatibility: Compatibility;
  readOnly: boolean;
}

export interface SyncPlan {
  version: 1;
  projectId: string;
  provider: "hermes";
  createdAt: string;
  actions: SyncAction[];
  warnings: string[];
  sourceMutation: false;
}

export type ProviderProtocol =
  | "responses"
  | "chatCompletions"
  | "anthropicMessages"
  | "geminiGenerateContent";
export type GoogleMode = "aiStudio" | "vertex" | "cloudCodeAssist";
export type ProviderTransport = "standard" | "kiro" | "githubCopilot";

export type CredentialSource = "none" | "environment" | "keychain" | "oAuth" | "command" | "forward";
export type CredentialTransport = "bearer" | "xApiKey" | "customHeader";

export interface CredentialCommand {
  program: string;
  args: string[];
  cwd: string | null;
  timeoutMs: number;
  refreshIntervalMs: number;
}

export interface ProviderCredential {
  source: CredentialSource;
  reference: string | null;
  transport: CredentialTransport;
  headerName: string | null;
  command: CredentialCommand | null;
}

export interface ProviderCapabilities {
  streaming: boolean;
  tools: boolean;
  parallelTools: boolean;
  vision: boolean;
  audio: boolean;
  reasoning: boolean;
  webSearch: boolean;
  imageGeneration: boolean;
  videoGeneration: boolean;
  realtime: boolean;
  websockets: boolean;
  statefulResponses: boolean;
  structuredOutput: boolean;
  serviceTier: boolean;
  customTools: boolean;
  toolSearch: boolean;
  mcpNamespaces: boolean;
  providerMetadata: boolean;
}

export interface ProviderLimits {
  connectTimeoutMs: number;
  requestTimeoutMs: number;
  streamIdleTimeoutMs: number;
  requestRetries: number;
  streamRetries: number;
  retryOn429: boolean;
  max429Retries: number;
  requestPacingMs: number;
  emptyCompletionRetries: number;
  maxRequestBytes: number;
  maxResponseBytes: number;
}

export interface ModelDiscoverySettings {
  enabled: boolean;
  path: string;
  maxModels: number;
}

export interface ProviderDefinition {
  id: string;
  name: string;
  baseUrl: string;
  protocol: ProviderProtocol;
  transport?: ProviderTransport;
  googleMode?: GoogleMode;
  project?: string | null;
  location?: string | null;
  azureDeployment?: string | null;
  azureApiVersion?: string | null;
  kiroProfileArn?: string | null;
  modelProtocols?: Record<string, ProviderProtocol>;
  modelWireIds?: Record<string, string>;
  imageGenerationModels?: string[];
  modelReasoningModes?: Record<string, string>;
  stripModelBracketSuffix?: boolean;
  responsesPath?: string | null;
  realtimeWsBaseUrl?: string | null;
  statelessResponses?: boolean;
  requiresAdjacentResponsesToolResults?: boolean;
  responsesSnapshotRepair?: boolean;
  repairInvalidResponseItemIds?: boolean;
  apiKeyEnv: string | null;
  credentialSource?: CredentialSource;
  defaultModel: string | null;
  models: string[];
  modelContextWindows?: Record<string, number>;
  modelMaxInputTokens?: Record<string, number>;
  modelMaxOutputTokens?: Record<string, number>;
  modelInputModalities?: Record<string, Array<"text" | "image" | "audio" | "video">>;
  modelReasoningEfforts?: Record<string, string[]>;
  modelDefaultReasoningEfforts?: Record<string, string>;
  enabled: boolean;
  allowPrivateNetwork: boolean;
  credential?: ProviderCredential;
  headers?: Record<string, string>;
  envHeaders?: Record<string, string>;
  queryParams?: Record<string, string>;
  reasoningEffortMap?: Record<string, string>;
  modelReasoningEffortMap?: Record<string, Record<string, string>>;
  noReasoningModels?: string[];
  noTemperatureModels?: string[];
  noTopPModels?: string[];
  noPenaltyModels?: string[];
  noStructuredOutputModels?: string[];
  serviceTierModels?: string[];
  chatServiceTier?: string | null;
  anthropicEofToleranceModels?: string[];
  terminalContinuationGuardModels?: string[];
  emptyCompletionRetryModels?: string[];
  autoToolChoiceOnlyModels?: string[];
  preserveReasoningContentModels?: string[];
  requiresReasoningPlaceholderModels?: string[];
  reasoningSplitModels?: string[];
  thinkingToggleModels?: string[];
  thinkingBudgetModels?: string[];
  escapeBuiltinToolNames?: boolean;
  responseItemIdRepair?: {
    message: string[];
    reasoning: string[];
    repairMissingTerminalIds: boolean;
  };
  parallelToolCalls?: boolean | null;
  promptCacheKey?: boolean;
  capabilities?: ProviderCapabilities;
  limits?: ProviderLimits;
  discovery?: ModelDiscoverySettings;
}

export interface ProviderPreset {
  id: string;
  name: string;
  description: string;
  baseUrl: string;
  protocol: ProviderProtocol;
  apiKeyEnv: string | null;
  credentialSource: CredentialSource;
  credentialTransport: CredentialTransport;
  allowPrivateNetwork: boolean;
  discovery: boolean;
  requiresCustomUrl: boolean;
  capabilities: ProviderCapabilities;
}

export interface ProviderConnectionReport {
  providerId: string;
  reachable: boolean;
  authenticated: boolean;
  status: number | null;
  latencyMs: number;
  modelCount: number;
  message: string;
}

export interface GatewayDiagnosticReport {
  checks: Array<{
    id: string;
    level: "pass" | "warning" | "error";
    summary: string;
    remediation: string | null;
  }>;
  passed: number;
  warnings: number;
  errors: number;
}

export type MaintenanceSeverity = "healthy" | "attention" | "critical" | "unknown";
export type MaintenanceCategory =
  | "storage"
  | "database"
  | "tasks"
  | "processes"
  | "logs"
  | "mcp"
  | "git"
  | "system"
  | "configuration";

export interface MaintenanceFinding {
  id: string;
  category: MaintenanceCategory;
  severity: MaintenanceSeverity;
  title: string;
  summary: string;
  technicalDetails: string[];
  affectedPaths: string[];
  affectedThreadIds: string[];
  detectedAtMs: number;
  estimatedReclaimableBytes: number | null;
  requiresCodexShutdown: boolean;
  repairActionId: string | null;
  reversible: boolean;
  confidence: "high" | "medium" | "low";
}

export interface MaintenanceStorageEntry {
  id: string;
  label: string;
  path: string;
  bytes: number;
  fileCount: number | null;
  directoryCount: number | null;
  topLevelDirectoryCount: number | null;
  recent24hModifiedBytes: number | null;
  status: MaintenanceSeverity;
  scanTruncated: boolean;
}

export interface MaintenanceProcessInfo {
  pid: number;
  parentPid: number | null;
  parentName: string | null;
  name: string;
  startedAt: string | null;
  terminal: string | null;
  cpuPercent: number | null;
  memoryBytes: number | null;
}

export interface MaintenanceFileLock {
  path: string;
  threadId: string | null;
  threadName: string | null;
  preview: string | null;
  threadStatus: string | null;
  updatedAtMs: number | null;
  cwd: string | null;
  process: MaintenanceProcessInfo;
}

export interface MaintenanceSqliteHealth {
  path: string;
  available: boolean;
  physicalBytes: number;
  pageSize: number | null;
  pageCount: number | null;
  freelistCount: number | null;
  reclaimableBytes: number | null;
  estimatedLiveBytes: number | null;
  journalMode: string | null;
  queryDurationMs: number | null;
  openBy: MaintenanceProcessInfo[];
  error: string | null;
}

export interface MaintenanceMcpStatus {
  name: string;
  configured: boolean;
  enabled: boolean;
  startupMs: number | null;
  errorCount: number;
  authErrorCount: number;
  disableCandidate: boolean;
  status: MaintenanceSeverity;
}

export interface MaintenanceGitStatus {
  path: string;
  status: MaintenanceSeverity;
  changedFiles: number | null;
  untrackedFiles: number | null;
  estimatedDiffBytes: number | null;
  upstreamConfigured: boolean | null;
  originConfigured: boolean | null;
  generatedFileCandidates: string[];
  gitignoreCandidates: string[];
  note: string;
}

export interface MaintenanceSystemHealth {
  diskTotalBytes: number | null;
  diskFreeBytes: number | null;
  diskUsedPercent: number | null;
  swapTotalBytes: number | null;
  swapUsedBytes: number | null;
  memoryFreePercent: number | null;
  codexProcessCount: number;
  codexCpuPercent: number;
  codexMemoryBytes: number;
}

export interface MaintenanceReport {
  schemaVersion: 1;
  generatedAtMs: number;
  durationMs: number;
  platform: string;
  overallStatus: MaintenanceSeverity;
  readOnly: true;
  privacyNote: string;
  findings: MaintenanceFinding[];
  storage: MaintenanceStorageEntry[];
  sqlite: MaintenanceSqliteHealth;
  fileLocks: MaintenanceFileLock[];
  orphanProcesses: MaintenanceProcessInfo[];
  processes: MaintenanceProcessInfo[];
  mcp: MaintenanceMcpStatus[];
  mcpMaxStartupMs: number | null;
  git: MaintenanceGitStatus[];
  system: MaintenanceSystemHealth;
  partialFailures: string[];
}

export type MaintenanceRiskLevel = "low" | "medium" | "high";
export type MaintenanceJobStatus = "running" | "waitingForIdle" | "completed" | "failed" | "cancelled" | "rolledBack" | "rollbackFailed";

export interface MaintenancePreviewInput {
  logRetentionDays: 7 | 30 | 90 | null;
  compactSqlite: boolean;
  repairOrphanPins: boolean;
  disableMcpServers: string[];
}

export interface MaintenanceFileCandidate {
  relativePath: string;
  bytes: number;
  modifiedMs: number;
}

export type MaintenanceActionDetails =
  | { type: "cleanupTextLogs"; retentionDays: number; logRoot: string; candidates: MaintenanceFileCandidate[] }
  | { type: "compactSqlite"; database: string; physicalBytes: number; estimatedLiveBytes: number; requiredFreeBytes: number }
  | { type: "repairOrphanPins"; statePath: string; orphanIds: string[]; sessionScanComplete: boolean }
  | { type: "disableMcpServers"; configPath: string; serverNames: string[] };

export interface MaintenanceActionPreview {
  id: string;
  kind: "cleanupTextLogs" | "compactSqlite" | "repairOrphanPins" | "disableMcpServers";
  title: string;
  summary: string;
  requiresCodexShutdown: boolean;
  reversible: boolean;
  estimatedReclaimableBytes: number;
  affectedItemCount: number;
  blockedReason: string | null;
  details: MaintenanceActionDetails;
}

export interface MaintenancePlan {
  schemaVersion: number;
  id: string;
  generatedAtMs: number;
  expiresAtMs: number;
  codexRunning: boolean;
  diskFreeBytes: number | null;
  backupRoot: string;
  actions: MaintenanceActionPreview[];
  warnings: string[];
}

export interface MaintenanceJob {
  id: string;
  planId: string;
  status: MaintenanceJobStatus;
  createdAtMs: number;
  finishedAtMs: number | null;
  actionIds: string[];
  reclaimedBytes: number;
  error: string | null;
  rollbackAvailable: boolean;
}

export interface MaintenanceExecuteRequest {
  planId: string;
  actionIds: string[];
}

export interface CodexShutdownResult {
  requested: boolean;
  stopped: boolean;
  remainingPids: number[];
  message: string;
}

export interface CodexRestartResult {
  requested: boolean;
  started: boolean;
  processIds: number[];
  message: string;
}

export interface CodexWriterActionResult {
  pid: number;
  requested: boolean;
  stopped: boolean;
  message: string;
}

export interface CodexArchiveResult {
  threadId: string;
  archived: boolean;
  transport: "appServer";
  message: string;
}

export interface ObservabilitySummary {
  totalRequests: number;
  successfulRequests: number;
  failedRequests: number;
  inputTokens: number;
  outputTokens: number;
  cachedInputTokens: number;
  reasoningTokens: number;
  totalTokens: number;
  estimatedCostUsd: number;
  lastEventAtMs: number | null;
  storageBytes: number;
  storagePath: string | null;
  persistenceError: string | null;
}

export interface ObservabilityBreakdownRow {
  key: string;
  requests: number;
  successfulRequests: number;
  failedRequests: number;
  inputTokens: number;
  outputTokens: number;
  cachedInputTokens: number;
  reasoningTokens: number;
  totalTokens: number;
  estimatedCostUsd: number;
  totalLatencyMs: number;
  maxLatencyMs: number;
}

export interface ObservabilityBreakdown {
  generatedAtMs: number;
  sinceMs: number;
  scannedEvents: number;
  truncated: boolean;
  daily: ObservabilityBreakdownRow[];
  providers: ObservabilityBreakdownRow[];
  models: ObservabilityBreakdownRow[];
  surfaces: ObservabilityBreakdownRow[];
}

export interface ObservabilityCleanupPreview {
  generatedAtMs: number;
  totalBytesBefore: number;
  bytesAfter: number;
  files: Array<{ name: string; bytes: number; day: number }>;
}

export interface ObservabilityTrashEntry {
  transactionId: string;
  createdAtMs: number;
  files: number;
  bytes: number;
}

export interface ObservabilityTrashReport {
  transactionId: string;
  files: number;
  bytes: number;
}

export interface DebugScope {
  id: string;
  startedAtMs: number;
  expiresAtMs: number;
}

export interface ObservationEvent {
  ledgerSequence: number;
  timestampMs: number;
  requestId: string;
  providerId: string | null;
  upstreamModel: string | null;
  upstreamEndpoint: string | null;
  exposedModel: string;
  routeId: string | null;
  accountId: string | null;
  statusCode: number;
  outcome: string;
  failureCategory: string | null;
  latencyMs: number;
  attempts: number;
  candidateOrdinal: number;
  sendCount: number;
  recoveryKinds: string[];
  recoveryKind: string | null;
  streaming: boolean;
  inputTokens: number;
  outputTokens: number;
  cachedInputTokens: number;
  reasoningTokens: number;
  totalTokens: number;
  estimatedCostUsd: number | null;
  shadow: boolean;
  shadowRuleId: string | null;
  parentRequestId: string | null;
}

export interface UpdateCheck {
  currentVersion: string;
  updateAvailable: boolean;
  manifest: {
    version: string;
    channel: string;
    downloadUrl: string;
    sha256: string;
    artifactSizeBytes: number;
    settingsSchemaVersion: number;
    publishedAt: string;
    notesUrl: string | null;
  };
}

export interface GatewayServiceStatus {
  supported: boolean;
  installed: boolean;
  running: boolean;
  definitionPath: string | null;
  shimInstalled: boolean;
  shimPath: string | null;
  supervisor: string;
  restartPolicy: string;
  message: string;
}

export interface ServiceInstallReport {
  installed: boolean;
  started: boolean;
  definitionPath: string;
  shimPath: string | null;
  warnings: string[];
}

export interface ServiceUninstallReport {
  stopped: boolean;
  removedDefinition: boolean;
  removedShim: boolean;
}

export interface ProviderOAuthLaunchReport {
  providerId: string;
  broker: string;
  launched: boolean;
  tokenCommand: string;
  instructions: string;
}

export interface OAuthProviderDescriptor {
  id: string;
  aliases: string[];
  displayName: string;
  flow: string;
  nativeLogin: boolean;
  cliImport: boolean;
}

export interface CompatibilityLabReport {
  generatedFromRegistryRevision: number;
  readOnly: boolean;
  rows: Array<{
    providerId: string;
    protocol: ProviderProtocol;
    fixtureId: string;
    expectation: "accept" | "reject";
    status: "pass" | "fail" | "skip";
    supported: boolean;
    configured: boolean;
    reason: string;
  }>;
}

export interface RouteDryRunReport {
  requestedModel: string;
  selected: string | null;
  candidates: Array<{
    rank: number;
    target: string;
    accountId: string | null;
    eligible: boolean;
    healthPercent: number;
    score: number;
    reasons: string[];
  }>;
}

export interface ExternalClientIntegrationInput {
  claudeCode: boolean;
  claudeDesktop: boolean;
  opencode: boolean;
  grok: boolean;
  pi: boolean;
  hermes: boolean;
}

export interface ClientIntegrationReport {
  clients: Array<{
    client: string;
    enabled: boolean;
    path: string | null;
    instructions: string;
  }>;
}

export interface PresetInstallInput {
  presetId: string;
  baseUrl: string | null;
  makeDefault: boolean;
}

export interface ModelMetadata {
  providerId: string;
  modelId: string;
  displayName: string | null;
  enabled: boolean;
  contextWindow: number | null;
  maxInputTokens: number | null;
  maxOutputTokens: number | null;
  inputModalities: Array<"text" | "image" | "audio" | "video">;
  reasoningEfforts: string[];
  defaultReasoningEffort: string | null;
  capabilities: ProviderCapabilities;
  inputPricePerMillion: number | null;
  outputPricePerMillion: number | null;
}

export type RouteStrategy = "failover" | "weightedRoundRobin" | "leastUsage" | "policy";

export interface RouteDefinition {
  id: string;
  name: string;
  description: string | null;
  alias: string | null;
  strategy: RouteStrategy;
  targets: Array<{ model: string; weight: number }>;
  stickyRequests: number;
  failureThreshold: number;
  defaultReasoningEffort: string | null;
  enabled: boolean;
  policy: {
    requiredCapabilities: string[];
    healthWeight: number;
    costWeight: number;
    quotaWeight: number;
    contextWeight: number;
    maxInputPricePerMillion: number | null;
    maxOutputPricePerMillion: number | null;
  };
}

export interface GatewayConfiguration {
  version: number;
  registryRevision: number;
  defaultProvider: string | null;
  providers: ProviderDefinition[];
  modelCatalog: ModelMetadata[];
  catalog: {
    selectedModels: string[];
    modelPickerOrder: string[];
    compatibilityLab: boolean;
  };
  routes: RouteDefinition[];
  runtime: {
    host: string;
    port: number;
    autoStart: boolean;
    standaloneService: boolean;
    dynamicPortFallback?: boolean;
    shutdownTimeoutMs: number;
    memoryBudgetBytes: number;
    maxInflightRequests: number;
  };
  security: {
    requireLocalToken: boolean;
    allowRemote: boolean;
    dnsPinning: boolean;
    corsAllowOrigins: string[];
    externalAccessKeys: Array<{
      id: string;
      label: string;
      envVar: string;
      scopes: Array<
        | "gateway:*"
        | "health:read"
        | "models:read"
        | "responses:write"
        | "chat:write"
        | "messages:write"
        | "gemini:write"
        | "tokens:count"
        | "images:write"
        | "search:write"
        | "videos:write"
        | "realtime:write"
        | "sidecars:write"
        | "compatibility:read"
        | "routing:read"
        | "memory:read"
        | "management:write"
      >;
      enabled: boolean;
      expiresAtUnix: number | null;
    }>;
  };
  observability: {
    requestLog: boolean;
    usageLog: boolean;
    redactContent: boolean;
    retentionDays: number;
    maxStorageBytes: number;
    trashRetentionDays: number;
    maxTrashBytes: number;
  };
  accountPool: {
    accounts: Array<{
      id: string;
      providerId: string;
      label: string;
      credential: ProviderCredential;
      enabled: boolean;
      priority: number;
      paused: boolean;
      pauseUntilUnix: number | null;
      pinned: boolean;
    }>;
    strategy: "quota" | "roundRobin" | "fillFirst";
    activeAccounts: Record<string, string>;
    autoSwitchThresholdPercent: number;
    stickyRequests: number;
  };
  agents: {
    multiAgentV2: boolean;
    surfaceMode: "v1" | "default" | "v2";
    maxThreads: number;
    subagentModels: string[];
    subagentFallback: string[];
    subagentFallbackByModel: Record<string, string[]>;
    effortCap: string | null;
    subagentEffortCap: string | null;
    imageInputMode: "auto" | "native" | "text";
    videoInputMode: "auto" | "native" | "text";
    documentInputMode: "auto" | "native" | "text";
    auxiliaryTimeoutMs: number;
    videoSampleFrames: number;
    documentMaxPages: number;
    ocrEnabled: boolean;
  };
  sidecars: {
    webSearchModel: string | null;
    visionModel: string | null;
    videoInputModel: string | null;
    documentModel: string | null;
    imageModel: string | null;
    videoModel: string | null;
    liveModel: string | null;
  };
  shadows: Array<{
    id: string;
    sourceModel: string;
    targets: string[];
    samplePercent: number;
    timeoutMs: number;
    maxResponseBytes: number;
    enabled: boolean;
  }>;
  helperIntercept: {
    enabled: boolean;
    targetModel: string | null;
    sourceModels: string[];
  };
  codex: {
    autoConnect: boolean;
    autoSyncCatalog: boolean;
  };
  integrations: {
    codex: boolean;
    claudeCode: boolean;
    claudeDesktop: boolean;
    opencode: boolean;
    grok: boolean;
    pi: boolean;
    hermes: boolean;
    claudeDesktopAliases: Record<string, string>;
    claudeDesktopFamilies: Record<string, "opus" | "fable" | "sonnet" | "haiku">;
    claudeDesktopDefaults: Partial<Record<"opus" | "fable" | "sonnet" | "haiku", string>>;
    managedClients: Record<string, {
      enabled: boolean;
      configPath: string | null;
      ownedFields: string[];
    }>;
  };
  updates: {
    channel: "stable" | "beta" | "nightly" | "custom";
    autoCheck: boolean;
    manifestUrl: string | null;
    publicKeyBase64: string | null;
    installerEndpoint: string | null;
    installerPublicKey: string | null;
  };
}

export interface GatewayStatus {
  running: boolean;
  url: string;
  providers: ProviderDefinition[];
  defaultProvider: string | null;
  codexConfigured: boolean;
  settingsPath: string | null;
}

export type AgentMediaTestKind = "image" | "video" | "document" | "imageGeneration";

export interface AgentMediaTestResult {
  kind: AgentMediaTestKind;
  model: string;
  summary: string;
  previewDataUrl: string | null;
  sourcePath: string | null;
  durationMs: number;
}

export interface CodexPluginStatus {
  installed: boolean;
  enabled: boolean;
  gatewayConnected: boolean;
  gatewayReachable: boolean;
  mcpHealthy: boolean;
  healthDetail: string | null;
  pluginId: string | null;
  pluginPath: string | null;
  configPath: string;
}

export interface HermesProfile {
  name: string;
  displayName: string | null;
  description: string;
}

export interface ProviderUpsertInput {
  provider: ProviderDefinition;
  makeDefault: boolean;
}

export interface CodexGatewayInstallInput {
  model: string | null;
}

export interface CodexRestoreReport {
  restored: boolean;
  configPath: string;
  conflicts: string[];
  removedCatalog: boolean;
}

export interface CodetasUninstallReport {
  restoredCodex: boolean;
  removedSettings: boolean;
  removedCatalog: boolean;
  removedObservability: boolean;
  removedService: boolean;
  stoppedGateway: boolean;
  conflicts: string[];
}
