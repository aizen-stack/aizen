//! `aizen time …` and `/timemachine` — the git-backed checkpoint timeline.
//!
//! Checkpoints are commits in a store that borrows objects from the working repo, so saving is cheap
//! and restoring is reversible (the pre-restore tree is itself checkpointed first). `diff_lines` and
//! `build_time_diff` are shared with the slash surface so the CLI and the REPL cannot drift.

use crate::agent::prompt_lanes::*;
use crate::core::cli_config;
use crate::core::session_store::*;
use crate::core::types::Message;
use crate::features::timemachine;
use crate::ui::{splash, tui};
use crate::{cli_args::TimeCmd, ui_theme};
use anyhow::{bail, Context, Result};
use console::style;
use dialoguer::{Input, Select};

pub(crate) fn run_time(cmd: TimeCmd) -> Result<()> {
    match cmd {
        TimeCmd::Save { label } => {
            let snap = timemachine::save(&label.join(" "), false)?;
            println!(
                "{} #{}  {}",
                style("✓ checkpoint").color256(splash::ACCENT),
                snap.id,
                style(&snap.created).dim()
            );
            Ok(())
        }
        TimeCmd::List => {
            print_timeline()?;
            Ok(())
        }
        TimeCmd::Restore { id } => {
            let snap = timemachine::restore(id)?;
            let label = if snap.label.is_empty() {
                "(no label)".to_string()
            } else {
                snap.label.clone()
            };
            println!(
                "{} #{} — {label}",
                style("⏪ restored to").color256(splash::ACCENT),
                snap.id
            );
            // Say WHAT changed and that it's undoable: aizen only rewinds the working tree (files),
            // never your chat/history — and because the pre-restore state was auto-snapshotted, you
            // can always go forward again (`aizen time redo`, or restore the newest checkpoint).
            println!("{}", style("  files only — your conversation is untouched · reversible with `aizen time redo`").dim());
            Ok(())
        }
        TimeCmd::Diff {
            from,
            to,
            patch,
            paths,
            json,
        } => run_time_diff(from, to, paths, patch, json),
        TimeCmd::Undo => {
            let snap = timemachine::undo()?;
            println!(
                "{} #{}",
                style("⏪ undo →").color256(splash::ACCENT),
                snap.id
            );
            Ok(())
        }
        TimeCmd::Redo => {
            let snap = timemachine::redo()?;
            println!(
                "{} #{}",
                style("⏩ redo →").color256(splash::ACCENT),
                snap.id
            );
            Ok(())
        }
        TimeCmd::Prune { keep } => {
            let k = keep.or(cli_config::load().timemachine_keep).unwrap_or(50);
            let dropped = timemachine::prune(k)?;
            println!(
                "{} {dropped} old checkpoint(s); kept ≤{k}.",
                style("🧹 pruned").color256(splash::ACCENT)
            );
            Ok(())
        }
        TimeCmd::Doctor { json, repair } => {
            let report = if repair {
                timemachine::doctor_repair()?
            } else {
                timemachine::doctor()?
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "{}  repo {} · worktree {} · {} checkpoint(s)",
                    if report.ok {
                        "✓ time machine healthy"
                    } else {
                        "⚠ time machine needs attention"
                    },
                    report.repo_id,
                    report.worktree_id,
                    report.checkpoints
                );
                println!("  store {}", report.store);
                // Loose objects are not an "issue" — a store with 15k of them still restores fine.
                // They are a disk-growth signal, so surface them only once there are enough to matter
                // and say what fixes it, rather than printing a zero on every healthy run.
                if report.loose_objects >= 1024 {
                    println!(
                        "  {} loose object(s) not packed — run `aizen time gc` to compact",
                        report.loose_objects
                    );
                }
                for issue in &report.issues {
                    println!("  - {issue}");
                }
            }
            if !report.ok {
                bail!("time-machine doctor found {} issue(s)", report.issues.len());
            }
            Ok(())
        }
        TimeCmd::Gc { all, apply } => {
            if all {
                let report = timemachine::gc_all(apply)?;
                let fmt_mb = |b: u64| format!("{:.1} MB", b as f64 / 1_048_576.0);
                if report.orphans.is_empty() {
                    println!(
                        "{} {} store(s) scanned · no orphans (every source repo still exists)",
                        style("🧹 time gc --all:").color256(splash::ACCENT),
                        report.stores.len()
                    );
                } else {
                    let total: u64 = report.orphans.iter().map(|o| o.bytes).sum();
                    println!(
                        "{} {} orphan store(s), {} reclaimable:",
                        style("🧹 time gc --all:").color256(splash::ACCENT),
                        report.orphans.len(),
                        fmt_mb(total)
                    );
                    for o in &report.orphans {
                        println!(
                            "  {} · {} · {} checkpoint(s) · source gone: {}",
                            o.repo_id,
                            fmt_mb(o.bytes),
                            o.checkpoints,
                            o.source.as_deref().unwrap_or("(unknown)")
                        );
                    }
                    if report.applied {
                        // `--apply` RENAMES; nothing here or anywhere else ever empties the trash.
                        // Saying only "moved" alongside a "reclaimable" figure reads as though the
                        // disk came back, and it has not — so name the step that actually frees it.
                        let trash = report.trash_dir.as_deref().unwrap_or("(trash)");
                        println!("  → moved to {trash}");
                        println!(
                            "  {} still on disk until you delete that directory — nothing reaps it for you",
                            style("note:").bold()
                        );
                    } else {
                        println!(
                            "  (dry-run — re-run with {} to move these to .trash/)",
                            style("--apply").bold()
                        );
                    }
                }
                Ok(())
            } else {
                let (report, compacted) = timemachine::doctor_gc()?;
                println!(
                    "{} repo {} · worktree {} · {} checkpoint(s)",
                    style("🧹 time metadata cleaned:").color256(splash::ACCENT),
                    report.repo_id,
                    report.worktree_id,
                    report.checkpoints
                );
                // Report bytes actually returned, not objects touched: packing 14k objects sounds
                // like work, but the number the user came for is how much smaller the store got.
                if let Some(c) = compacted.filter(|c| c.packed > 0) {
                    // Scale the unit: a small store printed as "0.0 MB → 0.0 MB" reads as a no-op
                    // when 36 objects were in fact packed.
                    let size = |b: u64| {
                        if b >= 1_048_576 {
                            format!("{:.1} MB", b as f64 / 1_048_576.0)
                        } else {
                            format!("{:.0} KB", b as f64 / 1024.0)
                        }
                    };
                    println!(
                        "  packed {} loose object(s) · {} → {}",
                        c.packed,
                        size(c.before_bytes),
                        size(c.after_bytes)
                    );
                }
                Ok(())
            }
        }
        TimeCmd::Clear => {
            let n = timemachine::clear()?;
            println!(
                "{} {n} checkpoint(s) deleted.",
                style("🧹 cleared").color256(splash::ACCENT)
            );
            Ok(())
        }
    }
}

