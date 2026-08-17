//! What each slash command actually DOES.
//!
//! The registry in [`crate::features::slash`] owns a command's identity — its name, aliases, help
//! line, and whether it takes over stdin. This module owns the other half: the body that runs when
//! one is invoked. `handle_slash` resolves the typed spelling to a [`SlashId`] and dispatches on it
//! with an exhaustive match, so a command in the table without a handler here is a compile error.
//!
//! These are REPL handlers: they read and mutate the live conversation, and they print through
//! `tui::emit_line` so their output lands correctly whether or not a retained frame is running.

use crate::core::approval::ApprovalMode;
use crate::core::session_store::*;
use crate::core::{cli_config, types};
use crate::features::slash::{self, SlashId};
use crate::features::{commands, coop, timemachine};
use crate::memory;
use crate::ui::{config_ui, icons, splash, theme, tui};
use crate::*;
use anyhow::Result;
use console::style;
use dialoguer::{Confirm, Select};
use types::Message;

fn slash_memory(arg: &str) -> Result<()> {
    let (sub, rest) = match arg.split_once(char::is_whitespace) {
        Some((s, r)) => (s.trim(), r.trim()),
        None => (arg.trim(), ""),
    };
    match sub {
        // Bare `/memory` keeps its old meaning (the rolled-up profile).
        "" => memory::cmd_profile(false),
        "list" | "ls" => memory::cmd_list(if rest.is_empty() { None } else { Some(rest) }),
        "show" | "cat" => {
            if rest.is_empty() {
                anyhow::bail!("usage: /memory show <id>  (ids from `/memory list`)");
            }
            memory::cmd_show(rest)
        }
        "remember" | "add" => {
            if rest.is_empty() {
                anyhow::bail!("usage: /memory remember <fact>");
            }
            let id = memory::remember(rest)?;
            tui::emit_line(
                &style(format!("{}remembered ({id})", icons::g(icons::learned())))
                    .color256(splash::ACCENT)
                    .to_string(),
            );
            Ok(())
        }
        "edit" | "update" => {
            // `/memory edit <id> <new body>` — the common correction. Field-by-field editing
            // (description/type/scope) stays on the CLI, which has real flags for it.
            let (id, body) = rest
                .split_once(char::is_whitespace)
                .map(|(i, b)| (i.trim(), b.trim()))
                .unwrap_or((rest, ""));
            if id.is_empty() || body.is_empty() {
                anyhow::bail!("usage: /memory edit <id> <corrected fact>  (field flags: `aizen memory edit --help`)");
            }
            memory::cmd_edit(id, None, None, None, Some(body.to_string()), None)
        }
        "forget" | "rm" => {
            if rest.is_empty() {
                anyhow::bail!("usage: /memory forget <id>  (archived, not erased — restorable)");
            }
            memory::cmd_forget(rest)
        }
        "archive" => memory::cmd_archive_list(),
        // The review queue is where mid-confidence learned facts wait for a human. It had a CLI but
        // no REPL door, so the queue only ever grew — 29 items deep on this machine before anyone
        // could see them. Same functions as `aizen memory review`, so the two can't drift.
        "review" | "queue" => {
            let (op, key) = match rest.split_once(char::is_whitespace) {
                Some((o, k)) => (o.trim(), k.trim()),
                None => (rest, ""),
            };
            match op {
                "" => memory::cmd_review(None, None, false),
                "promote" | "keep" | "accept" => {
                    if key.is_empty() {
                        anyhow::bail!("usage: /memory review promote <id>  (ids from `/memory review`)");
                    }
                    memory::cmd_review(Some(key.to_string()), None, false)
                }
                "drop" | "reject" => {
                    if key.is_empty() {
                        anyhow::bail!("usage: /memory review drop <id>  (ids from `/memory review`)");
                    }
                    memory::cmd_review(None, Some(key.to_string()), false)
                }
                "clear" => memory::cmd_review(None, None, true),
                other => anyhow::bail!(
                    "unknown: /memory review {other}  (try: review | review promote <id> | review drop <id> | review clear)"
                ),
            }
        }
        "restore" => {
            if rest.is_empty() {
                anyhow::bail!(
                    "usage: /memory restore <id> [--as <new-id>]  (ids from `/memory archive`)"
                );
            }
            // `--as` has to be reachable from here too: a collision makes plain `restore` fail, and
            // without the escape hatch in the REPL the only way out would be to leave the REPL.
            let (id, as_id) = match rest.split_once("--as") {
                Some((a, b)) => (a.trim(), Some(b.trim()).filter(|s| !s.is_empty())),
                None => (rest, None),
            };
            if id.is_empty() {
                anyhow::bail!("usage: /memory restore <id> [--as <new-id>]");
            }
            memory::cmd_restore(id, as_id)
        }
        "profile" => memory::cmd_profile(false),
        "style" => memory::cmd_style(),
        "frozen" | "core" => memory::cmd_frozen(false),
        // Anything else is treated as a search query, which is what `/memory <words>` always did.
        _ => memory::cmd_search(arg, 5, None, None, None),
    }
}

pub(crate) enum SlashOutcome {
    Continue,
    Quit,
    /// A custom command expanded to this prompt — feed it through the normal chat path.
    Submit(String),
}

/// Bare `/` → an arrow-key picker over the slash commands; runs the chosen one (default args).
/// Built-ins and user-defined custom commands both come from the shared [`crate::features::slash`]
/// catalog, so the picker, the live palette, and `/help` can never drift apart.
pub(crate) async fn slash_menu(
    history: &mut Vec<Message>,
    model_label: &mut String,
) -> SlashOutcome {
    let catalog = crate::features::slash::list();
    let items: Vec<String> = catalog
        .iter()
        .map(|c| {
            let hint = if c.argument_hint.is_empty() {
                String::new()
            } else {
                format!(" {}", c.argument_hint)
            };
            let icon = icons::g(icons::slash(if c.custom { "commands" } else { &c.name }));
            format!("{icon}/{}{hint}  —  {}", c.name, c.description)
        })
        .collect();
    let theme = ui_theme();
    match Select::with_theme(&theme)
        .with_prompt("slash command")
        .items(&items)
        .default(0)
        .interact_opt()
    {
        // Every entry (built-in or custom) dispatches by name through the one `handle_slash` path.
        Ok(Some(i)) => handle_slash(&catalog[i].name, history, model_label).await,
        _ => SlashOutcome::Continue, // Esc / error → back to the prompt
    }
}

/// Slash commands that drive the terminal directly (dialoguer menus, the Telegram daemon) and so
/// need the sticky box SUSPENDED. Everything else is pure-print: it runs with the box still up and
/// its `tui::emit_line` output flows into the scroll region (so short output isn't painted over).
///
/// Delegates to the ONE shared table in `tui`. This used to be a second, independently maintained
/// list, and the two had drifted: this copy matched whole command names, so `/timeline pick` and
/// `/tools menu` opened a dialoguer menu without suspending the box, while `/memory` (pure-print)
/// was suspended for nothing. Anything that owns stdin must appear in exactly one place.
pub(crate) fn slash_is_interactive(cmd: &str) -> bool {
    tui::slash_takes_stdin(cmd)
}

async fn slash_tools(_arg: &str) {
    tui::emit_line(&agent::toolsets::format_config_status());
}

/// `/workflows` — live multi-agent activity. In retained mode the panel REFRESHES itself (elapsed
/// times tick while you watch); on a pipe/CI it degrades to a single printed snapshot.
///
/// `/workflows stop <id>` cancels one running row without touching the rest of the turn: Esc is
/// all-or-nothing (it cancels the whole turn), so a fan-out with one child stuck behind a slow model
/// call previously left the user no choice but to kill everything.
async fn slash_workflows(arg: &str) {
    // Shared with the input thread's mid-turn fast path, so an idle stop and a mid-turn stop cannot
    // report the same action differently.
    if let Some(note) = agent::orchestration::try_stop_command(arg) {
        tui::emit_line(&theme::muted(note).to_string());
        return;
    }
    if !tui::retained_overlay_open_live("Activity", agent::orchestration::format_status) {
        tui::emit_line(&agent::orchestration::format_status());
    }
}

