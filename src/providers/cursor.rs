//! Cursor Agent CLI subscription quota.
//!
//! Cursor CLI (`cursor-agent`) stores its login in `~/.cursor/auth.json` on
//! macOS and `$XDG_CONFIG_HOME/cursor/auth.json` (else `~/.config/cursor/auth.json`)
//! on Linux, or `$CURSOR_AUTH_FILE`. Only `accessToken` is read. When that file
//! is missing or has no token, the collector falls back to the desktop app's
//! `state.vscdb` key `cursorAuth/accessToken` — the same account the CLI shares
//! — opened read-only and selecting only that one key. The 20 GB SQLite file is
//! never copied, and its mtime is never used as a credential gate: the IDE
//! writes it constantly.
//!
//! Quota is `POST https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage`,
//! the DashboardService call the CLI itself makes, authenticated with that
//! token. That is the Grok/Devin pattern — local credential plus the CLI
//! contract — not a browser cookie. The CLI's usage panel maps Included to
//! `totalPercentUsed` when that field is present, and only then falls back to
//! `includedSpend / limit`. Copying spend/limit first is how a spent dollar
//! cap showed as 0% remaining while the CLI still showed Included 7% used.
//! The CLI panel's three bars map onto the three sidebar slots: Auto → `at`
//! (5h), named-model usage → `api` (7d), Included → `30d`. `billingCycleEnd`
//! is Unix milliseconds.
//! The token is sent only to that fixed host.
//!
//! Model and topic are local. `cli-config.json` `model.displayName` is the
//! account default and what the CLI footer shows after a model switch.
//! A session's `chats/<hash>/<id>/store.db` meta `lastUsedModel` overrides
//! that only when it names a specific model. `default` / `auto` stay on the
//! catalog — Cursor does not rewrite `lastUsedModel` when you change models
//! in an existing session. The topic is the generated
//! session title (`meta.json` `title`, else `store.db` `name`), so a later
//! follow-up does not replace the session name. Placeholder titles such as
//! `New Agent` fall back to the last `<user_query>` in
//! `projects/*/agent-transcripts/<id>/<id>.jsonl`. Cursor panes are never
//! read. Turn token counts are not in that jsonl. Context percent is
//! `store.db` `ConversationStateStructure.token_details` (`used_tokens` /
//! `max_tokens`) — the same numbers the CLI footer prints as `Auto · 8.1%`.
//! Only that protobuf field is read; conversation messages are not. Cache
//! still comes from the interactive CLI's `afterAgentResponse` / `stop` /
//! `preCompact` hooks. A Cursor `statusLine` is not used: installing one
//! replaces the native CLI footer.
//!
//! Everything fails closed. A missing or unreadable percentage yields no
//! window, never "0% used". Cache identity is `sha256("cursor\0" || token)` so
//! another login cannot inherit the previous account's last-good snapshot.
//! The collector never writes credentials, never refreshes or exchanges them,
//! never opens Keychain, and never calls a bare `agent` binary — that name is
//! Grok's on this machine.

use crate::cache::CacheStore;
use crate::model::{
    CacheUsage, ContextUsage, Provider, ProviderSnapshot, ResetAt, UsageWindow, WindowKind,
};
use crate::providers::ProviderError;
use anyhow::{Context, Result};
use rusqlite::{types::ValueRef, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

const USAGE_URL: &str = "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage";
const IDE_ACCESS_TOKEN_KEY: &str = "cursorAuth/accessToken";
/// `auth.json` / `cli-config.json` are a few kilobytes. Anything larger is not
/// a file this collector understands.
const MAX_AUTH_BYTES: u64 = 256 * 1024;
/// Unix timestamps at or above this are milliseconds, not seconds.
const MILLIS_THRESHOLD: u64 = 1_000_000_000_000;
const SESSION_TAIL_BYTES: u64 = 2 * 1024 * 1024;
const MAX_PROJECT_DIRS: usize = 2048;
const MAX_SUMMARY_CHARS: usize = 200;
const MAX_HOOK_MAILBOXES: usize = 128;
const HOOK_MAILBOX_DIR: &str = "cursor-hooks";
/// Composer 2.x's documented context window. Used only when a hook payload
/// does not name `context_window_size`. Other model ids fail closed.
const COMPOSER_2_CONTEXT_WINDOW: u64 = 200_000;
/// Auto/`default` afterAgentResponse omits `context_window_size`. Used only
/// when `store.db` has no `token_details` and preCompact did not name a window.
const AUTO_CONTEXT_WINDOW: u64 = 256_000;
/// Conversation root blobs are a few kilobytes. Anything larger is not the
/// token_details record this collector reads.
const MAX_CONVERSATION_BLOB_BYTES: u64 = 256 * 1024;

struct CursorCredentials {
    access_token: String,
}

impl std::fmt::Debug for CursorCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CursorCredentials")
            .field("access_token", &"[redacted]")
            .finish()
    }
}

/// Fetch Cursor's included monthly pool for the signed-in account.
pub fn fetch_for_sessions(session_ids: &[String]) -> Result<ProviderSnapshot> {
    match fetch_once(session_ids) {
        Err(ProviderError::MissingCredentials) => fetch_once(session_ids),
        result => result,
    }
    .map_err(anyhow::Error::from)
}

fn fetch_once(session_ids: &[String]) -> std::result::Result<ProviderSnapshot, ProviderError> {
    let credentials = read_credentials()?;
    let account_id = account_pin(&credentials.access_token);
    let mut snapshot = fetch_usage(&credentials, CacheStore::now_unix())?
        .with_account_id(Some(account_id))
        .with_model(configured_model());
    enrich_local_sessions(&mut snapshot, session_ids);
    Ok(snapshot)
}

fn fetch_usage(
    credentials: &CursorCredentials,
    now: u64,
) -> std::result::Result<ProviderSnapshot, ProviderError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(10))
        .timeout_write(Duration::from_secs(10))
        .redirects(0)
        .build();
    let response = agent
        .post(USAGE_URL)
        .set(
            "Authorization",
            &format!("Bearer {}", credentials.access_token),
        )
        .set("Content-Type", "application/json")
        .set("Connect-Protocol-Version", "1")
        .send_string("{}")
        .map_err(|error| map_request_error(&error))?;
    let value: Value = response.into_json().map_err(|_| {
        ProviderError::UnsupportedResponse("Cursor usage response is not JSON".into())
    })?;
    parse_current_period_usage(&value, now)
}

/// Parse `GetCurrentPeriodUsage` the way the CLI usage panel does.
///
/// Included is `totalPercentUsed` when present, otherwise spend/limit. The
/// CLI panel's three bars map onto the three sidebar slots: Auto → `at`
/// (5h), named-model usage → `api` (7d), Included → `30d`.
pub fn parse_current_period_usage(
    value: &Value,
    fetched_at_unix: u64,
) -> std::result::Result<ProviderSnapshot, ProviderError> {
    let plan = value
        .get("planUsage")
        .filter(|plan| plan.is_object())
        .ok_or_else(|| ProviderError::UnsupportedResponse("missing planUsage".to_string()))?;
    let included = included_used_percent(plan).ok_or_else(|| {
        ProviderError::UnsupportedResponse("no readable included usage in planUsage".to_string())
    })?;
    let reset = value.get("billingCycleEnd").and_then(parse_cycle_timestamp);
    let mut windows = Vec::new();
    if let Some(auto) = bounded_percent(plan.get("autoPercentUsed")) {
        windows.push(
            UsageWindow::new(WindowKind::FiveHour, auto, reset)
                .map_err(|error| ProviderError::UnsupportedResponse(error.to_string()))?
                .with_source_window("at", None),
        );
    }
    if let Some(api) = bounded_percent(plan.get("apiPercentUsed")) {
        windows.push(
            UsageWindow::new(WindowKind::Weekly, api, reset)
                .map_err(|error| ProviderError::UnsupportedResponse(error.to_string()))?
                .with_source_window("api", None),
        );
    }
    windows.push(
        UsageWindow::new(WindowKind::Monthly, included, reset)
            .map_err(|error| ProviderError::UnsupportedResponse(error.to_string()))?,
    );
    Ok(ProviderSnapshot::new(
        Provider::Cursor,
        windows,
        fetched_at_unix,
    ))
}

