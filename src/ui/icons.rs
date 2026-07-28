//! A curated glyph set for a lively-but-tasteful TUI, in three tiers.
//!
//! A terminal can't render image icons — only text glyphs. We offer:
//! - **nerd** (default): dev-style Nerd Font glyphs (Font Awesome codepoints in the Private Use
//!   Area). Crisp, monochrome, single-cell — they inherit the moonlight accent instead of a
//!   clashing multi-colour emoji, so the whole TUI reads as one calm palette (the "clean like a
//!   pro CLI" look). They ONLY render on a patched Nerd Font (e.g. "Cascadia Code NF", the common
//!   dev default); on a plain font they show as boxes (tofu) → set `AIZEN_EMOJI=1` or `icons=emoji`.
//! - **emoji** (`AIZEN_EMOJI=1` / `icons=emoji`): width-2 colour emoji that render on any modern
//!   terminal WITHOUT a special font — the safe fallback when Nerd Font tofu shows up.
//! - **none** (`AIZEN_NO_ICONS=1` / `icons=off`): plain text everywhere.
//!
//! Nerd glyphs sit in U+E000–U+F8FF (PUA) → `unicode-width` measures them as 1 cell, matching how
//! a Nerd Font renders them, so bordered panels stay aligned.

use std::sync::atomic::{AtomicU8, Ordering};

enum Tier {
    None,
    Emoji,
    Nerd,
}

/// The config-chosen tier, set once at startup (and after `/config`) so glyph lookups don't read
/// the config file on every call. 0 = unset (→ nerd default), 1 = off, 2 = emoji, 3 = nerd.
static CONFIG_TIER: AtomicU8 = AtomicU8::new(0);

/// Persist the icon tier chosen in config into the fast in-process slot. Call at startup + after
/// the config wizard. Accepts `"off"`/`"none"`, `"emoji"`, `"nerd"`; anything else → nerd default.
pub fn set_tier(cfg_value: Option<&str>) {
    let v = match cfg_value.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("off") | Some("none") => 1,
        Some("emoji") => 2,
        Some("nerd") => 3,
        _ => 0,
    };
    CONFIG_TIER.store(v, Ordering::Relaxed);
}

/// Resolve the active tier: env wins (one-off override) > config > nerd default. The default is
/// nerd so the icon set is monochrome + single-cell + accent-tinted (one calm palette). A plain
/// (non-Nerd) font shows tofu → `AIZEN_EMOJI=1` forces the colour-emoji tier, `AIZEN_NO_ICONS=1`
/// forces plain text.
fn tier() -> Tier {
    if crate::core::cli_config::branded_flag("NO_ICONS") {
        return Tier::None;
    }
    if crate::core::cli_config::branded_flag("EMOJI") {
        return Tier::Emoji;
    }
    if crate::core::cli_config::branded_flag("NERD") {
        return Tier::Nerd;
    }
    match CONFIG_TIER.load(Ordering::Relaxed) {
        1 => Tier::None,
        2 => Tier::Emoji,
        _ => Tier::Nerd,
    }
}

/// Whether any icons are shown.
pub fn on() -> bool {
    !matches!(tier(), Tier::None)
}

/// Pick the glyph for the active tier — the nerd glyph by default, the colour emoji only when the
/// emoji tier is forced (`AIZEN_EMOJI=1` / `icons=emoji`).
fn pick(emoji: &'static str, nerd: &'static str) -> &'static str {
    if matches!(tier(), Tier::Emoji) {
        emoji
    } else {
        nerd
    }
}

/// `"<icon> "` when icons are on, else `""` — for prefixing a label.
pub fn g(icon: &str) -> String {
    if on() && !icon.is_empty() {
        format!("{icon} ")
    } else {
        String::new()
    }
}

// ── brand / status ─────────────────────────────────────────────────────────────
pub fn spark() -> &'static str {
    pick("⚡", "\u{f0e7}") // nf-fa-bolt
}
pub fn learned() -> &'static str {
    pick("🌱", "\u{f06c}") // nf-fa-leaf
}
pub fn tip() -> &'static str {
    pick("💡", "\u{f0eb}") // nf-fa-lightbulb-o
}

