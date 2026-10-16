//! Local identity for the Hermes Agent harness.
//!
//! Hermes keeps each session's current model and billing route in
//! `<hermes home>/state.db`, and rewrites both on a `/model` switch. That row
//! is the evidence here: `state.db` is opened read-only and only `model`,
//! `billing_provider`, `model_config`, and `title` are selected — never a
//! message. From `session_model_usage` only the numeric token counters and
//! `last_seen` of the main-loop row (`task = ''`) for that model and provider
//! are selected, for the cache ratio. `auth.json` is never opened.
//!
//! The row names a provider, not an account. Hermes keeps the credential that
//! serves a session in memory only, and it can rotate between pooled
//! credentials mid-session, so nothing on disk proves which login a pane is
//! spending. A Hermes pane therefore shows its icon, group, topic, provider,
//! model, and session cache ratio, and no quota of its own: not the
//! Codex/Claude/Grok CLI's, and not the number `hermes usage` would report for
//! whichever credential a new session would pick.

use crate::model::CacheUsage;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SESSION_ROW: &str =
    "SELECT model, billing_provider, model_config, title FROM sessions WHERE id = ?1 LIMIT 1";

/// The newest main-loop usage row of the session's current route. Hermes adds
/// every API call's delta here, so the counters cover the whole session on
/// that model; Hermes's own route lookup orders the same way.
const USAGE_ROW: &str = "SELECT input_tokens, cache_read_tokens, cache_write_tokens, last_seen \
     FROM session_model_usage WHERE session_id = ?1 AND task = '' AND trim(model) = ?2 \
     AND lower(trim(billing_provider)) = ?3 ORDER BY last_seen DESC, rowid DESC LIMIT 1";

/// What `state.db` says about one Hermes session right now.
#[derive(Debug, Clone, PartialEq)]
pub struct HermesSession {
    pub model: Option<String>,
    /// Hermes's provider id (`openai-codex`, `anthropic`, `xai-oauth`, …).
    /// `None` when the row does not name one, or names two different ones.
    pub provider_id: Option<String>,
    pub title: Option<String>,
    /// Cumulative cache ratio of the current model and provider. `None` on a
    /// Hermes without `session_model_usage`, or before the route's first call.
    pub cache: Option<CacheUsage>,
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
    let model = model.as_deref().and_then(display_text);
    let cache = session_cache(
        &connection,
        session_id,
        model.as_deref(),
        provider_id.as_deref(),
        crate::cache::CacheStore::now_unix(),
    );
    Some(HermesSession {
        model,
        provider_id,
        title: title.as_deref().and_then(display_text),
        cache,
    })
}

