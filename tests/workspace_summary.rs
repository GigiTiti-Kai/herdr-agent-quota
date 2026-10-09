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
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-agent-quota"))
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
