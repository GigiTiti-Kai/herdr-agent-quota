# 第三者モデル用メーター（DeepSeek / OpenRouter）

Date: 2026-09-24
Status: 設計承認済み、spec レビュー待ち
Fork: `GigiTiti-Kai/herdr-agent-quota`（branch `feat/third-party-meters`）＋ `GigiTiti-Kai/dotfiles`

## 問題

Claude Code のハーネスで DeepSeek（`claude-ds`）や OpenRouter（`claude-or`）のモデルを動かすと、
サイドバーと Claude Code の statusLine の両方に Claude サブスクの 5h / 7d / Fab が出る。
使っているのは Claude の枠ではないので、この数字はそのペインにとって無意味。

代わりに出すもの（ユーザー決定、2026-09-24）:

| 行 | 意味 |
| --- | --- |
| `bal` | 残高。ゲージ付き。満タン＝最後に残高が増えた（チャージした）直後の値 |
| `day` / `mon` | 今日・今月の使用額（日本時間の日付・月） |
| `ses` | このセッションの使用額（Claude Code のセッション ID 単位） |

対象は `claude-ds` と `claude-or` だけ。ローカル LLM は撤去済み（dotfiles PR #410）。

## 実測（2026-09-24、すべてこのマシン）

- **キーの所在**: dotfiles PR #409 以降、DeepSeek のキーは `claude` の env に無い。セッション専用ポートの
  中継（`bin/claude-or-proxy.py`）だけが stdin で受け取って持つ。statusLine の collector は `claude` の子なので、
  キーを使って残高を取ることはできない。→ **残高取得と計量は中継が担う**
- **セッション ID**: Claude Code は毎リクエストに `X-Claude-Code-Session-Id` ヘッダを付ける
  （`claude-cli/2.1.280`、claude-ds・claude-or の両方で確認）。中継だけでセッション別に計上できる
- **OpenRouter の金額**: 応答の `message_delta.usage.cost` に実額（USD）が入る（例 `0.000797286`）。単価表は要らない
- **DeepSeek の金額**: `usage` は `input_tokens` / `cache_read_input_tokens` / `output_tokens` だけ。
  単価表で計算する。単価（USD / 1M tokens、2026-09-19 公開の公式ページ）:

  | モデル | 入力（キャッシュ命中） | 入力（キャッシュ外） | 出力 |
  | --- | --- | --- | --- |
  | `deepseek-flash`（旧名 `deepseek-v4-flash` 含む） | 0.006 | 0.3 | 1.2 |
  | `deepseek-v4-pro` | 0.044 | 1.32 | 3.96 |

  上は peak。off-peak は半額。peak は UTC の月〜金 01:00–04:00 と 06:00–10:00。中国の祝日は off-peak だが扱わない（祝日は高めに出る）
- **DeepSeek の残高**: `GET https://api.deepseek.com/user/balance` → `{"is_available", "balance_infos":[{"currency":"USD","total_balance":"<文字列>", "granted_balance", "topped_up_balance"}]}`。このアカウントは USD
- **OpenRouter の残高**: `GET /api/v1/credits` → `{"data":{"total_credits","total_usage"}}`。Management key 専用で、普通のキーは 403
  （公式ドキュメント）。Management key は BWS project `ai-billing`（`b31dd68b-5e47-47b2-ab61-b4ce011f6c24`）の
  `OPENROUTER_MANAGEMENT_KEY`、有効期限 90 日（2026-09-24 発行）
- **statusLine collector**: `herdr-agent-quota claude-statusline` が payload を cache に保存してから `node ~/.claude/statusline.js` を
  同じ stdin で呼び、末尾に Claude 5h の pace を足す（`src/configure/claude.rs:71`）

## 設計

```
claude ─→ 中継（キーを持つ唯一のプロセス）─→ DeepSeek / OpenRouter
            │ 応答ごと: ledger に1行追記、summary を更新
            │ 60 秒ごと: 残高を取得して balance を更新
            ▼
   ~/.local/state/claude-billing/
     ledger-YYYY-MM.jsonl     監査・再集計用（日本時間の月）
     summary-<backend>.json   表示用の集計（O(1) で読める）
     balance-<backend>.json   残高と満タン値
            ▲ 読むだけ（通信しない・キーを持たない）
   herdr-agent-quota（サイドバー）   statusline.js（statusLine 2 行目）
```

### 1. 中継（dotfiles `bin/claude-or-proxy.py`）

- `CLAUDE_BILLING_BACKEND`（`deepseek` / `openrouter`）が設定された時だけ計量する。未設定なら今の動作のまま
- **計量**: `/v1/messages` の応答（SSE と非 SSE の両方）から最後の `usage` を拾う。セッション ID は
  リクエストの `X-Claude-Code-Session-Id`、モデルは応答の `model`
  - openrouter: `usage.cost` をそのまま使う
  - deepseek: 単価表 × トークン数。peak / off-peak は応答を受けた時刻（UTC）で決める
  - 単価表に無いモデル・`cost` が無い応答は `usd: null` として記録し、summary の `unpriced` を 1 増やす
- **ledger の1行**: `{"ts", "backend", "session", "model", "input", "cache_read", "cache_write", "output", "usd"}`
- **summary**（backend ごと）: `{"backend", "day", "day_usd", "month", "month_usd", "sessions": {"<id>": {"usd", "last"}}, "unpriced", "updated_at"}`。
  `day` / `month` は日本時間の日付と月。日付が変わった最初の書き込みで 0 から数え直す。`sessions` は最終更新から 7 日経ったものを消す