/// The route's cache ratio, plus Codex's 30-minute estimate from the last call
/// when the route is `openai-codex`. Other providers publish no lifetime this
/// plugin can trust, so they show the ratio alone. Any read failure — an older
/// Hermes without the table or `task` column included — is no cache, never a
/// failed session lookup.
fn session_cache(
    connection: &Connection,
    session_id: &str,
    model: Option<&str>,
    provider_id: Option<&str>,
    now: u64,
) -> Option<CacheUsage> {
    let provider_id = provider_id?;
    let (input, read, write, last_seen) = connection
        .query_row(USAGE_ROW, [session_id, model?, provider_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<f64>>(3)?,
            ))
        })
        .ok()?;
    let count = |value: i64| u64::try_from(value).ok();
    let cache = CacheUsage::from_token_counts(count(input)?, count(read)?, count(write)?)?;
    // A future `last_seen` would render a lifetime longer than the estimate.
    // shortcut: openai-codex over api.meta.ai / Bedrock Mantle retains 24h, not
    // 30m; telling them apart needs `billing_base_url`, outside the read set.
    let last_seen = last_seen
        .filter(|seen| seen.is_finite() && *seen >= 0.0)
        .map(|seen| seen as u64)
        .filter(|seen| *seen <= now);
    Some(match last_seen {
        Some(seen) if provider_id == "openai-codex" => cache.with_ttl_estimate(
            crate::providers::codex::CODEX_PROMPT_CACHE_TTL_SECONDS,
            seen,
        ),
        _ => cache,
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

/// `(model, billing_provider, task, input_tokens, cache_read_tokens, last_seen)`.
#[cfg(test)]
pub(crate) type UsageRow<'a> = (&'a str, &'a str, &'a str, i64, i64, Option<f64>);

/// `session_model_usage` rows for session `s1`.
#[cfg(test)]
pub(crate) fn write_usage_fixture(home: &Path, rows: &[UsageRow]) {
    let connection = Connection::open(home.join("state.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS session_model_usage (session_id TEXT NOT NULL, \
             model TEXT NOT NULL, billing_provider TEXT NOT NULL DEFAULT '', \
             billing_base_url TEXT NOT NULL DEFAULT '', billing_mode TEXT NOT NULL DEFAULT '', \
             task TEXT NOT NULL DEFAULT '', input_tokens INTEGER NOT NULL DEFAULT 0, \
             cache_read_tokens INTEGER NOT NULL DEFAULT 0, \
             cache_write_tokens INTEGER NOT NULL DEFAULT 0, last_seen REAL);",
        )
        .unwrap();
    for (model, provider, task, input, read, last_seen) in rows {
        connection
            .execute(
                "INSERT INTO session_model_usage (session_id, model, billing_provider, task, \
                 input_tokens, cache_read_tokens, last_seen) VALUES ('s1', ?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![model, provider, task, input, read, last_seen],
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

    /// The cache a session `s1` on `model`/`provider` gets from `rows`.
    fn cache(model: &str, provider: &str, rows: &[UsageRow]) -> Option<CacheUsage> {
        let dir = tempfile::tempdir().unwrap();
        write_fixture_db(dir.path(), &[("s1", model, provider, "", "")]);
        write_usage_fixture(dir.path(), rows);
        lookup(dir.path(), "s1").expect("session row").cache
    }

    /// Auxiliary calls (compression, titles, approval) are separate rows; only
    /// the main loop's counts. Its last call starts Codex's 30-minute estimate.
    #[test]
    fn the_cache_is_the_main_loop_row_of_the_current_route() {
        let found = cache(
            "model-a",
            "openai-codex",
            &[
                ("model-a", "OpenAI-Codex", "", 10, 290, Some(1_000.7)),
                (
                    "model-a",
                    "openai-codex",
                    "compression",
                    5_000,
                    0,
                    Some(1_500.0),
                ),
            ],
        )
        .expect("main-loop row");
        assert_eq!((found.fresh_input_tokens, found.read_tokens), (10, 290));
        assert_eq!(found.ttl_seconds, Some(30 * 60));
        assert_eq!(found.last_activity_unix, Some(1_000));
    }

    /// After `/model` the session row names the new model; the old model's
    /// newer row is not its cache, and before the new route's first call there
    /// is none.
    #[test]
    fn a_model_switch_reads_the_new_models_row() {
        let rows = [
            ("model-a", "openai-codex", "", 10, 90, Some(2_000.0)),
            ("model-b", "openai-codex", "", 50, 50, Some(1_000.0)),
        ];
        let found = cache("model-b", "openai-codex", &rows).expect("model-b row");
        assert_eq!(found.read_tokens, 50);
        assert_eq!(cache("model-c", "openai-codex", &rows), None);
        assert_eq!(cache("model-a", "anthropic", &rows), None, "other provider");
    }

    /// Only `openai-codex` gets a lifetime estimate, and a `last_seen` the
    /// clock has not reached yet gives none rather than one over 30 minutes.
    #[test]
    fn a_ttl_needs_codex_and_a_past_last_call() {
        let anthropic = cache(
            "model-c",
            "anthropic",
            &[("model-c", "anthropic", "", 10, 90, Some(1_000.0))],
        )
        .expect("ratio without a ttl");
        assert_eq!((anthropic.read_tokens, anthropic.ttl_seconds), (90, None));
        for last_seen in [Some(4_102_444_800.0), None] {
            let codex = cache(
                "model-a",
                "openai-codex",
                &[("model-a", "openai-codex", "", 10, 90, last_seen)],
            )
            .expect("ratio without a ttl");
            assert_eq!(codex.ttl_seconds, None, "{last_seen:?}");
        }
    }

    /// An older Hermes has no usage table (or no `task` column): the session
    /// still resolves, only without a cache.
    #[test]
    fn a_hermes_without_the_usage_table_still_names_the_session() {
        let found = session(("s1", "model-a", "openai-codex", "", "")).expect("session");
        assert_eq!(found.cache, None);
        let dir = tempfile::tempdir().unwrap();
        write_fixture_db(dir.path(), &[("s1", "model-a", "openai-codex", "", "")]);
        Connection::open(dir.path().join("state.db"))
            .unwrap()
            .execute_batch(
                "CREATE TABLE session_model_usage (session_id TEXT, model TEXT, \
                 billing_provider TEXT, input_tokens INTEGER, cache_read_tokens INTEGER, \
                 cache_write_tokens INTEGER, last_seen REAL);",
            )
            .unwrap();
        let found = lookup(dir.path(), "s1").expect("session");
        assert_eq!(found.model.as_deref(), Some("model-a"));
        assert_eq!(found.cache, None);
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
