//! `aizen zone …`, `aizen team …` and `aizen work …` — the non-interactive twins of `/zone`,
//! `/team` and `/work`.
//!
//! Same registry and same plan as the slash surfaces; these print with `println!` because no
//! retained frame is up when a subcommand runs.

use crate::cli::time::diff_lines;
use crate::cli_args::*;
use crate::features::coop;
use crate::features::slash_handlers::team_status_lines;
use crate::ui::{splash, theme};
use anyhow::{bail, Result};
use console::style;

pub(crate) fn run_zone(cmd: ZoneCmd) -> Result<()> {
    match cmd {
        ZoneCmd::Migrate { apply } => {
            let plan = crate::features::zones::plan()?;
            println!("current zone: {}", plan.current_slug);
            if plan.legacy.is_empty() {
                println!("no legacy zones found for this project — nothing to merge.");
                return Ok(());
            }
            println!("legacy zones of this project:");
            for z in &plan.legacy {
                println!("  {}", z.summary());
            }
            if !apply {
                println!("\ndry-run — nothing was changed. Re-run with `aizen zone migrate --apply` to merge into {}.", plan.current_slug);
                return Ok(());
            }
            let rep = crate::features::zones::apply(&plan);
            for a in &rep.actions {
                println!("  ✓ {a}");
            }
            for w in &rep.warnings {
                eprintln!("  ⚠ {w}");
            }
            println!(
                "merged {} legacy zone(s) into {}: {} action(s), {} warning(s).",
                plan.legacy.len(),
                plan.current_slug,
                rep.actions.len(),
                rep.warnings.len()
            );
            if !rep.warnings.is_empty() {
                anyhow::bail!("zone migrate finished with warnings — each one above states exactly what moved and what didn't");
            }
            Ok(())
        }
    }
}

/// `aizen team …` — the non-interactive twin of `/team`. Same registry, same plan, `println!` instead
/// of `tui::emit_line` (no retained frame is up here, so a raw print is the correct surface).
pub(crate) fn run_team(cmd: TeamCmd) -> Result<()> {
    match cmd {
        TeamCmd::Status => {
            for line in team_status_lines() {
                println!("{line}");
            }
            Ok(())
        }
        TeamCmd::Diff { session, patch } => {
            let view = coop::resolve(&session)?;
            let reports = coop::session_diff(&view, patch)?;
            if reports.is_empty() {
                println!(
                    "{}",
                    style(format!(
                        "session {} has no file changes on disk",
                        view.manifest.session_id
                    ))
                    .dim()
                );
                return Ok(());
            }
            for report in &reports {
                for line in diff_lines(report, "--patch off") {
                    println!("{line}");
                }
            }
            Ok(())
        }
        TeamCmd::Claims => {
            let claims = coop::claims();
            if claims.is_empty() {
                println!("{}", style("no path claims recorded yet").dim());
                return Ok(());
            }
            for (path, claim) in claims {
                println!("  {path}  ← {}", style(&claim.session_id).dim());
            }
            for o in coop::overlaps() {
                println!(
                    "  {} {}  ({} → {})",
                    style("⚠").color256(theme::WARN),
                    o.path,
                    o.first,
                    o.second
                );
            }
            Ok(())
        }
        TeamCmd::Commit {
            session,
            message,
            dry_run,
            force,
            yes,
        } => {
            let view = coop::resolve(&session)?;
            let plan = coop::plan_commit(&view)?;
            for b in &plan.blockers {
                println!("{} {b}", style("⚠").color256(theme::WARN));
            }
            if !plan.blockers.is_empty() && !force {
                bail!("refusing to commit; nothing was staged (re-run with --force to override)");
            }
            if !plan.shared_paths.is_empty() {
                println!(
                    "{} {} file(s) are shared with another session — committing them carries that \
                     session's edits to the SAME file along: {}",
                    style("⚠").color256(theme::WARN),
                    plan.shared_paths.len(),
                    plan.shared_paths.join(", ")
                );
            }
            let review = coop::stage_plan(&plan)?;
            for line in review.stat.lines() {
                println!("  {line}");
            }
            if !review.separated.is_empty() {
                println!(
                    "{} {} shared file(s) were separated: the commit holds only this session's \
                     version, while the working tree keeps both sessions' edits: {}",
                    style("↔").color256(splash::ACCENT),
                    review.separated.len(),
                    review.separated.join(", ")
                );
            }
            // `--yes` is the only path to an actual commit: a bare `aizen team commit` reviews and
            // rolls the index back, so discovering the command cannot land one.
            if dry_run || !yes {
                coop::unstage_plan(&plan)?;
                println!(
                    "{}",
                    style(
                        "review only — unstaged again, nothing was committed (add --yes to commit)"
                    )
                    .dim()
                );
                return Ok(());
            }
            let msg = message.unwrap_or_else(|| {
                let task = view.manifest.task.trim();
                if task.is_empty() {
                    format!("aizen session {}", plan.session_id)
                } else {
                    task.to_string()
                }
            });
            let out = coop::commit_staged(&plan, &msg, &review)?;
            for line in out.lines().take(6) {
                println!("  {line}");
            }
            Ok(())
        }
    }
}

/// `aizen work …` — isolated worktrees, for when two windows should not share one tree at all.
pub(crate) fn run_work(cmd: WorkCmd) -> Result<()> {
    match cmd {
        WorkCmd::New { name } => {
            let wt = coop::work_new(&name)?;
            println!(
                "{} {}\n  branch {}\n  open a session there:  cd {} && aizen",
                style("✓ worktree").color256(splash::ACCENT),
                wt.path.display(),
                wt.branch,
                wt.path.display()
            );
            Ok(())
        }
        WorkCmd::List => {
            let all = coop::work_list()?;
            if all.is_empty() {
                println!(
                    "{}",
                    style("no aizen worktrees — create one with `aizen work new <name>`").dim()
                );
                return Ok(());
            }
            for wt in &all {
                let mut notes = Vec::new();
                if wt.dirty {
                    notes.push("dirty".to_string());
                }
                if wt.ahead > 0 {
                    notes.push(format!("{} unmerged commit(s)", wt.ahead));
                }
                if wt.sessions > 0 {
                    notes.push(format!("{} live session(s)", wt.sessions));
                }
                let tail = if notes.is_empty() {
                    style("clean".to_string()).dim().to_string()
                } else {
                    style(notes.join(" · ")).color256(theme::WARN).to_string()
                };
                println!("  {:<20} {:<24} {tail}", wt.name, wt.branch);
                println!("    {}", style(wt.path.display().to_string()).dim());
            }
            Ok(())
        }
        WorkCmd::Remove { name, force } => {
            let msg = coop::work_remove(&name, force)?;
            println!("{} {msg}", style("✓").color256(splash::ACCENT));
            Ok(())
        }
    }
}