/// `/init` — build (or incrementally refresh) the per-repo codebase index that powers
/// `codebase_search` + automatic per-turn retrieval. `--force`/`-f` rebuilds from scratch;
/// `--status`/`-s` shows the current index without scanning. Esc cancels a running scan cleanly
/// (the existing index is left untouched). The scan runs on a blocking thread so the REPL stays
/// responsive; progress is reported by phase (scan → chunk → build), never one line per file.
async fn slash_init(arg: &str) {
    use crate::agent::codebase;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let flags: Vec<String> = arg
        .split_whitespace()
        .map(|s| s.to_ascii_lowercase())
        .collect();
    let want = |names: &[&str]| flags.iter().any(|f| names.contains(&f.as_str()));

    // `--status`: print the current index state, no scan.
    if want(&["--status", "-s", "status"]) {
        match codebase::load() {
            Some(idx) => {
                let ago = fmt_time_ago(idx.built_unix);
                tui::emit_line(&format!(
                    "{} {} file(s), {} chunk(s) — indexed {}",
                    style("✓ codebase index:").color256(splash::ACCENT),
                    idx.files.len(),
                    idx.chunks.len(),
                    ago
                ));
                let summary = codebase::analysis_summary(&idx.analysis);
                if !summary.trim().is_empty() {
                    tui::emit_line(&style(summary).dim().to_string());
                }
            }
            None => tui::emit_line(
                &style("no codebase index yet — run /init to build it")
                    .dim()
                    .to_string(),
            ),
        }
        return;
    }

    let force = want(&["--force", "-f", "force", "rebuild"]);
    let incremental = !force;

    // Arm a cancel token for this scan so Esc aborts it (the input thread calls request_cancel).
    let cancel = crate::core::cancel::TurnCancel::new();
    tui::arm_cancel(cancel.clone());
    tui::emit_line(
        &style(if force {
            "rebuilding codebase index…"
        } else {
            "indexing codebase…"
        })
        .dim()
        .to_string(),
    );

    // Phase progress, decile-throttled so a large scan reports ~10 lines, not one per file.
    let last_decile = std::sync::Arc::new(AtomicUsize::new(usize::MAX));
    let ld = last_decile.clone();
    let progress = move |phase: codebase::Phase| match phase {
        codebase::Phase::Scanning { done, total } => {
            if total == 0 {
                return;
            }
            let decile = done * 10 / total.max(1);
            if ld.swap(decile, Ordering::Relaxed) != decile {
                tui::emit_line(
                    &style(format!("  scanning… {}%", decile * 10))
                        .dim()
                        .to_string(),
                );
            }
        }
        codebase::Phase::Chunking => {
            tui::emit_line(&style("  chunking symbols…").dim().to_string())
        }
        codebase::Phase::Building => tui::emit_line(&style("  building index…").dim().to_string()),
    };

    let cancel_for_task = cancel.clone();
    let result = tokio::task::spawn_blocking(move || {
        codebase::build_index(incremental, Some(&cancel_for_task), &progress)
    })
    .await;
    tui::disarm_cancel(&cancel);

    match result {
        Ok(Ok(stats)) => {
            let mut parts = vec![
                format!("{} file(s)", stats.indexed),
                format!("{} chunk(s)", stats.chunks),
            ];
            if stats.reused > 0 {
                parts.push(format!("{} reused", stats.reused));
            }
            if stats.added > 0 {
                parts.push(format!("{} updated", stats.added));
            }
            if stats.removed > 0 {
                parts.push(format!("{} removed", stats.removed));
            }
            tui::emit_line(&format!(
                "{} {} in {}ms",
                style("✓ codebase indexed:").color256(splash::ACCENT),
                parts.join(", "),
                stats.elapsed_ms
            ));
            // Sensitivity / skip accounting — surfaced so the user knows coverage + that secrets
            // were protected, without ever printing a path or a secret value.
            let mut notes = Vec::new();
            if stats.sensitive > 0 {
                notes.push(format!(
                    "{} sensitive file(s) stored path-only",
                    stats.sensitive
                ));
            }
            if stats.redacted > 0 {
                notes.push(format!("{} file(s) had secrets redacted", stats.redacted));
            }
            if stats.skipped_large > 0 {
                notes.push(format!("{} oversized skipped", stats.skipped_large));
            }
            if stats.skipped_binary > 0 {
                notes.push(format!("{} binary skipped", stats.skipped_binary));
            }
            if stats.capped {
                notes.push("scan hit the file cap (coverage bounded)".to_string());
            }
            if !notes.is_empty() {
                tui::emit_line(&style(format!("  {}", notes.join(" · "))).dim().to_string());
            }
            let summary = codebase::analysis_summary(
                &codebase::load().map(|i| i.analysis).unwrap_or_default(),
            );
            if !summary.trim().is_empty() {
                tui::emit_line(&style(summary).dim().to_string());
            }
        }
        Ok(Err(e)) => {
            // A cancel is a clean, expected outcome — show it calmly, not as a hard error.
            let msg = e.to_string();
            if msg.contains("cancelled") {
                tui::emit_line(
                    &style("/init cancelled — the existing index was left unchanged")
                        .color256(theme::WARN)
                        .to_string(),
                );
            } else {
                tui::emit_line(&format!("{} {msg}", style("/init:").red()));
            }
        }
        Err(e) => tui::emit_line(&format!("{} scan task failed: {e}", style("/init:").red())),
    }
}

/// `/agents` — list installed specialists with their effective provider/model assignment. The normal
/// write path is `/agents set-provider`; legacy card `set-model` remains available for compatibility.
fn slash_agents(arg: &str) {
    let mut parts = arg.splitn(2, char::is_whitespace);
    let sub = parts.next().unwrap_or("").trim();
    let rest = parts.next().unwrap_or("").trim();
    match sub {
        "set-provider" | "provider" => {
            let mut rp = rest.split_whitespace();
            let name = rp.next().unwrap_or("");
            let provider = rp.next().unwrap_or("");
            let model = rp.next();
            if name.is_empty() || provider.is_empty() {
                tui::emit_line(&style("usage: /agents set-provider <agent> <provider> [model]   ·   clear: /agents set-provider <agent> clear").dim().to_string());
                return;
            }
            let mut cfg = cli_config::load();
            let result = if provider.eq_ignore_ascii_case("clear") || provider == "-" {
                cfg.set_agent_route(name, None, None)
            } else {
                cfg.set_agent_route(name, Some(provider.to_string()), model.map(str::to_string))
            };
            match result.and_then(|_| cli_config::save(&cfg)) {
                Ok(()) => tui::emit_line(
                    &style(format!("updated provider assignment for '{name}'"))
                        .color256(theme::OK)
                        .to_string(),
                ),
                Err(e) => tui::emit_line(&format!("{} {e:#}", style("agents:").red())),
            }
        }
        "set-model" | "model" => {
            let mut rp = rest.splitn(2, char::is_whitespace);
            let name = rp.next().unwrap_or("").trim();
            let model = rp.next().unwrap_or("").trim();
            if name.is_empty() {
                tui::emit_line(&style("usage: /agents set-model <name> <model>   (omit <model> or pass `clear` to remove the pin)").dim().to_string());
                return;
            }
            let clear = model.is_empty() || model.eq_ignore_ascii_case("clear") || model == "-";
            let value = if clear { None } else { Some(model.to_string()) };
            match agents::set_model(name, value.as_deref()) {
                Ok(path) => {
                    let msg = match &value {
                        Some(m) => format!("pinned '{name}' → model {m}  ({})", path.display()),
                        None => format!("cleared model pin on '{name}'  ({})", path.display()),
                    };
                    tui::emit_line(&style(msg).color256(theme::OK).to_string());
                }
                Err(e) => tui::emit_line(&format!("{} {e:#}", style("agents:").red())),
            }
        }
        "" | "list" => {
            let all = agents::list();
            if all.is_empty() {
                tui::emit_line(&style("no specialist agents installed — `aizen agents install msitarzewski/agency-agents`").dim().to_string());
                return;
            }
            let enabled = agents::enabled_set();
            let mut out = String::from("specialist agents (● pinned to <agents> index / ○ not):\n");
            let cfg = cli_config::load();
            for def in &all {
                let slug = def.slug();
                let pin = enabled.as_ref().map(|s| s.contains(&slug)).unwrap_or(true);
                let mark = if pin { "●" } else { "○" };
                let route = cfg.agent_route(&slug);
                let provider = route
                    .and_then(|r| r.provider.as_deref())
                    .unwrap_or("inherit");
                let model = route
                    .and_then(|r| r.model.as_deref())
                    .or(def.model.as_deref())
                    .unwrap_or("default");
                out.push_str(&format!(
                    "  {mark} {:<24} provider: {provider} · model: {model}\n",
                    slug
                ));
            }
            out.push_str("\nset a provider: /agents set-provider <agent> <provider> [model]   ·   clear: ... <agent> clear");
            tui::emit_line(out.trim_end());
        }
        other => {
            tui::emit_line(&style(format!("unknown /agents subcommand '{other}' — try /agents or /agents set-provider <agent> <provider> [model]")).dim().to_string());
        }
    }
}

