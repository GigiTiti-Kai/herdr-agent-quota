//! Account limits a live Hermes session reports about its own credential.
//!
//! This plugin cannot tell which credential serves a Hermes session, so it
//! never fetches Hermes quota itself. The bridge plugin installed into Hermes
//! (`assets/hermes/herdr-agent-quota/`) runs inside the Hermes process, asks
//! Hermes's usage fetcher about exactly the key the live agent holds, and
//! writes `<state>/hermes-bridge/<session id>.json`. This module only reads
//! that mailbox.
//!
//! A record carries the producer's current route epoch and the epoch its
//! quota was fetched under. The producer starts a new epoch, and drops the
//! quota, whenever the provider, endpoint, pool entry, or token changes, and
//! holds an exclusive lock on `<session id>.lock` for as long as it tracks
//! the session. A reading is used only when every gate below holds; anything
//! else is no reading at all, never an older or a neighbouring one.
//!
//! A supported route may also carry `principal`: [`chatgpt_principal_stamp`]
//! of the ChatGPT account and user (`sub`) the live access token names. It is
//! the only value in a record that another process can compare, and it is
//! compared with the same stamp of the Codex login the Codex row belongs to,
//! never with a raw id. Both halves matter: members of one ChatGPT workspace
//! share the account id but each has their own quota.
//!
//! A native Anthropic route (never supported: no quota is fetched for it) may
//! carry `claude_credentials`: [`claude_credentials_stamp`] of Claude Code's
//! credentials file, written once the producer saw the live key equal the
//! token in that file. The Claude row reads the same file, so an equal stamp
//! is a session on that row's login. The file follows one login however often
//! its token is refreshed, which is why the path is the identity and the
//! token never is.

use crate::model::{Provider, ProviderSnapshot, ResetAt, UsageWindow, WindowKind};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;

pub const MAILBOX_DIR: &str = "hermes-bridge";
/// The only route the bridge fetches for.
pub const BRIDGED_PROVIDER: &str = "openai-codex";
/// The route whose records may carry a Claude credentials file stamp.
pub const CLAUDE_ROUTE_PROVIDER: &str = "anthropic";

const SCHEMA: u64 = 1;
const MAX_RECORD_BYTES: u64 = 16 * 1024;
/// Clock disagreement tolerated between the producer and this reader.
const FUTURE_SLACK_SECONDS: u64 = 120;
/// How long a route check stays good. The producer re-stamps `observed_at`
/// every 30 seconds while its poller or a hook still looks at the live agent.
const ROUTE_TTL_SECONDS: u64 = 120;
/// Mailbox entries one scan looks at. Each live session owns two files, and
/// the producer prunes what dead ones leave behind.
const MAX_SCANNED_ENTRIES: usize = 512;
/// Domain of [`chatgpt_principal_stamp`]. The bridge plugin hashes the same
/// bytes; `the_principal_stamp_is_pinned` keeps the two from drifting apart.
const PRINCIPAL_STAMP_DOMAIN: &[u8] = b"herdr-agent-quota/chatgpt-principal\0";
/// Domain of [`claude_credentials_stamp`]. The bridge plugin hashes the same
/// bytes; `the_claude_credentials_stamp_is_pinned` keeps the two together.
const CLAUDE_CREDENTIALS_STAMP_DOMAIN: &[u8] = b"herdr-agent-quota/claude-credentials-file\0";

fn hex_sha256(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The comparable form of a ChatGPT principal: lowercase hex of
/// `sha256("herdr-agent-quota/chatgpt-principal\0" || account id || "\0" || sub)`,
/// both taken from an access token's own claims.
///
/// Neither raw value leaves the process that read it. The bridge stamps the
/// principal its live access token names; this plugin stamps the current
/// Codex login's. Equal stamps are the same seat of the same account, the
/// identity Hermes itself uses to tell two Codex credentials apart.
pub fn chatgpt_principal_stamp(account_id: &str, subject: &str) -> String {
    hex_sha256(&[
        PRINCIPAL_STAMP_DOMAIN,
        account_id.as_bytes(),
        &[0],
        subject.as_bytes(),
    ])
}

/// The comparable form of a Claude credentials file: lowercase hex of
/// `sha256("herdr-agent-quota/claude-credentials-file\0" || resolved path)`.
///
/// `resolved` must already be canonical (symlinks and `..` resolved); the
/// bridge hashes `os.path.realpath(..., strict=True)` of the file Hermes
/// borrows its Claude login from, and the Claude row hashes
/// `fs::canonicalize` of the file it reads. Neither side writes the token.
/// `None` where paths are not bytes: the bridge never runs there.
#[cfg(unix)]
pub fn claude_credentials_stamp(resolved: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    Some(hex_sha256(&[
        CLAUDE_CREDENTIALS_STAMP_DOMAIN,
        resolved.as_os_str().as_bytes(),
    ]))
}

#[cfg(not(unix))]
pub fn claude_credentials_stamp(_resolved: &Path) -> Option<String> {
    None
}

/// One mailbox record that passed every gate.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub snapshot: ProviderSnapshot,
    /// [`chatgpt_principal_stamp`] of the account and user the session's live
    /// token names, when it names both.
    pub principal: Option<String>,
}

