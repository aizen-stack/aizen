//! Baked colour feature-cards — the six illustrated panels that describe what Aizen is. Unlike the
//! silver-grey `moonscape`, these keep their ORIGINAL colours. The PNGs are `include_bytes!`'d and
//! re-encoded to sixel AT RUNTIME to the terminal's exact pixel size (cover-crop fullscreen), so the
//! image fills any window without upscaling a fixed raster. The binary stays one self-contained file
//! (the PNGs live in `.rodata`; nothing is read from disk at runtime). Two surfaces consume them:
//!
//!   * startup — bare `aizen` shows ONE card, rotating to the next on each launch (a persisted
//!     counter under `~/.aizen`), so the landing screen feels alive without a wall of images.
//!   * screensaver — after 15s idle in the sticky TUI, ONE card fills the alt-screen, cleared on the
//!     next keystroke.
//!
//! The index-picking (which card) is cheap and lives wherever the surface is triggered; the sixel
//! ENCODE is done by the render thread, which owns the terminal and knows its pixel geometry.

use crate::core::config::nextgen_home;
use std::sync::atomic::{AtomicUsize, Ordering};

/// One baked card: a human title (for a caption / debug) and its PNG bytes (decoded on demand).
pub struct Card {
    pub title: &'static str,
    pub png: &'static [u8],
}

/// The six feature cards, in a stable order (rotation walks this array). Keep new cards alphabetised
/// by slug so the rotation stays stable across releases. The index of each card is ALSO a semantic
/// slot referenced by [`card_for_tool`] — reordering here means updating that map.
pub const CARDS: &[Card] = &[
    Card {
        title: "A layered identity",
        png: include_bytes!("cards/a-layered-identity.png"),
    },
    Card {
        title: "A memory that learns you",
        png: include_bytes!("cards/a-memory-that-learns-you.png"),
    },
    Card {
        title: "Delegate to sub-agents",
        png: include_bytes!("cards/delegate-to-sub-agents.png"),
    },
    Card {
        title: "Lives everywhere",
        png: include_bytes!("cards/lives-everywhere.png"),
    },
    Card {
        title: "Researches the web",
        png: include_bytes!("cards/researches-the-web.png"),
    },
    Card {
        title: "Safe autonomy",
        png: include_bytes!("cards/safe-autonomy.png"),
    },
];

/// The card whose feature the session most recently exercised (a sub-agent spawn, a memory write, a
/// web search…). `usize::MAX` = "nothing yet, fall back to rotation". Set from the tool-result hook
/// on the agent thread, read by the screensaver on the input thread — a plain atomic is enough (no
/// ordering coupling to other state, last-writer-wins is exactly what we want).
static CONTEXT_CARD: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Map a tool name to the feature card it illustrates, so the idle screensaver can reflect what the
/// user just did instead of a random slide. Returns `None` for tools with no card (most of them) —
/// those simply don't move the context pointer. The indices match [`CARDS`] above.
pub fn card_for_tool(tool: &str) -> Option<usize> {
    Some(match tool {
        // Delegation → "Delegate to sub-agents"
        "task" | "workflow" => 2,
        // Memory writes/reads → "A memory that learns you"
        "memory_search" | "memory_profile" | "memory_ask" | "memory_write"
        | "memory_consolidate" => 1,
        // Web research → "Researches the web"
        "web_search" | "web_fetch" | "web_crawl" => 4,
        // Identity/persona layers → "A layered identity"
        "persona_create" => 0,
        // Cross-surface presence → "Lives everywhere"
        "telegram_send" | "telegram_ask" | "bot_admin" | "notify" => 3,
        _ => return None,
    })
}

/// Record that a tool just ran (successfully), pointing the screensaver's context card at the feature
/// it illustrates. A no-op for tools with no card. Cheap enough to call from the tool-result hook on
/// every call. "Safe autonomy" (card 5) is NOT wired here — it's driven by the approval prompt (an
/// explicit gate), set via [`note_approval`], not by any tool completing.
pub fn note_tool_activity(tool: &str) {
    if let Some(idx) = card_for_tool(tool) {
        CONTEXT_CARD.store(idx, Ordering::Relaxed);
    }
}

/// Point the context card at "Safe autonomy" — called when a risky action raises an approval prompt,
/// so an idle screensaver right after reflects the guardrail the user just saw.
pub fn note_approval() {
    CONTEXT_CARD.store(5, Ordering::Relaxed);
}

/// The card's human title, for the screensaver caption. `None` if the index is out of range.
pub fn card_title(idx: usize) -> Option<&'static str> {
    CARDS.get(idx).map(|c| c.title)
}

/// Max palette colours per encoded card. Sixel supports 256 registers; 128 keeps the illustrations
/// faithful while holding the payload down.
const PALETTE: usize = 128;
/// Alpha (0..255) below which a pixel is left transparent (terminal background shows through).
const ALPHA_CUTOFF: u8 = 24;

