//! The `ng` landing splash — a hermes-style title + a bordered panel listing the agent's tools
//! and the CLI's commands. One cohesive gold accent (no rainbow), rich structure. Rendered once
//! when you open the interactive menu (bare `ng`). [`render`] builds it as a string (so the sticky
//! TUI can print it into its scroll region); [`print`] writes that string to stdout.

use crate::cli_config;
use console::{measure_text_width, style};
use std::fmt::Write as _;
use std::io::IsTerminal as _;

/// Silver gradient for the 5 title rows (near-white → darker grey), 256-color. The block-art logo
/// stays metallic silver — a deliberate contrast against the gold accent (classic silver+gold pair).
const TITLE: [u8; 5] = [255, 253, 251, 248, 245];
/// The single brand accent — now the warm gold from [`crate::theme`] (the "Studio" identity), shared
/// across the whole TUI so borders / headers / item names / the input box all speak one colour.
pub const ACCENT: u8 = crate::theme::ACCENT;
const INNER: usize = 78;

/// Agent tools, grouped (mirrors `agent::builtin` — keep in sync). Conditional/dynamic tools
/// (telegram_* when configured, mcp_<server>_<tool> from ~/.aizen/mcp.json) are not counted here.
const TOOL_GROUPS: &[(&str, &str)] = &[
    ("memory", "memory_search, memory_profile, memory_ask"),
    ("skills", "skill_load, skill_save, skill_search, skill_install"),
    ("files", "file_read, file_glob, file_edit, multi_edit, search_files"),
    ("shell", "shell_run, process"),
    ("web", "web_search, web_fetch, web_crawl"),
    ("tasks", "todo_write, clarify"),
    ("persona", "persona_create"),
    ("delegate", "task"),
];
/// Browser tools exist only under `--features browser`; listed (and counted) when present.
#[cfg(feature = "browser")]
const BROWSER_TOOLS: &str = "browser_navigate, browser_snapshot, browser_click, browser_type, browser_eval";
#[cfg(feature = "browser")]
const BROWSER_EXTRA: usize = 5;
#[cfg(not(feature = "browser"))]
const BROWSER_EXTRA: usize = 0;
const TOOL_COUNT: usize = 21 + BROWSER_EXTRA;

/// Top-level commands (mirrors `Commands` in main.rs, in declaration order).
const COMMANDS: &str =
    "chat · agent · workflow · memory · skill · persona · soul · bench · config · models · crawl · serve · telegram · discord · time · cron · mcp · apps";
const COMMAND_COUNT: usize = 18;

// ── block-art title ────────────────────────────────────────────────────────────

/// 5-row block glyphs (each exactly 5 columns) for the letters in AIZEN (+ NEXTGEN legacy set).
fn glyph(c: char) -> [&'static str; 5] {
    match c {
        'A' => [" ███ ", "█   █", "█████", "█   █", "█   █"],
        'I' => ["█████", "  █  ", "  █  ", "  █  ", "█████"],
        'Z' => ["█████", "   █ ", "  █  ", " █   ", "█████"],
        'N' => ["█   █", "██  █", "█ █ █", "█  ██", "█   █"],
        'E' => ["█████", "█    ", "████ ", "█    ", "█████"],
        'X' => ["█   █", " █ █ ", "  █  ", " █ █ ", "█   █"],
        'T' => ["█████", "  █  ", "  █  ", "  █  ", "  █  "],
        'G' => [" ████", "█    ", "█  ██", "█   █", " ████"],
        _ => ["     ", "     ", "     ", "     ", "     "],
    }
}

