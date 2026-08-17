//! `aizen agents …` — the specialist sub-agent library (agency-agents format).
//!
//! Listing, enabling and installing agent definitions. Install accepts four source shapes
//! (`owner/repo`, a git URL, a single `.md` over http(s), a local directory) and stages every one
//! of them through a temporary directory so a partial fetch can never leave half an agent behind.

use crate::cli_args::AgentsCmd;
use crate::core::cli_config;
use crate::ui::splash;
use crate::{agents, ui_theme};
use anyhow::{Context, Result};
use console::style;
use dialoguer::Confirm;

/// A classified install source for `aizen agents install`.
#[derive(Debug, PartialEq)]
pub(crate) enum InstallSource {
    /// `owner/repo` → cloned from github.com.
    GitHubShorthand(String),
    /// A full git URL (https `.git`/repo, `git@…`, `ssh://…`).
    GitUrl(String),
    /// A single `.md` agent file fetched over http(s).
    FileUrl(String),
    /// A local directory tree.
    LocalDir(std::path::PathBuf),
}

/// Classify an install source string. Pure (no IO except an existing-dir probe) so it's unit-testable.
pub(crate) fn classify_source(raw: &str) -> Result<InstallSource> {
    let s = raw.trim();
    if s.is_empty() {
        anyhow::bail!("an install source is required (owner/repo, a git/.md URL, or a local dir)");
    }
    // http(s): a single .md file vs a git repo.
    if s.starts_with("http://") || s.starts_with("https://") {
        let path_only = s.split(['?', '#']).next().unwrap_or(s);
        if path_only.to_ascii_lowercase().ends_with(".md") {
            return Ok(InstallSource::FileUrl(s.to_string()));
        }
        return Ok(InstallSource::GitUrl(s.to_string()));
    }
    // scp-like / ssh git URLs, or any bare `*.git`.
    if s.starts_with("git@") || s.starts_with("ssh://") || s.ends_with(".git") {
        return Ok(InstallSource::GitUrl(s.to_string()));
    }
    // Explicit local-path forms, a Windows drive (`C:\…`), or an existing directory.
    let drive = {
        let b = s.as_bytes();
        b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
    };
    let looks_local = s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with(".\\")
        || s.starts_with("..\\")
        || s.starts_with('/')
        || s.starts_with('\\')
        || drive;
    if looks_local || std::path::Path::new(s).is_dir() {
        return Ok(InstallSource::LocalDir(std::path::PathBuf::from(s)));
    }
    // GitHub shorthand: exactly `owner/repo` (one slash, both halves non-empty, no whitespace).
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() == 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && !p.contains(char::is_whitespace))
    {
        return Ok(InstallSource::GitHubShorthand(s.to_string()));
    }
    anyhow::bail!(
        "unrecognized source '{raw}' — use owner/repo, an https git URL, a `.md` URL, or a local directory"
    );
}