/// Where the rotation counter is persisted, so a fresh launch shows the NEXT card (not always the
/// first). A tiny plaintext file next to the CLI config; a missing/corrupt file just starts at 0.
fn rotation_path() -> std::path::PathBuf {
    nextgen_home().join(".card-rotation")
}

/// Index of the card to show at startup, advancing the persisted rotation by one. `None` when no
/// cards are baked in (defensive — the array is non-empty in practice). Best-effort persistence: a
/// read-only home just replays the same card rather than blocking the launch.
pub fn next_startup_card() -> Option<usize> {
    if CARDS.is_empty() {
        return None;
    }
    let prev = std::fs::read_to_string(rotation_path())
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let idx = prev % CARDS.len();
    let next = (idx + 1) % CARDS.len();
    let _ = crate::core::persist::atomic_write(&rotation_path(), next.to_string().as_bytes());
    Some(idx)
}

/// The card the idle screensaver should show. Prefers the CONTEXT card — the feature the session most
/// recently exercised (a sub-agent spawn, a web search, an approval prompt…) — so the image reflects
/// what the user just did. With nothing exercised yet, falls back to a stable per-session rotation
/// pick (so a session that never touches a wired tool still shows a consistent image, not a flicker).
pub fn screensaver_card() -> Option<usize> {
    if CARDS.is_empty() {
        return None;
    }
    let ctx = CONTEXT_CARD.load(Ordering::Relaxed);
    if ctx != usize::MAX && ctx < CARDS.len() {
        return Some(ctx);
    }
    static PICK: AtomicUsize = AtomicUsize::new(usize::MAX);
    let cur = PICK.load(Ordering::Relaxed);
    let idx = if cur == usize::MAX {
        let pick = std::process::id() as usize % CARDS.len();
        PICK.store(pick, Ordering::Relaxed);
        pick
    } else {
        cur
    };
    Some(idx)
}