/// The CLI helper is `percentage !== undefined ? percentage : used/limit*100`.
fn included_used_percent(plan: &Value) -> Option<f64> {
    if let Some(percent) = bounded_percent(plan.get("totalPercentUsed")) {
        return Some(percent);
    }
    if let Some(limit) = json_number(plan.get("limit")).filter(|limit| *limit > 0.0) {
        if let Some(included) = json_number(plan.get("includedSpend")) {
            return Some((included / limit * 100.0).clamp(0.0, 100.0));
        }
        if let Some(remaining) = json_number(plan.get("remaining")) {
            return Some(((limit - remaining) / limit * 100.0).clamp(0.0, 100.0));
        }
        if let Some(used) = json_number(plan.get("used")) {
            return Some((used / limit * 100.0).clamp(0.0, 100.0));
        }
    }
    None
}

fn bounded_percent(value: Option<&Value>) -> Option<f64> {
    json_number(value).filter(|percent| (0.0..=100.0).contains(percent))
}

fn json_number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64().filter(|value| value.is_finite()),
        Value::String(text) => text
            .trim()
            .parse()
            .ok()
            .filter(|value: &f64| value.is_finite()),
        _ => None,
    }
}

fn parse_cycle_timestamp(value: &Value) -> Option<ResetAt> {
    let millis = match value {
        Value::Number(number) => number.as_u64().or_else(|| {
            number
                .as_f64()
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(|value| value as u64)
        })?,
        Value::String(text) => text.trim().parse().ok()?,
        _ => return None,
    };
    Some(ResetAt::from_unix_seconds(millis_to_seconds(millis)))
}

fn millis_to_seconds(value: u64) -> u64 {
    if value >= MILLIS_THRESHOLD {
        value / 1000
    } else {
        value
    }
}

fn map_request_error(error: &ureq::Error) -> ProviderError {
    match error {
        ureq::Error::Status(401, _) | ureq::Error::Status(403, _) => {
            ProviderError::MissingCredentials
        }
        ureq::Error::Status(code, _) => ProviderError::Request(format!("HTTP {code}")),
        ureq::Error::Transport(error) => ProviderError::Request(error.kind().to_string()),
    }
}

pub fn auth_path() -> Result<PathBuf> {
    if let Some(path) = non_empty_env("CURSOR_AUTH_FILE") {
        return Ok(PathBuf::from(path));
    }
    Ok(cursor_config_dir()?.join("auth.json"))
}

pub fn current_account_id() -> Option<String> {
    Some(account_pin(&read_credentials().ok()?.access_token))
}

/// mtime of the CLI auth file only. The IDE database is written constantly
/// and must not bust a still-valid snapshot.
pub fn auth_mtime_unix() -> Option<u64> {
    CacheStore::file_mtime_unix(&auth_path().ok()?)
}

fn account_pin(access_token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"cursor\0");
    hasher.update(access_token.trim().as_bytes());
    format!("{:x}", hasher.finalize())
}

fn read_credentials() -> std::result::Result<CursorCredentials, ProviderError> {
    if let Ok(path) = auth_path() {
        if path.is_file() {
            match read_auth_file(&path) {
                Ok(credentials) => return Ok(credentials),
                Err(ProviderError::MissingCredentials) => {}
                Err(error) => return Err(error),
            }
        }
    }
    read_ide_access_token()
}

fn read_auth_file(path: &Path) -> std::result::Result<CursorCredentials, ProviderError> {
    let metadata = fs::metadata(path).map_err(|_| ProviderError::MissingCredentials)?;
    if !metadata.is_file() || metadata.len() > MAX_AUTH_BYTES {
        return Err(ProviderError::MissingCredentials);
    }
    let bytes = fs::read(path).map_err(|_| ProviderError::MissingCredentials)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        ProviderError::Unavailable("Cursor auth file is not valid JSON".to_string())
    })?;
    token_from_value(&value).ok_or(ProviderError::MissingCredentials)
}

fn token_from_value(value: &Value) -> Option<CursorCredentials> {
    let access_token = value
        .get("accessToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())?
        .to_string();
    Some(CursorCredentials { access_token })
}

fn read_ide_access_token() -> std::result::Result<CursorCredentials, ProviderError> {
    let path = state_db_path().map_err(|_| ProviderError::MissingCredentials)?;
    if !path.is_file() {
        return Err(ProviderError::MissingCredentials);
    }
    let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| ProviderError::MissingCredentials)?;
    let token = connection
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?1 LIMIT 1",
            [IDE_ACCESS_TOKEN_KEY],
            token_from_row,
        )
        .map_err(|_| ProviderError::MissingCredentials)?;
    let access_token = token.trim();
    if access_token.is_empty() {
        return Err(ProviderError::MissingCredentials);
    }
    Ok(CursorCredentials {
        access_token: access_token.to_string(),
    })
}

fn token_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<String> {
    match row.get_ref(0)? {
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
            Ok(String::from_utf8_lossy(bytes).into_owned())
        }
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn state_db_path() -> Result<PathBuf> {
    if let Some(path) = non_empty_env("CURSOR_STATE_DB") {
        return Ok(PathBuf::from(path));
    }
    #[cfg(target_os = "macos")]
    {
        let home = non_empty_env("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home)
            .join("Library/Application Support/Cursor/User/globalStorage/state.vscdb"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(cursor_ide_config_dir()?.join("User/globalStorage/state.vscdb"))
    }
}

fn cursor_config_dir() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = non_empty_env("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home).join(".cursor"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(xdg) = non_empty_env("XDG_CONFIG_HOME") {
            return Ok(PathBuf::from(xdg).join("cursor"));
        }
        let home = non_empty_env("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home).join(".config/cursor"))
    }
}

#[cfg(not(target_os = "macos"))]
fn cursor_ide_config_dir() -> Result<PathBuf> {
    if let Some(xdg) = non_empty_env("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg).join("Cursor"));
    }
    let home = non_empty_env("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/Cursor"))
}

fn non_empty_env(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

fn cursor_data_home() -> Result<PathBuf> {
    if let Some(path) = non_empty_env("CURSOR_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = non_empty_env("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".cursor"))
}

fn configured_model() -> Option<String> {
    configured_model_from(&read_bounded_json(
        &cursor_data_home().ok()?.join("cli-config.json"),
    )?)
}

fn configured_model_from(config: &Value) -> Option<String> {
    let catalog = config.get("model");
    let catalog_id = catalog.and_then(model_id_from);
    let catalog_name = catalog.and_then(display_name_from_model);
    let selected = config.get("selectedModel").and_then(model_id_from);
    match (selected, catalog_id, catalog_name) {
        (Some(id), Some(cid), Some(name)) if id == cid => Some(name),
        (Some(id), _, _) => Some(id),
        (_, _, Some(name)) => Some(name),
        (_, Some(id), _) => Some(id),
        _ => None,
    }
}

fn model_id_from(value: &Value) -> Option<String> {
    json_text(value.get("modelId")).or_else(|| json_text(value.get("displayModelId")))
}

fn display_name_from_model(value: &Value) -> Option<String> {
    json_text(value.get("displayName"))
        .or_else(|| json_text(value.get("displayNameShort")))
        .or_else(|| json_text(value.get("modelId")))
}

fn json_text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn enrich_local_sessions(snapshot: &mut ProviderSnapshot, session_ids: &[String]) {
    let Ok(home) = cursor_data_home() else {
        return;
    };
    enrich_local_sessions_at(snapshot, &home, session_ids);
    for session_id in session_ids {
        overlay_hook_context(snapshot, Some(session_id));
    }
}

fn enrich_local_sessions_at(snapshot: &mut ProviderSnapshot, home: &Path, session_ids: &[String]) {
    if session_ids.is_empty() {
        return;
    }
    let catalog_id = read_bounded_json(&home.join("cli-config.json"))
        .as_ref()
        .and_then(|config| config.get("model"))
        .and_then(|model| json_text(model.get("modelId")));
    for session_id in session_ids {
        if let Some(model) = session_model(home, session_id, catalog_id.as_deref()) {
            snapshot.session_models.insert(session_id.clone(), model);
        }
        if let Some(prompt) = session_prompt(home, session_id) {
            snapshot
                .session_summaries
                .insert(session_id.clone(), prompt);
        }
        overlay_store_context_at(snapshot, home, Some(session_id));
    }
}

fn session_model(home: &Path, session_id: &str, catalog_id: Option<&str>) -> Option<String> {
    let model_id = last_used_model(home, session_id)?;
    display_name_for_id(home, &model_id, catalog_id)
}

fn display_name_for_id(home: &Path, model_id: &str, catalog_id: Option<&str>) -> Option<String> {
    let config = read_bounded_json(&home.join("cli-config.json"));
    // `lastUsedModel` of default/auto is the CLI's "use the selected model"
    // sentinel. The footer follows cli-config after a switch; the store field
    // does not.
    if is_auto_cursor_model(model_id) {
        return config
            .as_ref()
            .and_then(configured_model_from)
            .or_else(|| Some(friendly_cursor_model_label(model_id)));
    }
    if catalog_id.is_some_and(|id| cursor_model_ids_match(id, model_id)) {
        if let Some(name) = config
            .as_ref()
            .and_then(|config| config.get("model"))
            .and_then(display_name_from_model)
        {
            return Some(name);
        }
    }
    Some(friendly_cursor_model_label(model_id))
}

fn cursor_model_ids_match(catalog_id: &str, model_id: &str) -> bool {
    let catalog = normalize_cursor_model_id(catalog_id);
    let model = normalize_cursor_model_id(model_id);
    catalog == model || model.starts_with(&format!("{catalog}-"))
}

fn normalize_cursor_model_id(model_id: &str) -> String {
    model_id
        .trim()
        .strip_prefix("cursor-")
        .unwrap_or(model_id.trim())
        .to_ascii_lowercase()
}

fn is_auto_cursor_model(model_id: &str) -> bool {
    matches!(
        normalize_cursor_model_id(model_id).as_str(),
        "default" | "auto"
    )
}

fn friendly_cursor_model_label(model_id: &str) -> String {
    if is_auto_cursor_model(model_id) {
        "Auto".to_string()
    } else {
        model_id.to_string()
    }
}

fn last_used_model(home: &Path, session_id: &str) -> Option<String> {
    json_text(store_meta(&find_chat_dir(home, session_id)?.join("store.db"))?.get("lastUsedModel"))
}

fn session_context_from_store(home: &Path, session_id: &str) -> Option<ContextUsage> {
    let (used, max) =
        conversation_token_details(&find_chat_dir(home, session_id)?.join("store.db"))?;
    if max == 0 {
        return None;
    }
    let used_percent = (used as f64 / max as f64 * 100.0).clamp(0.0, 100.0);
    ContextUsage::new(used_percent).ok()
}

/// `used_tokens` / `max_tokens` from the conversation root blob.
///
/// Same fields the CLI footer uses. The blob is a protobuf
/// `ConversationStateStructure`; only field 5 (`token_details`) is walked.
fn conversation_token_details(path: &Path) -> Option<(u64, u64)> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let raw: String = connection
        .query_row("SELECT value FROM meta LIMIT 1", [], |row| row.get(0))
        .ok()?;
    let blob_id = json_text(parse_store_meta(&raw)?.get("latestRootBlobId"))?;
    if !is_blob_id(&blob_id) {
        return None;
    }
    let data: Vec<u8> = connection
        .query_row("SELECT data FROM blobs WHERE id = ?1", [blob_id], |row| {
            row.get(0)
        })
        .ok()?;
    if data.len() as u64 > MAX_CONVERSATION_BLOB_BYTES {
        return None;
    }
    token_details_from_root_blob(&data)
}

