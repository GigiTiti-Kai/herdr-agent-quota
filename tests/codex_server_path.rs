#![cfg(unix)]

use herdr_agent_quota::cache::CacheStore;
use herdr_agent_quota::cli::SummaryFormat;
use herdr_agent_quota::model::{Provider, WindowKind};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn executable(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

struct Fixture {
    dir: tempfile::TempDir,
    runtime: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        CacheStore::new(root.join("state"))
            .set_account_summary(SummaryFormat::Numbers)
            .unwrap();
        fs::write(root.join("usage"), "20\n").unwrap();
        fs::write(
            root.join("auth.json"),
            r#"{"tokens":{"account_id":"fixture"}}"#,
        )
        .unwrap();
        // The server finds the Codex wrapper, but no node on its own PATH.
        executable(&root.join("bin/codex"), "#!/bin/sh\nexec node app-server\n");
        let data = if cfg!(target_os = "macos") {
            "home/Library/Application Support"
        } else {
            "home/.local/share"
        };
        let runtime = root.join(data).join("fnm/aliases/default/bin");
        executable(
            &runtime.join("node"),
            r#"#!/bin/sh
while IFS= read -r request; do
  case "$request" in
    *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{}}' ;;
    *'"method":"account/read"'*) printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt","id":"fixture"}}}' ;;
    *'"method":"account/rateLimits/read"'*)
      read -r used < "$TEST_ROOT/usage"
      printf '{"id":3,"result":{"rateLimits":{"primary":{"usedPercent":%s,"windowDurationMins":10080}}}}\n' "$used" ;;
    *'"method":"thread/list"'*) printf '%s\n' '{"id":4,"result":{"data":[]}}' ;;
  esac
done
"#,
        );
        executable(
            &root.join("herdr"),
            r#"#!/bin/sh
case "$1 $2" in
  'agent list') printf '%s\n' '{"result":{"agents":[{"pane_id":"w1:p1","workspace_id":"w1","agent":"codex","agent_status":"working"}]}}' ;;
  'workspace list') printf '%s\n' '{"result":{"workspaces":[{"workspace_id":"w1","label":"fixture"}]}}' ;;
  'workspace report-metadata') printf '%s\n' "$@" >> "$TEST_ROOT/reports" ;;
  'pane get') printf '%s\n' '{"result":{"pane":{"scroll":{"offset_from_bottom":0}}}}' ;;
  'pane report-metadata') ;;
  *) exit 1 ;;
esac
"#,
        );
        Self { dir, runtime }
    }

    fn command(&self) -> Command {
        let root = self.dir.path();
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-agent-usage"));
        command
            .env_clear()
            .env("HOME", root.join("home"))
            .env("TMPDIR", root)
            .env("HERMES_HOME", root.join("home/.hermes"))
            .env("PATH", root.join("bin"))
            .env("CODEX_HOME", root.join("home/.codex"))
            .env("CODEX_AUTH_FILE", root.join("auth.json"))
            .env("HERDR_PLUGIN_STATE_DIR", root.join("state"))
            .env("HERDR_PLUGIN_CONFIG_DIR", root.join("config"))
            .env("HERDR_CONFIG_FILE", root.join("herdr.toml"))
            .env("HERDR_BIN_PATH", root.join("herdr"))
            .env("HERDR_AGENT_QUOTA_AGENTS", "only,codex")
            .env("HERDR_AGENT_QUOTA_AGENT_ORDER", "default")
            .env("TEST_ROOT", root);
        command
    }

    fn assert_usage(&self, used: f64) {
        let snapshot = CacheStore::new(self.dir.path().join("state"))
            .load(Provider::Codex)
            .unwrap()
            .expect("the collector must replace the cache under the server PATH");
        assert_eq!(
            snapshot.window(WindowKind::Weekly).unwrap().used_percent,
            used
        );
        let reports = fs::read_to_string(self.dir.path().join("reports")).unwrap();
        assert!(reports.contains(&format!("{}%", 100.0 - used)), "{reports}");
    }
}

