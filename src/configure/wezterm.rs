//! Export the sidebar icon size for WezTerm to read.
//!
//! WezTerm's own config owns the size → scale mapping. This plugin writes only
//! the size name, in the one file that config reads:
//! `{"icon_size":"medium"}`, UTF-8 without a BOM. Nothing else in the WezTerm
//! config folder is touched.
//!
//! The file lives on `/mnt/c`, a 9P mount with no rename-over-existing, so the
//! temp-file rename below is **not** atomic: a reader can find no file for a
//! few milliseconds. WezTerm reads it only on a config reload (Ctrl+Shift+R)
//! and treats a missing, empty, or invalid file as `medium`.
//!
//! The file is ours only while it still holds exactly the bytes this plugin
//! last wrote (their sha256 is kept in the plugin state directory). Anything
//! else — a symlink, a hand-edited file, a file with other keys — is left
//! alone and reported, never overwritten or removed.

use crate::cli::IconSize;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// This user's WezTerm config folder as WSL sees it. A host without that
/// folder is unsupported; the folder is never created.
const SIZE_FILE: &str = "/mnt/c/Users/hadas/.config/wezterm/herdr-icon-size.local.json";
/// Points a direct CLI run or a test somewhere else. A Herdr plugin action
/// never sees it, so the installed plugin always uses [`SIZE_FILE`].
const SIZE_FILE_ENV: &str = "HERDR_AGENT_QUOTA_WEZTERM_ICON_SIZE_FILE";
const OWNED_MARKER: &str = "owned-wezterm-icon-size";

/// What follows `Icon size <name> ` when the export worked. The settings pane
/// keys on it to tell success from a failure it must not hide.
pub const SAVED: &str = "saved. Press Ctrl+Shift+R in WezTerm.";