/// The current route of a record, once its route-level gates hold.
struct Route {
    epoch: u64,
    identity: String,
    principal: Option<String>,
}

/// The quota of the credential serving `session_id` in `pane_id` right now.
///
/// `provider_id` is what Hermes's own session row names; the record must
/// agree with it, so an idle `/model` switch the producer has not written yet
/// reads as unknown rather than as the previous provider's bars.
pub fn load(
    state: &Path,
    pane_id: &str,
    session_id: &str,
    provider_id: &str,
    now_unix: u64,
) -> Option<Reading> {
    if provider_id != BRIDGED_PROVIDER || !is_session_id(session_id) {
        return None;
    }
    let directory = state.join(MAILBOX_DIR);
    if !files::is_private_directory(&directory) {
        return None;
    }
    let record = live_record(&directory, session_id)?;
    // A shared Hermes gateway names the pane that started it, not this session's: such a
    // record proves the badge's principal but never a pane's bars.
    if record.get("pane_scoped").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    parse_record(&record, pane_id, session_id, provider_id, now_unix)
}

/// The principal stamp of every Hermes session a live producer is watching
/// on the bridged route right now, one entry per session.
///
/// This is what the footer's Hermes badge counts. It reads only this
/// plugin's own mailbox, never a pane: a session counts while its producer
/// holds the lock and re-checked its route within [`ROUTE_TTL_SECONDS`],
/// whichever pane it reports from. A session whose token names no
/// principal, or that is on any other route, is not counted.
pub fn live_principals(state: &Path, now_unix: u64) -> Vec<String> {
    live_records(state)
        .into_iter()
        .filter_map(|(session_id, record)| {
            let pane_id = record_pane_id(&record)?;
            parse_route(&record, pane_id, &session_id, BRIDGED_PROVIDER, now_unix)?.principal
        })
        .collect()
}

/// The Claude credentials file stamp of every Hermes session a live
/// producer is watching on the native Anthropic route right now, one entry
/// per session.
///
/// The Claude row's Hermes badge counts these. Same rules as
/// [`live_principals`]: only this plugin's mailbox is read, the producer
/// must hold the lock and have re-checked the route within
/// [`ROUTE_TTL_SECONDS`]. A session whose key was not seen in the file, or
/// that is on any other route, is not counted.
pub fn live_claude_credentials(state: &Path, now_unix: u64) -> Vec<String> {
    live_records(state)
        .into_iter()
        .filter_map(|(session_id, record)| {
            let pane_id = record_pane_id(&record)?;
            parse_claude_credentials(&record, pane_id, &session_id, now_unix)
        })
        .collect()
}

/// Every record whose producer still holds its lock, by session id, in
/// session id order. At most [`MAX_SCANNED_ENTRIES`] directory entries are
/// looked at.
fn live_records(state: &Path) -> Vec<(String, Value)> {
    let directory = state.join(MAILBOX_DIR);
    if !files::is_private_directory(&directory) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Vec::new();
    };
    let mut sessions: Vec<String> = entries
        .take(MAX_SCANNED_ENTRIES)
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            let session_id = name.strip_suffix(".json")?;
            is_session_id(session_id).then(|| session_id.to_owned())
        })
        .collect();
    sessions.sort();
    sessions
        .into_iter()
        .filter_map(|session_id| {
            let record = live_record(&directory, &session_id)?;
            Some((session_id, record))
        })
        .collect()
}

/// The pane a record reports from, when it has the shape the producer
/// accepts.
fn record_pane_id(record: &Value) -> Option<&str> {
    record
        .get("pane_id")
        .and_then(Value::as_str)
        .filter(|pane| is_pane_id(pane))
}

/// The record of `session_id`, only while its producer still holds the lock.
fn live_record(directory: &Path, session_id: &str) -> Option<Value> {
    if !files::producer_is_alive(&directory.join(format!("{session_id}.lock"))) {
        return None;
    }
    let bytes = files::read_private(&directory.join(format!("{session_id}.json")))?;
    serde_json::from_slice(&bytes).ok()
}