/// Resolve the `[FROM] [TO]` positional pair into two timeline sides.
///
/// The defaults encode the question people actually ask. Bare `time diff` means "what have I changed
/// since the last checkpoint" (cursor → working tree), which is the state you want before deciding
/// whether to keep or rewind. One argument means "since THAT point" (given → working tree), because
/// naming a single checkpoint and getting a checkpoint↔checkpoint diff against an unnamed second
/// point would be guesswork.
fn resolve_diff_sides(
    from: Option<&str>,
    to: Option<&str>,
) -> Result<(timemachine::DiffSide, timemachine::DiffSide)> {
    use timemachine::DiffSide;
    let parse = |s: &str| {
        DiffSide::parse(s).with_context(|| {
            format!("`{s}` is not a checkpoint id or `working` (try `aizen time list`)")
        })
    };
    match (from, to) {
        (None, _) => {
            let (snaps, cursor) = timemachine::timeline()?;
            let cur = cursor.and_then(|i| snaps.get(i)).map(|s| s.id).context(
                "no checkpoints yet — nothing to diff against (`aizen time save` first)",
            )?;
            Ok((DiffSide::Checkpoint(cur), DiffSide::Working))
        }
        (Some(f), None) => Ok((parse(f)?, DiffSide::Working)),
        (Some(f), Some(t)) => Ok((parse(f)?, parse(t)?)),
    }
}

