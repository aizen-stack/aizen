//! Offline asset builder: decode the colour feature-card PNGs, downscale them, quantise each to a
//! per-image palette (median cut), and emit one sixel payload per card into `src/ui/cards/<slug>.sixel`.
//! Those files are then `include_str!`'d into the binary (still self-contained — no runtime asset).
//! Unlike `gen_moon_sixel` (6 silver greys, monochrome moonscape) this keeps the ORIGINAL colours, so
//! the cards read as the real illustrations. Run with:
//!
//!   cargo run --example gen_card_sixels -- <input_dir> [target_width]
//!
//! Every `*.png` in <input_dir> becomes `src/ui/cards/<slug>.sixel`, the slug derived from the file
//! stem. Uses only the `png` crate (already a dependency) — no new C toolchain requirement.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Max palette colours per card. Sixel supports 256 registers; 128 keeps the illustrations faithful
/// while holding the baked payload down.
const PALETTE: usize = 128;
/// Alpha (0..255) below which a pixel is left transparent (terminal background shows through).
const ALPHA_CUTOFF: u8 = 24;

fn main() {
    let mut args = std::env::args().skip(1);
    let input_dir = args
        .next()
        .expect("usage: gen_card_sixels <input_dir> [target_width]");
    let target_w: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(420);

    let out_dir: PathBuf = ["src", "ui", "cards"].iter().collect();
    fs::create_dir_all(&out_dir).expect("create src/ui/cards");

    let mut entries: Vec<PathBuf> = fs::read_dir(&input_dir)
        .expect("read input dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case("png"))
                .unwrap_or(false)
        })
        .collect();
    entries.sort();
    if entries.is_empty() {
        panic!("no .png files in {input_dir}");
    }

    for path in &entries {
        let (sixel, w, h, colors) = encode_card(path, target_w);
        let slug = slugify(path.file_stem().and_then(|s| s.to_str()).unwrap_or("card"));
        let out = out_dir.join(format!("{slug}.sixel"));
        File::create(&out)
            .unwrap()
            .write_all(sixel.as_bytes())
            .unwrap();
        eprintln!(
            "wrote {} ({} bytes) — {w}x{h}, {colors} colours",
            out.display(),
            sixel.len()
        );
    }
}

