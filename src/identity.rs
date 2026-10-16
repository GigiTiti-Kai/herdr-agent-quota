/// Current Herdr plugin id. Must match `id` in `herdr-plugin.toml`.
pub const PLUGIN_ID: &str = "herdr-agent-usage";

/// Earlier ids still treated as this plugin during upgrade and uninstall.
pub const PLUGIN_ID_ALIASES: &[&str] = &["herdr-agent-quota"];

pub fn all_plugin_ids() -> impl Iterator<Item = &'static str> {
    std::iter::once(PLUGIN_ID).chain(PLUGIN_ID_ALIASES.iter().copied())
}

pub fn command_mentions_us(command: &str) -> bool {
    all_plugin_ids().any(|id| command.contains(id))
}

pub fn comment_owns(suffix: &str) -> bool {
    all_plugin_ids().any(|id| suffix.contains(id))
}

pub fn owns_row_comment(suffix: &str) -> bool {
    all_plugin_ids().any(|id| suffix.contains(&format!("{id}-row")))
}

pub fn owns_provider_comment(suffix: &str) -> bool {
    all_plugin_ids().any(|id| suffix.contains(&format!("{id}-provider")))
}

pub fn is_managed_keybinding(command: &str) -> bool {
    all_plugin_ids()
        .any(|id| command == format!("{id}.refresh") || command == format!("{id}.open-settings"))
}

pub fn refresh_action() -> String {
    format!("{PLUGIN_ID}.refresh")
}

pub fn settings_action() -> String {
    format!("{PLUGIN_ID}.open-settings")
}

pub fn row_marker() -> String {
    format!("{PLUGIN_ID}-row")
}

pub fn provider_marker() -> String {
    format!("{PLUGIN_ID}-provider")
}

pub fn font_marker_start(id: &str) -> String {
    format!("# BEGIN {id} font")
}

pub fn font_marker_end(id: &str) -> String {
    format!("# END {id} font")
}

pub fn hooks_script_name(id: &str) -> String {
    format!("{id}-hooks.sh")
}

pub fn managed_by(id: &str) -> String {
    format!("managed by {id}")
}

pub fn grok_hook_file(id: &str) -> String {
    format!("{id}.json")
}

pub fn grok_legacy_refresh_action(id: &str) -> String {
    format!("{id}.refresh-grok")
}

pub fn agent_view_source() -> String {
    format!("plugin:{PLUGIN_ID}")
}

pub fn keychain_approve_command(provider: &str) -> String {
    format!("{PLUGIN_ID} refresh --provider {provider} --keychain-approve")
}

/// Move files left under an alias id into the directories Herdr injected.
///
/// A linked checkout can pick up the new manifest id without `./install.sh`.
/// Herdr then points `HERDR_PLUGIN_STATE_DIR` and `HERDR_PLUGIN_CONFIG_DIR` at
/// empty sibling directories while preferences, quota caches, and the Cursor
/// Keychain marker still sit under `herdr-agent-quota`. Destination names win,
/// so a refresh that already wrote the new account is kept. `*-hooks.sh` stays
/// put: Cursor's `hooks.json` calls that script by its old absolute path until
/// `configure` rewrites the command.
pub fn adopt_alias_plugin_dirs() {
    adopt_env_sibling("HERDR_PLUGIN_STATE_DIR");
    adopt_env_sibling("HERDR_PLUGIN_CONFIG_DIR");
}

fn adopt_env_sibling(variable: &str) {
    let Some(current) = std::env::var_os(variable) else {
        return;
    };
    let current = std::path::PathBuf::from(current);
    if current.file_name().and_then(|name| name.to_str()) != Some(PLUGIN_ID) {
        return;
    }
    let Some(parent) = current.parent() else {
        return;
    };
    for alias in PLUGIN_ID_ALIASES {
        adopt_tree(&parent.join(alias), &current);
    }
}