/// Encode card `idx` as a sixel that COVER-fills a `px_w`×`px_h` pixel viewport: the square source is
/// cropped to the viewport's aspect (excess top/bottom or left/right trimmed), then scaled to the
/// exact pixel size and quantised to a per-image palette. Returns `None` if the index is out of range,
/// the viewport is degenerate, or the PNG fails to decode. Cost is one-time (the render thread caches
/// the result and only re-encodes when the terminal size changes).
pub fn render_cover_sixel(idx: usize, px_w: u32, px_h: u32) -> Option<String> {
    if px_w == 0 || px_h == 0 {
        return None;
    }
    let card = CARDS.get(idx)?;
    let (src, sw, sh) = decode_rgba(card.png)?;
    if sw == 0 || sh == 0 {
        return None;
    }

    // ── Cover crop: pick the largest centred source rect matching the target aspect ──
    let (tw, th) = (px_w as u64, px_h as u64);
    let (sw64, sh64) = (sw as u64, sh as u64);
    // Compare aspect ratios via cross-multiplication (no float): src_w/src_h vs tw/th.
    let (crop_w, crop_h) = if sw64 * th > tw * sh64 {
        // Source is wider than target → crop the sides. crop_h = full height.
        ((sh64 * tw / th) as usize, sh as usize)
    } else {
        // Source is taller (or equal) → crop top/bottom. crop_w = full width.
        (sw as usize, (sw64 * th / tw) as usize)
    };
    let crop_w = crop_w.clamp(1, sw as usize);
    let crop_h = crop_h.clamp(1, sh as usize);
    let off_x = (sw as usize - crop_w) / 2;
    let off_y = (sh as usize - crop_h) / 2;

    // ── Scale the crop to the exact target pixel grid (nearest — we're upscaling) ──
    let (tw_us, th_us) = (px_w as usize, px_h as usize);
    let mut px = vec![[0u8; 4]; tw_us * th_us];
    for ty in 0..th_us {
        let sy = off_y + (ty * crop_h) / th_us;
        for tx in 0..tw_us {
            let sx = off_x + (tx * crop_w) / tw_us;
            px[ty * tw_us + tx] =
                src[sy.min(sh as usize - 1) * sw as usize + sx.min(sw as usize - 1)];
        }
    }

    // ── Palette over the opaque pixels (median cut) ──
    let opaque: Vec<[u8; 3]> = px
        .iter()
        .filter(|p| p[3] >= ALPHA_CUTOFF)
        .map(|p| [p[0], p[1], p[2]])
        .collect();
    let palette = median_cut(&opaque, PALETTE);

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
    let mut grid = vec![usize::MAX; tw_us * th_us];
    for (i, p) in px.iter().enumerate() {
        if p[3] >= ALPHA_CUTOFF {
            grid[i] = idx_of([p[0], p[1], p[2]]);
        }
    }

    // ── Emit sixel ──
    let mut s = String::from("\x1bP0;1;0q"); // P2=1 ⇒ 0-bits stay transparent
    s.push_str(&format!("\"1;1;{tw_us};{th_us}")); // 1:1 pixel aspect + image size
    for (i, &c) in palette.iter().enumerate() {
        let r = (c[0] as u32 * 100 + 127) / 255; // sixel RGB is 0..100 (round to nearest)
        let g = (c[1] as u32 * 100 + 127) / 255;
        let b = (c[2] as u32 * 100 + 127) / 255;
        s.push_str(&format!("#{i};2;{r};{g};{b}"));
    }
    for band in (0..th_us).step_by(6) {
        let mut used = vec![false; palette.len()];
        for x in 0..tw_us {
            for row in 0..6 {
                let y = band + row;
                if y < th_us {
                    let gi = grid[y * tw_us + x];
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
            for x in 0..tw_us {
                let mut bits = 0u8;
                for r in 0..6 {
                    let y = band + r;
                    if y < th_us && grid[y * tw_us + x] == ci {
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
    Some(s)
}

/// Decode a PNG to a flat RGBA buffer. Returns `(pixels, width, height)` or `None` on any decode
/// error. Expands palette/low-bit and strips 16-bit down to 8, so `ch` is 1/2/3/4 as below.
fn decode_rgba(bytes: &[u8]) -> Option<(Vec<[u8; 4]>, u32, u32)> {
    let mut d = png::Decoder::new(bytes);
    d.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = d.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let ch = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => 3, // EXPAND turns this into RGB
    };
    let mut px = vec![[0u8; 4]; w * h];
    for (i, out) in px.iter_mut().enumerate() {
        let j = i * ch;
        *out = match ch {
            1 => [buf[j], buf[j], buf[j], 255],
            2 => [buf[j], buf[j], buf[j], buf[j + 1]],
            3 => [buf[j], buf[j + 1], buf[j + 2], 255],
            _ => [buf[j], buf[j + 1], buf[j + 2], buf[j + 3]],
        };
    }
    Some((px, info.width, info.height))
}

/// Median-cut colour quantisation: recursively split the colour cube along its widest channel until
/// we have `want` boxes, then average each box. Subsamples large inputs to bound the cost.
fn median_cut(pixels: &[[u8; 3]], want: usize) -> Vec<[u8; 3]> {
    if pixels.is_empty() {
        return vec![[0, 0, 0]];
    }
    let step = (pixels.len() / 40_000).max(1);
    let sample: Vec<[u8; 3]> = pixels.iter().step_by(step).cloned().collect();
    let mut boxes: Vec<Vec<[u8; 3]>> = vec![sample];
    while boxes.len() < want {
        let mut target: Option<(usize, usize)> = None;
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
            None => break,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_card_png_decodes() {
        for c in CARDS {
            let decoded = decode_rgba(c.png);
            assert!(decoded.is_some(), "{} must decode as PNG", c.title);
            let (px, w, h) = decoded.unwrap();
            assert!(
                w > 0 && h > 0 && px.len() == (w * h) as usize,
                "{} bad dims",
                c.title
            );
            assert!(!c.title.is_empty());
        }
    }

    #[test]
    fn cover_sixel_is_a_dcs_and_declares_target_size() {
        // A wide viewport → the square card is cropped top/bottom; the payload must be a sixel DCS
        // that declares the exact target pixel size we asked for.
        let s = render_cover_sixel(0, 200, 100).expect("encode");
        assert!(s.starts_with("\x1bP"), "must open a sixel DCS");
        assert!(s.ends_with("\x1b\\"), "must close with ST");
        assert!(
            s.contains("\"1;1;200;100"),
            "must declare 200x100 raster attributes"
        );
    }

    #[test]
    fn degenerate_viewport_and_bad_index_yield_none() {
        assert!(render_cover_sixel(0, 0, 100).is_none());
        assert!(render_cover_sixel(0, 100, 0).is_none());
        assert!(render_cover_sixel(CARDS.len(), 100, 100).is_none());
    }

    #[test]
    fn screensaver_pick_is_stable_within_session() {
        let a = screensaver_card();
        let b = screensaver_card();
        assert_eq!(a, b, "screensaver card must not change within a session");
    }

    #[test]
    fn tool_to_card_maps_features_and_titles_resolve() {
        // Each wired tool points at a real card whose title names the matching feature. Pure map —
        // deliberately NOT `note_tool_activity`, which would mutate the shared CONTEXT_CARD atomic
        // and race the stability test above.
        let cases = [
            ("task", "Delegate"),
            ("workflow", "Delegate"),
            ("memory_search", "memory"),
            ("web_search", "web"),
            ("web_fetch", "web"),
            ("persona_create", "layered identity"),
            ("telegram_send", "everywhere"),
        ];
        for (tool, needle) in cases {
            let idx = card_for_tool(tool).unwrap_or_else(|| panic!("{tool} must map to a card"));
            let title = card_title(idx).expect("mapped index must have a title");
            assert!(
                title.contains(needle),
                "{tool} → {title:?} should mention {needle:?}"
            );
        }
        // An unwired tool moves nothing.
        assert!(card_for_tool("file_read").is_none());
        // Out-of-range title is None.
        assert!(card_title(CARDS.len()).is_none());
    }
}