/// Fill the PetalMark into a `dw`×`dh` boolean pixel mask (true = petal). Resolution-independent, so
/// braille uses a tiny grid and sixel a large one: 16 long outer petals + 16 shorter inner petals
/// (offset half a step), each a tapered lance from the centre — the brand's chrysanthemum-sun.
fn petal_mask(dw: usize, dh: usize) -> Vec<bool> {
    use std::f64::consts::PI;
    let mut grid = vec![false; dw * dh];
    let c = (dw as f64 / 2.0, dh as f64 / 2.0);
    let r = (dw.min(dh) as f64) / 2.0 - 1.0;

    let sign = |a: (f64, f64), b: (f64, f64), p: (f64, f64)| (a.0 - p.0) * (b.1 - p.1) - (b.0 - p.0) * (a.1 - p.1);
    let in_tri = |p: (f64, f64), a: (f64, f64), b: (f64, f64), cc: (f64, f64)| {
        let (d1, d2, d3) = (sign(a, b, p), sign(b, cc, p), sign(cc, a, p));
        !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
    };

    // (petal count, length, half-width at the shoulder, angular offset in steps). Thin lances so the
    // DARK GAPS between adjacent rays read clearly. The long rays own the mid-radius (16 of them, wide
    // gaps); the short rays are small accents between them that END well before mid-radius, so the
    // middle stays an open rayed sun (not a dense crosshatch).
    let layers: [(usize, f64, f64, f64); 2] = [(16, r * 0.97, r * 0.026, 0.0), (16, r * 0.40, r * 0.018, 0.5)];
    for (count, len, halfw, off) in layers {
        for i in 0..count {
            let ang = 2.0 * PI * ((i as f64 + off) / count as f64);
            let (s, co) = ang.sin_cos();
            let (d, perp) = ((co, s), (-s, co));
            let base = c;
            let tip = (c.0 + d.0 * len, c.1 + d.1 * len);
            // widest near the middle → a thin lance/leaf tapering to points at BOTH the tip and the
            // centre (a clean radial convergence, no central disc/"eye").
            let sh = (c.0 + d.0 * 0.5 * len, c.1 + d.1 * 0.5 * len);
            let rs = (sh.0 + perp.0 * halfw, sh.1 + perp.1 * halfw);
            let ls = (sh.0 - perp.0 * halfw, sh.1 - perp.1 * halfw);
            // scan only the petal's bounding box (keeps the high-res sixel fill fast)
            let xs = [base.0, tip.0, rs.0, ls.0];
            let ys = [base.1, tip.1, rs.1, ls.1];
            let x0 = xs.iter().cloned().fold(f64::MAX, f64::min).floor().max(0.0) as usize;
            let x1 = (xs.iter().cloned().fold(f64::MIN, f64::max).ceil() as usize).min(dw - 1);
            let y0 = ys.iter().cloned().fold(f64::MAX, f64::min).floor().max(0.0) as usize;
            let y1 = (ys.iter().cloned().fold(f64::MIN, f64::max).ceil() as usize).min(dh - 1);
            for y in y0..=y1 {
                for x in x0..=x1 {
                    let p = (x as f64 + 0.5, y as f64 + 0.5);
                    if in_tri(p, base, rs, tip) || in_tri(p, base, tip, ls) {
                        grid[y * dw + x] = true;
                    }
                }
            }
        }
    }
    // Solid core: the petals converge to a mathematical point, so the pixels BETWEEN their thin bases
    // can be left unfilled — a transparent pinhole that reads as a central "eye". Fill a small disc so
    // the convergence is clean and solid (matches the reference: petals meeting at a filled centre).
    let core = r * 0.035;
    for y in 0..dh {
        for x in 0..dw {
            let (dx, dy) = (x as f64 + 0.5 - c.0, y as f64 + 0.5 - c.1);
            if dx * dx + dy * dy <= core * core {
                grid[y * dw + x] = true;
            }
        }
    }
    grid
}

