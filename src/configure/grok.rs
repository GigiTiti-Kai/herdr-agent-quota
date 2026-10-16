use crate::cache::CacheStore;
use crate::identity::{self, PLUGIN_ID};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub fn check() -> Result<()> {
    let mut found = false;
    for path in hook_paths()? {
        if is_managed_hook(&path) {
            println!(
                "Legacy Grok quota hook will be removed; the unified active-turn watcher handles refreshes: {}",
                path.display()
            );
            found = true;
        }
    }
    if !found {
        println!(
            "No Grok response hook is needed; the unified active-turn watcher handles {}",
            hook_path_for(PLUGIN_ID)?.display()
        );
    }
    Ok(())
}

pub fn apply() -> Result<()> {
    let cache = CacheStore::from_env()?;
    let executable = std::env::current_exe().context("resolve plugin executable")?;
    for path in hook_paths()? {
        apply_at(&path, cache.root(), &executable)?;
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    for path in hook_paths()? {
        uninstall_at(&path)?;
    }
    Ok(())
}

pub fn apply_at(path: &Path, _state: &Path, _executable: &Path) -> Result<()> {
    // The unified watcher replaces the old per-tool Grok hook. Only remove a
    // file that this plugin owns; a user's unrelated hook is never touched.
    if is_managed_hook(path) {
        fs::remove_file(path).context("remove legacy Grok quota hook")?;
        println!("Removed legacy Grok quota hook from {}", path.display());
    }
    Ok(())
}

pub fn uninstall_at(path: &Path) -> Result<()> {
    if is_managed_hook(path) {
        fs::remove_file(path).context("remove Grok quota hook")?;
        println!("Removed Grok quota hook from {}", path.display());
    }
    Ok(())
}

fn hooks_dir() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("GROK_HOME") {
        return Ok(PathBuf::from(home).join("hooks"));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".grok/hooks"))
}

fn hook_path_for(id: &str) -> Result<PathBuf> {
    Ok(hooks_dir()?.join(identity::grok_hook_file(id)))
}

fn hook_paths() -> Result<Vec<PathBuf>> {
    identity::all_plugin_ids().map(hook_path_for).collect()
}

fn is_managed_hook(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|contents| {
        identity::all_plugin_ids().any(|id| {
            contents.contains(&identity::grok_legacy_refresh_action(id))
                || (contents.contains(id) && contents.contains("refresh --provider grok"))
        })
    })
}
