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
// ponytail: unused until the follow-up task wires the HTTP call; kept here now
// so both tasks share one set of constants instead of duplicating the literals.
#[allow(dead_code)]
pub(crate) const USAGE_URL: &str =
    "https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1";
#[allow(dead_code)]
pub(crate) const OAUTH_BETA: &str = "oauth-2025-04-20";
#[allow(dead_code)]
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
