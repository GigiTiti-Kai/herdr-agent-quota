#![cfg(unix)]

//! An idle Argon pane's published `$quota_icon` follows its Arg bucket.

use herdr_agent_quota::cache::CacheStore;
use herdr_agent_quota::model::Provider;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

/// The `quota_icon` value one `refresh --provider agy` reports for an idle
/// Argon pane whose Arg bucket has `arg_left` of 1 left. A second refresh
/// over the published tokens must report nothing: a tagged icon that is
/// rewritten every pass would repaint the pane every pass.
fn published_icon(arg_left: f64) -> String {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let cache = CacheStore::new(root);
    let payload = json!({
        "session_id": "w1:p1",
        "model": {"display_name": "Gemini 4 Argon (High)"},
        "quota": {
            "gemini-5h": {"remaining_fraction": 0.9},
            "gemini-weekly": {"remaining_fraction": 0.9},
            "gemini-argon-wtu-weekly": {"remaining_fraction": arg_left},
        }
    });
    let snapshot =
        herdr_agent_quota::providers::agy::parse_statusline(&payload, CacheStore::now_unix())
            .unwrap();
    cache
        .save_statusline_observation(Provider::Agy, snapshot, &payload)
        .unwrap();
    fs::write(
        root.join("herdr"),
        r#"#!/bin/sh
case "$1 $2" in
  'agent list') cat "$TEST_INVENTORY" ;;
  'pane get') printf '%s\n' '{"result":{"pane":{"scroll":{"offset_from_bottom":0}}}}' ;;
  'pane report-metadata') printf '%s\n' "$@" >> "$TEST_REPORT" ;;
  *) exit 1 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(root.join("herdr"), fs::Permissions::from_mode(0o755)).unwrap();
    let refresh = |tokens: &BTreeMap<String, String>| {
        fs::write(
            root.join("inventory.json"),
            serde_json::to_vec(&json!({"result":{"agents":[{
                "agent":"agy", "pane_id":"w1:p1", "agent_status":"idle", "tokens":tokens
            }]}}))
            .unwrap(),
        )
        .unwrap();
        fs::write(root.join("report"), "").unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_herdr-agent-usage"))
            .args(["refresh", "--provider", "agy"])
            .env("HERDR_PLUGIN_STATE_DIR", root)
            .env("HERDR_PLUGIN_CONFIG_DIR", root)
            .env("HERDR_BIN_PATH", root.join("herdr"))
            .env("HERDR_CONFIG_FILE", root.join("config.toml"))
            .env("HERDR_SOCKET_PATH", root.join("herdr.sock"))
            .env("XDG_STATE_HOME", root.join("xdg-state"))
            .env("HERDR_AGENT_QUOTA_AGENTS", "agy")
            .env("TEST_INVENTORY", root.join("inventory.json"))
            .env("TEST_REPORT", root.join("report"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::read_to_string(root.join("report")).unwrap()
    };
    fs::write(root.join("config.toml"), "").unwrap();
    let report = refresh(&BTreeMap::new());
    let args: Vec<_> = report.lines().collect();
    let mut tokens = BTreeMap::new();
    for pair in args.windows(2) {
        match pair[0] {
            "--token" => {
                let (name, value) = pair[1].split_once('=').unwrap();
                tokens.insert(name.to_string(), value.to_string());
            }
            "--clear-token" => {
                tokens.remove(pair[1]);
            }
            _ => {}
        }
    }
    let icon = tokens
        .get("quota_icon")
        .unwrap_or_else(|| panic!("no quota_icon in the report:\n{report}"))
        .clone();
    let again = refresh(&tokens);
    assert!(again.is_empty(), "second pass rewrote the pane:\n{again}");
    icon
}

#[test]
fn an_idle_argon_icon_differs_between_a_full_and_a_spent_arg() {
    let full = published_icon(0.95);
    let spent = published_icon(0.01);
    assert_ne!(full, spent);
    assert!(spent.contains(herdr_agent_quota::icons::QUOTA_DANGER_TAG));
    assert!(!full.contains(herdr_agent_quota::icons::QUOTA_DANGER_TAG));
    assert!(!full.contains(herdr_agent_quota::icons::QUOTA_WARNING_TAG));
}
