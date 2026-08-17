//! `/sessions`, `/import` and `aizen import` — loading a saved conversation over the live one.
//!
//! One store holds sessions from every project this binary has run in, plus transcripts imported
//! from other CLIs (Claude Code, Codex), so everything here leads with provenance: whose project a
//! file came from, how old it is, which one is live, and which ones will not parse.

use crate::agent;
use crate::agent::prompt_lanes::{refresh_prompt_lanes_for_thread_switch, reset_per_session_state};
use crate::core::config;
use crate::core::session_store::*;
use crate::core::types::Message;
use crate::features;
use crate::ui::{icons, splash, tui};
use crate::ui_theme;
use anyhow::{Context, Result};
use console::style;
use dialoguer::{Confirm, Input, Select};

/// `/sessions` — pick a saved conversation and load it over the live one.
///
/// Newest first, with provenance (age, origin project, a marker on the live file) because the same
/// store holds sessions from every project this binary has ever run in. A file that fails to parse
/// is listed as unreadable rather than as a plausible empty session — see the serialize/deserialize
/// asymmetry that used to hide those.
pub(crate) async fn sessions_menu(history: &mut Vec<Message>, model_label: &str) -> Result<()> {
    loop {
        let theme = ui_theme();
        // Newest first, with provenance: age, origin project for foreign/unlabeled files, a
        // "● current" marker on the live conversation's file, and corrupt files called out as
        // unreadable instead of posing as plausible "0 msgs" sessions.
        let pool = scan_sessions();
        let names: Vec<String> = pool.iter().map(|s| s.name.clone()).collect();
        let n_sessions = pool.len();
        let current = current_session_slug();
        let mut items: Vec<String> = pool
            .iter()
            .map(|s| {
                let count = match s.msgs {
                    Some(n) => format!("{n} msg{}", if n == 1 { "" } else { "s" }),
                    None => "(unreadable)".to_string(),
                };
                let mut row = format!(
                    "{} {}  —  {count} · {}",
                    icons::g(icons::slash("sessions")),
                    s.name,
                    fmt_session_age(s.mtime_ms)
                );
                if s.here == Some(false) {
                    row.push_str(&format!(" · {}", session_origin_label(s.meta.as_ref())));
                }
                if current.as_deref() == Some(s.name.as_str()) {
                    row.push_str(" · ● current");
                }
                row
            })
            .collect();
        items.push("+ Save current conversation…".to_string());
        if n_sessions > 0 {
            items.push("Delete a session".to_string());
        }
        items.push("Back".to_string());

        let prompt = if n_sessions == 0 {
            "Sessions — none saved yet (Esc to go back)".to_string()
        } else {
            format!("Sessions — {n_sessions} saved, newest first · pick one to restore (Esc to go back)")
        };
        let pick = match Select::with_theme(&theme)
            .with_prompt(prompt)
            .items(&items)
            .default(0)
            .interact_opt()?
        {
            Some(i) => i,
            None => return Ok(()),
        };

        if pick < n_sessions {
            let name = &names[pick];
            match load_session(history, name, model_label) {
                Ok(n) => {
                    reset_per_session_state(); // thread switch — same contract as /resume
                    println!(
                        "{}",
                        style(format!(
                            "restored '{}' ({n} messages)",
                            pretty_session_name(name)
                        ))
                        .color256(splash::ACCENT)
                    );
                    agent::replay_transcript(history);
                    return Ok(());
                }
                Err(e) => tui::note_line(&format!("{} {e}", style("restore:").red())),
            }
        } else if items[pick].starts_with("+ Save") {
            let suggested = suggest_session_name(history);
            let name: String = Input::with_theme(&theme)
                .with_prompt("Save as")
                .with_initial_text(suggested)
                .interact_text()
                .unwrap_or_default();
            if !name.trim().is_empty() {
                // Saving over a DIFFERENT conversation's file is destructive — confirm it. Re-saving
                // the live conversation's own file is the normal case and stays silent.
                let target = sanitize_name(name.trim());
                if let Some(why) = session_save_name_error(&name) {
                    eprintln!("{} {why}", style("save:").red());
                    continue;
                }
                let exists = sessions_dir().join(format!("{target}.json")).exists();
                if exists && current.as_deref() != Some(target.as_str()) {
                    let overwrite = Confirm::with_theme(&theme)
                        .with_prompt(format!(
                            "'{}' already exists — overwrite it?",
                            pretty_session_name(&target)
                        ))
                        .default(false)
                        .interact_opt()?
                        .unwrap_or(false);
                    if !overwrite {
                        continue;
                    }
                }
                match save_session(history, name.trim(), Some(model_label)) {
                    Ok(_) => {
                        // Pin the session to this name so later autosaves keep rewriting the SAME file
                        // (both paths route through `sanitize_name`, so the raw name maps to one file).
                        set_session_slug(Some(name.trim().to_string()));
                        update_live_history(history); // exit-flush snapshot follows the pinned name
                        println!(
                            "{}",
                            style(format!("saved '{}'", name.trim())).color256(splash::ACCENT)
                        );
                    }
                    Err(e) => tui::note_line(&format!("{} {e}", style("save:").red())),
                }
            }
        } else if items[pick] == "Delete a session" {
            if let Ok(Some(i)) = Select::with_theme(&theme)
                .with_prompt("Delete which session? (Esc to cancel)")
                .items(&names)
                .default(0)
                .interact_opt()
            {
                let slug = &names[i];
                let pretty = pretty_session_name(slug);
                let confirmed = Confirm::with_theme(&theme)
                    .with_prompt(format!("Delete '{pretty}' permanently?"))
                    .default(false)
                    .interact_opt()?
                    .unwrap_or(false);
                if !confirmed {
                    continue;
                }
                match delete_session(slug) {
                    Ok(_) => {
                        if current_session_slug().as_deref() == Some(slug.as_str()) {
                            set_session_slug(None);
                        }
                        println!(
                            "{}",
                            style(format!("deleted '{pretty}'")).color256(splash::ACCENT)
                        );
                    }
                    Err(e) => tui::note_line(&format!("{} {e}", style("delete:").red())),
                }
            }
        } else {
            return Ok(()); // Back
        }
    }
}

