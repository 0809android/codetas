# モデル混雑・上流応答失敗の診断

通常の `observability/events-*.jsonl` に、本文を含まない診断を追加する。
`usageLog=true` なら `requestLog=false` のままで保存される。
両方無効の場合は保存されない。既存の保存期間・容量制限に従う。

## `requestDiagnostic`

通常のHTTP上流送信に付く（特殊transportは対象外）。

- `clientThreadId`: クライアントのsession/threadヘッダーから取得したUUID。
  任意文字列は保存しない。ヘッダーがなければnull。
- `wireBytes`: 変換・sanitize後、実際にHTTP送信したJSONのバイト数。
- `inputItems` / `toolCount`: 送信JSONの入力要素数・トップレベルツール数。
- `store`: 明示された値。省略時はnullで、trueと推定しない。
- `hasPreviousResponseId`: 上流送信時に継続IDが存在したか。IDの値は保存しない。
- `clientTransport` / `upstreamTransport`: WebSocket→HTTPの変換も区別する。
- `websocketHistoryReplayed`: このWebSocket入力でローカル履歴mergeが
  成功したか。HTTP入力はnull。falseはmerge missや継続IDなしも含む。
- `upstreamStatusCode` / `upstreamRequestId`: 実際のHTTP応答ステータスと
  上流request ID。Gatewayが分類したイベント全体のstatusCodeとは別。

既存の `requestId`、モデル、latency、sendCount、recoveryKindsと突合する。
再試行で応答を再構築した場合、診断は最初の応答の送信条件を示すことがある。
全試行の本文や送信条件の配列を保存するものではない。

## SSE終端の `upstreamError`

Responsesのfailed/incomplete終端について、既知の安全なerror code/type、
混雑の定型文、上流request ID、エラーオブジェクトのサイズとSHA256を記録する。
HTTP 200のストリーム内失敗も保存する。イベント全体が502でも
`upstreamError.statusCode=200` ならHTTP 502を受けた意味ではない。

会話本文・ツール引数・認証ヘッダー・response全体は保存しない。
上流の自由文エラーも入力や秘密情報を反映し得るため保存しない。
未知のcode/typeも省略し、SHA256で同一性を比較する。
この制限は今回追加したSSE診断に適用され、既存HTTPエラー診断を
変更するものではない。

## 切り分け

1. clientThreadId/requestIdと時刻で対象イベントを特定する。
2. 上流HTTP失敗か、HTTP 200内のSSE失敗かを区別する。
3. 直前の成功とwireBytes、履歴再送、store、継続ID有無を比較する。
4. timestampMs（完了時刻）とlatencyMsから送信区間を概算し、重なりを見る。
5. 同じ上流request IDと失敗codeを使って上流側の調査につなげる。

この記録だけで「混雑の原因はCODETAS」と断定はできない。
実行中アプリには更新済みバイナリの反映が必要。診断追加のために
CODETAS本体を自動終了・再起動しない。
