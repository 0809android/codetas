import assert from "node:assert/strict";
import test from "node:test";
import { clearRetiredBotStorage } from "../src/retired-storage.ts";

test("retired storage cleanup removes both Bot formats and preserves unrelated settings", () => {
  const data = new Map([
    ["codetas.bots.v2", "private bot conversations"],
    ["codetas.chatSessions.v1", "legacy conversations"],
    ["codetas.language", "ja"],
    ["unrelated", "keep"],
  ]);
  const storage = { removeItem(key: string) { data.delete(key); } };
  clearRetiredBotStorage(storage);
  clearRetiredBotStorage(storage);
  assert.deepEqual([...data], [["codetas.language", "ja"], ["unrelated", "keep"]]);
});

test("inaccessible storage does not prevent application startup", () => {
  assert.doesNotThrow(() => clearRetiredBotStorage({ removeItem() { throw new Error("denied"); } }));
});