pub fn size_file() -> PathBuf {
    std::env::var_os(SIZE_FILE_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(SIZE_FILE))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Export {
    Saved,
    /// No WezTerm config folder on this host.
    Unsupported,
    /// The file is there but is not ours to replace.
    Refused(&'static str),
    Failed(String),
}

impl Export {
    /// One line, short enough for the settings pane's status row.
    pub fn note(&self, size: IconSize) -> String {
        let size = size.as_str();
        match self {
            Self::Saved => format!("Icon size {size} {SAVED}"),
            Self::Unsupported => {
                format!("Icon size {size} not exported: no WezTerm config folder.")
            }
            Self::Refused(why) => format!("Icon size {size} not exported: {why}."),
            Self::Failed(error) => format!("Icon size {size} not exported: {error}"),
        }
    }
}

enum Current {
    Missing,
    Ours(Vec<u8>),
    Foreign(&'static str),
}

fn current(file: &Path, state_dir: &Path) -> std::io::Result<Current> {
    let metadata = match fs::symlink_metadata(file) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Current::Missing),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        return Ok(Current::Foreign("file is a symlink"));
    }
    if !metadata.is_file() {
        return Ok(Current::Foreign("file is not a regular file"));
    }
    let bytes = fs::read(file)?;
    let marker = fs::read_to_string(state_dir.join(OWNED_MARKER)).ok();
    if marker.as_deref() == Some(digest(&bytes).as_str()) {
        Ok(Current::Ours(bytes))
    } else {
        Ok(Current::Foreign("file not made by this plugin"))
    }
}

pub fn export(size: IconSize, file: &Path, state_dir: &Path) -> Export {
    let Some(folder) = file.parent().filter(|folder| folder.is_dir()) else {
        return Export::Unsupported;
    };
    let body = format!("{{\"icon_size\":\"{}\"}}\n", size.as_str());
    match current(file, state_dir) {
        Err(error) => Export::Failed(error.to_string()),
        Ok(Current::Foreign(why)) => Export::Refused(why),
        // Rewriting identical bytes would only open the 9P gap for nothing.
        Ok(Current::Ours(bytes)) if bytes == body.as_bytes() => Export::Saved,
        Ok(_) => match replace(folder, file, state_dir, &body) {
            Ok(()) => Export::Saved,
            Err(error) => Export::Failed(format!("{error:#}")),
        },
    }
}

fn replace(folder: &Path, file: &Path, state_dir: &Path, body: &str) -> Result<()> {
    let temp = folder.join(format!(".herdr-icon-size.{}.tmp", std::process::id()));
    fs::write(&temp, body).context("write temp file")?;
    if let Err(error) = fs::rename(&temp, file) {
        let _ = fs::remove_file(&temp);
        return Err(error).context("rename into place");
    }
    fs::write(state_dir.join(OWNED_MARKER), digest(body.as_bytes())).context("record ownership")
}

/// Remove the size file only if it is still ours; WezTerm then reads the
/// missing file as `medium`. A file someone else left there stays.
pub fn uninstall(file: &Path, state_dir: &Path) -> Result<()> {
    if let Current::Ours(_) = current(file, state_dir).context("inspect WezTerm icon size file")? {
        fs::remove_file(file).context("remove WezTerm icon size file")?;
    }
    match fs::remove_file(state_dir.join(OWNED_MARKER)) {
        Err(error) if error.kind() != ErrorKind::NotFound => {
            Err(error).context("remove WezTerm icon size ownership marker")
        }
        _ => Ok(()),
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Fixture {
        _root: tempfile::TempDir,
        folder: PathBuf,
        state: PathBuf,
        file: PathBuf,
    }

    /// A WezTerm folder that already holds the user's own config.
    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("wezterm");
        let state = root.path().join("state");
        fs::create_dir_all(&folder).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::write(folder.join("wezterm.lua"), "return {}\n").unwrap();
        let file = folder.join("herdr-icon-size.local.json");
        Fixture {
            _root: root,
            folder,
            state,
            file,
        }
    }

    fn names(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_export_writes_only_the_size_name_and_leaves_no_temp_file() {
        let f = fixture();
        assert_eq!(export(IconSize::Large, &f.file, &f.state), Export::Saved);
        let bytes = fs::read(&f.file).unwrap();
        assert_eq!(bytes, b"{\"icon_size\":\"large\"}\n");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed, serde_json::json!({"icon_size": "large"}));
        assert_eq!(
            names(&f.folder),
            ["herdr-icon-size.local.json", "wezterm.lua"]
        );
        assert_eq!(
            fs::read_to_string(f.folder.join("wezterm.lua")).unwrap(),
            "return {}\n"
        );

        // Ours, so a new size replaces it.
        assert_eq!(export(IconSize::Small, &f.file, &f.state), Export::Saved);
        assert_eq!(fs::read(&f.file).unwrap(), b"{\"icon_size\":\"small\"}\n");
    }

    /// No WezTerm folder means an unsupported host, and the folder is not
    /// made up to pretend otherwise.
    #[test]
    fn a_host_without_the_wezterm_folder_is_unsupported() {
        let f = fixture();
        let file = f.folder.join("missing").join("herdr-icon-size.local.json");
        let outcome = export(IconSize::Large, &file, &f.state);
        assert_eq!(outcome, Export::Unsupported);
        assert!(!file.parent().unwrap().exists());
        assert!(!f.state.join(OWNED_MARKER).exists());
        assert!(outcome.note(IconSize::Large).contains("not exported"));
    }

    #[test]
    fn a_file_this_plugin_did_not_write_is_never_replaced_or_removed() {
        let f = fixture();
        // Same schema, written by hand; and one with a key WezTerm ignores.
        for foreign in [
            "{\"icon_size\":\"small\"}\n",
            "{\"icon_size\":\"small\",\"note\":\"mine\"}\n",
        ] {
            fs::write(&f.file, foreign).unwrap();
            let outcome = export(IconSize::Large, &f.file, &f.state);
            assert_eq!(outcome, Export::Refused("file not made by this plugin"));
            assert_eq!(fs::read_to_string(&f.file).unwrap(), foreign);
            uninstall(&f.file, &f.state).unwrap();
            assert_eq!(fs::read_to_string(&f.file).unwrap(), foreign);
        }

        // Ours once, then edited by hand: no longer ours.
        fs::remove_file(&f.file).unwrap();
        assert_eq!(export(IconSize::Large, &f.file, &f.state), Export::Saved);
        fs::write(&f.file, "{\"icon_size\":\"medium\"}\n").unwrap();
        assert!(matches!(
            export(IconSize::Small, &f.file, &f.state),
            Export::Refused(_)
        ));
        assert_eq!(
            fs::read_to_string(&f.file).unwrap(),
            "{\"icon_size\":\"medium\"}\n"
        );
    }

    #[test]
    fn a_symlink_is_refused_and_its_target_untouched() {
        let f = fixture();
        let target = f.folder.join("elsewhere.json");
        fs::write(&target, "{\"icon_size\":\"small\"}\n").unwrap();
        std::os::unix::fs::symlink(&target, &f.file).unwrap();
        assert_eq!(
            export(IconSize::Large, &f.file, &f.state),
            Export::Refused("file is a symlink")
        );
        uninstall(&f.file, &f.state).unwrap();
        assert!(fs::symlink_metadata(&f.file)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "{\"icon_size\":\"small\"}\n"
        );
    }

    /// A folder that refuses writes: the old file stays as it was, nothing
    /// is claimed, and the note carries the error instead of "saved".
    #[test]
    fn a_failed_write_keeps_the_previous_file_and_says_so() {
        let f = fixture();
        assert_eq!(export(IconSize::Large, &f.file, &f.state), Export::Saved);
        let marker = fs::read_to_string(f.state.join(OWNED_MARKER)).unwrap();
        fs::set_permissions(&f.folder, fs::Permissions::from_mode(0o500)).unwrap();
        let outcome = export(IconSize::Small, &f.file, &f.state);
        fs::set_permissions(&f.folder, fs::Permissions::from_mode(0o700)).unwrap();

        let Export::Failed(error) = &outcome else {
            panic!("{outcome:?}");
        };
        assert!(error.contains("write temp file"), "{error}");
        let note = outcome.note(IconSize::Small);
        assert!(note.starts_with("Icon size small not exported:"), "{note}");
        assert!(!note.contains(SAVED), "{note}");
        assert_eq!(fs::read(&f.file).unwrap(), b"{\"icon_size\":\"large\"}\n");
        assert_eq!(
            fs::read_to_string(f.state.join(OWNED_MARKER)).unwrap(),
            marker
        );
        assert_eq!(
            names(&f.folder),
            ["herdr-icon-size.local.json", "wezterm.lua"]
        );
    }

    /// Uninstall removes our file, so WezTerm falls back to medium, and
    /// nothing else in the folder.
    #[test]
    fn uninstall_removes_only_our_file_and_its_marker() {
        let f = fixture();
        assert_eq!(export(IconSize::Large, &f.file, &f.state), Export::Saved);
        uninstall(&f.file, &f.state).unwrap();
        assert!(!f.file.exists());
        assert!(!f.state.join(OWNED_MARKER).exists());
        assert_eq!(names(&f.folder), ["wezterm.lua"]);
        // Nothing left to do is not an error.
        uninstall(&f.file, &f.state).unwrap();
    }

    #[test]
    fn every_note_fits_the_settings_status_row() {
        let outcomes = [
            Export::Saved,
            Export::Unsupported,
            Export::Refused("file is not a regular file"),
            Export::Refused("file not made by this plugin"),
        ];
        for outcome in outcomes {
            for size in IconSize::CHOICES {
                let note = outcome.note(size);
                assert!(note.chars().count() <= 66, "too long: {note}");
            }
        }
    }
}
