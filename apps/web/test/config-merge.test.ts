import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { mergeConfigurationChanges } from "../src/config-merge.ts";

const base = {
  runtime: { port: 42421, autoStart: true },
  codex: { autoConnect: true, autoSyncCatalog: true },
  providers: [{ id: "openai" }],
  catalog: { selectedModels: ["a"] },
};

function changed(edit: (config: typeof base) => void): typeof base {
  const mine = structuredClone(base);
  edit(mine);
  return mine;
}

test("a save does not undo a disconnect the form never saw", () => {
  const latest = changed((config) => { config.codex.autoConnect = false; });
  const mine = changed((config) => { config.runtime.port = 50000; });

  const saved = mergeConfigurationChanges(base, mine, latest);

  assert.equal(saved.codex.autoConnect, false);
  assert.equal(saved.runtime.port, 50000);
});

test("a save does not drop a connection adopted after the form was drawn", () => {
  const latest = changed((config) => { config.providers.push({ id: "adopted" }); });
  const mine = changed((config) => { config.catalog.selectedModels = ["b"]; });

  const saved = mergeConfigurationChanges(base, mine, latest);

  assert.deepEqual(saved.providers.map((provider) => provider.id), ["openai", "adopted"]);
  assert.deepEqual(saved.catalog.selectedModels, ["b"]);
});

test("what the user changed wins over the file", () => {
  const latest = changed((config) => { config.codex.autoConnect = false; });
  const mine = changed((config) => { config.codex.autoConnect = false; config.codex.autoSyncCatalog = false; });
  assert.equal(mergeConfigurationChanges(base, mine, latest).codex.autoSyncCatalog, false);

  const turnedOn = changed((config) => { config.runtime.autoStart = false; });
  const file = changed((config) => { config.runtime.autoStart = true; config.runtime.port = 1; });
  const saved = mergeConfigurationChanges(base, turnedOn, file);
  assert.equal(saved.runtime.autoStart, false);
  assert.equal(saved.runtime.port, 1);
});

test("an unchanged form saves exactly the file", () => {
  const latest = changed((config) => { config.codex.autoConnect = false; config.providers = []; });
  assert.deepEqual(mergeConfigurationChanges(base, structuredClone(base), latest), latest);
});

test("editing one connection keeps a connection adopted meanwhile", () => {
  const latest = changed((config) => { config.providers.push({ id: "adopted" }); });
  const mine = changed((config) => { config.providers[0] = { id: "openai", enabled: false } as never; });

  const saved = mergeConfigurationChanges(base, mine, latest);

  assert.deepEqual(saved.providers, [{ id: "openai", enabled: false }, { id: "adopted" }]);
});

test("the user's removal wins, and an item removed elsewhere stays removed unless edited", () => {
  const latest = changed((config) => { config.providers.push({ id: "adopted" }); });
  const removedMine = changed((config) => { config.providers = []; });
  assert.deepEqual(mergeConfigurationChanges(base, removedMine, latest).providers, [{ id: "adopted" }]);

  const removedElsewhere = changed((config) => { config.providers = []; });
  const untouched = changed((config) => { config.providers.push({ id: "mine" }); });
  assert.deepEqual(mergeConfigurationChanges(base, untouched, removedElsewhere).providers, [{ id: "mine" }]);
  const edited = changed((config) => { config.providers[0] = { id: "openai", enabled: false } as never; });
  assert.deepEqual(mergeConfigurationChanges(base, edited, removedElsewhere).providers, [{ id: "openai", enabled: false }]);
});

test("accounts merge by provider and id", () => {
  const accounts = (...items: Array<[string, string, number]>) =>
    items.map(([providerId, id, priority]) => ({ providerId, id, priority }));
  const start = { accountPool: { accounts: accounts(["kimi", "a", 0], ["zai", "a", 0]) } };
  const mine = { accountPool: { accounts: accounts(["kimi", "a", 5], ["zai", "a", 0]) } };
  const latest = { accountPool: { accounts: accounts(["kimi", "a", 0], ["zai", "a", 0], ["kimi", "b", 0]) } };
  assert.deepEqual(
    mergeConfigurationChanges(start, mine, latest).accountPool.accounts,
    accounts(["kimi", "a", 5], ["zai", "a", 0], ["kimi", "b", 0]),
  );
});

test("an array without ids is one value, and a removed key stays removed", () => {
  const latest = changed((config) => { config.catalog.selectedModels = ["a", "c"]; });
  const mine = changed((config) => { config.catalog.selectedModels = []; });
  assert.deepEqual(mergeConfigurationChanges(base, mine, latest).catalog.selectedModels, []);

  const removed = structuredClone(base) as Partial<typeof base>;
  delete removed.catalog;
  assert.ok(!("catalog" in mergeConfigurationChanges<Partial<typeof base>>(base, removed, base)));
});

test("every whole-file save goes through the merge, and the window keeps up", () => {
  const actions = readFileSync(new URL("../src/actions.ts", import.meta.url), "utf8");
  const saves = actions.split('invoke<GatewayConfiguration>("save_gateway_configuration"').slice(1);
  assert.ok(saves.length >= 2);
  for (const save of saves) {
    assert.ok(save.slice(0, 120).includes("configuration: await mergeOnLatest("), save.slice(0, 120));
  }

  const start = actions.indexOf("async function autoSyncConnectionStatus");
  const autoSync = actions.slice(start, actions.indexOf("export function startConnectionAutoSync", start));
  assert.ok(autoSync.includes('invoke<GatewayConfiguration>("gateway_configuration")'));
  const refresh = autoSync.indexOf('invoke<GatewayConfiguration>("gateway_configuration")');
  assert.ok(autoSync.lastIndexOf("!editing", refresh) >= 0, "only when no form is being edited");
});
