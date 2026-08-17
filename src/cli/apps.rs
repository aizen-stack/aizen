//! `aizen apps …` — connecting apps through the MCP registry.
//!
//! Adding an app writes a server entry (command + args + env) into the MCP config; the runner
//! (`npx`/`uvx`/`docker`) is PATH-checked first so a missing runtime is reported here rather than as
//! a mystery failure on the next launch. Secrets are masked on every display path.

use crate::agent::app_catalog;
use crate::cli_args::AppsCmd;

use crate::ui::{icons, splash};
use crate::ui_theme;
use anyhow::{anyhow, Context, Result};
use console::style;
use dialoguer::{Confirm, Input, Password, Select};

/// `aizen apps …` — connect apps via the MCP registry.
pub(crate) async fn run_apps(cmd: Option<AppsCmd>) -> Result<()> {
    match cmd {
        None | Some(AppsCmd::List) => {
            apps_print_list();
            Ok(())
        }
        Some(AppsCmd::Search { query, limit }) => {
            let q = query.join(" ");
            if q.trim().is_empty() {
                return Err(anyhow!("usage: aizen apps search <keywords>"));
            }
            let hits = app_catalog::dedupe_latest(
                app_catalog::search(q.trim(), limit.unwrap_or(20).clamp(1, 50)).await?,
            );
            if hits.is_empty() {
                println!("no apps on {} match '{q}'", app_catalog::registry_base());
                return Ok(());
            }
            println!(
                "{}",
                style(format!(
                    "{} result(s) from {} — `aizen apps add <name>` to connect:",
                    hits.len(),
                    app_catalog::registry_base()
                ))
                .dim()
            );
            for s in &hits {
                let name = style(&s.name).color256(splash::ACCENT);
                println!("  {name}\n    {}", s.summary_line());
            }
            Ok(())
        }
        Some(AppsCmd::Add { name }) => apps_add(&name).await,
        Some(AppsCmd::Info { name }) => apps_info(&name).await,
        Some(AppsCmd::Login { name }) => {
            crate::agent::mcp::login(&name).await?;
            println!(
                "{}",
                style(format!("✓ signed in to '{name}'. Its tools load on your next message (/mcp to verify)."))
                    .color256(splash::ACCENT)
            );
            Ok(())
        }
        Some(AppsCmd::Remove { name }) => {
            if app_catalog::remove_server(&name)? {
                crate::agent::mcp_oauth::clear_token(&name); // drop any cached OAuth token too
                crate::agent::mcp::invalidate();
                println!(
                    "{}",
                    style(format!("✓ disconnected '{name}'.")).color256(splash::ACCENT)
                );
            } else {
                println!("no connected app keyed '{name}' (see `aizen apps list`).");
            }
            Ok(())
        }
    }
}

/// Render the featured catalog with connection badges.
fn apps_print_list() {
    let installed = app_catalog::installed_keys();
    println!(
        "{}",
        style("Apps — connect via the MCP registry (`aizen apps add <key>`):").bold()
    );
    for f in app_catalog::FEATURED {
        let on = installed.iter().any(|k| k == f.key);
        let badge = if on {
            style("✓").color256(splash::ACCENT).to_string()
        } else {
            style("○").dim().to_string()
        };
        println!(
            "  {badge}  {} {:<18} {}",
            icons::g(f.icon),
            style(f.key).color256(splash::ACCENT),
            style(f.blurb).dim()
        );
    }
    // Apps the user connected that aren't in the featured set (added via `aizen apps add <name>`).
    let custom: Vec<&String> = installed
        .iter()
        .filter(|k| !app_catalog::FEATURED.iter().any(|f| f.key == **k))
        .collect();
    if !custom.is_empty() {
        println!("\n{}", style("connected (custom):").bold());
        for k in &custom {
            println!(
                "  {}  {} {}",
                style("✓").color256(splash::ACCENT),
                icons::g("🧩"),
                style(k).color256(splash::ACCENT)
            );
        }
    }
    println!(
        "\n{}",
        style("details: `aizen apps info <key>`   ·   search: `aizen apps search <keywords>`   ·   remove: `aizen apps remove <key>`").dim()
    );
}