/// Rows for `/team status` — every aizen session registered in this repository.
///
/// The point of this table is the LAST two columns: who is still running, and which files each
/// window changed. `git diff` alone cannot answer either question once two windows share a tree.
pub(crate) fn team_status_lines() -> Vec<String> {
    let sessions = coop::list();
    if sessions.is_empty() {
        return vec![style(
            "no aizen sessions registered here yet — this window registers itself on start",
        )
        .dim()
        .to_string()];
    }
    let mut out = vec![format!(
        "{}  {} session(s) in this repository",
        style("⚑ team").color256(splash::ACCENT).bold(),
        sessions.len()
    )];
    for (i, v) in sessions.iter().enumerate() {
        let m = &v.manifest;
        let state = v.effective_state();
        let color = match state {
            "working" | "awaiting-approval" => theme::LINK,
            "done" | "committed" | "finished" => theme::OK,
            "abandoned" | "failed" => theme::WARN,
            _ => theme::MUTED,
        };
        let mine = if v.is_self { " ●" } else { "" };
        out.push(format!(
            "  {:>2}. {}{mine}  {}  {}",
            i + 1,
            style(&m.session_id).bold(),
            style(format!("[{state}]")).color256(color),
            style(format!(
                "{} file(s) · {} turn(s) · {}",
                m.files.len(),
                m.turns,
                relative_age_secs(now_unix_secs().saturating_sub(m.updated_unix))
            ))
            .dim(),
        ));
        if !m.task.is_empty() {
            out.push(format!(
                "      {}",
                style(&m.task).color256(theme::ACCENT_DIM)
            ));
        }
        let wt = v.worktree_label();
        if !m.branch.as_deref().unwrap_or("").is_empty() || !wt.is_empty() {
            out.push(
                style(format!(
                    "      {} · branch {}",
                    wt,
                    m.branch.as_deref().unwrap_or("(detached)")
                ))
                .dim()
                .to_string(),
            );
        }
        if !v.overlapping.is_empty() {
            out.push(
                style(format!(
                    "      ⚠ shares {} file(s) with another session: {}",
                    v.overlapping.len(),
                    v.overlapping.join(", ")
                ))
                .color256(theme::WARN)
                .to_string(),
            );
        }
        if let Some(reason) = &m.degraded {
            out.push(
                style(format!("      ⚠ no per-session diff: {reason}"))
                    .color256(theme::WARN)
                    .to_string(),
            );
        }
        if m.truncated_files > 0 {
            out.push(
                style(format!(
                    "      ⚠ {} path(s) beyond the tracking ceiling are not recorded",
                    m.truncated_files
                ))
                .color256(theme::WARN)
                .to_string(),
            );
        }
    }
    out.push(
        style(
            "/team diff <n> [-p] · /team files <n> · /team claims · /team commit <n> · /team done",
        )
        .dim()
        .to_string(),
    );
    out
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Compact age from an already-computed elapsed span. `fmt_time_ago` takes a timestamp and reads the
/// clock itself; the `/team` tables have many rows against ONE `now`, so they subtract once.
fn relative_age_secs(secs: u64) -> String {
    if secs < 60 {
        "now".to_string()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// `/team` — read and act on what the OTHER aizen windows in this repository are doing.
async fn slash_team(arg: &str) {
    let mut parts = arg.splitn(2, char::is_whitespace);
    let sub = parts.next().unwrap_or("").trim();
    let rest = parts.next().unwrap_or("").trim();
    match sub {
        "" | "status" | "ls" | "list" => {
            for line in team_status_lines() {
                tui::emit_line(&line);
            }
        }
        "task" => {
            if rest.is_empty() {
                tui::emit_line(
                    &style("usage: /team task <what this window is working on>")
                        .dim()
                        .to_string(),
                );
                return;
            }
            coop::set_task(rest);
            tui::emit_line(
                &style(format!("this session's task: {rest}"))
                    .color256(theme::OK)
                    .to_string(),
            );
        }
        "done" => {
            coop::set_state(coop::SessionState::Done);
            tui::emit_line(
                &style("marked this session done — another window can now commit its work")
                    .color256(theme::OK)
                    .to_string(),
            );
        }
        "files" => match coop::resolve(rest) {
            Ok(view) => {
                let paths = coop::session_paths(&view);
                if paths.is_empty() {
                    tui::emit_line(
                        &style(format!(
                            "{} has no recorded file changes",
                            view.manifest.session_id
                        ))
                        .dim()
                        .to_string(),
                    );
                    return;
                }
                tui::emit_line(&format!(
                    "{} {} file(s) changed by {}",
                    style("⚑").color256(splash::ACCENT),
                    paths.len(),
                    style(&view.manifest.session_id).bold()
                ));
                for f in &view.manifest.files {
                    tui::emit_line(&format!(
                        "  {} {}  {}",
                        f.status,
                        f.path,
                        style(format!("from checkpoint #{}", f.base)).dim()
                    ));
                }
            }
            Err(e) => tui::emit_line(&format!("{} {e:#}", style("team:").red())),
        },
        "diff" => {
            let mut patch = false;
            let mut who = String::new();
            for tok in rest.split_whitespace() {
                match tok {
                    "-p" | "--patch" => patch = true,
                    _ => {
                        if who.is_empty() {
                            who = tok.to_string();
                        }
                    }
                }
            }
            let view = match coop::resolve(&who) {
                Ok(v) => v,
                Err(e) => {
                    tui::emit_line(&format!("{} {e:#}", style("team:").red()));
                    return;
                }
            };
            match coop::session_diff(&view, patch) {
                Ok(reports) if reports.is_empty() => tui::emit_line(
                    &style(format!(
                        "{} has no changes still present in the working tree",
                        view.manifest.session_id
                    ))
                    .dim()
                    .to_string(),
                ),
                Ok(reports) => {
                    tui::emit_line(&format!(
                        "{} changes attributed to {}",
                        style("⚑").color256(splash::ACCENT),
                        style(&view.manifest.session_id).bold()
                    ));
                    for report in &reports {
                        for line in diff_lines(report, "/team diff <n>") {
                            tui::emit_line(&line);
                        }
                    }
                    if !view.overlapping.is_empty() {
                        tui::emit_line(
                            &style(format!(
                                "⚠ {} file(s) were also changed by another session — the diff above \
                                 cannot separate their lines: {}",
                                view.overlapping.len(),
                                view.overlapping.join(", ")
                            ))
                            .color256(theme::WARN)
                            .to_string(),
                        );
                    }
                }
                Err(e) => tui::emit_line(&format!("{} {e:#}", style("team diff:").red())),
            }
        }
        "claims" => {
            let claims = coop::claims();
            if claims.is_empty() {
                tui::emit_line(&style("no files are claimed yet").dim().to_string());
                return;
            }
            tui::emit_line(&format!(
                "{} {} claimed file(s) — most recent writer owns the claim",
                style("⚑ claims").color256(splash::ACCENT).bold(),
                claims.len()
            ));
            for (path, claim) in claims.iter().take(200) {
                tui::emit_line(&format!(
                    "  {path}  {}",
                    style(format!(
                        "{} · {}",
                        claim.session_id,
                        relative_age_secs(now_unix_secs().saturating_sub(claim.unix))
                    ))
                    .dim()
                ));
            }
            let overlaps = coop::overlaps();
            if !overlaps.is_empty() {
                tui::emit_line(
                    &style(format!("⚠ {} overlapping file(s):", overlaps.len()))
                        .color256(theme::WARN)
                        .to_string(),
                );
                for o in overlaps.iter().take(100) {
                    tui::emit_line(&format!("  {}  {} → {}", o.path, o.first, o.second));
                }
            }
        }
        "commit" => slash_team_commit(rest).await,
        other => tui::emit_line(
            &style(format!(
                "unknown /team subcommand '{other}' — status · task · done · files · diff · claims · commit"
            ))
            .dim()
            .to_string(),
        ),
    }
}

/// `/team commit <session> [-m msg] [--verify] [--force] [--dry-run]`.
///
/// Staging is derived from the session's ledger, so the coordinator commits ONE window's work rather
/// than whatever happens to be in the tree. The commit itself is confirmed interactively — it is the
/// one irreversible step here.
async fn slash_team_commit(rest: &str) {
    let mut who = String::new();
    let mut message = String::new();
    let mut verify = false;
    let mut force = false;
    let mut dry_run = false;
    let mut expect_message = false;
    for tok in rest.split_whitespace() {
        if expect_message {
            if !message.is_empty() {
                message.push(' ');
            }
            message.push_str(tok);
            continue;
        }
        match tok {
            "-m" | "--message" => expect_message = true,
            "--verify" => verify = true,
            "--force" => force = true,
            "--dry-run" | "-n" => dry_run = true,
            _ if who.is_empty() => who = tok.to_string(),
            _ => {}
        }
    }
    let view = match coop::resolve(&who) {
        Ok(v) => v,
        Err(e) => {
            tui::emit_line(&format!("{} {e:#}", style("team commit:").red()));
            return;
        }
    };
    let plan = match coop::plan_commit(&view) {
        Ok(p) => p,
        Err(e) => {
            tui::emit_line(&format!("{} {e:#}", style("team commit:").red()));
            return;
        }
    };
    tui::emit_line(&format!(
        "{} {} file(s) from {}",
        style("⚑ commit plan").color256(splash::ACCENT).bold(),
        plan.paths.len(),
        style(&plan.session_id).bold()
    ));
    for p in plan.paths.iter().take(100) {
        tui::emit_line(&format!("  {p}"));
    }
    if plan.paths.len() > 100 {
        tui::emit_line(
            &style(format!("  … {} more", plan.paths.len() - 100))
                .dim()
                .to_string(),
        );
    }
    if !plan.shared_paths.is_empty() {
        tui::emit_line(
            &style(format!(
                "⚠ {} of these file(s) were also changed by another session; committing them takes \
                 that work along — git cannot split one file by author: {}",
                plan.shared_paths.len(),
                plan.shared_paths.join(", ")
            ))
            .color256(theme::WARN)
            .to_string(),
        );
    }
    if !plan.blockers.is_empty() {
        for b in &plan.blockers {
            tui::emit_line(&style(format!("⚠ {b}")).color256(theme::WARN).to_string());
        }
        if !force {
            tui::emit_line(
                &style("nothing was staged. Re-run with --force to proceed anyway.")
                    .dim()
                    .to_string(),
            );
            return;
        }
    }
    let review = match coop::stage_plan(&plan) {
        Ok(s) => s,
        Err(e) => {
            tui::emit_line(&format!("{} {e:#}", style("team commit:").red()));
            return;
        }
    };
    for line in review.stat.lines() {
        tui::emit_line(&format!("  {line}"));
    }
    if !review.separated.is_empty() {
        tui::emit_line(
            &style(format!(
                "↔ {} shared file(s) were separated: the commit holds only this session's version, \
                 while the working tree keeps both sessions' edits — {}",
                review.separated.len(),
                review.separated.join(", ")
            ))
            .color256(splash::ACCENT)
            .to_string(),
        );
    }
    if verify {
        match crate::agent::verify_gate::run_verify_gate(&plan.root, 300).await {
            None => tui::emit_line(
                &style("verify: no known verify command for this project — skipped")
                    .dim()
                    .to_string(),
            ),
            Some(r) if r.passed => tui::emit_line(
                &style(format!("verify: {} passed", r.command))
                    .color256(theme::OK)
                    .to_string(),
            ),
            Some(r) => {
                tui::emit_line(
                    &style(crate::agent::verify_gate::format_gate_failure(&r))
                        .color256(theme::WARN)
                        .to_string(),
                );
                let _ = coop::unstage_plan(&plan);
                tui::emit_line(
                    &style("unstaged the plan — the working tree was not touched")
                        .dim()
                        .to_string(),
                );
                return;
            }
        }
    }
    if dry_run {
        let _ = coop::unstage_plan(&plan);
        tui::emit_line(
            &style("dry run — unstaged again, nothing was committed")
                .dim()
                .to_string(),
        );
        return;
    }
    let msg = if message.trim().is_empty() {
        let task = view.manifest.task.trim();
        if task.is_empty() {
            format!("aizen session {}", plan.session_id)
        } else {
            task.to_string()
        }
    } else {
        message.trim().to_string()
    };
    let status = tui::current_status();
    tui::suspend();
    let go = Confirm::with_theme(&ui_theme())
        .with_prompt(format!("Commit {} file(s) as “{msg}”?", plan.paths.len()))
        .default(false)
        .interact()
        .unwrap_or(false);
    tui::resume(&status);
    if !go {
        let _ = coop::unstage_plan(&plan);
        tui::emit_line(
            &style("cancelled — unstaged, working tree untouched")
                .dim()
                .to_string(),
        );
        return;
    }
    match coop::commit_staged(&plan, &msg, &review) {
        Ok(out) => {
            for line in out.lines().take(6) {
                tui::emit_line(&format!("  {line}"));
            }
            tui::emit_line(
                &style(format!("committed {}'s work", plan.session_id))
                    .color256(theme::OK)
                    .to_string(),
            );
        }
        Err(e) => tui::emit_line(&format!("{} {e:#}", style("team commit:").red())),
    }
}

/// `/work` — isolated worktree mode: one aizen window per linked worktree + branch.
fn slash_work(arg: &str) {
    let mut parts = arg.splitn(2, char::is_whitespace);
    let sub = parts.next().unwrap_or("").trim();
    let rest = parts.next().unwrap_or("").trim();
    match sub {
        "" | "list" | "ls" => match coop::work_list() {
            Ok(list) if list.is_empty() => tui::emit_line(
                &style("no aizen worktrees — create one with /work new <name>")
                    .dim()
                    .to_string(),
            ),
            Ok(list) => {
                tui::emit_line(&format!(
                    "{}  {} worktree(s)",
                    style("⚑ work").color256(splash::ACCENT).bold(),
                    list.len()
                ));
                for w in &list {
                    let flags = coop::work_remove_blockers(w);
                    let note = if flags.is_empty() {
                        style("clean".to_string()).color256(theme::OK)
                    } else {
                        style(flags.join("; ")).color256(theme::WARN)
                    };
                    tui::emit_line(&format!(
                        "  {:<20} {}  {}",
                        w.name,
                        style(&w.branch).dim(),
                        note
                    ));
                    tui::emit_line(
                        &style(format!("      {}", w.path.display()))
                            .dim()
                            .to_string(),
                    );
                }
            }
            Err(e) => tui::emit_line(&format!("{} {e:#}", style("work:").red())),
        },
        "new" | "add" => {
            if rest.is_empty() {
                tui::emit_line(&style("usage: /work new <name>").dim().to_string());
                return;
            }
            match coop::work_new(rest) {
                Ok(wt) => {
                    tui::emit_line(
                        &style(format!(
                            "created worktree {} on {}",
                            wt.path.display(),
                            wt.branch
                        ))
                        .color256(theme::OK)
                        .to_string(),
                    );
                    tui::emit_line(
                        &style(format!(
                            "open a new terminal there: cd {}",
                            wt.path.display()
                        ))
                        .dim()
                        .to_string(),
                    );
                }
                Err(e) => tui::emit_line(&format!("{} {e:#}", style("work new:").red())),
            }
        }
        "remove" | "rm" => {
            let mut name = String::new();
            let mut force = false;
            for tok in rest.split_whitespace() {
                match tok {
                    "--force" | "-f" => force = true,
                    _ if name.is_empty() => name = tok.to_string(),
                    _ => {}
                }
            }
            if name.is_empty() {
                tui::emit_line(
                    &style("usage: /work remove <name> [--force]")
                        .dim()
                        .to_string(),
                );
                return;
            }
            match coop::work_remove(&name, force) {
                Ok(msg) => tui::emit_line(&style(msg).color256(theme::OK).to_string()),
                Err(e) => tui::emit_line(&format!("{} {e:#}", style("work remove:").red())),
            }
        }
        other => tui::emit_line(
            &style(format!(
                "unknown /work subcommand '{other}' — list · new · remove"
            ))
            .dim()
            .to_string(),
        ),
    }
}

pub(crate) async fn handle_slash(
    input: &str,
    history: &mut Vec<Message>,
    model_label: &mut String,
) -> SlashOutcome {
    let mut parts = input.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("").trim();
    let arg = parts.next().unwrap_or("").trim();
    // Resolve the typed spelling to a command IDENTITY first, then dispatch on that identity. Every
    // alias therefore reaches the same arm as its canonical name for free, and because the match
    // below is exhaustive over `SlashId`, a new row in `slash::BUILTINS` does not compile until it
    // has a handler here. The name lists used to live in five places; this is the only one left.
    // An empty name still means help: the retained REPL routes a bare `/` to the picker, but the
    // host bot and the plain REPL can both reach here with nothing after the slash.
    let Some(builtin) = slash::resolve(if name.is_empty() { "help" } else { name }) else {
        return slash_custom_or_unknown(name, arg);
    };
    match builtin.id {
        SlashId::Help => tui::emit_line(&style(slash::help_page()).dim().to_string()),
        SlashId::Quit => return SlashOutcome::Quit,
        SlashId::Clear => {
            rebuild_system(history, model_label);
            reset_per_session_state(); // fresh todos/cost/grants/@refs for the new conversation
            set_session_slug(None); // the next turn names + autosaves a brand-new session file
            update_live_history(history); // drop the old chat from the exit-flush snapshot too, so an
                                          // immediate window-close after /clear doesn't re-save it
            tui::emit_line(&style("(new conversation)").dim().to_string());
        }
        SlashId::Where => {
            tui::emit_line(&where_report());
            // In the REPL, "where" includes WHICH FILE this conversation is being written to.
            let sess = match current_session_slug() {
                Some(s) => format!("session:  {}", sessions_dir().join(format!("{s}.json")).display()),
                None => "session:  (not saved yet — named on the first autosave)".to_string(),
            };
            tui::emit_line(&style(sess).dim().to_string());
        }
        SlashId::Tokens => print_status_line(history, model_label),
        SlashId::Context => print_context(history, model_label),
        SlashId::Cost => print_cost(history, model_label),
        // /save + /load folded into /sessions (the current chat autosaves under its own name).
        SlashId::Save => {
            tui::emit_line(&style("→ use /sessions — restore / save / delete are all there now").dim().to_string());
        }
        SlashId::Sessions => {
            if let Err(e) = sessions_menu(history, model_label).await {
                tui::note_line(&format!("{} {e}", style("sessions:").red()));
            }
        }
        SlashId::Import => {
            if let Err(e) = import_menu(history, model_label).await {
                tui::note_line(&format!("{} {e}", style("import:").red()));
            }
        }
        // One keystroke back into the last conversation. `/sessions` could already restore, but it
        // costs a menu and knowing which of a dozen files is the newest — so in practice a reopened
        // terminal started from scratch even though the transcript was on disk the whole time.
        SlashId::Resume => {
            // Bare `/resume` carries the offer's origin label through, so opening another project's
            // conversation (only offered when this project has none) says so on the confirmation
            // line — including for pre-provenance files, where `load_session` has nothing to warn with.
            let (target, origin) = if arg.is_empty() {
                match most_recent_session() {
                    Some((slug, _, origin)) => (Some(slug), origin),
                    None => (None, None),
                }
            } else {
                (Some(sanitize_name(arg)), None)
            };
            match target {
                None => tui::emit_line(&style("no saved conversation to resume yet").dim().to_string()),
                Some(name) => match load_session(history, &name, model_label) {
                    Ok(n) => {
                        // A restore is a thread switch — the restored thread must not inherit the
                        // previous one's todos/cost/grants. Only on success: a failed load leaves
                        // the live thread (and its state) untouched.
                        reset_per_session_state();
                        // Replay so the restored thread is VISIBLE, not just present in the request:
                        // resuming into an empty-looking screen reads as "it didn't work".
                        agent::replay_transcript(history);
                        let origin_note = origin.map(|o| format!(" ({o})")).unwrap_or_default();
                        tui::emit_line(
                            &style(format!(
                                "⟲ resumed “{}”{origin_note} — {n} messages, context restored",
                                pretty_session_name(&name)
                            ))
                            .color256(splash::ACCENT)
                            .to_string(),
                        );
                    }
                    Err(e) => tui::emit_line(&format!("{} {e}", style("resume:").red())),
                },
            }
        }
        SlashId::Workflows => slash_workflows(arg).await,
        // Multi-window cooperation: who else is in this repo, what they changed, and committing one
        // window's work. `/work` manages the isolated-worktree mode.
        SlashId::Team => slash_team(arg).await,
        SlashId::Work => slash_work(arg),
        SlashId::Agents => slash_agents(arg),
        SlashId::Recover => {
            let repo_scope = crate::core::recovery::current_repo_scope();
            let offers = crate::core::recovery::scan_stale(&repo_scope);
            if offers.is_empty() {
                tui::emit_line(&style("no recoverable sessions found").dim().to_string());
            } else if arg == "discard" || arg == "drop" {
                for offer in &offers {
                    let _ = crate::core::recovery::discard(offer);
                }
                tui::emit_line(&style(format!("discarded {} recovery lease(s)", offers.len())).dim().to_string());
            } else {
                // Restore the newest offer. Side effects are never auto-replayed — only history + draft.
                let offer = &offers[0];
                match crate::core::recovery::accept(offer) {
                    Ok((restored, draft)) => {
                        *history = restored;
                        migrate_legacy_prompt_lanes(history, model_label);
                        refresh_prompt_lanes_for_thread_switch(history, model_label);
                        // Same thread-switch contract as /resume: the crashed thread's todos/cost/
                        // grants belong to it, not to whatever was live before accepting.
                        reset_per_session_state();
                        agent::replay_transcript(history);
                        if let Some(d) = draft {
                            tui::set_draft(&d);
                            tui::emit_line(&style("restored interrupted draft into the input box (not submitted)").dim().to_string());
                        }
                        if offer.manifest.side_effects_possible {
                            let checkpoint = offer
                                .manifest
                                .checkpoint_id
                                .map(|id| format!(" Check Time Machine checkpoint #{id} before retrying."))
                                .unwrap_or_else(|| " Check Time Machine before retrying.".to_string());
                            tui::emit_line(
                                &style(format!(
                                    "⚠ a previous tool may already have completed; retrying could repeat its side effect.{checkpoint}"
                                ))
                                .color256(theme::WARN)
                                .to_string(),
                            );
                        }
                    }
                    Err(e) => tui::emit_line(&format!("{} {e}", style("recover:").red())),
                }
            }
        }
        SlashId::Compact => {
            // Harvest what the older turns touched BEFORE they collapse — once summarized their tool
            // calls are gone, so the tree must read the history while it's still whole.
            let tp = agent::compact::context_touchpoints(history);
            tui::emit_line(&style("compacting… (Esc to stop)").dim().to_string());
            // Interruptible: the summarizer call is a network round-trip on the REPL's own thread.
            // Without this the whole app is frozen until it returns (or the 300s read timeout).
            match cancellable_slash(compact_now(history)).await {
                Some(Ok((b, a))) => print_compact_summary(b, a, &tp),
                Some(Err(e)) => tui::emit_line(&format!("{} {e}", style("compact:").red())),
                // Cancelled before the summary landed. `compact_history` only splices AFTER a
                // non-empty summary returns, so dropping the future leaves history untouched.
                None => tui::emit_line(&theme::muted("⏹ compact stopped — context unchanged.").to_string()),
            }
        }
        SlashId::Handoff => {
            if arg.trim().is_empty() {
                tui::emit_line(&style("usage: /handoff <new goal> — start a fresh thread carrying only what matters for it").dim().to_string());
            } else {
                tui::emit_line(&style("handing off…").dim().to_string());
                // Same cancellable wrapper as /compact: this is a blocking model call inside the
                // REPL loop, so without an armed token Esc can't reach it.
                match cancellable_slash(handoff_now(history, arg.trim())).await {
                    Some(Ok(summary)) => {
                        // Fresh thread: new system prompt, the goal-relevant extraction seeded as
                        // context, todos cleared, destructive-op session grants re-armed (like /clear).
                        rebuild_system(history, model_label);
                        // The marker prefix keeps the seed alive through lane rewrites (/config,
                        // /model, resume) — `leading_system_count` stops at it, so lane splices go
                        // around the seed instead of overwriting it.
                        history.push(Message::system(format!(
                            "{}\n{summary}",
                            agent::compact::HANDOFF_MARKER_PREFIX
                        )));
                        reset_per_session_state();
                        // The finished conversation keeps its file; the handoff starts a NEW one.
                        // Without re-slugging, the very next autosave overwrote the previous
                        // thread's saved transcript with this freshly seeded stub.
                        let previous = current_session_slug();
                        set_session_slug(None);
                        update_live_history(history);
                        tui::emit_line(&style("handoff — fresh thread seeded with the relevant context").color256(splash::ACCENT).to_string());
                        // Name the thread being left behind, so the full transcript is findable.
                        if let Some(prev) = previous {
                            tui::emit_line(
                                &style(format!("  (the previous thread stays saved as “{prev}” — /sessions to reopen it)"))
                                    .dim()
                                    .to_string(),
                            );
                        }
                        return SlashOutcome::Submit(arg.trim().to_string());
                    }
                    Some(Err(e)) => tui::emit_line(&format!("{} {e}", style("handoff:").red())),
                    // Cancelled before the extraction landed. Nothing was rebuilt, so the current
                    // thread continues untouched.
                    None => tui::emit_line(&theme::muted("⏹ handoff stopped — thread unchanged.").to_string()),
                }
            }
        }
        SlashId::Goal => {
            // Goal mode: run cap-free with smart retry until the model declares completion
            // (`goal_complete`) AND the verify gate passes. `/goal off` (or bare `/goal`) turns it off.
            let a = arg.trim();
            if a.is_empty() || a.eq_ignore_ascii_case("off") || a.eq_ignore_ascii_case("stop") {
                crate::agent::goal::set_goal(None);
                crate::agent::goal::arm(false);
                crate::agent::goal::clear();
                tui::emit_line(&style("goal mode off.").dim().to_string());
            } else {
                // Arm the tool gate + record the goal for every subsequent turn, and drain any stale
                // completion claim from a previous goal so it can't leak into this one.
                crate::agent::goal::set_goal(Some(a.to_string()));
                crate::agent::goal::arm(true);
                crate::agent::goal::clear();
                tui::emit_line(
                    &style("🎯 goal mode: running until done (self-declared + verified). Esc to cancel.")
                        .color256(splash::ACCENT)
                        .to_string(),
                );
                // Kick off immediately by submitting the goal text as the first user turn.
                return SlashOutcome::Submit(a.to_string());
            }
        }
        SlashId::Lsp => {
            use crate::agent::lsp::LSP;
            let sub = arg.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
            match sub.as_str() {
                "" | "status" | "st" => tui::emit_line(&LSP.status().render()),
                "on" | "enable" => match LSP.enable() {
                    Ok(_) => tui::emit_line(
                        &style("LSP on — references · definition · symbols · symbol_replace/insert · diagnostics (rust/python/js-ts; servers start lazily on first use; rust-analyzer can use ~1–3GB RAM). /lsp off to stop.")
                            .color256(splash::ACCENT).to_string(),
                    ),
                    Err(e) => tui::emit_line(&format!("{} {e}", style("lsp:").red())),
                },
                "off" | "disable" => {
                    LSP.disable();
                    tui::emit_line(&style("LSP off — servers shut down, RAM reclaimed.").dim().to_string());
                }
                "restart" => {
                    LSP.disable();
                    match LSP.enable() {
                        Ok(_) => tui::emit_line(&style("LSP restarted.").dim().to_string()),
                        Err(e) => tui::emit_line(&format!("{} {e}", style("lsp:").red())),
                    }
                }
                "edits" => {
                    let mode = arg.split_whitespace().nth(1).unwrap_or("").to_ascii_lowercase();
                    match mode.as_str() {
                        "on" => {
                            LSP.set_edit_feedback(true);
                            tui::emit_line(&style("LSP edit feedback on — new diagnostics fold into edit results.").dim().to_string());
                        }
                        "off" => {
                            LSP.set_edit_feedback(false);
                            tui::emit_line(&style("LSP edit feedback off.").dim().to_string());
                        }
                        _ => tui::emit_line(
                            &style(format!(
                                "usage: /lsp edits on|off  (currently {})",
                                if LSP.edit_feedback_enabled() { "on" } else { "off" }
                            ))
                            .dim()
                            .to_string(),
                        ),
                    }
                }
                other => tui::emit_line(
                    &style(format!("usage: /lsp [status|on|off|restart|edits on|off]  (unknown '{other}')")).dim().to_string(),
                ),
            }
        }
        SlashId::Reach => {
            use crate::agent::reach;
            let sub = arg.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
            match sub.as_str() {
                "" | "status" | "st" => tui::emit_line(&reach::render_passive()),
                "doctor" | "dr" | "check" => {
                    tui::emit_line(&style("probing every backend (a few seconds)…").dim().to_string());
                    let reports = reach::doctor().await;
                    tui::emit_line(&reach::render_report(&reports));
                }
                other => tui::emit_line(
                    &style(format!("usage: /reach [status|doctor]  (unknown '{other}')")).dim().to_string(),
                ),
            }
        }
        SlashId::Update => {
            // Talks to GitHub, so wrap in `cancellable_slash` for Esc; owns stdin through the
            // dialoguer picker, which is why `tui::slash_takes_stdin` suspends the frame for it.
            match cancellable_slash(features::update::run()).await {
                None => tui::emit_line(&style("update cancelled").dim().to_string()),
                Some(Err(e)) => tui::emit_line(&format!("{} {e:#}", style("update:").red())),
                Some(Ok(())) => {}
            }
        }
        SlashId::Approval => {
            let requested = arg.split_whitespace().next().unwrap_or("status");
            let mut cfg = cli_config::load();
            if requested.is_empty() || matches!(requested, "status" | "st") {
                tui::emit_line(&style(format!("approval: {} · ask=prompt · smart=read-only auto · yolo=pre-authorized", approval_mode())).dim().to_string());
            } else if let Ok(mode) = requested.parse::<ApprovalMode>() {
                cfg.set_approval_mode(mode);
                match cli_config::save(&cfg) {
                    Ok(_) => tui::emit_line(&style(format!("approval → {mode}")).color256(splash::ACCENT).to_string()),
                    Err(e) => tui::emit_line(&format!("{} {e}", style("approval:").red())),
                }
            } else {
                tui::emit_line(&style("usage: /approval ask|smart|yolo").dim().to_string());
            }
        }
        SlashId::Yolo => {
            let mut cfg = cli_config::load();
            let mode = if cfg.persisted_approval_mode() == ApprovalMode::Yolo { ApprovalMode::Ask } else { ApprovalMode::Yolo };
            cfg.set_approval_mode(mode);
            let _ = cli_config::save(&cfg);
            tui::emit_line(&style(format!("approval → {mode} (legacy /yolo alias)")).color256(splash::ACCENT).to_string());
        }
        SlashId::Smart => {
            let mut cfg = cli_config::load();
            let mode = if cfg.persisted_approval_mode() == ApprovalMode::Smart { ApprovalMode::Ask } else { ApprovalMode::Smart };
            cfg.set_approval_mode(mode);
            let _ = cli_config::save(&cfg);
            tui::emit_line(&style(format!("approval → {mode} (legacy /smart alias)")).color256(splash::ACCENT).to_string());
        }
        SlashId::Ultimate => {
            let mut cfg = cli_config::load();
            let now = !cfg.ultimate.unwrap_or(false);
            cfg.ultimate = Some(now);
            if now {
                // ultracode = max effort + orchestrate-by-default: pin max, bypass auto-detect.
                cfg.reasoning_effort = Some("max".to_string());
                cfg.auto_effort = Some(false);
            } else {
                // back to the default: auto-detect ON, no pinned tier.
                cfg.reasoning_effort = None;
                cfg.auto_effort = None;
            }
            match cli_config::save(&cfg) {
                Ok(_) if now => tui::emit_line(
                    &style("✦ ultimate ON — max reasoning effort every turn + prefers launching workflows for fan-out-able tasks. /ultimate again to turn it off.")
                        .color256(splash::ACCENT).to_string(),
                ),
                Ok(_) => tui::emit_line(&style("ultimate OFF — effort back to auto-detect, no orchestration nudge.").dim().to_string()),
                Err(e) => tui::emit_line(&format!("{} {e}", style("ultimate:").red())),
            }
            // Recolour the input box to match: gold framing while ultimate is ON (mirrors the
            // `✦ ultimate` status chip), moonlight when OFF. Reflects the effective state — an
            // env-forced ON wins over the toggle, so read it back rather than trusting `now`.
            tui::set_ultimate(cli_config::ultimate_enabled());
            if std::env::var("AIZEN_ULTIMATE").is_ok() {
                tui::emit_line(&style("(note: AIZEN_ULTIMATE is set in your environment — it forces ultimate ON regardless of this toggle)").dim().to_string());
            }
        }
        SlashId::Effort => {
            let sub = arg.trim().to_ascii_lowercase();
            match sub.as_str() {
                // No arg → the interactive drag slider (falls back to a text report off-TTY).
                "" => effort_slider_flow(),
                // `status`/`st` → the plain text report (no slider).
                "status" | "st" => effort_status_report(),
                "auto" | "on" => {
                    let mut cfg = cli_config::load();
                    cfg.auto_effort = None; // None ⇒ ON (the default); clears any explicit off.
                    match cli_config::save(&cfg) {
                        Ok(_) => tui::emit_line(
                            &style("effort auto ON — each turn's effort is detected from your message (keyword + complexity).")
                                .color256(splash::ACCENT).to_string(),
                        ),
                        Err(e) => tui::emit_line(&format!("{} {e}", style("effort:").red())),
                    }
                }
                "off" => {
                    let mut cfg = cli_config::load();
                    cfg.auto_effort = Some(false);
                    match cli_config::save(&cfg) {
                        Ok(_) => tui::emit_line(
                            &style("effort auto OFF — every turn uses the fixed reasoning_effort (or omits it if unset).")
                                .dim().to_string(),
                        ),
                        Err(e) => tui::emit_line(&format!("{} {e}", style("effort:").red())),
                    }
                }
                "low" | "medium" | "high" | "xhigh" | "max" => {
                    let mut cfg = cli_config::load();
                    cfg.reasoning_effort = Some(sub.clone());
                    cfg.auto_effort = Some(false); // pinning a fixed tier turns auto off.
                    match cli_config::save(&cfg) {
                        Ok(_) => tui::emit_line(
                            &style(format!("effort pinned to {sub} (auto off) — every turn now sends reasoning_effort={sub}."))
                                .color256(splash::ACCENT).to_string(),
                        ),
                        Err(e) => tui::emit_line(&format!("{} {e}", style("effort:").red())),
                    }
                }
                "none" | "clear" => {
                    let mut cfg = cli_config::load();
                    cfg.reasoning_effort = None;
                    cfg.auto_effort = None; // back to the default (auto ON, no fixed tier).
                    match cli_config::save(&cfg) {
                        Ok(_) => tui::emit_line(
                            &style("effort cleared — auto ON, no fixed tier (requests omit reasoning_effort unless auto detects one).")
                                .dim().to_string(),
                        ),
                        Err(e) => tui::emit_line(&format!("{} {e}", style("effort:").red())),
                    }
                }
                other => tui::emit_line(
                    &style(format!("usage: /effort [auto|off|low|medium|high|xhigh|max|none]  (unknown '{other}')")).dim().to_string(),
                ),
            }
        }
        SlashId::Provider => {
            let selected = if arg.eq_ignore_ascii_case("add") || arg.eq_ignore_ascii_case("manage") {
                let mut cfg = cli_config::load();
                config_ui::config_edit_providers(&mut cfg).await.and_then(|_| {
                    cli_config::save(&cfg)?;
                    Ok(None)
                })
            } else if arg.is_empty() {
                provider_menu().await
            } else {
                activate_provider_profile(arg).map(Some)
            };
            match selected {
                Ok(Some(profile)) => {
                    *model_label = profile.model.clone();
                    cli_config::pin_session_model(&profile.model); // switching provider is a deliberate model change for THIS window
                    refresh_prompt_lanes_in_place(history, model_label);
                    tui::emit_line(
                        &style(format!(
                            "provider → {} · {} · {}",
                            profile.name,
                            profile.model,
                            config_ui::redact_url_userinfo(&profile.base_url)
                        ))
                        .color256(splash::ACCENT)
                        .to_string(),
                    );
                    let overridden: Vec<&str> = ["BASE_URL", "API_KEY", "MODEL"]
                        .into_iter()
                        .filter(|name| cli_config::branded_env(name).is_some())
                        .collect();
                    if !overridden.is_empty() {
                        tui::emit_line(
                            &style(format!(
                                "note: AIZEN_{} override the selected profile at runtime",
                                overridden.join(" / AIZEN_")
                            ))
                            .color256(theme::WARN)
                            .to_string(),
                        );
                    }
                }
                Ok(None) => {}
                Err(e) => tui::note_line(&format!("{} {e}", style("provider:").red())),
            }
        }
        SlashId::Model => {
            if let Err(e) = slash_model(model_label).await {
                if tui::active() {
                    tui::emit_line(&format!("{} {e}", style("model:").red()));
                } else {
                    tui::note_line(&format!("{} {e}", style("model:").red()));
                }
            } else {
                // Also in place: the help text promises `/model` "switches models mid-session", and a
                // switch that silently discarded the session would make that promise a lie. The stable
                // lane carries `model:`, so it must be rewritten — see `refresh_prompt_lanes_in_place`.
                refresh_prompt_lanes_in_place(history, model_label);
            }
        }
        SlashId::Config => {
            if let Err(e) = config_ui::config_wizard().await {
                tui::note_line(&format!("{} {e}", style("config:").red()));
            }
            *model_label = cli_config::load().model.unwrap_or_else(|| model_label.clone());
            // The wizard just wrote config for THIS window on purpose, so adopt its model as the new
            // session pin rather than letting the startup pin override what the user just chose.
            cli_config::pin_session_model(model_label);
            // Refresh IN PLACE — retuning settings mid-chat must not end the conversation.
            refresh_prompt_lanes_in_place(history, model_label);
        }
        SlashId::Memory => {
            if let Err(e) = slash_memory(arg) {
                tui::note_line(&format!("{} {e}", style("memory:").red()));
            }
        }
        SlashId::Persona => {
            if let Err(e) = personas_menu(history, model_label).await {
                tui::note_line(&format!("{} {e}", style("persona:").red()));
            }
        }
        SlashId::Skills => {
            if let Err(e) = skills_menu().await {
                tui::note_line(&format!("{} {e}", style("skills:").red()));
            }
        }
        SlashId::Apps => {
            if let Err(e) = apps_menu().await {
                tui::note_line(&format!("{} {e}", style("apps:").red()));
            }
        }
        SlashId::Mcp => tui::emit_line(&crate::agent::mcp::summary()),
        SlashId::Browser => {
            #[cfg(feature = "browser")]
            {
                if matches!(arg, "doctor" | "check" | "probe") {
                    tui::emit_line(&style("probing browser profiles…").dim().to_string());
                    tui::emit_line(&crate::agent::browser::doctor().await);
                } else {
                    tui::emit_line(&crate::agent::browser::status());
                }
            }
            #[cfg(not(feature = "browser"))]
            tui::emit_line(&style("browser tools are not included in this build (build with --features browser)").dim().to_string());
        }
        SlashId::Tools => slash_tools(arg).await,
        SlashId::Commands => match commands::summary() {
            Some(s) => tui::emit_line(&style(s).dim().to_string()),
            None => tui::emit_line(
                &style("No custom commands yet. Drop a markdown file in ~/.aizen/commands/ (or ./.aizen/commands/ for this project) — see /help.").dim().to_string()
            ),
        },
        SlashId::Telegram => {
            if let Err(e) = telegram_menu().await {
                tui::note_line(&format!("{} {e}", style("telegram:").red()));
            }
        }
        // `/serve` kept as a direct shortcut to the daemon (also reachable via the Telegram menu).
        SlashId::Serve => {
            if let Err(e) = hostbot::run_serve(Vec::new()).await {
                tui::note_line(&format!("{} {e}", style("serve:").red()));
            }
        }
        // ── time machine (git snapshots) ──
        // `/timemachine` is ONE command: it opens the checkpoint list, and picking a row rewinds to
        // that code + chat. No `pick`/`restore` argument to remember, no separate read-only print —
        // the list itself shows the history, so browsing and restoring are the same gesture (Esc
        // leaves without touching anything).
        SlashId::Timemachine => {
            if let Err(e) = timemachine_menu(history, model_label).await {
                tui::note_line(&format!("{} {e}", style("time:").red()));
            }
        }
        // Capture the conversation alongside the tree so a pick in `/timemachine` can rewind chat as
        // well as code — a `/checkpoint` is a deliberate save point where the chat is worth keeping,
        // unlike the loop's per-edit auto-snapshots (which restore files only).
        SlashId::Checkpoint => match timemachine::save_with_chat(arg, false, history) {
            Ok(s) => tui::emit_line(&format!(
                "{} #{} saved ({})",
                style("✓ checkpoint").color256(splash::ACCENT),
                s.id,
                if s.has_chat { "code + chat" } else { "files only" }
            )),
            Err(e) => tui::emit_line(&style(format!("checkpoint: {e}")).color256(crate::ui::theme::WARN).to_string()),
        },
        // `/diff` — see what changed before deciding to rewind. Argument forms mirror the CLI:
        // bare = active checkpoint vs disk, `#5` = that checkpoint vs disk, `#1 #2` = the pair.
        // `-p`/`--patch` anywhere switches from stat to hunks; anything after `--` narrows to paths.
        SlashId::Diff => {
            let mut sides: Vec<String> = Vec::new();
            let mut paths: Vec<String> = Vec::new();
            let mut patch = false;
            let mut after_sep = false;
            for tok in arg.split_whitespace() {
                match tok {
                    "--" => after_sep = true,
                    "-p" | "--patch" => patch = true,
                    _ if after_sep => paths.push(tok.to_string()),
                    _ => sides.push(tok.to_string()),
                }
            }
            let (from, to) = (sides.first().cloned(), sides.get(1).cloned());
            match build_time_diff(from, to, paths, patch) {
                // Must go through `emit_line`: raw `println!` from inside the REPL is wiped by the
                // retained render thread's next repaint.
                Ok(report) => {
                    for line in diff_lines(&report, "-- <path>") {
                        tui::emit_line(&line);
                    }
                }
                Err(e) => tui::emit_line(&style(format!("diff: {e}")).color256(crate::ui::theme::WARN).to_string()),
            }
        }
        SlashId::Undo => match timemachine::undo() {
            Ok(s) => tui::emit_line(&format!("{} checkpoint #{}", style("⏪ rewound to").color256(splash::ACCENT), s.id)),
            Err(e) => tui::emit_line(&style(format!("undo: {e}")).color256(crate::ui::theme::WARN).to_string()),
        },
        SlashId::Redo => match timemachine::redo() {
            Ok(s) => tui::emit_line(&format!("{} checkpoint #{}", style("⏩ re-applied").color256(splash::ACCENT), s.id)),
            Err(e) => tui::emit_line(&style(format!("redo: {e}")).color256(crate::ui::theme::WARN).to_string()),
        },
        // Codebase index: scan the repo into a semantic chunk index for `codebase_search` +
        // per-turn retrieval injection. `/init` incrementally refreshes; `/init --force` rebuilds
        // from scratch; `/init --status` shows the current index without scanning. Esc cancels.
        SlashId::Init => {
            slash_init(arg).await;
        }
    }
    SlashOutcome::Continue
}
/// Not a built-in: either a user-defined command (`~/.aizen/commands/<name>.md`) whose template we
/// expand and run as a normal chat turn, or nothing we know at all.
///
/// Split out of the dispatch match so that match can be exhaustive over [`SlashId`] — a catch-all
/// arm would have silently absorbed any command whose handler someone forgot to write.
fn slash_custom_or_unknown(name: &str, arg: &str) -> SlashOutcome {
    match commands::find(name) {
        Some(cmd) => match commands::expand(&cmd, arg) {
            Ok(prompt) if !prompt.trim().is_empty() => return SlashOutcome::Submit(prompt),
            Ok(_) => tui::emit_line(
                &style(format!("/{name} expanded to an empty prompt"))
                    .dim()
                    .to_string(),
            ),
            Err(e) => tui::emit_line(&format!("{} {e}", style(format!("/{name}:")).red())),
        },
        None => tui::emit_line(
            &style(format!("unknown command /{name} — try /help"))
                .dim()
                .to_string(),
        ),
    }
    SlashOutcome::Continue
}
