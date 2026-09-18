# Claude usage API collector Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Read Claude quota from the account-wide OAuth usage endpoint so it refreshes while a pane is idle, and surface the model-scoped weekly cap the statusLine cannot report.

**Architecture:** A new `claude_api` collector fetches `limits[]` from Anthropic's usage endpoint, mirroring how `grok.rs` polls its billing endpoint with `ureq` and a bearer token. Its windows are overlaid onto the existing statusLine snapshot, which stays authoritative for per-session context, cache and model. Any failure falls back to today's statusLine-only behaviour.

**Tech Stack:** Rust, `ureq` (already a dependency), `serde_json`, the project's existing `UsageWindow` / `ProviderSnapshot` model.

**Spec:** `fork/specs/2026-09-18-claude-usage-api-design.md`

## Global Constraints

- Endpoint: `https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1`
- Required headers: `Authorization: Bearer <token>`, `anthropic-beta: oauth-2025-04-20`, `User-Agent: claude-cli/2.1.0 (external, cli)`
- Credentials: `~/.claude/.credentials.json`, member `claudeAiOauth`, fields `accessToken` and `expiresAt` (milliseconds since epoch)
- Never refresh or rotate the token. An expired credential is an error that takes the fallback.
- A failed request must never publish zero usage; it falls back to the last verified statusLine reading.
- No new crate dependencies.
- Tests live beside the code in `#[cfg(test)] mod tests` and must never touch the network or a real credential.
- Sidebar window labels are three characters so the `5h` / `7d` / `30d` column stays aligned.
- Conventional commit subjects, lowercase.

---

### Task 1: Parse the usage response into windows

**Files:**
- Create: `src/providers/claude_api.rs`
- Modify: `src/providers/mod.rs` (add `pub mod claude_api;` beside the existing `pub mod claude;`)
- Test: `src/providers/claude_api.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `crate::model::{Provider, ProviderSnapshot, ResetAt, UsageWindow, WindowKind}`, `crate::providers::ProviderError`
- Produces:
  - `pub fn parse_usage(value: &serde_json::Value, fetched_at_unix: u64) -> Result<ProviderSnapshot, ProviderError>`
  - `pub struct ClaudeCredentials { pub access_token: String, pub expires_at_unix: Option<u64> }`
  - `pub fn read_credentials(path: &std::path::Path) -> Result<ClaudeCredentials, ProviderError>`
  - `pub fn credentials_path() -> Result<std::path::PathBuf, ProviderError>`

- [ ] **Step 1: Write the failing test**

Create `src/providers/claude_api.rs` containing only the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn response() -> serde_json::Value {
        json!({
            "five_hour": {"utilization": 8.0},
            "seven_day": {"utilization": 62.0},
            "limits": [
                {"kind": "session", "group": "session", "percent": 8,
                 "severity": "normal", "resets_at": "2026-09-18T08:00:00.556018+00:00",
                 "scope": null},
                {"kind": "weekly_all", "group": "weekly", "percent": 62,
                 "severity": "normal", "resets_at": "2026-09-19T17:59:59.556045+00:00",
                 "scope": null},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 92,
                 "severity": "critical", "resets_at": "2026-09-19T17:59:59.556276+00:00",
                 "scope": {"model": {"id": null, "display_name": "Fable"}}}
            ]
        })
    }

    #[test]
    fn parses_session_and_weekly_windows() {
        let snapshot = parse_usage(&response(), 1_000).unwrap();
        assert_eq!(snapshot.provider, crate::model::Provider::Claude);
        // The account-wide endpoint is not session-local, unlike the statusLine.
        assert!(!snapshot.session_quota_only);

        let five = snapshot
            .windows
            .iter()
            .find(|window| window.kind == WindowKind::FiveHour)
            .expect("5h window");
        assert_eq!(five.used_percent, 8.0);
        assert_eq!(five.remaining_percent, 92.0);
        assert!(five.resets_at.is_some());

        let weekly = snapshot
            .windows
            .iter()
            .find(|window| window.kind == WindowKind::Weekly)
            .expect("7d window");
        assert_eq!(weekly.used_percent, 62.0);
    }

    #[test]
    fn ignores_unknown_kinds_without_failing() {
        let mut value = response();
        value["limits"][0]["kind"] = json!("some_future_bucket");
        let snapshot = parse_usage(&value, 1_000).unwrap();
        // The session row is gone, the weekly one still parses.
        assert!(snapshot
            .windows
            .iter()
            .all(|window| window.kind != WindowKind::FiveHour));
        assert!(snapshot
            .windows
            .iter()
            .any(|window| window.kind == WindowKind::Weekly));
    }

    #[test]
    fn a_response_without_any_known_window_is_an_error() {
        let value = json!({"limits": []});
        assert!(parse_usage(&value, 1_000).is_err());
    }

    #[test]
    fn missing_credentials_file_is_missing_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let error = read_credentials(&dir.path().join("absent.json")).unwrap_err();
        assert!(matches!(error, ProviderError::MissingCredentials));
    }

    #[test]
    fn an_expired_token_is_rejected_rather_than_sent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        // expiresAt is milliseconds; this one lapsed in 1970.
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "claudeAiOauth": {"accessToken": "t", "expiresAt": 1_000_u64}
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            read_credentials(&path).unwrap_err(),
            ProviderError::MissingCredentials
        ));
    }

    #[test]
    fn a_live_token_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        let future_ms = (CacheStore::now_unix() + 3_600) * 1_000;
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "claudeAiOauth": {"accessToken": "token", "expiresAt": future_ms}
            }))
            .unwrap(),
        )
        .unwrap();
        let credentials = read_credentials(&path).unwrap();
        assert_eq!(credentials.access_token, "token");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib providers::claude_api`
