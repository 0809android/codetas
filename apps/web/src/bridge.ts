import { invoke } from "@tauri-apps/api/core";
import type {
  CodexGatewayInstallInput,
  CodexRestoreReport,
  CodetasUninstallReport,
  DebugScope,
  ClientIntegrationReport,
  ExternalClientIntegrationInput,
  GatewayConfiguration,
  GatewayDiagnosticReport,
  GatewayStatus,
  GatewayServiceStatus,
  ObservabilitySummary,
  ObservationEvent,
  ProjectInspection,
  ProviderConnectionReport,
  ProviderDefinition,
  ProviderPreset,
  PresetInstallInput,
  ProviderUpsertInput,
  ServiceInstallReport,
  ServiceUninstallReport,
  UpdateCheck,
} from "@codetas/core";

declare global {
  interface Window {
    __TAURI_INTERNALS__?: unknown;
  }
}

export const isDesktopRuntime = (): boolean =>
  typeof window !== "undefined" && Boolean(window.__TAURI_INTERNALS__);

const demoInspection = (): ProjectInspection => ({
  id: "demo-project",
  name: "demo-project",
  path: "/Users/demo/Projects/demo-project",
  contextFile: "/Users/demo/Projects/demo-project/.hermes.md",
  agentsFile: "/Users/demo/Projects/demo-project/AGENTS.md",
  skillsDirectory: "/Users/demo/Projects/demo-project/skills",
  skillsCount: 3,
  mcpFile: "/Users/demo/Projects/demo-project/mcp.json",
  codexConfigFile: null,
  warnings: ["MCP設定は適用前に変換内容の確認が必要です。"],
  inspectedAt: new Date().toISOString(),
});

let demoGateway: GatewayStatus = {
  running: true,
  url: "http://127.0.0.1:42421/v1",
  providers: [
    {
      id: "local",
      name: "Local model server",
      baseUrl: "http://127.0.0.1:11434/v1",
      protocol: "chatCompletions",
      apiKeyEnv: null,
      defaultModel: "local-model",
      models: ["local-model"],
      enabled: true,
      allowPrivateNetwork: true,
    },
  ],
  defaultProvider: "local",
  codexConfigured: false,
  settingsPath: "/Users/demo/Library/Application Support/jp.kinocode.codetas/providers.json",
};

const demoConfiguration = (): GatewayConfiguration => ({
  version: 2,
  registryRevision: 1,
  defaultProvider: demoGateway.defaultProvider,
  providers: structuredClone(demoGateway.providers),
  modelCatalog: [],
  catalog: { selectedModels: [], modelPickerOrder: [], compatibilityLab: true },
  routes: [],
  runtime: {
    host: "127.0.0.1",
    port: 42421,
    autoStart: true,
    standaloneService: false,
    shutdownTimeoutMs: 10000,
    memoryBudgetBytes: 536870912,
    maxInflightRequests: 64,
  },
  security: {
    requireLocalToken: false,
    allowRemote: false,
    dnsPinning: true,
    corsAllowOrigins: [],
    externalAccessKeys: [],
  },
  observability: {
    requestLog: false,
    usageLog: true,
    redactContent: true,
    retentionDays: 14,
    maxStorageBytes: 268435456,
    trashRetentionDays: 7,
    maxTrashBytes: 268435456,
  },
  accountPool: {
    accounts: [],
    strategy: "quota",
    activeAccounts: {},
    autoSwitchThresholdPercent: 90,
    stickyRequests: 32,
  },
  agents: {
    multiAgentV2: false,
    surfaceMode: "default",
    maxThreads: 4,
    subagentModels: [],
    subagentFallback: [],
    subagentFallbackByModel: {},
    effortCap: null,
    subagentEffortCap: null,
    imageInputMode: "auto",
    videoInputMode: "auto",
    documentInputMode: "auto",
    auxiliaryTimeoutMs: 120000,
    videoSampleFrames: 8,
    documentMaxPages: 12,
    ocrEnabled: true,
  },
  sidecars: {
    webSearchModel: null,
    visionModel: null,
    videoInputModel: null,
    documentModel: null,
    imageModel: null,
    videoModel: null,
    liveModel: null,
  },
  shadows: [],
  helperIntercept: {
    enabled: false,
    targetModel: null,
    sourceModels: ["gpt-5.4-mini", "gpt-5.6-luna"],
  },
  codex: {
    autoConnect: true,
    autoSyncCatalog: true,
  },
  integrations: {
    codex: true,
    claudeCode: false,
    claudeDesktop: false,
    opencode: false,
    grok: false,
    pi: false,
    hermes: false,
    claudeDesktopAliases: {},
    claudeDesktopFamilies: {},
    claudeDesktopDefaults: {},
    managedClients: {},
  },
  updates: {
    channel: "stable",
    autoCheck: false,
    manifestUrl: null,
    publicKeyBase64: null,
    installerEndpoint: null,
    installerPublicKey: null,
  },
});

