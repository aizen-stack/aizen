//! `aizen memory …` — the CLI half of the memory store (the REPL half is `/memory`).
//!
//! Both surfaces call the same functions with the same ids, so a fact written from one is visible
//! and correctable from the other. `reconcile` is the batch pass that retires superseded facts.

use crate::cli::read_stdin;
use crate::cli::where_report::memory_where_report;
use crate::cli_args::*;
use crate::core::endpoint::{http_client, resolve_endpoint};
use crate::core::{config, types};
use crate::memory;
use crate::repl::postturn::chore_chat;
use crate::summarizer_endpoint;
use anyhow::Result;
use types::Message;

/// `aizen memory reconcile [--apply]` — the M2b batch pass, run by hand.
///
/// One model call, at most `MAX_PAIRS` pairs, and **dry-run by default**: the actions this pass
/// proposes overwrite bodies and retire facts, so the harmless mode has to be the one you get by
/// typing the short command. `--apply` is the sentence where the user says they read the dry run.
///
/// The call is routed through the summarizer role like every other chore call, so a cheap model can
/// own it without touching the main endpoint.
async fn run_memory_reconcile(apply: bool) -> Result<()> {
    let (pairs, live) = memory::reconcile_inputs()?;
    if pairs.is_empty() {
        crate::ui::tui::emit_line("no suspicious pairs — nothing to reconcile.");
        return Ok(());
    }
    let (base, key, model) = resolve_endpoint(None, None, None)?;
    let http = http_client()?;
    let ep = summarizer_endpoint(&base, &key, &model);

    // `judge` is the ONLY place this pass can reach a model, and it is `FnOnce` — the ≤1-call budget
    // is enforced by the type, not by remembering to not loop.
    let judge = |sys: &str, user: &str| -> Option<String> {
        let msgs = [
            Message::system(sys.to_string()),
            Message::user(user.to_string()),
        ];
        let fut = chore_chat(&http, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[]);
        // The surrounding fn is async, but `batch_pass` is sync (so every rail in it stays unit
        // testable without a runtime). Blocking here is safe: this is a one-shot CLI command with
        // nothing else on the runtime waiting on us.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(fut)
                .ok()?
                .content
        })
    };

    let report = memory::learning::reconcile::batch_pass(
        &pairs,
        judge,
        !apply,
        &memory::learning::default_session_id(),
        &live,
    );
    memory::print_reconcile_report(&report);
    Ok(())
}