Expected: FAIL to compile — `parse_usage`, `read_credentials` and `ProviderError` are not in scope.

- [ ] **Step 3: Write the implementation**

Put this above the test module in `src/providers/claude_api.rs`:

```rust
//! Claude subscription quota from the account-wide OAuth usage endpoint.
//!
//! The statusLine carries `five_hour` and `seven_day` and nothing else, and
//! only while a turn renders it, so an idle pane keeps a stale figure. This is
//! the endpoint Claude Code's own `/usage` reads: it needs no turn, and it
//! reports the model-scoped weekly cap the statusLine schema has no member for.

use crate::cache::CacheStore;
use crate::model::{Provider, ProviderSnapshot, ResetAt, UsageWindow, WindowKind};
use crate::providers::ProviderError;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// `cedar_ember=1` selects the current response shape; `skip_spend=1` drops the
/// spend block, which only a gateway account populates.
pub(crate) const USAGE_URL: &str =
    "https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1";
pub(crate) const OAUTH_BETA: &str = "oauth-2025-04-20";
pub(crate) const USER_AGENT: &str = "claude-cli/2.1.0 (external, cli)";

pub struct ClaudeCredentials {
    pub access_token: String,
    pub expires_at_unix: Option<u64>,
}

pub fn credentials_path() -> Result<PathBuf, ProviderError> {
    if let Some(path) = std::env::var_os("CLAUDE_CREDENTIALS_FILE") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME").ok_or(ProviderError::MissingCredentials)?;
    Ok(PathBuf::from(home).join(".claude").join(".credentials.json"))
}

/// Read the subscription token. We never refresh it: Claude Code owns that
/// lifecycle, and a lapsed token is reported as missing so the caller falls
/// back instead of sending a request that can only 401.
pub fn read_credentials(path: &Path) -> Result<ClaudeCredentials, ProviderError> {
    let bytes = std::fs::read(path).map_err(|_| ProviderError::MissingCredentials)?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| ProviderError::MissingCredentials)?;
    let oauth = value
        .get("claudeAiOauth")
        .ok_or(ProviderError::MissingCredentials)?;
    let access_token = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or(ProviderError::MissingCredentials)?
        .to_string();
    // `expiresAt` is milliseconds since the epoch.
    let expires_at_unix = oauth
        .get("expiresAt")
        .and_then(Value::as_u64)
        .map(|millis| millis / 1_000);
    if expires_at_unix.is_some_and(|expiry| expiry <= CacheStore::now_unix()) {
        return Err(ProviderError::MissingCredentials);
    }
    Ok(ClaudeCredentials {
        access_token,
        expires_at_unix,
    })
}

/// Map one `limits[]` element onto a window kind, or `None` for a bucket this
/// build does not know. Skipping is deliberate: a new bucket upstream must not
/// break the rows we do understand.
fn window_kind(kind: &str) -> Option<WindowKind> {
    match kind {
        "session" => Some(WindowKind::FiveHour),
        "weekly_all" => Some(WindowKind::Weekly),
        _ => None,
    }
}

pub fn parse_usage(
    value: &Value,
    fetched_at_unix: u64,
) -> Result<ProviderSnapshot, ProviderError> {
    let limits = value
        .get("limits")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::UnsupportedResponse("limits is not an array".to_string()))?;
    let mut windows = Vec::new();
    for limit in limits {
        let Some(kind) = limit.get("kind").and_then(Value::as_str).and_then(window_kind) else {
            continue;
        };
        let Some(percent) = limit.get("percent").and_then(Value::as_f64) else {
            continue;
        };
        let resets_at = limit
            .get("resets_at")
            .and_then(Value::as_str)
            .and_then(ResetAt::parse_rfc3339);
        let window = UsageWindow::new(kind, percent.clamp(0.0, 100.0), resets_at)
            .map_err(|error| ProviderError::UnsupportedResponse(error.to_string()))?;
        windows.push(window);
    }
    if windows.is_empty() {
        return Err(ProviderError::UnsupportedResponse(
            "no known quota window in limits".to_string(),
        ));
    }
    // No `session_local()`: this reading is keyed by the credential, so every
    // Claude pane on this login may share it.
    Ok(ProviderSnapshot::new(
        Provider::Claude,
        windows,
        fetched_at_unix,
    ))
}
```