export async function pickAndInspectProject(): Promise<ProjectInspection | null> {
  if (!isDesktopRuntime()) {
    return demoInspection();
  }

  const path = await invoke<string | null>("pick_project");
  if (!path) return null;
  return invoke<ProjectInspection>("inspect_project", { path });
}

export async function refreshProject(path: string): Promise<ProjectInspection> {
  if (!isDesktopRuntime()) {
    return { ...demoInspection(), path, inspectedAt: new Date().toISOString() };
  }
  return invoke<ProjectInspection>("inspect_project", { path });
}

export async function startProviderGateway(): Promise<GatewayStatus> {
  if (!isDesktopRuntime()) return structuredClone(demoGateway);
  return invoke<GatewayStatus>("start_provider_gateway");
}

export async function getGatewayConfiguration(): Promise<GatewayConfiguration> {
  if (!isDesktopRuntime()) return demoConfiguration();
  return invoke<GatewayConfiguration>("gateway_configuration");
}

export async function checkForCodetasUpdate(): Promise<UpdateCheck> {
  if (!isDesktopRuntime()) {
    return {
      currentVersion: "0.1.0",
      updateAvailable: false,
      manifest: {
        version: "0.1.0",
        channel: "stable",
        downloadUrl: "https://example.invalid/codetas",
        sha256: "0".repeat(64),
        artifactSizeBytes: 0,
        settingsSchemaVersion: 2,
        publishedAt: new Date().toISOString(),
        notesUrl: null,
      },
    };
  }
  return invoke<UpdateCheck>("check_for_codetas_update");
}

export async function saveGatewayConfiguration(
  configuration: GatewayConfiguration,
): Promise<GatewayConfiguration> {
  if (!isDesktopRuntime()) {
    demoGateway.providers = structuredClone(configuration.providers);
    demoGateway.defaultProvider = configuration.defaultProvider;
    return structuredClone(configuration);
  }
  return invoke<GatewayConfiguration>("save_gateway_configuration", { configuration });
}

export async function listProviderPresets(): Promise<ProviderPreset[]> {
  if (!isDesktopRuntime()) return [];
  return invoke<ProviderPreset[]>("list_provider_presets");
}

export async function testGatewayProvider(
  providerId: string,
): Promise<ProviderConnectionReport> {
  if (!isDesktopRuntime()) {
    return {
      providerId,
      reachable: true,
      authenticated: true,
      status: 200,
      latencyMs: 12,
      modelCount: 1,
      message: "demo provider is reachable",
    };
  }
  return invoke<ProviderConnectionReport>("test_gateway_provider", { providerId });
}

export async function runGatewayDiagnostics(): Promise<GatewayDiagnosticReport> {
  if (!isDesktopRuntime()) {
    return {
      checks: [{ id: "demo", level: "pass", summary: "デモ環境は正常です", remediation: null }],
      passed: 1,
      warnings: 0,
      errors: 0,
    };
  }
  return invoke<GatewayDiagnosticReport>("gateway_diagnostics");
}

