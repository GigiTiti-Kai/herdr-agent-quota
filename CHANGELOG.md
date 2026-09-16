# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Cursor Agent CLI is a supported harness: `--agent cursor`, `--provider cursor`,
  its own settings row, and a sidebar row with a Cursor brand color. Quota is
  the included monthly pool from the same
  `aiserver.v1.DashboardService/GetCurrentPeriodUsage` call the CLI makes,
  authenticated with `accessToken` in `~/.cursor/auth.json` (macOS) or
  `$XDG_CONFIG_HOME/cursor/auth.json` (Linux), or `$CURSOR_AUTH_FILE`. When
  that file is missing or has no token, the collector reads only
  `cursorAuth/accessToken` from the desktop app's `state.vscdb`, opened
  read-only. The IDE database's mtime is never a credential gate. Included
  follows the CLI usage panel: `totalPercentUsed` when present, otherwise
  `includedSpend / limit`. Auto, API, and Included map onto at / api / 30d.
  A 30d sidebar field was added so a monthly window is not hidden behind 7d.
  Model comes from
  `cli-config.json` (`selectedModel` mapped through `model.displayName`); a
  session's `lastUsedModel` overrides it. The sidebar title uses Grok's hue.
  The last
  `<user_query>` in the session jsonl is the topic, so Cursor panes are never
  read. Cache and context come from the interactive CLI's
  `afterAgentResponse` / `stop` / `preCompact` hooks (token counts and, when
  present, `context_usage_percent` / `context_window_size`). Composer 2.x
  uses its documented 200k window when the hook omits the size. Cycle end is
  Unix milliseconds. Snapshots are stamped with
  `sha256("cursor\0" || token)`. The collector never writes or refreshes
  Cursor credentials, never opens Keychain, and never calls a bare `agent`
  binary.

### Fixed

- Agy sidebar quota no longer stays on `5h N/A` when Herdr's
  `antigravity-cli` session id is a spawned subagent conversation while
  statusLine identifies the parent `conversation_id`. Windows, model, and
  context remain keyed by the statusLine conversation because the active
  model selects the account's `gemini-*` or `3p-*` pool. A mismatched Herdr
  id bridges to that observation only when exactly one Agy conversation is
  retained; with multiple possible conversations it fails closed instead of
  borrowing another pane's model, context, or quota pool. Antigravity's idle
  zero cache counters are suppressed only in the Agy parser, leaving shared
  statusLine zero-hit semantics unchanged. Remaining 99.5–99.9% no longer
  prints as `100%` (Agy gemini-5h at `remaining_fraction` 0.9986 was a full
  green bar while headroom was 99).
- Grok cache no longer vanishes mid-turn. The CLI now writes session
  totals to `usage.json` while the turn is still running;
  `updates.jsonl` only gets a `usage` object on `turn_completed`, and a
  long working turn's 128 KB tail is tool-call payloads, so the jsonl
  scan saw nothing. Context still comes from `signals.json`.
- Codex sidebar model no longer sticks on the session-start `turn_context`.
  A long turn writes `turn_context` once at the beginning, then enough
  `token_count` / tool output that the 256 KB tail has no model line. The
  previous head fallback then published the first turn's model (`Codex/gpt-6-astra`
  while the TUI footer already showed `gpt-5.6-sol`). The collector now
  scans backwards from EOF for the latest `turn_context`, capped so a 40 MB
  rollout is not read on every watch pulse.
- `rustls` 0.23.43 → 0.23.45 (`RUSTSEC-2026-0285`). It is a `ureq` TLS
  dependency; the collector sends bearer tokens to provider endpoints, so a
  known-vulnerable handshake stack fails `cargo audit --deny warnings`.

## [1.5.5] - 2026-09-14

### Added