/// `/import` — pick a conversation recorded by Claude Code or Codex (for THIS project) and resume
/// it inside aizen. The foreign transcript is parsed into `Vec<Message>`, repaired so it satisfies
/// `assert_valid_history`, then handed to the SAME thread-switch path `/resume` uses: refresh the
/// prompt lanes for the current project + model, reset per-session state, and replay so the
/// restored thread is VISIBLE rather than silently present.
///
/// Foreign transcripts are never autosaved back over themselves — the imported conversation becomes
/// the live one and is autosaved under a fresh aizen slug from then on, exactly like a `/resume`
/// of an aizen session. The source file is only ever READ.
pub(crate) async fn import_menu(history: &mut Vec<Message>, model_label: &str) -> Result<()> {
    let theme = ui_theme();
    let pool = features::foreign_session::discover(&config::project_root());
    if pool.is_empty() {
        tui::emit_line(
            &style("no Claude Code or Codex transcripts found for this project")
                .dim()
                .to_string(),
        );
        tui::emit_line(
            &style("(they appear here once you've used `claude` or `codex` in this directory)")
                .dim()
                .to_string(),
        );
        return Ok(());
    }
    // Clip to the terminal, minus dialoguer's own `❯ ` prefix and one cell of right margin. An item
    // that overflows gets WRAPPED onto a second line, which breaks the column alignment for every row
    // below it — the whole reason the list is scannable.
    let width = console::Term::stdout().size().1 as usize;
    let items: Vec<String> = pool
        .iter()
        .map(|s| s.row(fmt_session_age_compact, width.saturating_sub(4).max(24)))
        .collect();
    let prompt = format!("Import — {} conversations from claude/codex", pool.len());
    let pick = match Select::with_theme(&theme)
        .with_prompt(prompt)
        .items(&items)
        .default(0)
        .interact_opt()?
    {
        Some(i) => i,
        None => return Ok(()),
    };
    let session = &pool[pick];
    match features::foreign_session::load(session) {
        Ok(imported) => {
            // Drop any system lanes the source CLI's harness left in (already filtered in parse,
            // but defend against a future schema that embeds them) before splicing aizen's own.
            *history = imported;
            refresh_prompt_lanes_for_thread_switch(history, model_label);
            // Same thread-switch contract as /resume: the foreign thread's todos/cost/grants
            // belong to it, not to whatever was live before the import.
            reset_per_session_state();
            set_session_slug(None); // the imported chat autosaves under a fresh aizen slug from here
            agent::replay_transcript(history);
            let tag = match session.cli {
                features::foreign_session::Cli::Claude => "claude",
                features::foreign_session::Cli::Codex => "codex",
            };
            tui::emit_line(
                &style(format!(
                    "⇲ imported “{}” from {} — {} messages, context restored",
                    if session.first_prompt.is_empty() {
                        "(no prompt)"
                    } else {
                        &session.first_prompt
                    },
                    tag,
                    history.len()
                ))
                .color256(splash::ACCENT)
                .to_string(),
            );
            tui::emit_line(
                &style(format!("  source: {}", session.path.display()))
                    .dim()
                    .to_string(),
            );
        }
        Err(e) => tui::emit_line(&format!("{} {e}", style("import:").red())),
    }
    Ok(())
}

