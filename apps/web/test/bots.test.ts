import assert from "node:assert/strict";
import test from "node:test";
import { chatCompletedText, chatEventText, consumeChatSse } from "../src/bot-chat.ts";
import {
  appendBotDelta,
  beginBotTurn,
  copyTextFromBotMessage,
  deleteBot,
  endBotTurn,
  finishBotError,
  finishBotReply,
  loadBots,
  saveBots,
  state,
  type Bot,
} from "../src/state.ts";

class MemoryStorage {
  store = new Map<string, string>();
  getItem(key: string): string | null {
    return this.store.has(key) ? this.store.get(key)! : null;
  }
  setItem(key: string, value: string): void {
    this.store.set(key, value);
  }
  removeItem(key: string): void {
    this.store.delete(key);
  }
}

function bot(overrides: Partial<Bot> = {}): Bot {
  return {
    id: "bot-1",
    name: "Bot",
    model: "local/model",
    instructions: "stay concise",
    messages: [{ role: "user", content: "hello" }],
    createdAt: 1,
    updatedAt: 2,
    collapsed: false,
    ...overrides,
  };
}

test("loadBots migrates legacy chat sessions into persistent bots", () => {
  const storage = new MemoryStorage();
  (globalThis as { localStorage?: MemoryStorage }).localStorage = storage;
  storage.setItem("codetas.chatSessions.v1", JSON.stringify([{
    id: "legacy-1",
    title: "Old chat",
    messages: [{ role: "user", content: "hello" }],
    createdAt: 1,
    updatedAt: 2,
  }]));

  const bots = loadBots();
  assert.equal(bots.length, 1);
  assert.equal(bots[0]?.id, "legacy-1");
  assert.equal(bots[0]?.name, "Old chat");
  assert.equal(bots[0]?.messages[0]?.content, "hello");
  assert.equal(bots[0]?.collapsed, false);
  assert.ok(storage.getItem("codetas.bots.v2"));
});

test("loadBots ignores malformed persisted bots", () => {
  const storage = new MemoryStorage();
  (globalThis as { localStorage?: MemoryStorage }).localStorage = storage;
  storage.setItem("codetas.bots.v2", JSON.stringify([
    { id: "ok", name: "Bot", model: null, instructions: "", messages: [], createdAt: 1, updatedAt: 2, collapsed: true },
    { id: 3, name: "bad" },
  ]));

  const bots = loadBots();
  assert.equal(bots.length, 1);
  assert.equal(bots[0]?.id, "ok");
  assert.equal(bots[0]?.collapsed, true);
});

test("saveBots round-trips a persistent bot session", () => {
  const storage = new MemoryStorage();
  (globalThis as { localStorage?: MemoryStorage }).localStorage = storage;
  saveBots([bot({ collapsed: true })]);
  const loaded = loadBots();
  assert.equal(loaded.length, 1);
  assert.equal(loaded[0]?.id, "bot-1");
  assert.equal(loaded[0]?.model, "local/model");
  assert.equal(loaded[0]?.instructions, "stay concise");
  assert.equal(loaded[0]?.collapsed, true);
  assert.equal(loaded[0]?.messages[0]?.content, "hello");
});

test("chatEventText keeps SSE deltas and does not replay completed text", () => {
  assert.equal(chatEventText({ type: "response.output_text.delta", delta: "Hel" }), "Hel");
  assert.equal(chatEventText({ type: "response.output_text.done", text: "Hello" }), "");
  assert.equal(chatCompletedText({ type: "response.output_text.done", text: "Hello" }), "Hello");
});

test("consumeChatSse flushes a trailing event without a final newline", () => {
  const first = consumeChatSse("", 'data: {"type":"response.output_text.delta","delta":"Hel"}\n', false);
  assert.equal(first.text, "Hel");
  const second = consumeChatSse(first.buffer, 'data: {"type":"response.output_text.delta","delta":"lo"}', true);
  assert.equal(second.text, "lo");
});

test("consumeChatSse uses completed text only when no deltas arrived", () => {
  const onlyDone = consumeChatSse("", 'data: {"type":"response.output_text.done","text":"Hello"}\n', true);
  assert.equal(onlyDone.text, "");
  assert.equal(onlyDone.completed, "Hello");
  const withDelta = consumeChatSse("", 'data: {"type":"response.output_text.delta","delta":"Hel"}\ndata: {"type":"response.output_text.done","text":"Hello"}\n', true);
  assert.equal(withDelta.text, "Hel");
  assert.equal(withDelta.completed, "Hello");
});

