//! Clean-room terminal idle animation kernels for Aizen retained mode.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use std::f32::consts::TAU;

const SX: usize = 3;
const SY: usize = 3;

pub(super) fn enabled(has_conversation: bool, working: bool, focused: bool, idle_secs: u64) -> bool {
    use crate::core::cli_config::{IdleAnimation, TuiPerformance};
    if has_conversation || working || !focused || idle_secs >= 30 {
        return false;
    }
    let cfg = crate::core::cli_config::load();
    if matches!(cfg.tui_performance(), TuiPerformance::Minimal) {
        return false;
    }
    !matches!(cfg.idle_animation(), IdleAnimation::Off)
}

pub(super) fn target_frame_ms() -> u64 {
    use crate::core::cli_config::TuiPerformance;
    match crate::core::cli_config::load().tui_performance() {
        TuiPerformance::Full => 42,
        TuiPerformance::Reduced | TuiPerformance::Auto => 90,
        TuiPerformance::Minimal => 1000,
    }
}

/// One lance (tapered petal) of the starburst as a scalar field. Returns luminance 0.0 (outside) or
/// ~0.55..1.0 (inside, brighter along the spine). `count` rays evenly spaced, rotated by `spin`
/// (+`offset` phase); each ray is a rhombus widest at its mid-radius, tapering to points at both the
/// centre and the tip — the brand's chrysanthemum-sun ray. `fx/fy` are aspect-corrected physical
/// coords from centre; `r` is the mask radius.
#[allow(clippy::too_many_arguments)]
fn lance(fx: f32, fy: f32, theta: f32, spin: f32, count: f32, offset: f32, len_frac: f32, halfw_frac: f32, r: f32) -> f32 {
    let sector = TAU / count;
    // snap to the nearest ray axis for this layer
    let a = theta - spin - offset;
    let phi = (a / sector).round() * sector + spin + offset;
    let (sp, cp) = phi.sin_cos();
    let proj = fx * cp + fy * sp; // distance along the ray axis
    let len = len_frac * r;
    if proj < 0.0 || proj > len {
        return 0.0;
    }
    let perp = (fx * -sp + fy * cp).abs(); // perpendicular distance to the axis
    let t = proj / len; // 0 at centre → 1 at tip
    let hw = halfw_frac * r * (1.0 - (2.0 * t - 1.0).abs()); // triangular taper, widest at mid
    if hw <= 0.0 || perp > hw {
        return 0.0;
    }
    0.55 + 0.45 * (1.0 - perp / hw) // brightest on the spine, fading to the edge
}

/// The Aizen logo — the 16-long + 16-short-ray chrysanthemum sun — spinning infinitely. Replaces the
/// old torus idle kernel; mirrors `splash::petal_mask` geometry as a rotating scalar field so it reads
/// as the brand mark, not a generic shape. `elapsed` drives a slow, endless rotation.
pub(super) fn logo_lines(width: u16, height: u16, elapsed: f32) -> Vec<Line<'static>> {
    let cw = width.max(8) as usize;
    let ch = height.max(4) as usize;
    let sw = cw * SX;
    let sh = ch * SY;
    let mut lum = vec![0.0f32; sw * sh];

    let cx = sw as f32 / 2.0;
    let cy = sh as f32 / 2.0;
    // Terminal cells are ~2× taller than wide; scale y by 2 so the sun renders round, not squashed.
    let r = (cx.min(sh as f32) - 2.0).max(4.0);
    let spin = elapsed * 0.5; // radians — slow endless rotation
    // A light source sweeping around the disc so the symmetric mark visibly turns even head-on.
    let core = 0.05 * r;

    for y in 0..sh {
        let fy = (y as f32 + 0.5 - cy) * 2.0;
        for x in 0..sw {
            let fx = x as f32 + 0.5 - cx;
            let rho = (fx * fx + fy * fy).sqrt();
            if rho > r {
                continue;
            }
            let theta = fy.atan2(fx);
            // 16 long rays + 16 shorter rays offset half a step (matches the splash petal mask).
            let mut v = lance(fx, fy, theta, spin, 16.0, 0.0, 0.97, 0.045, r)
                .max(lance(fx, fy, theta, spin, 16.0, sector_half(16.0), 0.40, 0.05, r));
            if rho <= core {
                v = v.max(1.0); // solid convergence at the centre
            }
            if v > 0.0 {
                // rotating glint: a bright arc that sweeps the mark so the spin is unmistakable
                let glint = 0.75 + 0.25 * (theta - elapsed * 1.3).cos().max(0.0);
                lum[y * sw + x] = (v * glint).min(1.0);
            }
        }
    }

    cells_to_lines(&lum, sw, cw, ch, elapsed)
}

/// Half of a layer's angular step — the phase offset that interleaves the short rays between the long.
fn sector_half(count: f32) -> f32 {
    TAU / count / 2.0
}

fn cells_to_lines(lum: &[f32], sw: usize, cw: usize, ch: usize, elapsed: f32) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(ch);
    for cy in 0..ch {
        let mut spans = Vec::with_capacity(cw);
        for cx in 0..cw {
            let mut bits = 0u16;
            let mut brightness = 0.0f32;
            for dy in 0..SY {
                for dx in 0..SX {
                    let v = lum[(cy * SY + dy) * sw + cx * SX + dx];
                    brightness = brightness.max(v);
                    if v > 0.12 {
                        bits |= 1 << (dy * SX + dx);
                    }
                }
            }
            let ch = glyph(bits, brightness);
            let hue = ((elapsed * 18.0 + cx as f32 * 0.8 + cy as f32 * 0.5) as u16 % 24) as u8;
            let color = Color::Indexed(232u8.saturating_add(hue.min(23)));
            spans.push(Span::styled(ch.to_string(), Style::default().fg(color)));
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn glyph(bits: u16, brightness: f32) -> char {
    if bits == 0 {
        return ' ';
    }
    let count = bits.count_ones();
    match (count, brightness) {
        (1..=2, b) if b < 0.35 => '·',
        (1..=3, _) => '░',
        (4..=6, b) if b < 0.6 => '▒',
        (4..=7, _) => '▓',
        _ => '█',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_has_exact_geometry() {
        let lines = logo_lines(32, 10, 1.25);
        assert_eq!(lines.len(), 10);
        assert!(lines.iter().all(|line| line.width() == 32));
    }

    /// The starburst must actually rotate — two frames a quarter-second apart must differ somewhere,
    /// or the "spinning infinitely" claim is a lie (a static mark).
    #[test]
    fn logo_spins_over_time() {
        let a = logo_lines(40, 16, 0.0);
        let b = logo_lines(40, 16, 0.75);
        let sa: String = a.iter().flat_map(|l| l.spans.iter().map(|s| s.content.clone())).collect();
        let sb: String = b.iter().flat_map(|l| l.spans.iter().map(|s| s.content.clone())).collect();
        assert_ne!(sa, sb, "starburst animation is static across frames");
    }

    /// At a reasonable size the mark must render some filled glyphs (not an empty rect) — guards
    /// against the geometry collapsing to nothing after a refactor.
    #[test]
    fn logo_is_non_empty() {
        let lines = logo_lines(48, 18, 1.0);
        let any = lines.iter().any(|l| l.spans.iter().any(|s| s.content.trim() != ""));
        assert!(any, "starburst rendered entirely blank");
    }

    #[test]
    fn glyph_empty_is_space() {
        assert_eq!(glyph(0, 0.0), ' ');
        assert_ne!(glyph(1, 1.0), ' ');
    }
}
