//! Merge a silent Cursor CLI hook collector into `~/.cursor/hooks.json`.
//!
//! Herdr already owns `sessionStart` in that file. This collector only adds
//! `afterAgentResponse`, `stop`, and `preCompact`, and never replaces or
//! removes a sessionStart entry. A Cursor `statusLine` is not installed: that
//! setting replaces the native CLI footer.

use crate::cache::CacheStore;
use crate::providers::cursor;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const MANAGED_SUBCOMMAND: &str = "cursor-hooks";
const SCRIPT_NAME: &str = "herdr-agent-quota-hooks.sh";
const SCRIPT_MARKER: &str = "managed by herdr-agent-quota";
const HOOK_EVENTS: [&str; 3] = ["afterAgentResponse", "stop", "preCompact"];

pub fn check() -> Result<()> {
    let path = hooks_path()?;
    if is_installed(&read_hooks(&path)?) {
        println!(
            "Cursor cache/context collector is installed: {}",
            path.display()
        );
    } else {
        println!(
            "Cursor cache/context collector is not installed: {}",
            path.display()
        );
    }
    Ok(())
}

pub fn apply() -> Result<()> {
    let cache = CacheStore::from_env()?;
    let executable = std::env::current_exe().context("resolve plugin executable")?;
    apply_at(&hooks_path()?, cache.root(), &executable)
}

pub fn uninstall() -> Result<()> {
    uninstall_at(&hooks_path()?)
}

pub fn apply_at(path: &Path, state: &Path, executable: &Path) -> Result<()> {
    let script = script_path(path);
    write_wrapper_script(&script, state, executable)?;
    let command = wrapper_command(&script);
    let mut root = read_hooks(path)?;
    let Some(hooks) = hooks_object_mut(&mut root)? else {
        anyhow::bail!("Cursor hooks.json must be a JSON object");
    };
    for event in HOOK_EVENTS {
        install_event(hooks, event, &command);
    }
    write_hooks(path, &root)
}

pub fn uninstall_at(path: &Path) -> Result<()> {
    remove_wrapper_script(&script_path(path));
    if !path.exists() {
        return Ok(());
    }
    let mut root = read_hooks(path)?;
    let (changed, empty) = {
        let Some(hooks) = hooks_object_mut(&mut root)? else {
            return Ok(());
        };
        let mut changed = false;
        for event in HOOK_EVENTS {
            changed |= remove_ours(hooks, event);
        }
        (changed, hooks.is_empty())
    };
    if !changed {
        return Ok(());
    }
    if empty && !path_has_unrelated_keys(&root) {
        fs::remove_file(path).context("remove empty Cursor hooks.json")?;
        return Ok(());
    }
    write_hooks(path, &root)
}

/// Consume one Cursor hook payload and store cache/context for that session.
///
/// Always exits successfully: Cursor awaits the hook, and a parse failure
/// must not fail the agent turn. Stdout is left empty so we never feed a
/// `user_message` back into `preCompact`.
pub fn run_hook() -> Result<()> {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    if let Ok(value) = serde_json::from_slice::<Value>(&input) {
        if let Ok(cache) = CacheStore::from_env() {
            let _ = cursor::save_hook_observation(cache.root(), &value);
        }
    }
    Ok(())
}

pub(crate) fn hooks_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CURSOR_HOOKS_FILE").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    if let Some(home) = std::env::var_os("CURSOR_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home).join("hooks.json"));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".cursor/hooks.json"))
}

fn script_path(hooks: &Path) -> PathBuf {
    hooks
        .parent()
        .map(|parent| parent.join(SCRIPT_NAME))
        .unwrap_or_else(|| PathBuf::from(SCRIPT_NAME))
}

fn wrapper_command(script: &Path) -> String {
    format!("bash {}", shell_quote(script))
}

fn write_wrapper_script(script: &Path, state: &Path, executable: &Path) -> Result<()> {
    if let Some(parent) = script.parent() {
        fs::create_dir_all(parent).context("create Cursor hooks directory")?;
    }
    let contents = format!(
        "#!/bin/sh\n# {SCRIPT_MARKER}; reinstalling the collector replaces this file.\nexport HERDR_PLUGIN_STATE_DIR={}\nexec {} {MANAGED_SUBCOMMAND}\n",
        shell_quote(state),
        shell_quote(executable),
    );
    fs::write(script, contents).context("write Cursor collector hook script")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(script)
            .context("read Cursor collector hook script metadata")?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(script, permissions).context("chmod Cursor collector hook script")?;
    }
    Ok(())
}

fn remove_wrapper_script(script: &Path) {
    let Ok(contents) = fs::read_to_string(script) else {
        return;
    };
    if contents.contains(SCRIPT_MARKER) && contents.contains(MANAGED_SUBCOMMAND) {
        let _ = fs::remove_file(script);
    }
}

fn is_ours(command: &str) -> bool {
    command.contains(SCRIPT_NAME)
        || (command.contains(MANAGED_SUBCOMMAND)
            && (command.contains("herdr-agent-quota")
                || command.contains("HERDR_PLUGIN_STATE_DIR=")))
}

fn is_installed(root: &Value) -> bool {
    let Some(hooks) = root.get("hooks").and_then(Value::as_object) else {
        return false;
    };
    HOOK_EVENTS.iter().all(|event| {
        hook_commands(hooks.get(*event))
            .iter()
            .any(|command| is_ours(command))
    })
}

fn install_event(hooks: &mut serde_json::Map<String, Value>, event: &str, command: &str) {
    let mut entries = match hooks.get(event) {
        Some(Value::Array(values)) => values.clone(),
        _ => Vec::new(),
    };
    entries.retain(|entry| !entry_is_ours(entry));
    entries.push(json!({ "command": command }));
    hooks.insert(event.to_string(), Value::Array(entries));
}

