import type { MessageMap } from "../i18n";

// Korean is intentionally a partial locale. Missing messages fall back to the
// complete English catalog in i18n.ts, so adding a locale never requires
// duplicating or silently diverging the canonical message set.
export const ko: Partial<MessageMap> = {
  "nav.overview": "개요",
  "nav.maintenance": "Codex 유지관리",
  "nav.providers": "연결과 모델",
  "nav.routing": "라우팅",
  "nav.agents": "에이전트",
  "nav.projects": "프로필",
  "nav.clients": "클라이언트",
  "nav.settings": "설정",
  "shell.gateway": "게이트웨이",
  "runtime.running": "실행 중",
  "runtime.stopped": "중지됨",
  "routing.title": "라우팅",
  "routing.dryRun": "라우트 dry-run",
  "routing.compatibilityLab": "호환성 랩",
  "routing.readOnly": "읽기 전용 공급자 적합성 결과입니다.",
  "settings.gateway": "게이트웨이",
  "settings.security": "보안",
  "settings.keyPool": "API 키 풀",
  "settings.addAccount": "계정 추가",
  "settings.save": "설정 저장",
  "clients.title": "클라이언트 통합",
  "clients.generate": "통합 생성",
  "agents.modelFallbackMap": "모델별 서브에이전트 폴백",
};
