# Claude usage API collector and per-model weekly row

Date: 2026-09-18
Status: approved design, not yet implemented
Fork: `GigiTiti-Kai/herdr-agent-quota`, branch `dev`

## Problem

Two requests, both about Claude quota in the Herdr agent sidebar.

1. **Quota goes stale while idle.** Codex, Grok and Devin poll a provider
   endpoint, so the watcher interval refreshes them on its own. Claude and Agy
   are read from the harness statusLine, which the harness only emits when it
   renders — in practice, when a turn runs. An idle Claude pane keeps whatever
   number it last saw.
2. **No per-model weekly figure.** The Claude subscription has a weekly cap
   scoped to a single model on top of the account-wide weekly cap. The sidebar
   cannot show it.

Both reduce to one cause: the plugin's Claude collector only sees the statusLine
payload, and that payload does not carry enough.

## Evidence

Gathered 2026-09-18 on this machine. All read-only.

### The statusLine cannot satisfy either request

The Claude Code binary documents its statusLine `rate_limits` schema as exactly
three optional members:

| member | meaning |
| --- | --- |
| `five_hour` | 5-hour session limit |
| `seven_day` | 7-day weekly limit |
| `spend_limit` | only behind a Claude gateway |

There is no model-scoped member. A live capture
(`claude-statusline.observation.json`) matches: `five_hour` and `seven_day` only.

### An account-wide endpoint carries both

`GET https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1`

| header | value |
| --- | --- |
| `Authorization` | `Bearer <claudeAiOauth.accessToken>` |
| `anthropic-beta` | `oauth-2025-04-20` |
| `User-Agent` | `claude-cli/<version> (external, cli)` |

Verified once by hand: HTTP 200. The response carries a render-ready `limits`
array; the observed values were

| `kind` | `group` | `percent` | `severity` | `scope.model.display_name` |
| --- | --- | --- | --- | --- |
| `session` | `session` | 8 | `normal` | — |
| `weekly_all` | `weekly` | 62 | `normal` | — |
| `weekly_scoped` | `weekly` | 92 | `critical` | `Fable` |

Each element also carries `resets_at` as an ISO-8601 timestamp. Top-level
`five_hour` and `seven_day` objects repeat the first two as `utilization`
percentages. This is the same endpoint Claude Code's own `/usage` renders, so it
needs no turn and is independent of any session.

Credentials live at `~/.claude/.credentials.json` under `claudeAiOauth`, whose
members are `accessToken`, `refreshToken`, `expiresAt`, `refreshTokenExpiresAt`,
`scopes`, `rateLimitTier` and `subscriptionType`.

### Antigravity has no equivalent

The `agy` binary talks to a Google protobuf/gRPC backend (`cloudquotas`,
`serviceusage`). There is no small REST usage endpoint to poll. Its statusLine
is already richer than Claude's — it reports `gemini-5h`, `gemini-weekly`,
`3p-5h` and `3p-weekly` — but it stays turn-driven.

## Goals

1. Claude's 5h and 7d figures refresh on the watcher interval while the pane is idle.
2. The model-scoped weekly cap appears as its own sidebar row.
3. The fork stays cheap to re-sync with upstream.

## Non-goals

- Idle auto-fetch for Agy. Tracked separately as workstream D; the gRPC surface
  is a different size of job and this design does not depend on it.
- Refreshing or rotating the OAuth token. Claude Code owns that lifecycle.
- Any change to Codex, Grok, Devin, OpenCode, Pi or OMP.
- Publishing this upstream. The endpoint is undocumented; keep it fork-local
  unless upstream asks.

## Design

### Split the Claude collector by what each source knows

The API is account-wide; the statusLine is session-scoped. Each keeps what it is
authoritative for, and neither is asked for the other's data.

| data | source after this change |
| --- | --- |
| 5h / 7d / model-scoped weekly | **new API collector** (account-wide, pollable) |
| context %, cache %, TTL, model, topic | existing statusLine (per session) |

This also fixes an existing limitation. Today's comment in `refresh.rs` notes
that Claude observations are not shared across sessions because the statusLine
reports no reliable serving account. The API result is keyed by the credential,
so one fetch serves every Claude pane.

### New module: `src/providers/claude_api.rs`

Modelled on `src/providers/grok.rs`, which already does `ureq` + `Bearer` against
a provider endpoint.