Register the module in `src/providers/mod.rs` next to the existing `pub mod claude;`:

```rust
pub mod claude_api;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib providers::claude_api`
Expected: PASS, 6 tests.

If `tempfile` is not already a dev-dependency, it is — `grok.rs` tests use `tempfile::tempdir`.

- [ ] **Step 5: Commit**

```bash
git add src/providers/claude_api.rs src/providers/mod.rs
git commit -m "feat(claude): parse the account-wide usage response into windows"
```

---

### Task 2: Fetch the endpoint

**Files:**
- Modify: `src/providers/claude_api.rs`

**Interfaces:**
- Consumes: `parse_usage`, `read_credentials`, `credentials_path` from Task 1
- Produces: `pub fn fetch(fetched_at_unix: u64) -> anyhow::Result<ProviderSnapshot>`

There is no unit test for `fetch` itself: it is one `ureq` call whose only logic is
header assembly, and a test of it would either hit the network or assert on a mock
of our own making. Its parsing is already covered by Task 1. Verify it by hand in
Step 3.

- [ ] **Step 1: Write the implementation**

Add to `src/providers/claude_api.rs`:

```rust
use anyhow::{Context, Result};
use std::time::Duration;

fn http_error_status(error: &ureq::Error) -> String {
    match error {
        ureq::Error::Status(code, _) => format!("HTTP {code}"),
        ureq::Error::Transport(error) => error.to_string(),
    }
}

/// Fetch the account-wide subscription windows. Callers treat any error as
/// "use the statusLine instead" — this never invents a zero reading.
pub fn fetch(fetched_at_unix: u64) -> Result<ProviderSnapshot> {
    let path = credentials_path().map_err(anyhow::Error::from)?;
    let credentials = read_credentials(&path).map_err(anyhow::Error::from)?;
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(10))
        .timeout_write(Duration::from_secs(10))
        .build();
    let response = agent
        .get(USAGE_URL)
        .set(
            "Authorization",
            &format!("Bearer {}", credentials.access_token),
        )
        .set("anthropic-beta", OAUTH_BETA)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/json")
        .call()
        .map_err(|error| ProviderError::Request(http_error_status(&error)))
        .map_err(anyhow::Error::from)?;
    let value: Value = response
        .into_json()
        .context("decode Claude usage response")?;
    parse_usage(&value, fetched_at_unix).map_err(anyhow::Error::from)
}
```

- [ ] **Step 2: Run the build and the existing tests**

Run: `cargo test --lib providers::claude_api`
Expected: PASS, still 6 tests, no new warnings.

- [ ] **Step 3: Verify against the live endpoint once, by hand**

