import assert from "node:assert/strict";
import test from "node:test";
import type { CatalogDisplayNameFormat, GatewayConfiguration } from "@codetas/core";
import { catalogModelDisplayName, codexPublicModelSlug } from "../src/format.ts";

test("API and subscription labels remain distinct even with identical custom names", () => {
  const config = {
    providers: [
      { id: "openai", name: "Same name", credential: { source: "forward" } },
      { id: "openai-api", name: "Same name", credential: { source: "keychain" } },
      { id: "openai-apikey", name: "Same name", credential: { source: "environment" } },
      { id: "anthropic", name: "Same name", credential: { source: "oAuth" } },
      { id: "anthropic-apikey", name: "Same name", credential: { source: "keychain" } },
    ],
    catalog: {},
  } as unknown as GatewayConfiguration;
  for (const format of ["default", "modelId", "providerModel", "providerIdModel", "custom"] satisfies CatalogDisplayNameFormat[]) {
    config.catalog.displayNameFormat = format;
    for (const [providerId, label] of [
      ["openai", "サブスク"], ["openai-api", "API"], ["openai-apikey", "API"],
      ["anthropic", "サブスク"], ["anthropic-apikey", "API"],
    ]) {
      const name = catalogModelDisplayName(config, { providerId: providerId!, modelId: "same-model", displayName: "Custom" });
      assert.ok(name.endsWith(`[${label}]`), name);
      assert.ok(name.includes("Custom"), name);
    }
  }
  assert.equal(codexPublicModelSlug("openai", "gpt-test"), "gpt-test");
  assert.equal(codexPublicModelSlug("openai-api", "gpt-test"), "openai-api/gpt-test");
});

test("credential changes update labels without guessing unrelated OAuth billing", () => {
  const config = {
    providers: [{ id: "anthropic", name: "Claude", credential: { source: "environment" } }],
    catalog: {},
  } as unknown as GatewayConfiguration;
  const entry = { providerId: "anthropic", modelId: "claude-test", displayName: null };
  assert.ok(catalogModelDisplayName(config, entry).endsWith("[API]"));
  config.providers[0]!.credential.source = "oAuth";
  assert.ok(catalogModelDisplayName(config, entry).endsWith("[サブスク]"));
  config.providers[0]!.id = "google";
  assert.equal(catalogModelDisplayName(config, { ...entry, providerId: "google" }), "Claude claude-test");
});
