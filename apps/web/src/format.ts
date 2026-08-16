import type { GatewayConfiguration } from "@codetas/core";

export function h(value: unknown): string {
  return String(value ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#039;");
}

export function formatNumber(value: number | null | undefined): string {
  return new Intl.NumberFormat("ja-JP", { maximumFractionDigits: 1 }).format(value ?? 0);
}

export function formatBytes(value: number | null | undefined): string {
  const bytes = value ?? 0;
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
  if (bytes < 1024 ** 4) return `${(bytes / 1024 ** 3).toFixed(1)} GB`;
  return `${(bytes / 1024 ** 4).toFixed(1)} TB`;
}

export function statusDot(active: boolean, warning = false): string {
  return `<span class="status-dot ${active ? (warning ? "warning" : "active") : "idle"}" aria-hidden="true"></span>`;
}

export function helpTip(help: string): string {
  return `<span class="help-tip" tabindex="0" aria-label="${h(help)}">i<span role="tooltip">${h(help)}</span></span>`;
}

export function protocolLabel(protocol: string | undefined): string {
  switch (protocol) {
    case "responses":
      return "Responses";
    case "chatCompletions":
      return "Chat";
    case "anthropicMessages":
      return "Messages";
    case "geminiGenerateContent":
      return "Gemini";
    default:
      return protocol || "-";
  }
}

export function modelCount(config: GatewayConfiguration): number {
  return providerModelIds(config).length;
}

export function providerModelIds(config: GatewayConfiguration): string[] {
  const ids = new Set<string>();
  for (const provider of config.providers) {
    for (const model of provider.models ?? []) ids.add(`${provider.id}/${model}`);
    if (provider.defaultModel) ids.add(`${provider.id}/${provider.defaultModel}`);
  }
  for (const model of config.modelCatalog) ids.add(`${model.providerId}/${model.modelId}`);
  return [...ids].sort((left, right) => left.localeCompare(right));
}

export function allModelIds(config: GatewayConfiguration): string[] {
  const imageIds = new Set(imageModelIds(config));
  const ids = new Set(providerModelIds(config).filter((model) => !imageIds.has(model)));
  for (const route of config.routes) {
    const publicId = route.alias ?? route.id;
    if (route.enabled && !imageIds.has(publicId)) ids.add(publicId);
  }
  return [...ids].sort((left, right) => left.localeCompare(right));
}

const IMAGE_MODEL_TERMS = [
  "gpt-image",
  "imagegen",
  "image-generation",
  "imagen",
  "dall-e",
  "flux",
  "stable-diffusion",
];

export function explicitImageGenerationModelIds(
  provider: GatewayConfiguration["providers"][number],
): Set<string> {
  const ids = new Set(provider.imageGenerationModels ?? []);
  for (const [alias, wire] of Object.entries(provider.modelWireIds ?? {})) {
    if (ids.has(alias) || ids.has(wire)) {
      ids.add(alias);
      ids.add(wire);
    }
  }
  return ids;
}

export function imageModelIds(config: GatewayConfiguration): string[] {
  const ids = new Set<string>();
  for (const provider of config.providers) {
    if (!provider.enabled
      || (provider.transport ?? "standard") !== "standard"
      || !provider.capabilities?.imageGeneration) continue;
    const modelMetadata = config.modelCatalog.filter((model) => model.providerId === provider.id);
    const explicitIds = explicitImageGenerationModelIds(provider);
    for (const model of modelMetadata) {
      if (model.enabled && model.capabilities.imageGeneration) explicitIds.add(model.modelId);
    }
    for (const [alias, wire] of Object.entries(provider.modelWireIds ?? {})) {
      if (explicitIds.has(alias) || explicitIds.has(wire)) {
        explicitIds.add(alias);
        explicitIds.add(wire);
      }
    }
    const modelIds = explicitIds.size > 0
      ? explicitIds
      : new Set([
        ...(provider.models ?? []),
        ...Object.keys(provider.modelWireIds ?? {}),
        ...modelMetadata.map((model) => model.modelId),
      ]);
    for (const modelId of modelIds) {
      if (explicitIds.size === 0
        && !IMAGE_MODEL_TERMS.some((term) => modelId.toLowerCase().includes(term))) continue;
      const metadata = config.modelCatalog.find(
        (model) => model.providerId === provider.id && model.modelId === modelId,
      );
      const wireModelId = provider.modelWireIds?.[modelId] ?? modelId;
      const wireMetadata = config.modelCatalog.find(
        (model) => model.providerId === provider.id && model.modelId === wireModelId,
      );
      if ((metadata && (!metadata.enabled || !metadata.capabilities.imageGeneration))
        || (wireMetadata && (!wireMetadata.enabled
          || !wireMetadata.capabilities.imageGeneration))) continue;
      ids.add(`${provider.id}/${modelId}`);
    }
  }
  for (const route of config.routes.filter((route) => route.enabled)) {
    if (route.targets.length > 0 && route.targets.every((target) => ids.has(target.model))) {
      ids.add(route.alias ?? route.id);
    }
  }
  return [...ids].sort((left, right) => left.localeCompare(right));
}

export function lines(value: FormDataEntryValue | null): string[] {
  return String(value ?? "").split(/\r?\n|,/).map((item) => item.trim()).filter(Boolean);
}
