# 第三者モデル用メーター 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** DeepSeek / OpenRouter で動く Claude Code ペインのサイドバーと statusLine に、Claude の 5h/7d/Fab の代わりに `bal` / `day · mon` / `ses` を出す。

**Architecture:** キーを持つ唯一のプロセスである中継（dotfiles `bin/claude-or-proxy.py`）が応答ごとに金額を ledger / summary に書き、残高を 60 秒ごとに balance に書く。herdr-agent-quota の statusLine hook がセッション ID と backend を結びつけ、サイドバーの refresh と `statusline.js` はファイルを読むだけで描く。

**Tech Stack:** Python 3.12 標準ライブラリ（中継）、bash（起動スクリプト）、Node（statusline.js）、Rust 2021 + `time` 0.3 + `serde`（プラグイン）

**Spec:** `fork/specs/2026-09-24-third-party-meters-design.md`

## Global Constraints

- 請求データの置き場: `${CLAUDE_BILLING_DIR:-$HOME/.local/state/claude-billing}`、ファイル名は `ledger-YYYY-MM.jsonl` / `summary-<backend>.json` / `balance-<backend>.json`、`<backend>` は `deepseek` か `openrouter`
- backend の合図は env `CLAUDE_BILLING_BACKEND`（秘密値ではない）。未設定なら全コンポーネントが今の動作のまま
- 日付と月は日本時間（UTC+9 固定）
- DeepSeek 単価（USD/1M、peak）: flash = hit 0.006 / miss 0.3 / out 1.2、v4-pro = hit 0.044 / miss 1.32 / out 3.96。off-peak は半額。peak = UTC 月〜金の 01:00–04:00 と 06:00–10:00
- 残高の取得間隔 60 秒。`balance-<backend>.json` の `fetched_at` が 60 秒以内なら取りに行かない
- キーは env・argv・ファイル・ログに出さない。中継へは stdin（1 行目キー、2 行目 Management key）
- ai-billing project ID: `b31dd68b-5e47-47b2-ab61-b4ce011f6c24`、secret 名 `OPENROUTER_MANAGEMENT_KEY`

## Review Focus

1. 計量中に例外（壊れた SSE、書き込み失敗）が出ても応答の中継は最後まで届くこと
2. 日付が変わった後、新しい応答が無いまま表示した時に昨日の `day` が出ないこと（読む側で 0 にする）
3. `sessions` にまだ無いセッション（最初の応答前）で `ses $0.00` が出て、行が消えないこと
4. Management key が無い・期限切れの時に `claude-or` が起動を止めず、`bal ?` / `bal key期限切れ` になること
5. 2 つの中継が同時に summary を更新しても JSON が壊れず、合計が両方の分になること

## spec からの意図的な差分（実装前に確認済み）

- **判定箇所**: spec は `route.rs` に `Resolution::Metered` を足す案だったが、`$quota_group` は provider ではなく
  workspace ラベルから決まる（`src/herdr.rs:1188 group_label_for`）ので、見出しがずれる心配は無かった。
  判定は `refresh.rs::resolved_pane_tokens` の Claude 分岐 1 か所で行い、`metered::overlay` で tokens を差し替える。
  判定箇所が 1 つで、presentation に `if` を散らさない点は spec のとおり
- **severity**: `week_style_base`（`src/herdr.rs:1529`）は 5h が空かどうかで配置を変えるだけで、色は各トークンの
  severity で独立に決まっていた。追加の対処は不要
- **mtime キャッシュ**: プラグインはイベントごとの短命プロセスなので、小さな JSON 2 つを毎回読む。
  ponytail: 常駐 watch で重くなったら mtime キャッシュを足す

---

### Task 1: 中継に計量・残高・ready 通知を足す（dotfiles）

**Files:**
- Modify: `bin/claude-or-proxy.py`
- Test: `tests/claude_proxy_test.py`

**Interfaces:**
- Produces（ファイル形式、Task 3・4 が読む）:
  - ledger 1 行: `{"ts": <unix float>, "backend", "session", "model", "input", "cache_read", "cache_write", "output", "usd": <float|null>}`
  - summary: `{"backend", "day": "YYYY-MM-DD", "day_usd", "month": "YYYY-MM", "month_usd", "sessions": {"<id>": {"usd", "unpriced", "last"}}, "unpriced", "updated_at"}`
  - balance: `{"status": "ok"|"error"|"key_expired", "balance": <float|null>, "full": <float|null>, "currency": "USD", "fetched_at", "last_ok_at"}`
  - 起動完了時に stdout へ `ready <port>` を 1 行（flush）
- 追加の env: `CLAUDE_BILLING_BACKEND`、`CLAUDE_BILLING_DIR`（テスト用）、`CLAUDE_BILLING_BALANCE_URL`（テスト用、既定は backend ごとの公式 URL）、`CLAUDE_BILLING_NOW`（テスト用の固定時刻、unix 秒）

