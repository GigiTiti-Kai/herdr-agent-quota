#![cfg(unix)]

use herdr_agent_quota::cache::CacheStore;
use herdr_agent_quota::cli::SummaryFormat;
use herdr_agent_quota::model::{Provider, ProviderSnapshot, ResetAt, UsageWindow, WindowKind};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

const FAKE_HERDR: &str = r#"#!/bin/sh
printf '%s %s %s\n' "$1" "$2" "$3" >> "$TEST_CALLS"
case "$1 $2" in
  'agent list') printf '%s\n' '{"result":{"agents":[]}}' ;;
  'workspace list') cat "$TEST_WORKSPACES" ;;
  'workspace report-metadata') printf '%s\n' "$@" > "$TEST_REPORTS/$3" ;;
  *) exit 1 ;;
esac
"#;

/// Tokens a report leaves on a workspace that started with `start`.
fn apply_report(start: &BTreeMap<String, String>, report: &str) -> BTreeMap<String, String> {
    let mut tokens = start.clone();
    let args: Vec<_> = report.lines().collect();
    for pair in args.windows(2) {
        match pair[0] {
            "--token" => {
                let (name, value) = pair[1].split_once('=').unwrap();
                tokens.insert(name.into(), value.into());
            }
            "--clear-token" => {
                tokens.remove(pair[1]);
            }
            _ => {}
        }
    }
    tokens
}

