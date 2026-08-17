//! `aizen persona …` and `aizen soul …` — the operator's card and the durable operating identity.
//!
//! A persona is `<name>.md` (the card) beside `<name>.self/` (its self-memory); SOUL is the single
//! always-on identity document. Both are plain files on purpose, so `edit` just prints the path.

use crate::cli::read_stdin;
use crate::cli_args::{PersonaCmd, SoulCmd};
use crate::core::cli_config;
use crate::persona::soul;
use crate::ui::menus::persona_self_view_n;
use crate::{persona, skill};
use anyhow::Result;
use console::style;

pub(crate) fn run_persona(cmd: PersonaCmd) -> Result<()> {
    match cmd {
        PersonaCmd::List => {
            let active_slug = cli_config::load()
                .persona
                .as_deref()
                .map(skill::sanitize_name);
            let ps = persona::list();
            if ps.is_empty() {
                println!("(no personas yet — `aizen persona new <name>`, or /persona in the REPL)");
                return Ok(());
            }
            for p in &ps {
                let slug = skill::sanitize_name(&p.name);
                let mark = if active_slug.as_deref() == Some(slug.as_str()) {
                    "●"
                } else {
                    "○"
                };
                let sub = if p.role.is_empty() {
                    p.voice.clone()
                } else {
                    p.role.clone()
                };
                let (eps, ins) = persona::self_mem::counts(&slug);
                println!(
                    "{mark} {} — {sub}  ({ins} insights, {eps} episodes)",
                    p.name
                );
            }
            // Retired cards are recoverable, so say they exist — a soft delete nobody can see is
            // indistinguishable from a hard one.
            let retired = persona::list_archive();
            if !retired.is_empty() {
                let names: Vec<&str> = retired.iter().map(|p| p.name.as_str()).collect();
                println!(
                    "\nretired: {} — `aizen persona restore <name>` brings one back",
                    names.join(", ")
                );
            }
            // Personas have no `where` sub-command of their own (one folder, no zoning), so the path
            // goes here directly — the card and its `.self/` memory are plain files worth editing.
            println!(
                "{}",
                style(format!(
                    "\nfiles: {}   (`<name>.md` is the card, `<name>.self/` its memory)",
                    persona::personas_dir().display()
                ))
                .dim()
            );
            Ok(())
        }
        PersonaCmd::Show { name } => {
            let p = persona::load(&name).ok_or_else(|| anyhow::anyhow!("no persona '{name}'"))?;
            println!("# {}", p.name);
            if !p.role.is_empty() {
                println!("role: {}", p.role);
            }
            if !p.voice.is_empty() {
                println!("voice: {}", p.voice);
            }
            if !p.body.is_empty() {
                println!("\n{}", p.body);
            }
            Ok(())
        }
        PersonaCmd::New {
            name,
            role,
            voice,
            body,
        } => {
            let body = match body {
                Some(b) => b,
                None => read_stdin("reading persona body from stdin")?,
            };
            let path = persona::save(
                &name,
                role.as_deref().unwrap_or(""),
                voice.as_deref().unwrap_or(""),
                &body,
            )?;
            println!("saved persona → {}", path.display());
            Ok(())
        }
        PersonaCmd::Use { name } => {
            let p = persona::load(&name)
                .ok_or_else(|| anyhow::anyhow!("no persona '{name}' (see `aizen persona list`)"))?;
            let mut cfg = cli_config::load();
            cfg.persona = Some(p.name.clone());
            cli_config::save(&cfg)?;
            println!("now playing: {}", p.name);
            Ok(())
        }
        PersonaCmd::Clear => {
            let mut cfg = cli_config::load();
            cfg.persona = None;
            cli_config::save(&cfg)?;
            println!("persona cleared → default assistant voice");
            Ok(())
        }
        PersonaCmd::SelfMem { name, all } => {
            let slug = match name {
                Some(n) => skill::sanitize_name(&n),
                None => persona::active_slug().ok_or_else(|| {
                    anyhow::anyhow!("no active persona — pass a name or `aizen persona use <name>`")
                })?,
            };
            let label = persona::load(&slug)
                .map(|p| p.name)
                .unwrap_or_else(|| slug.clone());
            persona_self_view_n(&slug, &label, all);
            Ok(())
        }
        PersonaCmd::Forget { id, name } => {
            let slug = match name {
                Some(n) => skill::sanitize_name(&n),
                None => persona::active_slug().ok_or_else(|| {
                    anyhow::anyhow!("no active persona — pass --name or `aizen persona use <name>`")
                })?,
            };
            persona::self_mem::forget(&slug, &id)?;
            println!(
                "retired self-memory '{id}' (archived — `aizen persona unforget {id}` restores it)"
            );
            Ok(())
        }
        PersonaCmd::Unforget { id, name } => {
            let slug = match name {
                Some(n) => skill::sanitize_name(&n),
                None => persona::active_slug().ok_or_else(|| {
                    anyhow::anyhow!("no active persona — pass --name or `aizen persona use <name>`")
                })?,
            };
            persona::self_mem::restore(&slug, &id)?;
            println!("restored self-memory '{id}'");
            Ok(())
        }
        PersonaCmd::Remember { text, importance } => {
            let slug = persona::active_slug().ok_or_else(|| {
                anyhow::anyhow!("no active persona — `aizen persona use <name>` first")
            })?;
            // Explicit CLI remember is always formative: force Explicit kind + floor ≥ FORMATIVE_MIN.
            let imp = importance
                .unwrap_or_else(|| {
                    persona::self_mem::classify_turn(&text, 0)
                        .map(|s| s.importance)
                        .unwrap_or(6)
                })
                .max(persona::self_mem::FORMATIVE_MIN)
                .min(10);
            let body = if text.trim().starts_with("correction:")
                || text.trim().starts_with("preference:")
                || text.trim().starts_with("work:")
                || text.trim().starts_with("bond:")
                || text.trim().starts_with("explicit:")
            {
                text.clone()
            } else {
                persona::self_mem::format_episode_body(
                    persona::self_mem::EventKind::Explicit,
                    &text,
                    0,
                    "",
                )
            };
            match persona::self_mem::record_episode(&slug, &body, imp)? {
                Some(id) => println!("recorded episode '{id}' (importance {imp})"),
                None => println!("(skipped — near-duplicate of a recent episode/insight)"),
            }
            Ok(())
        }
        PersonaCmd::Delete { name } => {
            if persona::delete(&name)? {
                // Clear the active pointer too, or the config keeps naming a card that is gone.
                let mut cfg = cli_config::load();
                if cfg
                    .persona
                    .as_deref()
                    .map(|p| skill::sanitize_name(p) == skill::sanitize_name(&name))
                    .unwrap_or(false)
                {
                    cfg.persona = None;
                    cli_config::save(&cfg)?;
                    println!("retired persona '{name}' (was active — back to the default voice)");
                } else {
                    println!("retired persona '{name}'");
                }
                println!(
                    "card + self-memory archived under {} — `aizen persona restore {name}`",
                    persona::archive_dir().display()
                );
            } else {
                println!("no persona named '{name}'");
            }
            Ok(())
        }
        PersonaCmd::Restore { name } => {
            let p = persona::restore(&name)?;
            println!("restored persona '{name}' → {}", p.display());
            Ok(())
        }
        PersonaCmd::Block => {
            match persona::prompt_block() {
                Some(p) => println!("<persona>\n{}\n</persona>", p.trim()),
                None => {
                    println!("(no persona active — `aizen persona use <name>`)");
                    return Ok(());
                }
            }
            match persona::self_block() {
                Some(s) => println!("\n<self>\n{}\n</self>", s.trim()),
                None => println!("\n(no <self> yet — the character has no self-memory; `aizen persona remember \"...\"`)"),
            }
            Ok(())
        }
    }
}