- [ ] **Step 1: 失敗するテストを書く** — 偽の上流に `/v1/messages`（SSE と JSON）と `/balance` を足し、次を assert する
  - openrouter: `message_delta.usage.cost` が ledger の `usd`、summary の `day_usd` / `sessions[<id>].usd` に入る
  - deepseek: `CLAUDE_BILLING_NOW` を peak（月曜 02:00 UTC）と off-peak（土曜）にして、input 1,000,000 / output 1,000,000 の flash が $1.5 と $0.75
  - 単価表に無いモデル → `usd: null`、`unpriced` が 1
  - summary の `day` が前日なら新しい応答で `day_usd` がその 1 件分から数え直し
  - 残高: 1 回目 10.0 → `full` 10.0、2 回目 8.0 → `full` 10.0 のまま、3 回目 20.0 → `full` 20.0、上流 401 → `status: key_expired` で `balance` は前回値を保持
  - 並行: 中継 2 つに同時に 20 リクエストずつ → summary が JSON として読め、`day_usd` が 40 件分
  - 上流が壊れた SSE（`data: {` で途切れる）を返しても、クライアントは上流と同じバイト列を受け取る
  - ready: 起動直後の stdout 1 行目が `ready <port>`
- [ ] **Step 2: 実行して失敗を見る** — `python3 tests/claude_proxy_test.py` → AssertionError
- [ ] **Step 3: 実装** — 中継に次を足す
  - `_forward` の応答中継ループで、`/v1/messages`（`count_tokens` 以外）かつ 200 の時だけ、SSE の `data:` 行を行バッファで拾い `usage` をマージ（`message_start.message.usage` → `message_delta.usage` で上書き）。非 SSE は本文の `usage`。中継が終わってから `record(session, model, usage)` を try/except で呼ぶ
  - `record`: deepseek は単価表 × トークン、openrouter は `usage.cost`。`fcntl.flock(<dir>/.lock)` の下で ledger 追記と summary の読み替え（一時ファイル → `os.replace`）
  - 残高スレッド（daemon）: 起動時と 60 秒ごとに `refresh_balance()`。deepseek は `GET https://api.deepseek.com/user/balance`（`balance_infos` の USD の `total_balance`）、openrouter は `GET https://openrouter.ai/api/v1/credits`（`total_credits - total_usage`、Management key、401 → `key_expired`、キー無し → `error`）
  - stdin の 2 行目を Management key として読む
  - `serve_forever` の前に `print(f"ready {PORT}", flush=True)`
- [ ] **Step 4: テストが通るのを見る** — `python3 tests/claude_proxy_test.py` → `proxy_check: N/N ok`
- [ ] **Step 5: commit** — `feat(claude-proxy): 応答ごとの金額と残高を記録する`

### Task 2: 起動スクリプトと bws-run（dotfiles）

**Files:**
- Modify: `bin/claude-ds`、`bin/claude-or`、`bin/bws-run`
- Test: `tests/claude_ds_test.sh`、Create: `tests/claude_or_test.sh`

**Interfaces:**
- Consumes: Task 1 の `ready <port>` 行、stdin 2 行
- Produces: claude の env に `CLAUDE_BILLING_BACKEND`（`deepseek` / `openrouter`）

- [ ] **Step 1: 失敗するテスト** — `claude_ds_test.sh` に「claude の env に `CLAUDE_BILLING_BACKEND=deepseek`」「終了後に中継プロセスが残っていない」を追加。`claude_or_test.sh` を ds 版と同じ形で作る（偽 `bws-run` は `BWS_PROJECT_ID` が ai-billing の時 `OPENROUTER_MANAGEMENT_KEY`、それ以外は `OPENROUTER_API_KEY` を入れる）。マーカーは変数で組む（gitleaks 対策）
- [ ] **Step 2: 失敗を見る** — `bash tests/claude_ds_test.sh; bash tests/claude_or_test.sh`
- [ ] **Step 3: 実装**
  - `bws-run`: `PROJECT_ID=${BWS_PROJECT_ID:-46f399e5-ee93-44d1-878e-b4ca01253b43}`
  - `claude-ds`: 中継と claude に `CLAUDE_BILLING_BACKEND=deepseek`。起動待ちを「ログに `ready ` 行が出るまで（中継が死んだら即失敗）」に変える。終了時のログ削除は `ready` 行しか無ければ
  - `claude-or`: claude-ds と同じ形に書き直す（env の `OPENROUTER_API_KEY` は外して自分を re-exec、ai-dev からキー、`BWS_PROJECT_ID=b31dd68b-…` で Management key（取れなければ空で続行）、空きポート、stdin 2 行、`exec` しない、trap で畳む、`CLAUDE_BILLING_BACKEND=openrouter`）。モデルの export は今のまま