/// `aizen import [path]` — CLI surface. No path lists every foreign transcript for this project;
/// a path loads that file's transcript and prints a one-line summary (the CLI can't resume into a
/// REPL, so it reports what WOULD be loaded — the actual resume happens via `/import` in the REPL).
pub(crate) async fn run_import(path: Option<String>) -> Result<()> {
    match path {
        Some(p) => {
            let p = std::path::PathBuf::from(&p);
            let bytes = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
            // Detect the CLI from the file's own shape rather than the path: a Claude line has a
            // top-level `type` that is "user"/"assistant"/"mode"/…; a Codex line has `session_meta`
            // or `response_item`. Sniff the first parseable line.
            let cli = sniff_cli(&bytes);
            let sess = features::foreign_session::ForeignSession {
                cli,
                path: p.clone(),
                cwd: String::new(),
                mtime_ms: None,
                turns: 0,
                first_prompt: String::new(),
            };
            match features::foreign_session::load(&sess) {
                Ok(msgs) => {
                    let tag = match cli {
                        features::foreign_session::Cli::Claude => "claude",
                        features::foreign_session::Cli::Codex => "codex",
                    };
                    println!(
                        "{}",
                        style(format!(
                            "⇲ {} transcript — {} messages ready to resume",
                            tag,
                            msgs.len()
                        ))
                        .color256(splash::ACCENT)
                    );
                    println!("  source: {}", p.display());
                    println!(
                        "  {}",
                        style("open the REPL in this project and run /import to resume it").dim()
                    );
                    Ok(())
                }
                Err(e) => anyhow::bail!("import: {e}"),
            }
        }
        None => {
            let pool = features::foreign_session::discover(&config::project_root());
            if pool.is_empty() {
                println!(
                    "{}",
                    style("no Claude Code or Codex transcripts found for this project").dim()
                );
                println!(
                    "{}",
                    style(
                        "(they appear here once you've used `claude` or `codex` in this directory)"
                    )
                    .dim()
                );
                return Ok(());
            }
            println!(
                "{}",
                style(format!(
                    "Foreign transcripts for this project ({}), newest first:",
                    pool.len()
                ))
                .color256(splash::ACCENT)
            );
            for s in &pool {
                let tag = match s.cli {
                    features::foreign_session::Cli::Claude => "claude",
                    features::foreign_session::Cli::Codex => "codex",
                };
                println!(
                    "  [{}] {:>6} · {} turns · {}",
                    tag,
                    fmt_session_age(s.mtime_ms),
                    s.turns,
                    s.path.display()
                );
            }
            println!(
                "{}",
                style("resume one with: /import  (in the REPL)  or  aizen import <path>").dim()
            );
            Ok(())
        }
    }
}

/// Decide which CLI a transcript belongs to from its content, not its path. Falls back to Claude
/// (the more permissive parser — it ignores unrecognized line types) when the sniff is inconclusive.
pub(crate) fn sniff_cli(bytes: &[u8]) -> features::foreign_session::Cli {
    for line in bytes.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if ty == "session_meta"
            || ty == "response_item"
            || ty == "event_msg"
            || ty == "turn_context"
        {
            return features::foreign_session::Cli::Codex;
        }
        if ty == "user" || ty == "assistant" || ty == "mode" || ty == "file-history-snapshot" {
            return features::foreign_session::Cli::Claude;
        }
    }
    features::foreign_session::Cli::Claude
}
