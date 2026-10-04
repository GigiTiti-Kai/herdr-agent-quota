//! `configure` end to end against a fixture WezTerm folder: the writer runs,
//! the JSON reads back, and every outcome lands in the report.
//!
//! The environment is cleared so nothing reaches the real Herdr server,
//! socket, config, or the real WezTerm folder.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for directory in ["home", "state", "prefs", "wezterm"] {
            fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        fs::write(root.path().join("wezterm/wezterm.lua"), "return {}\n").unwrap();
        Self { root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    fn size_file(&self) -> PathBuf {
        self.path("wezterm/herdr-icon-size.local.json")
    }

    fn pref(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.path("prefs").join(name))
            .ok()
            .map(|value| value.trim().to_string())
    }

    fn configure(&self, size_file: &Path, arguments: &[&str]) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_herdr-agent-quota"))
            .arg("configure")
            .args(arguments)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("home/.config"))
            .env("XDG_DATA_HOME", self.path("home/.local/share"))
            .env("HERDR_PLUGIN_STATE_DIR", self.path("state"))
            .env("HERDR_PLUGIN_CONFIG_DIR", self.path("prefs"))
            .env("HERDR_CONFIG_FILE", self.path("home/herdr.toml"))
            .env("HERDR_BIN_PATH", self.path("no-herdr"))
            .env("HERDR_AGENT_QUOTA_WEZTERM_ICON_SIZE_FILE", size_file)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            output.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        stdout
    }
}

fn json(path: &Path) -> serde_json::Value {
    let bytes = fs::read(path).unwrap();
    assert_ne!(bytes.first(), Some(&0xEF), "no BOM");
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn configure_exports_the_size_name_and_reports_every_outcome() {
    let f = Fixture::new();
    let file = f.size_file();
    for (name, value) in [
        ("agents", "only,codex\n"),
        ("agent-order", "default\n"),
        ("row-gap", "0\n"),
    ] {
        fs::write(f.path("prefs").join(name), value).unwrap();
    }

    let report = f.configure(
        &file,
        &["--apply", "--agent", "codex", "--icon-size", "large"],
    );
    assert!(
        report.contains("Icon size large saved. Press Ctrl+Shift+R in WezTerm."),
        "{report}"
    );
    assert_eq!(json(&file), serde_json::json!({"icon_size": "large"}));
    assert_eq!(f.pref("icon-size").as_deref(), Some("large"));
    // Preferences the user set for other options survive the run.
    assert_eq!(f.pref("agents").as_deref(), Some("only,codex"));
    assert_eq!(f.pref("agent-order").as_deref(), Some("default"));
    assert_eq!(f.pref("row-gap").as_deref(), Some("0"));

    // No flag: the stored size is exported again, unchanged.
    let report = f.configure(&file, &["--apply", "--agent", "codex"]);
    assert!(report.contains("Icon size large saved."), "{report}");
    assert_eq!(json(&file), serde_json::json!({"icon_size": "large"}));

    // A write the folder refuses: the preference moves, the file does not,
    // and the report says the export failed.
    let folder = f.path("wezterm");
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o500)).unwrap();
    let report = f.configure(
        &file,
        &["--apply", "--agent", "codex", "--icon-size", "small"],
    );
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(report.contains("Icon size small not exported:"), "{report}");
    assert!(!report.contains("Icon size small saved"), "{report}");
    assert_eq!(f.pref("icon-size").as_deref(), Some("small"));
    assert_eq!(json(&file), serde_json::json!({"icon_size": "large"}));

    // The next apply retries from the preference.
    let report = f.configure(&file, &["--apply", "--agent", "codex"]);
    assert!(report.contains("Icon size small saved."), "{report}");
    assert_eq!(json(&file), serde_json::json!({"icon_size": "small"}));

    // No WezTerm folder: unsupported, and the folder is not created.
    let elsewhere = f.path("no-wezterm/herdr-icon-size.local.json");
    let report = f.configure(&elsewhere, &["--apply", "--agent", "codex"]);
    assert!(
        report.contains("Icon size small not exported: no WezTerm config folder."),
        "{report}"
    );
    assert!(!f.path("no-wezterm").exists());
}

#[test]
fn a_full_uninstall_removes_only_the_size_file_it_wrote() {
    let f = Fixture::new();
    let file = f.size_file();
    f.configure(
        &file,
        &["--apply", "--agent", "codex", "--icon-size", "large"],
    );
    assert!(file.exists());

    f.configure(&file, &["--uninstall", "--agent", "all"]);
    assert!(!file.exists(), "WezTerm reads a missing file as medium");
    assert_eq!(
        fs::read_to_string(f.path("wezterm/wezterm.lua")).unwrap(),
        "return {}\n"
    );
    assert_eq!(f.pref("icon-size"), None);

    // A file the plugin did not write survives an uninstall.
    fs::write(&file, "{\"icon_size\":\"large\"}\n").unwrap();
    f.configure(&file, &["--uninstall", "--agent", "all"]);
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "{\"icon_size\":\"large\"}\n"
    );
}
