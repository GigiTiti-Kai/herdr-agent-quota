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

use crate::model::{Provider, ProviderSnapshot, ResetAt, UsageWindow, WindowKind};
use serde_json::Value;
use std::path::Path;

pub const MAILBOX_DIR: &str = "hermes-bridge";
/// The only route the bridge fetches for.
pub const BRIDGED_PROVIDER: &str = "openai-codex";

const SCHEMA: u64 = 1;
const MAX_RECORD_BYTES: u64 = 16 * 1024;
/// Clock disagreement tolerated between the producer and this reader.
const FUTURE_SLACK_SECONDS: u64 = 120;
/// How long a route check stays good. The producer re-stamps `observed_at`
/// every 30 seconds while its poller or a hook still looks at the live agent.
const ROUTE_TTL_SECONDS: u64 = 120;

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
) -> Option<ProviderSnapshot> {
    if provider_id != BRIDGED_PROVIDER || !is_session_id(session_id) {
        return None;
    }
    let directory = state.join(MAILBOX_DIR);
    if !files::is_private_directory(&directory) {
        return None;
    }
    if !files::producer_is_alive(&directory.join(format!("{session_id}.lock"))) {
        return None;
    }
    let bytes = files::read_private(&directory.join(format!("{session_id}.json")))?;
    let record: Value = serde_json::from_slice(&bytes).ok()?;
    parse_record(&record, pane_id, session_id, provider_id, now_unix)
}

/// Validate one mailbox record against the pane that wants to show it.
pub fn parse_record(
    record: &Value,
    pane_id: &str,
    session_id: &str,
    provider_id: &str,
    now_unix: u64,
) -> Option<ProviderSnapshot> {
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let not_future = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_u64)
            .filter(|time| *time <= now_unix.saturating_add(FUTURE_SLACK_SECONDS))
    };
    if record.get("schema").and_then(Value::as_u64) != Some(SCHEMA)
        || text(record, "session_id").as_deref() != Some(session_id)
        || text(record, "pane_id").as_deref() != Some(pane_id)
        || text(record, "profile").as_deref() != Some("default")
    {
        return None;
    }
    let route = record.get("route")?;
    let epoch = route.get("epoch").and_then(Value::as_u64)?;
    let identity = text(route, "identity").filter(|identity| is_identity(identity))?;
    if text(route, "provider").as_deref() != Some(provider_id)
        || route.get("supported").and_then(Value::as_bool) != Some(true)
    {
        return None;
    }
    // A producer whose poller died or hangs still holds its lock, but no longer
    // sees an idle `/model` or account switch. Its last check goes stale; how
    // long ago the quota itself was fetched does not matter here.
    let observed_at = not_future(route, "observed_at")?;
    if now_unix.saturating_sub(observed_at) > ROUTE_TTL_SECONDS {
        return None;
    }
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
    Some(ProviderSnapshot::new(Provider::Hermes, windows, fetched_at))
}

/// A Hermes session id that is safe to use as a file name.
fn is_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
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
        parse_record(record, "w1:p1", "s1", BRIDGED_PROVIDER, NOW)
    }

    fn loaded(state: &Path) -> Option<ProviderSnapshot> {
        load(state, "w1:p1", "s1", BRIDGED_PROVIDER, NOW)
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
