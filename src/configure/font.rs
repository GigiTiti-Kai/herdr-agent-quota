//! Install the vendor icon font and map its codepoints in known terminals.
//!
//! Herdr draws the Agents sidebar inside the host terminal. Private Use Area
//! glyphs only render when that terminal loads `Herdr Agent Icons Max` for
//! `U+E1A0`–`U+E1B6`. Without the map, the cells are tofu — so configure
//! always tries to install both.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;

const FONT_FAMILY: &str = "Herdr Agent Icons Max";
const FONT_BASENAME: &str = "HerdrAgentIconsMax";
const FONT_BYTES: &[u8] = include_bytes!("../../assets/fonts/HerdrAgentIconsMax-Regular.ttf");
/// Same ranges herdr-radar maps: vendor logos, then state marks kept so a
/// shared Ghostty/kitty map stays compatible if both plugins are linked.
const CODEPOINT_RANGES: [(&str, &str); 2] = [("E1A0", "E1B6"), ("E1C0", "E1C5")];
const MARKER_START: &str = "# BEGIN herdr-agent-quota font";
const MARKER_END: &str = "# END herdr-agent-quota font";

/// Copy the font into the user font directory and write Ghostty / kitty maps
/// when those configs already exist. Never creates a terminal config from
/// scratch.
pub fn install() -> Result<Vec<String>> {
    let mut notes = install_font()?;
    let mapped = configure_terminals()?;
    if mapped.is_empty() {
        notes.push(
            "font: no Ghostty/kitty config found; map U+E1A0-U+E1B6 to \"Herdr Agent Icons Max\" \
             in your terminal if icons show as boxes"
                .into(),
        );
    } else {
        notes.extend(mapped);
    }
    Ok(notes)
}

fn install_font() -> Result<Vec<String>> {
    let mut notes = Vec::new();
    let dir = user_font_dir();
    fs::create_dir_all(&dir).with_context(|| format!("create font dir {}", dir.display()))?;
    let hash = font_hash();
    let target = dir.join(format!("{FONT_BASENAME}-{hash}.ttf"));
    if target.exists() {
        notes.push(format!("font: already installed ({})", target.display()));
    } else {
        fs::write(&target, FONT_BYTES)
            .with_context(|| format!("write font {}", target.display()))?;
        notes.push(format!("font: installed {}", target.display()));
    }
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if path == target {
                continue;
            }
            if name.starts_with(FONT_BASENAME) && name.ends_with(".ttf") {
                match fs::remove_file(&path) {
                    Ok(()) => notes.push(format!("font: removed old {name}")),
                    Err(_) => notes.push(format!(
                        "font: {name} is in use; restart the terminal and re-run configure"
                    )),
                }
            }
        }
    }
    Ok(notes)
}

fn configure_terminals() -> Result<Vec<String>> {
    let mut notes = Vec::new();
    for target in terminal_targets() {
        if !target.path.exists() {
            continue;
        }
        let original = fs::read_to_string(&target.path)
            .with_context(|| format!("read {} config {}", target.name, target.path.display()))?;
        let body = marked_block(target.lines);
        let updated = upsert_marked(&original, &body);
        if updated == original {
            notes.push(format!(
                "{}: codepoint map already in {}",
                target.name,
                target.path.display()
            ));
            continue;
        }
        fs::write(&target.path, updated)
            .with_context(|| format!("write {} config {}", target.name, target.path.display()))?;
        notes.push(format!(
            "{}: codepoint map written to {} — {}",
            target.name,
            target.path.display(),
            target.reload
        ));
    }
    Ok(notes)
}

struct TerminalTarget {
    name: &'static str,
    path: PathBuf,
    lines: Vec<String>,
    reload: &'static str,
}

fn terminal_targets() -> Vec<TerminalTarget> {
    let home = directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let ghostty_lines = CODEPOINT_RANGES
        .into_iter()
        .map(|(start, end)| format!("font-codepoint-map = U+{start}-U+{end}=\"{FONT_FAMILY}\""))
        .collect::<Vec<_>>();
    let kitty_lines = CODEPOINT_RANGES
        .into_iter()
        .map(|(start, end)| format!("symbol_map U+{start}-U+{end} {FONT_FAMILY}"))
        .collect::<Vec<_>>();
    let mut targets = Vec::new();
    for name in ["config", "config.ghostty"] {
        targets.push(TerminalTarget {
            name: "ghostty",
            path: home
                .join("Library/Application Support/com.mitchellh.ghostty")
                .join(name),
            lines: ghostty_lines.clone(),
            reload: "reload Ghostty config (cmd+shift+,) or reopen the terminal",
        });
        targets.push(TerminalTarget {
            name: "ghostty",
            path: xdg.join("ghostty").join(name),
            lines: ghostty_lines.clone(),
            reload: "reload Ghostty config (cmd+shift+,) or reopen the terminal",
        });
    }
    targets.push(TerminalTarget {
        name: "kitty",
        path: xdg.join("kitty/kitty.conf"),
        lines: kitty_lines,
        reload: "reload kitty (ctrl+shift+f5) or reopen the terminal",
    });
    targets
}

fn marked_block(lines: Vec<String>) -> String {
    std::iter::once(MARKER_START.to_string())
        .chain(lines)
        .chain(std::iter::once(MARKER_END.to_string()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn upsert_marked(text: &str, body: &str) -> String {
    if let Some(from) = text.find(MARKER_START) {
        if let Some(rel_end) = text[from..].find(MARKER_END) {
            let to = from + rel_end + MARKER_END.len();
            let mut next = String::new();
            next.push_str(text[..from].trim_end());
            next.push_str("\n\n");
            next.push_str(body);
            let rest = text[to..].trim_start_matches('\n');
            if !rest.is_empty() {
                next.push('\n');
                next.push_str(rest);
            } else {
                next.push('\n');
            }
            return next;
        }
    }
    let mut next = text.trim_end().to_string();
    if !next.is_empty() {
        next.push_str("\n\n");
    }
    next.push_str(body);
    next.push('\n');
    next
}

fn user_font_dir() -> PathBuf {
    let home = directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    if cfg!(target_os = "macos") {
        return home.join("Library/Fonts");
    }
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"))
        .join("fonts")
}

fn font_hash() -> String {
    format!("{:x}", Sha256::digest(FONT_BYTES))[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_is_idempotent() {
        let body = marked_block(
            CODEPOINT_RANGES
                .into_iter()
                .map(|(start, end)| {
                    format!("font-codepoint-map = U+{start}-U+{end}=\"{FONT_FAMILY}\"")
                })
                .collect(),
        );
        let first = upsert_marked("# existing\n", &body);
        let again = upsert_marked(&first, &body);
        assert_eq!(first, again);
        assert!(first.contains("font-codepoint-map"));
        assert_eq!(first.matches(MARKER_START).count(), 1);
    }

    #[test]
    fn bundled_font_is_present() {
        assert!(FONT_BYTES.len() > 1000);
        assert_eq!(font_hash().len(), 8);
    }

    #[test]
    fn path_helper_compiles_for_coverage() {
        assert!(!user_font_dir().as_os_str().is_empty());
    }
}