pub(crate) async fn run_agents(cmd: Option<AgentsCmd>) -> Result<()> {
    match cmd {
        None => {
            agents_default_view();
            Ok(())
        }
        Some(AgentsCmd::List {
            division,
            source,
            enabled,
            json,
        }) => agents_list(division.as_deref(), source.as_deref(), enabled, json),
        Some(AgentsCmd::Show { name }) => match agents::load(&name) {
            Some(def) => {
                println!("{}", agents::render_card(&def));
                Ok(())
            }
            None => anyhow::bail!("no agent named '{name}' (try `aizen agents list`)"),
        },
        Some(AgentsCmd::Where) => {
            agents_where();
            Ok(())
        }
        Some(AgentsCmd::Install {
            source,
            yes,
            enable_all,
            as_name,
        }) => agents_install(&source, yes, enable_all, as_name.as_deref()).await,
        Some(AgentsCmd::Remove { name }) => {
            if agents::delete_home(&name)? {
                let _ = agents::set_enabled(&name, false);
                println!("removed '{name}' from ~/.aizen/agents and unpinned it");
            } else {
                println!("(no agent named '{name}' under ~/.aizen/agents — `aizen agents where` shows the dirs)");
            }
            Ok(())
        }
        Some(AgentsCmd::Enable { name, all }) => agents_set_enabled(name.as_deref(), all, true),
        Some(AgentsCmd::Disable { name, all }) => agents_set_enabled(name.as_deref(), all, false),
        Some(AgentsCmd::SetProvider {
            name,
            provider,
            model,
            clear,
        }) => {
            let mut cfg = cli_config::load();
            if agents::load(&name).is_none() {
                anyhow::bail!("no agent named '{name}' (try `aizen agents list`)");
            }
            if clear {
                cfg.set_agent_route(&name, None, None)?;
                cli_config::save(&cfg)?;
                println!(
                    "{} cleared provider assignment on '{}'",
                    crate::ui::theme::ok("✓"),
                    name
                );
            } else {
                let provider = provider
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .context("pass a saved provider name (or --clear)")?;
                cfg.set_agent_route(&name, Some(provider.to_string()), model)?;
                cli_config::save(&cfg)?;
                let route = cfg.agent_route(&name).expect("route just saved");
                println!(
                    "{} assigned '{}' to provider {} · model {}",
                    crate::ui::theme::ok("✓"),
                    name,
                    route.provider.as_deref().unwrap_or("inherit"),
                    route.model.as_deref().unwrap_or("provider default")
                );
            }
            Ok(())
        }
        Some(AgentsCmd::SetModel { name, model, clear }) => {
            let new_model = if clear {
                None
            } else {
                match model.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    Some(m) => Some(m.to_string()),
                    None => anyhow::bail!("pass a model id (or --clear to remove the pin)"),
                }
            };
            let path = agents::set_model(&name, new_model.as_deref())?;
            match &new_model {
                Some(m) => println!(
                    "{} pinned '{name}' to model {} ({})",
                    crate::ui::theme::ok("✓"),
                    style(m).color256(splash::ACCENT),
                    path.display()
                ),
                None => println!(
                    "{} cleared the model pin on '{name}' ({})",
                    crate::ui::theme::ok("✓"),
                    path.display()
                ),
            }
            Ok(())
        }
    }
}

/// Bare `aizen agents`: list when any exist, else the install nudge.
fn agents_default_view() {
    if agents::has_any() {
        let _ = agents_list(None, None, false, false);
    } else {
        agents_nudge();
    }
}

fn agents_nudge() {
    println!("No specialist agents yet. Install the agency-agents library with:");
    println!(
        "  {}",
        style("aizen agents install msitarzewski/agency-agents").color256(splash::ACCENT)
    );
    println!("…or drop `.md` personas into ~/.aizen/agents (or ~/.claude/agents).");
}