/// Resolve a featured key or registry name → fetch spec → pick transport → prompt secrets → write
/// the mcp.json entry. Interactive (hidden secret prompts); transparent about what it chose.
pub(crate) async fn apps_add(name: &str) -> Result<()> {
    let theme = ui_theme();
    // Resolve to the VIABLE candidate set and let the user CHOOSE (publisher + local/hosted shown) —
    // connecting an app hands it your token, so we never silently wire whatever sorts first. The
    // best heuristic match (pick_best) is the pre-selected default. (A featured app's vendor is just
    // a search hint + default; the official server is often OAuth-only, so community servers appear.)
    let (key0, query, prefer, label) = match app_catalog::featured(name) {
        Some(f) => (
            Some(f.key.to_string()),
            f.query.to_string(),
            f.prefer.to_string(),
            f.label.to_string(),
        ),
        None => (None, name.to_string(), name.to_string(), name.to_string()),
    };
    let hits = app_catalog::dedupe_latest(app_catalog::search(&query, 50).await?);
    let viable: Vec<app_catalog::RegistryServer> = hits
        .into_iter()
        .filter(|s| app_catalog::is_viable(s))
        .collect();
    if viable.is_empty() {
        return Err(anyhow!(
            "no connectable '{label}' server found on the registry (only legacy sse-only entries, which aizen's client doesn't speak). Run `aizen apps search {query}` to explore."
        ));
    }
    let default_idx = app_catalog::pick_best(&viable, &prefer)
        .and_then(|best| viable.iter().position(|s| s.name == best.name))
        .unwrap_or(0);
    // One clean, COLUMN-ALIGNED line per server: ★ recommended · transport · short name · short desc.
    // (The old 2-line label repeated the name and let long descriptions wrap into a wall of text.)
    let trunc = |s: &str, n: usize| -> String {
        let s = s.replace(['\n', '\r'], " ");
        if s.chars().count() <= n {
            s
        } else {
            format!(
                "{}…",
                s.chars().take(n.saturating_sub(1)).collect::<String>()
            )
        }
    };
    let name_w = viable
        .iter()
        .map(|s| s.short_name().chars().count())
        .max()
        .unwrap_or(8)
        .clamp(8, 30);
    let tag_w = viable
        .iter()
        .map(|s| s.transport_tag().chars().count())
        .max()
        .unwrap_or(7)
        .clamp(7, 14);
    let mut labels: Vec<String> = viable
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let star = if i == default_idx { "★" } else { " " };
            let tag = format!("{:<w$}", trunc(&s.transport_tag(), tag_w), w = tag_w);
            let nm = format!("{:<w$}", trunc(&s.short_name(), name_w), w = name_w);
            format!("{star} {tag}  {nm}  {}", trunc(&s.description, 52))
        })
        .collect();
    labels.push("Cancel".to_string());
    println!(
        "{}",
        style(format!("Connect {label}  —  ★ recommended · local·X = your machine · sign-in = OAuth · hosted = third party")).dim()
    );
    let idx = match Select::with_theme(&theme)
        .with_prompt("Pick a server (↑↓, Enter)")
        .items(&labels)
        .default(default_idx)
        .interact_opt()?
    {
        Some(i) if i < viable.len() => i,
        _ => {
            println!("{}", style("cancelled.").dim());
            return Ok(());
        }
    };
    let server = viable[idx].clone();
    let key = key0.unwrap_or_else(|| app_catalog::slug_from_name(&server.name));

    // Runtime-aware: prefer a transport whose runner is actually on PATH (don't wire an npx server
    // when Node isn't installed if a remote would work).
    let choice = app_catalog::pick_transport_for_install(&server)
        .context("this server declares no transport aizen can use")?;
    let repo = server
        .repository
        .as_ref()
        .map(|r| r.url.clone())
        .unwrap_or_default();
    println!(
        "{}",
        style(format!("→ {}", server.name)).color256(splash::ACCENT)
    );
    if !server.description.is_empty() {
        println!("  {}", style(&server.description).dim());
    }
    if !repo.is_empty() {
        println!("  {}", style(&repo).dim());
    }
    if let Some(rt) = app_catalog::runtime_prereq(&server, choice) {
        let have = which_runtime(rt);
        let note = if have {
            format!("runs locally via {rt} (found)")
        } else {
            format!("runs locally via {rt} — NOT found on PATH; install it to run this app")
        };
        println!("  {}", style(note).dim());
    }
    // Static-token hosted remote → an explicit host-named confirm before we collect a token (it
    // leaves your machine for a third party). This is the strongest gate; refusing aborts the connect.
    if let app_catalog::TransportChoice::Remote(i) = choice {
        let host = server
            .remotes
            .get(i)
            .map(|r| app_catalog::host_of(&r.url))
            .unwrap_or_default();
        println!(
            "  {}",
            style(format!(
                "⚠ hosted remote @ {host} — a third party runs this server."
            ))
            .yellow()
        );
        let go = Confirm::with_theme(&theme)
            .with_prompt(format!(
                "Send your credentials to '{host}' (a third party)?"
            ))
            .default(false)
            .interact()
            .unwrap_or(false);
        if !go {
            println!(
                "{}",
                style("cancelled — no third-party remote connected.").dim()
            );
            return Ok(());
        }
    }
    // OAuth remote → you authenticate directly with the vendor (no token leaves via us); confirm we
    // may open the browser to sign in.
    if let app_catalog::TransportChoice::OAuthRemote(i) = choice {
        let host = server
            .remotes
            .get(i)
            .map(|r| app_catalog::host_of(&r.url))
            .unwrap_or_default();
        println!(
            "  {}",
            style(format!(
                "🔐 sign-in app @ {host} — Aizen will open your browser to authorize."
            ))
            .dim()
        );
        let go = Confirm::with_theme(&theme)
            .with_prompt(format!("Connect '{host}' and sign in now?"))
            .default(true)
            .interact()
            .unwrap_or(false);
        if !go {
            println!("{}", style("cancelled.").dim());
            return Ok(());
        }
    }

    // Collect any declared secrets (hidden), with a confirm gate (we're writing a token to disk).
    let mut ask = |spec: &app_catalog::PromptSpec| -> String {
        let prompt = if spec.description.is_empty() {
            format!("{} ", spec.label)
        } else {
            format!("{} ({})", spec.label, spec.description)
        };
        let val = if spec.is_secret {
            Password::with_theme(&theme)
                .with_prompt(prompt.trim())
                .allow_empty_password(true)
                .interact()
                .unwrap_or_default()
        } else {
            Input::<String>::with_theme(&theme)
                .with_prompt(prompt.trim())
                .allow_empty(true)
                .interact_text()
                .unwrap_or_default()
        };
        val.trim().to_string()
    };
    let entry = app_catalog::build_entry(&server, choice, &mut ask)?;

    // Confirm gate (we're about to write a token to disk) — show the resolved entry with secrets
    // MASKED so the user sees exactly what gets written before committing.
    println!("\n{}", style(format!("About to connect '{key}':")).bold());
    print_entry_summary(&entry, Some(&key));
    let ok = Confirm::with_theme(&theme)
        .with_prompt("Write this to mcp.json?")
        .default(true)
        .interact()
        .unwrap_or(false);
    if !ok {
        println!("{}", style("cancelled — nothing written.").dim());
        return Ok(());
    }
    app_catalog::write_server(&key, entry)?;
    crate::agent::mcp::invalidate(); // hot-reload: the next message reconnects from the new mcp.json

    // OAuth app → run the browser sign-in right now so it's usable immediately. A failure isn't fatal:
    // the entry is written, the user can retry with `aizen apps login <key>`.
    if matches!(choice, app_catalog::TransportChoice::OAuthRemote(_)) {
        match crate::agent::mcp::login(&key).await {
            Ok(()) => println!(
                "{}",
                style(format!("✓ connected & signed in to '{key}'. Its tools load on your next message (/mcp to verify)."))
                    .color256(splash::ACCENT)
            ),
            Err(e) => println!(
                "{}",
                style(format!("connected '{key}', but sign-in didn't finish — {e:#}\n  finish it with `aizen apps login {key}`."))
                    .yellow()
            ),
        }
        return Ok(());
    }
    println!(
        "{}",
        style(format!(
            "✓ connected '{key}'.  Its tools load on your next message (/mcp to verify)."
        ))
        .color256(splash::ACCENT)
    );
    Ok(())
}