export async function getGatewayObservabilitySummary(): Promise<ObservabilitySummary> {
  if (!isDesktopRuntime()) {
    return {
      totalRequests: 128,
      successfulRequests: 125,
      failedRequests: 3,
      inputTokens: 482100,
      outputTokens: 92340,
      cachedInputTokens: 120800,
      reasoningTokens: 28400,
      totalTokens: 574440,
      estimatedCostUsd: 1.82,
      lastEventAtMs: Date.now(),
      storageBytes: 98304,
      storagePath: "/Users/demo/Library/Application Support/jp.kinocode.codetas/observability",
      persistenceError: null,
    };
  }
  return invoke<ObservabilitySummary>("gateway_observability_summary");
}

export async function startGatewayDebugScope(durationSeconds = 300): Promise<DebugScope> {
  if (!isDesktopRuntime()) {
    const startedAtMs = Date.now();
    return {
      id: `debug-demo-${startedAtMs}`,
      startedAtMs,
      expiresAtMs: startedAtMs + durationSeconds * 1000,
    };
  }
  return invoke<DebugScope>("start_gateway_debug_scope", { durationSeconds });
}

export async function getGatewayDebugEvents(
  scopeId: string,
  limit = 100,
): Promise<ObservationEvent[]> {
  if (!isDesktopRuntime()) return [];
  return invoke<ObservationEvent[]>("gateway_debug_events", { scopeId, limit });
}

export async function stopGatewayDebugScope(scopeId: string): Promise<void> {
  if (!isDesktopRuntime()) return;
  await invoke("stop_gateway_debug_scope", { scopeId });
}

export async function getGatewayServiceStatus(): Promise<GatewayServiceStatus> {
  if (!isDesktopRuntime()) {
    return {
      supported: true,
      installed: false,
      running: false,
      definitionPath: null,
      shimInstalled: false,
      shimPath: "/Users/demo/Library/Application Support/codetas/bin/codetas-codex",
      supervisor: "launchd",
      restartPolicy: "KeepAlive / 5秒スロットル",
      message: "CODETAS Gatewayサービスは未登録です",
    };
  }
  return invoke<GatewayServiceStatus>("gateway_service_status");
}

export async function installGatewayService(
  installShim: boolean,
): Promise<ServiceInstallReport> {
  if (!isDesktopRuntime()) {
    return {
      installed: true,
      started: true,
      definitionPath: "/Users/demo/Library/LaunchAgents/jp.kinocode.codetas.gateway.plist",
      shimPath: installShim ? "/Users/demo/Library/Application Support/codetas/bin/codetas-codex" : null,
      warnings: [],
    };
  }
  return invoke<ServiceInstallReport>("install_gateway_service", {
    input: { installShim },
  });
}

export async function startGatewayService(): Promise<GatewayServiceStatus> {
  if (!isDesktopRuntime()) {
    return {
      supported: true,
      installed: true,
      running: true,
      definitionPath: "/Users/demo/Library/LaunchAgents/jp.kinocode.codetas.gateway.plist",
      shimInstalled: true,
      shimPath: "/Users/demo/Library/Application Support/codetas/bin/codetas-codex",
      supervisor: "launchd",
      restartPolicy: "KeepAlive / 5秒スロットル",
      message: "CODETAS Gatewayサービスは起動中です",
    };
  }
  return invoke<GatewayServiceStatus>("start_gateway_service");
}

export async function uninstallGatewayService(): Promise<ServiceUninstallReport> {
  if (!isDesktopRuntime()) {
    return { stopped: true, removedDefinition: true, removedShim: true };
  }
  return invoke<ServiceUninstallReport>("uninstall_gateway_service");
}

export async function syncClientIntegrations(
  input: ExternalClientIntegrationInput,
): Promise<ClientIntegrationReport> {
  if (!isDesktopRuntime()) {
    return {
      clients: Object.entries(input).map(([client, enabled]) => ({
        client,
        enabled,
        path: enabled ? "/Users/demo/Library/Application Support/codetas/client-launchers/" + client : null,
        instructions: enabled ? "Use the generated CODETAS launcher." : "Disabled.",
      })),
    };
  }
  return invoke<ClientIntegrationReport>("sync_client_integrations", { input });
}

