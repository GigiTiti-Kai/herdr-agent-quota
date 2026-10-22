use super::statusline::{settings_path, statusline_disabled, Adapter};
use crate::cache::CacheStore;
use crate::model::Provider;
use crate::providers::agy::parse_statusline;
use anyhow::{Context, Result};
use serde_json::Value;
use std::io::{Read, Write};
use std::path::Path;

const CONFIG: Adapter = Adapter {
    label: "Agy",
    subcommand: "agy-statusline",
    backup_file: "agy-statusline.original.json",
};

pub fn check() -> Result<()> {
    let cache = CacheStore::from_env()?;
    let executable = std::env::current_exe().context("resolve plugin executable")?;
    CONFIG.check(
        &settings_path("AGY_SETTINGS_FILE", ".gemini/antigravity-cli/settings.json")?,
        cache.root(),
        &executable,
    )
}

pub fn apply() -> Result<()> {
    let cache = CacheStore::from_env()?;
    let executable = std::env::current_exe().context("resolve plugin executable")?;
    apply_at(
        &settings_path("AGY_SETTINGS_FILE", ".gemini/antigravity-cli/settings.json")?,
        cache.root(),
        &executable,
    )
}

pub fn uninstall() -> Result<()> {
    let cache = CacheStore::from_env()?;
    uninstall_at(
        &settings_path("AGY_SETTINGS_FILE", ".gemini/antigravity-cli/settings.json")?,
        cache.root(),
    )
}

/// What the pane-keyed Agy cache may still claim as current.
#[derive(Debug, Default, PartialEq)]
pub struct CollectorState {
    /// `/statusline` wrote `enabled:false` and nothing was observed since:
    /// the cached context belongs to an earlier conversation.
    pub disabled: bool,
    /// Root `model` of `settings.json`, which `/model` rewrites, and when it
    /// was written (unix seconds). For a pane observed before that write it
    /// is the newest evidence of the active model, whether or not the hook
    /// still runs; the snapshot decides per pane.
    pub settings_model: Option<(String, u64)>,
}

/// Agy's `/statusline` toggle writes `enabled:false`; its pane-keyed
/// observations then stop updating and would outlive the conversation.
/// An Agy started before that write keeps calling the collector, so an
/// observation saved after the settings changed proves it is still live.
pub fn collector_state(observation: &Path) -> CollectorState {
    settings_path("AGY_SETTINGS_FILE", ".gemini/antigravity-cli/settings.json")
        .map(|settings| state_at(&settings, observation))
        .unwrap_or_default()
}

fn state_at(settings: &Path, observation: &Path) -> CollectorState {
    let modified = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
    };
    let settings_newer = match (modified(observation), modified(settings)) {
        (Some(observed), Some(written)) => observed < written,
        _ => true,
    };
    // Compared with each pane's own last observation, not the shared
    // observation file: another Agy calling the collector must not hand a
    // silent pane back its stale pane-keyed model.
    // shortcut: `model` is one value for every Agy pane, so a pane that has
    // not rendered since another pane's `/model` shows that model until its
    // next observation; key it by pane if Agy ever writes a per-pane model.
    let written = modified(settings)
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|at| at.as_secs());
    let settings_model = std::fs::read(settings)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| {
            value
                .get("model")?
                .as_str()
                .map(str::trim)
                .map(str::to_string)
        })
        .filter(|model| !model.is_empty())
        .zip(written);
    CollectorState {
        disabled: settings_newer && statusline_disabled(settings),
        settings_model,
    }
}

pub fn apply_at(settings: &Path, state: &Path, executable: &Path) -> Result<()> {
    CONFIG.apply(settings, state, executable)
}

pub fn uninstall_at(settings: &Path, state: &Path) -> Result<()> {
    CONFIG.uninstall(settings, state)
}

/// Keep the provider payload untouched for a user-owned statusLine command,
/// while keying the plugin's private mailbox by the Herdr pane that owns the
/// Agy process. Antigravity's PreInvocation hook can report a spawned
/// subagent conversation while statusLine still describes the parent; the
/// pane id is inherited by both and stays stable across that mismatch.
fn observation_for_cache(value: &Value, pane_id: Option<&str>) -> Value {
    let Some(pane_id) = pane_id.filter(|pane_id| !pane_id.is_empty()) else {
        return value.clone();
    };
    let mut observation = value.clone();
    if let Some(object) = observation.as_object_mut() {
        object.insert("session_id".to_string(), Value::String(pane_id.to_string()));
    }
    observation
}