fn is_blob_id(value: &str) -> bool {
    (16..=128).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn token_details_from_root_blob(data: &[u8]) -> Option<(u64, u64)> {
    let mut i = 0;
    while i < data.len() {
        let (key, next) = read_varint(data, i)?;
        i = next;
        let field = key >> 3;
        let wire = key & 7;
        match wire {
            0 => {
                let (_, next) = read_varint(data, i)?;
                i = next;
            }
            1 => i = i.checked_add(8).filter(|end| *end <= data.len())?,
            2 => {
                let (len, next) = read_varint(data, i)?;
                i = next;
                let end = i
                    .checked_add(len as usize)
                    .filter(|end| *end <= data.len())?;
                if field == 5 {
                    return token_details_message(&data[i..end]);
                }
                i = end;
            }
            5 => i = i.checked_add(4).filter(|end| *end <= data.len())?,
            _ => return None,
        }
    }
    None
}

fn token_details_message(data: &[u8]) -> Option<(u64, u64)> {
    let mut i = 0;
    let mut used = None;
    let mut max = None;
    while i < data.len() {
        let (key, next) = read_varint(data, i)?;
        i = next;
        let field = key >> 3;
        let wire = key & 7;
        match wire {
            0 => {
                let (value, next) = read_varint(data, i)?;
                i = next;
                if field == 1 {
                    used = Some(value);
                } else if field == 2 {
                    max = Some(value);
                }
            }
            1 => i = i.checked_add(8).filter(|end| *end <= data.len())?,
            2 => {
                let (len, next) = read_varint(data, i)?;
                i = next;
                i = i
                    .checked_add(len as usize)
                    .filter(|end| *end <= data.len())?;
            }
            5 => i = i.checked_add(4).filter(|end| *end <= data.len())?,
            _ => return None,
        }
    }
    Some((used?, max.filter(|max| *max > 0)?))
}

fn read_varint(data: &[u8], mut i: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0;
    while i < data.len() {
        let byte = data[i];
        i += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Some((value, i));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    None
}

fn find_chat_dir(home: &Path, session_id: &str) -> Option<PathBuf> {
    let chats = home.join("chats");
    let Ok(entries) = fs::read_dir(&chats) else {
        return None;
    };
    for entry in entries.flatten().take(MAX_PROJECT_DIRS) {
        let dir = entry.path().join(session_id);
        if dir.is_dir() {
            return Some(dir);
        }
    }
    None
}

fn store_meta(path: &Path) -> Option<Value> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let raw: String = connection
        .query_row("SELECT value FROM meta LIMIT 1", [], |row| row.get(0))
        .ok()?;
    parse_store_meta(&raw)
}

fn parse_store_meta(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed).ok();
    }
    serde_json::from_slice(&decode_hex(trimmed)?).ok()
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(text.len() / 2);
    let chars: Vec<u8> = text.as_bytes().to_vec();
    for pair in chars.chunks_exact(2) {
        bytes.push((from_hex_digit(pair[0])? << 4) | from_hex_digit(pair[1])?);
    }
    Some(bytes)
}

fn from_hex_digit(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

fn session_prompt(home: &Path, session_id: &str) -> Option<String> {
    session_title(home, session_id).or_else(|| {
        let path = find_transcript(home, session_id)?;
        let tail = read_tail(&path, SESSION_TAIL_BYTES)?;
        last_user_query(&tail)
    })
}

fn session_title(home: &Path, session_id: &str) -> Option<String> {
    let dir = find_chat_dir(home, session_id)?;
    titled_value(
        read_bounded_json(&dir.join("meta.json"))
            .as_ref()
            .and_then(|value| value.get("title")),
    )
    .or_else(|| titled_value(store_meta(&dir.join("store.db"))?.get("name")))
}

fn titled_value(value: Option<&Value>) -> Option<String> {
    json_text(value).filter(|title| !is_placeholder_title(title))
}

fn is_placeholder_title(title: &str) -> bool {
    title.eq_ignore_ascii_case("new agent")
}

fn find_transcript(home: &Path, session_id: &str) -> Option<PathBuf> {
    let projects = home.join("projects");
    let Ok(entries) = fs::read_dir(&projects) else {
        return None;
    };
    for entry in entries.flatten().take(MAX_PROJECT_DIRS) {
        let path = entry
            .path()
            .join("agent-transcripts")
            .join(session_id)
            .join(format!("{session_id}.jsonl"));
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn read_tail(path: &Path, max_bytes: u64) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(max_bytes).read_to_end(&mut bytes).ok()?;
    let tail = String::from_utf8_lossy(&bytes);
    if start == 0 {
        return Some(tail.into_owned());
    }
    tail.split_once('\n').map(|(_, lines)| lines.to_string())
}

fn last_user_query(tail: &str) -> Option<String> {
    let mut prompt = None;
    for line in tail.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(text) = user_message_text(&value) else {
            continue;
        };
        if let Some(query) = user_query_from(&text) {
            prompt = Some(query);
        } else if prompt.is_none() {
            prompt = summary_line(&text);
        }
    }
    prompt
}

fn user_message_text(value: &Value) -> Option<String> {
    let content = value
        .pointer("/message/content")
        .or_else(|| value.get("content"))?;
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let mut text = String::new();
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(piece) = block.get("text").and_then(Value::as_str) {
                        text.push_str(piece);
                    }
                }
            }
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn user_query_from(text: &str) -> Option<String> {
    let start = text.find("<user_query>")? + "<user_query>".len();
    let rest = &text[start..];
    let end = rest.find("</user_query>")?;
    summary_line(&rest[..end])
}

