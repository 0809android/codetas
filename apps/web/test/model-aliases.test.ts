import assert from "node:assert/strict";
import test from "node:test";
import type { GatewayConfiguration } from "@codetas/core";
import { catalogModelEntries } from "../src/format.ts";

test("canonical picker entries hide old variants without deleting their routing metadata", () => {
  const base = "claude-opus-5-5";
  const aliases = Object.fromEntries(["low", "medium", "high"].map((effort) => [`${base}-${effort}`, base]));
  const config = {
    providers: [{
      id: "google-antigravity", name: "Antigravity", models: [base],
      defaultModel: `${base}-high`, modelCatalogAliases: aliases, capabilities: {},
    }],
    modelCatalog: [base, ...Object.keys(aliases)].map((modelId) => ({
      providerId: "google-antigravity", modelId, enabled: true, capabilities: {},
    })),
    routes: [], catalog: { selectedModels: [`google-antigravity/${base}`] },
  } as unknown as GatewayConfiguration;
  const before = JSON.stringify(config);
  assert.deepEqual(catalogModelEntries(config).map((entry) => entry.modelId), [base]);
  assert.equal(catalogModelEntries(config)[0].published, true);
  assert.equal(JSON.stringify(config), before);
});
