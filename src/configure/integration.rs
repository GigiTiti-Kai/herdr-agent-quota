use crate::cli::AgentSelection;
use crate::model::Harness;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Herdr's integration id for a harness, when it has one.
///
/// Agy quota comes from the statusLine hook, not Herdr's session id. Herdr
/// ships `antigravity-cli` for resume, but its PreInvocation id can be a
/// spawned subagent conversation while statusLine describes the parent. The
/// quota plugin therefore keeps statusLine evidence keyed by its own
/// conversation id and only bridges an id mismatch when that attribution is
/// unambiguous. Muse quota is account-level, so a Muse pane needs no session
/// id to be attributed.
fn integration_id(harness: Harness) -> Option<&'static str> {
    match harness {
        Harness::Claude => Some("claude"),
        Harness::Codex => Some("codex"),
        Harness::Grok => Some("grok"),
        Harness::Agy | Harness::Muse => None,
        Harness::OpenCode => Some("opencode"),
        Harness::Pi => Some("pi"),
        Harness::Omp => Some("omp"),
        Harness::Devin => Some("devin"),
        Harness::Cursor => Some("cursor"),
    }
}

pub fn ensure_integrations(agents: &AgentSelection, missing_omp_is_error: bool) -> Result<()> {
    for harness in AgentSelection::SUPPORTED {
        if !agents.includes(harness) {
            continue;
        }
        let Some(integration) = integration_id(harness) else {
            continue;
        };
        match Command::new("herdr")
            .args(["integration", "install", integration])
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(_) if harness == Harness::Omp && !missing_omp_is_error => {
                eprintln!("warning: omp integration was not installed; continuing");
            }
            Ok(_) if harness == Harness::Omp => bail!(
                "failed to install Herdr's omp integration. Install/repair omp, then rerun configure"
            ),
            Ok(_) => bail!("failed to install Herdr's {integration} integration"),
            Err(error) if harness == Harness::Omp && !missing_omp_is_error => {
                eprintln!("warning: could not run herdr integration install omp: {error}");
            }
            Err(error) if harness == Harness::Omp => {
                return Err(error).context(
                    "run herdr integration install omp. Install/repair omp, then rerun configure",
                );
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("run herdr integration install {integration}")
                });
            }
        }
    }
    Ok(())
}

pub fn integration_state() -> Result<BTreeMap<String, bool>> {
    let output = Command::new("herdr")
        .args(["integration", "status", "--json"])
        .output()
        .context("run herdr integration status --json")?;
    if !output.status.success() {
        bail!("herdr integration status --json failed");
    }
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parse integration status JSON")?;
    let mut state = BTreeMap::new();
    let Some(integrations) = value.get("integrations").and_then(|value| value.as_array()) else {
        return Ok(state);
    };
    for integration in integrations {
        let Some(id) = integration.get("id").and_then(|value| value.as_str()) else {
            continue;
        };
        let installed = integration
            .get("installed")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        state.insert(id.to_string(), installed);
    }
    Ok(state)
}

pub fn locate_integration_marker(integration: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let candidates = [
        home.join(".config/herdr/integrations").join(integration),
        home.join(".local/share/herdr/integrations")
            .join(integration),
    ];
    candidates.into_iter().find(|path| path.exists())
}

pub fn remove_integration_marker(integration: &str) -> Result<()> {
    let Some(path) = locate_integration_marker(integration) else {
        return Ok(());
    };
    if path.is_dir() {
        fs::remove_dir_all(&path)
            .with_context(|| format!("remove integration marker {}", path.display()))?;
    } else {
        fs::remove_file(&path)
            .with_context(|| format!("remove integration marker {}", path.display()))?;
    }
    Ok(())
}

pub fn path_exists(path: impl AsRef<Path>) -> bool {
    path.as_ref().exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harnesses_map_to_expected_integrations() {
        assert_eq!(integration_id(Harness::Claude), Some("claude"));
        assert_eq!(integration_id(Harness::Codex), Some("codex"));
        assert_eq!(integration_id(Harness::Grok), Some("grok"));
        assert_eq!(integration_id(Harness::Agy), None);
        assert_eq!(integration_id(Harness::OpenCode), Some("opencode"));
        assert_eq!(integration_id(Harness::Pi), Some("pi"));
        assert_eq!(integration_id(Harness::Omp), Some("omp"));
        assert_eq!(integration_id(Harness::Devin), Some("devin"));
        assert_eq!(integration_id(Harness::Muse), None);
        assert_eq!(integration_id(Harness::Cursor), Some("cursor"));
    }
}
