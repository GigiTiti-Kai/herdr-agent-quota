#![cfg(unix)]
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[test]
fn normal_upgrade_preserves_preferences_and_runs_recovery_in_the_server_environment() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    let config = dir.path().join("config");
    let plugin = dir.path().join("plugin/target/release");
    for path in [&bin, &config, &plugin] {
        fs::create_dir_all(path).unwrap();
    }
    fs::write(config.join("agents"), "codex,omp\n").unwrap();
    fs::write(config.join("sidebar-layout"), "stacked\n").unwrap();
    fs::write(config.join("watch-interval-seconds"), "300\n").unwrap();
    let log = dir.path().join("calls");
    let manifest: toml::Value = toml::from_str(include_str!("../herdr-plugin.toml")).unwrap();
    let configure = manifest["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["id"].as_str() == Some("configure"))
        .unwrap()["command"][2]
        .as_str()
        .unwrap();
    for (path, script) in [
        (bin.join("cargo"), "#!/bin/sh\nexit 0\n"),
        (
            plugin.join("herdr-agent-usage"),
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$TEST_LOG\"\n",
        ),
        (
            bin.join("herdr"),
            r#"#!/bin/sh
case "$1 $2" in
  'plugin link') exit 0 ;;
  'plugin list') exit 0 ;;
  'plugin config-dir') printf '%s\n' "$TEST_CONFIG" ;;
  'plugin action')
    HERDR_PLUGIN_ROOT="$TEST_PLUGIN" sh -c "$TEST_ACTION" || exit 1
    printf '%s\n' '{"log_id":"upgrade-test","status":"succeeded"}' ;;
  'plugin log') printf '%s\n' '{"log_id":"upgrade-test","status":"succeeded"}' ;;
  'server reload-config') printf '%s\n' reload-config >> "$TEST_LOG" ;;
  *) exit 2 ;;
esac
"#,
        ),
    ] {
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let output = Command::new("bash")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
        .env_remove("HERDR_SOCKET_PATH")
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("TEST_CONFIG", &config)
        .env("TEST_PLUGIN", dir.path().join("plugin"))
        .env("TEST_ACTION", configure)
        .env("TEST_LOG", &log)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(log).unwrap(),
        "configure --apply\nreload-config\nstartup --provider all\n"
    );
    assert_eq!(
        fs::read_to_string(config.join("agents")).unwrap(),
        "codex,omp\n"
    );
    assert_eq!(
        fs::read_to_string(config.join("sidebar-layout")).unwrap(),
        "stacked\n"
    );
    assert_eq!(
        fs::read_to_string(config.join("watch-interval-seconds")).unwrap(),
        "300\n"
    );
}

#[test]
fn an_explicit_install_agent_list_is_stored_as_a_subset() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    let config = dir.path().join("config");
    let plugin = dir.path().join("plugin/target/release");
    for path in [&bin, &config, &plugin] {
        fs::create_dir_all(path).unwrap();
    }
    let log = dir.path().join("calls");
    let manifest: toml::Value = toml::from_str(include_str!("../herdr-plugin.toml")).unwrap();
    let configure = manifest["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["id"].as_str() == Some("configure"))
        .unwrap()["command"][2]
        .as_str()
        .unwrap();
    for (path, script) in [
        (bin.join("cargo"), "#!/bin/sh\nexit 0\n"),
        (
            plugin.join("herdr-agent-usage"),
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$TEST_LOG\"\n",
        ),
        (
            bin.join("herdr"),
            r#"#!/bin/sh
case "$1 $2" in
  'plugin link') exit 0 ;;
  'plugin list') exit 0 ;;
  'plugin config-dir') printf '%s\n' "$TEST_CONFIG" ;;
  'plugin action')
    HERDR_PLUGIN_ROOT="$TEST_PLUGIN" sh -c "$TEST_ACTION" || exit 1
    printf '%s\n' '{"log_id":"upgrade-test","status":"succeeded"}' ;;
  'plugin log') printf '%s\n' '{"log_id":"upgrade-test","status":"succeeded"}' ;;
  'server reload-config') printf '%s\n' reload-config >> "$TEST_LOG" ;;
  *) exit 2 ;;
esac
"#,
        ),
    ] {
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let output = Command::new("bash")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
        .args(["--agent", "claude,codex,grok,agy,opencode,pi,omp,devin"])
        .env_remove("HERDR_SOCKET_PATH")
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("TEST_CONFIG", &config)
        .env("TEST_PLUGIN", dir.path().join("plugin"))
        .env("TEST_ACTION", configure)
        .env("TEST_LOG", &log)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(config.join("agents")).unwrap(),
        "only,claude,codex,grok,agy,opencode,pi,omp,devin\n"
    );
}

