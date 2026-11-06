import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { codexRestoreOutcome } from "../src/codex-restore.ts";

test("conflicts are never reported as nothing to restore", () => {
  assert.equal(codexRestoreOutcome({ restored: false, conflicts: ["model was changed"] }), "conflicts");
  assert.equal(codexRestoreOutcome({ restored: false, conflicts: [] }), "nothingToRestore");
  assert.equal(codexRestoreOutcome({ restored: true, conflicts: [] }), "restored");
});

test("the disconnect handler keeps the Gateway running when conflicts remain", () => {
  const actions = readFileSync(new URL("../src/actions.ts", import.meta.url), "utf8");
  const start = actions.indexOf('case "confirm-restore-codex":');
  const handler = actions.slice(start, actions.indexOf('case "sync-catalog":', start));

  const conflictBranch = handler.indexOf('outcome === "conflicts"');
  const stop = handler.indexOf('"stop_provider_gateway"');
  assert.ok(conflictBranch > 0, "the handler checks for conflicts");
  assert.ok(conflictBranch < stop, "the check comes before the Gateway stops");
  const branch = handler.slice(conflictBranch, stop);
  assert.ok(branch.includes("toast.restoreConflicts"), "the conflicts are shown");
  assert.ok(branch.includes("return;"), "the handler leaves before stopping");
  assert.ok(!branch.includes("toast.restoreNone"), "conflicts are not called nothing to restore");
});

test("the restore toasts exist in both languages", () => {
  const i18n = readFileSync(new URL("../src/i18n.ts", import.meta.url), "utf8");
  for (const key of ["toast.restoreConflicts", "toast.restoreNone", "toast.codexDisconnected"]) {
    assert.equal(i18n.split(`"${key}":`).length - 1, 2, key);
  }
  assert.ok(!i18n.includes("toast.conflictSuffix"));
});

test("a Codex problem shows on every page, not only on Connections", () => {
  const main = readFileSync(new URL("../src/main.ts", import.meta.url), "utf8");
  const views = readFileSync(new URL("../src/views.ts", import.meta.url), "utf8");
  assert.ok(main.includes("${renderCodexProblem()}"));
  const start = views.indexOf("export function renderConnections");
  const connections = views.slice(start, views.indexOf("export function", start + 1));
  assert.ok(!connections.includes("fallbackReconnectError"));
});

test("tray and startup failures reach the banner instead of stderr alone", () => {
  const desktop = readFileSync(new URL("../../desktop/src-tauri/src/main.rs", import.meta.url), "utf8");
  const tray = desktop.slice(desktop.indexOf('"codetas-start-gateway" => {'), desktop.indexOf('"codetas-quit" => {'));
  for (const command of ["start_provider_gateway", "install_codex_gateway_config", "stop_provider_gateway"]) {
    const call = tray.indexOf(`gateway_ops::${command}(`);
    assert.ok(call > 0, command);
    assert.ok(tray.slice(call - 80, call).includes("report_codex_problem("), command);
  }
  assert.ok(!tray.includes("let _ ="), "no result is thrown away");

  const startup = desktop.slice(desktop.indexOf("settings.runtime.auto_start"), desktop.indexOf("settings.codex.auto_connect"));
  assert.ok(startup.includes("report_codex_problem("));
});