- **残高**: 起動時と以降 60 秒ごと。`balance-<backend>.json` の `fetched_at` が 60 秒以内なら取りに行かない（同時に動く中継どうしで取り合わない）
  - `{"status": "ok"|"error"|"key_expired", "balance", "full", "currency", "fetched_at", "last_ok_at"}`
  - `full` の更新: 前回より増えていたらチャージとみなして `full = balance`。減っていたら `full` は据え置き。初回は `full = balance`（ゲージは 100% から始まる）
  - openrouter の `balance` = `total_credits - total_usage`。401 は `key_expired`（Management key の期限切れ）
- **並行書き込み**: 複数セッションの中継が同じファイルを書くので、ledger 追記と summary / balance の更新は
  `fcntl.flock` の排他ロックの下で行う。summary と balance は一時ファイル → `os.replace` で置き換える
- **失敗の扱い**: 計量や残高取得で例外が出ても、応答の中継は止めない。例外はログ（launcher が stderr を逃がしている先）へ出す
- キーの受け取り: stdin の 1 行目がキー、2 行目があれば Management key（openrouter のみ）

### 2. 起動スクリプト（dotfiles）

- `claude-ds`: `CLAUDE_BILLING_BACKEND=deepseek` を中継と claude の両方に渡す（秘密値ではない）
- `claude-or`: `claude-ds` と同じ形に揃える。セッション専用の空きポート、キーは stdin、`exec` しない、
  終了時に中継を畳む。`OPENROUTER_API_KEY` に加えて `OPENROUTER_MANAGEMENT_KEY` を 2 行目で渡す。
  `CLAUDE_BILLING_BACKEND=openrouter`
- `bws-run`: `BWS_PROJECT_ID` で project を切り替えられるようにする（既定は今の ai-dev）。
  `claude-or` は ai-billing から Management key を取るためにこれを使う。取れなくても起動は続け、`bal` が `error` になるだけ
- PR #409 レビューの deferred P3 をここで直す: 中継が bind できたら stdout（ログ）に `ready <port>` を出し、
  launcher は TCP 接続ではなくこの行を待つ（空きポートを別プロセスに取られた時に誤認しない）。
  終了時のログ削除は「空なら消す」から「`ready` 行しか無ければ消す」に変える

### 3. サイドバー（herdr-agent-quota）

- collector（`claude-statusline`）は env の `CLAUDE_BILLING_BACKEND` を見て、payload の `session_id` を
  その backend に結びつけて cache に保存する（新しい optional フィールド。無ければ今の動作）
- 結びついたセッションのペインでは:
  - Claude の 5h / 7d / Fab の行を出さない
  - 既存の 3 つの枠（5h・7d・週スコープ）を流用して、`bal` / `day · mon` / `ses` を出す。herdr 側のトークン名とテンプレートは変えない
  - `bal` は残り割合のゲージ（燃料計の向き。Claude の行は使った割合なので向きが逆になる点に注意）。
    残り 20% 未満で warning、10% 未満で danger
  - provider 表示は `Claude` でなく `DeepSeek` / `OpenRouter`
- `summary` と `balance` は mtime が変わった時だけ読み直す。`day` / `month` が今日・今月でなければ 0 と表示する
- statusLine 末尾の Claude 5h の pace も付けない

### 4. statusLine 2 行目（dotfiles `claude/statusline.js`）

- `CLAUDE_BILLING_BACKEND` がある時は、5h / 7d の代わりに `bal 72% $7.20 │ day $0.21 │ mon $3.40 │ ses $0.05` を出す。
  `ses` は payload の `session_id` で summary から引く
- OAuth の usage API（Claude サブスク）は呼ばない

### 5. 取れない時の表示

| 状態 | 表示 |
| --- | --- |
| 残高の取得失敗（`error`） | 最後に取れた値と経過時間（例 `bal $7.20 (12m前)`）。一度も取れていなければ `bal ?` |
| Management key の期限切れ | `bal key期限切れ` |
| 単価不明の応答がある | `day` / `mon` / `ses` の後ろに `+?` |
| まだ応答が無い | `day $0.00` など 0 で出す |

行を黙って消すことはしない。

## テスト

- 中継: 偽の上流で、(a) openrouter は `cost` を合算する、(b) deepseek は peak / off-peak の単価で計算する、
  (c) 日付が変わると `day` が 0 から数え直す、(d) 単価不明は `unpriced`、(e) 並行 2 プロセスで summary が壊れない、
  (f) 残高の `full` がチャージで上がり、使用では据え置き、(g) 401 で `key_expired`。`tests/claude_proxy_test.py` を拡張
- 起動スクリプト: `tests/claude_ds_test.sh` と同じ形の `claude_or` 版。`ready` 行待ちと、キーが claude 側に出ないことを見る
- herdr-agent-quota: `AppState` 相当の純粋関数テスト。backend に結びついたセッションで Claude の行が消え、3 行が出ること、
  日付がずれた summary で 0 になること、`bal` の severity のしきい値
- statusline.js: 既存の `tests/statusline_render_test.sh` に第三者モデルの fixture を足す
- 実通信: `claude-ds -p` と `claude-or -p` を 1 回ずつ流し、ledger・summary・balance が書かれ、サイドバーと statusLine に出ることを目で確認する

## 実装中に確かめること

- DeepSeek の `input_tokens` に `cache_read_input_tokens` 分が含まれるか。キャッシュが効く 2 回目の呼び出しで確かめ、
  含まれるなら計算で差し引く
- OpenRouter の `/credits` の応答を Management key で 1 回取り、形がドキュメントどおりか
- `cost` が SSE でない応答にも入るか

## 範囲外

- `claude-or` 以外（`or-ask` 等）で使った OpenRouter の金額は `day` / `mon` に入らない（`bal` には反映される）
- 中国の祝日の off-peak、CNY 建てアカウント
- Management key の自動更新（90 日ごとに手で作り直す。期限切れは `bal key期限切れ` で気づける）
