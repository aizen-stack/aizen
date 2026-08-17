//! The interactive surface of reasoning effort: this turn's tier, the `/effort` slider, and the
//! status report. The CLASSIFIER is pure and lives in `core::effort`; everything here reads config
//! and prints, which is why it could not live there.

use crate::core::cli_config;
use crate::ui::{splash, theme, tui};
use console::style;

/// Decide THIS turn's reasoning effort. When auto-detect is ON (default), classify the user's
/// fully-expanded message (keyword ladder → complexity heuristic); a hit forces that tier, else we
/// fall back to the configured `reasoning_effort` (which may itself be `None` ⇒ omit the field).
/// PURE-ish wrapper (loads config) around the pure `core::effort::classify_effort`. The result is
/// armed into the per-turn override cell read by the LLM client, and cleared at turn end.
pub(crate) fn resolve_turn_effort(line: &str) -> Option<String> {
    // Ultimate mode pins max effort every turn (auto-detect is bypassed) — the aizen `ultracode`.
    if cli_config::ultimate_enabled() {
        return Some("max".to_string());
    }
    if cli_config::auto_effort_enabled() {
        // P3: adaptive routing lets the complexity heuristic climb to `xhigh` on the hardest turns.
        let adaptive = cli_config::adaptive_effort_enabled();
        if let Some(e) = crate::core::effort::classify_effort_with(line, adaptive) {
            return Some(e.as_str().to_string());
        }
    }
    cli_config::load().reasoning_effort.clone()
}

/// The per-turn "effort: <tier>" status line, tinted to match the slider's tier colours (auto =
/// moonlight · low = green · medium = dim silver · high = gold) so the whole effort feature reads as
/// one system. `None` ⇒ the field is omitted this turn → shown as a faint "default".
pub(crate) fn effort_turn_line(eff: Option<&str>) -> String {
    // low = green, medium = dim silver; the three "hot" rungs escalate high → xhigh → max
    // (gold → bold gold → salmon) so the eye can tell them apart at a glance.
    let styled = match eff {
        Some("low") => console::style("low".to_string()).color256(theme::OK),
        Some("medium") => console::style("medium".to_string()).color256(theme::ACCENT_DIM),
        Some("high") => console::style("high".to_string()).color256(theme::WARN),
        Some("xhigh") => console::style("xhigh".to_string())
            .color256(theme::WARN)
            .bold(),
        Some("max") => console::style("max".to_string())
            .color256(theme::ERR)
            .bold(),
        Some(other) => console::style(other.to_string()).color256(theme::ACCENT),
        None => console::style("default".to_string()).color256(theme::FAINT),
    };
    format!("{} {}", theme::faint("  effort:"), styled)
}

/// The current effort setting as a slider index: 0 = auto (auto-detect ON, no pinned tier), else the
/// pinned tier (1=low · 2=medium · 3=high). A pinned-but-unknown effort string, or auto-off with no
/// pin, both fall back to `auto` so the slider always opens on a valid stop.
fn effort_slider_start() -> usize {
    let cfg = cli_config::load();
    if cli_config::auto_effort_enabled() {
        return 0; // auto ON ⇒ the "auto" stop, regardless of any stale pinned value
    }
    match cfg.reasoning_effort.as_deref() {
        Some("low") => 1,
        Some("medium") => 2,
        Some("high") => 3,
        Some("xhigh") => 4,
        Some("max") => 5,
        _ => 0,
    }
}

/// Apply a slider choice to the config and persist it. `0` ⇒ auto (auto_effort=None, clear the pin);
/// `1..=5` ⇒ pin low/medium/high/xhigh/max and turn auto off — the exact same writes as `/effort auto`
/// and `/effort low|medium|high|xhigh|max`, so the slider and the text commands stay in lockstep.
fn apply_effort_choice(idx: usize) {
    let mut cfg = cli_config::load();
    let msg = match idx {
        1..=5 => {
            let tier = ["", "low", "medium", "high", "xhigh", "max"][idx];
            cfg.reasoning_effort = Some(tier.to_string());
            cfg.auto_effort = Some(false);
            format!("effort pinned to {tier} (auto off) — every turn now sends reasoning_effort={tier}.")
        }
        _ => {
            cfg.auto_effort = None; // None ⇒ auto ON (the default)
            cfg.reasoning_effort = None; // clear any stale pin so auto isn't shadowed
            "effort auto ON — each turn's effort is detected from your message (keyword + complexity).".to_string()
        }
    };
    match cli_config::save(&cfg) {
        Ok(_) => tui::emit_line(&style(msg).color256(splash::ACCENT).to_string()),
        Err(e) => tui::emit_line(&format!("{} {e}", style("effort:").red())),
    }
}

/// The plain text status report for `/effort status` (and the off-TTY fallback of the bare `/effort`).
pub(crate) fn effort_status_report() {
    let cfg = cli_config::load();
    let auto = if cli_config::auto_effort_enabled() {
        "on"
    } else {
        "off"
    };
    let fixed = cfg
        .reasoning_effort
        .as_deref()
        .unwrap_or("(none — omitted)");
    tui::emit_line(
        &style(format!(
            "effort: auto-detect {auto} · fixed reasoning_effort {fixed}\n\
             /effort auto|off · /effort low|medium|high (pins it, turns auto off) · /effort none (clear)"
        ))
        .dim()
        .to_string(),
    );
    if std::env::var("AIZEN_AUTO_EFFORT").is_ok() {
        tui::emit_line(
            &style("(note: AIZEN_AUTO_EFFORT is set — it overrides the auto toggle)")
                .dim()
                .to_string(),
        );
    }
}

/// Bare `/effort` → the animated drag slider. Opens on the current setting; a commit persists the
/// choice, Esc keeps things as-is. Off-TTY the slider returns `None` immediately, so we fall back to
/// the text report instead of leaving the user with no output.
pub(crate) fn effort_slider_flow() {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        effort_status_report();
        return;
    }
    match tui::effort_slider(effort_slider_start()) {
        Some(idx) => apply_effort_choice(idx),
        None => tui::emit_line(&style("(effort unchanged)").dim().to_string()),
    }
}
