#![cfg(unix)]

use herdr_agent_quota::cli::AgentSelection;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn watcher_inherits_current_server_environment_after_handoff() {
    assert_watcher_adopts_environment("startup");
}

#[test]
fn refresh_repairs_a_watcher_when_plugin_enable_does_not_run_startup() {
    assert_watcher_adopts_environment("refresh");
}

fn assert_watcher_adopts_environment(entrypoint: &str) {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("herdr-old");
    let current = dir.path().join("herdr-current");
    fs::write(
        &old,
        "#!/bin/sh\nprintf '%s\\n' 'protocol_mismatch' >&2\nexit 1\n",
    )
    .unwrap();
    fs::write(
        &current,
        r#"#!/bin/sh
[ "$HERDR_SOCKET_PATH" = "$TEST_CURRENT_SOCKET" ] || exit 2
printf '%s\n' "$*" >> "$TEST_LOG"
printf '%s\n' '{"result":{"agents":[]}}'
"#,
    )
    .unwrap();
    for path in [&old, &current] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let log = dir.path().join("calls");
    let socket = dir.path().join("current.sock");
    let command = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_herdr-agent-usage"));
        cmd.env("HERDR_PLUGIN_STATE_DIR", dir.path())
            .env("HERDR_PLUGIN_CONFIG_DIR", dir.path())
            .env("HERDR_AGENT_QUOTA_AGENT_ORDER", "default")
            .env("TEST_CURRENT_SOCKET", &socket)
            .env("TEST_LOG", &log)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    };
    assert!(command()
        .args([entrypoint, "--provider", "agy"])
        .env("HERDR_BIN_PATH", &current)
        .env("HERDR_SOCKET_PATH", &socket)
        .status()
        .unwrap()
        .success());
    // Startup may launch its own short-lived watcher; wait for its lease.
    let cache = herdr_agent_quota::cache::CacheStore::new(dir.path());
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let expected = if entrypoint == "startup" { 2 } else { 1 };
        if fs::read_to_string(&log).unwrap().lines().count() >= expected
            && cache.try_lock_named("turn.lock").unwrap().is_some()
        {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    let before = fs::read_to_string(&log).unwrap().lines().count();
    let mut child = command()
        .args(["watch", "--provider", "agy"])
        .env("HERDR_BIN_PATH", &old)
        .env("HERDR_SOCKET_PATH", dir.path().join("old.sock"))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert!(status.is_some_and(|status| status.success()),
        "watcher kept polling the old incompatible Herdr client instead of adopting startup's environment");
    assert!(fs::read_to_string(log).unwrap().lines().count() > before);
    remove_state_after_watchers_stop(dir);
}

