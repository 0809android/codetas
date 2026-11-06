/** What disconnecting did, which decides whether the Gateway may stop. */
export type CodexRestoreOutcome = "restored" | "nothingToRestore" | "conflicts";

export function codexRestoreOutcome(report: { restored: boolean; conflicts: string[] }): CodexRestoreOutcome {
  if (report.conflicts.length > 0) return "conflicts";
  return report.restored ? "restored" : "nothingToRestore";
}
