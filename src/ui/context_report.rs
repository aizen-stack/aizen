//! What is in the context window, what it cost, and how full it is.
//!
//! Read-only reporting for `/tokens`, `/context` and `/cost`. The window size is a heuristic keyed
//! off the model name and is deliberately NOT a cap: the provider enforces the real limit, and this
//! only decides how honest the HUD looks. Guessing low would nag the user into compacting a
//! conversation that fits; guessing high would let the fill bar read green right up to a 400.

use crate::agent;
use crate::core::cli_config;
use crate::core::types::Message;
use crate::ui::{theme, tui};
use crate::*;

/// Approximate context window (tokens) for a model, by name pattern. A rough heuristic for the
/// `% context` HUD only — not a hard cap (the upstream enforces the real limit). Defaults to 128K.
pub(crate) fn ctx_window_for(model: &str) -> usize {
    let m = model.to_ascii_lowercase();
    if m.contains("1m") {
        1_000_000 // explicit 1M-context variants (e.g. opus-4-8-1m-thinking) — checked before the family heuristics
    } else if m.contains("gemini") {
        1_000_000
    } else if m.contains("claude")
        || m.contains("opus")
        || m.contains("sonnet")
        || m.contains("haiku")
    {
        200_000
    } else if m.contains("gpt-4.1") || m.contains("o3") || m.contains("o4") {
        1_000_000
    } else if m.contains("deepseek") {
        64_000
    } else {
        128_000 // gpt-4o family + safe default
    }
}

/// A 10-cell context-fill bar, coloured by pressure using the semantic palette (P-ctx4): OK below
/// 50%, WARN gold from 50%, ERR salmon from 80% — the same green/gold/salmon meanings the rest of
/// the UI uses, instead of bespoke 256-colour indices.
pub(crate) fn ctx_bar(pct: f64) -> String {
    const CELLS: usize = 10;
    let filled = ((pct / 100.0) * CELLS as f64)
        .round()
        .clamp(0.0, CELLS as f64) as usize;
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(CELLS - filled));
    let color: u8 = if pct >= 80.0 {
        theme::ERR
    } else if pct >= 50.0 {
        theme::WARN
    } else {
        theme::OK
    };
    style(bar).color256(color).to_string()
}

/// The effective window from an explicit/configured value (when present) over the name heuristic.
/// Returns `(tokens, was_configured)`. Pure — callers pass the value (lets the wizard compute it
/// against unsaved in-memory config).
pub(crate) fn effective_ctx_window(model: &str, configured: Option<usize>) -> (usize, bool) {
    match configured {
        Some(w) if w > 0 => (w, true),
        _ => (ctx_window_for(model), false),
    }
}

/// The effective context window for `model`: a provider-reported/manually-set value in config (when
/// it matches the active model) wins over the name heuristic. Returns `(tokens, was_configured)`.
pub(crate) fn resolve_ctx_window(model: &str) -> (usize, bool) {
    let cfg = cli_config::load();
    let configured = cfg
        .model_context_window
        .filter(|_| cfg.model.as_deref() == Some(model));
    effective_ctx_window(model, configured)
}

/// Rough session size in tokens — shared by the HUD + auto-compact. Delegates to the agent
/// estimator (content + tool-call payloads + envelopes) plus the tool-schema overhead the loop
/// last published, so the HUD and the mid-loop guards agree on request size.
pub(crate) fn session_tokens(history: &[Message]) -> usize {
    history
        .iter()
        .map(agent::estimate_message_tokens)
        .sum::<usize>()
        + agent::schema_overhead_tokens()
}

