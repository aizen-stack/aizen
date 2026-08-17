//! `aizen skill …` — the skill store: list, show, save, refine, fetch, remove.
//!
//! A skill is a markdown file with `name`/`description`/`when` frontmatter plus a body. Everything
//! here is thin plumbing over `crate::skills`; the zone-aware lookup and the registry live there.

use crate::cli::read_stdin;
use crate::cli_args::SkillCmd;
use crate::ui::splash;
use crate::{elide, skill, skill_registry, skill_where_report};
use anyhow::{Context, Result};
use console::style;

pub(crate) async fn run_skill(cmd: SkillCmd) -> Result<()> {
    match cmd {
        SkillCmd::List { all_zones } => {
            let skills = skill::list();
            if skills.is_empty() && !all_zones {
                println!(
                    "(no skills — add one with `aizen skill add <name>`, or /skills in the REPL)"
                );
                return Ok(());
            }
            // Aligned columns, and the usage count as the triage signal: a skill the agent has never
            // loaded is either mis-triggered (`when:` doesn't match how the task gets phrased) or not
            // needed. Unaligned `name — description` gave no way to compare that across rows.
            let namew = skills
                .iter()
                .map(|s| s.name.chars().count())
                .max()
                .unwrap_or(0)
                .min(38);
            for s in &skills {
                let d = if s.description.is_empty() {
                    &s.when
                } else {
                    &s.description
                };
                let tag = match s.origin {
                    skill::SkillOrigin::Global => "",
                    skill::SkillOrigin::Project => " [project]",
                    skill::SkillOrigin::Repo => " [repo]",
                };
                let uses = if s.uses > 0 {
                    format!("{}×", s.uses)
                } else {
                    "cold".to_string()
                };
                let pad = namew.saturating_sub(s.name.chars().count());
                println!(
                    "  {}{}  {:<5} {}{tag}",
                    s.name,
                    " ".repeat(pad),
                    uses,
                    elide(d, 92)
                );
            }
            let cold = skills.iter().filter(|s| s.uses == 0).count();
            println!("\n{} skill(s)", skills.len());
            if cold > 0 {
                println!(
                    "{}",
                    style(format!(
                        "{cold} cold (never loaded — check `when:` matches how you'd phrase the task)"
                    ))
                    .dim()
                );
            }
            println!(
                "{}",
                style(
                    "`skill show <name>` reads one · `skill refine <name>` rewrites the steps · \
                     `skill delete <name>` retires (restorable)\n\
                     `skill where` prints the three folders skills are read from"
                )
                .dim()
            );
            if all_zones {
                let others = skill::list_other_zones();
                if !others.is_empty() {
                    println!("\nother workspaces' zones (invisible here):");
                    for (zone, s) in &others {
                        println!("  {}  [p:{zone}]  {}", s.name, elide(&s.description, 80));
                    }
                }
            }
            let retired = skill::list_archive();
            if !retired.is_empty() {
                let names: Vec<&str> = retired.iter().map(|s| s.name.as_str()).collect();
                println!(
                    "\nretired: {} — `aizen skill restore <name>` brings one back",
                    names.join(", ")
                );
            }
            Ok(())
        }
        SkillCmd::Show { name } => match skill::load(&name) {
            Some(sk) => {
                println!("{}", skill::render_loaded(&sk));
                Ok(())
            }
            None => anyhow::bail!("no skill named '{name}' (try `aizen skill list`)"),
        },
        SkillCmd::Where => {
            println!("{}", skill_where_report());
            Ok(())
        }
        SkillCmd::Add {
            name,
            description,
            when,
            body,
        } => {
            let body = match body {
                Some(b) => b,
                None => read_stdin("reading skill body from stdin")?,
            };
            let path = skill::save(
                &name,
                description.as_deref().unwrap_or(""),
                when.as_deref().unwrap_or(""),
                &body,
            )?;
            println!("saved skill → {}", path.display());
            Ok(())
        }
        SkillCmd::Delete { name } => {
            if skill::delete(&name)? {
                println!(
                    "retired '{name}' (archived — `aizen skill restore {name}` brings it back)"
                );
            } else {
                println!("(no skill named '{name}')");
            }
            Ok(())
        }
        SkillCmd::Restore { name } => {
            let p = skill::restore(&name)?;
            println!("restored '{name}' → {}", p.display());
            Ok(())
        }
        SkillCmd::Refine {
            name,
            description,
            when,
            body,
        } => {
            let body = match body {
                Some(b) => b,
                None => read_stdin("reading the refined skill body from stdin")?,
            };
            let (version, archived) =
                skill::refine(&name, &body, description.as_deref(), when.as_deref())?;
            println!(
                "{} '{name}' → v{version} (prior version archived at {})",
                style("refined").color256(splash::ACCENT),
                archived.display()
            );
            Ok(())
        }
        SkillCmd::Fetch { url, name } => run_skill_fetch(&url, name.as_deref()).await,
        SkillCmd::Search { query, limit } => {
            let q = query.join(" ");
            if q.trim().is_empty() {
                anyhow::bail!(
                    "a search query is required, e.g. `aizen skill search deploy fastapi`"
                );
            }
            let hits = skill_registry::search(&q, limit.unwrap_or(20).clamp(1, 50)).await?;
            if hits.is_empty() {
                println!(
                    "no skills on {} match '{q}'",
                    skill_registry::registry_base()
                );
                return Ok(());
            }
            println!(
                "{}",
                style(format!(
                    "{} result(s) from {} — install with `aizen skill install <owner/name>`",
                    hits.len(),
                    skill_registry::registry_base()
                ))
                .dim()
            );
            for sk in &hits {
                println!("{}", sk.summary_line());
            }
            Ok(())
        }
        SkillCmd::Install { slug } => {
            let sk = skill_registry::install(&slug).await?;
            println!(
                "{} '{}' → {}",
                style("installed").color256(splash::ACCENT),
                sk.name,
                skill::skills_dir()
                    .join(format!("{}.md", skill::sanitize_name(&sk.name)))
                    .display()
            );
            Ok(())
        }
    }
}

/// GET a markdown skill from `url` and save it (name from `--name` > frontmatter > URL filename).
/// SSRF-guarded like every other outbound fetch: the URL passes the net_guard floor and the fetch
/// goes through the shared guarded client (no auto-redirects; every hop re-vetted; body bounded).
pub(crate) async fn run_skill_fetch(url: &str, name_override: Option<&str>) -> Result<()> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        anyhow::bail!("fetch needs an absolute http(s) URL");
    }
    crate::core::net_guard::guard_url_async(url).await?;
    let http = crate::agent::reach::http::client()?;
    let resp = crate::agent::reach::http::get(&http, url, &[])
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.is_success() {
        anyhow::bail!("upstream returned HTTP {}", resp.status);
    }
    let text = resp.text();
    // Fallback name from the URL's filename (strip a trailing .md).
    let stem = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("skill");
    let stem = stem
        .split(['?', '#'])
        .next()
        .unwrap_or(stem)
        .trim_end_matches(".md");
    let sk = skill::parse_markdown(&text, stem);
    let name = name_override.unwrap_or(&sk.name);
    let path = skill::save(name, &sk.description, &sk.when, &sk.body)?;
    println!("fetched skill '{name}' → {}", path.display());
    Ok(())
}