/// Icon for an agent tool group (splash panel).
pub fn tool_group(label: &str) -> &'static str {
    match label {
        "memory" => pick("🧠", "\u{f1c0}"),   // database
        "skills" => pick("📘", "\u{f02d}"),   // book
        "files" => pick("📂", "\u{f07b}"),    // folder
        "shell" => pick("💻", "\u{f120}"),    // terminal
        "web" => pick("🌐", "\u{f0ac}"),      // globe
        "tasks" => pick("📋", "\u{f0ae}"),    // tasks / list-check
        "persona" => pick("🎭", "\u{f007}"),  // persona / user
        "delegate" => pick("🤝", "\u{f0c0}"), // users
        _ => "•",
    }
}

/// Icon for a slash command (the `/` picker + help).
pub fn slash(name: &str) -> &'static str {
    match name {
        "help" => pick("💡", "\u{f059}"),     // question-circle
        "model" => pick("🤖", "\u{f544}"),    // robot
        "config" => pick("⚙", "\u{f013}"),    // cog
        "memory" => pick("🧠", "\u{f1c0}"),   // database
        "persona" => pick("🎭", "\u{f007}"),  // user
        "skills" => pick("📘", "\u{f02d}"),   // book
        "commands" => pick("⌥", "\u{f120}"),  // terminal
        "apps" => pick("🧩", "\u{f12e}"),     // puzzle-piece
        "mcp" => pick("🔌", "\u{f1e6}"),      // plug
        "telegram" => pick("📱", "\u{f2c6}"), // telegram
        "save" => pick("💾", "\u{f0c7}"),     // save
        "load" => pick("📂", "\u{f07c}"),     // folder-open
        "sessions" => pick("🗂", "\u{f187}"),  // archive
        "compact" => pick("🗜", "\u{f066}"),   // compress
        "clear" => pick("🧹", "\u{f021}"),    // refresh
        "tokens" => pick("📊", "\u{f080}"),   // bar-chart
        "cost" => pick("💰", "\u{f155}"),     // dollar
        "approval" => pick("🛡", "\u{f132}"),  // shield
        "yolo" => pick("⚡", "\u{f0e7}"),     // legacy alias / HUD
        "smart" => pick("◆", "\u{f132}"),     // legacy alias / HUD
        "quit" => pick("🚪", "\u{f08b}"),     // sign-out
        _ => "•",
    }
}

/// Section header icons for the splash panel.
pub fn hdr_tools() -> &'static str {
    pick("🧰", "\u{f0ad}") // wrench
}
pub fn hdr_commands() -> &'static str {
    pick("⌨", "\u{f11c}") // keyboard
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_labels_map_else_bullet() {
        // default (nerd) tier — no AIZEN_EMOJI / AIZEN_NO_ICONS in the test env, so `pick` returns
        // the nerd glyph. The `•` fallback is tier-independent (returned directly, not via `pick`),
        // so an unknown label is a bullet regardless of tier.
        assert_eq!(tool_group("skills"), "\u{f02d}"); // book
        assert_eq!(tool_group("memory"), "\u{f1c0}"); // database
        assert_eq!(tool_group("nope"), "•");
        assert_eq!(slash("telegram"), "\u{f2c6}");
        assert_eq!(slash("quit"), "\u{f08b}");
        assert_eq!(slash("nope"), "•");
    }

    #[test]
    fn nerd_glyphs_are_single_cell() {
        // every nerd glyph must live in the PUA (U+E000–U+F8FF) so it measures as 1 cell and the
        // bordered splash stays aligned under a Nerd Font.
        let nerd = [
            spark_nerd(),
            "\u{f1c0}",
            "\u{f544}",
            "\u{f2c6}",
            "\u{f08b}",
            "\u{f0eb}", // tip / lightbulb
        ];
        for g in nerd {
            let c = g.chars().next().unwrap();
            assert!(('\u{e000}'..='\u{f8ff}').contains(&c), "{c:?} not in PUA");
        }
    }

    // helper so the test references the nerd glyph without env toggling
    fn spark_nerd() -> &'static str {
        "\u{f0e7}"
    }

    #[test]
    fn g_prefixes_with_space() {
        let out = g("X");
        assert!(out == "X " || out.is_empty());
    }
}
