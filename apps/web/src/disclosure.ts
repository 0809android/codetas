const STORAGE_KEY = "codetas.disclosure.v1";

function read(): Record<string, boolean> {
  try {
    const parsed: unknown = JSON.parse(globalThis.localStorage?.getItem(STORAGE_KEY) ?? "{}");
    return parsed && typeof parsed === "object" ? parsed as Record<string, boolean> : {};
  } catch {
    return {};
  }
}

/** Accordion open/closed state, kept per viewer so re-renders and restarts keep it. */
export function isDisclosureOpen(key: string): boolean {
  return read()[key] === true;
}

export function setDisclosureOpen(key: string, open: boolean): void {
  try {
    const next = read();
    if (open) next[key] = true; else delete next[key];
    globalThis.localStorage?.setItem(STORAGE_KEY, JSON.stringify(next));
  } catch {
    // Restricted WebViews still render; the accordion just starts closed.
  }
}

const DISMISS_KEY = "codetas.dismissedWarnings.v1";

function readDismissed(): Record<string, string> {
  try {
    const parsed: unknown = JSON.parse(globalThis.localStorage?.getItem(DISMISS_KEY) ?? "{}");
    return parsed && typeof parsed === "object" ? parsed as Record<string, string> : {};
  } catch {
    return {};
  }
}

/** A dismissed warning stays hidden until its version (alert sequence, quota reset time) changes. */
export function isWarningDismissed(key: string, version: string): boolean {
  return readDismissed()[key] === version;
}

export function dismissWarning(key: string, version: string): void {
  try {
    const next = readDismissed();
    next[key] = version;
    const keys = Object.keys(next);
    for (const old of keys.slice(0, Math.max(0, keys.length - 200))) delete next[old];
    globalThis.localStorage?.setItem(DISMISS_KEY, JSON.stringify(next));
  } catch {
    // Without storage the warning simply reappears after the next refresh.
  }
}

export function clearDismissal(key: string): void {
  try {
    const next = readDismissed();
    if (!(key in next)) return;
    delete next[key];
    globalThis.localStorage?.setItem(DISMISS_KEY, JSON.stringify(next));
  } catch {
    // Nothing persisted, nothing to clear.
  }
}

const LOW_QUOTA_KEY = "codetas.lowQuotaActive.v1";

/** Quota windows already reported as low and not yet seen recovered; survives restarts. */
export function readLowQuotaActive(): Set<string> {
  try {
    const parsed: unknown = JSON.parse(globalThis.localStorage?.getItem(LOW_QUOTA_KEY) ?? "[]");
    return new Set(Array.isArray(parsed) ? parsed.filter((item): item is string => typeof item === "string") : []);
  } catch {
    return new Set();
  }
}

export function writeLowQuotaActive(active: Set<string>): void {
  try { globalThis.localStorage?.setItem(LOW_QUOTA_KEY, JSON.stringify([...active])); } catch { /* in-memory state still applies */ }
}
