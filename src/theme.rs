//! The single source of truth for the `ng` TUI palette — the "Studio" identity: one warm gold
//! accent + a small, restrained set of semantic colours (ok / error / warn / link) + a code-syntax
//! sub-palette. Everything is 256-colour (universally supported; no truecolor dependency), routed
//! through `console::style` so `NO_COLOR` and non-TTY output are auto-stripped.
//!
//! Discipline: NOT a rainbow. Gold carries brand + structure (borders, prompt, gutter, tool names,
//! headings). The semantic colours appear only where they MEAN something (green = success/added,
//! red = error/removed, blue = links/inline-code, amber = warnings). Anything else is neutral grey.
//!
//! Use the helpers (`accent`, `ok`, `err`, …) instead of scattering raw `color256(..)` calls so the
//! palette can be retuned in one place.
//!
//! Aizen brand palette (from the official logo, for reference / future retune):
//!   ink #0d0d0c · paper #f7f6f3 · indigo #243a73 (藍染 = "indigo dyeing", the namesake) ·
//!   gold-noir #b0894c (≈ 256-colour 137) · crimson-noir #993030. The terminal uses the *noir*
//!   reading: a silver-white wordmark + a gold mark on the dark ground — the warm gold below is the
//!   gold-noir variant. To match the brand gold exactly, set ACCENT to 137.

use console::{style, StyledObject};
use std::fmt::Display;

// ── core palette (256-colour indices) ───────────────────────────────────────────
/// Warm gold — brand + structure (borders, prompt arrow, gutter, tool names, headings).
pub const ACCENT: u8 = 178;
/// Darker gold — rules / borders that should sit quieter than the accent.
pub const ACCENT_DIM: u8 = 136;
/// Neutral grey for secondary text (the old `.dim()` role, but a defined shade).
pub const MUTED: u8 = 245;
/// Very faint grey — separators, the code-block rule, timestamps.
pub const FAINT: u8 = 240;

// ── semantic (used ONLY where the colour carries meaning) ────────────────────────
/// Success / confirmation / added.
pub const OK: u8 = 71;
/// Error / failure / removed.
pub const ERR: u8 = 167;
/// Warning / caution.
pub const WARN: u8 = 179;
/// Links + inline code (a calm blue, distinct from the gold accent).
pub const LINK: u8 = 110;

// ── code-syntax sub-palette (light, best-effort highlighter) ─────────────────────
pub const CODE_KEYWORD: u8 = 176; // soft mauve
pub const CODE_STRING: u8 = 108; // sage green
pub const CODE_NUMBER: u8 = 110; // blue
pub const CODE_COMMENT: u8 = 244; // grey
pub const CODE_RULE: u8 = 240; // the left │ / box border

// ── helpers (return StyledObject so callers can still chain .bold()/.italic()) ───
pub fn accent<D: Display>(d: D) -> StyledObject<D> {
    style(d).color256(ACCENT)
}
pub fn accent_dim<D: Display>(d: D) -> StyledObject<D> {
    style(d).color256(ACCENT_DIM)
}
pub fn muted<D: Display>(d: D) -> StyledObject<D> {
    style(d).color256(MUTED)
}
pub fn faint<D: Display>(d: D) -> StyledObject<D> {
    style(d).color256(FAINT)
}
pub fn ok<D: Display>(d: D) -> StyledObject<D> {
    style(d).color256(OK)
}
pub fn err<D: Display>(d: D) -> StyledObject<D> {
    style(d).color256(ERR)
}
pub fn warn<D: Display>(d: D) -> StyledObject<D> {
    style(d).color256(WARN)
}
pub fn link<D: Display>(d: D) -> StyledObject<D> {
    style(d).color256(LINK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_colours_are_distinct() {
        // A regression guard: if two semantic roles collapse onto the same index the UI loses
        // meaning. Accent/ok/err/link/warn must all differ.
        let all = [ACCENT, OK, ERR, WARN, LINK];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b, "two semantic colours share index {a}");
            }
        }
    }

    #[test]
    fn helpers_render_to_nonempty() {
        // Under the test harness colours may be stripped (no TTY); the text must still be present.
        assert!(accent("x").to_string().contains('x'));
        assert!(ok("done").to_string().contains("done"));
    }
}