pub(crate) fn run_soul(cmd: Option<SoulCmd>) -> Result<()> {
    match cmd.unwrap_or(SoulCmd::Show) {
        SoulCmd::Show => {
            match soul::prompt_block() {
                Some(b) => println!("<agent_identity>\n{}\n</agent_identity>", b.trim()),
                None if soul::exists() => println!(
                    "(SOUL.md exists but renders nothing — it is empty or was dropped by the safety \
                     scan; see {})",
                    soul::soul_path().display()
                ),
                None => println!(
                    "(no operating identity yet — set one with `aizen soul set` or edit {})",
                    soul::soul_path().display()
                ),
            }
            Ok(())
        }
        SoulCmd::Set { body } => {
            let body = match body {
                Some(b) => b,
                None => read_stdin("reading SOUL body from stdin")?,
            };
            let path = soul::write(&body)?;
            println!("saved operating identity → {}", path.display());
            if soul::prompt_block().is_none() {
                println!(
                    "{}",
                    style("⚠ heads up: it renders nothing — the safety scan dropped it (a credential or \
                     injection-looking line). It will NOT be injected until fixed.")
                        .yellow()
                );
            }
            Ok(())
        }
        SoulCmd::Clear => {
            if soul::clear()? {
                println!("operating identity cleared");
            } else {
                println!("(no operating identity to clear)");
            }
            Ok(())
        }
        SoulCmd::Path => {
            println!("{}", soul::soul_path().display());
            Ok(())
        }
    }
}