- Muse Code (Meta Muse Spark) is a supported harness: `--agent muse`,
  `--provider muse`, its own settings row, and a sidebar row with a Muse brand
  color. Quota is the `subs_usage` block of the same `muse-code/key` call the
  CLI makes at startup and for `/usage`, authenticated with the account login
  in `~/.config/muse/auth.json` (or `$XDG_CONFIG_HOME` / `$MUSE_AUTH_PATH`).
  A `storage: "keychain"` login (typical on macOS) keeps the token out of that
  file; the collector reads the CLI's Keychain item through
  `security find-generic-password`, with a deadline so a prompt cannot stall a
  refresh, and keeps a successful token in-process for the daemon lifetime.
  The call returns the key the CLI already stored, so polling it does not
  sign Muse out. The session window is published as 5h and the weekly window
  as 7d; a different advertised session length keeps its own label. Only the
  usage block is read — the key and account identity in the same response
  are discarded. Snapshots are stamped with `sha256("muse\0" || token)`.
  API-key logins and inactive subscriptions show no quota but keep the
  session fields below. A rejected token or a failed request keeps the last
  quota cached for that account.
- Muse panes get model, topic, context, and cache like other agents. Herdr
  reports no Muse session, so on Linux the pane is matched to its session
  through Muse's own `.session.lock` (`pid=<n>`) and the `HERDR_PANE_ID` the
  `muse-bin` process inherited. The session's `session.jsonl` tail supplies
  the last model call's model and token usage — context against the local
  `model-catalog` limit, cache as that call's read share — and the last
  submitted prompt as the topic, so Muse panes are never read for a topic.
  Muse publishes no prompt-cache lifetime, so there is no TTL. Without that
  evidence (for example on macOS) the pane shows quota and the default model
  only.
- The provider name is a sidebar field like any other: `--fields` and the
  settings pane accept `provider`, listed first. It defaults on, so an
  existing configuration renders exactly as before; turning it off leaves the
  row with its icon and numbers. A packed identity row follows its two halves,
  so hiding the model degrades `$quota_provider_model` to `$quota_provider`,
  hiding the provider degrades it to `$quota_model`, and hiding both writes
  no identity row. The error token stays unconditional: it says the plugin
  could not speak for a pane, which is a failure, not a field.

### Fixed

- A saved agent list that was complete before a new provider was added no
  longer makes `configure` abort when omp is not installed. Those builds
  wrote "everything on" as an enumeration (`claude,codex,grok,agy,opencode,pi,omp,devin`
  before Muse), which was then judged partial against the longer supported
  list, so a missing omp integration became a hard failure. That exact
  prefix is still read as every agent. A complete selection is now stored
  as `all`, and a subset with a leading `only` marker, so turning the
  newest agent off is not mistaken for the legacy full list.
- An idle pane now follows its own session's quota as soon as the cache has it.
  A Claude statusLine hook only writes the observation mailbox, so a pane that
  never starts a turn kept publishing whatever it last published: one pane sat
  on `7d 24%` and `5h N/A` while its session's stored windows had moved on and
  every sibling pane showed the new reading. A watch pass now also covers an
  idle pane whose published quota rows differ from the ones its cached
  snapshot would render, alongside the existing expired-window case.
- A `fields` preference saved before the provider was a field no longer hides
  the provider on upgrade. Those builds wrote "everything on" as
  `topic,model,cache,ttl,context,5h,7d` and drew the provider name regardless,
  so that exact list is still read as every field. A selection that hides only
  the provider is stored with a leading `no-provider` marker, which names it
  without being mistaken for that legacy list.
- Gauges now keeps `no cached` as an amber token when it joins the cache row;
  live TTL continues to fold into the uncoloured cache token.
- `omp usage` now passes `--profile` when the pane's agent directory is an omp
  named profile (`~/.omp/profiles/<name>/agent`), so that pane is billed to
  the profile's credential store rather than the default one. Non-profile
  layouts still use `PI_CONFIG_DIR`.

## [1.5.4] - 2026-09-10

### Added

- A third sidebar layout, `gauges`, now the default: each quota field gets
  its own row with a meter beside the number. Bars fill to the printed
  number, so `cx`, `5h`, `7d` and `30d` all follow `quota-percent`
  (remaining by default). Labels are three characters so those periods
  align; a provider-named window too long for the column keeps a plain row.
  Cache and TTL share a line when they fit (`cache 95.2% · ttl≈29m`) and
  split when the sidebar is too narrow. Meters size to the connected Herdr
  endpoint after its secondary-row indent and scrollbar — six cells at the
  default 26 columns, twelve at 32 or wider — and drop rather than clip.
  The `cx` row takes a muted green/amber/red colour from remaining context.
  New installs use `gauges`. Existing `packed` or `stacked` preferences are
  kept and can still be chosen from the settings pane.