/// Print an mcp.json entry's transport + config with secret VALUES masked (shared by the add-confirm
/// preview and `apps info`). Never prints a token value — presence only. `key` (when known) lets it
/// show OAuth sign-in state from the token cache.
fn print_entry_summary(entry: &serde_json::Value, key: Option<&str>) {
    if let Some(url) = entry.get("url").and_then(|v| v.as_str()) {
        println!("  {} remote (streamable-http)", style("transport").dim());
        println!("  {} {url}", style("url      ").dim());
        println!(
            "  {} {}",
            style("host     ").dim(),
            style(app_catalog::host_of(url)).dim()
        );
        if entry.get("auth").and_then(|v| v.as_str()) == Some("oauth") {
            let signed = key.map(crate::agent::mcp_oauth::has_token).unwrap_or(false);
            let state = if signed {
                "signed in".to_string()
            } else {
                "not signed in — `aizen apps login <key>`".to_string()
            };
            println!("  {} oauth ({state})", style("auth     ").dim());
        }
    } else if let Some(cmd) = entry.get("command").and_then(|v| v.as_str()) {
        let args = entry
            .get("args")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        println!("  {} local (stdio)", style("transport").dim());
        println!("  {} {cmd} {args}", style("command  ").dim());
        if !cmd.contains(['/', '\\']) {
            let have = which_runtime(cmd);
            let note = if have {
                format!("{cmd}: found on PATH")
            } else {
                format!("{cmd}: NOT on PATH — install it to run this app")
            };
            println!("  {} {note}", style("runtime  ").dim());
        }
    }
    for field in ["env", "headers"] {
        if let Some(obj) = entry.get(field).and_then(|v| v.as_object()) {
            for (k, v) in obj {
                println!(
                    "  {} {k} = {}",
                    style(format!("{field:<8}")).dim(),
                    mask_secret(v.as_str().unwrap_or(""))
                );
            }
        }
    }
}

