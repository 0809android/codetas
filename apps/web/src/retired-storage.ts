/** Remove only storage owned by the retired Bot feature, never other settings. */
export function clearRetiredBotStorage(storage?: Pick<Storage, "removeItem">): void {
  try {
    const target = storage ?? globalThis.localStorage;
    target.removeItem("codetas.bots.v2");
    target.removeItem("codetas.chatSessions.v1");
  } catch {
    // Restricted WebViews must still start. Retry cleanup on the next launch.
  }
}