/// Render a diff report as display lines. Shared so `aizen time diff` (stdout) and `/diff` (the TUI,
/// which MUST go through `tui::emit_line` or the render thread wipes the output) format identically.
/// `narrow_hint` differs between the two because the flag spelling does: `--path p` vs `-- p`.
pub(crate) fn diff_lines(report: &timemachine::DiffReport, narrow_hint: &str) -> Vec<String> {
    if report.is_empty() {
        return vec![style(format!(
            "⎇ no changes between {} and {}",
            report.from, report.to
        ))
        .dim()
        .to_string()];
    }
    let mut out = vec![format!(
        "{}  {} → {}  ·  {} file(s), {}",
        style("⎇ diff").color256(splash::ACCENT).bold(),
        report.from,
        report.to,
        report.files.len(),
        style(format!(
            "+{} -{}",
            report.total_added(),
            report.total_deleted()
        ))
        .dim(),
    )];
    for f in &report.files {
        // `None` counts mean git reported `-`: a binary file, not a zero-line change.
        let churn = match (f.added, f.deleted) {
            (Some(a), Some(d)) => format!("+{a} -{d}"),
            _ => "binary".to_string(),
        };
        let path = match &f.old_path {
            Some(old) => format!("{old} → {}", f.path),
            None => f.path.clone(),
        };
        out.push(format!("  {} {path}  {}", f.status, style(churn).dim()));
    }
    match &report.patch {
        Some(text) => {
            out.push(String::new());
            out.extend(text.lines().map(|l| l.to_string()));
            if report.patch_truncated {
                out.push(
                    style(format!("… patch truncated — narrow it with {narrow_hint}"))
                        .dim()
                        .to_string(),
                );
            }
        }
        None => out.push(
            style(format!(
                "  --patch for the full text · {narrow_hint} to narrow it"
            ))
            .dim()
            .to_string(),
        ),
    }
    out
}

/// `aizen time diff` — print the changes between two points in the timeline.
fn run_time_diff(
    from: Option<String>,
    to: Option<String>,
    paths: Vec<String>,
    patch: bool,
    json: bool,
) -> Result<()> {
    let report = build_time_diff(from, to, paths, patch)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    for line in diff_lines(&report, "--path <p>") {
        println!("{line}");
    }
    Ok(())
}

/// Cap on emitted patch bytes. Generous enough for a real review, bounded so a huge rewrite cannot
/// flood the terminal (or, for the agent tool, the tool-result budget).
const DIFF_PATCH_LIMIT: usize = 400 * 1024;

pub(crate) fn build_time_diff(
    from: Option<String>,
    to: Option<String>,
    paths: Vec<String>,
    patch: bool,
) -> Result<timemachine::DiffReport> {
    let (a, b) = resolve_diff_sides(from.as_deref(), to.as_deref())?;
    timemachine::diff(&a, &b, &paths, patch.then_some(DIFF_PATCH_LIMIT))
}

/// Human "2m ago" from a snapshot's stored LOCAL timestamp. Pure core (`rel_time_from`) takes `now`
/// so the bucketing is unit-testable; a malformed timestamp degrades to the raw string.
fn rel_time(created: &str) -> String {
    rel_time_from(created, chrono::Local::now().naive_local())
}
fn rel_time_from(created: &str, now: chrono::NaiveDateTime) -> String {
    match chrono::NaiveDateTime::parse_from_str(created, "%Y-%m-%d %H:%M:%S") {
        Ok(t) => {
            let secs = (now - t).num_seconds();
            if secs < 0 {
                "just now".to_string()
            } else if secs < 60 {
                format!("{secs}s ago")
            } else if secs < 3600 {
                format!("{}m ago", secs / 60)
            } else if secs < 86_400 {
                format!("{}h ago", secs / 3600)
            } else {
                format!("{}d ago", secs / 86_400)
            }
        }
        Err(_) => created.to_string(),
    }
}