/// `A layered identity` → `a-layered-identity`. ASCII-only, collapses runs of punctuation to one dash.
fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in s.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Decode → downscale → quantise → sixel-encode a single card. Returns (payload, w, h, palette_len).
fn encode_card(path: &Path, target_w: usize) -> (String, usize, usize, usize) {
    // ── Decode the PNG (expand palette/low-bit, strip 16-bit down to 8) ──────
    let decoder = {
        let mut d = png::Decoder::new(File::open(path).expect("open input png"));
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

    let rgba_at = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * sw + x) * ch;
        match ch {
            1 => [buf[i], buf[i], buf[i], 255],
            2 => [buf[i], buf[i], buf[i], buf[i + 1]],
            3 => [buf[i], buf[i + 1], buf[i + 2], 255],
            _ => [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]],
        }
    };

    // ── Downscale by box-averaging (coverage-weighted RGB so edges don't darken) ──
    let target_w = target_w.min(sw).max(64);
    let target_h = ((target_w as f32) * (sh as f32) / (sw as f32)).round() as usize;
    let mut px = vec![[0u8; 4]; target_w * target_h];
    for ty in 0..target_h {
        let sy0 = ty * sh / target_h;
        let sy1 = ((ty + 1) * sh / target_h).max(sy0 + 1).min(sh);
        for tx in 0..target_w {
            let sx0 = tx * sw / target_w;
            let sx1 = ((tx + 1) * sw / target_w).max(sx0 + 1).min(sw);
            let (mut r, mut g, mut b, mut a, mut n) = (0f32, 0f32, 0f32, 0f32, 0f32);
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let p = rgba_at(sx, sy);
                    let af = p[3] as f32 / 255.0;
                    r += p[0] as f32 * af; // premultiply so transparent pixels don't bleed colour
                    g += p[1] as f32 * af;
                    b += p[2] as f32 * af;
                    a += p[3] as f32;
                    n += 1.0;
                }
            }
            if n > 0.0 {
                let af = a / (n * 255.0);
                let (rr, gg, bb) = if af > 0.0 {
                    ((r / (n * af)), (g / (n * af)), (b / (n * af))) // un-premultiply
                } else {
                    (0.0, 0.0, 0.0)
                };
                px[ty * target_w + tx] = [
                    rr.round() as u8,
                    gg.round() as u8,
                    bb.round() as u8,
                    (a / n).round() as u8,
                ];
            }
        }
    }

    // ── Build a palette over the opaque pixels (median cut) ──────────────────
    let opaque: Vec<[u8; 3]> = px
        .iter()
        .filter(|p| p[3] >= ALPHA_CUTOFF)
        .map(|p| [p[0], p[1], p[2]])
        .collect();
    let palette = median_cut(&opaque, PALETTE);

    // Map each opaque pixel to its nearest palette index; transparent pixels stay `usize::MAX`.
    let idx_of = |c: [u8; 3]| -> usize {
        let mut best = 0usize;
        let mut bd = i64::MAX;
        for (i, &p) in palette.iter().enumerate() {
            let dr = c[0] as i64 - p[0] as i64;
            let dg = c[1] as i64 - p[1] as i64;
            let db = c[2] as i64 - p[2] as i64;
            let d = dr * dr + dg * dg + db * db;
            if d < bd {
                bd = d;
                best = i;
            }
        }
        best
    };
    let mut grid = vec![usize::MAX; target_w * target_h];
    for (i, p) in px.iter().enumerate() {
        if p[3] >= ALPHA_CUTOFF {
            grid[i] = idx_of([p[0], p[1], p[2]]);
        }
    }

    // ── Emit sixel ───────────────────────────────────────────────────────────
    let mut s = String::from("\x1bP0;1;0q"); // P2=1 ⇒ 0-bits stay transparent
    s.push_str(&format!("\"1;1;{target_w};{target_h}")); // 1:1 pixel aspect + image size
    for (i, &c) in palette.iter().enumerate() {
        // sixel RGB is 0..100 (round-to-nearest, not truncate)
        let r = (c[0] as u32 * 100 + 127) / 255;
        let g = (c[1] as u32 * 100 + 127) / 255;
        let b = (c[2] as u32 * 100 + 127) / 255;
        s.push_str(&format!("#{i};2;{r};{g};{b}"));
    }

    for band in (0..target_h).step_by(6) {
        // Which palette indices appear anywhere in this 6-row band? (skip the rest — most bands use
        // only a handful of the 128 colours, so this keeps the payload compact.)
        let mut used = vec![false; palette.len()];
        for x in 0..target_w {
            for row in 0..6 {
                let y = band + row;
                if y < target_h {
                    let gi = grid[y * target_w + x];
                    if gi != usize::MAX {
                        used[gi] = true;
                    }
                }
            }
        }
        for (ci, is_used) in used.iter().enumerate() {
            if !is_used {
                continue;
            }
            let mut row = String::new();
            let (mut run_ch, mut run_n) = ('\u{0}', 0usize);
            for x in 0..target_w {
                let mut bits = 0u8;
                for r in 0..6 {
                    let y = band + r;
                    if y < target_h && grid[y * target_w + x] == ci {
                        bits |= 1 << r;
                    }
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
            s.push('#');
            s.push_str(&ci.to_string());
            s.push_str(&row);
            s.push('$'); // carriage return: overlay the next colour on the same band
        }
        s.push('-'); // graphics newline: next band
    }
    s.push_str("\x1b\\"); // ST — end the DCS
    (s, target_w, target_h, palette.len())
}

/// Median-cut colour quantisation: recursively split the colour cube along its widest channel until
/// we have `want` boxes, then average each box. Subsamples very large inputs to bound offline cost.
fn median_cut(pixels: &[[u8; 3]], want: usize) -> Vec<[u8; 3]> {
    if pixels.is_empty() {
        return vec![[0, 0, 0]];
    }
    let step = (pixels.len() / 40_000).max(1);
    let sample: Vec<[u8; 3]> = pixels.iter().step_by(step).cloned().collect();
    let mut boxes: Vec<Vec<[u8; 3]>> = vec![sample];
    while boxes.len() < want {
        // Pick the splittable box with the largest single-channel spread.
        let mut target: Option<(usize, usize)> = None; // (box index, channel)
        let mut best_range = 0i32;
        for (i, bx) in boxes.iter().enumerate() {
            if bx.len() < 2 {
                continue;
            }
            let (ch, range) = channel_range(bx);
            if range > best_range {
                best_range = range;
                target = Some((i, ch));
            }
        }
        let (idx, ch) = match target {
            Some(v) => v,
            None => break, // every box is a single colour
        };
        let mut bx = boxes.swap_remove(idx);
        bx.sort_by_key(|p| p[ch]);
        let mid = bx.len() / 2;
        let hi = bx.split_off(mid);
        boxes.push(bx);
        boxes.push(hi);
    }
    boxes.iter().map(|bx| avg(bx)).collect()
}

/// (widest channel, its range) across a box of colours.
fn channel_range(bx: &[[u8; 3]]) -> (usize, i32) {
    let mut lo = [255i32; 3];
    let mut hi = [0i32; 3];
    for p in bx {
        for c in 0..3 {
            lo[c] = lo[c].min(p[c] as i32);
            hi[c] = hi[c].max(p[c] as i32);
        }
    }
    let mut best = 0usize;
    let mut best_range = -1i32;
    for c in 0..3 {
        let range = hi[c] - lo[c];
        if range > best_range {
            best_range = range;
            best = c;
        }
    }
    (best, best_range)
}

fn avg(bx: &[[u8; 3]]) -> [u8; 3] {
    if bx.is_empty() {
        return [0, 0, 0];
    }
    let (mut r, mut g, mut b) = (0u64, 0u64, 0u64);
    for p in bx {
        r += p[0] as u64;
        g += p[1] as u64;
        b += p[2] as u64;
    }
    let n = bx.len() as u64;
    [(r / n) as u8, (g / n) as u8, (b / n) as u8]
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
