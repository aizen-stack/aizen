//! A moonlit braille scene — the "loading a module on the moon" tableau, drawn entirely in Unicode
//! braille (2×4 dots per cell) so it stays pure text (no image protocol, no bundled asset) and works
//! on any terminal. Everything is outline/halftone line-art in the moonlight-silver palette: a dotted
//! moon, a ringed planet, a starfield, layered mountains, flowing dunes, an astronaut planting a flag,
//! and a `[■■■□] Loading modules…` bar. Depth is faked with a 5-step brightness ramp (far = faint,
//! near = bright).
//!
//! `aizen art` renders one frame to stdout; the same [`frame`] builder is what the idle screensaver
//! can drive later by advancing `phase`.
//!
//! On sixel-capable terminals (Windows Terminal, WezTerm, iTerm2, foot, …) `aizen art` instead
//! blits [`MOON_SIXEL`] — a real raster of the source moonscape, quantised to a few silver greys and
//! baked into the binary at build time via `include_str!` (still one self-contained file, no runtime
//! asset). Everywhere else it falls back to the braille [`frame`] so the command always shows
//! *something* printable.

use console::{style, Term};
use std::f64::consts::PI;
use std::fmt::Write as _;

/// Grayscale ramp (256-colour cube greys), faint → bright. Index into this for depth.
const LEVELS: [u8; 5] = [238, 242, 246, 250, 255];

/// A braille dot canvas. One logical pixel = one braille dot; 2×4 dots collapse into one glyph cell.
struct Canvas {
    w: usize,
    h: usize,
    /// Per-dot brightness, stored as `level + 1` (0 = unlit).
    px: Vec<u8>,
}

/// Braille bit index for a dot at (col 0..2, row 0..4) — matches the Unicode dot numbering
/// (1 4 / 2 5 / 3 6 / 7 8).
const BIT: [[u32; 4]; 2] = [[0, 1, 2, 6], [3, 4, 5, 7]];

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            px: vec![0; w * h],
        }
    }

    /// Light one dot at the brightness `lvl` (0..=4), keeping the brighter value if already lit.
    fn plot(&mut self, x: i32, y: i32, lvl: u8) {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
            return;
        }
        let i = y as usize * self.w + x as usize;
        let v = lvl.min(4) + 1;
        if v > self.px[i] {
            self.px[i] = v;
        }
    }

    fn plotf(&mut self, x: f64, y: f64, lvl: u8) {
        self.plot(x.round() as i32, y.round() as i32, lvl);
    }

    /// Bresenham line between two dot coordinates.
    fn line(&mut self, x0: f64, y0: f64, x1: f64, y1: f64, lvl: u8) {
        let (mut x0, mut y0) = (x0.round() as i32, y0.round() as i32);
        let (x1, y1) = (x1.round() as i32, y1.round() as i32);
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            self.plot(x0, y0, lvl);
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }

    /// A full circle outline.
    fn ring(&mut self, cx: f64, cy: f64, r: f64, lvl: u8) {
        let steps = ((r * 6.5) as usize).max(16);
        for t in 0..steps {
            let a = 2.0 * PI * (t as f64) / (steps as f64);
            self.plotf(cx + r * a.cos(), cy + r * a.sin(), lvl);
        }
    }

    /// An ellipse outline (planet ring).
    fn ellipse(&mut self, cx: f64, cy: f64, rx: f64, ry: f64, lvl: u8) {
        let steps = ((rx.max(ry) * 6.5) as usize).max(20);
        for t in 0..steps {
            let a = 2.0 * PI * (t as f64) / (steps as f64);
            self.plotf(cx + rx * a.cos(), cy + ry * a.sin(), lvl);
        }
    }

    /// One rounded corner: a quarter arc centred at (cx,cy), bulging toward (sx,sy) ∈ {−1,1}².
    fn corner(&mut self, cx: f64, cy: f64, r: f64, sx: f64, sy: f64, lvl: u8) {
        let steps = ((r * 3.0) as usize).max(6);
        for t in 0..=steps {
            let a = (PI / 2.0) * (t as f64) / (steps as f64);
            self.plotf(cx + sx * r * a.cos(), cy + sy * r * a.sin(), lvl);
        }
    }

    /// A rounded-rectangle outline (the suit's helmet, torso, limbs).
    fn rrect(&mut self, x0: f64, y0: f64, x1: f64, y1: f64, r: f64, lvl: u8) {
        let r = r.min((x1 - x0) / 2.0).min((y1 - y0) / 2.0).max(0.0);
        self.line(x0 + r, y0, x1 - r, y0, lvl);
        self.line(x0 + r, y1, x1 - r, y1, lvl);
        self.line(x0, y0 + r, x0, y1 - r, lvl);
        self.line(x1, y0 + r, x1, y1 - r, lvl);
        self.corner(x0 + r, y0 + r, r, -1.0, -1.0, lvl);
        self.corner(x1 - r, y0 + r, r, 1.0, -1.0, lvl);
        self.corner(x1 - r, y1 - r, r, 1.0, 1.0, lvl);
        self.corner(x0 + r, y1 - r, r, -1.0, 1.0, lvl);
    }

    /// A stippled (halftone) filled disc — the dotted moon.
    fn stipple_disc(&mut self, cx: f64, cy: f64, r: f64, step: usize, lvl: u8) {
        let r2 = r * r;
        let y0 = (cy - r).floor() as i32;
        let y1 = (cy + r).ceil() as i32;
        let x0 = (cx - r).floor() as i32;
        let x1 = (cx + r).ceil() as i32;
        let step = step.max(1) as i32;
        let mut y = y0;
        while y <= y1 {
            let mut x = x0;
            while x <= x1 {
                let (dx, dy) = (x as f64 - cx, y as f64 - cy);
                if dx * dx + dy * dy <= r2 {
                    self.plot(x, y, lvl);
                }
                x += step;
            }
            y += step;
        }
    }

    /// A stippled filled rectangle — the flag's field.
    fn stipple_rect(&mut self, x0: f64, y0: f64, x1: f64, y1: f64, step: usize, lvl: u8) {
        let step = step.max(1) as i32;
        let mut y = y0.round() as i32;
        let ye = y1.round() as i32;
        while y <= ye {
            let mut x = x0.round() as i32;
            let xe = x1.round() as i32;
            while x <= xe {
                self.plot(x, y, lvl);
                x += step;
            }
            y += step;
        }
    }

    /// Collapse the dot grid into coloured braille rows (one ANSI-styled glyph per lit cell).
    fn to_lines(&self) -> Vec<String> {
        let mut lines = Vec::with_capacity(self.h / 4 + 1);
        let mut cy = 0;
        while cy < self.h {
            let mut line = String::new();
            let mut cx = 0;
            while cx < self.w {
                let mut bits: u32 = 0;
                let mut lvl: u8 = 0;
                for dx in 0..2 {
                    for dy in 0..4 {
                        let (px, py) = (cx + dx, cy + dy);
                        if px < self.w && py < self.h {
                            let v = self.px[py * self.w + px];
                            if v > 0 {
                                bits |= 1 << BIT[dx][dy];
                                lvl = lvl.max(v - 1);
                            }
                        }
                    }
                }
                if bits == 0 {
                    line.push(' ');
                } else {
                    let ch = char::from_u32(0x2800 + bits).unwrap_or(' ');
                    let _ = write!(line, "{}", style(ch).color256(LEVELS[lvl as usize]));
                }
                cx += 2;
            }
            // Trim trailing spaces so we don't paint a wall of blank cells.
            while line.ends_with(' ') {
                line.pop();
            }
            lines.push(line);
            cy += 4;
        }
        lines
    }
}

