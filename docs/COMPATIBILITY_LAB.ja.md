# Compatibility Lab とポリシールーティング

この文書は [英語版](COMPATIBILITY_LAB.md) の日本語運用ガイドです。設定項目の
完全な仕様は英語版を正とし、この文書では管理画面と運用時の確認点を説明します。

CODETAS はプロバイダー互換性をモデル名の推測ではなく、実行可能な fixture として
管理します。CI は各プロバイダープリセットの正例・負例を Responses、Chat
Completions、Anthropic Messages、Gemini generateContent の対応アダプターへ通します。
管理画面の「Compatibility Lab」と `GET /v1/compatibility` は、その読み取り専用の
結果表です。

`GET /v1/routes/dry-run?model=<route>` は、次に選ばれる候補と、除外された候補を
順位付きで表示します。除外理由には、能力不足、価格上限、無効・一時停止中の
アカウント、cooldown、quota が含まれます。dry-run は routing runtime の snapshot
だけを評価し、round-robin、使用量、失敗、quota、cooldown の実状態を変更しません。

## 公開カタログ

- `catalog.selectedModels` は公開 allowlist です。空なら生成可能な全モデルを公開します。
- `catalog.modelPickerOrder` はモデル選択画面の安定した表示順です。
- OpenAI の `gpt-*` と `openai/gpt-*` は、Codex のネイティブ slug と標準 API の
  qualified ID を表す同一モデルとして解決します。route ID と route alias は完全一致です。
- `/healthz` はプロセスの生存確認です。`/readyz` は、有効なプロバイダー、使用可能な
  default provider、同期済みで空でない公開カタログも要求します。

## リクエストとアカウント運用

- request pacing はプロバイダー単位の共有キューで upstream 開始時刻を間隔化します。
  429 retry の回数・待機ポリシーとは独立しています。
- pin されたアカウントは手動選択として最優先です。利用不能なら priority tier に戻ります。
- quota、round-robin、fill-first の並び替えは同じ priority tier 内だけに適用されます。
  低い tier は高い tier が利用不能になった場合の failover です。
- API キーは設定ファイルへ保存せず、環境変数、keychain、OAuth、所有権付き command
  などの参照として管理します。

## メモリと管理操作

`runtime.memoryBudgetBytes` と `runtime.maxInflightRequests` は推論リクエストの admission
を制限します。`GET /v1/management/memory` は本文を含まない使用状況だけを返します。
embedded Gateway のメモリ予算変更は安全に再起動して実効 body limit を同期し、失敗時は
以前の設定・カタログ・runtime へ戻します。`/healthz` と `/readyz` は推論 admission の
対象外です。

field-scoped management IPC は毎回最新設定を読み、指定された field だけを更新します。
registry migration revision を設定世代のロックとしては使用しません。OpenCode、Pi、
Claude Desktop、Hermes の生成物は CODETAS が所有する field を記録し、他の設定を
読み替えたり上書きしたりしません。

Anthropic、Gemini、Kiro の署名付き推論 metadata は再送に必要な provider-owned field
だけで保持します。observability は本文、API キー、OAuth token、署名を保存しません。
