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
}

export interface ProviderLimits {
  connectTimeoutMs: number;
  requestTimeoutMs: number;
  streamIdleTimeoutMs: number;
  requestRetries: number;
  streamRetries: number;
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
  modelReasoningModes?: Record<string, string>;
  stripModelBracketSuffix?: boolean;
  responsesPath?: string | null;
  realtimeWsBaseUrl?: string | null;
  statelessResponses?: boolean;
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
  autoToolChoiceOnlyModels?: string[];
  preserveReasoningContentModels?: string[];
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
  exposedModel: string;
  routeId: string | null;
  accountId: string | null;
  statusCode: number;
  outcome: string;
  failureCategory: string | null;
  latencyMs: number;
  attempts: number;
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

export interface ExternalClientIntegrationInput {
  claudeCode: boolean;
  claudeDesktop: boolean;
  opencode: boolean;
  grok: boolean;
  pi: boolean;
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

export type RouteStrategy = "failover" | "weightedRoundRobin" | "leastUsage";

export interface RouteDefinition {
  id: string;
  name: string;
  alias: string | null;
  strategy: RouteStrategy;
  targets: Array<{ model: string; weight: number }>;
  stickyRequests: number;
  failureThreshold: number;
  defaultReasoningEffort: string | null;
  enabled: boolean;
}

export interface GatewayConfiguration {
  version: number;
  defaultProvider: string | null;
  providers: ProviderDefinition[];
  modelCatalog: ModelMetadata[];
  routes: RouteDefinition[];
  runtime: {
    host: string;
    port: number;
    autoStart: boolean;
    standaloneService: boolean;
    dynamicPortFallback?: boolean;
    shutdownTimeoutMs: number;
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
    effortCap: string | null;
    subagentEffortCap: string | null;
  };
  sidecars: {
    webSearchModel: string | null;
    visionModel: string | null;
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
    claudeDesktopAliases: Record<string, string>;
    claudeDesktopFamilies: Record<string, "opus" | "fable" | "sonnet" | "haiku">;
    claudeDesktopDefaults: Partial<Record<"opus" | "fable" | "sonnet" | "haiku", string>>;
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