/// The sun as Unicode-braille lines (2×4 dots/glyph) — the pure-text fallback when the terminal has
/// no inline-image support. Looks like a dotted approximation; sixel (below) is the smooth version.
fn sun_lines() -> Vec<String> {
    const DW: usize = 64;
    const DH: usize = 64;
    let grid = petal_mask(DW, DH);
    const BIT: [[u32; 4]; 2] = [[0, 1, 2, 6], [3, 4, 5, 7]];
    let mut lines = Vec::with_capacity(DH / 4);
    for cy in (0..DH).step_by(4) {
        let mut line = String::new();
        for cx in (0..DW).step_by(2) {
            let mut bits: u32 = 0;
            for dx in 0..2 {
                for dy in 0..4 {
                    if grid[(cy + dy) * DW + (cx + dx)] {
                        bits |= 1 << BIT[dx][dy];
                    }
                }
            }
            line.push(char::from_u32(0x2800 + bits).unwrap_or(' '));
        }
        lines.push(line);
    }
    lines
}

fn sixel_rle(out: &mut String, ch: char, n: usize) {
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

/// The sun as a TRUE raster image via the sixel protocol — smooth petals, gold, on a transparent
/// ground. This is the only way to show the actual vector logo in a terminal (Windows Terminal &
/// other sixel terminals). `petal_mask` rasterised at 200² then packed into 6-row sixel bands.
fn sun_sixel() -> String {
    const W: usize = 200;
    const H: usize = 200;
    let mask = petal_mask(W, H);
    let mut s = String::from("\x1bP0;1;0q"); // DCS … P2=1 ⇒ 0-bits stay transparent
    // Raster attributes "Pan;Pad;Ph;Pv = 1:1 pixel aspect (else P1=0 means 2:1 → the sun renders
    // stretched/oval) + the image size. Proven libsixel pattern; honoured by Windows Terminal et al.
    let _ = write!(s, "\"1;1;{W};{H}");
    s.push_str("#1;2;69;54;30"); // register colour 1 = brand gold #b0894c (sixel uses 0–100 RGB)
    for band in (0..H).step_by(6) {
        s.push_str("#1");
        let (mut run_ch, mut run_n) = ('\u{0}', 0usize);
        for x in 0..W {
            let mut bits = 0u8;
            for row in 0..6 {
                let y = band + row;
                if y < H && mask[y * W + x] {
                    bits |= 1 << row;
                }
            }
            let ch = (0x3F + bits as u32) as u8 as char;
            if run_n > 0 && ch == run_ch {
                run_n += 1;
            } else {
                sixel_rle(&mut s, run_ch, run_n);
                run_ch = ch;
                run_n = 1;
            }
        }
        sixel_rle(&mut s, run_ch, run_n);
        s.push('-'); // graphics newline (next band)
    }
    s.push_str("\x1b\\"); // ST — end the DCS
    s
}

/// Logo mode. `AIZEN_LOGO=sixel|braille` forces it. Otherwise auto-detect terminals KNOWN to render
/// sixel — cross-platform (Windows / Linux / macOS) — and fall back to braille everywhere else
/// (printing raw sixel on a terminal that can't read it would show escape-code garbage, so we only
/// emit it when confident). A DA1 capability probe would be more precise but needs raw-mode timed
/// stdin reads; this env heuristic covers the common cases with zero risk. Tests always use braille.
///
/// NOTE for Linux: the default **Ubuntu GNOME Terminal (VTE) does NOT support sixel**, so it gets the
/// braille fallback. Sixel-capable Linux terminals — **foot, WezTerm, mlterm, recent xterm, Konsole
/// ≥22.04, contour** — show the real image (auto-detected where they self-identify, else
/// `AIZEN_LOGO=sixel`). macOS: **iTerm2** yes; Terminal.app no.
fn logo_is_sixel() -> bool {
    if cfg!(test) {
        return false;
    }
    match std::env::var("AIZEN_LOGO").ok().as_deref() {
        Some("sixel") => return true,
        Some("braille") | Some("text") | Some("off") => return false,
        _ => {}
    }
    if !std::io::stdout().is_terminal() {
        return false;
    }
    if std::env::var_os("WT_SESSION").is_some() {
        return true; // Windows Terminal
    }
    if let Ok(tp) = std::env::var("TERM_PROGRAM") {
        if tp == "WezTerm" || tp == "iTerm.app" {
            return true;
        }
    }
    let term = std::env::var("TERM").unwrap_or_default();
    if term.starts_with("foot") || term == "mlterm" || term.contains("sixel") {
        return true;
    }
    // Konsole gained sixel in 22.04 → KONSOLE_VERSION like "220400".
    if let Ok(kv) = std::env::var("KONSOLE_VERSION") {
        if kv.trim().parse::<u32>().map(|v| v >= 220400).unwrap_or(false) {
            return true;
        }
    }
    false
}

/// Print the sun mark, centred over the wordmark: a real sixel image where supported, else braille.
fn push_sun(out: &mut String) {
    if logo_is_sixel() {
        out.push_str("       "); // nudge the image toward the wordmark's centre
        out.push_str(&sun_sixel());
        out.push('\n');
    } else {
        for line in sun_lines() {
            let _ = writeln!(out, " {}", style(&line).color256(ACCENT).bold());
        }
    }
}

/// Append the Aizen logo: the sun mark, then the block-art wordmark (silver gradient), then the
/// tagline. The sun + silver wordmark = the logo's noir lockup translated to the terminal.
fn push_title(out: &mut String, word: &str) {
    out.push('\n');
    push_sun(out);
    out.push('\n');
    for row in 0..5 {
        let line: String = word.chars().map(|c| glyph(c)[row]).collect::<Vec<_>>().join(" ");
        let _ = writeln!(out, "  {}", style(line).color256(TITLE[row]).bold());
    }
    let _ = writeln!(out, "  {}", style("ARTIFICIAL INTELLIGENCE AGENT").color256(crate::theme::MUTED));
    out.push('\n');
}

// ── bordered panel ───────────────────────────────────────────────────────────

fn rule(out: &mut String, left: &str, right: &str) {
    let _ = writeln!(out, "{}", style(format!("{left}{}{right}", "─".repeat(INNER + 2))).color256(ACCENT));
}

/// Append one boxed line, padding the (possibly styled) content to the inner width. Uses
/// `measure_text_width` so ANSI color codes don't throw off the alignment.
fn boxline(out: &mut String, content: &str) {
    let pad = INNER.saturating_sub(measure_text_width(content));
    let b = style("│").color256(ACCENT);
    let _ = writeln!(out, "{b} {content}{} {b}", " ".repeat(pad));
}

fn boxblank(out: &mut String) {
    boxline(out, "");
}

/// Pack `sep`-separated items into as few lines as possible, each (visible) no wider than `avail`.
/// Used for the Commands row so a long list wraps inside the box instead of spilling past the border.
fn wrap_items(items: &str, sep: &str, avail: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for item in items.split(sep) {
        let candidate = if cur.is_empty() { item.to_string() } else { format!("{cur}{sep}{item}") };
        if !cur.is_empty() && measure_text_width(&candidate) > avail {
            lines.push(std::mem::take(&mut cur));
            cur = item.to_string();
        } else {
            cur = candidate;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Build the whole landing screen (title + info/tools/commands panel) as a string.
pub fn render() -> String {
    let mut out = String::new();
    push_title(&mut out, "AIZEN");

    let cfg = cli_config::load();
    let model = cfg.model.as_deref().unwrap_or("(not set)");
    let endpoint = cfg.base_url.as_deref().unwrap_or("(not set)");
    let key = if cfg.api_key.is_some() {
        style("configured").color256(ACCENT).to_string()
    } else {
        style("not set").red().to_string()
    };

    rule(&mut out, "╭", "╮");
    boxline(
        &mut out,
        &format!(
            "{} {}",
            style("Aizen").color256(ACCENT).bold(),
            style(format!("v{} · {} · {}", env!("CARGO_PKG_VERSION"), model, endpoint)).dim()
        ),
    );
    boxline(&mut out, &format!("{} {key}", style("key:").dim()));
    boxblank(&mut out);

    boxline(
        &mut out,
        &format!("{}{}", crate::icons::g(crate::icons::hdr_tools()), style("Agent tools").color256(ACCENT).bold()),
    );
    for (label, items) in TOOL_GROUPS {
        boxline(
            &mut out,
            &format!(
                "  {}{} {}",
                crate::icons::g(crate::icons::tool_group(label)),
                style(format!("{label:<9}")).dim(),
                style(*items).color256(ACCENT)
            ),
        );
    }
    #[cfg(feature = "browser")]
    boxline(
        &mut out,
        &format!(
            "  {}{} {}",
            crate::icons::g(crate::icons::tool_group("browser")),
            style(format!("{:<9}", "browser")).dim(),
            style(BROWSER_TOOLS).color256(ACCENT)
        ),
    );
    boxblank(&mut out);

    boxline(
        &mut out,
        &format!("{}{}", crate::icons::g(crate::icons::hdr_commands()), style("Commands").color256(ACCENT).bold()),
    );
    // Indent is 2 cols → wrap to INNER-2 so styled lines never overrun the right border.
    for line in wrap_items(COMMANDS, " · ", INNER - 2) {
        boxline(&mut out, &format!("  {}", style(line).color256(ACCENT)));
    }
    boxblank(&mut out);

    boxline(
        &mut out,
        &format!(
            "{}",
            style(format!("{TOOL_COUNT} tools · {COMMAND_COUNT} commands · type to chat · /help · Esc/Ctrl-C to exit"))
                .dim()
        ),
    );
    rule(&mut out, "╰", "╯");
    out
}

/// Render the landing screen to stdout (plain line-REPL path).
pub fn print() {
    print!("{}", render());
}

/// First-run welcome banner: the block-art logo + a short value prop. Shown ONCE to a brand-new
/// user (no endpoint configured yet) right before the setup wizard — the "intro" before onboarding.
pub fn welcome() -> String {
    let mut out = String::new();
    push_title(&mut out, "AIZEN");
    let _ = writeln!(
        out,
        "  {}",
        style("Welcome to Aizen — your agentic coding companion.").color256(ACCENT).bold()
    );
    let _ = writeln!(
        out,
        "  {}",
        style("One fast binary: chat · tools · automation · a memory that learns you.").dim()
    );
    let _ = writeln!(out, "  {}", style("Let's get you connected — about 30 seconds.").dim());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The displayed counts must match the displayed lists — else the splash lies about its own
    /// surface. (Guards against editing one and forgetting the other.)
    #[test]
    fn counts_match_listed_names() {
        let tools: usize =
            TOOL_GROUPS.iter().map(|(_, items)| items.split(", ").count()).sum::<usize>() + BROWSER_EXTRA;
        assert_eq!(tools, TOOL_COUNT, "TOOL_COUNT out of sync with TOOL_GROUPS");
        assert_eq!(COMMANDS.split(" · ").count(), COMMAND_COUNT, "COMMAND_COUNT out of sync with COMMANDS");
        // Mirror of the top-level `Commands` enum in main.rs — every variant must be listed so a new
        // subcommand can't be added without surfacing on the splash (the `apps` omission regression).
        for c in [
            "chat", "agent", "workflow", "memory", "skill", "persona", "soul", "bench", "config",
            "models", "crawl", "serve", "telegram", "discord", "time", "cron", "mcp", "apps",
        ] {
            assert!(COMMANDS.split(" · ").any(|x| x == c), "command '{c}' missing from splash COMMANDS");
        }
    }

    /// Every bordered line must be exactly the box width — a wider one means content overran the
    /// right border (the Commands-row overflow regression).
    #[test]
    fn no_boxed_line_overflows() {
        let expected = INNER + 4; // "│ " + INNER + " │"
        for line in render().lines() {
            if line.contains('│') {
                assert_eq!(measure_text_width(line), expected, "boxed line overruns border: {line:?}");
            }
        }
    }
}
