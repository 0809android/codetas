import type {
  ProjectInspection,
  SyncAction,
  SyncPlan,
  SyncPreference,
} from "./types";

export function createSyncPlan(
  project: ProjectInspection,
  preference: SyncPreference,
): SyncPlan {
  const actions: SyncAction[] = [];

  if (preference.context && project.contextFile) {
    actions.push({
      id: "load-project-context",
      category: "context",
      source: project.contextFile,
      target: "Codex SessionStart context",
      summary: "Hermesのプロジェクト指示をセッション開始時に読み込みます。",
      compatibility: "ready",
      readOnly: true,
    });
  }

  if (preference.skills && project.skillsDirectory) {
    actions.push({
      id: "review-compatible-skills",
      category: "skills",
      source: project.skillsDirectory,
      target: "Codex plugin skills",
      summary: `${project.skillsCount}件のスキルを互換性レビューの対象にします。`,
      compatibility: "review",
      readOnly: true,
    });
  }

  if (preference.mcp && project.mcpFile) {
    actions.push({
      id: "review-mcp-connections",
      category: "mcp",
      source: project.mcpFile,
      target: "Codex plugin MCP configuration",
      summary: "接続定義を変換候補にし、秘密情報はコピーしません。",
      compatibility: "review",
      readOnly: true,
    });
  }

  return {
    version: 1,
    projectId: project.id,
    provider: "hermes",
    createdAt: new Date().toISOString(),
    actions,
    warnings: [...project.warnings],
    sourceMutation: false,
  };
}
