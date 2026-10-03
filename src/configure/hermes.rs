//! Install the bridge plugin into Hermes Agent's default profile.
//!
//! Only a live Hermes session knows which credential serves it, so the quota
//! of a Hermes pane has to come from inside Hermes. This writes a small
//! Hermes plugin (`assets/hermes/herdr-agent-quota/`) into
//! `<hermes home>/plugins/herdr-agent-quota/`, tells it where this plugin's
//! state directory and executable are (`bridge.json`), and adds it to
//! Hermes's plugin allow-list through Hermes's own CLI.
//!
//! It is a separate plugin from Herdr's `herdr-agent-state`, which only
//! reports session identity and is replaced by Herdr's own updates. Hermes
//! core, its credentials, and its other settings are not touched.

use crate::cache::CacheStore;
use crate::providers::hermes::MAILBOX_DIR;
use anyhow::{Context, Result};
use serde_json::json;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PLUGIN_NAME: &str = "herdr-agent-quota";
const MARKER: &str = "managed by herdr-agent-quota";
const INIT_PY: &str = include_str!("../../assets/hermes/herdr-agent-quota/__init__.py");
const MANIFEST: &str = include_str!("../../assets/hermes/herdr-agent-quota/plugin.yaml");
/// Written once Hermes accepted the plugin, so a later repair does not undo a
/// user who has since disabled it in Hermes.
const ENABLED_MARKER: &str = ".enabled";
const MANAGED_FILES: [&str; 4] = ["__init__.py", "plugin.yaml", "bridge.json", ENABLED_MARKER];

pub fn check() -> Result<()> {
    match default_home() {
        Some(home) if installed(&plugin_dir(&home)) => println!(
            "Hermes quota bridge is installed: {}",
            plugin_dir(&home).display()
        ),
        Some(home) => println!(
            "Hermes quota bridge is not installed: {}",
            plugin_dir(&home).display()
        ),
        None => println!("Hermes quota bridge is not installed: no default Hermes profile."),
    }
    Ok(())
}

pub fn apply() -> Result<()> {
    let Some(home) = default_home() else {
        println!("Skipped the Hermes quota bridge: no default Hermes profile.");
        return Ok(());
    };
    let cache = CacheStore::from_env()?;
    let executable = std::env::current_exe().context("resolve plugin executable")?;
    apply_at(&home, cache.root(), &executable, &hermes_bin())
}

pub fn uninstall() -> Result<()> {
    let cache = CacheStore::from_env()?;
    match default_home() {
        Some(home) => uninstall_at(&home, cache.root(), &hermes_bin()),
        None => remove_mailbox(cache.root()),
    }
}

fn hermes_bin() -> OsString {
    std::env::var_os("HERDR_AGENT_QUOTA_HERMES_BIN").unwrap_or_else(|| "hermes".into())
}

/// The default profile's home, and nothing else.
///
/// A named profile (`<root>/profiles/<name>`) has its own credentials and its
/// own session store. This plugin reads the default profile's, and the bridge
/// refuses to run anywhere else, so installing it there would only be noise.
fn default_home() -> Option<PathBuf> {
    default_profile_home(crate::hermes::home_from_env()?)
}

fn default_profile_home(home: PathBuf) -> Option<PathBuf> {
    let named_profile = home
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|parent| parent == "profiles");
    (home.is_dir() && !named_profile).then_some(home)
}

fn plugin_dir(home: &Path) -> PathBuf {
    home.join("plugins").join(PLUGIN_NAME)
}

fn installed(directory: &Path) -> bool {
    fs::read_to_string(directory.join("__init__.py")).is_ok_and(|text| text.contains(MARKER))
}

pub fn apply_at(home: &Path, state: &Path, executable: &Path, hermes: &OsString) -> Result<()> {
    let directory = plugin_dir(home);
    if directory.join("__init__.py").exists() && !installed(&directory) {
        println!(
            "Preserved user-owned Hermes plugin at {}; the quota bridge was not installed.",
            directory.display()
        );
        return Ok(());
    }
    let (Some(state_text), Some(executable_text)) = (state.to_str(), executable.to_str()) else {
        anyhow::bail!("the plugin state directory and executable must be UTF-8 paths");
    };
    fs::create_dir_all(&directory).context("create Hermes quota bridge plugin directory")?;
    write_if_changed(&directory.join("__init__.py"), INIT_PY.as_bytes(), 0o644)?;
    write_if_changed(&directory.join("plugin.yaml"), MANIFEST.as_bytes(), 0o644)?;
    let bridge = json!({"state_dir": state_text, "executable": executable_text});
    write_if_changed(
        &directory.join("bridge.json"),
        &serde_json::to_vec_pretty(&bridge)?,
        0o600,
    )?;
    // The bridge writes here and nobody else may read it.
    let mailbox = state.join(MAILBOX_DIR);
    fs::create_dir_all(&mailbox).context("create Hermes bridge mailbox")?;
    set_mode(&mailbox, 0o700)?;

    let marker = directory.join(ENABLED_MARKER);
    if marker.exists() {
        return Ok(());
    }
    if run_hermes(hermes, home, &["plugins", "enable", PLUGIN_NAME]) {
        fs::write(&marker, b"").context("record Hermes quota bridge activation")?;
        println!(
            "Installed the Hermes quota bridge. Hermes sessions started from now on report their quota."
        );
    } else {
        println!(
            "Hermes quota bridge files are in {}; enable them with `hermes plugins enable {PLUGIN_NAME}`.",
            directory.display()
        );
    }
    Ok(())
}

