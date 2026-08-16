import type { GatewayConfiguration, ModelMetadata, ProviderDefinition } from "@codetas/core";

export type AgentPresetId = "deepseek-gpt" | "kimi-gpt" | "current-gpt";

export interface ResolvedAgentPreset {
  id: AgentPresetId;
  mainModel: string | null;
  visionModel: string | null;
  imageModel: string | null;
  available: boolean;
}

type CatalogCandidate = {
  qualifiedId: string;
  provider: ProviderDefinition;
  metadata: ModelMetadata | null;
};

function catalogCandidates(config: GatewayConfiguration): CatalogCandidate[] {
  const candidates = new Map<string, CatalogCandidate>();
  for (const provider of config.providers.filter((item) => item.enabled)) {
    const models = new Set([
      ...(provider.models ?? []),
      ...(provider.defaultModel ? [provider.defaultModel] : []),
      ...Object.keys(provider.modelWireIds ?? {}).filter((model) =>
        ["gpt-image", "imagegen", "image-generation", "imagen", "dall-e", "flux", "stable-diffusion"]
          .some((term) => model.toLowerCase().includes(term))),
    ]);
    for (const modelId of models) {
      const metadata = config.modelCatalog.find((item) => item.providerId === provider.id && item.modelId === modelId) ?? null;
      if (metadata && !metadata.enabled) continue;
      const qualifiedId = `${provider.id}/${modelId}`;
      candidates.set(qualifiedId, {
        qualifiedId,
        provider,
        metadata,
      });
    }
  }
  for (const metadata of config.modelCatalog.filter((item) => item.enabled)) {
    const provider = config.providers.find((item) => item.id === metadata.providerId && item.enabled);
    if (!provider) continue;
    const qualifiedId = `${provider.id}/${metadata.modelId}`;
    candidates.set(qualifiedId, { qualifiedId, provider, metadata });
  }
  return [...candidates.values()];
}

function candidateSupportsVision({ qualifiedId, provider, metadata }: CatalogCandidate): boolean {
  const modelId = metadata?.modelId ?? qualifiedId.split("/").at(-1) ?? "";
  if (metadata?.inputModalities.length) return metadata.inputModalities.includes("image");
  const providerModalities = provider.modelInputModalities?.[modelId];
  if (providerModalities) return providerModalities.includes("image");
  return Boolean(metadata?.capabilities.vision || provider.capabilities?.vision);
}

function candidateSupportsImageGeneration({ provider, metadata }: CatalogCandidate): boolean {
  return (provider.transport ?? "standard") === "standard"
    && Boolean(provider.capabilities?.imageGeneration)
    && (metadata ? metadata.enabled && metadata.capabilities.imageGeneration : true);
}

function numericVersion(value: string): number {
  const matches = [...value.matchAll(/(?:^|[-_.])(\d+)(?:\.(\d+))?/g)];
  return matches.reduce((score, match) => Math.max(score, Number(match[1] ?? 0) * 100 + Number(match[2] ?? 0)), 0);
}

function namedVersion(value: string, name: string): number {
  const match = value.match(new RegExp(`${name}[-_.](\\d+)(?:[._](\\d+))?`));
  return match ? Number(match[1] ?? 0) * 100 + Number(match[2] ?? 0) : 0;
}

function newest(candidates: CatalogCandidate[], score: (candidate: CatalogCandidate) => number): string | null {
  return candidates
    .map((candidate) => ({ candidate, score: score(candidate) }))
    .filter((item) => item.score > 0)
    .sort((left, right) => right.score - left.score || right.candidate.qualifiedId.localeCompare(left.candidate.qualifiedId))[0]
    ?.candidate.qualifiedId ?? null;
}

function latestGptVision(candidates: CatalogCandidate[]): string | null {
  return newest(candidates, (candidate) => {
    const { qualifiedId, provider } = candidate;
    const value = qualifiedId.toLowerCase();
    const openAi = provider.id.toLowerCase().includes("openai") || provider.name.toLowerCase().includes("openai");
    const gpt = /(?:^|\/)gpt[-_.]/.test(value);
    if ((!openAi && !gpt) || !candidateSupportsVision(candidate) || value.includes("gpt-image")) return 0;
    const tier = value.includes("sol") ? 500 : value.includes("pro") ? 420 : value.includes("terra") ? 360 : value.includes("luna") ? 220 : value.includes("mini") ? 50 : 300;
    return 10_000 + namedVersion(value, "gpt") * 1_000 + tier;
  });
}

function latestImageGenerator(candidates: CatalogCandidate[]): string | null {
  return newest(candidates, (candidate) => {
    const { qualifiedId } = candidate;
    const value = qualifiedId.toLowerCase();
    const explicitImageModel = ["gpt-image", "imagegen", "image-generation", "imagen", "dall-e", "flux", "stable-diffusion"]
      .some((term) => value.includes(term));
    if (!explicitImageModel || !candidateSupportsImageGeneration(candidate)) return 0;
    return 10_000 + namedVersion(value, "gpt-image") * 10 + (value.includes("gpt-image") ? 500 : 0);
  });
}

function latestFamily(candidates: CatalogCandidate[], terms: string[]): string | null {
  return newest(candidates, ({ qualifiedId, provider }) => {
    const value = `${qualifiedId} ${provider.name}`.toLowerCase();
    if (!terms.some((term) => value.includes(term))) return 0;
    const tier = value.includes("reasoner") || value.includes("thinking") ? 120 : value.includes("chat") ? 80 : 100;
    const defaultModel = provider.defaultModel && qualifiedId === `${provider.id}/${provider.defaultModel}` ? 1_000_000 : 0;
    return 10_000 + defaultModel + numericVersion(value) * 10 + tier;
  });
}

function currentDefaultModel(config: GatewayConfiguration, candidates: CatalogCandidate[]): string | null {
  const provider = config.providers.find((item) => item.enabled && item.id === config.defaultProvider);
  if (!provider) return null;
  const preferred = provider.defaultModel ? `${provider.id}/${provider.defaultModel}` : null;
  if (preferred && candidates.some((candidate) => candidate.qualifiedId === preferred)) return preferred;
  return candidates.find((candidate) => candidate.provider.id === provider.id)?.qualifiedId ?? null;
}

export function resolveAgentPreset(config: GatewayConfiguration, id: AgentPresetId): ResolvedAgentPreset {
  const candidates = catalogCandidates(config);
  const visionModel = latestGptVision(candidates);
  const imageModel = latestImageGenerator(candidates);
  const mainModel = id === "deepseek-gpt"
    ? latestFamily(candidates, ["deepseek"])
    : id === "kimi-gpt"
      ? latestFamily(candidates, ["kimi", "moonshot"])
      : currentDefaultModel(config, candidates);
  return {
    id,
    mainModel,
    visionModel,
    imageModel,
    available: Boolean(visionModel && mainModel),
  };
}