fn agents_list(
    division: Option<&str>,
    source: Option<&str>,
    enabled_only: bool,
    json: bool,
) -> Result<()> {
    let enabled = agents::enabled_set();
    let is_enabled = |slug: &str| enabled.as_ref().map(|e| e.contains(slug)).unwrap_or(false);

    let mut all = agents::list();
    if let Some(d) = division {
        let d = d.to_lowercase();
        all.retain(|a| a.division.as_deref() == Some(d.as_str()));
    }
    if let Some(src) = source {
        let src = src.to_lowercase();
        all.retain(|a| a.source.label() == src);
    }
    if enabled_only {
        all.retain(|a| is_enabled(&a.slug()));
    }

    let cfg = cli_config::load();
    if json {
        let arr: Vec<serde_json::Value> = all
            .iter()
            .map(|a| {
                serde_json::json!({
                    "slug": a.slug(),
                    "name": a.name,
                    "description": a.description,
                    "division": a.division,
                    "source": a.source.label(),
                    "model": a.model,
                    "provider": cfg.agent_route(&a.slug()).and_then(|r| r.provider.clone()),
                    "route_model": cfg.agent_route(&a.slug()).and_then(|r| r.model.clone()),
                    "tools": a.tools,
                    "enabled": is_enabled(&a.slug()),
                    "path": a.source_path.display().to_string(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }

    if all.is_empty() {
        if agents::has_any() {
            println!("(no agents match the filter)");
        } else {
            agents_nudge();
        }
        return Ok(());
    }

    let mut by_div: std::collections::BTreeMap<String, Vec<&agents::AgentDef>> =
        std::collections::BTreeMap::new();
    for a in &all {
        by_div
            .entry(
                a.division
                    .clone()
                    .unwrap_or_else(|| "(no division)".to_string()),
            )
            .or_default()
            .push(a);
    }
    let total = all.len();
    let enabled_count = all.iter().filter(|a| is_enabled(&a.slug())).count();
    for (div, items) in &by_div {
        println!("{}", style(format!("{div} ({})", items.len())).bold());
        for a in items {
            let mark = if is_enabled(&a.slug()) {
                style("●").color256(splash::ACCENT).to_string()
            } else {
                style("○").dim().to_string()
            };
            let desc: String = a.description.chars().take(80).collect();
            let route = cfg.agent_route(&a.slug());
            let route_hint = route
                .map(|r| {
                    format!(
                        " · provider {} · {}",
                        r.provider.as_deref().unwrap_or("inherit"),
                        r.model.as_deref().unwrap_or("default model")
                    )
                })
                .unwrap_or_default();
            println!(
                "  {} {}  —  {}{}",
                mark,
                a.slug(),
                desc.replace('\n', " "),
                route_hint
            );
        }
    }
    let hint = if enabled.is_some() {
        format!("{total} agent(s) · {enabled_count} pinned to <agents>. Dispatch: task(agent=\"<slug>\").")
    } else {
        format!("{total} agent(s) · none pinned — `aizen agents enable <slug>` to advertise them.")
    };
    println!("{}", style(hint).dim());
    Ok(())
}

fn agents_where() {
    println!("Specialist agent sources (lower → higher precedence):");
    for (src, dir, n) in agents::source_counts() {
        let status = if !dir.exists() {
            style("(absent)").dim().to_string()
        } else {
            format!("{n} agent(s)")
        };
        println!("  {:<16} {}  [{}]", src.label(), dir.display(), status);
    }
    println!(
        "{}",
        style(
            "Installs write to ~/.aizen/agents; a higher-precedence dir wins on a slug collision."
        )
        .dim()
    );
}

fn agents_set_enabled(name: Option<&str>, all: bool, on: bool) -> Result<()> {
    if all {
        agents::set_all_enabled(on)?;
        println!(
            "{} all agents {} the <agents> index",
            if on { "pinned" } else { "unpinned" },
            if on { "to" } else { "from" }
        );
        return Ok(());
    }
    let name = name.context("provide an agent name, or pass --all")?;
    let def = agents::load(name)
        .with_context(|| format!("no agent named '{name}' (try `aizen agents list`)"))?;
    agents::set_enabled(&def.slug(), on)?;
    println!(
        "{} '{}' {} the <agents> index",
        if on { "pinned" } else { "unpinned" },
        def.slug(),
        if on { "to" } else { "from" }
    );
    Ok(())
}

fn confirm_write(prompt: &str) -> Result<bool> {
    Ok(Confirm::with_theme(&ui_theme())
        .with_prompt(prompt)
        .default(true)
        .interact_opt()
        .ok()
        .flatten()
        .unwrap_or(false))
}

/// A filesystem-safe directory name from a repo slug/URL (last segment, `.git` stripped).
pub(crate) fn sanitize_repo_name(s: &str) -> String {
    let base = s
        .trim_end_matches('/')
        .rsplit(['/', ':', '\\'])
        .next()
        .unwrap_or(s);
    let base = base.trim_end_matches(".git");
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches(['-', '.']).to_string();
    if cleaned.is_empty() {
        "agents".to_string()
    } else {
        cleaned
    }
}

fn unique_n() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

/// `git clone --depth 1` (shallow, quiet, NO submodule recursion) into `dest`. Cloning runs NO repo
/// code — it only reads files. `--no-recurse-submodules` stops a hostile `.gitmodules` from making git
/// fetch arbitrary (possibly internal) submodule URLs we never vetted.
fn git_clone_shallow(url: &str, dest: &std::path::Path) -> Result<()> {
    let out = crate::core::gitx::command()?
        .args([
            "clone",
            "--depth",
            "1",
            "--no-recurse-submodules",
            "--quiet",
            url,
        ])
        .arg(dest)
        .output()
        .context("running `git clone` (is git installed and on PATH?)")?;
    if !out.status.success() {
        anyhow::bail!(
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Extract the host from a NON-http(s) git URL (`git@host:path`, `ssh://[user@]host[:port]/path`,
/// `git://host/path`) so the SSRF floor can guard it too. `None` if no host is discernible.
pub(crate) fn git_url_host(url: &str) -> Option<String> {
    let non_empty = |s: &str| {
        let s = s.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    };
    // scp-like: [user@]host:path (no scheme).
    if url.starts_with("git@") || (url.contains('@') && url.contains(':') && !url.contains("://")) {
        let after_at = url.rsplit('@').next().unwrap_or(url);
        return non_empty(after_at.split(':').next().unwrap_or(after_at));
    }
    let rest = url
        .strip_prefix("ssh://")
        .or_else(|| url.strip_prefix("git://"))?;
    let after_at = rest.rsplit('@').next().unwrap_or(rest);
    non_empty(after_at.split(['/', ':']).next().unwrap_or(after_at))
}

/// Copy every `*.md` that `looks_like_agent` from `src` (recursively, dotdirs skipped) into
/// `dest_root`, preserving the relative subpath. Returns `(copied, skipped)`.
fn copy_agent_tree(src: &std::path::Path, dest_root: &std::path::Path) -> Result<(usize, usize)> {
    let mut copied = 0;
    let mut skipped = 0;
    copy_agent_walk(src, src, dest_root, &mut copied, &mut skipped, 0)?;
    Ok((copied, skipped))
}

fn copy_agent_walk(
    dir: &std::path::Path,
    src_root: &std::path::Path,
    dest_root: &std::path::Path,
    copied: &mut usize,
    skipped: &mut usize,
    depth: usize,
) -> Result<()> {
    if depth > 12 {
        return Ok(());
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue; // skip .git, dotfiles
        }
        // Never follow symlinks out of an UNTRUSTED cloned tree (a symlinked `x.md` could pull in a
        // file outside the repo; a symlinked dir could escape it). The loader treats the user's own
        // dirs as trusted, but install copies third-party content, so refuse symlinks here.
        if e.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
            continue;
        }
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            copy_agent_walk(&p, src_root, dest_root, copied, skipped, depth + 1)?;
        } else if p
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x.eq_ignore_ascii_case("md"))
        {
            let Ok(content) = std::fs::read_to_string(&p) else {
                continue;
            };
            if !agents::looks_like_agent(&content) {
                *skipped += 1;
                continue;
            }
            let rel = p.strip_prefix(src_root).unwrap_or(&p);
            let dest = dest_root.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(&dest, content)
                .with_context(|| format!("writing {}", dest.display()))?;
            *copied += 1;
        }
    }
    Ok(())
}

async fn agents_install(
    source: &str,
    yes: bool,
    enable_all: bool,
    as_name: Option<&str>,
) -> Result<()> {
    let classified = classify_source(source)?;
    println!(
        "{}",
        style("⚠ agent bodies are third-party system prompts — they run as sub-agents with edit/shell scope. Review before pinning.").dim()
    );
    match classified {
        InstallSource::FileUrl(url) => {
            crate::core::net_guard::guard_url_async(&url).await?;
            if !yes && !confirm_write(&format!("Fetch and install the agent at {url}?"))? {
                println!("cancelled.");
                return Ok(());
            }
            // Fetch through the shared guarded client (auto-redirects OFF, every hop re-vetted
            // against the net_guard floor) — a plain reqwest client follows up to 10 redirects and
            // would re-vet only the first hop, so a 302 → 169.254.169.254 / localhost slips through.
            let http = crate::agent::reach::http::client()?;
            let resp = crate::agent::reach::http::get(&http, &url, &[])
                .await
                .with_context(|| format!("GET {url}"))?;
            if !resp.is_success() {
                anyhow::bail!("upstream returned HTTP {}", resp.status);
            }
            let text = resp.text();
            if !agents::looks_like_agent(&text) {
                anyhow::bail!(
                    "that URL isn't an agent (needs frontmatter `name:` + a non-empty body)"
                );
            }
            let stem = url
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("agent");
            let stem = stem
                .split(['?', '#'])
                .next()
                .unwrap_or(stem)
                .trim_end_matches(".md");
            let stem = if stem.is_empty() { "agent" } else { stem }; // e.g. a URL ending in "/.md"
            let path = agents::save_home(&text, as_name.unwrap_or(stem))?;
            println!("installed 1 agent → {}", path.display());
            if enable_all {
                agents::set_all_enabled(true)?;
                println!("…and pinned all agents to <agents>.");
            }
            Ok(())
        }
        InstallSource::LocalDir(dir) => {
            if !dir.is_dir() {
                anyhow::bail!("not a directory: {}", dir.display());
            }
            if !yes && !confirm_write(&format!("Install agents from {}?", dir.display()))? {
                println!("cancelled.");
                return Ok(());
            }
            let label = dir.file_name().and_then(|s| s.to_str()).unwrap_or("local");
            let dest = agents::agents_dir().join(sanitize_repo_name(label));
            std::fs::create_dir_all(&dest)
                .with_context(|| format!("creating {}", dest.display()))?;
            let (copied, skipped) = copy_agent_tree(&dir, &dest)?;
            crate::core::config::harden_dir(&agents::agents_dir());
            finish_install(copied, skipped, &dest, enable_all)
        }
        InstallSource::GitHubShorthand(slug) => {
            install_from_git(
                &format!("https://github.com/{slug}.git"),
                &slug,
                yes,
                enable_all,
            )
            .await
        }
        InstallSource::GitUrl(url) => {
            let label = sanitize_repo_name(&url);
            install_from_git(&url, &label, yes, enable_all).await
        }
    }
}

async fn install_from_git(url: &str, label: &str, yes: bool, enable_all: bool) -> Result<()> {
    // SSRF floor — guard the destination host whatever the git transport is.
    if url.starts_with("https://") || url.starts_with("http://") {
        crate::core::net_guard::guard_url_async(url).await?;
    } else if let Some(host) = git_url_host(url) {
        // ssh:// / git@ / git:// never go through the http(s) guard; guard the resolved host directly
        // so an internal endpoint (e.g. git@10.0.0.5:…) can't be reached past the floor.
        crate::core::net_guard::guard_url_async(&format!("https://{host}")).await?;
    }
    if !yes
        && !confirm_write(&format!(
            "Clone {url} and install its agents into ~/.aizen/agents?"
        ))?
    {
        println!("cancelled.");
        return Ok(());
    }
    let repo = sanitize_repo_name(label);
    let dest = agents::agents_dir().join(&repo);
    let staging = std::env::temp_dir().join(format!(
        "aizen-agents-clone-{}-{}",
        std::process::id(),
        unique_n()
    ));
    let _ = std::fs::remove_dir_all(&staging);
    println!("{}", style(format!("cloning {url} …")).dim());

    let url_s = url.to_string();
    let staging_c = staging.clone();
    let clone_res = tokio::task::spawn_blocking(move || git_clone_shallow(&url_s, &staging_c))
        .await
        .context("clone task panicked")?;

    // Always clean the staging clone, whether or not the copy succeeds.
    let outcome = (|| -> Result<(usize, usize)> {
        clone_res?;
        std::fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;
        let counts = copy_agent_tree(&staging, &dest)?;
        crate::core::config::harden_dir(&agents::agents_dir());
        Ok(counts)
    })();
    let _ = std::fs::remove_dir_all(&staging);

    let (copied, skipped) = outcome?;
    finish_install(copied, skipped, &dest, enable_all)
}

fn finish_install(
    copied: usize,
    skipped: usize,
    dest: &std::path::Path,
    enable_all: bool,
) -> Result<()> {
    if copied == 0 {
        // Nothing landed. Use `remove_dir` (empty-only), NOT `remove_dir_all`: it removes the dir we
        // just created but can never wipe pre-existing user files in a same-named directory.
        let _ = std::fs::remove_dir(dest);
        anyhow::bail!(
            "no agents found ({skipped} non-agent file(s) skipped) — the source had no `*.md` with frontmatter `name:` + a body"
        );
    }
    println!(
        "installed {copied} agent(s) → {} ({skipped} non-agent file(s) skipped)",
        dest.display()
    );
    if enable_all {
        agents::set_all_enabled(true)?;
        println!("…and pinned all agents to <agents>.");
    } else {
        println!(
            "{}",
            style("none are pinned yet — `aizen agents enable <slug>` (or re-run with --enable-all) to advertise them.").dim()
        );
    }
    println!(
        "{}",
        style("review: `aizen agents list` · `aizen agents show <slug>`").dim()
    );
    Ok(())
}
