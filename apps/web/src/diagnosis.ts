import type { ObservationEvent, ProviderConnectionReport } from "@codetas/core";

export type Problem = { title: string; cause: string; action: string };
export function connectionSucceeded(report: ProviderConnectionReport): boolean {
  return report.reachable && report.authenticated && (report.status == null || report.status < 400);
}
export function explainFailure(status: number | null, detail: string): Problem {
  if (status === 401 || status === 403) return { title: "認証・アクセス権を確認", cause: "認証情報が無効、またはこのモデルへのアクセス権がありません。", action: "「接続」で対象サービスに再ログインし、契約・モデルの利用権限を確認してください。" };
  if (status === 429) return { title: "利用枠または送信頻度の制限", cause: "サービス側がリクエストを制限しました。利用枠の使い切りとは限りません。", action: "下の上流メッセージとサービスの利用枠を確認してください。リセット待ち、または別のモデル／接続先を選択できます。" };
  if (status != null && status >= 500) return { title: "接続先が応答できません", cause: "サービス側の混雑、障害、または通信エラーです。", action: "少し待って再確認してください。続く場合は「接続」でURL・ネットワークを確認してください。" };
  return { title: "接続を確認してください", cause: detail || "接続確認を完了できませんでした。", action: "「接続」でURL・認証設定を確認してください。詳細は下のメッセージを参照してください。" };
}
export function latestModelEvents(events: ObservationEvent[]): ObservationEvent[] {
  const latest = new Map<string, ObservationEvent>();
  for (const event of [...events].sort((a, b) => a.timestampMs - b.timestampMs || a.ledgerSequence - b.ledgerSequence)) {
    if (event.shadow || event.attemptOnly) continue;
    const key = event.exposedModel;
    const previous = latest.get(key);
    // Local compression/cooldown does not prove that a provider recovered.
    if (previous && event.sendCount === 0 && !event.upstreamError
      && (event.statusCode < 400 || event.failureCategory === "provider_cooling_down")) continue;
    if (!previous || previous.timestampMs < event.timestampMs || (previous.timestampMs === event.timestampMs && previous.ledgerSequence < event.ledgerSequence)) latest.set(key, event);
  }
  return [...latest.values()].sort((a, b) => b.timestampMs - a.timestampMs);
}
export function eventPurpose(event: ObservationEvent): string {
  if (event.recoveryKinds.some((kind) => kind.includes("compaction"))) return event.sendCount === 0 ? "ローカル圧縮（外部送信なし）" : "圧縮・復旧";
  if (event.shadow) return "並行送信";
  if (event.sendCount > 1) return "リクエスト（再試行あり）";
  return event.sendCount === 0 ? "ローカル処理（外部送信なし）" : "モデルへのリクエスト";
}
export function eventHasProblem(event: ObservationEvent): boolean {
  return event.statusCode >= 400 || Boolean(event.upstreamError);
}

/** A provider estimate from a dated log, not a promise of access recovery. */
export function quotaResetAt(event: ObservationEvent): number | null {
  if ((event.upstreamError?.statusCode ?? event.statusCode) !== 429) return null;
  const match = /Resets in (?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?\./.exec(event.upstreamError?.errorMessage ?? "");
  if (!match || !match.slice(1).some(Boolean)) return null;
  const seconds = Number(match[1] ?? 0) * 3600 + Number(match[2] ?? 0) * 60 + Number(match[3] ?? 0);
  const deadline = event.timestampMs + seconds * 1000;
  return Number.isFinite(deadline) && seconds <= 365 * 86400 ? deadline : null;
}