/// Compact a token count for display: `12.4K` / `300`.
pub(crate) fn fmt_k(n: usize) -> String {
    if n >= 1000 {
        format!("{:.1}K", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// `/cost` — session token accounting + (when rates are set) an estimated $ cost. Honest by design:
/// shows REAL provider-reported tokens when the endpoint sends `usage`, else the chars/4 context
/// estimate clearly labelled — and never invents a price or a credit balance.
pub(crate) fn print_cost(history: &[Message], model: &str) {
    let (p, c, calls) = client::cost_meter().snapshot();
    let cfg = cli_config::load();
    if calls > 0 {
        let total = p + c;
        let mut line = format!(
            "{}  {} in + {} out = {} tok  ({} call{} reported usage)",
            style("💰 session usage").color256(splash::ACCENT).bold(),
            fmt_k(p as usize),
            fmt_k(c as usize),
            fmt_k(total as usize),
            calls,
            if calls == 1 { "" } else { "s" },
        );
        match (cfg.price_in, cfg.price_out) {
            (Some(pin), Some(pout)) => {
                let cost = p as f64 / 1_000_000.0 * pin + c as f64 / 1_000_000.0 * pout;
                line.push_str(&format!(
                    "  ·  {}",
                    style(format!("est ${cost:.4} (@ ${pin}/${pout} per 1M in/out)")).color256(splash::ACCENT)
                ));
            }
            _ => line.push_str(&format!(
                "  ·  {}",
                style("set rates for a $ estimate: aizen config set --price-in <$/1M> --price-out <$/1M>").dim()
            )),
        }
        // Prompt-cache payoff (only when the provider reported cache reads → confirms caching works).
        let cached = client::cost_meter().cache_read();
        if cached > 0 {
            line.push_str(&format!(
                "  ·  {}",
                style(format!("{} cached @ ~0.1× in", fmt_k(cached as usize))).color256(theme::OK)
            ));
        }
        tui::emit_line(&line);
    } else {
        // No real usage from the provider → fall back to the context-size estimate (not a $ figure).
        let est = session_tokens(history);
        let (window, _) = resolve_ctx_window(model);
        tui::emit_line(&format!(
            "{}  ~{} tok in context · window {} {}",
            style("📊 estimated").color256(splash::ACCENT).bold(),
            fmt_k(est),
            fmt_k(window),
            style("(chars/4 — the provider didn't report token usage, so no per-call $ to show)")
                .dim()
        ));
    }
}

/// Decompose the live system prompt into its named blocks by XML tag, returning (label, char count)
/// for the leftover base instructions plus every block actually present. Pure (byte-index scan over
/// ASCII tags) so it's unit-testable; char counts ÷4 ≈ tokens, the same basis the HUD estimator uses.
pub(crate) fn system_block_chars(system: &str) -> Vec<(&'static str, usize)> {
    // (display label, tag) in build order — an absent block contributes nothing.
    const BLOCKS: &[(&str, &str)] = &[
        ("environment", "environment"),
        ("agent identity", "agent_identity"),
        ("persona", "persona"),
        ("persona memory", "self"),
        ("user memory", "user_memory"),
        ("skills index", "skills"),
        ("project context", "project_context"),
        ("agents index", "agents"),
    ];
    let mut rows = Vec::new();
    let mut tagged = 0usize;
    for (label, tag) in BLOCKS {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        if let (Some(s), Some(e)) = (system.find(&open), system.find(&close)) {
            if e >= s {
                // Tags are ASCII, so byte slicing lands on char boundaries.
                let c = system[s..e + close.len()].chars().count();
                tagged += c;
                rows.push((*label, c));
            }
        }
    }
    let base = system.chars().count().saturating_sub(tagged);
    let mut out = vec![("base instructions", base)];
    out.extend(rows);
    out
}

/// Render the `/compact` result as a small tree: a headline with the token delta, then one `└` leaf
/// per file the collapsed turns had referenced and one for the skills they had loaded. The leaves are
/// what makes compaction feel non-lossy — the dense summary note is invisible, but this shows at a
/// glance the concrete context (which files, which skills) those turns carried, harvested by
/// [`agent::compact::context_touchpoints`] BEFORE the collapse.
pub(crate) fn print_compact_summary(before: usize, after: usize, tp: &agent::compact::Touchpoints) {
    let saved = before.saturating_sub(after);
    tui::emit_line(&format!(
        "{}  {} → {} tok{}",
        style("✳ Compacted").color256(splash::ACCENT).bold(),
        style(format!("~{}", fmt_k(before))).dim(),
        style(format!("~{}", fmt_k(after))).color256(splash::ACCENT),
        if saved > 0 {
            style(format!("  · freed ~{}", fmt_k(saved)))
                .dim()
                .to_string()
        } else {
            String::new()
        },
    ));
    let leaf = style("  └").color256(theme::FAINT).to_string();
    for f in &tp.files {
        tui::emit_line(&format!(
            "{leaf} {} {}",
            style("Referenced file").dim(),
            style(f).color256(theme::ACCENT_DIM)
        ));
    }
    if !tp.skills.is_empty() {
        tui::emit_line(&format!(
            "{leaf} {} ({})",
            style("Skills restored").dim(),
            style(tp.skills.join(", ")).color256(theme::ACCENT_DIM),
        ));
    }
    if tp.files.is_empty() && tp.skills.is_empty() {
        tui::emit_line(&format!(
            "{leaf} {}",
            style("no files or skills to carry forward").dim()
        ));
    }
}

/// `/context` — where the tokens are going right now: the system prompt split into its blocks, the
/// tool-schema overhead (rides every request, lives in no message), and the conversation split by
/// role. Estimated (chars/4) — the same honest basis the HUD + auto-compact use; `/cost` shows the
/// provider's REAL billed count when the endpoint reports usage.
pub(crate) fn print_context(history: &[Message], model: &str) {
    let (window, auto) = resolve_ctx_window(model);
    let total = session_tokens(history);
    let pct = (total as f64 / window as f64 * 100.0).min(100.0);

    let system = history
        .first()
        .filter(|m| m.role == "system")
        .and_then(|m| m.content.as_deref())
        .unwrap_or("");
    let sys_blocks = system_block_chars(system);
    let sys_tok: usize = sys_blocks.iter().map(|(_, c)| c / 4).sum();
    let schemas = agent::schema_overhead_tokens();

    // Everything after the system message, bucketed by role.
    let (mut user_tok, mut asst_tok, mut tool_tok) = (0usize, 0usize, 0usize);
    for m in history.iter().skip(1) {
        let t = agent::estimate_message_tokens(m);
        match m.role.as_str() {
            "assistant" => asst_tok += t,
            "tool" => tool_tok += t,
            _ => user_tok += t, // user turns + any stray system nudges
        }
    }
    let convo = user_tok + asst_tok + tool_tok;

    // One aligned row: label left-padded to a column, "~X.XK tok" right; sub-rows dimmed + indented.
    fn line(label: &str, tok: usize, depth: usize, dim: bool) -> String {
        let name = format!("{}{}", "  ".repeat(depth), label);
        let s = format!("{name:<26} {:>10}", format!("~{} tok", fmt_k(tok)));
        if dim {
            style(s).dim().to_string()
        } else {
            s
        }
    }

    tui::emit_line(&format!(
        "{}  {model} · window {}{}",
        style("📊 context breakdown")
            .color256(splash::ACCENT)
            .bold(),
        fmt_k(window),
        if auto { "" } else { " (est)" },
    ));
    tui::emit_line(&line("system prompt", sys_tok, 0, false));
    for (label, c) in &sys_blocks {
        if c / 4 > 0 {
            tui::emit_line(&line(label, c / 4, 1, true));
        }
    }
    tui::emit_line(&line("tool schemas", schemas, 0, false));
    tui::emit_line(&line("conversation", convo, 0, false));
    if convo > 0 {
        tui::emit_line(&line("user turns", user_tok, 1, true));
        tui::emit_line(&line("assistant turns", asst_tok, 1, true));
        tui::emit_line(&line("tool results", tool_tok, 1, true));
    }
    let bar = format!("{} {}", ctx_bar(pct), style(format!("{pct:.0}%")).dim());
    tui::emit_line(&format!(
        "{}  {} {bar}",
        style(format!("{:<26}", "total"))
            .color256(splash::ACCENT)
            .bold(),
        style(format!("~{} / {} tok", fmt_k(total), fmt_k(window))).color256(splash::ACCENT),
    ));
}

#[cfg(test)]
#[path = "../tests/context_breakdown.rs"]
mod context_breakdown_tests;
