//! Claude subscription quota from the account-wide OAuth usage endpoint.
//!
//! The statusLine carries `five_hour` and `seven_day` and nothing else, and
//! only while a turn renders it, so an idle pane keeps a stale figure. This is
//! the endpoint Claude Code's own `/usage` reads: it needs no turn, and it
//! reports the model-scoped weekly cap the statusLine schema has no member for.

use crate::cache::CacheStore;
use crate::model::{Provider, ProviderSnapshot, ResetAt, UsageWindow, WindowKind};
use crate::providers::ProviderError;
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `cedar_ember=1` selects the current response shape; `skip_spend=1` drops the
/// spend block, which only a gateway account populates.
pub(crate) const USAGE_URL: &str =
    "https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1";
pub(crate) const OAUTH_BETA: &str = "oauth-2025-04-20";
pub(crate) const USER_AGENT: &str = "claude-cli/2.1.0 (external, cli)";

#[derive(Debug)]
pub struct ClaudeCredentials {
    pub access_token: String,
    pub expires_at_unix: Option<u64>,
}

pub fn credentials_path() -> Result<PathBuf, ProviderError> {
    if let Some(path) = std::env::var_os("CLAUDE_CREDENTIALS_FILE") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME").ok_or(ProviderError::MissingCredentials)?;
    Ok(PathBuf::from(home)
        .join(".claude")
        .join(".credentials.json"))
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

pub fn parse_usage(value: &Value, fetched_at_unix: u64) -> Result<ProviderSnapshot, ProviderError> {
    let limits = value
        .get("limits")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::UnsupportedResponse("limits is not an array".to_string()))?;
    let mut windows = Vec::new();
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
            ("session", _) => {
                UsageWindow::new(WindowKind::FiveHour, percent.clamp(0.0, 100.0), resets_at)
            }
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
            // `model` is only known non-blank once trimmed: an untrimmed name
            // produces a whitespace label, which `with_source_window` drops,
            // and the row falls back to the `wks` placeholder this variant
            // exists to keep off screen.
            .map(|window| window.with_source_window(scoped_label(model.trim()), None)),
            _ => continue,
        }
        .map_err(|error| ProviderError::UnsupportedResponse(error.to_string()))?;
        windows.push(window);
    }
    if windows.is_empty() {
        return Err(ProviderError::UnsupportedResponse(
            "no known quota window in limits".to_string(),
        ));
    }
    // No `session_local()` here: this is the raw endpoint reading. The caller
    // (`overlay_claude_windows`) marks the merged snapshot session-local before
    // it is cached, because Claude snapshots have no account gate to be
    // validated against.
    Ok(ProviderSnapshot::new(
        Provider::Claude,
        windows,
        fetched_at_unix,
    ))
}

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
        // Skipped, not remapped: a scoped limit landing on `Weekly` would draw
        // a second, unexplained `7d` row beside the account-wide one.
        assert_eq!(snapshot.windows.len(), 2);
    }

    #[test]
    fn scoped_labels_are_three_characters() {
        assert_eq!(scoped_label("Fable"), "Fab");
        assert_eq!(scoped_label("opus"), "Opu");
        // Shorter names are used as they are rather than padded.
        assert_eq!(scoped_label("Pi"), "Pi");
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