fn summary_line(prompt: &str) -> Option<String> {
    let collapsed: String = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let line = collapsed.trim_start_matches([':', '：']).trim();
    (!line.is_empty()).then(|| line.chars().take(MAX_SUMMARY_CHARS).collect())
}

fn read_bounded_json(path: &Path) -> Option<Value> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_AUTH_BYTES {
        return None;
    }
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HookObservation {
    pub conversation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_usage_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_size: Option<u64>,
}

/// Overlay live Cursor context for one session onto a quota snapshot.
///
/// `store.db` `token_details` is the CLI footer (`Auto · 8.1%`). The hook
/// mailbox supplies cache, and fills context only when the store has none.
pub fn overlay_live_context(snapshot: &mut ProviderSnapshot, session_id: Option<&str>) {
    overlay_store_context(snapshot, session_id);
    overlay_hook_context(snapshot, session_id);
}

/// Overlay the hook mailbox for one session onto a quota snapshot.
///
/// Publish reads the cached quota snapshot, which may be older than the last
/// turn's hook. The mailbox is local and cheap, so it is applied at publish
/// time as well as at fetch time.
pub fn overlay_hook_context(snapshot: &mut ProviderSnapshot, session_id: Option<&str>) {
    let Some(root) = CacheStore::from_env().ok() else {
        return;
    };
    overlay_hook_context_at(snapshot, root.root(), session_id);
}

pub fn overlay_store_context(snapshot: &mut ProviderSnapshot, session_id: Option<&str>) {
    let Ok(home) = cursor_data_home() else {
        return;
    };
    overlay_store_context_at(snapshot, &home, session_id);
}

pub fn overlay_store_context_at(
    snapshot: &mut ProviderSnapshot,
    home: &Path,
    session_id: Option<&str>,
) {
    let Some(session_id) = session_id.filter(|id| is_session_id(id)) else {
        return;
    };
    let Some(context) = session_context_from_store(home, session_id) else {
        return;
    };
    snapshot
        .session_contexts
        .insert(session_id.to_string(), context);
}

pub fn overlay_hook_context_at(
    snapshot: &mut ProviderSnapshot,
    state: &Path,
    session_id: Option<&str>,
) {
    let Some(session_id) = session_id.filter(|id| is_session_id(id)) else {
        return;
    };
    overlay_hook_model_at(snapshot, state, session_id);
    let Some(hook) = load_mailbox_context(&state.join(HOOK_MAILBOX_DIR), session_id) else {
        return;
    };
    match snapshot.session_contexts.get_mut(session_id) {
        Some(existing) => {
            if existing.cache.is_none() {
                existing.cache = hook.cache;
            }
        }
        None => {
            snapshot
                .session_contexts
                .insert(session_id.to_string(), hook);
        }
    }
}

fn overlay_hook_model_at(snapshot: &mut ProviderSnapshot, state: &Path, session_id: &str) {
    let Some(hook_model) = load_observation(
        &state
            .join(HOOK_MAILBOX_DIR)
            .join(format!("{session_id}.json")),
    )
    .and_then(|observation| observation.model)
    .filter(|model| !model.trim().is_empty()) else {
        return;
    };
    if is_auto_cursor_model(&hook_model) {
        return;
    }
    let catalog_id = cursor_data_home()
        .ok()
        .and_then(|home| read_bounded_json(&home.join("cli-config.json")))
        .as_ref()
        .and_then(|config| config.get("model"))
        .and_then(|model| json_text(model.get("modelId")));
    let name = cursor_data_home()
        .ok()
        .and_then(|home| display_name_for_id(&home, &hook_model, catalog_id.as_deref()));
    snapshot.session_models.insert(
        session_id.to_string(),
        name.unwrap_or_else(|| friendly_cursor_model_label(&hook_model)),
    );
}

/// Merge one Cursor hook payload into the per-session mailbox.
pub fn save_hook_observation(state: &Path, payload: &Value) -> Result<()> {
    let Some(incoming) = parse_hook_payload(payload) else {
        return Ok(());
    };
    let dir = state.join(HOOK_MAILBOX_DIR);
    fs::create_dir_all(&dir).context("create Cursor hook mailbox")?;
    let path = dir.join(format!("{}.json", incoming.conversation_id));
    let merged = if let Some(previous) = load_observation(&path) {
        merge_hook_observation(previous, incoming)
    } else {
        incoming
    };
    let temporary = dir.join(format!(
        ".{}.{}.tmp",
        merged.conversation_id,
        std::process::id()
    ));
    let bytes = serde_json::to_vec(&merged).context("serialize Cursor hook observation")?;
    fs::write(&temporary, bytes).context("write Cursor hook observation")?;
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(error).context("replace Cursor hook observation");
    }
    prune_mailboxes(&dir);
    Ok(())
}

pub fn parse_hook_payload(value: &Value) -> Option<HookObservation> {
    let conversation_id = json_text(value.get("conversation_id"))
        .or_else(|| json_text(value.get("conversationId")))
        .or_else(|| json_text(value.get("session_id")))
        .or_else(|| json_text(value.get("sessionId")))
        .filter(|id| is_session_id(id))?;
    let model = json_text(value.get("model"));
    Some(HookObservation {
        conversation_id,
        model,
        input_tokens: token_count(value, "input_tokens", "inputTokens"),
        output_tokens: token_count(value, "output_tokens", "outputTokens"),
        cache_read_tokens: token_count(value, "cache_read_tokens", "cacheReadTokens"),
        cache_write_tokens: token_count(value, "cache_write_tokens", "cacheWriteTokens"),
        context_usage_percent: bounded_percent(
            value
                .get("context_usage_percent")
                .or_else(|| value.get("contextUsagePercent")),
        ),
        context_tokens: token_count(value, "context_tokens", "contextTokens"),
        context_window_size: token_count(value, "context_window_size", "contextWindowSize")
            .filter(|window| *window > 0),
    })
}

pub fn merge_hook_observation(
    mut previous: HookObservation,
    incoming: HookObservation,
) -> HookObservation {
    previous.conversation_id = incoming.conversation_id;
    if incoming.model.is_some() {
        previous.model = incoming.model;
    }
    if incoming.input_tokens.is_some() {
        previous.input_tokens = incoming.input_tokens;
        // A new turn's tokens replace the compaction snapshot's percent;
        // keep the window so context can be recomputed.
        if incoming.context_usage_percent.is_none() {
            previous.context_usage_percent = None;
        }
    }
    if incoming.output_tokens.is_some() {
        previous.output_tokens = incoming.output_tokens;
    }
    if incoming.cache_read_tokens.is_some() {
        previous.cache_read_tokens = incoming.cache_read_tokens;
    }
    if incoming.cache_write_tokens.is_some() {
        previous.cache_write_tokens = incoming.cache_write_tokens;
    }
    if incoming.context_usage_percent.is_some() {
        previous.context_usage_percent = incoming.context_usage_percent;
    }
    if incoming.context_tokens.is_some() {
        previous.context_tokens = incoming.context_tokens;
    }
    if incoming.context_window_size.is_some() {
        previous.context_window_size = incoming.context_window_size;
    }
    previous
}

pub fn context_from_observation(observation: &HookObservation) -> Option<ContextUsage> {
    let window = observation.context_window_size.or_else(|| {
        observation
            .model
            .as_deref()
            .and_then(documented_context_window)
    });
    let used = observation.context_usage_percent.or_else(|| {
        let input = observation.input_tokens?;
        let window = window?;
        Some((input as f64 / window as f64 * 100.0).clamp(0.0, 100.0))
    })?;
    let cache = cache_from_observation(observation);
    ContextUsage::new(used)
        .ok()
        .map(|context| context.with_cache(cache))
}