#[cfg(test)]
#[path = "../tests/rel_time.rs"]
mod rel_time_tests;

/// `aizen time list` — a static, glanceable print of the checkpoint timeline (newest first), with the
/// active point marked `▸`, relative times, labels, and `auto`/`+chat` tags. Read-only, and CLI-only:
/// in the REPL `/timemachine` shows the same history as a picker, so there is nothing to print.
fn print_timeline() -> Result<()> {
    let (snaps, cursor) = timemachine::timeline()?;
    if snaps.is_empty() {
        tui::emit_line(
            &style("⎇ timeline — no checkpoints yet · /checkpoint to save one")
                .dim()
                .to_string(),
        );
        return Ok(());
    }
    let n = snaps.len();
    tui::emit_line(&format!(
        "{}  {n} checkpoint(s)",
        style("⎇ timeline").color256(splash::ACCENT).bold(),
    ));
    // Align the `#id` column to the widest id present (+1 for the leading `#`).
    let id_w = snaps
        .iter()
        .map(|s| s.id.to_string().len())
        .max()
        .unwrap_or(1)
        + 1;
    // Newest first (the ledger stores oldest → newest).
    for (i, s) in snaps.iter().enumerate().rev() {
        let is_cur = Some(i) == cursor;
        let id = format!("#{}", s.id);
        let rel = rel_time(&s.created);
        let label = if s.label.is_empty() {
            "(no label)".to_string()
        } else {
            s.label.clone()
        };
        let mut tags = String::new();
        if s.auto {
            tags.push_str(" · auto");
        }
        if s.has_chat {
            tags.push_str(" · +chat");
        }
        let head = format!("{id:<id_w$}  {rel:<9}  {label}");
        let mark = if is_cur { "▸" } else { " " };
        // Current point accented; tags always dim. Style the marker+head as one segment, then append
        // the dim tags separately so no ANSI code nests inside another.
        let body = if is_cur {
            format!(
                "{} {}",
                style(mark).color256(splash::ACCENT).bold(),
                style(head).color256(splash::ACCENT)
            )
        } else {
            format!("{mark} {head}")
        };
        let tag_str = if tags.is_empty() {
            String::new()
        } else {
            style(tags).dim().to_string()
        };
        tui::emit_line(&format!("{body}{tag_str}"));
    }
    tui::emit_line(
        &style("▸ = current · restore: aizen time restore <id>   (or /timemachine in the REPL)")
            .dim()
            .to_string(),
    );
    Ok(())
}