- [ ] **Step 4: 通るのを見る** — 両テストと `shellcheck bin/claude-ds bin/claude-or bin/bws-run`
- [ ] **Step 5: commit** — `feat(claude-or): claude-ds と同じ中継方式にして金額を記録する`

### Task 3: statusline.js の 2 行目（dotfiles）

**Files:**
- Modify: `claude/statusline.js:185-214`
- Test: `tests/statusline_render_test.sh`

- [ ] **Step 1: 失敗するテスト** — `CLAUDE_BILLING_BACKEND=deepseek` と `CLAUDE_BILLING_DIR=<tmp>` に summary / balance の fixture を置き、ANSI を落とした 2 行目が `bal`・`72%`・`$7.20`・`day $0.21`・`mon $3.40`・`ses $0.05` を含み、`5h` を含まないことを assert。`day` が昨日の fixture で `day $0.00`、key_expired で `bal key期限切れ`
- [ ] **Step 2: 失敗を見る** — `bash tests/statusline_render_test.sh`
- [ ] **Step 3: 実装** — `CLAUDE_BILLING_BACKEND` がある時は OAuth usage を取らず、`billingSegments(backend, sessionId)` で 4 つの segment を `line2` に積む
- [ ] **Step 4: 通るのを見る**
- [ ] **Step 5: commit** — `feat(statusline): 第三者モデルでは残高と使用額を出す`

### Task 4: サイドバー（herdr-agent-quota）

**Files:**
- Create: `src/metered.rs`
- Modify: `src/lib.rs`（mod 追加）、`src/configure/claude.rs:71`（hook）、`src/refresh.rs:759-782`（Claude 分岐）、`src/presentation.rs`（`meter` / `gauge_cells` / `provider_model_label` / `GAUGE_LABEL_WIDTH` を `pub(crate)` に）

**Interfaces:**
- Consumes: Task 1 の summary / balance 形式、`CLAUDE_BILLING_BACKEND`
- Produces:
  - `pub enum Backend { DeepSeek, OpenRouter }`、`Backend::parse(&str) -> Option<Backend>`、`key()`、`display_name()`
  - `pub fn record_session(state_root: &Path, session_id: &str, backend: Backend, now: u64) -> anyhow::Result<()>`（`<state>/session-backends.json`、7 日で掃除）
  - `pub fn session_backend(state_root: &Path, session_id: &str) -> Option<Backend>`
  - `pub fn overlay(values: &mut MetadataTokens, backend: Backend, session_id: &str, billing_dir: Option<&Path>, now: u64, shape: SidebarShape)`

- [ ] **Step 1: 失敗するテスト**（`src/metered.rs` の `#[cfg(test)]`、tempdir）
  - summary / balance 正常 → `quota_5h` が `bal` で始まり `72%` と `$7.20` を含む、`quota_week` = `day $0.21 · mon $3.40`、`quota_week_scoped` = `ses $0.05`、`quota_month` 空、`quota_headroom` None、`quota_provider` = `DeepSeek`
  - summary の `day` が昨日 → `day $0.00`
  - summary 無し → `day $0.00 · mon $0.00` と `ses $0.00`
  - balance 無し → `bal ?`、key_expired → `bal key期限切れ`、error で前回値あり → `bal $7.20 (12m前)`
  - severity: 残り 15% → Warning、5% → Danger
  - `record_session` → `session_backend` の往復、8 日前のエントリが消える
- [ ] **Step 2: 失敗を見る** — `cargo test metered`
- [ ] **Step 3: 実装** — `metered.rs` を書き、hook で `CLAUDE_BILLING_BACKEND` があれば `record_session` して pace を付けない。`resolved_pane_tokens` の Claude 分岐で `session_backend` が Some なら `overlay`
- [ ] **Step 4: 通るのを見る** — `cargo fmt && cargo test && cargo clippy --release`
- [ ] **Step 5: commit** — `feat(fork): 第三者モデルのペインに残高と使用額を出す`

### Task 5: 実通信での確認

- [ ] `cargo build --release` してプラグインを再読込（`herdr plugin disable herdr-agent-quota && herdr plugin enable herdr-agent-quota`）
- [ ] DeepSeek のキャッシュ: `claude-ds -p` を同じプロンプトで 2 回。2 回目の ledger の `input` と `cache_read` を見て、`input` がキャッシュ分を含むなら Task 1 の計算で差し引く
- [ ] OpenRouter: `claude-or -p` を 1 回。balance が `ok` になり `balance` が数値
- [ ] サイドバーと statusLine の見た目を DeepSeek ペインで確認（スクリーンショットをユーザーに頼む）