fn cache_from_observation(observation: &HookObservation) -> Option<CacheUsage> {
    let input = observation.input_tokens?;
    let cache_read = observation.cache_read_tokens.unwrap_or(0);
    let cache_write = observation.cache_write_tokens.unwrap_or(0);
    if cache_read > input {
        return None;
    }
    // The CLI's statusLine `current_usage` is
    // `input - cache_read - cache_write` as fresh tokens.
    let fresh = input.saturating_sub(cache_read).saturating_sub(cache_write);
    CacheUsage::from_token_counts(fresh, cache_read, cache_write).map(|cache| {
        cache.with_session_totals(
            crate::model::CacheTotals::from_token_counts(fresh, cache_read, cache_write),
            observation.conversation_id.clone(),
            0,
        )
    })
}

fn documented_context_window(model: &str) -> Option<u64> {
    let id = normalize_cursor_model_id(model);
    if id.starts_with("composer-2") {
        return Some(COMPOSER_2_CONTEXT_WINDOW);
    }
    is_auto_cursor_model(&id).then_some(AUTO_CONTEXT_WINDOW)
}

fn is_session_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn token_count(value: &Value, snake: &str, camel: &str) -> Option<u64> {
    json_u64(value.get(snake)).or_else(|| json_u64(value.get(camel)))
}

