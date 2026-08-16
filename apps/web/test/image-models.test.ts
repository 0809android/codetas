import assert from "node:assert/strict";
import test from "node:test";
import type { GatewayConfiguration, ProviderDefinition } from "@codetas/core";
import { allModelIds, imageModelIds } from "../src/format.ts";

function provider(overrides: Partial<ProviderDefinition> = {}): ProviderDefinition {
  return {
    id: "images",
    name: "Images",
    baseUrl: "https://api.example.test/v1",
    protocol: "responses",
    transport: "standard",
    apiKeyEnv: null,
    defaultModel: null,
    models: [],
    enabled: true,
    allowPrivateNetwork: false,
    clients: [],
    capabilities: { imageGeneration: true },
    ...overrides,
  } as ProviderDefinition;
}

function configuration(
  imageProvider: ProviderDefinition,
  routes: GatewayConfiguration["routes"] = [],
  modelCatalog: GatewayConfiguration["modelCatalog"] = [],
): GatewayConfiguration {
  return {
    providers: [imageProvider],
    modelCatalog,
    routes,
  } as GatewayConfiguration;
}

test("explicit imageGenerationModels bypass name heuristics and normalize aliases", () => {
  const config = configuration(provider({
    models: ["gpt-5.5"],
    imageGenerationModels: ["art-v2"],
    modelWireIds: { "studio-art": "art-v2" },
  }));

  assert.deepEqual(imageModelIds(config), ["images/art-v2", "images/studio-art"]);
  assert.deepEqual(allModelIds(config), ["images/gpt-5.5"]);
});

test("provider-wide capability alone does not promote a normal chat model", () => {
  const config = configuration(provider({ models: ["gpt-5.5"] }));

  assert.deepEqual(imageModelIds(config), []);
  assert.deepEqual(allModelIds(config), ["images/gpt-5.5"]);
});

test("image identity remains excluded from chat when provider availability is off", () => {
  const config = configuration(provider({
    models: ["gpt-5.5"],
    imageGenerationModels: ["art-v2"],
    capabilities: { imageGeneration: false },
  }));

  assert.deepEqual(imageModelIds(config), []);
  assert.deepEqual(allModelIds(config), ["images/gpt-5.5"]);
});

test("disabled canonical metadata blocks image availability without losing alias identity", () => {
  const config = configuration(
    provider({
      models: ["gpt-5.5"],
      imageGenerationModels: ["studio-art"],
      modelWireIds: { "studio-art": "art-v2" },
    }),
    [],
    [{
      providerId: "images",
      modelId: "art-v2",
      enabled: false,
      capabilities: { imageGeneration: true },
    }] as GatewayConfiguration["modelCatalog"],
  );

  assert.deepEqual(imageModelIds(config), []);
  assert.deepEqual(allModelIds(config), ["images/gpt-5.5"]);
});

test("legacy configurations use the image-name heuristic only without an explicit list", () => {
  const config = configuration(provider({ models: ["legacy-imagegen-v1"] }));

  assert.deepEqual(imageModelIds(config), ["images/legacy-imagegen-v1"]);
  assert.deepEqual(allModelIds(config), []);
});

test("normal model options hide all-image routes and keep mixed routes", () => {
  const config = configuration(
    provider({
      models: ["gpt-5.5"],
      imageGenerationModels: ["art-v2"],
    }),
    [
      {
        id: "only-image",
        alias: "img-route",
        enabled: true,
        targets: [{ model: "images/art-v2", weight: 1 }],
      },
      {
        id: "mixed",
        alias: null,
        enabled: true,
        targets: [
          { model: "images/art-v2", weight: 1 },
          { model: "images/gpt-5.5", weight: 1 },
        ],
      },
    ] as GatewayConfiguration["routes"],
  );

  assert.deepEqual(allModelIds(config), ["images/gpt-5.5", "mixed"]);
});
