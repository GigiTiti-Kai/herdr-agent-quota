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
/// its `turn.lock`. When it fell back to a separate ProjectDirs cache, both
/// watchers published with different preferences and the bar flipped between
/// footer and pane (HAQ-15, measured again in HAQ-16).
///
/// While Herdr's watcher holds the lock the outside one stays alive and makes
/// no Herdr call; when Herdr's ends it takes over with the same preferences,
/// so the footer never moves back into the pane.
#[test]
fn an_outside_watcher_waits_silently_and_takes_over_without_moving_the_footer() {
    for herdr_dir in ["herdr-agent-usage", "state/herdr-agent-usage"] {
        let fixture = OutsideFixture::new(herdr_dir, false);
        let mut herdrs = fixture.watch(Some(&fixture.state));
        fixture.wait_for_report_from(herdrs.id());
        let mut outside = fixture.watch(None);
        thread::sleep(Duration::from_secs(3));
        assert!(
            outside.try_wait().unwrap().is_none(),
            "{herdr_dir}: the outside watcher exited instead of waiting"
        );
        let outside_pid = outside.id();
        let calls = fixture.calls();
        assert!(
            !calls.iter().any(|call| call.pid == outside_pid),
            "{herdr_dir}: the outside watcher called Herdr while Herdr's held the lock"
        );

        herdrs.kill().unwrap();
        herdrs.wait().unwrap();
        fixture.wait_for_report_from(outside_pid);
        outside.kill().unwrap();
        outside.wait().unwrap();

        let calls = fixture.calls();
        let footer = |pid: u32| {
            calls
                .iter()
                .filter(|call| {
                    call.pid == pid && call.args.starts_with("workspace report-metadata")
                })
                .flat_map(|call| call.tokens("--token"))
                .collect::<std::collections::BTreeSet<_>>()
        };
        let herdrs_footer = footer(herdrs.id());
        assert!(
            herdrs_footer.contains("quota_acct_cl_icon"),
            "{herdr_dir}: no footer: {calls:#?}"
        );
        assert_eq!(footer(outside_pid), herdrs_footer, "{herdr_dir}");
        for call in &calls {
            assert!(
                !call
                    .tokens("--clear-token")
                    .any(|name| name.starts_with("quota_acct_")),
                "{herdr_dir}: footer cleared: {call:?}"
            );
            assert!(
                !(call.args.starts_with("pane ")
                    && call
                        .tokens("--token")
                        .any(|name| name.starts_with("quota_acct_"))),
                "{herdr_dir}: account row moved into the pane: {call:?}"
            );
        }
        assert!(
            !fixture.legacy.join("turn.lock").exists(),
            "{herdr_dir}: the outside watcher used the old cache"
        );
    }
}

/// Both candidate directories exist and Herdr uses the second: no guess is
/// safe, so the outside watcher must stop instead of taking the other lock.
#[test]
fn an_ambiguous_state_dir_stops_the_outside_watcher() {
    for herdr_dir in ["state/herdr-agent-usage", "herdr-agent-usage"] {
        let fixture = OutsideFixture::new(herdr_dir, true);
        let mut herdrs = fixture.watch(Some(&fixture.state));
        fixture.wait_for_report_from(herdrs.id());
        let mut outside = fixture.watch(None);
        let deadline = Instant::now() + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = outside.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                outside.kill().unwrap();
                outside.wait().unwrap();
                panic!("{herdr_dir}: the outside watcher kept running");
            }
            thread::sleep(Duration::from_millis(20));
        };
        herdrs.kill().unwrap();
        herdrs.wait().unwrap();
        // Ambiguity is an error, so the failure is visible in watch.log.
        assert!(!status.success(), "{herdr_dir}");
        assert!(
            !fixture.calls().iter().any(|call| call.pid == outside.id()),
            "{herdr_dir}: the outside watcher called Herdr"
        );
        for other in ["herdr-agent-usage", "state/herdr-agent-usage"] {
            if other != herdr_dir {
                assert!(
                    !fixture.plugins.join(other).join("turn.lock").exists(),
                    "{herdr_dir}: the outside watcher took the other lock"
                );
            }
        }
        assert!(!fixture.legacy.join("turn.lock").exists());
    }
}

