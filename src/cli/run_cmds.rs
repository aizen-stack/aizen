//! The one-shot, non-interactive subcommands: `prompt-size`, `crawl`, `reach`, `models`, `chat`,
//! `agent`, `workflow`.
//!
//! Each resolves an endpoint, does one job and exits — no REPL, no session file unless asked. They
//! share the turn helpers with the interactive surfaces so behaviour cannot drift between them.

use crate::agent::prompt_lanes::active_system_prompt_bundle;
use crate::agent::{self, AgentConfig, StopReason};
use crate::cli::read_stdin;
use crate::cli_args::*;
use crate::core::approval::ApprovalMode;
use crate::core::endpoint::{http_client, resolve_base_key, resolve_endpoint};
use crate::core::types::ToolDef;
use crate::core::{cli_config, session_store, types};
use crate::features::crawl;
use crate::llm::client;
use crate::memory;
use crate::ui::context_report::resolve_ctx_window;
use crate::{arm_lsp_session, eager_enabled};
use anyhow::{Context, Result};
use console::style;
use types::Message;

/// `aizen prompt-size` — byte breakdown of the per-turn fixed overhead (system prompt + tool
/// schemas). Offline: builds the same lanes and the same registry a real turn would, then measures
/// them. No request is made, so it costs nothing and works without a configured key.
pub(crate) fn run_prompt_size(
    model: Option<String>,
    show_tools: bool,
    as_json: bool,
) -> Result<()> {
    let model = model
        .or_else(|| cli_config::load().model)
        .unwrap_or_else(|| "gpt-4o".to_string());

    // Same lanes a turn would send: static base + <environment> (stable) and the memory/persona/
    // tool-routing blocks (dynamic). LSP is armed first so the registry advertises the symbolic-edit
    // tools; the registry is built BEFORE the lanes because it publishes the tool surface the routing
    // map is generated from — measuring the lanes first would under-report the real per-turn cost.
    arm_lsp_session();
    let registry = agent::builtin::default_registry_with_task(
        reqwest::Client::new(),
        String::new(),
        String::new(),
        model.clone(),
        crate::core::approval::ApprovalMode::Ask,
        resolve_ctx_window(&model).0,
        None,
    )?;
    let bundle = active_system_prompt_bundle(&model);
    let defs = registry.defs();
    let tools_json = serde_json::to_string(&defs)?;

    let stable = bundle.stable.len();
    let dynamic = bundle.dynamic.len();
    let prompt = stable + dynamic;
    let tools_bytes = tools_json.len();
    let total = prompt + tools_bytes;
    // Rough: 4 bytes/token. The real count is tokenizer-specific — this is a budget, not a bill.
    let tok = |b: usize| b / 4;

    let mut per_tool: Vec<(usize, String)> = defs
        .iter()
        .map(|d| {
            let n = serde_json::to_string(d).map(|s| s.len()).unwrap_or(0);
            (n, d.function.name.clone())
        })
        .collect();
    per_tool.sort_by(|a, b| b.0.cmp(&a.0));

    if as_json {
        let out = serde_json::json!({
            "model": model,
            "system_prompt": { "bytes": prompt, "stable_bytes": stable, "dynamic_bytes": dynamic },
            "tools": { "count": defs.len(), "json_bytes": tools_bytes },
            "fixed_total": { "bytes": total, "approx_tokens": tok(total) },
            "per_tool": per_tool
                .iter()
                .map(|(n, name)| serde_json::json!({ "name": name, "bytes": n }))
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let kb = |b: usize| format!("{:.1} KB", b as f64 / 1024.0);
    println!("Prompt-size breakdown (model={model})\n");
    println!("  System prompt        : {prompt:>8} B  ({})", kb(prompt));
    println!(
        "    stable  (base+env) : {stable:>8} B  ({})  \u{2190} cache-stable prefix",
        kb(stable)
    );
    println!("    dynamic (memory)   : {dynamic:>8} B  ({})", kb(dynamic));
    println!(
        "  Tool schemas         : {tools_bytes:>8} B  ({}, {} tools, avg {} B)",
        kb(tools_bytes),
        defs.len(),
        if defs.is_empty() {
            0
        } else {
            tools_bytes / defs.len()
        }
    );
    println!(
        "  Fixed per turn       : {total:>8} B  ({}, ~{}k tokens)",
        kb(total),
        tok(total) / 1000
    );
    if show_tools {
        println!("\n  Per tool, largest first:");
        for (n, name) in &per_tool {
            println!("    {n:>6} B  {name}");
        }
    } else {
        println!("\n  (--tools for per-tool sizes, --json for machine output)");
    }
    Ok(())
}

pub(crate) async fn run_crawl(args: CrawlArgs) -> Result<()> {
    let opts = crawl::CrawlOptions {
        seeds: args.urls,
        max_depth: args.depth,
        max_pages: args.max_pages,
        scope: crawl::Scope::parse(&args.scope)?,
        concurrency: args.concurrency,
        timeout_secs: args.timeout,
    };
    let http = http_client()?;
    let report = crawl::crawl(&http, &opts).await.context("crawl failed")?;

    if args.json {
        let arr: Vec<serde_json::Value> = report
            .found
            .iter()
            .map(|f| serde_json::json!({"url": f.url, "depth": f.depth, "via": f.via.tag()}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else {
        for f in &report.found {
            if args.show_source {
                println!(
                    "{}  {}",
                    f.url,
                    style(format!("[{} d{}]", f.via.tag(), f.depth)).dim()
                );
            } else {
                println!("{}", f.url);
            }
        }
    }
    eprintln!(
        "{}",
        style(format!(
            "crawled {} page(s) → {} URL(s)",
            report.pages_fetched,
            report.found.len()
        ))
        .dim()
    );
    Ok(())
}

/// `aizen reach doctor [--json]` / `aizen reach status` — the web-access health check.
pub(crate) async fn run_reach(cmd: ReachCmd) -> Result<()> {
    match cmd {
        ReachCmd::Status => {
            println!("{}", crate::agent::reach::render_passive());
        }
        ReachCmd::Doctor { json } => {
            if !json {
                eprintln!("{}", style("probing every backend (a few seconds)…").dim());
            }
            let reports = crate::agent::reach::doctor().await;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&crate::agent::reach::report_json(&reports))?
                );
            } else {
                println!("{}", crate::agent::reach::render_report(&reports));
            }
        }
    }
    Ok(())
}

/// Run the agent loop once (non-streaming, quiet) and return its final text — used by `aizen serve`
/// to answer a Telegram message.
pub(crate) async fn run_agent_capture(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    task: &str,
    approval_mode: ApprovalMode,
) -> Result<String> {
    let frozen = memory::refresh_frozen_core();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    // Registry BEFORE prompt: building it publishes this session's live tool surface, which is what
    // the prompt's tool-routing map is generated from. Assembling the prompt first would emit a
    // routing map for whatever surface a previous run left published — or none at all on the first.
    let registry = agent::builtin::default_registry_with_task(
        http.clone(),
        base_url.to_string(),
        api_key.to_string(),
        model.to_string(),
        approval_mode,
        resolve_ctx_window(model).0,
        None, // cwd IS the project on the CLI path
    )?;
    let system = agent::build_top_level_system_prompt(
        &cwd,
        std::env::consts::OS,
        &date,
        model,
        Some(&frozen),
    );
    let cfg = AgentConfig {
        approval_mode,
        quiet: true,
        enable_verify_gate: false,
        ..Default::default()
    };

    let http_ref = http;
    let base = base_url;
    let key = api_key;
    let model_ref = model;
    let chat = move |msgs: Vec<Message>, defs: Vec<ToolDef>| async move {
        client::chat_with_tools(http_ref, base, key, model_ref, &msgs, &defs).await
    };
    let outcome = agent::run_agent(chat, &cfg, &registry, &system, task).await?;
    // A `clarify` yield in a captured (non-REPL) run — e.g. `aizen serve` — has no input box to loop
    // back to, so surface the question as the reply itself. Over Telegram the owner just answers
    // with their next message; for a plain capture caller it reads as the agent's question.
    if let StopReason::AwaitingInput(q) = &outcome.stop {
        return Ok(format!("❓ {q}"));
    }
    Ok(outcome
        .final_text
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "(the agent produced no answer)".to_string()))
}

pub(crate) async fn run_models(args: ModelsArgs) -> Result<()> {
    let (base_url, api_key) = resolve_base_key(args.base_url, args.api_key)?;
    // Codex has no stable OpenAI-style /models; print the curated experimental catalog.
    if crate::llm::oauth_codex::is_codex_base_url(&base_url) {
        let current = cli_config::load().model;
        println!("ChatGPT Codex models (experimental catalog):");
        for (id, label) in crate::llm::codex_models::CODEX_MODELS {
            let mark = if current.as_deref() == Some(*id) {
                " (default)"
            } else {
                ""
            };
            println!("{id}  · {label}  · codex{mark}");
        }
        if !crate::llm::oauth_codex::has_token() {
            println!("(not logged in — run: aizen auth login codex)");
        }
        return Ok(());
    }
    let http = http_client()?;
    let infos = client::fetch_models_info(&http, &base_url, &api_key)
        .await
        .context("fetching models")?;
    if infos.is_empty() {
        println!("(provider returned no models)");
        return Ok(());
    }
    let current = cli_config::load().model;
    let any_ctx = infos.iter().any(|m| m.context_length.is_some());
    for m in &infos {
        let mark = if current.as_deref() == Some(m.id.as_str()) {
            " (default)"
        } else {
            ""
        };
        let free = if m.is_free || client::is_free_model_id(&m.id) {
            "  · free"
        } else {
            ""
        };
        let ctx = match m.context_length {
            Some(n) if n >= 1000 => format!("  · ctx {}K", n / 1000),
            Some(n) => format!("  · ctx {n}"),
            None => String::new(),
        };
        println!("{}{free}{ctx}{mark}", m.id);
    }
    if !any_ctx {
        println!(
            "\n{}",
            style(
                "(this provider doesn't report context windows — the HUD estimates by model name)"
            )
            .dim()
        );
    }
    println!("\nset a default: `aizen config set --model <id>`");
    Ok(())
}

pub(crate) async fn run_chat(args: ChatArgs) -> Result<()> {
    let prompt = match args.prompt {
        Some(p) => p,
        None => read_stdin("reading prompt from stdin")?,
    };
    if prompt.trim().is_empty() {
        anyhow::bail!("empty prompt (pass --prompt or pipe text on stdin)");
    }
    let (base_url, api_key, model) = resolve_endpoint(args.base_url, args.api_key, args.model)?;
    let http = http_client()?;

    let messages = vec![Message::user(prompt)];
    client::stream_chat_with_visual_contract(&http, &base_url, &api_key, &model, messages, true)
        .await
        .context("chat completion failed")?;
    Ok(())
}

pub(crate) async fn run_agent_cmd(args: AgentArgs) -> Result<()> {
    if args.task.trim().is_empty() {
        anyhow::bail!("empty task (pass the task as the first argument)");
    }
    let (base_url, api_key, model) = resolve_endpoint(args.base_url, args.api_key, args.model)?;
    let http = http_client()?;

    // Armed before anything builds a request, and never cleared: a one-shot is one turn, and the
    // process ends with it. `auto` runs the same classifier a typed REPL turn goes through, so the
    // two surfaces answer "how hard should this be" the same way.
    if let Some(want) = args.effort.as_deref() {
        let tier = match want {
            "auto" => crate::ui::effort_ui::resolve_turn_effort(args.task.trim()),
            other => Some(other.to_string()),
        };
        eprintln!(
            "{}",
            crate::ui::effort_ui::effort_turn_line(tier.as_deref())
        );
        cli_config::set_effort_override(tier);
    }

    // Session start: rebuild the always-on core for THIS project slug (STYLE + global prefs
    // only). Do not reuse a stale foreign-repo core.active — refresh_frozen_core is slug-aware.
    let frozen = memory::refresh_frozen_core();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();

    // Registry includes the `task` sub-agent tool (depth 0); a spawned sub-agent uses a
    // role-scoped registry WITHOUT `task` (no recursion).
    let cli_approval = if args.yes {
        ApprovalMode::Yolo
    } else {
        ApprovalMode::Ask
    };
    arm_lsp_session();
    // Built BEFORE the prompt: it publishes the live tool surface the routing map is generated from.
    let registry = agent::builtin::default_registry_with_task(
        http.clone(),
        base_url.clone(),
        api_key.clone(),
        model.clone(),
        cli_approval,
        resolve_ctx_window(&model).0,
        None, // cwd IS the project on the CLI path
    )?;
    let system = agent::build_top_level_system_prompt(
        &cwd,
        std::env::consts::OS,
        &date,
        &model,
        Some(&frozen),
    );
    let max = args.max_iters.unwrap_or(25).max(1);
    let cfg = AgentConfig {
        max_iters: max,
        auto_extend_to: max.saturating_mul(2),
        approval_mode: cli_approval,
        context_window: resolve_ctx_window(&model).0,
        enable_lsp: crate::agent::lsp::LSP.is_enabled(),
        ..Default::default()
    };

    // The model call, injected into the loop. http_ref/base/key/model are all Copy
    // (&Client / &str), so the closure stays `Fn` across the loop's repeated calls.
    let http_ref = &http;
    let base = base_url.as_str();
    let key = api_key.as_str();
    let model_ref = model.as_str();
    let registry_ref = &registry;
    let cfg_ref = &cfg;
    let eager_on = eager_enabled();
    let chat = move |msgs: Vec<Message>, defs: Vec<ToolDef>| async move {
        if eager_on {
            let starter = agent::eager_starter(registry_ref, cfg_ref);
            client::stream_chat_with_tools_eager(
                http_ref,
                base,
                key,
                model_ref,
                &msgs,
                &defs,
                Some(&starter),
            )
            .await
        } else {
            client::stream_chat_with_tools(http_ref, base, key, model_ref, &msgs, &defs).await
        }
    };

    // The transcript is built here rather than inside `run_agent` so this path can still hold it
    // afterwards: the loop appends every turn — the final answer included — to the vector it is
    // handed, and `run_agent` is exactly these two opening messages plus that call.
    let mut history = vec![Message::system(&system), Message::user(args.task.trim())];
    let result = agent::run_agent_loop(chat, &cfg, &registry, &mut history).await;
    // Saved before the error is propagated. A run that ended badly still happened, and the REPL
    // treats persistence as not optional — that promise should not be weaker off a terminal.
    if args.save_session {
        save_finished_session(&history, &model);
    }
    let outcome = result?;
    match outcome.stop {
        // The final answer was already streamed to stdout during the call.
        StopReason::Done => {}
        StopReason::Divergence => eprintln!(
            "\n[stopped after {} steps: recent attempts added no new evidence; the answer above is the best result from established facts]",
            outcome.iters
        ),
        StopReason::MaxIters => eprintln!(
            "\n[stopped: step budget exhausted after {} steps, including the automatic continuations — the task may be incomplete]",
            outcome.iters
        ),
        StopReason::VerificationFailed => eprintln!(
            "\n[stopped: edits were made but verification never passed after {} steps]",
            outcome.iters
        ),
        // One-shot `aizen agent` is non-interactive: there is no next message to answer with, so
        // surface the question and exit rather than hang. Re-run in the REPL to answer it.
        StopReason::AwaitingInput(q) => eprintln!(
            "\n[the agent needs clarification — re-run interactively (`aizen`) to answer]\n❓ {q}"
        ),
        StopReason::Cancelled => eprintln!(
            "\n[stopped: cancelled by user after {} step(s)]",
            outcome.iters
        ),
        // A top-level run sets no wall-clock budget (the user is watching and owns Esc), so this is
        // effectively unreachable here — but the match must be total, and if a caller ever does set
        // one, saying "time" rather than "steps" is the difference between a useful message and a
        // misleading one.
        StopReason::Deadline => eprintln!(
            "\n[stopped: wall-clock budget reached after {} step(s) — the task may be incomplete]",
            outcome.iters
        ),
    }
    Ok(())
}

/// Put a finished one-shot conversation into core's own session pool, and say where it went.
///
/// The stamp is `save_session`'s: project key, root and slug come from `config`, which resolves the
/// repository this ran in — so a saved one-shot is filed exactly where the same conversation held
/// in the REPL would have been, and `/sessions` reopens it with no idea which surface produced it.
///
/// The line goes to stderr because stdout is the agent's answer: a caller piping it wants the
/// answer and nothing else.
fn save_finished_session(history: &[Message], model: &str) {
    // A run that never got a user turn onto the wire is not a conversation.
    if !history.iter().any(|m| m.role == "user") {
        return;
    }
    let slug = session_store::allocate_session_slug(history);
    match session_store::save_session(history, &slug, Some(model)) {
        Ok(path) => eprintln!("\n[saved as “{slug}” — {path}]"),
        // Not fatal: the work is done and the answer is already printed. Saying so is the whole
        // duty here — silence would leave the caller believing there is something to reopen.
        Err(e) => eprintln!("\n[this session was NOT saved: {e:#}]"),
    }
}

pub(crate) async fn run_workflow_cmd(args: WorkflowArgs) -> Result<()> {
    let text = std::fs::read_to_string(&args.spec)
        .with_context(|| format!("reading workflow spec {}", args.spec))?;
    let spec: agent::workflow::WorkflowSpec =
        serde_json::from_str(&text).context("parsing workflow spec JSON")?;

    let (base_url, api_key, model) = resolve_endpoint(args.base_url, args.api_key, args.model)?;
    let http = http_client()?;
    let trace = args.trace.as_deref().map(std::path::Path::new);

    let approval = if args.yes {
        ApprovalMode::Yolo
    } else {
        ApprovalMode::Ask
    };
    agent::workflow::run_workflow(&http, &base_url, &api_key, &model, approval, &spec, trace).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::session_store::{parse_session_bytes, sessions_dir, set_session_slug};

    /// What `--save-session` is FOR: a one-shot run leaves a file the picker can reopen, stamped
    /// with the project it ran in. The provenance is the point — a transcript on disk that cannot
    /// say which repo it came from is what makes a per-project session list impossible.
    #[test]
    fn a_saved_one_shot_lands_in_the_pool_with_its_provenance() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-oneshot-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("AIZEN_HOME", &home);
        set_session_slug(None);
        std::fs::create_dir_all(sessions_dir()).unwrap();

        let history = vec![
            Message::system("lane"),
            Message::user("fix the delete button"),
            Message::assistant("done"),
        ];
        save_finished_session(&history, "model-x");

        let files: Vec<_> = std::fs::read_dir(sessions_dir())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        assert_eq!(files.len(), 1, "one run, one file");
        // Named from the task rather than a timestamp: the picker is read by a person.
        let stem = files[0].file_stem().unwrap().to_string_lossy().into_owned();
        assert!(
            stem.contains("fix"),
            "slug should come from the task: {stem}"
        );

        let (msgs, meta) = parse_session_bytes(&std::fs::read(&files[0]).unwrap())
            .expect("a transcript we wrote ourselves must be readable");
        assert_eq!(msgs.len(), 3);
        let meta = meta.expect("a session written today carries meta");
        assert_eq!(meta.model.as_deref(), Some("model-x"));
        assert!(
            meta.project_root.is_some_and(|r| !r.is_empty()),
            "without a root, no front-end can tell whose session this is"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    /// A run that never got a question onto the wire is not a conversation, and must not litter the
    /// pool with an empty file the picker would then offer to restore.
    #[test]
    fn a_run_with_no_user_turn_writes_nothing() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-oneshot-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("AIZEN_HOME", &home);
        set_session_slug(None);
        std::fs::create_dir_all(sessions_dir()).unwrap();

        save_finished_session(&[Message::system("lane")], "model-x");

        assert_eq!(std::fs::read_dir(sessions_dir()).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&home);
    }
}