fn remove_ours(hooks: &mut serde_json::Map<String, Value>, event: &str) -> bool {
    let Some(Value::Array(entries)) = hooks.get_mut(event) else {
        return false;
    };
    let before = entries.len();
    entries.retain(|entry| !entry_is_ours(entry));
    let changed = entries.len() != before;
    if entries.is_empty() {
        hooks.remove(event);
    }
    changed
}

fn entry_is_ours(entry: &Value) -> bool {
    match entry {
        Value::String(command) => is_ours(command),
        Value::Object(map) => map
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(is_ours),
        _ => false,
    }
}

fn hook_commands(value: Option<&Value>) -> Vec<&str> {
    match value {
        Some(Value::Array(entries)) => entries
            .iter()
            .filter_map(|entry| match entry {
                Value::String(command) => Some(command.as_str()),
                Value::Object(map) => map.get("command").and_then(Value::as_str),
                _ => None,
            })
            .collect(),
        Some(Value::String(command)) => vec![command.as_str()],
        _ => Vec::new(),
    }
}

fn hooks_object_mut(root: &mut Value) -> Result<Option<&mut serde_json::Map<String, Value>>> {
    if !root.is_object() {
        anyhow::bail!("Cursor hooks.json must be a JSON object");
    }
    if !root.get("hooks").is_some_and(Value::is_object) {
        root.as_object_mut()
            .expect("object")
            .insert("hooks".to_string(), json!({}));
    }
    if root.get("version").is_none() {
        root.as_object_mut()
            .expect("object")
            .insert("version".to_string(), json!(1));
    }
    Ok(root.get_mut("hooks").and_then(Value::as_object_mut))
}

fn path_has_unrelated_keys(root: &Value) -> bool {
    root.as_object()
        .is_some_and(|object| object.keys().any(|key| key != "hooks" && key != "version"))
}

fn read_hooks(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({ "version": 1, "hooks": {} }));
    }
    let value: Value = serde_json::from_slice(
        &fs::read(path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))?;
    if !value.is_object() {
        anyhow::bail!("Cursor hooks.json must be a JSON object");
    }
    Ok(value)
}

fn write_hooks(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("create Cursor hooks directory")?;
    }
    let temporary = path.with_extension("json.herdr-agent-quota.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?).context("write Cursor hooks.json")?;
    fs::rename(&temporary, path).context("replace Cursor hooks.json")
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn apply_merges_collector_hooks_without_touching_session_start() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        fs::write(
            &path,
            r#"{
  "version": 1,
  "hooks": {
    "sessionStart": [{ "command": "bash '/tmp/herdr-agent-state.sh' session" }]
  }
}"#,
        )
        .unwrap();
        apply_at(&path, dir.path(), Path::new("/opt/herdr-agent-quota")).unwrap();
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let session = &value["hooks"]["sessionStart"][0]["command"];
        assert_eq!(session, "bash '/tmp/herdr-agent-state.sh' session");
        let script = dir.path().join(SCRIPT_NAME);
        let script_text = fs::read_to_string(&script).unwrap();
        assert!(script_text.contains("HERDR_PLUGIN_STATE_DIR="));
        assert!(script_text.contains("/opt/herdr-agent-quota"));
        assert!(script_text.contains(MANAGED_SUBCOMMAND));
        for event in HOOK_EVENTS {
            let command = value["hooks"][event][0]["command"].as_str().unwrap();
            assert!(command.contains("bash"));
            assert!(command.contains(SCRIPT_NAME));
            assert!(!command.contains("HERDR_PLUGIN_STATE_DIR="));
        }
    }

    #[test]
    fn apply_replaces_only_our_command_and_keeps_a_user_hook() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        fs::write(
            &path,
            r#"{
  "hooks": {
    "afterAgentResponse": [
      { "command": "echo user" },
      { "command": "HERDR_PLUGIN_STATE_DIR='/old' '/old/herdr-agent-quota' cursor-hooks" }
    ]
  }
}"#,
        )
        .unwrap();
        apply_at(&path, dir.path(), Path::new("/new/herdr-agent-quota")).unwrap();
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let commands: Vec<&str> = value["hooks"]["afterAgentResponse"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["command"].as_str().unwrap())
            .collect();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0], "echo user");
        assert!(commands[1].contains(SCRIPT_NAME));
        assert!(!commands[1].contains("/old/herdr-agent-quota"));
        let script_text = fs::read_to_string(dir.path().join(SCRIPT_NAME)).unwrap();
        assert!(script_text.contains("/new/herdr-agent-quota"));
    }

    #[test]
    fn uninstall_removes_our_hooks_and_leaves_session_start() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        apply_at(&path, dir.path(), Path::new("/opt/herdr-agent-quota")).unwrap();
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["hooks"]["sessionStart"] = json!([{ "command": "herdr session" }]);
        fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        uninstall_at(&path).unwrap();
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            value["hooks"]["sessionStart"][0]["command"],
            "herdr session"
        );
        for event in HOOK_EVENTS {
            assert!(value["hooks"].get(event).is_none());
        }
    }

    #[test]
    fn uninstall_deletes_a_file_that_only_held_our_hooks() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        apply_at(&path, dir.path(), Path::new("/opt/herdr-agent-quota")).unwrap();
        let script = dir.path().join(SCRIPT_NAME);
        assert!(script.exists());
        uninstall_at(&path).unwrap();
        assert!(!path.exists());
        assert!(!script.exists());
    }
}