pub(crate) async fn run_memory(cmd: MemoryCmd) -> Result<()> {
    match cmd {
        MemoryCmd::Add {
            name,
            description,
            mtype,
            body,
        } => {
            let body = match body {
                Some(b) => b,
                None => read_stdin("reading memory body from stdin")?,
            };
            if body.trim().is_empty() {
                anyhow::bail!("empty memory body (pass --body or pipe text on stdin)");
            }
            memory::cmd_add(&name, &description, &mtype, body.trim())
        }
        MemoryCmd::List {
            scope,
            scope_flag,
            superseded,
        } => {
            if superseded {
                memory::cmd_list_superseded()
            } else {
                memory::cmd_list(scope.or(scope_flag).as_deref())
            }
        }
        MemoryCmd::Revive { id } => memory::cmd_revive(&id),
        MemoryCmd::Show { id } => memory::cmd_show(&id),
        MemoryCmd::Search {
            query,
            k,
            dimension,
            category,
            scope,
        } => memory::cmd_search(&query, k, dimension, category, scope.as_deref()),
        MemoryCmd::Frozen { rebuild } => memory::cmd_frozen(rebuild),
        MemoryCmd::Learn { text, yes, dry_run } => {
            let text = match text {
                Some(t) => t,
                None => read_stdin("reading user turn from stdin")?,
            };
            if text.trim().is_empty() {
                anyhow::bail!("empty turn (pass text or pipe it on stdin)");
            }
            memory::cmd_learn(text.trim(), yes, dry_run)
        }
        MemoryCmd::Style => memory::cmd_style(),
        MemoryCmd::Profile { json } => memory::cmd_profile(json),
        MemoryCmd::Ask { question, json } => memory::cmd_ask(&question, json),
        MemoryCmd::Review {
            promote,
            drop_key,
            clear,
        } => memory::cmd_review(promote, drop_key, clear),
        MemoryCmd::AsOf { date } => memory::cmd_as_of(date.trim()),
        MemoryCmd::Supersede { old, new } => memory::cmd_supersede(&old, &new),
        MemoryCmd::Edit {
            id,
            name,
            description,
            mtype,
            body,
            scope,
        } => {
            // `--body -` reads the replacement body from stdin (so a multi-line rewrite can be piped
            // in); omitting `--body` entirely leaves the body untouched.
            let body = match body.as_deref() {
                Some("-") => Some(read_stdin("reading replacement body from stdin")?),
                _ => body,
            };
            memory::cmd_edit(&id, name, description, mtype, body, scope)
        }
        MemoryCmd::Forget { id } => memory::cmd_forget(&id),
        MemoryCmd::Purge { id, yes } => {
            if !yes {
                anyhow::bail!(
                    "`memory purge` permanently deletes an archived fact — pass --yes to confirm"
                );
            }
            memory::cmd_purge(&id)
        }
        MemoryCmd::Archive => memory::cmd_archive_list(),
        MemoryCmd::Restore { id, as_id } => memory::cmd_restore(&id, as_id.as_deref()),
        MemoryCmd::Compact => memory::cmd_compact(),
        MemoryCmd::Reconcile { apply } => run_memory_reconcile(apply).await,
        MemoryCmd::Doctor => memory::cmd_doctor(),
        MemoryCmd::Where => {
            println!("{}", memory_where_report());
            Ok(())
        }
        MemoryCmd::Health => memory::cmd_health(),
        MemoryCmd::Neighbors { id, k } => memory::cmd_neighbors(&id, k),
        MemoryCmd::ModelDownload { name } => memory::model_dl::download(name.as_deref())
            .await
            .map(|_| ()),
        MemoryCmd::ModelList => run_memory_model_list(),
    }
}

/// `aizen memory model-list` — show every model2vec model this machine already has, and which one
/// the dense tier would pick. Exists because the old failure mode was SILENT: with no model at the
/// configured name the loader fell back to the (non-semantic) hashing embedder, so a user who had
/// downloaded a perfectly good model under another name had no way to see why dense wasn't working.
fn run_memory_model_list() -> Result<()> {
    let configured = config::embed_model_name();
    let found = memory::embed::list_local_models();
    let chosen = memory::embed::discover_local_model();
    println!("configured model name: {configured}");
    println!("(override with AIZEN_EMBED_MODEL)");
    println!();
    if found.is_empty() {
        println!("no model2vec models found on this machine.");
        println!("  looked in: {}", config::models_dir().display());
        println!("             the Hugging Face hub cache (~/.cache/huggingface/hub, %LOCALAPPDATA%\\huggingface\\hub, $HF_HUB_CACHE)");
        println!();
        println!("get one with: aizen memory model-download");
        return Ok(());
    }
    println!(
        "found {} model2vec model{}:",
        found.len(),
        if found.len() == 1 { "" } else { "s" }
    );
    let chosen_dir = chosen.as_ref().map(|c| c.dir.clone());
    for c in &found {
        let marker = if Some(&c.dir) == chosen_dir.as_ref() {
            "▸"
        } else {
            " "
        };
        println!("  {marker} {} [{}]  {}", c.name, c.source, c.dir.display());
    }
    println!();
    match &chosen {
        Some(c) if c.name == configured => {
            println!("dense would load '{}' (the configured name).", c.name);
        }
        Some(c) => {
            println!(
                "dense would AUTO-DETECT '{}' from {} — '{configured}' is not present.",
                c.name, c.source
            );
        }
        None => println!("dense would fall back to the hashing embedder (not semantic)."),
    }
    // The weights only LOAD on a `--features dense` build; say so rather than implying the default
    // binary will use what we just listed.
    if cfg!(feature = "dense") {
        println!("this build has the dense feature: the model above will be loaded.");
    } else {
        println!(
            "note: this build has NO dense feature — rebuild with `--features dense` to use it."
        );
    }
    Ok(())
}
