//! Vendor marks for the sidebar identity row.
//!
//! Prefer Private Use Area glyphs from the bundled Herdr Agent Icons Max
//! face (same marks herdr-radar ships). Muse has no glyph in that face, so
//! it keeps a one-cell text stand-in. Terminals need the codepoint map that
//! [`crate::configure::font`] writes for Ghostty / kitty; without it the
//! PUA cells render as missing glyphs.

use crate::model::Harness;

/// One-cell mark for a harness.
pub fn for_harness(harness: Harness) -> &'static str {
    match harness {
        Harness::Claude => "\u{e1a0}",
        Harness::Codex => "\u{e1a1}",
        Harness::OpenCode => "\u{e1a2}",
        Harness::Omp => "\u{e1a3}",
        Harness::Pi => "\u{e1a9}",
        Harness::Hermes => "\u{e1aa}",
        Harness::Cursor => "\u{e1ab}",
        Harness::Grok => "\u{e1b1}",
        Harness::Agy => "\u{e1b2}",
        Harness::Devin => "\u{e1b5}",
        // Not in HerdrAgentIconsMax; keep a plain mark rather than a tofu.
        Harness::Muse => "◈",
    }
}

/// Three cells the sidebar reserves after the mark.
///
/// The terminal draws the mark larger than one cell (WezTerm's fallback font
/// `scale`), and only lets a glyph spill into blanks in its own attribute run,
/// so the blanks travel inside the token value. Herdr trims both ends of every
/// value, so the run ends on U+2800 BRAILLE PATTERN BLANK: not whitespace, one
/// cell wide, no ink. A terminal without a Braille-capable font shows a box
/// in that cell, and copying the sidebar copies it.
pub const SIDEBAR_RESERVE: &str = "  \u{2800}";

/// The value published for a harness's `$quota_icon*` token: mark plus room.
pub fn sidebar_mark(harness: Harness) -> String {
    format!("{}{SIDEBAR_RESERVE}", for_harness(harness))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::AgentSelection;

    /// Four cells survive Herdr's trim: the mark, two blanks, U+2800.
    #[test]
    fn the_published_mark_keeps_its_reserved_cells_through_a_trim() {
        for harness in AgentSelection::SUPPORTED {
            let value = sidebar_mark(harness);
            assert_eq!(value.trim(), value, "{harness:?}");
            assert_eq!(value.chars().count(), 4, "{harness:?}");
            assert!(value.starts_with(for_harness(harness)));
        }
    }

    #[test]
    fn every_supported_harness_has_a_one_cell_mark() {
        for harness in AgentSelection::SUPPORTED {
            let mark = for_harness(harness);
            assert_eq!(mark.chars().count(), 1, "{harness:?} -> {mark:?}");
        }
    }

    #[test]
    fn supported_harnesses_use_the_radar_pua_except_muse() {
        assert_eq!(for_harness(Harness::Claude), "\u{e1a0}");
        assert_eq!(for_harness(Harness::Codex), "\u{e1a1}");
        assert_eq!(for_harness(Harness::Grok), "\u{e1b1}");
        assert_eq!(for_harness(Harness::Agy), "\u{e1b2}");
        assert_eq!(for_harness(Harness::Cursor), "\u{e1ab}");
        assert_eq!(for_harness(Harness::Hermes), "\u{e1aa}");
        assert_eq!(for_harness(Harness::Muse), "◈");
    }
}