#[derive(Debug)]
struct HerdrCall {
    pid: u32,
    args: String,
}

impl HerdrCall {
    /// Token names following `flag` (`--token name=value`, `--clear-token name`).
    fn tokens<'a>(&'a self, flag: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        let words: Vec<&str> = self.args.split(' ').collect();
        words
            .windows(2)
            .filter(move |pair| pair[0] == flag)
            .map(|pair| pair[1].split('=').next().unwrap_or_default())
            .collect::<Vec<_>>()
            .into_iter()
    }
}

struct OutsideFixture {
    dir: tempfile::TempDir,
    home: std::path::PathBuf,
    plugins: std::path::PathBuf,
    state: std::path::PathBuf,
    legacy: std::path::PathBuf,
}

impl OutsideFixture {
    fn new(herdr_dir: &str, both_exist: bool) -> Self {
        use herdr_agent_quota::cache::CacheStore;
        use herdr_agent_quota::cli::SummaryFormat;
        use herdr_agent_quota::model::{Provider, ProviderSnapshot, UsageWindow, WindowKind};

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
        // The footer is on in Herdr's state dir and backed by a usable reading.
        let cache = CacheStore::new(&state);
        cache.set_account_summary(SummaryFormat::Compact).unwrap();
        let windows = vec![UsageWindow::new(WindowKind::Weekly, 35.0, None).unwrap()];
        cache
            .save(
                &ProviderSnapshot::new(Provider::Claude, windows.clone(), CacheStore::now_unix())
                    .session_local()
                    .with_account_windows(windows),
            )
            .unwrap();
        // Herdr's workspace already shows a footer row, so a publisher whose
        // summary is off has something to clear.
        // The old fallback directory exists on a machine that ran the CLI
        // before, with no summary preference: publishing from it clears the
        // footer.
        let legacy = home.join(".local/share/herdr-agent-usage");
        fs::create_dir_all(&legacy).unwrap();
        let herdr = dir.path().join("herdr");
        fs::write(
            &herdr,
            r#"#!/bin/sh
printf '%s %s\n' "$PPID" "$*" >> "$TEST_LOG"
case "$1 $2" in
  'agent list') printf '%s\n' '{"result":{"agents":[{"pane_id":"w1:p1","workspace_id":"w1","agent":"agy","agent_status":"working"}]}}' ;;
  'workspace list') printf '%s\n' '{"result":{"workspaces":[{"workspace_id":"w1","label":"api","tokens":{"quota_acct_cl_icon":"old"}}]}}' ;;
  *) printf '%s\n' '{"result":{}}' ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&herdr, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            dir,
            home,
            plugins,
            state,
            legacy,
        }
    }

    fn watch(&self, state_dir: Option<&std::path::Path>) -> std::process::Child {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-agent-usage"));
        command
            .args(["watch", "--provider", "agy", "--interval-seconds", "30"])
            .env_clear()
            .env("PATH", "")
            .env("HOME", &self.home)
            .env("HERDR_BIN_PATH", self.dir.path().join("herdr"))
            .env("HERDR_PLUGIN_CONFIG_DIR", self.dir.path())
            .env("HERDR_AGENT_QUOTA_AGENT_ORDER", "default")
            .env("TEST_LOG", self.dir.path().join("calls"))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(state_dir) = state_dir {
            command.env("HERDR_PLUGIN_STATE_DIR", state_dir);
        }
        command.spawn().unwrap()
    }

    fn calls(&self) -> Vec<HerdrCall> {
        fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let (pid, args) = line.split_once(' ')?;
                Some(HerdrCall {
                    pid: pid.parse().ok()?,
                    args: args.to_string(),
                })
            })
            .collect()
    }

    /// Until `pid` has sent its workspace footer, the last report of a pass.
    fn wait_for_report_from(&self, pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self
            .calls()
            .iter()
            .any(|call| call.pid == pid && call.args.starts_with("workspace report-metadata"))
        {
            assert!(Instant::now() < deadline, "watcher {pid} never published");
            thread::sleep(Duration::from_millis(20));
        }
    }
}