/// Validate one mailbox record against the pane that wants to show it.
pub fn parse_record(
    record: &Value,
    pane_id: &str,
    session_id: &str,
    provider_id: &str,
    now_unix: u64,
) -> Option<Reading> {
    let Route {
        epoch,
        identity,
        principal,
    } = parse_route(record, pane_id, session_id, provider_id, now_unix)?;
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let not_future = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_u64)
            .filter(|time| *time <= now_unix.saturating_add(FUTURE_SLACK_SECONDS))
    };
    // The quota must have been fetched under the route the producer is on now.
    let quota = record.get("quota").filter(|quota| quota.is_object())?;
    if quota.get("epoch").and_then(Value::as_u64) != Some(epoch)
        || text(quota, "identity").as_deref() != Some(identity.as_str())
    {
        return None;
    }
    let fetched_at = not_future(quota, "fetched_at")?;
    let entries = quota.get("windows").and_then(Value::as_array)?;
    if entries.is_empty() || entries.len() > 2 {
        return None;
    }
    let mut windows: Vec<UsageWindow> = Vec::new();
    for entry in entries {
        let kind = match entry.get("kind").and_then(Value::as_str)? {
            "5h" => WindowKind::FiveHour,
            "7d" => WindowKind::Weekly,
            _ => return None,
        };
        if windows.iter().any(|window| window.kind == kind) {
            return None;
        }
        let used = entry.get("used_percent").and_then(Value::as_f64)?;
        let resets_at = match entry.get("resets_at") {
            None | Some(Value::Null) => None,
            Some(value) => Some(ResetAt::from_unix_seconds(value.as_u64()?)),
        };
        // Out-of-range or non-finite is a malformed record, not a full bar.
        windows.push(UsageWindow::new(kind, used, resets_at).ok()?);
    }
    Some(Reading {
        snapshot: ProviderSnapshot::new(Provider::Hermes, windows, fetched_at),
        principal,
    })
}

/// The route-level gates: who the record is for, and the route its producer
/// checked recently. The quota is not looked at.
fn parse_route(
    record: &Value,
    pane_id: &str,
    session_id: &str,
    provider_id: &str,
    now_unix: u64,
) -> Option<Route> {
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let route = checked_route(record, pane_id, session_id, now_unix)?;
    let epoch = route.get("epoch").and_then(Value::as_u64)?;
    let identity = text(route, "identity").filter(|identity| is_identity(identity))?;
    if text(route, "provider").as_deref() != Some(provider_id)
        || route.get("supported").and_then(Value::as_bool) != Some(true)
    {
        return None;
    }
    // Absent from records written before the stamp existed, and null when the
    // token names no principal. Anything else that is not a stamp is malformed.
    let principal = optional_stamp(route, "principal")?;
    Some(Route {
        epoch,
        identity,
        principal,
    })
}

/// The Claude credentials file stamp of a native Anthropic route its
/// producer checked recently. That route is never supported and carries no
/// identity or quota; only the stamp is read.
fn parse_claude_credentials(
    record: &Value,
    pane_id: &str,
    session_id: &str,
    now_unix: u64,
) -> Option<String> {
    let route = checked_route(record, pane_id, session_id, now_unix)?;
    if route.get("provider").and_then(Value::as_str) != Some(CLAUDE_ROUTE_PROVIDER) {
        return None;
    }
    optional_stamp(route, "claude_credentials")?
}

/// The record's route, once the record is for this session and pane in the
/// default profile and its producer checked that route recently.
fn checked_route<'a>(
    record: &'a Value,
    pane_id: &str,
    session_id: &str,
    now_unix: u64,
) -> Option<&'a Value> {
    let text = |key: &str| record.get(key).and_then(Value::as_str);
    if record.get("schema").and_then(Value::as_u64) != Some(SCHEMA)
        || text("session_id") != Some(session_id)
        || text("pane_id") != Some(pane_id)
        || text("profile") != Some("default")
    {
        return None;
    }
    let route = record.get("route")?;
    // A producer whose poller died or hangs still holds its lock, but no longer
    // sees an idle `/model` or account switch. Its last check goes stale; how
    // long ago the quota itself was fetched does not matter here.
    let observed_at = route
        .get("observed_at")
        .and_then(Value::as_u64)
        .filter(|time| *time <= now_unix.saturating_add(FUTURE_SLACK_SECONDS))?;
    if now_unix.saturating_sub(observed_at) > ROUTE_TTL_SECONDS {
        return None;
    }
    Some(route)
}

/// `Some(None)` for an absent or null stamp, `Some(Some(_))` for a stamp,
/// `None` for anything else (a malformed record).
fn optional_stamp(route: &Value, key: &str) -> Option<Option<String>> {
    match route.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(value) => Some(Some(
            value
                .as_str()
                .filter(|stamp| is_identity(stamp))?
                .to_owned(),
        )),
    }
}

/// A Hermes session id that is safe to use as a file name.
fn is_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The pane id shape the producer accepts from `HERDR_PANE_ID`.
fn is_pane_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '-'))
}