- Read `accessToken` from the credentials file. Missing file, unreadable JSON, or
  an `expiresAt` in the past is an error, not a guess.
- `GET` the endpoint with the three headers above.
- Parse `limits[]` and ignore unknown `kind` values rather than failing, so a
  new bucket upstream cannot break the row.

Map each element to the existing `UsageWindow`:

| `kind` | `WindowKind` | label |
| --- | --- | --- |
| `session` | `FiveHour` | `5h` |
| `weekly_all` | `Weekly` | `7d` |
| `weekly_scoped` | `WeeklyScoped` (new) | derived from the model name |

`percent` is used quota, matching `UsageWindow::used_percent`. `severity` is not
carried over: the plugin already derives its own colour from remaining headroom,
and two competing severity scales in one row would be a bug.

### The new window kind

`WindowKind` gains `WeeklyScoped`. It is deliberately not `Weekly` with a
different `source_label`: the enum's existing doc comment warns that a value must
never be published through another kind's token, and two rows both labelled `7d`
would be exactly that.

The model name rides along so the row can name itself. `UsageWindow` already
carries `source_label: Option<String>` for provider-supplied labels (omp uses
it), so the scoped window sets `source_label = Some(<3-char label>)` and the
renderer prefers it over `kind.label()`.

**Label rule** — labels are three characters so the `5h` / `7d` / `30d` column
stays aligned. Take the model's `display_name`, keep its first three characters,
title-case them: `Fable` becomes `Fab`, `Opus` becomes `Opu`. A display name
shorter than three characters is used as-is. This is the one piece of the design
worth revisiting once a second scoped model is observed in the wild.

### Fallback

`refresh.rs` currently routes `Provider::Claude` to `load_statusline_snapshot`.
It becomes: try the API, and on any failure fall back to the statusLine snapshot.

Failure means a missing or expired credential, a transport error, a non-200
status, or a body that does not parse. In every case the behaviour degrades to
exactly what ships today, including the existing rule that a failed request
preserves the last verified reading rather than publishing zero usage. The
scoped-weekly row is simply absent on the fallback path, because the statusLine
never had that number.

### Polling

The API collector joins the existing background watcher at the configured
interval (default 60s) alongside Codex, Grok and Devin. It needs no new timer,
no new setting, and no new cache file beyond its own snapshot.

## Testing

Unit tests, next to the code as the project does elsewhere:

- `limits[]` fixture → three windows with the right kinds, percentages and reset times.
- An unknown `kind` is ignored and the known ones still parse.
- A response with no `weekly_scoped` yields two windows and no scoped row.
- Malformed JSON, non-200 and a past `expiresAt` each produce an error, so the
  caller takes the fallback.
- Label derivation: `Fable` → `Fab`, a two-character name unchanged.

The live endpoint is exercised once by hand during implementation and not in CI;
a test must never depend on a real credential or network.

## Risks

- **Undocumented endpoint.** Anthropic can change or withdraw it without notice.
  Mitigated by the fallback and by parsing defensively; a break degrades to
  today's behaviour rather than an error state.
- **Public fork.** A fork of a public repository cannot be made private, so this
  code and this document are public. The endpoint is recoverable from the Claude
  Code binary with `strings`, and comparable community tools already document
  Claude usage endpoints, so this publishes no credential and no novel secret.
  Worth a second look if that judgement ever stops holding.
- **Sync conflicts.** This touches `refresh.rs`, `model.rs` and `cli.rs`, all
  files upstream edits. Expect to resolve conflicts on each sync; `fork/` itself
  never conflicts.
- **Token expiry.** We never refresh the token. A stale credential silently takes
  the fallback path, which is correct but quiet; a one-line diagnostic on the
  dashboard would make it visible. Out of scope here.

## Workstreams

| # | branch | scope | depends on |
| --- | --- | --- | --- |
| A | `dev` | fork scaffolding, sync tooling, this spec | — |
| B | `feat/claude-usage-api` | API collector, fallback, 5h/7d from the API | A |
| C | `feat/model-scoped-weekly` | `WeeklyScoped`, label rule, sidebar field, settings | B |
| D | — | Agy idle auto-fetch over gRPC | independent |

B and C share the endpoint and stack rather than run in parallel. D is a separate
investigation; if it outgrows a session, it leaves an issue and a handoff instead
of a half-built collector.