#[test]
fn install_adopts_alias_plugin_dirs_then_unlinks_the_old_id() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    let old_config = dir.path().join("old-config");
    let new_config = dir.path().join("new-config");
    let xdg_state = dir.path().join("xdg-state");
    let old_state = xdg_state.join("herdr/plugins/herdr-agent-quota");
    let plugin = dir.path().join("plugin/target/release");
    for path in [&bin, &old_config, &new_config, &old_state, &plugin] {
        fs::create_dir_all(path).unwrap();
    }
    fs::write(old_config.join("agents"), "only,claude\n").unwrap();
    fs::write(old_config.join("sidebar-layout"), "stacked\n").unwrap();
    fs::write(old_state.join("owned-font"), "keep-me\n").unwrap();
    let log = dir.path().join("calls");
    let manifest: toml::Value = toml::from_str(include_str!("../herdr-plugin.toml")).unwrap();
    let configure = manifest["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["id"].as_str() == Some("configure"))
        .unwrap()["command"][2]
        .as_str()
        .unwrap();
    for (path, script) in [
        (bin.join("cargo"), "#!/bin/sh\nexit 0\n"),
        (
            plugin.join("herdr-agent-usage"),
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$TEST_LOG\"\n",
        ),
        (
            bin.join("herdr"),
            r#"#!/bin/sh
case "$1 $2" in
  'plugin link') exit 0 ;;
  'plugin list') printf '%s\n' 'herdr-agent-quota' ;;
  'plugin config-dir')
    case "$3" in
      herdr-agent-quota) printf '%s\n' "$TEST_OLD_CONFIG" ;;
      *) printf '%s\n' "$TEST_CONFIG" ;;
    esac ;;
  'plugin disable'|'plugin unlink'|'plugin enable')
    printf '%s %s %s\n' "$1" "$2" "${3-}" >> "$TEST_LOG" ;;
  'plugin action')
    HERDR_PLUGIN_ROOT="$TEST_PLUGIN" sh -c "$TEST_ACTION" || exit 1
    printf '%s\n' '{"log_id":"upgrade-test","status":"succeeded"}' ;;
  'plugin log') printf '%s\n' '{"log_id":"upgrade-test","status":"succeeded"}' ;;
  'server reload-config') printf '%s\n' reload-config >> "$TEST_LOG" ;;
  *) exit 2 ;;
esac
"#,
        ),
    ] {
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let output = Command::new("bash")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
        .env_remove("HERDR_SOCKET_PATH")
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("HOME", dir.path())
        .env("XDG_STATE_HOME", &xdg_state)
        .env("TEST_CONFIG", &new_config)
        .env("TEST_OLD_CONFIG", &old_config)
        .env("TEST_PLUGIN", dir.path().join("plugin"))
        .env("TEST_ACTION", configure)
        .env("TEST_LOG", &log)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(new_config.join("agents")).unwrap(),
        "only,claude\n"
    );
    assert_eq!(
        fs::read_to_string(new_config.join("sidebar-layout")).unwrap(),
        "stacked\n"
    );
    let adopted_state = xdg_state.join("herdr/plugins/herdr-agent-usage/owned-font");
    assert_eq!(fs::read_to_string(adopted_state).unwrap(), "keep-me\n");
    let calls = fs::read_to_string(log).unwrap();
    assert!(calls.contains("plugin unlink herdr-agent-quota"), "{calls}");
    assert!(calls.contains("configure --apply"), "{calls}");
}

#[test]
fn install_adopts_alias_directories_when_the_old_id_is_no_longer_listed() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    let new_config = dir.path().join("new-config");
    let xdg_state = dir.path().join("xdg-state");
    let old_config = dir
        .path()
        .join(".config/herdr/plugins/config/herdr-agent-quota");
    let old_state = xdg_state.join("herdr/plugins/herdr-agent-quota");
    let plugin = dir.path().join("plugin/target/release");
    for path in [&bin, &new_config, &old_config, &old_state, &plugin] {
        fs::create_dir_all(path).unwrap();
    }
    fs::write(old_config.join("quota-percent"), "remaining\n").unwrap();
    fs::write(old_state.join("cursor-keychain-approved"), "ok\n").unwrap();
    fs::write(old_state.join("herdr-agent-quota-hooks.sh"), "stay\n").unwrap();
    let log = dir.path().join("calls");
    let manifest: toml::Value = toml::from_str(include_str!("../herdr-plugin.toml")).unwrap();
    let configure = manifest["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["id"].as_str() == Some("configure"))
        .unwrap()["command"][2]
        .as_str()
        .unwrap();
    for (path, script) in [
        (bin.join("cargo"), "#!/bin/sh\nexit 0\n"),
        (
            plugin.join("herdr-agent-usage"),
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$TEST_LOG\"\n",
        ),
        (
            bin.join("herdr"),
            r#"#!/bin/sh
case "$1 $2" in
  'plugin link') exit 0 ;;
  'plugin list') printf '%s\n' 'herdr-agent-usage' ;;
  'plugin config-dir')
    case "$3" in
      herdr-agent-quota) exit 1 ;;
      *) printf '%s\n' "$TEST_CONFIG" ;;
    esac ;;
  'plugin disable'|'plugin unlink'|'plugin enable')
    printf '%s %s %s\n' "$1" "$2" "${3-}" >> "$TEST_LOG" ;;
  'plugin action')
    HERDR_PLUGIN_ROOT="$TEST_PLUGIN" sh -c "$TEST_ACTION" || exit 1
    printf '%s\n' '{"log_id":"upgrade-test","status":"succeeded"}' ;;
  'plugin log') printf '%s\n' '{"log_id":"upgrade-test","status":"succeeded"}' ;;
  'server reload-config') printf '%s\n' reload-config >> "$TEST_LOG" ;;
  *) exit 2 ;;
esac
"#,
        ),
    ] {
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let output = Command::new("bash")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("XDG_CONFIG_HOME")
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("HOME", dir.path())
        .env("XDG_STATE_HOME", &xdg_state)
        .env("TEST_CONFIG", &new_config)
        .env("TEST_PLUGIN", dir.path().join("plugin"))
        .env("TEST_ACTION", configure)
        .env("TEST_LOG", &log)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout {}\nstderr {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(new_config.join("quota-percent")).unwrap(),
        "remaining\n"
    );
    assert_eq!(
        fs::read_to_string(
            xdg_state.join("herdr/plugins/herdr-agent-usage/cursor-keychain-approved")
        )
        .unwrap(),
        "ok\n"
    );
    assert_eq!(
        fs::read_to_string(old_state.join("herdr-agent-quota-hooks.sh")).unwrap(),
        "stay\n"
    );
    let calls = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !calls.contains("plugin unlink herdr-agent-quota"),
        "{calls}"
    );
}