export async function installProviderPreset(input: PresetInstallInput): Promise<GatewayStatus> {
  if (!isDesktopRuntime()) return structuredClone(demoGateway);
  return invoke<GatewayStatus>("install_provider_preset", { input });
}

export async function refreshGatewayProviderModels(
  providerId: string,
): Promise<GatewayConfiguration> {
  if (!isDesktopRuntime()) return demoConfiguration();
  return invoke<GatewayConfiguration>("refresh_gateway_provider_models", { providerId });
}

export async function syncCodexModelCatalog(): Promise<string> {
  if (!isDesktopRuntime()) return "/Users/demo/.codex/codetas-model-catalog.json";
  return invoke<string>("sync_codex_model_catalog");
}

export async function getProviderGatewayStatus(): Promise<GatewayStatus> {
  if (!isDesktopRuntime()) return structuredClone(demoGateway);
  return invoke<GatewayStatus>("provider_gateway_status");
}

export async function upsertGatewayProvider(input: ProviderUpsertInput): Promise<GatewayStatus> {
  if (!isDesktopRuntime()) {
    const index = demoGateway.providers.findIndex((provider) => provider.id === input.provider.id);
    if (index >= 0) demoGateway.providers[index] = input.provider;
    else demoGateway.providers = [...demoGateway.providers, input.provider];
    if (input.makeDefault || !demoGateway.defaultProvider) demoGateway.defaultProvider = input.provider.id;
    return structuredClone(demoGateway);
  }
  return invoke<GatewayStatus>("upsert_gateway_provider", { input });
}

export async function removeGatewayProvider(providerId: string): Promise<GatewayStatus> {
  if (!isDesktopRuntime()) {
    demoGateway.providers = demoGateway.providers.filter((provider) => provider.id !== providerId);
    if (demoGateway.defaultProvider === providerId) {
      demoGateway.defaultProvider = demoGateway.providers[0]?.id ?? null;
    }
    return structuredClone(demoGateway);
  }
  return invoke<GatewayStatus>("remove_gateway_provider", { providerId });
}

export async function setDefaultGatewayProvider(providerId: string): Promise<GatewayStatus> {
  if (!isDesktopRuntime()) {
    demoGateway.defaultProvider = providerId;
    return structuredClone(demoGateway);
  }
  return invoke<GatewayStatus>("set_default_gateway_provider", { providerId });
}

export async function installCodexGatewayConfig(input: CodexGatewayInstallInput): Promise<string> {
  if (!isDesktopRuntime()) {
    demoGateway.codexConfigured = true;
    return "/Users/demo/.codex/config.toml";
  }
  return invoke<string>("install_codex_gateway_config", { input });
}

export async function restoreCodexGatewayConfig(): Promise<CodexRestoreReport> {
  if (!isDesktopRuntime()) {
    demoGateway.codexConfigured = false;
    return {
      restored: true,
      configPath: "/Users/demo/.codex/config.toml",
      conflicts: [],
      removedCatalog: true,
    };
  }
  return invoke<CodexRestoreReport>("restore_codex_gateway_config");
}

export async function uninstallCodetasIntegration(): Promise<CodetasUninstallReport> {
  if (!isDesktopRuntime()) {
    demoGateway = {
      ...demoGateway,
      running: false,
      providers: [],
      defaultProvider: null,
      codexConfigured: false,
    };
    return {
      restoredCodex: true,
      removedSettings: true,
      removedCatalog: true,
      removedObservability: true,
      removedService: true,
      stoppedGateway: true,
      conflicts: [],
    };
  }
  return invoke<CodetasUninstallReport>("uninstall_codetas_integration");
}

export function providerRoute(provider: ProviderDefinition): string | null {
  const model = provider.defaultModel?.trim() || provider.models[0]?.trim();
  return model ? `${provider.id}/${model}` : null;
}
