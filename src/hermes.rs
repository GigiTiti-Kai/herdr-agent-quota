//! Local identity for the Hermes Agent harness.
//!
//! Hermes keeps each session's current model and billing route in
//! `<hermes home>/state.db`, and rewrites both on a `/model` switch. That row
//! is the evidence here: `state.db` is opened read-only and only `model`,
//! `billing_provider`, `model_config`, and `title` are selected — never a
//! message. `auth.json` is never opened.
//!
//! The row names a provider, not an account. Hermes keeps the credential that
//! serves a session in memory only, and it can rotate between pooled
//! credentials mid-session, so nothing on disk proves which login a pane is
//! spending. A Hermes pane therefore shows its icon, group, topic, provider,
//! and model, and no quota: not the Codex/Claude/Grok CLI's, and not the
//! number `hermes usage` would report for whichever credential a new session
//! would pick.

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SESSION_ROW: &str =
    "SELECT model, billing_provider, model_config, title FROM sessions WHERE id = ?1 LIMIT 1";

/// What `state.db` says about one Hermes session right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HermesSession {
    pub model: Option<String>,
    /// Hermes's provider id (`openai-codex`, `anthropic`, `xai-oauth`, …).
    /// `None` when the row does not name one, or names two different ones.
    pub provider_id: Option<String>,
    pub title: Option<String>,
}

/// Hermes's own home resolution: `HERMES_HOME`, else `~/.hermes`.
pub fn home_from_env() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HERMES_HOME").filter(|home| !home.is_empty()) {
        return Some(PathBuf::from(home));
    }
    Some(directories::BaseDirs::new()?.home_dir().join(".hermes"))
}

/// Read one session row. A missing database, a locked one, an older schema,
/// or an unknown id all yield `None`.
pub fn lookup(home: &Path, session_id: &str) -> Option<HermesSession> {
    let connection =
        Connection::open_with_flags(home.join("state.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    // Hermes writes this database while a turn runs; never wait long on it.
    connection.busy_timeout(Duration::from_millis(250)).ok()?;
    let (model, billing_provider, model_config, title) = connection
        .query_row(SESSION_ROW, [session_id], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .ok()?;
    let configured = model_config
        .as_deref()
        .and_then(|config| serde_json::from_str::<Value>(config).ok())
        .and_then(|config| {
            config
                .get("provider")
                .or_else(|| config.pointer("/gateway_runtime/provider"))
                .and_then(Value::as_str)
                .and_then(provider_id)
        });
    // A `/model` switch writes both places. When they name different
    // providers the switch is half-written, and neither is trusted.
    let provider_id = match (
        billing_provider.as_deref().and_then(provider_id),
        configured,
    ) {
        (Some(billing), Some(configured)) if billing != configured => None,
        (Some(billing), _) => Some(billing),
        (None, configured) => configured,
    };
    Some(HermesSession {
        model: model.as_deref().and_then(display_text),
        provider_id,
        title: title.as_deref().and_then(display_text),
    })
}

fn provider_id(value: &str) -> Option<String> {
    let value = value.trim().to_ascii_lowercase();
    (!value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control))
        .then_some(value)
}

fn display_text(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control))
        .then(|| value.to_string())
}

#[cfg(test)]
pub(crate) fn write_fixture_db(home: &Path, rows: &[(&str, &str, &str, &str, &str)]) {
    std::fs::create_dir_all(home).unwrap();
    let connection = Connection::open(home.join("state.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, model TEXT, \
             billing_provider TEXT, model_config TEXT, title TEXT);",
        )
        .unwrap();
    for (id, model, billing_provider, model_config, title) in rows {
        let optional = |value: &str| (!value.is_empty()).then(|| value.to_string());
        connection
            .execute(
                "INSERT OR REPLACE INTO sessions VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    id,
                    optional(model),
                    optional(billing_provider),
                    optional(model_config),
                    optional(title)
                ],
            )
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(row: (&str, &str, &str, &str, &str)) -> Option<HermesSession> {
        let dir = tempfile::tempdir().unwrap();
        write_fixture_db(dir.path(), &[row]);
        lookup(dir.path(), "s1")
    }

    #[test]
    fn a_session_row_names_the_model_provider_and_title() {
        let found = session(("s1", "model-a", " OpenAI-Codex ", "", "Fix the build")).unwrap();
        assert_eq!(found.model.as_deref(), Some("model-a"));
        assert_eq!(found.provider_id.as_deref(), Some("openai-codex"));
        assert_eq!(found.title.as_deref(), Some("Fix the build"));
    }

    /// Older rows carry the route only in `model_config`; the TUI writes the
    /// top-level key and the CLI the `gateway_runtime` one.
    #[test]
    fn the_provider_falls_back_to_model_config() {
        for config in [
            r#"{"provider":"xai-oauth"}"#,
            r#"{"gateway_runtime":{"provider":"xai-oauth"}}"#,
        ] {
            let found = session(("s1", "model-b", "", config, "")).unwrap();
            assert_eq!(found.provider_id.as_deref(), Some("xai-oauth"), "{config}");
        }
    }

    /// Mid-switch the two writers can disagree. The model is still shown; no
    /// provider is named.
    #[test]
    fn a_half_written_switch_names_no_provider() {
        let found = session((
            "s1",
            "model-b",
            "openai-codex",
            r#"{"provider":"anthropic"}"#,
            "",
        ))
        .unwrap();
        assert_eq!(found.model.as_deref(), Some("model-b"));
        assert_eq!(found.provider_id, None);
    }

    #[test]
    fn a_missing_row_database_or_column_is_no_evidence() {
        assert_eq!(session(("other", "model-a", "openai-codex", "", "")), None);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(lookup(dir.path(), "s1"), None);
        Connection::open(dir.path().join("state.db"))
            .unwrap()
            .execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY, model TEXT);")
            .unwrap();
        assert_eq!(lookup(dir.path(), "s1"), None);
    }
}