#[test]
fn finding_node_does_not_replace_the_codex_wrapper_selected_on_path() {
    let fixture = Fixture::new();
    executable(&fixture.runtime.join("codex"), "#!/bin/sh\nexit 77\n");
    let output = fixture
        .command()
        .args(["refresh", "--provider", "codex", "--force", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    fixture.assert_usage(20.0);
}

#[test]
fn an_existing_node_on_path_wins_over_the_fnm_default() {
    let fixture = Fixture::new();
    // A non-executable earlier name must not hide the usable PATH runtime.
    let blocked = fixture.dir.path().join("blocked");
    executable(&blocked.join("node"), "#!/bin/sh\nexit 77\n");
    fs::set_permissions(blocked.join("node"), fs::Permissions::from_mode(0o644)).unwrap();
    fs::copy(
        fixture.runtime.join("node"),
        fixture.dir.path().join("bin/node"),
    )
    .unwrap();
    executable(&fixture.runtime.join("node"), "#!/bin/sh\nexit 77\n");
    let output = fixture
        .command()
        .env(
            "PATH",
            std::env::join_paths([blocked, fixture.dir.path().join("bin")]).unwrap(),
        )
        .args(["refresh", "--provider", "codex", "--force"])
        .output()
        .unwrap();
    assert!(output.status.success());
    fixture.assert_usage(20.0);
}

#[test]
fn a_non_executable_node_on_path_does_not_stop_quota_updates() {
    let fixture = Fixture::new();
    let refresh = || {
        let output = fixture
            .command()
            .args(["refresh", "--provider", "codex", "--force"])
            .output()
            .unwrap();
        assert!(output.status.success());
    };
    refresh();
    fixture.assert_usage(20.0);
    let blocked = fixture.dir.path().join("bin/node");
    executable(&blocked, "#!/bin/sh\nexit 77\n");
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(fixture.dir.path().join("usage"), "65\n").unwrap();
    refresh();
    fixture.assert_usage(65.0);
}

#[test]
fn a_non_executable_node_in_an_earlier_fallback_does_not_hide_fnm() {
    let fixture = Fixture::new();
    let blocked = fixture.dir.path().join("home/.local/bin/node");
    executable(&blocked, "#!/bin/sh\nexit 77\n");
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o644)).unwrap();
    let output = fixture
        .command()
        .args(["refresh", "--provider", "codex", "--force"])
        .output()
        .unwrap();
    assert!(output.status.success());
    fixture.assert_usage(20.0);
}

#[test]
fn an_explicit_codex_shim_can_find_node_in_a_custom_fnm_directory() {
    let fixture = Fixture::new();
    let custom = fixture.dir.path().join("custom fnm");
    fs::create_dir_all(custom.join("aliases/default")).unwrap();
    fs::rename(&fixture.runtime, custom.join("aliases/default/bin")).unwrap();
    let output = fixture
        .command()
        .env("FNM_DIR", custom)
        .env("CODEX_BIN_PATH", fixture.dir.path().join("bin/codex"))
        .args(["refresh", "--provider", "codex", "--force"])
        .output()
        .unwrap();
    assert!(output.status.success());
    fixture.assert_usage(20.0);
}

// A panic must also reap the watcher; nothing escapes the fixture HOME.
struct Watcher(Child);
impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn forced_refresh_updates_the_footer_without_an_interactive_node_path() {
    let fixture = Fixture::new();
    // Same command as the footer's plugin action. Hold the watcher lease so
    // its detached repair exits without racing this manual refresh test.
    let cache = CacheStore::new(fixture.dir.path().join("state"));
    let _lease = cache.try_lock_named("turn.lock").unwrap().unwrap();
    let output = fixture
        .command()
        .args(["refresh", "--provider", "all", "--force"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fixture.assert_usage(20.0);
    fs::write(fixture.dir.path().join("usage"), "65\n").unwrap();
    let output = fixture
        .command()
        .args(["refresh", "--provider", "all", "--force"])
        .output()
        .unwrap();
    assert!(output.status.success());
    fixture.assert_usage(65.0);
}

#[test]
fn watcher_updates_the_footer_without_an_interactive_node_path() {
    let fixture = Fixture::new();
    let mut watcher = Watcher(
        fixture
            .command()
            .args(["watch", "--provider", "codex", "--interval-seconds", "30"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fs::read_to_string(fixture.dir.path().join("reports"))
        .is_ok_and(|report| report.contains("80%"))
        && Instant::now() < deadline
    {
        assert!(
            watcher.0.try_wait().unwrap().is_none(),
            "watcher exited before publishing"
        );
        thread::sleep(Duration::from_millis(20));
    }
    fixture.assert_usage(20.0);
    fs::write(fixture.dir.path().join("usage"), "65\n").unwrap();
    CacheStore::new(fixture.dir.path().join("state"))
        .mark_refresh_account(Provider::Codex, 0, Some("fixture"))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(35);
    loop {
        let snapshot = CacheStore::new(fixture.dir.path().join("state"))
            .load(Provider::Codex)
            .unwrap()
            .unwrap();
        if snapshot.window(WindowKind::Weekly).unwrap().used_percent == 65.0 {
            // Fetch saves before publish; wait for the footer report too.
            if fs::read_to_string(fixture.dir.path().join("reports"))
                .unwrap()
                .contains("35%")
            {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "next automatic poll did not update the footer"
        );
        thread::sleep(Duration::from_millis(20));
    }
    fixture.assert_usage(65.0);
}