Add a temporary `#[test] #[ignore]` that calls `fetch(CacheStore::now_unix())` and
prints the windows, run it with `cargo test --lib providers::claude_api::tests::live -- --ignored --nocapture`,
confirm it prints a 5h and a 7d window with plausible percentages, then delete the test
before committing. Do not commit a test that reads a real credential.

- [ ] **Step 4: Commit**

```bash
git add src/providers/claude_api.rs
git commit -m "feat(claude): fetch subscription windows from the usage endpoint"
```

---

### Task 3: Overlay the API windows onto the statusLine snapshot

**Files:**
- Modify: `src/refresh.rs:1086` (the `Provider::Claude | Provider::Agy` arm) and add the overlay helper beside `load_statusline_snapshot` at `src/refresh.rs:1235`
- Test: `src/refresh.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `claude_api::fetch` (Task 2), the existing `FetchedSnapshot { snapshot, preserve_context, session_id }` and `load_statusline_snapshot`
- Produces: `fn overlay_claude_windows(api: Result<ProviderSnapshot>, statusline: Result<FetchedSnapshot>) -> Result<FetchedSnapshot>`

The overlay is a pure function over two already-computed results so it can be tested
without a network or a cache.

- [ ] **Step 1: Write the failing test**

Add to the existing `#[cfg(test)] mod tests` in `src/refresh.rs`:

```rust
fn api_snapshot(used: f64) -> ProviderSnapshot {
    ProviderSnapshot::new(
        Provider::Claude,
        vec![UsageWindow::new(WindowKind::Weekly, used, None).unwrap()],
        10,
    )
}

fn statusline_fetched(used: f64) -> FetchedSnapshot {
    FetchedSnapshot {
        snapshot: ProviderSnapshot::new(
            Provider::Claude,
            vec![UsageWindow::new(WindowKind::Weekly, used, None).unwrap()],
            5,
        )
        .session_local()
        .with_model(Some("Opus 5".to_string())),
        preserve_context: true,
        session_id: Some("session-1".to_string()),
    }
}

#[test]
fn api_windows_replace_statusline_windows_and_keep_session_data() {
    let merged =
        overlay_claude_windows(Ok(api_snapshot(62.0)), Ok(statusline_fetched(10.0))).unwrap();
    assert_eq!(merged.snapshot.windows[0].used_percent, 62.0);
    // Session-scoped facts survive: only the windows come from the endpoint.
    assert_eq!(merged.snapshot.model.as_deref(), Some("Opus 5"));
    assert_eq!(merged.session_id.as_deref(), Some("session-1"));
    assert!(merged.preserve_context);
    // The reading is account-wide now, so panes may share it.
    assert!(!merged.snapshot.session_quota_only);
}

#[test]
fn a_failed_api_call_falls_back_to_the_statusline_reading() {
    let merged = overlay_claude_windows(
        Err(anyhow::anyhow!("HTTP 401")),
        Ok(statusline_fetched(10.0)),
    )
    .unwrap();
    assert_eq!(merged.snapshot.windows[0].used_percent, 10.0);
    assert!(merged.snapshot.session_quota_only);
}

#[test]
fn the_api_alone_still_publishes_when_no_statusline_observation_exists() {
    let merged = overlay_claude_windows(
        Ok(api_snapshot(62.0)),
        Err(anyhow::anyhow!("no observation yet")),
    )
    .unwrap();
    assert_eq!(merged.snapshot.windows[0].used_percent, 62.0);
    assert!(merged.session_id.is_none());
}

#[test]
fn both_failing_reports_the_statusline_error() {
    assert!(overlay_claude_windows(
        Err(anyhow::anyhow!("HTTP 401")),
        Err(anyhow::anyhow!("no observation yet")),
    )
    .is_err());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib refresh::tests::api_windows refresh::tests::a_failed_api refresh::tests::the_api_alone refresh::tests::both_failing`
Expected: FAIL to compile — `overlay_claude_windows` is not defined.

- [ ] **Step 3: Write the implementation**

Add next to `load_statusline_snapshot` in `src/refresh.rs`:

```rust
/// Claude quota is account-wide and pollable; context, cache, model and topic
/// are per session and only the statusLine has them. Take the windows from the
/// endpoint and keep everything else the session already reported.
///
/// Either source alone is still publishable: with no statusLine observation the
/// endpoint gives quota without context, and with no endpoint we degrade to
/// exactly the behaviour that shipped before this collector existed.
fn overlay_claude_windows(
    api: Result<ProviderSnapshot>,
    statusline: Result<FetchedSnapshot>,
) -> Result<FetchedSnapshot> {
    match (api, statusline) {
        (Ok(api), Ok(mut fetched)) => {
            fetched.snapshot.windows = api.windows;
            fetched.snapshot.session_quota_only = false;
            Ok(fetched)
        }
        (Ok(api), Err(_)) => Ok(FetchedSnapshot::direct(api)),
        (Err(_), statusline) => statusline,
    }
}
```

Replace the dispatch arm at `src/refresh.rs:1086`:

```rust
        Provider::Claude => overlay_claude_windows(
            crate::providers::claude_api::fetch(now),
            load_statusline_snapshot(cache, provider),
        ),
        Provider::Agy => load_statusline_snapshot(cache, provider),
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib refresh`
Expected: PASS, including the four new tests and every pre-existing `refresh` test.

- [ ] **Step 5: Commit**

```bash
git add src/refresh.rs
git commit -m "feat(claude): refresh quota from the endpoint and keep session data"
```

---

### Task 4: Add the model-scoped weekly window

**Files:**
- Modify: `src/model.rs:271` (`WindowKind`) and its `label()` at `src/model.rs:280`
- Modify: `src/providers/claude_api.rs` (`window_kind`, `parse_usage`)
- Test: `src/providers/claude_api.rs`, `src/model.rs`

**Interfaces:**
- Consumes: `UsageWindow::with_source_window(label, duration_seconds)` and `UsageWindow::display_label()`, which already prefers `source_label` over `kind.label()`
- Produces: `WindowKind::WeeklyScoped`, and `fn scoped_label(display_name: &str) -> String`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/providers/claude_api.rs`:

```rust
#[test]
fn parses_the_model_scoped_weekly_window() {
    let snapshot = parse_usage(&response(), 1_000).unwrap();
    let scoped = snapshot
        .windows
        .iter()
        .find(|window| window.kind == WindowKind::WeeklyScoped)
        .expect("scoped weekly window");
    assert_eq!(scoped.used_percent, 92.0);
    // The row names its own model rather than reusing the 7d token.
    assert_eq!(scoped.display_label(), "Fab");
}

#[test]
fn a_scoped_window_without_a_model_name_is_skipped() {
    let mut value = response();
    value["limits"][2]["scope"] = json!(null);
    let snapshot = parse_usage(&value, 1_000).unwrap();
    assert!(snapshot
        .windows
        .iter()
        .all(|window| window.kind != WindowKind::WeeklyScoped));
}

#[test]
fn scoped_labels_are_three_characters() {
    assert_eq!(scoped_label("Fable"), "Fab");
    assert_eq!(scoped_label("opus"), "Opu");
    // Shorter names are used as they are rather than padded.
    assert_eq!(scoped_label("Pi"), "Pi");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib providers::claude_api`
Expected: FAIL to compile — `WindowKind::WeeklyScoped` and `scoped_label` do not exist.

- [ ] **Step 3: Write the implementation**

In `src/model.rs`, add the variant to `WindowKind` and its label:

```rust
pub enum WindowKind {
    FiveHour,
    Weekly,
    /// Cached and rendered in the dashboard only. The sidebar has no monthly
    /// token, and a 30d value must never be published through a weekly one.
    Monthly,
    /// A weekly cap that applies to one model rather than the whole account.
    /// Deliberately not `Weekly` with a different label: the account-wide
    /// weekly is a different number, and two rows both reading `7d` would be
    /// the confusion this enum exists to prevent. The model name arrives as
    /// the window's `source_label`.
    WeeklyScoped,
}
```

```rust
    pub fn label(self) -> &'static str {
        match self {
            Self::FiveHour => "5h",
            Self::Weekly => "7d",
            Self::Monthly => "30d",
            // Never shown: a scoped window always carries a `source_label`,
            // which `display_label` prefers. This is the safety net.
            Self::WeeklyScoped => "wks",
        }
    }