function resetBotState(storage: MemoryStorage): void {
  (globalThis as { localStorage?: MemoryStorage }).localStorage = storage;
  state.bots = [];
  state.botInputs = {};
  state.botSending = new Set();
  state.botAborts = {};
}

test("deleteBot during an in-flight turn does not resurrect the bot", () => {
  const storage = new MemoryStorage();
  resetBotState(storage);
  const active = bot();
  state.bots = [active];
  state.botInputs[active.id] = "hello";
  beginBotTurn(active, "hello");
  assert.equal(state.botSending.has(active.id), true);
  assert.equal(loadBots().length, 1);

  deleteBot(active.id);
  finishBotReply(active, "should not persist", "error");
  endBotTurn(active.id);

  assert.equal(state.bots.length, 0);
  assert.equal(loadBots().length, 0);
  assert.equal(state.botSending.has(active.id), false);
});

test("deleteBot aborts the in-flight request", () => {
  const storage = new MemoryStorage();
  resetBotState(storage);
  let aborted = false;
  const active = bot();
  state.bots = [active];
  state.botAborts[active.id] = { abort() { aborted = true; } } as AbortController;
  state.botSending.add(active.id);
  deleteBot(active.id);
  assert.equal(aborted, true);
  assert.equal(state.botAborts[active.id], undefined);
});

test("finishBotError writes a stopped message only while the bot still exists", () => {
  const storage = new MemoryStorage();
  resetBotState(storage);
  const active = bot({ messages: [
    { role: "user", content: "hello" },
    { role: "assistant", content: "" },
  ] });
  state.bots = [active];
  finishBotError(active, "Stopped.", true);
  assert.equal(active.messages.at(-1)?.content, "Stopped.");
  deleteBot(active.id);
  finishBotError(active, "Stopped again.", true);
  assert.equal(loadBots().length, 0);
});

test("appendBotDelta streams into the last assistant message", () => {
  const storage = new MemoryStorage();
  resetBotState(storage);
  const active = bot({ messages: [
    { role: "user", content: "hello" },
    { role: "assistant", content: "" },
  ] });
  state.bots = [active];
  appendBotDelta(active, "Hel");
  appendBotDelta(active, "lo");
  assert.equal(active.messages.at(-1)?.content, "Hello");
  deleteBot(active.id);
  appendBotDelta(active, "!");
  assert.equal(active.messages.at(-1)?.content, "Hello");
});

test("copyTextFromBotMessage reads the rendered paragraph, not a data attribute", () => {
  const paragraph = { textContent: "hello & world" };
  const article = { querySelector(selector: string) { return selector === "p" ? paragraph : null; } };
  const button = { closest(selector: string) { return selector === ".chat-message" ? article : null; } };
  assert.equal(copyTextFromBotMessage(button as unknown as HTMLElement), "hello & world");
});

test("beginBotTurn persists the user turn without an empty assistant placeholder", () => {
  const storage = new MemoryStorage();
  resetBotState(storage);
  const active = bot({ messages: [] });
  state.bots = [active];
  beginBotTurn(active, "hello");
  assert.equal(active.messages.length, 2);
  assert.equal(active.messages.at(-1)?.role, "assistant");
  assert.equal(active.messages.at(-1)?.content, "");
  const loaded = loadBots();
  assert.equal(loaded[0]?.messages.length, 1);
  assert.equal(loaded[0]?.messages[0]?.role, "user");
  assert.equal(loaded[0]?.messages[0]?.content, "hello");
});

test("endBotTurn does not persist a still-empty assistant bubble", () => {
  const storage = new MemoryStorage();
  resetBotState(storage);
  const active = bot({ messages: [] });
  state.bots = [active];
  beginBotTurn(active, "hello");
  endBotTurn(active.id);
  const loaded = loadBots();
  assert.equal(loaded[0]?.messages.length, 1);
  assert.equal(loaded[0]?.messages.at(-1)?.role, "user");
});

test("loadBots drops a trailing empty assistant left by an interrupted stream", () => {
  const storage = new MemoryStorage();
  resetBotState(storage);
  saveBots([bot({
    messages: [
      { role: "user", content: "hello" },
      { role: "assistant", content: "" },
    ],
  })]);
  const loaded = loadBots();
  assert.equal(loaded.length, 1);
  assert.equal(loaded[0]?.messages.length, 1);
  assert.equal(loaded[0]?.messages[0]?.content, "hello");
});