### Fixed

- A `gauges` sidebar that is too narrow for a meter no longer flips the
  context number from remaining to used. Width lookup reads the connected
  endpoint's client-shell file instead of whichever state file was written
  last.

## [1.5.3] - 2026-09-10

### Fixed

- Idle panes no longer keep a frozen remaining count after a quota window
  resets. One policy covers every collector: a cached window whose reset is in
  the past bypasses the 60-second fetch debounce, and a watcher already running
  for another agent includes only those panes whose *displayed* windows have
  expired. Codex `/status` is still a session-local cache and is not scraped.

## [1.5.2] - 2026-09-08

### Changed

- Drop the native `agent` row from managed sidebar layouts. It duplicated the
  branded `$quota_provider_model` line (`grok` above `Grok/grok-4.6`). The
  machine/workspace/tab row stays; uninstall puts `agent` back.

## [1.5.1] - 2026-09-08

### Fixed

- Recover background quota updates across Herdr upgrades and live handoffs;
  normal installation/repair restores the watcher and retains preferences.
  Refresh and event paths respect the saved agent selection.
- Include Pi, OMP, and OpenCode in active-turn polling and complete a delayed
  final refresh when a turn ends inside the request debounce window.
- Bind OpenCode Go and ID-less Grok caches to their credentials; retain OMP
  readings for all reported account pins without spawning once per account.
- Remove unverified Codex rollout windows and rebuild legacy quota data from
  authoritative API responses or original StatusLine payloads.

### Changed

- Claude/Agy quota is session-local because StatusLine does not prove account
  identity. An unknown Agy model no longer combines two quota pools.
- Consolidate English/Chinese usage and upgrade documentation; separate dated
  research from current guidance and remove the completed internal task plan.

## [1.5.0] - 2026-09-08

### Changed

- Require Herdr 0.9.0 or later. Native machine/workspace/tab and agent
  identity rows stay above plugin fields in both sidebar layouts.
- Keep Herdr's native token styling, Space Git rows, and worktree grouping.
  Quota ordering remains opt-in.

### Fixed

- Focus events refresh the pane named in the event instead of the current
  global focus, including delayed events and independent Herdr clients.
- Reconfiguring managed shared Agent rows preserves added custom fields and
  styles while still migrating recognized older plugin layouts.

## [1.4.0] - 2026-09-06

### Added

- Devin CLI is a supported harness: `--agent devin`, its own settings row,
  and a sidebar row with a Devin brand color. Quota comes from the same
  Connect RPC `GetUserStatus` contract the Devin CLI uses, with the key read
  from `~/.local/share/devin/credentials.toml` (or `$DEVIN_CREDENTIALS_FILE`).
  Daily and weekly remaining percentages are flipped to used. The configured
  default model comes from `~/.config/devin/config.json` `agent.model` when
  present, then mapped through local `devin-models.json` for a display name.
  New sessions that never run `/model` use this same value. It is published
  as `snapshot.model` and used as the fallback when a session is not in
  `sessions.db`. The API `planInfo.planName` is the subscription plan and is
  not used as a model. A missing or malformed models catalog leaves the raw
  id and does not fail the quota fetch. Per-session active models are read
  from `~/.local/share/devin/cli/sessions.db` (SQLite, read-only, selecting
  only `id` and `model` — the omp `models.db` discipline, not `agent.db`),
  so two panes running different `/model` selections each show their own
  model.
  Snapshots are stamped with `sha256("devin\0" || key)`
  so a credential swap cannot keep the previous account's last-good value.
  The API key is never logged, stored, or included in error messages.
- `configure --apply` rewrites the shared `ui.sidebar.agents.rows` array only
  when it is empty, already managed by this plugin, or matches Herdr's default
  `["state_icon", "agent"]` row. Rows from another plugin or the user are
  left intact; `rows_by_agent`, managed `row_gap`, and keybindings are still
  added or updated. `workspace` and `pane` tokens are not treated as a safe
  default, so they cannot be silently replaced with `tab`.

### Changed

- Devin private orchestration under `.devin/` is not part of the published
  tree. Local Devin state stays gitignored, matching `.agents/`.

### Fixed