```

In `src/providers/claude_api.rs`, add the label rule and teach the parser about the kind:

```rust
/// Three characters keeps the scoped row aligned with `5h` and `7d`. A name
/// shorter than that is used as it is rather than padded.
fn scoped_label(display_name: &str) -> String {
    let mut chars = display_name.chars();
    let head: String = chars.by_ref().take(3).collect();
    let mut label = head.to_lowercase();
    if let Some(first) = label.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    label
}
```

Replace the body of the `limits` loop in `parse_usage` so the scoped kind is
recognised and carries its model name:

```rust
    for limit in limits {
        let Some(raw_kind) = limit.get("kind").and_then(Value::as_str) else {
            continue;
        };
        let Some(percent) = limit.get("percent").and_then(Value::as_f64) else {
            continue;
        };
        let resets_at = limit
            .get("resets_at")
            .and_then(Value::as_str)
            .and_then(ResetAt::parse_rfc3339);
        let scoped_model = limit
            .get("scope")
            .and_then(|scope| scope.get("model"))
            .and_then(|model| model.get("display_name"))
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty());
        let window = match (raw_kind, scoped_model) {
            ("session", _) => UsageWindow::new(
                WindowKind::FiveHour,
                percent.clamp(0.0, 100.0),
                resets_at,
            ),
            ("weekly_all", _) => {
                UsageWindow::new(WindowKind::Weekly, percent.clamp(0.0, 100.0), resets_at)
            }
            // An unnamed scoped window has nothing to label itself with, so it
            // is skipped rather than rendered as an anonymous second weekly.
            ("weekly_scoped", Some(model)) => UsageWindow::new(
                WindowKind::WeeklyScoped,
                percent.clamp(0.0, 100.0),
                resets_at,
            )
            .map(|window| window.with_source_window(scoped_label(model), None)),
            _ => continue,
        }
        .map_err(|error| ProviderError::UnsupportedResponse(error.to_string()))?;
        windows.push(window);
    }
```

Delete the now-unused `window_kind` helper.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS. The new `WindowKind` variant makes every exhaustive `match` on it a
compile error until handled; fix each one the compiler names. `src/presentation.rs:683`
filters `FiveHour | Weekly` and must now include `WeeklyScoped` so the scoped row is
treated as live quota.

- [ ] **Step 5: Commit**

```bash
git add src/model.rs src/providers/claude_api.rs src/presentation.rs
git commit -m "feat(claude): report the model-scoped weekly cap as its own window"
```

---

### Task 5: Render the scoped row in the sidebar

**Files:**
- Modify: `src/cli.rs:217` (`SidebarField`), `src/cli.rs:230` (`SidebarField::ALL`), `SidebarField::name`
- Modify: `src/presentation.rs:138-143` (the quota token struct), `src/presentation.rs:268-280` (token construction), `src/presentation.rs:344` (`headroom`)
- Test: `src/presentation.rs`

**Interfaces:**
- Consumes: `WindowKind::WeeklyScoped` from Task 4, the existing `window_in`,
  `compact_window_parts`, `Severity::for_window` and the test helper
  `fn window(kind: WindowKind, used: f64, reset: u64) -> UsageWindow` at `src/presentation.rs:814`
- Produces: `SidebarField::WeekScoped` (name `week-scoped`) and the `quota_week_scoped` token

`compact_window_parts` already renders through `window.display_label()`
(`src/presentation.rs:569`), which is how Cursor gets its `at` and `api` rows. The
scoped label therefore needs no formatter change — only a token to live in.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/presentation.rs`:

```rust
fn scoped_window(used: f64) -> UsageWindow {
    UsageWindow::new(WindowKind::WeeklyScoped, used, None)
        .unwrap()
        .with_source_window("Fab", None)
}

#[test]
fn the_scoped_weekly_window_gets_its_own_token() {
    let snapshot = ProviderSnapshot::new(
        Provider::Claude,
        vec![
            window(WindowKind::FiveHour, 40.0, 3_600),
            window(WindowKind::Weekly, 75.0, 183_600),
            scoped_window(92.0),
        ],
        0,
    );
    let tokens = MetadataTokens::from_snapshot(&snapshot, 0);
    // The row names its model instead of reading 7d a second time.
    assert!(tokens.quota_week_scoped.contains("Fab"));
    assert!(tokens.quota_week_scoped.contains("92"));
    // The account-wide weekly is untouched by the scoped one.
    assert!(tokens.quota_week.contains("75"));
}

#[test]
fn headroom_counts_the_scoped_weekly_window() {
    let snapshot = ProviderSnapshot::new(
        Provider::Claude,
        vec![window(WindowKind::Weekly, 60.0, 183_600), scoped_window(92.0)],
        0,
    );
    // 8 points left on Fable is tighter than the 40 left on the weekly.
    assert_eq!(
        MetadataTokens::from_snapshot(&snapshot, 0).quota_headroom,
        Some(8)
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib presentation`
Expected: FAIL to compile — `quota_week_scoped` and `SidebarField::WeekScoped` do not exist.

- [ ] **Step 3: Write the implementation**

In `src/cli.rs`, add the field:

```rust
pub enum SidebarField {
    Provider,
    Topic,
    Model,
    Cache,
    Ttl,
    Context,
    FiveHour,
    Week,
    WeekScoped,
    Month,
}
```

```rust
    pub const ALL: [Self; 10] = [
        Self::Provider,
        Self::Topic,
        Self::Model,
        Self::Cache,
        Self::Ttl,
        Self::Context,
        Self::FiveHour,
        Self::Week,
        Self::WeekScoped,
        Self::Month,
    ];
```

Add `Self::WeekScoped => "week-scoped",` to `SidebarField::name`. `FieldSet` is a
bitset keyed off `SidebarField::ALL`, so the new field needs no further wiring;
confirm `FieldSet::all()` still round-trips through `parse` by running its existing
tests.

In `src/presentation.rs`, add the token fields beside `quota_week` at line 140:

```rust
    pub quota_week_scoped: String,
    pub quota_week_scoped_severity: Option<Severity>,
```

Bind the window next to the others at line 271:

```rust
        let weekly_scoped = window_in(windows, WindowKind::WeeklyScoped);
```

and populate the token in the same struct literal, directly after `quota_week_severity`:

```rust
            quota_week_scoped: weekly_scoped
                .map(|window| compact_window_parts(window, now_unix, style, shape).rendered())
                .unwrap_or_default(),
            quota_week_scoped_severity: weekly_scoped
                .map(|window| Severity::for_window(window, now_unix)),
```

Add the scoped window to `headroom` at `src/presentation.rs:344` so a nearly-exhausted
model cap drives the sort order and the low-quota alert:

```rust
        fields
            .contains(SidebarField::WeekScoped)
            .then(|| window_in(windows, WindowKind::WeeklyScoped))
            .flatten(),
```

Finally, publish the token so Herdr can place the row. `src/herdr.rs:37-44` lists the
weekly token under each severity — `quota_week_normal`, `_warning`, `_danger`,
`_unknown`, and the four matching `quota_week_inline_*` — and the list repeats at
`src/herdr.rs:72`. Add the `quota_week_scoped_*` equivalents everywhere `quota_week_*`
appears, then add the row to the defaults written by `src/configure/herdr.rs` so a
fresh install shows it.

Run `grep -rn 'quota_week' src/ --include=*.rs` afterwards and confirm every hit has a
`quota_week_scoped` counterpart; a token published in one list but not the other is
silently dropped at render time.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS, whole suite.

- [ ] **Step 5: Verify in the live sidebar**

```bash
./install.sh
herdr plugin action invoke refresh --plugin herdr-agent-quota
```

Expected: each Claude row shows a third quota line labelled with the scoped model
(`Fab`) beside `5h` and `7d`, and the figure matches `/usage` in Claude Code.

- [ ] **Step 6: Commit**

```bash
git add src/cli.rs src/presentation.rs
git commit -m "feat(sidebar): show the model-scoped weekly cap as its own row"
```

---

## Out of scope

Workstream D from the spec — idle auto-fetch for Antigravity — is not planned here.
Its backend is protobuf/gRPC rather than a REST endpoint, which is a different size
of job, and nothing in this plan depends on it. It gets its own investigation; if
that outgrows a session it leaves an issue and a handoff rather than a half-built
collector.