fn is_identity(value: &str) -> bool {
    value.len() == 64
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

#[cfg(unix)]
mod files {
    use super::MAX_RECORD_BYTES;
    use std::fs::{File, OpenOptions, TryLockError};
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::Path;

    fn own_uid() -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    /// A real directory of ours that nobody else can read or write.
    pub fn is_private_directory(path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| {
            metadata.file_type().is_dir()
                && metadata.uid() == own_uid()
                && metadata.mode() & 0o077 == 0
        })
    }

    fn open(path: &Path) -> Option<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .ok()
    }

    /// The producer holds an exclusive lock while it tracks the session. A
    /// lock this reader can share, or no lock file, means nobody is watching
    /// that session's credential any more.
    pub fn producer_is_alive(lock: &Path) -> bool {
        let Some(file) = open(lock) else {
            return false;
        };
        matches!(file.try_lock_shared(), Err(TryLockError::WouldBlock))
    }

    /// A small regular file of ours that nobody else can read or write.
    pub fn read_private(path: &Path) -> Option<Vec<u8>> {
        let file = open(path)?;
        let metadata = file.metadata().ok()?;
        if !metadata.file_type().is_file()
            || metadata.uid() != own_uid()
            || metadata.mode() & 0o077 != 0
            || metadata.len() > MAX_RECORD_BYTES
        {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(MAX_RECORD_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        (bytes.len() as u64 <= MAX_RECORD_BYTES).then_some(bytes)
    }
}

#[cfg(not(unix))]
mod files {
    use std::path::Path;

    // Without flock and ownership checks there is no proof of a live producer.
    pub fn is_private_directory(_path: &Path) -> bool {
        false
    }
    pub fn producer_is_alive(_lock: &Path) -> bool {
        false
    }
    pub fn read_private(_path: &Path) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(all(test, unix))]
pub(crate) mod testing {
    use super::MAILBOX_DIR;
    use serde_json::{json, Value};
    use std::fs::{self, File, OpenOptions};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::path::Path;

    pub const IDENTITY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// A well-formed record as read at 2000: a weekly window at `used`
    /// percent, epoch 3, fetched long ago under a route checked just now.
    pub fn record(pane_id: &str, session_id: &str, used: f64) -> Value {
        json!({
            "schema": 1, "session_id": session_id, "pane_id": pane_id,
            "profile": "default", "pid": 1,
            "route": {"epoch": 3, "provider": "openai-codex", "supported": true,
                      "identity": IDENTITY, "observed_at": 1_990},
            "quota": {"epoch": 3, "identity": IDENTITY, "fetched_at": 1_000,
                      "windows": [{"kind": "7d", "used_percent": used, "resets_at": 9_000}]}
        })
    }

    /// Write a record the way the producer does, and hold its lock. Dropping
    /// the returned file is the producer going away.
    pub fn write(state: &Path, session_id: &str, record: &Value) -> File {
        let directory = state.join(MAILBOX_DIR);
        fs::create_dir_all(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join(format!("{session_id}.json"));
        let _ = fs::remove_file(&path);
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true).mode(0o600);
        let file = options.open(&path).unwrap();
        serde_json::to_writer(file, record).unwrap();
        let lock = options
            .open(directory.join(format!("{session_id}.lock")))
            .unwrap();
        lock.try_lock().expect("the test producer takes its lock");
        lock
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::testing::{record, write, IDENTITY};
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const NOW: u64 = 2_000;

    fn parsed(record: &Value) -> Option<ProviderSnapshot> {
        parse_record(record, "w1:p1", "s1", BRIDGED_PROVIDER, NOW).map(|reading| reading.snapshot)
    }

    fn loaded(state: &Path) -> Option<ProviderSnapshot> {
        load(state, "w1:p1", "s1", BRIDGED_PROVIDER, NOW).map(|reading| reading.snapshot)
    }

    fn stamp(account: &str, subject: &str) -> String {
        chatgpt_principal_stamp(account, subject)
    }

    /// The bridge plugin hashes the same bytes
    /// (`test_the_principal_stamp_is_pinned` in
    /// `tests/hermes_plugin/test_bridge.py` asserts this exact digest). If
    /// either side changes, every badge silently stops matching.
    #[test]
    fn the_principal_stamp_is_pinned() {
        assert_eq!(
            stamp("acc-1", "user-1"),
            "fbb8c62b1dfc955d0617ae4d6780b6b62fb040c7b8a3ec08c0f06d7f9f6c84e4"
        );
        // Another seat of the same workspace is another principal.
        assert_ne!(stamp("acc-1", "user-1"), stamp("acc-1", "user-2"));
        assert_ne!(stamp("acc-1", "user-1"), stamp("acc-2", "user-1"));
        // The separator keeps the two halves from sliding into each other.
        assert_ne!(stamp("acc-1", "user-1"), stamp("acc-1u", "ser-1"));
        assert!(!stamp("acc-1", "user-1").contains("acc-1"));
    }

    /// A route names its principal only as a stamp. A record written before
    /// the stamp existed, or for a token that names no principal, still
    /// reads; a value that is not a stamp is a malformed record.
    #[test]
    fn a_route_names_its_principal_only_as_a_stamp() {
        let read = |principal: Option<Value>| {
            let mut value = record("w1:p1", "s1", 12.0);
            match principal {
                Some(principal) => value["route"]["principal"] = principal,
                None => {
                    value["route"].as_object_mut().unwrap().remove("principal");
                }
            }
            parse_record(&value, "w1:p1", "s1", BRIDGED_PROVIDER, NOW)
        };
        let mine = stamp("acc-1", "user-1");
        assert_eq!(
            read(Some(json!(mine))).expect("reading").principal,
            Some(mine)
        );
        assert_eq!(read(Some(Value::Null)).expect("reading").principal, None);
        assert_eq!(read(None).expect("reading").principal, None);
        for malformed in [
            json!("acc-1"),
            json!("short"),
            json!(IDENTITY.to_uppercase()),
            json!(5),
            json!({"id": "acc-1"}),
        ] {
            assert!(read(Some(malformed.clone())).is_none(), "{malformed}");
        }
    }

    /// The badge counts sessions, not panes: every live producer on the
    /// bridged route whose token names a principal, whatever its quota.
    #[test]
    fn live_principals_are_the_stamps_of_live_bridged_sessions() {
        let state = tempfile::tempdir().unwrap();
        let a = stamp("acc-1", "user-1");
        let b = stamp("acc-1", "user-2");
        let with = |pane: &str, session: &str, principal: Option<&str>| {
            let mut value = record(pane, session, 12.0);
            value["route"]["principal"] = principal.map_or(Value::Null, |stamp| json!(stamp));
            value
        };
        let mut producers = Vec::new();
        producers.push(write(state.path(), "s1", &with("w1:p1", "s1", Some(&a))));
        // Another pane on the same principal, with no quota fetched yet.
        let mut fresh = with("w2:p4", "s2", Some(&a));
        fresh["quota"] = Value::Null;
        producers.push(write(state.path(), "s2", &fresh));
        // Another seat of the same workspace.
        producers.push(write(state.path(), "s3", &with("w1:p2", "s3", Some(&b))));
        // A token that names no principal is a session, but not a principal.
        producers.push(write(state.path(), "s4", &with("w1:p3", "s4", None)));
        // Another route never counts, whatever it carries.
        let mut other = with("w1:p5", "s5", Some(&a));
        other["route"]["provider"] = json!("anthropic");
        producers.push(write(state.path(), "s5", &other));
        // A route nobody re-checked lately.
        let mut stale = with("w1:p6", "s6", Some(&a));
        stale["route"]["observed_at"] = json!(NOW - ROUTE_TTL_SECONDS - 1);
        producers.push(write(state.path(), "s6", &stale));
        // A record whose session id is not its file name.
        let mut renamed = with("w1:p7", "s1", Some(&a));
        renamed["session_id"] = json!("s1");
        producers.push(write(state.path(), "s7", &renamed));
        // A producer that went away.
        drop(write(state.path(), "s8", &with("w1:p8", "s8", Some(&a))));

        assert_eq!(live_principals(state.path(), NOW), vec![a.clone(), a, b]);
        drop(producers);
        assert!(live_principals(state.path(), NOW).is_empty());
    }

    #[test]
    fn live_principals_need_a_private_mailbox() {
        let state = tempfile::tempdir().unwrap();
        assert!(live_principals(state.path(), NOW).is_empty());
        let mut value = record("w1:p1", "s1", 12.0);
        value["route"]["principal"] = json!(stamp("acc-1", "user-1"));
        let _producer = write(state.path(), "s1", &value);
        assert_eq!(live_principals(state.path(), NOW).len(), 1);
        let directory = state.path().join(MAILBOX_DIR);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(live_principals(state.path(), NOW).is_empty());
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(directory.join("s1.json"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(live_principals(state.path(), NOW).is_empty());
    }

    /// The bridge plugin hashes the same bytes
    /// (`test_the_claude_credentials_stamp_is_pinned` in
    /// `tests/hermes_plugin/test_bridge.py` asserts this exact digest).
    #[test]
    fn the_claude_credentials_stamp_is_pinned() {
        let stamp = |path: &str| claude_credentials_stamp(Path::new(path)).unwrap();
        assert_eq!(
            stamp("/home/u/.claude/.credentials.json"),
            "c04c99582d1c677beac50010fc7e9d060fe8994bf50b45c80a9bfd8a45b7fab4"
        );
        assert_ne!(
            stamp("/home/u/.claude/.credentials.json"),
            stamp("/home/v/.claude/.credentials.json")
        );
        // Not a principal stamp of anything: another domain.
        assert!(is_identity(&stamp("/home/u/.claude/.credentials.json")));
        assert!(!stamp("/home/u/.claude/.credentials.json").contains("claude"));
    }

    /// An Anthropic record with the given `claude_credentials` value, as the
    /// bridge writes it: never supported, no identity, no quota.
    fn claude_record(pane: &str, session: &str, stamp: Option<Value>) -> Value {
        let mut value = record(pane, session, 12.0);
        value["route"] = json!({
            "epoch": 2, "provider": "anthropic", "supported": false, "identity": null,
            "principal": null, "observed_at": NOW - 10,
        });
        if let Some(stamp) = stamp {
            value["route"]["claude_credentials"] = stamp;
        }
        value["quota"] = Value::Null;
        value
    }

    /// The Claude badge counts live Anthropic sessions whose producer stamped
    /// the credentials file, and nothing else: not another route, not a stale
    /// check, not a malformed or missing stamp, not a dead producer.
    #[test]
    fn live_claude_credentials_are_the_stamps_of_live_anthropic_sessions() {
        let state = tempfile::tempdir().unwrap();
        let file =
            claude_credentials_stamp(Path::new("/home/u/.claude/.credentials.json")).unwrap();
        let other = claude_credentials_stamp(Path::new("/srv/claude/.credentials.json")).unwrap();
        let mut producers = Vec::new();
        let mut put =
            |session: &str, value: Value| producers.push(write(state.path(), session, &value));
        put("s1", claude_record("w1:p1", "s1", Some(json!(file))));
        put("s2", claude_record("w2:p4", "s2", Some(json!(file))));
        put("s3", claude_record("w1:p2", "s3", Some(json!(other))));
        // A key that was not seen in the file.
        put("s4", claude_record("w1:p3", "s4", Some(Value::Null)));
        put("s5", claude_record("w1:p5", "s5", None));
        // Another route never counts, whatever it carries.
        let mut codex = record("w1:p6", "s6", 12.0);
        codex["route"]["claude_credentials"] = json!(file);
        put("s6", codex);
        let mut stale = claude_record("w1:p7", "s7", Some(json!(file)));
        stale["route"]["observed_at"] = json!(NOW - ROUTE_TTL_SECONDS - 1);
        put("s7", stale);
        let mut future = claude_record("w1:p8", "s8", Some(json!(file)));
        future["route"]["observed_at"] = json!(NOW + FUTURE_SLACK_SECONDS + 1);
        put("s8", future);
        for (session, malformed) in [
            ("s9", json!("/home/u/.claude/.credentials.json")),
            ("sa", json!(file.to_uppercase())),
            ("sb", json!(7)),
        ] {
            put(session, claude_record("w1:p9", session, Some(malformed)));
        }
        let mut profile = claude_record("w1:pc", "sc", Some(json!(file)));
        profile["profile"] = json!("work");
        put("sc", profile);
        let mut renamed = claude_record("w1:pd", "s1", Some(json!(file)));
        renamed["session_id"] = json!("s1");
        put("sd", renamed);
        drop(write(
            state.path(),
            "se",
            &claude_record("w1:pe", "se", Some(json!(file))),
        ));

        assert_eq!(
            live_claude_credentials(state.path(), NOW),
            vec![file.clone(), file, other]
        );
        // The Codex principals ignore Anthropic records.
        assert!(live_principals(state.path(), NOW).is_empty());
        drop(producers);
        assert!(live_claude_credentials(state.path(), NOW).is_empty());
    }

    /// Records written by a bridge that predates `claude_credentials`, and
    /// the Codex route's own reading, read exactly as before.
    #[test]
    fn a_claude_stamp_changes_nothing_on_the_codex_route() {
        let mut value = record("w1:p1", "s1", 12.0);
        value["route"]["claude_credentials"] =
            json!(claude_credentials_stamp(Path::new("/x/.credentials.json")).unwrap());
        assert_eq!(parsed(&value), parsed(&record("w1:p1", "s1", 12.0)));
        assert!(parsed(&value).is_some());
    }

    #[test]
    fn a_live_producers_current_epoch_quota_is_read() {
        let state = tempfile::tempdir().unwrap();
        let mut value = record("w1:p1", "s1", 12.5);
        value["quota"]["windows"] = json!([
            {"kind": "5h", "used_percent": 1.0, "resets_at": null},
            {"kind": "7d", "used_percent": 12.5, "resets_at": 9_000}
        ]);
        let _producer = write(state.path(), "s1", &value);
        let snapshot = loaded(state.path()).expect("reading");
        assert_eq!(snapshot.provider, Provider::Hermes);
        assert_eq!(snapshot.fetched_at_unix, 1_000);
        assert_eq!(
            snapshot.window(WindowKind::Weekly).unwrap().used_percent,
            12.5
        );
        assert_eq!(
            snapshot
                .window(WindowKind::Weekly)
                .unwrap()
                .resets_at
                .map(ResetAt::unix_seconds),
            Some(9_000)
        );
        assert_eq!(
            snapshot.window(WindowKind::FiveHour).unwrap().resets_at,
            None
        );
        // No account id, no session maps: it is this session's reading only.
        assert_eq!(snapshot.account_id, None);
    }

    /// A shared Hermes gateway writes the pane that started it into every
    /// session's record: still a principal for the badge, never a pane's bars.
    #[test]
    fn a_shared_gateways_record_counts_for_the_badge_but_is_no_panes_reading() {
        let state = tempfile::tempdir().unwrap();
        let principal = stamp("acc-1", "user-1");
        let mut value = record("w1:p1", "s1", 12.0);
        value["route"]["principal"] = json!(principal);
        value["pane_scoped"] = json!(false);
        let producer = write(state.path(), "s1", &value);
        assert!(loaded(state.path()).is_none());
        assert_eq!(live_principals(state.path(), NOW), vec![principal]);
        drop(producer);
        // An older bridge writes no such field: its pane is its own.
        value.as_object_mut().unwrap().remove("pane_scoped");
        let _producer = write(state.path(), "s1", &value);
        assert!(loaded(state.path()).is_some());
    }

    /// The same record is a reading for exactly one pane, session, and
    /// provider — never for a neighbour, a resumed copy, or after `/model`.
    #[test]
    fn a_record_is_only_a_reading_for_the_pane_session_and_provider_it_names() {
        let state = tempfile::tempdir().unwrap();
        let _producer = write(state.path(), "s1", &record("w1:p1", "s1", 12.0));
        assert!(loaded(state.path()).is_some());
        for (pane, session, provider) in [
            ("w1:p2", "s1", "openai-codex"),
            ("w1:p1", "s2", "openai-codex"),
            ("w1:p1", "s1", "anthropic"),
            ("w1:p1", "s1", "xai-oauth"),
            ("w1:p1", "../s1", "openai-codex"),
            ("w1:p1", "", "openai-codex"),
        ] {
            assert!(
                load(state.path(), pane, session, provider, NOW).is_none(),
                "{pane} {session} {provider}"
            );
        }
        // A session file renamed onto another id still names its own session.
        let directory = state.path().join(MAILBOX_DIR);
        fs::copy(directory.join("s1.json"), directory.join("s2.json")).unwrap();
        fs::set_permissions(directory.join("s2.json"), fs::Permissions::from_mode(0o600)).unwrap();
        let other = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("s2.lock"))
            .unwrap();
        other.try_lock().unwrap();
        assert!(load(state.path(), "w1:p1", "s2", BRIDGED_PROVIDER, NOW).is_none());
    }

    /// The producer drops the quota on every credential change. A record that
    /// still pairs a quota with another epoch or identity is never trusted.
    #[test]
    fn a_quota_from_another_epoch_or_identity_is_not_a_reading() {
        let good = record("w1:p1", "s1", 12.0);
        assert!(parsed(&good).is_some());
        let mutate = |edit: &dyn Fn(&mut Value)| {
            let mut value = good.clone();
            edit(&mut value);
            parsed(&value)
        };
        assert!(mutate(&|v| v["quota"] = Value::Null).is_none());
        assert!(mutate(&|v| v["quota"]["epoch"] = json!(2)).is_none());
        assert!(mutate(&|v| v["route"]["epoch"] = json!(4)).is_none());
        assert!(mutate(&|v| v["quota"]["identity"] = json!("f".repeat(64))).is_none());
        assert!(mutate(&|v| v["route"]["identity"] = Value::Null).is_none());
        assert!(mutate(&|v| v["route"]["identity"] = json!("short")).is_none());
        assert!(mutate(&|v| v["route"]["supported"] = json!(false)).is_none());
        assert!(mutate(&|v| v["route"]["provider"] = json!("anthropic")).is_none());
        assert!(mutate(&|v| v["route"]["provider"] = Value::Null).is_none());
        assert!(mutate(&|v| v["profile"] = json!("work")).is_none());
        assert!(mutate(&|v| v["schema"] = json!(2)).is_none());
        assert!(mutate(&|v| {
            v.as_object_mut().unwrap().remove("route");
        })
        .is_none());
        assert_eq!(IDENTITY.len(), 64);
    }

    #[test]
    fn a_malformed_window_or_a_future_timestamp_rejects_the_whole_record() {
        let good = record("w1:p1", "s1", 12.0);
        let with_windows = |windows: Value| {
            let mut value = good.clone();
            value["quota"]["windows"] = windows;
            parsed(&value)
        };
        for windows in [
            json!([]),
            json!([{"kind": "30d", "used_percent": 1.0, "resets_at": null}]),
            json!([{"kind": "7d", "used_percent": 100.5, "resets_at": null}]),
            json!([{"kind": "7d", "used_percent": -1.0, "resets_at": null}]),
            json!([{"kind": "7d", "used_percent": "12", "resets_at": null}]),
            json!([{"kind": "7d", "used_percent": 1.0, "resets_at": -5}]),
            json!([{"kind": "7d", "used_percent": 1.0, "resets_at": "soon"}]),
            json!([{"kind": "7d", "used_percent": 1.0}, {"kind": "7d", "used_percent": 2.0}]),
            json!([{"kind": "5h", "used_percent": 1.0}, {"kind": "7d", "used_percent": 2.0},
                   {"kind": "7d", "used_percent": 3.0}]),
            json!("many"),
        ] {
            assert!(with_windows(windows.clone()).is_none(), "{windows}");
        }
        let mut future = good.clone();
        future["quota"]["fetched_at"] = json!(NOW + FUTURE_SLACK_SECONDS + 1);
        assert!(parsed(&future).is_none());
        future["quota"]["fetched_at"] = json!(NOW + FUTURE_SLACK_SECONDS);
        assert!(parsed(&future).is_some());
        let mut observed = good.clone();
        observed["route"]["observed_at"] = json!(NOW + 10_000);
        assert!(parsed(&observed).is_none());
    }

    /// The lock proves a live process, not a live watcher. A route nobody has
    /// re-checked lately is no reading, however recent the fetch; an old
    /// fetch under a route that was just re-checked still is one.
    #[test]
    fn a_route_nobody_checked_recently_is_not_a_reading() {
        let mut value = record("w1:p1", "s1", 12.0);
        value["route"]["observed_at"] = json!(NOW - ROUTE_TTL_SECONDS);
        value["quota"]["fetched_at"] = json!(NOW);
        assert!(parsed(&value).is_some());
        value["route"]["observed_at"] = json!(NOW - ROUTE_TTL_SECONDS - 1);
        assert!(parsed(&value).is_none());
        value["route"]["observed_at"] = json!(NOW);
        value["quota"]["fetched_at"] = json!(1);
        assert_eq!(parsed(&value).expect("reading").fetched_at_unix, 1);
    }

    /// The lock is the producer's heartbeat: a killed Hermes cannot rewrite
    /// its record, but it cannot keep its lock either.
    #[test]
    fn a_record_without_a_live_producer_is_not_a_reading() {
        let state = tempfile::tempdir().unwrap();
        let producer = write(state.path(), "s1", &record("w1:p1", "s1", 12.0));
        assert!(loaded(state.path()).is_some());
        drop(producer);
        assert!(loaded(state.path()).is_none());
        // Reading must not leave a lock behind that blocks the next producer.
        let directory = state.path().join(MAILBOX_DIR);
        let next = fs::OpenOptions::new()
            .write(true)
            .open(directory.join("s1.lock"))
            .unwrap();
        next.try_lock().expect("a new producer can take the lock");
        assert!(loaded(state.path()).is_some());
        drop(next);
        fs::remove_file(directory.join("s1.lock")).unwrap();
        assert!(loaded(state.path()).is_none());
    }

    #[test]
    fn a_mailbox_others_can_reach_or_a_symlink_is_never_read() {
        let state = tempfile::tempdir().unwrap();
        let _producer = write(state.path(), "s1", &record("w1:p1", "s1", 12.0));
        let directory = state.path().join(MAILBOX_DIR);
        let path = directory.join("s1.json");
        assert!(loaded(state.path()).is_some());

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(loaded(state.path()).is_none(), "a world-readable record");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(loaded(state.path()).is_none(), "a shared directory");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();

        // A record that is a symlink, even to a valid private file.
        let real = directory.join("real.json");
        fs::rename(&path, &real).unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();
        assert!(loaded(state.path()).is_none(), "a symlinked record");
        fs::remove_file(&path).unwrap();
        fs::rename(&real, &path).unwrap();
        assert!(loaded(state.path()).is_some());

        // An oversized record is not parsed at all.
        let mut padded = record("w1:p1", "s1", 12.0);
        padded["padding"] = json!("x".repeat(MAX_RECORD_BYTES as usize));
        fs::write(&path, serde_json::to_vec(&padded).unwrap()).unwrap();
        assert!(loaded(state.path()).is_none(), "an oversized record");
        fs::write(&path, b"{ not json").unwrap();
        assert!(loaded(state.path()).is_none(), "a torn record");

        // The mailbox directory itself being a symlink.
        let elsewhere = state.path().join("elsewhere");
        fs::rename(&directory, &elsewhere).unwrap();
        fs::write(
            elsewhere.join("s1.json"),
            serde_json::to_vec(&record("w1:p1", "s1", 12.0)).unwrap(),
        )
        .unwrap();
        std::os::unix::fs::symlink(&elsewhere, &directory).unwrap();
        assert!(loaded(state.path()).is_none(), "a symlinked mailbox");
        assert!(load(state.path(), "w1:p1", "s1", BRIDGED_PROVIDER, NOW).is_none());
    }

    #[test]
    fn a_missing_mailbox_is_simply_no_reading() {
        let state = tempfile::tempdir().unwrap();
        assert!(loaded(state.path()).is_none());
        fs::create_dir(state.path().join(MAILBOX_DIR)).unwrap();
        assert!(loaded(state.path()).is_none());
    }
}