#[test]
fn every_supported_working_harness_keeps_the_watcher_alive() {
    for harness in AgentSelection::SUPPORTED {
        let harness = AgentSelection::harness_name(harness);
        let dir = tempfile::tempdir().unwrap();
        let herdr = dir.path().join("herdr");
        fs::write(&herdr, format!("#!/bin/sh\ntouch \"$TEST_INVENTORY\"\nprintf '%s\\n' '{{\"result\":{{\"agents\":[{{\"pane_id\":\"w1:p1\",\"agent\":\"{harness}\",\"agent_status\":\"working\"}}]}}}}'\n")).unwrap();
        fs::set_permissions(&herdr, fs::Permissions::from_mode(0o755)).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_herdr-agent-usage"))
            .args(["watch", "--provider", "all"])
            .env("HERDR_PLUGIN_STATE_DIR", dir.path())
            .env("HERDR_BIN_PATH", herdr)
            .env("TEST_INVENTORY", dir.path().join("inventory"))
            .env("CODEX_BIN_PATH", dir.path().join("absent"))
            .env("CODEX_AUTH_FILE", dir.path().join("absent"))
            .env("GROK_AUTH_FILE", dir.path().join("absent"))
            .env("DEVIN_CREDENTIALS_FILE", dir.path().join("absent"))
            .env("MUSE_AUTH_PATH", dir.path().join("absent"))
            .env("CURSOR_AUTH_FILE", dir.path().join("absent"))
            .env("CURSOR_STATE_DB", dir.path().join("absent"))
            .env("CLAUDE_CREDENTIALS_FILE", dir.path().join("absent"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !dir.path().join("inventory").exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(200));
        let premature = child.try_wait().unwrap();
        if premature.is_none() {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        assert!(
            premature.is_none(),
            "watcher exited while {harness} was working"
        );
    }
}

/// Whether a watcher with nothing to do is still running 3 s after its first pass.
fn idle_watcher_outlives_its_first_pass(auth: &std::path::Path) -> bool {
    let dir = tempfile::tempdir().unwrap();
    let herdr = dir.path().join("herdr");
    fs::write(
        &herdr,
        "#!/bin/sh\nprintf x >> \"$TEST_INVENTORY\"\nprintf '%s\\n' '{\"result\":{\"agents\":[]}}'\n",
    )
    .unwrap();
    fs::set_permissions(&herdr, fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_herdr-agent-usage"))
        .args(["watch", "--provider", "grok", "--interval-seconds", "30"])
        .env("HERDR_PLUGIN_STATE_DIR", dir.path())
        .env("HERDR_PLUGIN_CONFIG_DIR", dir.path())
        .env("HERDR_BIN_PATH", herdr)
        .env("TEST_INVENTORY", dir.path().join("inventory"))
        .env("GROK_AUTH_FILE", auth)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The first pass reads the inventory, then decides whether to stay.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !dir.path().join("inventory").exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let settle = Instant::now() + Duration::from_secs(3);
    let mut exited = child.try_wait().unwrap().is_some();
    while !exited && Instant::now() < settle {
        thread::sleep(Duration::from_millis(50));
        exited = child.try_wait().unwrap().is_some();
    }
    if !exited {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    !exited
}

#[test]
fn a_grok_login_keeps_an_idle_watcher_and_no_login_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let auth = dir.path().join("auth.json");
    assert!(
        !idle_watcher_outlives_its_first_pass(&auth),
        "a machine without a Grok login must not keep a watcher running"
    );
    fs::write(&auth, "{}").unwrap();
    assert!(
        idle_watcher_outlives_its_first_pass(&auth),
        "an idle watcher exited, so usage outside Herdr would stop showing"
    );
}

#[test]
fn partial_install_startup_and_events_ignore_unselected_working_agents() {
    let dir = tempfile::tempdir().unwrap();
    let herdr = dir.path().join("herdr");
    let log = dir.path().join("calls");
    fs::write(dir.path().join("agents"), "agy\n").unwrap();
    fs::write(
        &herdr,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$TEST_LOG"
printf '%s\n' '{"result":{"agents":[{"pane_id":"w1:p1","agent":"codex","agent_status":"working"}]}}'
"#,
    )
    .unwrap();
    fs::set_permissions(&herdr, fs::Permissions::from_mode(0o755)).unwrap();
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-agent-usage"));
        command
            .env("HERDR_PLUGIN_STATE_DIR", dir.path())
            .env("HERDR_PLUGIN_CONFIG_DIR", dir.path())
            .env_remove("HERDR_AGENT_QUOTA_AGENTS")
            .env("HERDR_AGENT_QUOTA_AGENT_ORDER", "default")
            .env("HERDR_BIN_PATH", &herdr)
            .env("HERDR_SOCKET_PATH", dir.path().join("test.sock"))
            .env("CODEX_BIN_PATH", dir.path().join("absent"))
            .env("GROK_AUTH_FILE", dir.path().join("absent"))
            .env("DEVIN_CREDENTIALS_FILE", dir.path().join("absent"))
            .env("MUSE_AUTH_PATH", dir.path().join("absent"))
            .env("CURSOR_AUTH_FILE", dir.path().join("absent"))
            .env("CURSOR_STATE_DB", dir.path().join("absent"))
            .env("CLAUDE_CREDENTIALS_FILE", dir.path().join("absent"))
            .env("TEST_LOG", &log)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    };
    assert!(command()
        .args(["startup", "--provider", "all"])
        .status()
        .unwrap()
        .success());
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs::read_to_string(&log).unwrap().lines().count() < 2 {
        assert!(
            Instant::now() < deadline,
            "startup watcher did not inspect inventory"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(command()
        .arg("event")
        .env(
            "HERDR_PLUGIN_EVENT_JSON",
            r#"{"pane_id":"w1:p1","agent":"codex","status":"working"}"#
        )
        .status()
        .unwrap()
        .success());
    assert!(
        fs::read_to_string(&log)
            .unwrap()
            .lines()
            .all(|line| { line == "agent list" || line == "workspace list" }),
        "unexpected Herdr calls: {}",
        fs::read_to_string(&log).unwrap()
    );
    let cache = herdr_agent_quota::cache::CacheStore::new(dir.path());
    assert!(!cache
        .should_debounce(
            herdr_agent_quota::model::Provider::Codex,
            herdr_agent_quota::cache::CacheStore::now_unix(),
            60
        )
        .unwrap());
    remove_state_after_watchers_stop(dir);
}

/// `startup` / `event` が起こした watcher を止めてから state を消す。
/// 走っている watcher の目の前で消すと、その周回の書き込みがディレクトリを
/// 作り直し、watcher が最大 1 時間 /tmp に残る（2026-09-24、10 回中 2 回）。
/// turn.lock を持ったまま消すので、遅れて起きた watcher も入れない。
fn remove_state_after_watchers_stop(dir: tempfile::TempDir) {
    let cache = herdr_agent_quota::cache::CacheStore::new(dir.path());
    cache.stop_turn_watchers().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let lock = loop {
        if let Some(lock) = cache.try_lock_named("turn.lock").unwrap() {
            break lock;
        }
        assert!(Instant::now() < deadline, "watcher did not stop");
        thread::sleep(Duration::from_millis(10));
    };
    dir.close().unwrap();
    drop(lock);
}

/// Herdr can start a hook or a detached watcher with neither
/// `HERDR_BIN_PATH` nor a `PATH`. The watcher must still find the user-local
/// Herdr CLI and publish, and say so when there is none.
#[test]
fn a_watcher_without_path_or_herdr_bin_path_still_publishes() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let local_bin = home.join(".local/bin");
    fs::create_dir_all(&local_bin).unwrap();
    let log = dir.path().join("calls");
    let herdr = local_bin.join("herdr");
    // Built-ins only: there is no PATH to find `touch` or `cat` on.
    fs::write(
        &herdr,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$TEST_LOG"
case "$1 $2" in
  'agent list') printf '%s\n' '{"result":{"agents":[{"pane_id":"w1:p1","workspace_id":"w1","agent":"agy","agent_status":"working"}]}}' ;;
  *) printf '%s\n' '{"result":{}}' ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&herdr, fs::Permissions::from_mode(0o755)).unwrap();
    let watch = |home: &std::path::Path, stderr: Stdio| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-agent-usage"));
        command
            .args(["watch", "--provider", "agy", "--interval-seconds", "30"])
            .env_clear()
            .env("PATH", "")
            .env("HOME", home)
            .env("HERDR_PLUGIN_STATE_DIR", dir.path())
            .env("HERDR_PLUGIN_CONFIG_DIR", dir.path())
            .env("HERDR_AGENT_QUOTA_AGENT_ORDER", "default")
            .env("TEST_LOG", &log)
            .stdout(Stdio::null())
            .stderr(stderr);
        command.spawn().unwrap()
    };

    let mut child = watch(&home, Stdio::null());
    let deadline = Instant::now() + Duration::from_secs(5);
    let published = loop {
        let calls = fs::read_to_string(&log).unwrap_or_default();
        if calls
            .lines()
            .any(|line| line.starts_with("pane report-metadata w1:p1") && line.contains("--token"))
        {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        thread::sleep(Duration::from_millis(20));
    };
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        published,
        "no tokens reached Herdr: {}",
        fs::read_to_string(&log).unwrap_or_default()
    );

    // No Herdr anywhere: the failure names the CLI instead of vanishing.
    let stderr = dir.path().join("stderr");
    let mut child = watch(
        &dir.path().join("no-home"),
        fs::File::create(&stderr).unwrap().into(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let logged = loop {
        let text = fs::read_to_string(&stderr).unwrap_or_default();
        if text.contains("watch: herdr agent list failed") || Instant::now() >= deadline {
            break text;
        }
        thread::sleep(Duration::from_millis(20));
    };
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        logged.contains("watch: herdr agent list failed"),
        "{logged}"
    );
}

/// A watcher started outside Herdr (a terminal, a systemd unit) carries no
/// `HERDR_PLUGIN_STATE_DIR`. It must still land on Herdr's state directory and
/// its `turn.lock`, so Herdr's own watcher stays the only publisher. When it
/// fell back to a separate ProjectDirs cache, both watchers published with
/// different preferences and the bar flipped between footer and pane.
#[test]
fn a_watcher_without_the_state_dir_variable_does_not_publish_beside_herdrs() {
    assert_outside_watcher_never_publishes("herdr-agent-usage", false);
    assert_outside_watcher_never_publishes("state/herdr-agent-usage", false);
}

/// Both candidate directories exist and Herdr uses the second: no guess is
/// safe, so the outside watcher must stop instead of taking the other lock.
#[test]
fn an_ambiguous_state_dir_stops_the_outside_watcher() {
    assert_outside_watcher_never_publishes("state/herdr-agent-usage", true);
    assert_outside_watcher_never_publishes("herdr-agent-usage", true);
}

fn assert_outside_watcher_never_publishes(herdr_dir: &str, both_exist: bool) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let plugins = home.join(".local/state/herdr/plugins");
    let state = plugins.join(herdr_dir);
    fs::create_dir_all(&state).unwrap();
    if both_exist {
        for other in ["herdr-agent-usage", "state/herdr-agent-usage"] {
            fs::create_dir_all(plugins.join(other)).unwrap();
        }
    }
    // The old fallback directory exists on a machine that ran the CLI before.
    let legacy = home.join(".local/share/herdr-agent-usage");
    fs::create_dir_all(&legacy).unwrap();
    let log = dir.path().join("calls");
    let herdr = dir.path().join("herdr");
    fs::write(
        &herdr,
        r#"#!/bin/sh
printf '%s %s\n' "$PPID" "$*" >> "$TEST_LOG"
case "$1 $2" in
  'agent list') printf '%s\n' '{"result":{"agents":[{"pane_id":"w1:p1","workspace_id":"w1","agent":"agy","agent_status":"working"}]}}' ;;
  *) printf '%s\n' '{"result":{}}' ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&herdr, fs::Permissions::from_mode(0o755)).unwrap();
    let watch = |state_dir: Option<&std::path::Path>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-agent-usage"));
        command
            .args(["watch", "--provider", "agy", "--interval-seconds", "30"])
            .env_clear()
            .env("PATH", "")
            .env("HOME", &home)
            .env("HERDR_BIN_PATH", &herdr)
            .env("HERDR_PLUGIN_CONFIG_DIR", dir.path())
            .env("HERDR_AGENT_QUOTA_AGENT_ORDER", "default")
            .env("TEST_LOG", &log)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(state_dir) = state_dir {
            command.env("HERDR_PLUGIN_STATE_DIR", state_dir);
        }
        command.spawn().unwrap()
    };

    let mut herdrs = watch(Some(&state));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fs::read_to_string(&log)
        .unwrap_or_default()
        .contains("report-metadata")
    {
        assert!(Instant::now() < deadline, "Herdr's watcher never published");
        thread::sleep(Duration::from_millis(20));
    }
    let mut outside = watch(None);
    let deadline = Instant::now() + Duration::from_secs(3);
    let outside_status = loop {
        if let Some(status) = outside.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            outside.kill().unwrap();
            outside.wait().unwrap();
            break None;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let outside_pid = outside.id().to_string();
    herdrs.kill().unwrap();
    herdrs.wait().unwrap();
    let calls = fs::read_to_string(&log).unwrap_or_default();
    let case = format!("{herdr_dir} both={both_exist}");
    let status = outside_status
        .unwrap_or_else(|| panic!("{case}: a second watcher kept running beside Herdr's: {calls}"));
    // Ambiguity is an error, so the failure is visible in watch.log.
    assert_eq!(status.success(), !both_exist, "{case}");
    assert!(
        !calls
            .lines()
            .any(|line| line.starts_with(&format!("{outside_pid} "))),
        "{case}: the outside watcher called Herdr: {calls}"
    );
    for other in ["herdr-agent-usage", "state/herdr-agent-usage"] {
        if other != herdr_dir {
            assert!(
                !plugins.join(other).join("turn.lock").exists(),
                "{case}: the outside watcher took the other lock"
            );
        }
    }
    assert!(
        !legacy.join("turn.lock").exists(),
        "{case}: the outside watcher used the old cache"
    );
}