pub fn uninstall_at(home: &Path, state: &Path, hermes: &OsString) -> Result<()> {
    remove_mailbox(state)?;
    let directory = plugin_dir(home);
    if !installed(&directory) {
        return Ok(());
    }
    // Never `hermes plugins remove`: it deletes the whole directory, including
    // anything the user put beside our files. `disable` only edits Hermes's
    // allow-list, and has to run while the manifest is still there to resolve.
    if !run_hermes(hermes, home, &["plugins", "disable", PLUGIN_NAME]) {
        println!(
            "Hermes was not reachable; `{PLUGIN_NAME}` may still be listed under plugins.enabled in its config.yaml."
        );
    }
    for name in MANAGED_FILES {
        let _ = fs::remove_file(directory.join(name));
    }
    let _ = fs::remove_dir_all(directory.join("__pycache__"));
    // Not recursive: anything the user added beside our files stays.
    let _ = fs::remove_dir(&directory);
    Ok(())
}

fn remove_mailbox(state: &Path) -> Result<()> {
    match fs::remove_dir_all(state.join(MAILBOX_DIR)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).context("remove Hermes bridge mailbox")
        }
        _ => Ok(()),
    }
}

/// Run one Hermes CLI command against exactly this home. `false` when the CLI
/// is missing or refused; the caller reports what to do by hand.
fn run_hermes(hermes: &OsString, home: &Path, arguments: &[&str]) -> bool {
    Command::new(hermes)
        .args(arguments)
        .env("HERMES_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn write_if_changed(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    if fs::read(path).is_ok_and(|current| current == contents) {
        return set_mode(path, mode);
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temporary, contents).with_context(|| format!("write {}", temporary.display()))?;
    set_mode(&temporary, mode)?;
    fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .with_context(|| format!("chmod {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            fs::create_dir_all(root.path().join("hermes")).unwrap();
            fs::create_dir_all(root.path().join("state")).unwrap();
            Self { root }
        }

        fn home(&self) -> PathBuf {
            self.root.path().join("hermes")
        }

        fn state(&self) -> PathBuf {
            self.root.path().join("state")
        }

        fn plugin(&self) -> PathBuf {
            plugin_dir(&self.home())
        }

        /// A stand-in for the Hermes CLI that records how it was called. Like
        /// the real one, its `plugins remove` deletes the whole plugin
        /// directory, whatever is in it.
        fn hermes(&self, exit: i32) -> OsString {
            let stub = self.root.path().join(format!("hermes-stub-{exit}"));
            fs::write(
                &stub,
                format!(
                    "#!/bin/sh\nprintf '%s|%s\\n' \"$*\" \"$HERMES_HOME\" >> {}\n\
                     if [ \"$1 $2\" = \"plugins remove\" ]; then rm -rf \"$HERMES_HOME/plugins/$3\"; fi\n\
                     exit {exit}\n",
                    self.log().display()
                ),
            )
            .unwrap();
            fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
            stub.into_os_string()
        }

        fn log(&self) -> PathBuf {
            self.root.path().join("hermes.log")
        }

        fn calls(&self) -> Vec<String> {
            fs::read_to_string(self.log())
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        fn apply(&self, hermes: &OsString) {
            apply_at(
                &self.home(),
                &self.state(),
                Path::new("/opt/quota/herdr-agent-quota"),
                hermes,
            )
            .unwrap();
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn apply_installs_the_plugin_and_enables_it_once_through_hermes() {
        let fixture = Fixture::new();
        let hermes = fixture.hermes(0);
        fixture.apply(&hermes);
        let plugin = fixture.plugin();
        assert_eq!(
            fs::read_to_string(plugin.join("__init__.py")).unwrap(),
            INIT_PY
        );
        assert_eq!(
            fs::read_to_string(plugin.join("plugin.yaml")).unwrap(),
            MANIFEST
        );
        let bridge: serde_json::Value =
            serde_json::from_slice(&fs::read(plugin.join("bridge.json")).unwrap()).unwrap();
        assert_eq!(
            bridge,
            json!({
                "state_dir": fixture.state().to_str().unwrap(),
                "executable": "/opt/quota/herdr-agent-quota",
            })
        );
        // What the plugin itself checks before trusting these files.
        assert_eq!(mode(&plugin.join("bridge.json")), 0o600);
        assert_eq!(mode(&fixture.state().join(MAILBOX_DIR)), 0o700);
        let enable = format!(
            "plugins enable herdr-agent-quota|{}",
            fixture.home().display()
        );
        assert_eq!(fixture.calls(), vec![enable.clone()]);

        // A repair rewrites nothing and does not re-enable a plugin the user
        // may have disabled in Hermes since.
        fixture.apply(&hermes);
        assert_eq!(fixture.calls(), vec![enable]);
    }

    #[test]
    fn a_failed_or_missing_hermes_cli_leaves_the_files_and_retries_later() {
        let fixture = Fixture::new();
        fixture.apply(&fixture.hermes(1));
        assert!(installed(&fixture.plugin()));
        assert!(!fixture.plugin().join(ENABLED_MARKER).exists());
        fixture.apply(&OsString::from(fixture.root.path().join("absent")));
        assert!(!fixture.plugin().join(ENABLED_MARKER).exists());
        fixture.apply(&fixture.hermes(0));
        assert!(fixture.plugin().join(ENABLED_MARKER).exists());
        assert_eq!(fixture.calls().len(), 2);
    }

    #[test]
    fn a_plugin_directory_this_did_not_write_is_left_alone() {
        let fixture = Fixture::new();
        let plugin = fixture.plugin();
        fs::create_dir_all(&plugin).unwrap();
        fs::write(plugin.join("__init__.py"), "def register(ctx):\n    pass\n").unwrap();
        let hermes = fixture.hermes(0);
        fixture.apply(&hermes);
        uninstall_at(&fixture.home(), &fixture.state(), &hermes).unwrap();
        assert_eq!(
            fs::read_to_string(plugin.join("__init__.py")).unwrap(),
            "def register(ctx):\n    pass\n"
        );
        assert!(!plugin.join("bridge.json").exists());
        assert!(fixture.calls().is_empty());
    }

    /// What the user put beside our files survives, whether or not the
    /// Hermes CLI can be reached.
    #[test]
    fn uninstall_disables_the_plugin_and_removes_only_what_it_installed() {
        for exit in [0, 1] {
            let fixture = Fixture::new();
            fixture.apply(&fixture.hermes(0));
            let mailbox = fixture.state().join(MAILBOX_DIR);
            fs::write(mailbox.join("s1.json"), "{}").unwrap();
            let plugin = fixture.plugin();
            fs::create_dir_all(plugin.join("__pycache__")).unwrap();
            fs::write(plugin.join("notes.txt"), "mine").unwrap();
            fs::create_dir_all(plugin.join("mine")).unwrap();
            fs::write(plugin.join("mine/keep.txt"), "also mine").unwrap();

            let hermes = fixture.hermes(exit);
            uninstall_at(&fixture.home(), &fixture.state(), &hermes).unwrap();
            assert!(!mailbox.exists());
            let calls = fixture.calls();
            assert_eq!(
                calls.last().unwrap(),
                &format!(
                    "plugins disable herdr-agent-quota|{}",
                    fixture.home().display()
                )
            );
            assert!(calls.iter().all(|call| !call.contains("remove")));
            let mut left: Vec<_> = fs::read_dir(&plugin)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            left.sort();
            assert_eq!(left, ["mine", "notes.txt"].map(OsString::from));
            assert_eq!(
                fs::read_to_string(plugin.join("mine/keep.txt")).unwrap(),
                "also mine"
            );
            // Uninstalling twice is quiet and asks Hermes nothing more.
            uninstall_at(&fixture.home(), &fixture.state(), &hermes).unwrap();
            assert_eq!(fixture.calls().len(), calls.len());
        }
    }

    #[test]
    fn uninstall_leaves_no_empty_plugin_directory_behind() {
        let fixture = Fixture::new();
        let hermes = fixture.hermes(0);
        fixture.apply(&hermes);
        uninstall_at(&fixture.home(), &fixture.state(), &hermes).unwrap();
        assert!(!fixture.plugin().exists());
    }

    #[test]
    fn only_an_existing_default_profile_home_is_installed_into() {
        let fixture = Fixture::new();
        assert_eq!(default_profile_home(fixture.home()), Some(fixture.home()));
        assert_eq!(
            default_profile_home(fixture.root.path().join("absent")),
            None
        );
        let named = fixture.home().join("profiles/work");
        fs::create_dir_all(&named).unwrap();
        assert_eq!(default_profile_home(named), None);
    }

    /// The embedded plugin is what the Python tests exercise, byte for byte.
    #[test]
    fn the_embedded_plugin_carries_its_marker_and_no_bridge_config() {
        assert!(INIT_PY.contains(MARKER));
        assert!(MANIFEST.contains("name: herdr-agent-quota"));
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/hermes/herdr-agent-quota");
        assert!(!assets.join("bridge.json").exists());
    }
}
