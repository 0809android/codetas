# Compatibility Lab 운영 요약

이 문서는 Compatibility Lab의 짧은 한국어 운영 안내입니다. 전체 설정 계약과 최신
세부 사항은 [영문 문서](COMPATIBILITY_LAB.md)를 기준으로 하며, 이 번역은 완전 번역이
아닙니다.

- Compatibility Lab과 `GET /v1/compatibility`는 공급자별 positive/negative fixture 결과를
  읽기 전용으로 표시합니다.
- route dry-run은 다음 선택 대상, 후보 순위, 제외 이유를 표시하며 실제 round-robin,
  quota, cooldown, failure 상태를 변경하지 않습니다.
- `selectedModels`는 공개 allowlist이고 `modelPickerOrder`는 picker 순서입니다. OpenAI의
  native `gpt-*`와 qualified `openai/gpt-*`는 같은 모델로 해석하지만 route alias는 정확히
  일치해야 합니다.
- `/healthz`는 liveness, `/readyz`는 provider/default/catalog 동기화까지 확인하는 readiness입니다.
- request pacing은 provider 공유 큐이며 429 retry 정책과 분리됩니다. account 전략은 같은
  priority tier 안에서만 순서를 바꾸고 낮은 tier는 failover로 사용합니다.
- 메모리 관리 API와 observability는 요청 본문, API key, OAuth token, reasoning signature를
  저장하지 않습니다.