fn hash(x: u64) -> u64 {
    let mut x = x.wrapping_add(0x9E3779B97F4A7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    x ^ (x >> 31)
}

/// Draw the astronaut (outline line-art) with feet at `feet_y`, centred on `cx`, total height `ah`.
fn astronaut(c: &mut Canvas, cx: f64, feet_y: f64, ah: f64) {
    let bright = 4u8;
    let mid = 3u8;
    let aw = ah * 0.24; // shoulder half-width
    let top = feet_y - ah;

    // Helmet: outer rounded square + inner visor.
    let head_r = ah * 0.135;
    let hx0 = cx - head_r * 1.2;
    let hx1 = cx + head_r * 1.2;
    let hy0 = top;
    let hy1 = top + head_r * 2.25;
    c.rrect(hx0, hy0, hx1, hy1, head_r * 0.85, bright);
    let inset = head_r * 0.35;
    c.rrect(
        hx0 + inset,
        hy0 + inset * 1.3,
        hx1 - inset,
        hy1 - inset * 0.7,
        head_r * 0.6,
        mid,
    );

    // Torso (life-support pack) — a tall rounded box below the helmet.
    let sh_y = hy1 + ah * 0.02;
    let torso_y1 = top + ah * 0.60;
    let tx0 = cx - aw;
    let tx1 = cx + aw;
    c.rrect(tx0, sh_y, tx1, torso_y1, aw * 0.45, bright);
    // Chest control panel + dial.
    c.rrect(
        cx - aw * 0.52,
        top + ah * 0.36,
        cx + aw * 0.52,
        top + ah * 0.52,
        aw * 0.14,
        mid,
    );
    c.ring(cx + aw * 0.24, top + ah * 0.44, aw * 0.13, mid);

    // Left arm hanging beside the torso.
    let arm_w = aw * 0.42;
    c.rrect(
        tx0 - arm_w,
        sh_y + ah * 0.02,
        tx0 + arm_w * 0.15,
        torso_y1 - ah * 0.02,
        arm_w * 0.5,
        bright,
    );

    // Legs (two rounded boxes) + a hint of boots.
    let gap = aw * 0.16;
    let leg_w = aw * 0.72;
    c.rrect(
        cx - gap - leg_w,
        torso_y1 - ah * 0.02,
        cx - gap,
        feet_y,
        leg_w * 0.34,
        bright,
    );
    c.rrect(
        cx + gap,
        torso_y1 - ah * 0.02,
        cx + gap + leg_w,
        feet_y,
        leg_w * 0.34,
        bright,
    );
    c.line(
        cx - gap - leg_w,
        feet_y,
        cx - gap - leg_w * 0.3,
        feet_y,
        bright,
    );
    c.line(
        cx + gap + leg_w * 0.3,
        feet_y,
        cx + gap + leg_w,
        feet_y,
        bright,
    );

    // Flag pole to the right + right arm reaching to grip it.
    let pole_x = cx + aw * 2.05;
    let pole_top = top - ah * 0.06;
    let grip_y = top + ah * 0.30;
    c.line(pole_x, feet_y + ah * 0.02, pole_x, pole_top, bright);
    c.plotf(pole_x, pole_top - 1.0, mid); // finial
                                          // Right arm: shoulder → grip on the pole.
    c.rrect(
        tx1 - arm_w * 0.15,
        sh_y + ah * 0.03,
        pole_x,
        sh_y + ah * 0.03 + arm_w,
        arm_w * 0.42,
        bright,
    );

    // Flag: a stippled field flying from the pole top.
    let fx0 = pole_x + 2.0;
    let fx1 = pole_x + aw * 1.7;
    let fy0 = pole_top + ah * 0.02;
    let fy1 = fy0 + ah * 0.15;
    c.rrect(fx0, fy0, fx1, fy1, 0.0, mid);
    c.stipple_rect(fx0 + 1.0, fy0 + 1.0, fx1 - 1.0, fy1 - 1.0, 3, 2);
}

/// Build one full frame (braille rows + the loading bar) for a `cols`×`rows` terminal. `phase`
/// advances the dune motion (0 = still); the screensaver can animate by increasing it.
pub fn frame(cols: usize, rows: usize, phase: f64) -> String {
    let cols = cols.clamp(48, 160);
    let art_rows = rows.saturating_sub(3).clamp(16, 44);
    let w = cols * 2;
    let h = art_rows * 4;
    let (wf, hf) = (w as f64, h as f64);
    let mut c = Canvas::new(w, h);

    let horizon = hf * 0.58;

    // ── Moon: a dotted halftone disc, top-left ──────────────────────────────
    let moon = (wf * 0.15, hf * 0.20, hf * 0.135);
    c.stipple_disc(moon.0, moon.1, moon.2, 3, 1);
    c.ring(moon.0, moon.1, moon.2, 1);

    // ── Ringed planet, top-right ────────────────────────────────────────────
    let planet = (wf * 0.87, hf * 0.16);
    let pr = hf * 0.052;
    c.ring(planet.0, planet.1, pr, 1);
    c.ellipse(planet.0, planet.1, pr * 2.3, pr * 0.55, 1);

    // ── Starfield (deterministic, avoids the moon/planet) ───────────────────
    let sky_h = horizon * 0.95;
    let count = (wf * sky_h / 1500.0) as u64;
    for i in 0..count {
        let hx = (hash(i * 3) % w as u64) as f64;
        let hy = (hash(i * 3 + 1) % (sky_h as u64).max(1)) as f64;
        let near_moon = ((hx - moon.0).powi(2) + (hy - moon.1).powi(2)).sqrt() < moon.2 + 4.0;
        let near_planet = ((hx - planet.0).powi(2) + (hy - planet.1).powi(2)).sqrt() < pr * 2.6;
        if near_moon || near_planet {
            continue;
        }
        match hash(i * 3 + 2) % 5 {
            0 | 1 => {
                // a '+' sparkle
                c.plotf(hx, hy, 3);
                c.plotf(hx - 2.0, hy, 2);
                c.plotf(hx + 2.0, hy, 2);
                c.plotf(hx, hy - 2.0, 2);
                c.plotf(hx, hy + 2.0, 2);
            }
            2 => c.plotf(hx, hy, 2),
            _ => c.plotf(hx, hy, 1),
        }
    }

    // ── Mountains: two ridge layers (back faint, front brighter) ────────────
    for (layer, (base, amp, lvl)) in [
        (horizon * 0.86, hf * 0.10, 1u8),
        (horizon * 0.99, hf * 0.15, 2u8),
    ]
    .into_iter()
    .enumerate()
    {
        let seed = layer as f64 * 11.0;
        let mut prev: Option<(f64, f64)> = None;
        let mut x = 0.0;
        while x <= wf {
            let n = (x * 0.035 + seed).sin() * 0.5
                + (x * 0.017 + seed * 2.0).sin() * 0.3
                + (x * 0.008 + seed).sin() * 0.2;
            let peak = (x * 0.06 + seed).sin().abs().powf(1.8);
            let y = base - (n * 0.5 + peak) * amp;
            if let Some((px, py)) = prev {
                c.line(px, py, x, y, lvl);
            }
            prev = Some((x, y));
            x += 1.0;
        }
    }

    // ── Dunes: flowing horizontal contour lines from horizon to the floor ───
    let bands = 9;
    for k in 0..bands {
        let t = k as f64 / (bands as f64 - 1.0);
        let base = horizon + (hf - horizon) * t.powf(1.35);
        let amp = hf * (0.010 + 0.045 * t);
        let lvl = if t < 0.35 {
            1
        } else if t < 0.7 {
            2
        } else {
            3
        };
        let ph = phase + k as f64 * 0.6;
        let mut prev: Option<(f64, f64)> = None;
        let mut x = 0.0;
        while x <= wf {
            let y = base + (x * 0.03 + ph).sin() * amp + (x * 0.013 - ph * 0.7).sin() * amp * 0.6;
            if let Some((px, py)) = prev {
                c.line(px, py, x, y, lvl);
            }
            prev = Some((x, y));
            x += 1.0;
        }
    }

    // ── Astronaut, standing on a near dune ──────────────────────────────────
    let feet_y = hf * 0.72;
    let ah = hf * 0.46;
    astronaut(&mut c, wf * 0.40, feet_y, ah);

    // ── Assemble: braille rows + a loading bar ──────────────────────────────
    let mut out = String::new();
    for line in c.to_lines() {
        out.push_str(&line);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&loading_bar(cols));
    out.push('\n');
    out
}

/// The `[ ■■■□ ] Loading modules…` bar, centred under the scene.
fn loading_bar(cols: usize) -> String {
    let total = 14usize;
    let filled = 8usize;
    let mut bar = String::new();
    let _ = write!(bar, "{}", style("[ ").color256(LEVELS[1]));
    for _ in 0..filled {
        let _ = write!(bar, "{}", style('■').color256(LEVELS[4]));
    }
    for _ in 0..(total - filled) {
        let _ = write!(bar, "{}", style('□').color256(LEVELS[1]));
    }
    let _ = write!(bar, "{}", style(" ] ").color256(LEVELS[1]));
    let _ = write!(bar, "{}", style("Loading modules…").color256(LEVELS[3]));

    let visible = 2 + total + 3 + "Loading modules…".chars().count();
    let pad = cols.saturating_sub(visible) / 2;
    format!("{}{}", " ".repeat(pad), bar)
}

/// The moonscape as a real raster image, sixel-encoded, baked into the binary at build time.
/// Generated offline by `cargo run --example gen_moon_sixel -- <png>`; regenerate that file to
/// change the picture. Kept as one embedded string so the binary stays a single self-contained file.
const MOON_SIXEL: &str = include_str!("moon.sixel");

/// `aizen art` — show the moonscape. A true raster image on sixel-capable terminals (Windows
/// Terminal, WezTerm, iTerm2, foot, …); the braille [`frame`] fallback everywhere else.
pub fn run() {
    if super::splash::logo_is_sixel() {
        // Centre the image roughly, then the loading bar beneath it.
        let (_, cols) = Term::stdout().size();
        println!();
        print!("{MOON_SIXEL}");
        println!();
        println!("{}", loading_bar(cols as usize));
    } else {
        let (rows, cols) = Term::stdout().size();
        print!("{}", frame(cols as usize, rows as usize, 0.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_is_nonempty_and_has_bar() {
        let f = frame(100, 30, 0.0);
        assert!(f.contains("Loading modules"), "loading bar present");
        assert!(
            f.lines().count() > 10,
            "several braille rows: {}",
            f.lines().count()
        );
    }

    #[test]
    fn canvas_plot_keeps_brightest() {
        let mut c = Canvas::new(4, 4);
        c.plot(0, 0, 1);
        c.plot(0, 0, 3);
        c.plot(0, 0, 2);
        assert_eq!(c.px[0], 4, "stores level 3 (+1), the brightest write");
    }
}
