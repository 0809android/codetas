import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { navigation, state } from "../src/state.ts";

test("navigation and state no longer expose persistent bots", () => {
  assert.equal(navigation.length, 4);
  assert.ok(!navigation.some((entry) => String(entry.id) === "bots"));
  for (const key of ["bots", "botInputs", "botSending", "botAborts"]) {
    assert.ok(!(key in state));
  }
  for (const id of ["connections", "models", "status", "settings"]) {
    assert.ok(navigation.some((entry) => entry.id === id));
  }
});

test("application state no longer reads or persists bot data", () => {
  const source = readFileSync(new URL("../src/state.ts", import.meta.url), "utf8");
  assert.ok(!source.includes("localStorage"));
});

test("profile management is absent while project guidance remains", () => {
  const views = readFileSync(new URL("../src/views.ts", import.meta.url), "utf8");
  const actions = readFileSync(new URL("../src/actions.ts", import.meta.url), "utf8");
  const desktop = readFileSync(new URL("../../desktop/src-tauri/src/main.rs", import.meta.url), "utf8");
  for (const command of ["list_hermes_profiles", "convert_hermes_profiles", "scan_hermes_sync", "save_hermes_editable_file"]) {
    assert.ok(!actions.includes(command));
    assert.ok(!desktop.includes(command));
  }
  for (const key of ["hermesProfiles", "hermesSyncInventory", "hermesEditableFiles", "hermesFileDrafts"]) {
    assert.ok(!(key in state));
  }
  assert.ok(!views.includes("profiles-form"));
  assert.ok(actions.includes('data.get("loadHermesContext")'));
  assert.ok(!desktop.includes("set_codex_skill_enabled"));
  assert.ok(!desktop.includes("save_codex_context_document"));
});

test("diagnostic log API is registered and application skill editors are not", () => {
 const desktop=readFileSync(new URL("../../desktop/src-tauri/src/main.rs",import.meta.url),"utf8");
 const maintenance=readFileSync(new URL("../../desktop/src-tauri/src/maintenance.rs",import.meta.url),"utf8");
 assert.ok(desktop.includes("provider_gateway::diagnostics::gateway_recent_events,"));
 for(const name of ["set_codex_skill_enabled","save_codex_context_document","scan_user_skills","context_load_diagnostics"]){assert.ok(!desktop.includes(name));assert.ok(!maintenance.includes(name));}
});
