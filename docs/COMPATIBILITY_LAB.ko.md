# Compatibility Lab 운영 요약

이 문서는 Compatibility Lab의 짧은 한국어 운영 안내입니다. 전체 설정 계약과 최신
세부 사항은 [영문 문서](COMPATIBILITY_LAB.md)를 기준으로 하며, 이 번역은 완전 번역이
아닙니다.

- Compatibility Lab과 `GET /v1/compatibility`는 공급자별 positive/negative fixture 결과를
  읽기 전용 `pass`/`fail`/`skip`으로 표시합니다. 현재 설정의 pure adapter/repair/pacing
  fixture만 실행하며 production upstream probe는 수행하지 않습니다.
- route dry-run은 다음 선택 대상, 후보 순위, 제외 이유를 표시하며 실제 round-robin,
  quota, cooldown, failure 상태를 변경하지 않습니다.
- `selectedModels`는 공개 allowlist이고 `modelPickerOrder`는 picker 순서입니다. OpenAI의
  native `gpt-*`와 qualified `openai/gpt-*`는 같은 모델로 해석하지만 route alias는 정확히
  일치해야 합니다.
- `/healthz`는 liveness, `/readyz`는 provider/default/catalog 동기화까지 확인하는 readiness입니다.
- request pacing은 provider 공유 큐이며 429 retry 정책과 분리됩니다. account 전략은 같은
  priority tier 안에서만 순서를 바꾸고 낮은 tier는 failover로 사용합니다.
- 생략된 고급 capability는 custom provider에서 안전하게 false로 처리되며, tool 하위
  capability는 기본 `tools` capability가 꺼져 있으면 활성화할 수 없습니다.
- 메모리 admission은 압축된 Content-Length가 아니라 디코딩된 stream의 실제 바이트를
  원자적으로 예약합니다. 디코딩된 JSON이 body limit를 넘으면 오래된 inline 이미지를
  path marker로 바꾼 뒤 limit를 다시 확인하고, 그래도 넘을 때만 HTTP 413을 반환합니다.
  JSON 작업 영역의 3배 예약은 그 rewrite 이후에만 적용합니다. Anthropic EOF 허용도 완전한 마지막 SSE frame만 처리하고 잘린
  JSON은 오류로 반환합니다.
- admission 예약은 SSE body가 완료되거나 오류/Drop될 때까지 유지됩니다. Responses
  snapshot repair는 added item과 delta를 병합하고 terminal frame만 다시 직렬화합니다.
- 메모리 관리 API와 observability는 요청 본문, API key, OAuth token, reasoning signature를
  저장하지 않습니다.