/// `/timemachine` — the whole time machine in one list: every checkpoint, and picking one rewinds to
/// that state.
///
/// A row carries the id, how long ago it was taken, its label, and the `+chat` tag when the
/// conversation was captured alongside the tree. Picking a row restores everything that checkpoint
/// holds — the working tree always, and the conversation too whenever a chat sidecar exists — so one
/// pick returns you to that code AND that chat. There is deliberately no Files/Task/Both sub-menu and
/// no `pick`/`restore` argument: this list IS the surface.
///
/// Every restore is reversible: the pre-restore tree is auto-snapshotted, and the live conversation is
/// saved to its own session file before being replaced.
pub(crate) async fn timemachine_menu(
    history: &mut Vec<Message>,
    model_label: &mut String,
) -> Result<()> {
    let theme = ui_theme();
    loop {
        let (snaps, cursor) = match timemachine::timeline() {
            Ok(t) => t,
            Err(e) => {
                println!("{e}");
                return Ok(());
            }
        };
        let n = snaps.len();
        let mut items: Vec<String> = snaps
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let here = if Some(i) == cursor { "▸ " } else { "  " };
                let label = if s.label.is_empty() {
                    "(no label)".to_string()
                } else {
                    s.label.clone()
                };
                // Spell out what a pick will rewind, since the pick itself is now the whole gesture.
                let scope = if s.has_chat {
                    "code + chat"
                } else {
                    "code only"
                };
                let tags = if s.auto {
                    format!(" · auto · {scope}")
                } else {
                    format!(" · {scope}")
                };
                format!(
                    "{here}#{} {}  {label}{}",
                    s.id,
                    style(rel_time(&s.created)).dim(),
                    style(tags).dim(),
                )
            })
            .collect();
        items.push("✚ Save a checkpoint now (code + chat)".to_string());
        items.push("Back".to_string());
        let prompt = format!(
            "Time machine — {n} checkpoint(s); pick one to rewind to it (reversible). Esc to go back"
        );
        let pick = match Select::with_theme(&theme)
            .with_prompt(prompt)
            .items(&items)
            .default(cursor.unwrap_or(0))
            .interact_opt()?
        {
            Some(i) => i,
            None => return Ok(()),
        };
        if pick < n {
            restore_checkpoint(&snaps[pick], history, model_label)?;
        } else if pick == n {
            let label: String = Input::with_theme(&theme)
                .with_prompt("Label (optional)")
                .allow_empty(true)
                .interact_text()?;
            // Capture the conversation alongside the tree so this checkpoint supports task restore.
            match timemachine::save_with_chat(label.trim(), false, history) {
                Ok(s) => println!(
                    "{} #{} ({})",
                    style("✓ checkpoint").color256(splash::ACCENT),
                    s.id,
                    if s.has_chat {
                        "code + chat"
                    } else {
                        "files only"
                    }
                ),
                Err(e) => println!("{}", style(format!("save failed: {e}")).red()),
            }
        } else {
            return Ok(());
        }
    }
}

/// Rewind to everything a checkpoint holds: the working tree, plus the conversation when that
/// checkpoint captured one.
///
/// There is no Files / Task / Both question any more — picking a row in the time machine means "put me
/// back there", and what that restores is a property of the checkpoint, not a choice to re-litigate.
/// A `/checkpoint` (or one saved from the picker) carries a chat sidecar and rewinds code + chat; an
/// auto/agent checkpoint has no sidecar and rewinds code only, which the row already says.
fn restore_checkpoint(
    snap: &timemachine::Snapshot,
    history: &mut Vec<Message>,
    model_label: &mut String,
) -> Result<()> {
    // Files-only checkpoints (auto/agent) have no chat to restore.
    if !snap.has_chat {
        return files_restore(snap.id);
    }
    // Preflight and durably back up chat BEFORE files move. The live `history` is only assigned
    // after file restore succeeds, so a failed files phase cannot leave files/chat divergent.
    let chat = timemachine::load_chat_checked(snap.id)?;
    if chat.is_empty() {
        bail!("checkpoint #{} has an empty saved conversation", snap.id);
    }
    let backup = current_session_slug().unwrap_or_else(|| allocate_session_slug(history));
    save_session(history, &backup, Some(model_label))
        .context("backing up the current conversation before restore")?;
    files_restore(snap.id)?;
    *history = chat;
    migrate_legacy_prompt_lanes(history, model_label);
    refresh_prompt_lanes_for_thread_switch(history, model_label);
    // The rewound thread continues under a NEW file — keeping the old slug would make the
    // next autosave overwrite the backup that was just written.
    set_session_slug(None);
    update_live_history(history);
    println!(
        "{} #{} — files and conversation rewound",
        style("⏪ restored").color256(splash::ACCENT),
        snap.id
    );
    println!(
        "{}",
        style(format!(
            "  (your previous chat was saved as “{backup}” — /sessions to get it back)"
        ))
        .dim()
    );
    Ok(())
}

/// Rewind only the working tree to checkpoint `id` (reversible — pre-restore tree auto-saved).
fn files_restore(id: u32) -> Result<()> {
    let s = timemachine::restore(id).with_context(|| format!("restoring checkpoint #{id}"))?;
    println!(
        "{} #{} — files rewound; your chat is untouched",
        style("⏪ restored").color256(splash::ACCENT),
        s.id
    );
    println!(
        "{}",
        style("  (reversible — the pre-restore tree was auto-saved; pick it to go back)").dim()
    );
    Ok(())
}