- Idle Claude panes on the same `CLAUDE_CONFIG_DIR` profile now share that
  profile's newest 5h/7d reading instead of freezing the last statusLine tick
  for each conversation. A later idle tick that repeats an older percentage
  for the same reset does not roll the shared figure back. Separate
  work/personal config directories stay isolated, including when their reset
  times happen to match. A window whose `resets_at` has already passed is
  shown as unknown rather than as a live percentage. Based on the report in
  #57.
- `configure` no longer runs `normalize_official_row` when generating
  `rows_by_agent` from user-owned shared rows, so tokens such as `pane` and
  `terminal_title_stripped` stay intact. With brand colors off, those custom
  shared rows still get plugin-managed per-agent quota rows — only the brand
  hue is omitted. Default Herdr rows are unchanged: brand off still writes no
  `rows_by_agent` copies.
- `configure` prints which user-owned `rows_by_agent` entries it left alone,
  instead of succeeding silently without installing quota for those agents.
- Sidebar tab names, topics, cache details, context, and unknown quota states
  now inherit Herdr's active theme instead of using text colors tuned for a
  dark background. Provider brand hues and quota severity colors remain
  plugin-owned because they carry plugin-specific meaning.

## [1.3.0] - 2026-09-01

### Added

- omp (oh-my-pi) is a supported harness: `--agent omp`, its own settings row,
  and automatic installation of Herdr's `omp` integration when selected. Model,
  context, and cache come from the same transcript reader Pi uses — omp is a
  fork of Pi and still writes JSONL v3 — with two field renames handled in the
  shared parser (`cttl.ephemeral1h` for Anthropic's one-hour cache writes, and
  omp's authoritative `contextTokens`). The agent directory is recovered from
  the absolute session path Herdr reports rather than from this process's
  environment, so a pane started under `PI_CONFIG_DIR` or `--profile` is read
  against its own state.
- omp quota comes from omp's own usage layer, `omp usage --json --provider <id>`,
  not from a guessed canonical credential. The pane's transcript identifies
  the provider, model, and a credential pin; the CLI returns sanitized account
  identity plus quota, and the plugin selects the pinned account without
  opening `agent.db`. The sanitized multi-account report is cached per omp
  provider so another pane can select its pin without another process spawn.
- The settings UI has an explicit `OMP CLI` dependency row. When `omp` is
  selected but missing, configure reports the missing binary and installation
  instructions instead of silently omitting quota.

### Fixed

- omp accounts are isolated even when one provider returns multiple logins.
  A failed account keeps its previous reading only while the new report still
  identifies that account; swapping or removing credentials cannot borrow a
  sibling account's quota.
- omp's cached usage TTL is respected by the plugin's watcher. A result from
  `omp usage --json` still goes through the plugin's own debounce, so active
  panes do not spawn a process every second while the provider cache is warm.
- Pi/OpenCode model, context, and cache attribution stays session-local when
  the same harness has multiple panes.

## [1.2.0] - 2026-08-28

### Added

- Pi and OpenCode panes now resolve their billed subscription from local
  transcript evidence instead of assuming the harness name is the billing
  provider. Pi/OpenCode can therefore display Claude, OpenAI/Codex, Google, or
  OpenCode Go quota without the agent name lying about who pays the request.
- OpenCode Go quota is collected from the local OpenCode credential store and
  presented as a scoped billing target rather than overwriting canonical
  provider caches.

### Fixed

- Provider snapshots now retain per-session models, contexts, and statusLine
  quota windows so sibling panes do not borrow one another's local evidence.
- Failed credential-scoped refreshes preserve the last good reading only for
  the same proven account.

## [1.1.0] - 2026-08-24

### Added

- Low quota alerts notify once when a provider first crosses the configured
  remaining-percent threshold. The provider is re-armed only after recovering
  above the threshold, and providers not present in a pass keep their alert
  state.
- Agent ordering by quota headroom can be enabled without adding a visible
  sidebar row. The hidden sort token is always published alongside quota, so
  toggling the order does not rewrite pane metadata.

### Fixed

- Cache upgrade paths preserve session-local diagnostics and avoid carrying
  stale quota across credential changes.

## [1.0.0] - 2026-08-20

### Added

- Initial public release with Claude, Codex, Grok, Agy, OpenCode, and Pi
  support.