/// Consume one Agy statusLine payload, cache quota silently, then preserve a
/// user-owned statusLine command when one existed before installation.
pub fn run_statusline_hook() -> Result<()> {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    if let Ok(value) = serde_json::from_slice::<Value>(&input) {
        if let Ok(snapshot) = parse_statusline(&value, CacheStore::now_unix()) {
            if let Ok(cache) = CacheStore::from_env() {
                let pane_id = std::env::var("HERDR_PANE_ID").ok();
                let observation = observation_for_cache(&value, pane_id.as_deref());
                let _ = cache.save_statusline_observation(Provider::Agy, snapshot, &observation);
            }
        }
    }
    let cache = CacheStore::from_env()?;
    let Some(output) = CONFIG.run_previous(cache.root(), &input)? else {
        return Ok(());
    };
    if output.timed_out {
        return Ok(());
    }
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stdout().flush()?;
    if output.exit_code != Some(0) {
        std::process::exit(output.exit_code.unwrap_or(1));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cache_observation_uses_the_herdr_pane_without_mutating_provider_identity() {
        let original = json!({
            "session_id": "parent-conversation",
            "conversation_id": "parent-conversation",
            "model": {"display_name": "Gemini Flash"}
        });
        let cached = observation_for_cache(&original, Some("w1:p7"));

        assert_eq!(
            cached.get("session_id").and_then(Value::as_str),
            Some("w1:p7")
        );
        assert_eq!(
            cached.get("conversation_id").and_then(Value::as_str),
            Some("parent-conversation")
        );
        assert_eq!(
            original.get("session_id").and_then(Value::as_str),
            Some("parent-conversation")
        );
    }

    #[test]
    fn an_observation_newer_than_the_disable_keeps_the_collector_live() {
        use std::time::{Duration, SystemTime};
        let directory = tempfile::tempdir().unwrap();
        let settings = directory.path().join("settings.json");
        let observation = directory.path().join("agy-statusline.observation.json");
        let touch = |path: &Path, contents: &str, at: u64| {
            std::fs::write(path, contents).unwrap();
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(at))
                .unwrap();
        };
        let disabled = r#"{"statusLine":{"command":"x","enabled":false}}"#;
        touch(&settings, disabled, 1_000);
        assert!(state_at(&settings, &observation).disabled, "no observation");
        touch(&observation, "{}", 999);
        assert!(
            state_at(&settings, &observation).disabled,
            "observed before"
        );
        touch(&observation, "{}", 1_001);
        assert!(
            !state_at(&settings, &observation).disabled,
            "observed after"
        );
        touch(&settings, r#"{"statusLine":{"command":"x"}}"#, 2_000);
        assert!(!state_at(&settings, &observation).disabled, "enabled");
    }

    /// Live failure: `/statusline` off, then `/model` Flash→Opus→Argon→Flash.
    /// Every `/model` rewrites `settings.json`, so the pane-keyed cache can
    /// never catch up; the root `model` there is the only current evidence.
    /// The model is read with its write time whatever the shared observation
    /// file says: the snapshot compares it with each pane's own observation.
    #[test]
    fn the_settings_model_comes_with_its_write_time() {
        use std::time::{Duration, SystemTime};
        let directory = tempfile::tempdir().unwrap();
        let settings = directory.path().join("settings.json");
        let observation = directory.path().join("agy-statusline.observation.json");
        let touch = |path: &Path, contents: &str, at: u64| {
            std::fs::write(path, contents).unwrap();
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(at))
                .unwrap();
        };
        let flash =
            r#"{"model":" Gemini 3.8 Flash (High) ","statusLine":{"command":"x","enabled":false}}"#;
        touch(&settings, flash, 1_000);
        touch(&observation, "{}", 999);
        let model = Some(("Gemini 3.8 Flash (High)".to_string(), 1_000));
        assert_eq!(
            state_at(&settings, &observation),
            CollectorState {
                disabled: true,
                settings_model: model.clone(),
            }
        );
        // Another Agy's observation after the write keeps the model: whether
        // it outranks a pane depends on that pane's own observation.
        touch(&observation, "{}", 1_003);
        assert_eq!(
            state_at(&settings, &observation),
            CollectorState {
                disabled: false,
                settings_model: model,
            }
        );
        // No model key gives nothing to fall back on.
        touch(
            &settings,
            r#"{"model":"","statusLine":{"command":"x","enabled":false}}"#,
            1_004,
        );
        assert_eq!(
            state_at(&settings, &observation),
            CollectorState {
                disabled: true,
                settings_model: None,
            }
        );
    }

    #[test]
    fn cache_observation_keeps_native_session_outside_herdr() {
        let original = json!({
            "session_id": "parent-conversation",
            "conversation_id": "parent-conversation"
        });
        assert_eq!(observation_for_cache(&original, None), original);
        assert_eq!(observation_for_cache(&original, Some("")), original);
    }
}