fn adopt_tree(from: &std::path::Path, to: &std::path::Path) {
    if from == to || !from.is_dir() {
        return;
    }
    if std::fs::create_dir_all(to).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(from) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().ends_with("-hooks.sh") {
            continue;
        }
        let destination = to.join(&name);
        if destination.exists() {
            continue;
        }
        let _ = std::fs::rename(entry.path(), destination);
    }
    let _ = std::fs::remove_dir(from);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_id_is_not_listed_as_an_alias() {
        assert!(!PLUGIN_ID_ALIASES.contains(&PLUGIN_ID));
        assert!(all_plugin_ids().any(|id| id == PLUGIN_ID));
        assert!(PLUGIN_ID_ALIASES.contains(&"herdr-agent-quota"));
    }

    #[test]
    fn alias_comments_and_commands_count_as_ours() {
        assert!(comment_owns(" # herdr-agent-quota"));
        assert!(owns_row_comment(" # herdr-agent-quota-row"));
        assert!(owns_provider_comment(" # herdr-agent-quota-provider"));
        assert!(is_managed_keybinding("herdr-agent-quota.refresh"));
        assert!(is_managed_keybinding("herdr-agent-quota.open-settings"));
        assert!(command_mentions_us(
            "/old/herdr-agent-quota claude-statusline"
        ));
        assert!(!is_managed_keybinding("other.refresh"));
        assert!(!owns_row_comment(" # someone-else-row"));
    }

    #[test]
    fn adopt_moves_alias_siblings_and_leaves_hook_scripts_and_newer_files() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let old_state = state.join("herdr-agent-quota");
        let new_state = state.join(PLUGIN_ID);
        std::fs::create_dir_all(&old_state).unwrap();
        std::fs::create_dir_all(&new_state).unwrap();
        std::fs::write(old_state.join("cursor-keychain-approved"), "ok\n").unwrap();
        std::fs::write(old_state.join("quota-percent"), "remaining\n").unwrap();
        std::fs::write(old_state.join("herdr-agent-quota-hooks.sh"), "old\n").unwrap();
        std::fs::write(old_state.join("grok-cli-billing.json"), "old-account\n").unwrap();
        std::fs::write(new_state.join("grok-cli-billing.json"), "new-account\n").unwrap();
        let config = root.path().join("config");
        let old_config = config.join("herdr-agent-quota");
        let new_config = config.join(PLUGIN_ID);
        std::fs::create_dir_all(&old_config).unwrap();
        std::fs::create_dir_all(&new_config).unwrap();
        std::fs::write(old_config.join("fields"), "all\n").unwrap();

        crate::prefs::testing::with_env(
            &[
                ("HERDR_PLUGIN_STATE_DIR", Some(new_state.as_os_str())),
                ("HERDR_PLUGIN_CONFIG_DIR", Some(new_config.as_os_str())),
            ],
            super::adopt_alias_plugin_dirs,
        );

        assert_eq!(
            std::fs::read_to_string(new_state.join("cursor-keychain-approved")).unwrap(),
            "ok\n"
        );
        assert_eq!(
            std::fs::read_to_string(new_state.join("quota-percent")).unwrap(),
            "remaining\n"
        );
        assert_eq!(
            std::fs::read_to_string(new_state.join("grok-cli-billing.json")).unwrap(),
            "new-account\n"
        );
        assert_eq!(
            std::fs::read_to_string(old_state.join("herdr-agent-quota-hooks.sh")).unwrap(),
            "old\n"
        );
        assert!(!old_state.join("cursor-keychain-approved").exists());
        assert_eq!(
            std::fs::read_to_string(new_config.join("fields")).unwrap(),
            "all\n"
        );
        assert!(!old_config.exists());
    }

    #[test]
    fn current_names_follow_the_plugin_id() {
        assert_eq!(refresh_action(), format!("{PLUGIN_ID}.refresh"));
        assert_eq!(settings_action(), format!("{PLUGIN_ID}.open-settings"));
        assert_eq!(row_marker(), format!("{PLUGIN_ID}-row"));
        assert_eq!(provider_marker(), format!("{PLUGIN_ID}-provider"));
        assert_eq!(agent_view_source(), format!("plugin:{PLUGIN_ID}"));
        assert_eq!(
            keychain_approve_command("cursor"),
            format!("{PLUGIN_ID} refresh --provider cursor --keychain-approve")
        );
    }
}