/// Mask a secret/config value for display: presence only, never the value (the standing key-safety
/// rule). Empty → "(empty)"; set → "•••• (set)".
fn mask_secret(v: &str) -> String {
    if v.trim().is_empty() {
        style("(empty)").dim().to_string()
    } else {
        style("•••• (set)").dim().to_string()
    }
}

/// `aizen apps info <key>` — the detail view for ONE connected app: its mcp.json config (transport +
/// secrets MASKED) plus a LIVE probe (handshake + the tools it actually exposes, or why it failed).
pub(crate) async fn apps_info(key: &str) -> Result<()> {
    let Some(entry) = app_catalog::installed_entry(key) else {
        return Err(anyhow!(
            "no connected app keyed '{key}' — see `aizen apps list`"
        ));
    };
    println!("{}", style(key).color256(splash::ACCENT).bold());
    print_entry_summary(&entry, Some(key));

    // Live probe.
    println!("  {}", style("probing (connect + tools/list)…").dim());
    match crate::agent::mcp::probe(key).await {
        Ok(rep) => {
            let info = rep.server_info.get("serverInfo");
            let sname = info
                .and_then(|s| s.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or(key);
            let sver = info
                .and_then(|s| s.get("version"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            println!(
                "  {} {}",
                style("✓").color256(splash::ACCENT),
                style(format!("{sname} {sver}  ·  {} tool(s)", rep.tools.len())).bold()
            );
            for t in &rep.tools {
                let ro = if t.read_only {
                    style(" [read-only]").dim().to_string()
                } else {
                    String::new()
                };
                let d: String = t.description.chars().take(72).collect();
                println!(
                    "    {}{ro}  {}",
                    style(&t.name).color256(splash::ACCENT),
                    style(d).dim()
                );
            }
            if rep.tools.is_empty() {
                println!("    {}", style("(this server advertised no tools)").dim());
            }
        }
        // `{e:#}` = the full anyhow chain (includes the server's stderr tail captured by the client).
        Err(e) => println!("  {}", style(format!("✗ could not connect — {e:#}")).red()),
    }
    Ok(())
}

/// Best-effort PATH check for a runner (npx/uvx/docker) — Windows adds `.cmd`/`.exe` variants.
fn which_runtime(rt: &str) -> bool {
    let exts: &[&str] = if cfg!(windows) {
        &["", ".cmd", ".exe", ".bat"]
    } else {
        &[""]
    };
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path) {
        for ext in exts {
            if dir.join(format!("{rt}{ext}")).is_file() {
                return true;
            }
        }
    }
    false
}
