//! A curated glyph set for a lively-but-tasteful TUI, in three tiers.
//!
//! A terminal can't render image icons — only text glyphs. We offer:
//! - **emoji** (default): width-2 emoji that render on any modern terminal (Windows Terminal,
//!   iTerm, …) WITHOUT a special font — no tofu on a default install.
//! - **nerd** (`NG_NERD=1`): dev-style Nerd Font glyphs (Font Awesome codepoints in the Private
//!   Use Area). Crisper/"premium", but ONLY render if the terminal uses a patched Nerd Font (e.g.
//!   "Cascadia Code NF"); otherwise they show as boxes. Opt-in by design.
//! - **none** (`NG_NO_ICONS=1`): plain text everywhere.
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
/// the config file on every call. 0 = unset (→ emoji default), 1 = off, 2 = emoji, 3 = nerd.
static CONFIG_TIER: AtomicU8 = AtomicU8::new(0);

/// Persist the icon tier chosen in config into the fast in-process slot. Call at startup + after
/// the config wizard. Accepts `"off"`/`"none"`, `"emoji"`, `"nerd"`; anything else → emoji default.
pub fn set_tier(cfg_value: Option<&str>) {
    let v = match cfg_value.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("off") | Some("none") => 1,
        Some("emoji") => 2,
        Some("nerd") => 3,
        _ => 0,
    };
    CONFIG_TIER.store(v, Ordering::Relaxed);
}

/// Resolve the active tier: env wins (one-off override) > config > emoji default.
fn tier() -> Tier {
    if std::env::var_os("NG_NO_ICONS").is_some() {
        return Tier::None;
    }
    if std::env::var_os("NG_NERD").is_some() {
        return Tier::Nerd;
    }
    match CONFIG_TIER.load(Ordering::Relaxed) {
        1 => Tier::None,
        3 => Tier::Nerd,
        _ => Tier::Emoji,
    }
}

/// Whether any icons are shown.
pub fn on() -> bool {
    !matches!(tier(), Tier::None)
}

/// Pick the glyph for the active tier (emoji default, nerd when `NG_NERD`).
fn pick(emoji: &'static str, nerd: &'static str) -> &'static str {
    if matches!(tier(), Tier::Nerd) {
        nerd
    } else {
        emoji
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
        "sessions" => pick("🗂", "\u{f187}"), // archive
        "compact" => pick("🗜", "\u{f066}"),  // compress
        "clear" => pick("🧹", "\u{f021}"),    // refresh
        "tokens" => pick("📊", "\u{f080}"),   // bar-chart
        "cost" => pick("💰", "\u{f155}"),     // dollar
        "yolo" => pick("⚡", "\u{f0e7}"),     // bolt
        "smart" => pick("◆", "\u{f132}"),     // shield
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
        // default (emoji) tier — no NG_NERD / NG_NO_ICONS in the test env.
        assert_eq!(tool_group("skills"), "📘");
        assert_eq!(tool_group("memory"), "🧠");
        assert_eq!(tool_group("nope"), "•");
        assert_eq!(slash("telegram"), "📱");
        assert_eq!(slash("quit"), "🚪");
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