fn json_u64(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(number) => number.as_u64().or_else(|| {
            number
                .as_f64()
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(|value| value as u64)
        }),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn load_mailbox_context(dir: &Path, session_id: &str) -> Option<ContextUsage> {
    context_from_observation(&load_observation(&dir.join(format!("{session_id}.json")))?)
}

fn load_observation(path: &Path) -> Option<HookObservation> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn prune_mailboxes(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                let modified = entry.metadata().ok()?.modified().ok()?;
                Some((modified, path))
            } else {
                None
            }
        })
        .collect();
    if files.len() <= MAX_HOOK_MAILBOXES {
        return;
    }
    files.sort_by_key(|(modified, _)| *modified);
    let drop = files.len() - MAX_HOOK_MAILBOXES;
    for (_, path) in files.into_iter().take(drop) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::providers::test_support::env_guard()
    }

    fn period_fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/cursor/current-period-usage.json"
        ))
        .expect("fixture is valid JSON")
    }

    #[test]
    fn the_cli_panel_maps_onto_aut_api_and_30d() {
        let snapshot = parse_current_period_usage(&period_fixture(), 1).expect("snapshot");
        assert_eq!(snapshot.provider, Provider::Cursor);
        let auto = snapshot.window(WindowKind::FiveHour).expect("Auto window");
        assert_eq!(auto.used_percent, 10.5);
        assert_eq!(auto.display_label(), "at");
        let api = snapshot.window(WindowKind::Weekly).expect("API window");
        assert_eq!(api.used_percent, 40.0);
        assert_eq!(api.display_label(), "api");
        let monthly = snapshot.window(WindowKind::Monthly).expect("30d window");
        assert!((monthly.used_percent - 7.165656565656565).abs() < 1e-9);
        assert_eq!(monthly.display_label(), "30d");
        assert_eq!(
            monthly.resets_at.map(ResetAt::unix_seconds),
            Some(1_790_950_387)
        );
    }

    #[test]
    fn the_fixture_carries_no_credentials_or_identity() {
        let text = include_str!("../../tests/fixtures/cursor/current-period-usage.json");
        for field in ["accessToken", "refreshToken", "email", "apiKey"] {
            assert!(
                period_fixture().get(field).is_none(),
                "{field} must stay out of the fixture"
            );
        }
        assert!(!text.contains("eyJ"));
        assert!(!text.contains('@'));
    }

    #[test]
    fn total_percent_used_wins_over_a_spent_dollar_cap() {
        let value = json!({
            "billingCycleEnd": "1790950387000",
            "planUsage": {
                "includedSpend": 2000,
                "limit": 2000,
                "totalPercentUsed": 7.16
            }
        });
        let monthly = parse_current_period_usage(&value, 1)
            .unwrap()
            .window(WindowKind::Monthly)
            .unwrap()
            .used_percent;
        assert_eq!(monthly, 7.16);
    }

    #[test]
    fn spend_over_limit_is_the_fallback_when_total_percent_used_is_absent() {
        let value = json!({"planUsage": {"includedSpend": 1000, "limit": 2000}});
        assert_eq!(
            parse_current_period_usage(&value, 1)
                .unwrap()
                .window(WindowKind::Monthly)
                .unwrap()
                .used_percent,
            50.0
        );
    }

    #[test]
    fn total_percent_used_is_the_fallback_when_spend_and_limit_are_absent() {
        let value = json!({"planUsage": {"totalPercentUsed": 12.5}});
        let snapshot = parse_current_period_usage(&value, 1).unwrap();
        let monthly = snapshot.window(WindowKind::Monthly).unwrap();
        assert_eq!(monthly.used_percent, 12.5);
        assert!(monthly.resets_at.is_none());
    }

    #[test]
    fn remaining_and_used_over_limit_are_accepted() {
        let remaining = json!({"planUsage": {"limit": 100, "remaining": 25}});
        assert_eq!(
            parse_current_period_usage(&remaining, 1)
                .unwrap()
                .window(WindowKind::Monthly)
                .unwrap()
                .used_percent,
            75.0
        );
        let used = json!({"planUsage": {"limit": 100, "used": 10}});
        assert_eq!(
            parse_current_period_usage(&used, 1)
                .unwrap()
                .window(WindowKind::Monthly)
                .unwrap()
                .used_percent,
            10.0
        );
    }

    #[test]
    fn unix_seconds_reset_is_not_divided() {
        let value = json!({
            "billingCycleEnd": 1_790_950_387,
            "planUsage": {"includedSpend": 1, "limit": 2}
        });
        assert_eq!(
            parse_current_period_usage(&value, 1)
                .unwrap()
                .window(WindowKind::Monthly)
                .unwrap()
                .resets_at
                .map(ResetAt::unix_seconds),
            Some(1_790_950_387)
        );
    }

    #[test]
    fn missing_or_unreadable_plan_usage_is_unsupported() {
        assert!(matches!(
            parse_current_period_usage(&json!({}), 1),
            Err(ProviderError::UnsupportedResponse(_))
        ));
        assert!(matches!(
            parse_current_period_usage(&json!({"planUsage": {}}), 1),
            Err(ProviderError::UnsupportedResponse(_))
        ));
        assert!(matches!(
            parse_current_period_usage(&json!({"planUsage": {"totalPercentUsed": 140}}), 1),
            Err(ProviderError::UnsupportedResponse(_))
        ));
    }

    #[test]
    fn account_pin_is_scoped_and_pinned() {
        assert_eq!(
            account_pin(" token "),
            format!("{:x}", Sha256::digest(b"cursor\0token"))
        );
        assert_ne!(
            account_pin("token"),
            crate::providers::credential_id("token")
        );
    }

    #[test]
    fn the_credentials_debug_output_is_redacted() {
        let credentials = CursorCredentials {
            access_token: "secret-token".into(),
        };
        assert!(!format!("{credentials:?}").contains("secret-token"));
    }

    #[test]
    fn auth_json_wins_over_the_ide_database() {
        let _guard = env_guard();
        let dir = tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        let db = dir.path().join("state.vscdb");
        fs::write(&auth, r#"{"accessToken":"cli-token"}"#).unwrap();
        write_state_db(&db, "ide-token");
        std::env::set_var("CURSOR_AUTH_FILE", &auth);
        std::env::set_var("CURSOR_STATE_DB", &db);
        let credentials = read_credentials().unwrap();
        std::env::remove_var("CURSOR_AUTH_FILE");
        std::env::remove_var("CURSOR_STATE_DB");
        assert_eq!(credentials.access_token, "cli-token");
    }

    #[test]
    fn an_empty_auth_token_falls_through_to_the_ide_database() {
        let _guard = env_guard();
        let dir = tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        let db = dir.path().join("state.vscdb");
        fs::write(&auth, r#"{"accessToken":"  "}"#).unwrap();
        write_state_db(&db, "ide-token");
        std::env::set_var("CURSOR_AUTH_FILE", &auth);
        std::env::set_var("CURSOR_STATE_DB", &db);
        let credentials = read_credentials().unwrap();
        std::env::remove_var("CURSOR_AUTH_FILE");
        std::env::remove_var("CURSOR_STATE_DB");
        assert_eq!(credentials.access_token, "ide-token");
    }

    #[test]
    fn a_malformed_auth_file_does_not_borrow_the_ide_token() {
        let _guard = env_guard();
        let dir = tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        let db = dir.path().join("state.vscdb");
        fs::write(&auth, "not-json").unwrap();
        write_state_db(&db, "ide-token");
        std::env::set_var("CURSOR_AUTH_FILE", &auth);
        std::env::set_var("CURSOR_STATE_DB", &db);
        let error = read_credentials().unwrap_err();
        std::env::remove_var("CURSOR_AUTH_FILE");
        std::env::remove_var("CURSOR_STATE_DB");
        assert!(matches!(error, ProviderError::Unavailable(_)));
    }

    #[test]
    fn missing_credentials_are_missing() {
        let _guard = env_guard();
        let dir = tempdir().unwrap();
        std::env::set_var("CURSOR_AUTH_FILE", dir.path().join("absent.json"));
        std::env::set_var("CURSOR_STATE_DB", dir.path().join("absent.vscdb"));
        let error = read_credentials().unwrap_err();
        std::env::remove_var("CURSOR_AUTH_FILE");
        std::env::remove_var("CURSOR_STATE_DB");
        assert!(matches!(error, ProviderError::MissingCredentials));
    }

    #[test]
    fn xdg_config_home_is_not_the_macos_auth_dir() {
        let _guard = env_guard();
        let dir = tempdir().unwrap();
        let xdg = dir.path().join("xdg");
        let previous_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let previous_auth = std::env::var_os("CURSOR_AUTH_FILE");
        std::env::remove_var("CURSOR_AUTH_FILE");
        std::env::set_var("XDG_CONFIG_HOME", &xdg);
        let path = auth_path().unwrap();
        match previous_auth {
            Some(value) => std::env::set_var("CURSOR_AUTH_FILE", value),
            None => std::env::remove_var("CURSOR_AUTH_FILE"),
        }
        match previous_xdg {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        #[cfg(target_os = "macos")]
        {
            assert_ne!(path, xdg.join("cursor/auth.json"));
            assert!(
                path.ends_with(".cursor/auth.json"),
                "macOS auth path {path:?}"
            );
        }
        #[cfg(not(target_os = "macos"))]
        assert_eq!(path, xdg.join("cursor/auth.json"));
    }

    #[test]
    fn ide_database_mtime_is_not_the_credential_gate() {
        let _guard = env_guard();
        let dir = tempdir().unwrap();
        let db = dir.path().join("state.vscdb");
        write_state_db(&db, "ide-token");
        std::env::set_var("CURSOR_AUTH_FILE", dir.path().join("absent.json"));
        std::env::set_var("CURSOR_STATE_DB", &db);
        let mtime = auth_mtime_unix();
        let account = current_account_id();
        std::env::remove_var("CURSOR_AUTH_FILE");
        std::env::remove_var("CURSOR_STATE_DB");
        assert!(mtime.is_none());
        assert_eq!(account.as_deref(), Some(account_pin("ide-token").as_str()));
    }

    #[test]
    fn a_user_query_from_the_transcript_is_the_topic() {
        let tail = include_str!("../../tests/fixtures/cursor/session-tail.jsonl");
        assert_eq!(last_user_query(tail).as_deref(), Some("hi"));
    }

    #[test]
    fn a_multiline_user_query_collapses_to_one_summary_line() {
        let tail = concat!(
            r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nThoroughness: very thorough.\n\nExplore the repo\n</user_query>"}]}}"#,
            "\n"
        );
        assert_eq!(
            last_user_query(tail).as_deref(),
            Some("Thoroughness: very thorough. Explore the repo")
        );
    }

    #[test]
    fn a_leading_fullwidth_colon_is_stripped_from_the_query() {
        let tail = concat!(
            r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\n： https://example.test/pull/5 review this\n</user_query>"}]}}"#,
            "\n"
        );
        assert_eq!(
            last_user_query(tail).as_deref(),
            Some("https://example.test/pull/5 review this")
        );
    }

    #[test]
    fn cli_config_display_name_is_the_configured_model() {
        let config = json!({
            "model": {
                "modelId": "composer-2.5",
                "displayName": "Composer 2.5"
            },
            "selectedModel": { "modelId": "composer-2.5" }
        });
        assert_eq!(
            configured_model_from(&config).as_deref(),
            Some("Composer 2.5")
        );
        let switched = json!({
            "model": {
                "modelId": "composer-2.5",
                "displayName": "Composer 2.5"
            },
            "selectedModel": { "modelId": "cursor-grok-4.6-high" }
        });
        assert_eq!(
            configured_model_from(&switched).as_deref(),
            Some("cursor-grok-4.6-high")
        );
        assert_eq!(configured_model_from(&json!({})), None);
    }

    #[test]
    fn default_and_auto_last_used_models_follow_the_cli_config_footer() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        fs::write(
            home.join("cli-config.json"),
            r#"{"model":{"modelId":"grok-4.6","displayName":"Cursor Grok 4.6 High"}}"#,
        )
        .unwrap();
        for (session_id, last_used) in [
            ("11111111-1111-1111-1111-111111111111", "default"),
            ("22222222-2222-2222-2222-222222222222", "auto"),
        ] {
            let chat = home.join("chats").join("hash").join(session_id);
            fs::create_dir_all(&chat).unwrap();
            write_store_meta(
                &chat.join("store.db"),
                &format!(r#"{{"agentId":"{session_id}","lastUsedModel":"{last_used}"}}"#),
            );
            let mut snapshot = ProviderSnapshot::new(Provider::Cursor, vec![], 1);
            enrich_local_sessions_at(&mut snapshot, home, &[session_id.to_string()]);
            assert_eq!(
                snapshot.session_models.get(session_id).map(String::as_str),
                Some("Cursor Grok 4.6 High"),
                "{last_used}"
            );
        }
    }

    #[test]
    fn a_default_last_used_model_stays_auto_when_cli_config_is_auto() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let session_id = "55555555-5555-5555-5555-555555555555";
        fs::write(
            home.join("cli-config.json"),
            r#"{"model":{"modelId":"default","displayName":"Auto"}}"#,
        )
        .unwrap();
        let chat = home.join("chats").join("hash").join(session_id);
        fs::create_dir_all(&chat).unwrap();
        write_store_meta(
            &chat.join("store.db"),
            &format!(r#"{{"agentId":"{session_id}","lastUsedModel":"default"}}"#),
        );
        let mut snapshot = ProviderSnapshot::new(Provider::Cursor, vec![], 1);
        enrich_local_sessions_at(&mut snapshot, home, &[session_id.to_string()]);
        assert_eq!(
            snapshot.session_models.get(session_id).map(String::as_str),
            Some("Auto")
        );
    }

    #[test]
    fn a_grok_last_used_model_uses_the_cli_config_display_name() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let session_id = "33333333-3333-3333-3333-333333333333";
        fs::write(
            home.join("cli-config.json"),
            r#"{"model":{"modelId":"grok-4.6","displayName":"Cursor Grok 4.6 High"}}"#,
        )
        .unwrap();
        let chat = home.join("chats").join("hash").join(session_id);
        fs::create_dir_all(&chat).unwrap();
        write_store_meta(
            &chat.join("store.db"),
            &format!(r#"{{"agentId":"{session_id}","lastUsedModel":"grok-4.6"}}"#),
        );
        let mut snapshot = ProviderSnapshot::new(Provider::Cursor, vec![], 1);
        enrich_local_sessions_at(&mut snapshot, home, &[session_id.to_string()]);
        assert_eq!(
            snapshot.session_models.get(session_id).map(String::as_str),
            Some("Cursor Grok 4.6 High")
        );
    }

    #[test]
    fn a_hook_grok_model_replaces_a_stale_auto_store_label() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let state = dir.path().join("plugin-state");
        let session_id = "44444444-4444-4444-4444-444444444444";
        fs::write(
            home.join("cli-config.json"),
            r#"{"model":{"modelId":"grok-4.6","displayName":"Cursor Grok 4.6 High"}}"#,
        )
        .unwrap();
        let chat = home.join("chats").join("hash").join(session_id);
        fs::create_dir_all(&chat).unwrap();
        write_store_meta(
            &chat.join("store.db"),
            &format!(r#"{{"agentId":"{session_id}","lastUsedModel":"default"}}"#),
        );
        let mailbox = state.join("cursor-hooks");
        fs::create_dir_all(&mailbox).unwrap();
        fs::write(
            mailbox.join(format!("{session_id}.json")),
            r#"{"conversation_id":"44444444-4444-4444-4444-444444444444","model":"cursor-grok-4.6-high"}"#,
        )
        .unwrap();

        let mut snapshot = ProviderSnapshot::new(Provider::Cursor, vec![], 1);
        enrich_local_sessions_at(&mut snapshot, home, &[session_id.to_string()]);
        assert_eq!(
            snapshot.session_models.get(session_id).map(String::as_str),
            Some("Cursor Grok 4.6 High")
        );
        let _guard = env_guard();
        std::env::set_var("CURSOR_HOME", home);
        overlay_hook_context_at(&mut snapshot, &state, Some(session_id));
        std::env::remove_var("CURSOR_HOME");
        assert_eq!(
            snapshot.session_models.get(session_id).map(String::as_str),
            Some("Cursor Grok 4.6 High")
        );
    }

    #[test]
    fn a_session_store_last_used_model_is_mapped_through_cli_config() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let session_id = "50b33403-da5a-40f4-bb9e-5fc3566f91a4";
        fs::write(
            home.join("cli-config.json"),
            r#"{"model":{"modelId":"composer-2.5","displayName":"Composer 2.5"}}"#,
        )
        .unwrap();
        let chat = home.join("chats").join("hash").join(session_id);
        fs::create_dir_all(&chat).unwrap();
        write_store_meta(
            &chat.join("store.db"),
            r#"{"agentId":"50b33403-da5a-40f4-bb9e-5fc3566f91a4","lastUsedModel":"composer-2.5"}"#,
        );
        let transcripts = home
            .join("projects")
            .join("repo")
            .join("agent-transcripts")
            .join(session_id);
        fs::create_dir_all(&transcripts).unwrap();
        fs::write(
            transcripts.join(format!("{session_id}.jsonl")),
            include_str!("../../tests/fixtures/cursor/session-tail.jsonl"),
        )
        .unwrap();

        let mut snapshot = ProviderSnapshot::new(Provider::Cursor, vec![], 1);
        enrich_local_sessions_at(&mut snapshot, home, &[session_id.to_string()]);
        assert_eq!(
            snapshot.session_models.get(session_id).map(String::as_str),
            Some("Composer 2.5")
        );
        assert_eq!(
            snapshot
                .session_summaries
                .get(session_id)
                .map(String::as_str),
            Some("hi")
        );
    }

    #[test]
    fn a_generated_session_title_wins_over_the_last_user_query() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let session_id = "40ca7390-d1e9-463d-b852-68a2693b1b75";
        let chat = home.join("chats").join("hash").join(session_id);
        fs::create_dir_all(&chat).unwrap();
        write_store_meta(
            &chat.join("store.db"),
            r#"{"agentId":"40ca7390-d1e9-463d-b852-68a2693b1b75","name":"Project Architecture Review","lastUsedModel":"default"}"#,
        );
        write_follow_up_transcript(home, session_id);
        let mut snapshot = ProviderSnapshot::new(Provider::Cursor, vec![], 1);
        enrich_local_sessions_at(&mut snapshot, home, &[session_id.to_string()]);
        assert_eq!(
            snapshot
                .session_summaries
                .get(session_id)
                .map(String::as_str),
            Some("Project Architecture Review")
        );
    }

    #[test]
    fn meta_json_title_is_used_when_store_name_is_a_placeholder() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let session_id = "40ca7390-d1e9-463d-b852-68a2693b1b75";
        let chat = home.join("chats").join("hash").join(session_id);
        fs::create_dir_all(&chat).unwrap();
        fs::write(
            chat.join("meta.json"),
            r#"{"schemaVersion":1,"title":"Project Architecture Review"}"#,
        )
        .unwrap();
        write_store_meta(
            &chat.join("store.db"),
            r#"{"agentId":"40ca7390-d1e9-463d-b852-68a2693b1b75","name":"New Agent"}"#,
        );
        write_follow_up_transcript(home, session_id);
        let mut snapshot = ProviderSnapshot::new(Provider::Cursor, vec![], 1);
        enrich_local_sessions_at(&mut snapshot, home, &[session_id.to_string()]);
        assert_eq!(
            snapshot
                .session_summaries
                .get(session_id)
                .map(String::as_str),
            Some("Project Architecture Review")
        );
    }

    #[test]
    fn placeholder_new_agent_title_falls_back_to_the_user_query() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let session_id = "50b33403-da5a-40f4-bb9e-5fc3566f91a4";
        let chat = home.join("chats").join("hash").join(session_id);
        fs::create_dir_all(&chat).unwrap();
        write_store_meta(
            &chat.join("store.db"),
            r#"{"agentId":"50b33403-da5a-40f4-bb9e-5fc3566f91a4","name":"New Agent"}"#,
        );
        let transcripts = home
            .join("projects")
            .join("repo")
            .join("agent-transcripts")
            .join(session_id);
        fs::create_dir_all(&transcripts).unwrap();
        fs::write(
            transcripts.join(format!("{session_id}.jsonl")),
            include_str!("../../tests/fixtures/cursor/session-tail.jsonl"),
        )
        .unwrap();
        let mut snapshot = ProviderSnapshot::new(Provider::Cursor, vec![], 1);
        enrich_local_sessions_at(&mut snapshot, home, &[session_id.to_string()]);
        assert_eq!(
            snapshot
                .session_summaries
                .get(session_id)
                .map(String::as_str),
            Some("hi")
        );
    }

    #[test]
    fn after_agent_response_maps_cache_the_way_the_cli_statusline_does() {
        let observation = parse_hook_payload(&json!({
            "conversation_id": "50b33403-da5a-40f4-bb9e-5fc3566f91a4",
            "model": "composer-2.5",
            "input_tokens": 1000,
            "output_tokens": 40,
            "cache_read_tokens": 800,
            "cache_write_tokens": 50
        }))
        .unwrap();
        let context = context_from_observation(&observation).unwrap();
        assert!((context.used_percent - 0.5).abs() < 1e-9);
        let cache = context.cache.unwrap();
        assert_eq!(cache.fresh_input_tokens, 150);
        assert_eq!(cache.read_tokens, 800);
        assert_eq!(cache.creation_tokens, 50);
        assert!((cache.hit_percent - 80.0).abs() < 1e-9);
    }

    #[test]
    fn precompact_percent_is_preferred_over_a_token_ratio() {
        let observation = parse_hook_payload(&json!({
            "conversation_id": "50b33403-da5a-40f4-bb9e-5fc3566f91a4",
            "context_usage_percent": 85.0,
            "context_tokens": 170000,
            "context_window_size": 200000
        }))
        .unwrap();
        let context = context_from_observation(&observation).unwrap();
        assert!((context.used_percent - 85.0).abs() < 1e-9);
        assert!(context.cache.is_none());
    }

    #[test]
    fn a_later_turn_recomputes_context_from_tokens_and_keeps_the_window() {
        let compact = parse_hook_payload(&json!({
            "conversation_id": "50b33403-da5a-40f4-bb9e-5fc3566f91a4",
            "context_usage_percent": 85.0,
            "context_window_size": 200000
        }))
        .unwrap();
        let turn = parse_hook_payload(&json!({
            "conversation_id": "50b33403-da5a-40f4-bb9e-5fc3566f91a4",
            "model": "composer-2.5",
            "input_tokens": 40000,
            "cache_read_tokens": 30000,
            "cache_write_tokens": 0
        }))
        .unwrap();
        let merged = merge_hook_observation(compact, turn);
        assert_eq!(merged.context_window_size, Some(200_000));
        assert!(merged.context_usage_percent.is_none());
        let context = context_from_observation(&merged).unwrap();
        assert!((context.used_percent - 20.0).abs() < 1e-9);
        assert_eq!(context.cache.unwrap().read_tokens, 30_000);
    }

    #[test]
    fn an_unknown_model_without_a_window_does_not_guess_context() {
        let observation = parse_hook_payload(&json!({
            "conversation_id": "50b33403-da5a-40f4-bb9e-5fc3566f91a4",
            "model": "gpt-5",
            "input_tokens": 1000,
            "cache_read_tokens": 800,
            "cache_write_tokens": 0
        }))
        .unwrap();
        assert!(context_from_observation(&observation).is_none());
    }

    #[test]
    fn conversation_token_details_match_the_cli_footer_percent() {
        let mut details = Vec::new();
        put_varint_field(&mut details, 1, 20_696);
        put_varint_field(&mut details, 2, 256_000);
        let mut root = Vec::new();
        put_len_field(&mut root, 1, &[0u8; 32]);
        put_len_field(&mut root, 5, &details);
        assert_eq!(token_details_from_root_blob(&root), Some((20_696, 256_000)));
        let used: f64 = 20_696.0 / 256_000.0 * 100.0;
        assert!(((used * 10.0).round() / 10.0 - 8.1).abs() < 1e-9);
    }

    #[test]
    fn a_root_blob_without_token_details_yields_no_context() {
        let mut root = Vec::new();
        put_len_field(&mut root, 1, &[0u8; 32]);
        put_varint_field(&mut root, 10, 1);
        assert_eq!(token_details_from_root_blob(&root), None);
    }

    #[test]
    fn store_token_details_are_the_session_context_percent() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let session_id = "7d18d6db-00ce-4042-be42-e4185dd18b9d";
        let blob_id = "9e8714f50925cd7a4695fa5fe229a8ac8f70ee8c0b80312232c6df44fa6f8b4e";
        let mut details = Vec::new();
        put_varint_field(&mut details, 1, 20_696);
        put_varint_field(&mut details, 2, 256_000);
        let mut root = Vec::new();
        put_len_field(&mut root, 5, &details);
        write_store(
            &home
                .join("chats")
                .join("hash")
                .join(session_id)
                .join("store.db"),
            &format!(r#"{{"agentId":"{session_id}","latestRootBlobId":"{blob_id}"}}"#),
            blob_id,
            &root,
        );
        let mut snapshot = ProviderSnapshot::new(Provider::Cursor, vec![], 1);
        overlay_store_context_at(&mut snapshot, home, Some(session_id));
        let context = snapshot.context_for_session(Some(session_id)).unwrap();
        assert!((context.used_percent - 20_696.0 / 256_000.0 * 100.0).abs() < 1e-9);
        assert!(context.cache.is_none());
    }

    #[test]
    fn store_token_details_win_over_hook_input_and_keep_hook_cache() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let state = dir.path().join("plugin-state");
        let session_id = "7d18d6db-00ce-4042-be42-e4185dd18b9d";
        let blob_id = "9e8714f50925cd7a4695fa5fe229a8ac8f70ee8c0b80312232c6df44fa6f8b4e";
        let mut details = Vec::new();
        put_varint_field(&mut details, 1, 20_696);
        put_varint_field(&mut details, 2, 256_000);
        let mut root = Vec::new();
        put_len_field(&mut root, 5, &details);
        write_store(
            &home
                .join("chats")
                .join("hash")
                .join(session_id)
                .join("store.db"),
            &format!(r#"{{"agentId":"{session_id}","latestRootBlobId":"{blob_id}"}}"#),
            blob_id,
            &root,
        );
        save_hook_observation(
            &state,
            &json!({
                "conversation_id": session_id,
                "model": "default",
                "input_tokens": 20673,
                "cache_read_tokens": 20480,
                "cache_write_tokens": 0
            }),
        )
        .unwrap();
        let mut snapshot = ProviderSnapshot::new(Provider::Cursor, vec![], 1);
        overlay_store_context_at(&mut snapshot, home, Some(session_id));
        overlay_hook_context_at(&mut snapshot, &state, Some(session_id));
        let context = snapshot.context_for_session(Some(session_id)).unwrap();
        assert!((context.used_percent - 20_696.0 / 256_000.0 * 100.0).abs() < 1e-9);
        assert!(context.cache.is_some());
    }

    #[test]
    fn auto_and_default_use_the_precompact_window_when_the_hook_omits_it() {
        for model in ["default", "Auto", "auto"] {
            let observation = parse_hook_payload(&json!({
                "conversation_id": "50b33403-da5a-40f4-bb9e-5fc3566f91a4",
                "model": model,
                "input_tokens": 25600,
                "cache_read_tokens": 20000,
                "cache_write_tokens": 0
            }))
            .unwrap();
            let context = context_from_observation(&observation)
                .unwrap_or_else(|| panic!("{model} should render context"));
            assert!(
                (context.used_percent - 10.0).abs() < 1e-9,
                "{model} used_percent {}",
                context.used_percent
            );
        }
    }

    #[test]
    fn impossible_cache_counters_yield_context_without_cache() {
        let observation = parse_hook_payload(&json!({
            "conversation_id": "50b33403-da5a-40f4-bb9e-5fc3566f91a4",
            "model": "composer-2.5",
            "input_tokens": 10,
            "cache_read_tokens": 20,
            "cache_write_tokens": 0
        }))
        .unwrap();
        let context = context_from_observation(&observation).unwrap();
        assert!(context.cache.is_none());
    }

    #[test]
    fn hook_mailbox_is_attributed_to_the_named_session_only() {
        let dir = tempdir().unwrap();
        save_hook_observation(
            dir.path(),
            &json!({
                "conversation_id": "50b33403-da5a-40f4-bb9e-5fc3566f91a4",
                "model": "composer-2.5",
                "input_tokens": 20000,
                "cache_read_tokens": 15000,
                "cache_write_tokens": 0
            }),
        )
        .unwrap();
        let mut snapshot = ProviderSnapshot::new(Provider::Cursor, vec![], 1);
        overlay_hook_context_at(
            &mut snapshot,
            dir.path(),
            Some("50b33403-da5a-40f4-bb9e-5fc3566f91a4"),
        );
        overlay_hook_context_at(&mut snapshot, dir.path(), Some("other-session"));
        assert!(snapshot
            .session_contexts
            .contains_key("50b33403-da5a-40f4-bb9e-5fc3566f91a4"));
        assert!(!snapshot.session_contexts.contains_key("other-session"));
        let context = snapshot
            .context_for_session(Some("50b33403-da5a-40f4-bb9e-5fc3566f91a4"))
            .unwrap();
        assert!((context.used_percent - 10.0).abs() < 1e-9);
    }

    fn write_follow_up_transcript(home: &Path, session_id: &str) {
        let transcripts = home
            .join("projects")
            .join("repo")
            .join("agent-transcripts")
            .join(session_id);
        fs::create_dir_all(&transcripts).unwrap();
        fs::write(
            transcripts.join(format!("{session_id}.jsonl")),
            concat!(
                r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\n你从头回顾一下我们讨论的待办和问题处都做了吗？\n</user_query>"}]}}"#,
                "\n"
            ),
        )
        .unwrap();
    }

    fn put_varint(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    fn put_varint_field(out: &mut Vec<u8>, field: u64, value: u64) {
        put_varint(out, field << 3);
        put_varint(out, value);
    }

    fn put_len_field(out: &mut Vec<u8>, field: u64, payload: &[u8]) {
        put_varint(out, (field << 3) | 2);
        put_varint(out, payload.len() as u64);
        out.extend_from_slice(payload);
    }

    fn write_store(path: &Path, meta_json: &str, blob_id: &str, blob: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let connection = Connection::open(path).unwrap();
        connection
            .execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)", [])
            .unwrap();
        connection
            .execute("CREATE TABLE blobs (id TEXT PRIMARY KEY, data BLOB)", [])
            .unwrap();
        let hex: String = meta_json
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        connection
            .execute(
                "INSERT INTO meta (key, value) VALUES ('0', ?1)",
                rusqlite::params![hex],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO blobs (id, data) VALUES (?1, ?2)",
                rusqlite::params![blob_id, blob],
            )
            .unwrap();
    }

    fn write_store_meta(path: &Path, json: &str) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)", [])
            .unwrap();
        let hex: String = json
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        connection
            .execute(
                "INSERT INTO meta (key, value) VALUES ('0', ?1)",
                rusqlite::params![hex],
            )
            .unwrap();
    }

    fn write_state_db(path: &Path, token: &str) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute(
                "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
                rusqlite::params![IDE_ACCESS_TOKEN_KEY, token],
            )
            .unwrap();
    }
}
