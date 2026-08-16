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
  const ids = new Set(providerModelIds(config));
  for (const route of config.routes) {
    if (route.enabled) ids.add(route.alias ?? route.id);
  }
  return [...ids].sort((left, right) => left.localeCompare(right));
}

export function lines(value: FormDataEntryValue | null): string[] {
  return String(value ?? "").split(/\r?\n|,/).map((item) => item.trim()).filter(Boolean);
}
