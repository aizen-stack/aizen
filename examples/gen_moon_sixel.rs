//! Offline asset builder: decode the moonscape PNG, downscale it, quantise to a few silver
//! grey levels, and emit a sixel payload to `src/ui/moon.sixel`. That file is then `include_str!`'d
//! into the binary (still one self-contained file — no runtime asset). Run with:
//!
//!   cargo run --example gen_moon_sixel -- <input.png> [target_width]
//!
//! Uses only the `png` crate (already a dependency) so there is no new C toolchain requirement.

use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;

/// Number of grey levels above the transparent background.
const LEVELS: usize = 6;
/// Luminance (0..255) below which a pixel is left transparent (terminal background shows through).
const BLACK_CUTOFF: f32 = 22.0;

fn main() {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .expect("usage: gen_moon_sixel <input.png> [target_width]");
    let target_w: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(760);

    // ── Decode the PNG (expand palette/low-bit, strip 16-bit down to 8) ──────
    let decoder = {
        let mut d = png::Decoder::new(File::open(&input).expect("open input png"));
        d.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        d
    };
    let mut reader = decoder.read_info().expect("read png header");
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("decode png frame");
    let (sw, sh) = (info.width as usize, info.height as usize);
    let ch = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => 3, // EXPAND turns this into RGB
    };
    eprintln!("source {sw}x{sh}, {ch} channels ({:?})", info.color_type);

    // Per-source-pixel luminance premultiplied by coverage (alpha).
    let lum_at = |x: usize, y: usize| -> (f32, f32) {
        let i = (y * sw + x) * ch;
        let (l, a) = match ch {
            1 => (buf[i] as f32, 255.0),
            2 => (buf[i] as f32, buf[i + 1] as f32),
            3 => (
                0.299 * buf[i] as f32 + 0.587 * buf[i + 1] as f32 + 0.114 * buf[i + 2] as f32,
                255.0,
            ),
            _ => (
                0.299 * buf[i] as f32 + 0.587 * buf[i + 1] as f32 + 0.114 * buf[i + 2] as f32,
                buf[i + 3] as f32,
            ),
        };
        (l, a / 255.0)
    };

    // ── Downscale by box-averaging, keeping the 16:9-ish aspect ──────────────
    let target_w = target_w.min(sw).max(64);
    let target_h = ((target_w as f32) * (sh as f32) / (sw as f32)).round() as usize;
    let mut grid = vec![0u8; target_w * target_h]; // 0 = transparent, 1..=LEVELS grey
    for ty in 0..target_h {
        let sy0 = ty * sh / target_h;
        let sy1 = ((ty + 1) * sh / target_h).max(sy0 + 1).min(sh);
        for tx in 0..target_w {
            let sx0 = tx * sw / target_w;
            let sx1 = ((tx + 1) * sw / target_w).max(sx0 + 1).min(sw);
            let (mut lsum, mut n) = (0.0f32, 0.0f32);
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let (l, a) = lum_at(sx, sy);
                    lsum += l * a; // coverage-weighted; transparent black stays dark
                    n += 1.0;
                }
            }
            let lum = if n > 0.0 { lsum / n } else { 0.0 };
            grid[ty * target_w + tx] = if lum < BLACK_CUTOFF {
                0
            } else {
                // Map [cutoff,255] → 1..=LEVELS with a mild gamma lift so faint dots survive.
                let t = ((lum - BLACK_CUTOFF) / (255.0 - BLACK_CUTOFF))
                    .clamp(0.0, 1.0)
                    .powf(0.82);
                ((t * LEVELS as f32).ceil() as usize).clamp(1, LEVELS) as u8
            };
        }
    }

    // ── Emit sixel ───────────────────────────────────────────────────────────
    let mut s = String::from("\x1bP0;1;0q"); // P2=1 ⇒ 0-bits stay transparent
    s.push_str(&format!("\"1;1;{target_w};{target_h}")); // 1:1 pixel aspect + image size
                                                         // Register LEVELS silver-tinted greys (sixel RGB is 0..100). Brightest ≈ moonlight #d2d4d9.
    for lvl in 1..=LEVELS {
        let t = lvl as f32 / LEVELS as f32;
        let r = (82.0 * t + 4.0).round() as u32;
        let g = (83.0 * t + 4.0).round() as u32;
        let b = (85.0 * t + 5.0).round() as u32;
        s.push_str(&format!("#{lvl};2;{r};{g};{b}"));
    }

    for band in (0..target_h).step_by(6) {
        for lvl in 1..=LEVELS as u8 {
            // Build this colour's sixel row across the whole width.
            let mut row = String::new();
            let (mut run_ch, mut run_n) = ('\u{0}', 0usize);
            let mut any = false;
            for x in 0..target_w {
                let mut bits = 0u8;
                for r in 0..6 {
                    let y = band + r;
                    if y < target_h && grid[y * target_w + x] == lvl {
                        bits |= 1 << r;
                    }
                }
                if bits != 0 {
                    any = true;
                }
                let c = (0x3F + bits as u32) as u8 as char;
                if run_n > 0 && c == run_ch {
                    run_n += 1;
                } else {
                    rle(&mut row, run_ch, run_n);
                    run_ch = c;
                    run_n = 1;
                }
            }
            rle(&mut row, run_ch, run_n);
            if any {
                s.push('#');
                s.push_str(&lvl.to_string());
                s.push_str(&row);
                s.push('$'); // carriage return: overlay the next colour on the same band
            }
        }
        s.push('-'); // graphics newline: next band
    }
    s.push_str("\x1b\\"); // ST — end the DCS

    let out: PathBuf = ["src", "ui", "moon.sixel"].iter().collect();
    File::create(&out).unwrap().write_all(s.as_bytes()).unwrap();
    eprintln!(
        "wrote {} ({} bytes) — {target_w}x{target_h}, {LEVELS} levels",
        out.display(),
        s.len()
    );
}

fn rle(out: &mut String, ch: char, n: usize) {
    if n == 0 {
        return;
    }
    if n >= 4 {
        out.push('!');
        out.push_str(&n.to_string());
        out.push(ch);
    } else {
        for _ in 0..n {
            out.push(ch);
        }
    }
}
