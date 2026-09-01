export function chatEventText(value: unknown): string {
  if (!value || typeof value !== "object") return "";
  const event = value as Record<string, unknown>;
  if (event.type === "response.output_text.delta" || event.type === "response.refusal.delta") {
    return typeof event.delta === "string" ? event.delta : "";
  }
  return "";
}

export function chatCompletedText(value: unknown): string {
  if (!value || typeof value !== "object") return "";
  const event = value as Record<string, unknown>;
  if (event.type === "response.output_text.done" && typeof event.text === "string") {
    return event.text;
  }
  return "";
}

export function chatResponseText(value: unknown): string {
  if (typeof value === "string") return value;
  if (Array.isArray(value)) {
    return value.map(chatResponseText).filter(Boolean).join("\n");
  }
  if (value && typeof value === "object") {
    const item = value as Record<string, unknown>;
    if (typeof item.output_text === "string" && item.output_text.trim()) return item.output_text;
    if (typeof item.text === "string" && item.text.trim()) return item.text;
    if (Array.isArray(item.content)) return chatResponseText(item.content);
    if (Array.isArray(item.output)) return chatResponseText(item.output);
  }
  return "";
}

export function consumeChatSse(
  buffer: string,
  chunk: string,
  flush: boolean,
): { buffer: string; text: string; completed: string } {
  buffer += chunk;
  const lines = buffer.split(/\r?\n/);
  buffer = flush ? "" : (lines.pop() ?? "");
  if (flush && lines.length && lines[lines.length - 1] === "") lines.pop();
  let text = "";
  let completed = "";
  for (const rawLine of lines) {
    const line = rawLine.trim();
    if (!line.startsWith("data:")) continue;
    const payload = line.slice(5).trim();
    if (!payload || payload === "[DONE]") continue;
    let event: unknown;
    try {
      event = JSON.parse(payload);
    } catch {
      continue;
    }
    const delta = chatEventText(event);
    if (delta) {
      text += delta;
      continue;
    }
    const done = chatCompletedText(event);
    if (done && !completed) completed = done;
  }
  return { buffer, text, completed };
}