fn refresh(root: &Path, workspaces: serde_json::Value) -> Vec<String> {
    fs::write(root.join("workspaces.json"), workspaces.to_string()).unwrap();
    fs::write(root.join("calls"), "").unwrap();
    let _ = fs::remove_dir_all(root.join("reports"));
    fs::create_dir(root.join("reports")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-agent-usage"))
        .args(["refresh", "--provider", "claude"])
        .env("HERDR_PLUGIN_STATE_DIR", root)
        .env("HERDR_PLUGIN_CONFIG_DIR", root)
        .env("HERDR_BIN_PATH", root.join("herdr"))
        .env("HERDR_CONFIG_FILE", root.join("config.toml"))
        .env("HERDR_SOCKET_PATH", root.join("herdr.sock"))
        .env("XDG_STATE_HOME", root.join("xdg-state"))
        .env("CLAUDE_CONFIG_DIR", root.join("claude"))
        // Absent on purpose: never reach the live Claude usage endpoint.
        .env(
            "CLAUDE_CREDENTIALS_FILE",
            root.join("absent-claude-auth.json"),
        )
        .env("HERDR_AGENT_QUOTA_AGENTS", "claude")
        .env("TEST_WORKSPACES", root.join("workspaces.json"))
        .env("TEST_CALLS", root.join("calls"))
        .env("TEST_REPORTS", root.join("reports"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::read_to_string(root.join("calls"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_pane_less_claude_footer_reaches_every_workspace_and_matching_ones_are_left_alone() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let cache = CacheStore::new(root);
    cache.set_account_summary(SummaryFormat::Compact).unwrap();
    let now = CacheStore::now_unix();
    let windows = vec![
        UsageWindow::new(
            WindowKind::FiveHour,
            20.0,
            Some(ResetAt::from_unix_seconds(now + 3_600)),
        )
        .unwrap(),
        UsageWindow::new(
            WindowKind::Weekly,
            35.0,
            Some(ResetAt::from_unix_seconds(now + 86_400)),
        )
        .unwrap(),
    ];
    cache
        .save(
            &ProviderSnapshot::new(Provider::Claude, windows.clone(), now)
                .session_local()
                .with_account_windows(windows),
        )
        .unwrap();
    fs::write(root.join("herdr"), FAKE_HERDR).unwrap();
    fs::set_permissions(root.join("herdr"), fs::Permissions::from_mode(0o755)).unwrap();

    // No agent pane at all; `tokens` is omitted, as Herdr does when empty.
    let calls = refresh(
        root,
        json!({"result":{"workspaces":[
            {"workspace_id":"w1","label":"api"},
            {"workspace_id":"w2","label":"web"}
        ]}}),
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| *call == "workspace list ")
            .count(),
        1
    );
    let first = fs::read_to_string(root.join("reports/w1")).unwrap();
    let published = apply_report(&BTreeMap::new(), &first);
    assert_eq!(
        published.get("quota_acct_title").map(String::as_str),
        Some("quota")
    );
    assert!(
        published.contains_key("quota_acct_cl_icon"),
        "{published:?}"
    );
    assert!(first.contains("herdr-agent-quota-summary"));
    assert!(root.join("reports/w2").exists());

    // w1 already carries the footer: only the new w2 is written.
    let mut w1_tokens = published.clone();
    w1_tokens.insert("foreign".into(), "kept".into());
    let calls = refresh(
        root,
        json!({"result":{"workspaces":[
            {"workspace_id":"w1","label":"api","tokens":w1_tokens},
            {"workspace_id":"w3","label":"new"}
        ]}}),
    );
    let reports: Vec<_> = calls
        .iter()
        .filter(|call| call.starts_with("workspace report-metadata"))
        .collect();
    assert_eq!(reports, ["workspace report-metadata w3"]);

    // `off` clears the footer from w1 and writes nothing else.
    cache.set_account_summary(SummaryFormat::Off).unwrap();
    refresh(
        root,
        json!({"result":{"workspaces":[
            {"workspace_id":"w1","label":"api","tokens":w1_tokens}
        ]}}),
    );
    let cleared = fs::read_to_string(root.join("reports/w1")).unwrap();
    assert!(!cleared.contains("--token"), "{cleared}");
    let left = apply_report(&w1_tokens, &cleared);
    assert_eq!(left.keys().collect::<Vec<_>>(), ["foreign"]);
}

/// A refresh of the Codex row with Hermes enabled: the real binary, a fake
/// Herdr, a Codex login fixture, and bridge records written the way the
/// Hermes plugin writes them.
fn refresh_codex(root: &Path) -> BTreeMap<String, String> {
    fs::write(
        root.join("workspaces.json"),
        json!({"result":{"workspaces":[{"workspace_id":"w1","label":"api"}]}}).to_string(),
    )
    .unwrap();
    fs::write(root.join("calls"), "").unwrap();
    let _ = fs::remove_dir_all(root.join("reports"));
    fs::create_dir(root.join("reports")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-agent-usage"))
        .args(["refresh", "--provider", "codex"])
        .env("HERDR_PLUGIN_STATE_DIR", root)
        .env("HERDR_PLUGIN_CONFIG_DIR", root)
        .env("HERDR_BIN_PATH", root.join("herdr"))
        .env("HERDR_CONFIG_FILE", root.join("config.toml"))
        .env("HERDR_SOCKET_PATH", root.join("herdr.sock"))
        .env("XDG_STATE_HOME", root.join("xdg-state"))
        .env("CODEX_AUTH_FILE", root.join("codex-auth.json"))
        .env("CODEX_HOME", root.join("codex-home"))
        // Absent on purpose: never start a real Codex app-server.
        .env("CODEX_BIN_PATH", root.join("absent-codex"))
        .env("HERMES_HOME", root.join("hermes-home"))
        .env("HERDR_AGENT_QUOTA_AGENTS", "codex,hermes")
        .env("TEST_WORKSPACES", root.join("workspaces.json"))
        .env("TEST_CALLS", root.join("calls"))
        .env("TEST_REPORTS", root.join("reports"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = fs::read_to_string(root.join("reports/w1")).unwrap_or_default();
    apply_report(&BTreeMap::new(), &report)
}

/// A bridge record for `session`, as the Hermes plugin writes it, held by a
/// live producer for as long as the returned lock is open.
fn hermes_session(root: &Path, session: &str, principal: &str, now: u64) -> fs::File {
    let identity = "0123456789abcdef".repeat(4);
    bridge_session(
        root,
        session,
        json!({"epoch": 1, "provider": "openai-codex", "supported": true,
               "identity": identity, "principal": principal, "observed_at": now}),
    )
}

/// A native Anthropic session's bridge record: never supported, no quota,
/// naming a Claude credentials file stamp (or none).
fn claude_session(root: &Path, session: &str, stamp: Option<&str>, now: u64) -> fs::File {
    bridge_session(
        root,
        session,
        json!({"epoch": 1, "provider": "anthropic", "supported": false, "identity": null,
               "principal": null, "claude_credentials": stamp, "observed_at": now}),
    )
}

fn bridge_session(root: &Path, session: &str, route: serde_json::Value) -> fs::File {
    use std::os::unix::fs::OpenOptionsExt;
    let mailbox = root.join("hermes-bridge");
    fs::create_dir_all(&mailbox).unwrap();
    fs::set_permissions(&mailbox, fs::Permissions::from_mode(0o700)).unwrap();
    let record = json!({
        "schema": 1, "session_id": session, "pane_id": "w9:p1", "profile": "default", "pid": 1,
        "route": route,
        "quota": null,
    });
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    let file = options
        .open(mailbox.join(format!("{session}.json")))
        .unwrap();
    serde_json::to_writer(file, &record).unwrap();
    let lock = options
        .open(mailbox.join(format!("{session}.lock")))
        .unwrap();
    lock.try_lock().unwrap();
    lock
}

/// A Codex `auth.json` for `account_id` whose made-up access token names
/// `account_id` and `subject` in its claims.
fn codex_login(root: &Path, account_id: &str, subject: &str) {
    use base64::Engine as _;
    let encode = |value: serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
    };
    let token = format!(
        "{}.{}.fake-signature",
        encode(json!({"alg": "none"})),
        encode(json!({"sub": subject,
                      "https://api.openai.com/auth": {"chatgpt_account_id": account_id}}))
    );
    fs::write(
        root.join("codex-auth.json"),
        json!({"auth_mode": "chatgpt",
               "tokens": {"account_id": account_id, "access_token": token}})
        .to_string(),
    )
    .unwrap();
}

#[test]
fn the_codex_row_carries_the_hermes_badge_only_for_live_sessions_on_its_login() {
    use herdr_agent_quota::providers::hermes::chatgpt_principal_stamp;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let cache = CacheStore::new(root);
    cache.set_account_summary(SummaryFormat::Lines).unwrap();
    let now = CacheStore::now_unix();
    let windows = vec![
        UsageWindow::new(
            WindowKind::FiveHour,
            20.0,
            Some(ResetAt::from_unix_seconds(now + 3_600)),
        )
        .unwrap(),
        UsageWindow::new(
            WindowKind::Weekly,
            2.0,
            Some(ResetAt::from_unix_seconds(now + 6 * 86_400)),
        )
        .unwrap(),
    ];
    cache
        .save(
            &ProviderSnapshot::new(Provider::Codex, windows, now)
                .with_account_id(Some("acc-test-1".to_string())),
        )
        .unwrap();
    // The Codex login the snapshot belongs to.
    codex_login(root, "acc-test-1", "user-test-1");
    fs::write(root.join("herdr"), FAKE_HERDR).unwrap();
    fs::set_permissions(root.join("herdr"), fs::Permissions::from_mode(0o755)).unwrap();
    // One session is the mark alone; more add the count. Both are as wide
    // as the provider mark the badge sits under.
    let badge = |count: usize| match count {
        1 => "\u{e1aa}  \u{2800}".to_string(),
        _ => format!("\u{e1aa} {count}\u{2800}"),
    };

    // No Hermes session: the Codex row, no badge.
    let published = refresh_codex(root);
    assert!(
        published.contains_key("quota_acct_cx_icon"),
        "{published:?}"
    );
    assert!(
        !published.contains_key("quota_acct_cx_hermes"),
        "{published:?}"
    );

    // Two live sessions on the Codex login; one on another seat of the same
    // workspace (same account id, own quota), one on another account.
    let mine = chatgpt_principal_stamp("acc-test-1", "user-test-1");
    let first = hermes_session(root, "s1", &mine, now);
    let second = hermes_session(root, "s2", &mine, now);
    let teammate = chatgpt_principal_stamp("acc-test-1", "user-test-2");
    let seat = hermes_session(root, "s3", &teammate, now);
    let other = hermes_session(
        root,
        "s5",
        &chatgpt_principal_stamp("acc-test-2", "user-test-1"),
        now,
    );
    let published = refresh_codex(root);
    assert_eq!(
        published.get("quota_acct_cx_hermes"),
        Some(&badge(2)),
        "{published:?}"
    );
    // Neither the raw ids nor the stamp ever reach Herdr.
    let report = fs::read_to_string(root.join("reports/w1")).unwrap();
    for secret in ["acc-test-1", "user-test-1", "fake-signature", mine.as_str()] {
        assert!(!report.contains(secret), "{report}");
    }

    // A session that ends (its producer releases the lock) stops counting.
    drop(second);
    assert_eq!(
        refresh_codex(root).get("quota_acct_cx_hermes"),
        Some(&badge(1))
    );
    drop(first);
    assert!(!refresh_codex(root).contains_key("quota_acct_cx_hermes"));

    // The Codex CLI logs in as the teammate: the snapshot's account still
    // matches, but the login is another seat, so the session no longer counts
    // and the teammate's does.
    let again = hermes_session(root, "s4", &mine, now);
    codex_login(root, "acc-test-1", "user-test-2");
    assert_eq!(
        refresh_codex(root).get("quota_acct_cx_hermes"),
        Some(&badge(1))
    );
    drop(seat);
    assert!(!refresh_codex(root).contains_key("quota_acct_cx_hermes"));

    // Another account: the snapshot no longer proves anything.
    codex_login(root, "acc-test-2", "user-test-1");
    assert!(!refresh_codex(root).contains_key("quota_acct_cx_hermes"));
    drop((again, other));
}

/// A refresh of the Claude row with Hermes enabled: the real binary, a fake
/// Herdr, and a Claude credentials file whose token has lapsed, so the usage
/// endpoint is never asked and the cached account windows stand.
fn refresh_claude(root: &Path, credentials: &Path) -> BTreeMap<String, String> {
    fs::write(
        root.join("workspaces.json"),
        json!({"result":{"workspaces":[{"workspace_id":"w1","label":"api"}]}}).to_string(),
    )
    .unwrap();
    fs::write(root.join("calls"), "").unwrap();
    let _ = fs::remove_dir_all(root.join("reports"));
    fs::create_dir(root.join("reports")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-agent-usage"))
        .args(["refresh", "--provider", "claude"])
        .env("HERDR_PLUGIN_STATE_DIR", root)
        .env("HERDR_PLUGIN_CONFIG_DIR", root)
        .env("HERDR_BIN_PATH", root.join("herdr"))
        .env("HERDR_CONFIG_FILE", root.join("config.toml"))
        .env("HERDR_SOCKET_PATH", root.join("herdr.sock"))
        .env("XDG_STATE_HOME", root.join("xdg-state"))
        .env("CLAUDE_CONFIG_DIR", root.join("claude-config"))
        .env("CLAUDE_CREDENTIALS_FILE", credentials)
        .env("HERMES_HOME", root.join("hermes-home"))
        .env("HERDR_AGENT_QUOTA_AGENTS", "claude,hermes")
        .env("TEST_WORKSPACES", root.join("workspaces.json"))
        .env("TEST_CALLS", root.join("calls"))
        .env("TEST_REPORTS", root.join("reports"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = fs::read_to_string(root.join("reports/w1")).unwrap_or_default();
    apply_report(&BTreeMap::new(), &report)
}

#[test]
fn the_claude_row_carries_the_hermes_badge_only_for_live_sessions_on_its_credentials_file() {
    use herdr_agent_quota::providers::hermes::{chatgpt_principal_stamp, claude_credentials_stamp};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let cache = CacheStore::new(root);
    cache.set_account_summary(SummaryFormat::Lines).unwrap();
    let now = CacheStore::now_unix();
    let windows = vec![
        UsageWindow::new(
            WindowKind::FiveHour,
            20.0,
            Some(ResetAt::from_unix_seconds(now + 3_600)),
        )
        .unwrap(),
        UsageWindow::new(
            WindowKind::Weekly,
            35.0,
            Some(ResetAt::from_unix_seconds(now + 6 * 86_400)),
        )
        .unwrap(),
    ];
    // The Claude row as the usage endpoint left it.
    cache
        .save(
            &ProviderSnapshot::new(Provider::Claude, windows.clone(), now)
                .session_local()
                .with_account_windows(windows),
        )
        .unwrap();
    // Claude Code's credentials file, reached through a symlinked directory.
    // Its token lapsed in 1970, so nothing is sent anywhere.
    let home = root.join("claude-home");
    fs::create_dir(&home).unwrap();
    fs::write(
        home.join(".credentials.json"),
        json!({"claudeAiOauth": {"accessToken": "fake-claude-token", "expiresAt": 1_000}})
            .to_string(),
    )
    .unwrap();
    std::os::unix::fs::symlink(&home, root.join("claude-link")).unwrap();
    let credentials = root.join("claude-link/.credentials.json");
    let file = claude_credentials_stamp(&fs::canonicalize(&credentials).unwrap()).unwrap();
    let elsewhere = claude_credentials_stamp(Path::new("/srv/other/.credentials.json")).unwrap();
    fs::write(root.join("herdr"), FAKE_HERDR).unwrap();
    fs::set_permissions(root.join("herdr"), fs::Permissions::from_mode(0o755)).unwrap();
    // One session is the mark alone; more add the count. Both are as wide
    // as the provider mark the badge sits under.
    let badge = |count: usize| match count {
        1 => "\u{e1aa}  \u{2800}".to_string(),
        _ => format!("\u{e1aa} {count}\u{2800}"),
    };

    // No Hermes session: the Claude row, no badge.
    let published = refresh_claude(root, &credentials);
    assert!(
        published.contains_key("quota_acct_cl_icon"),
        "{published:?}"
    );
    assert!(
        !published.contains_key("quota_acct_cl_hermes"),
        "{published:?}"
    );

    // Two sessions on the file's token; one on another file; one whose key
    // was not seen in the file; one Codex session.
    let first = claude_session(root, "s1", Some(&file), now);
    let second = claude_session(root, "s2", Some(&file), now);
    let other = claude_session(root, "s3", Some(&elsewhere), now);
    let unproven = claude_session(root, "s4", None, now);
    let codex = hermes_session(root, "s5", &chatgpt_principal_stamp("a", "u"), now);
    let published = refresh_claude(root, &credentials);
    assert_eq!(
        published.get("quota_acct_cl_hermes"),
        Some(&badge(2)),
        "{published:?}"
    );
    assert!(
        !published.contains_key("quota_acct_cx_hermes"),
        "{published:?}"
    );
    // Neither the stamp, the path nor the token ever reaches Herdr.
    let report = fs::read_to_string(root.join("reports/w1")).unwrap();
    for secret in [
        file.as_str(),
        "fake-claude-token",
        "claude-home",
        "credentials",
    ] {
        assert!(!report.contains(secret), "{report}");
    }

    // A session that ends stops counting.
    drop(second);
    assert_eq!(
        refresh_claude(root, &credentials).get("quota_acct_cl_hermes"),
        Some(&badge(1))
    );

    // The Claude row reads another file now: nothing proves the sessions.
    let moved = root.join("moved.json");
    fs::copy(home.join(".credentials.json"), &moved).unwrap();
    assert!(!refresh_claude(root, &moved).contains_key("quota_acct_cl_hermes"));
    // ... and no file at all proves nothing either.
    assert!(!refresh_claude(root, &root.join("absent.json")).contains_key("quota_acct_cl_hermes"));

    drop(first);
    assert!(!refresh_claude(root, &credentials).contains_key("quota_acct_cl_hermes"));
    drop((other, unproven, codex));
}
