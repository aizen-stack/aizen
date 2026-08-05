//! The generic host-bot daemon — ONE serial agent runner that drives any `Platform`.
//!
//! `aizen serve` (Telegram) and `aizen discord serve` (Discord) both funnel into `run_daemon<P>`:
//! the platform spawns its listener(s), inbound messages arrive on one channel tagged with the
//! originating sub-bot (`Inbound::route`), and a single loop processes them one at a time — chatting,
//! running `/commands`, and (on platforms that support it) routing a destructive-op approval back to
//! the SAME bot the request came from. Because it's serial, that per-turn approval route never races.
//!
//! Sessions are keyed by `(route, chat)` and persisted to `~/.aizen/hostbot/sessions/` after every
//! turn, so a daemon restart (the point of `Restart=always` self-host) keeps its context.

use anyhow::{Context, Result};
use console::style;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::mpsc::{self, Sender};

use crate::agent::{self, AgentConfig, StopReason};
use crate::core::approval::ApprovalMode;
use crate::core::types::Message;
use crate::hostbot::health;
use crate::hostbot::lane;
use crate::hostbot::platform::{Inbound, Platform};
use crate::hostbot::platforms;
use crate::hostbot::store;
use crate::llm::client;

/// Resolve when the host asks us to stop, and name which signal did it. Ctrl-C is the terminal case;
/// SIGTERM is how systemd, `docker stop`, and Kubernetes all ask first (SIGKILL follows their grace
/// period, and nothing can be done about that one). Returning the name lets the log say which.
async fn shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        // If installing the SIGTERM handler fails we still honor Ctrl-C rather than never returning.
        match signal(SignalKind::terminate()) {
            Ok(mut term) => tokio::select! {
                _ = tokio::signal::ctrl_c() => "SIGINT",
                _ = term.recv() => "SIGTERM",
            },
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                "SIGINT"
            }
        }
    }
    // Windows has no SIGTERM. `ctrl_c` also fires for Ctrl-Break; window-close / logoff / shutdown are
    // handled by the console control handler installed elsewhere.
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "SIGINT"
    }
}

/// The single source of truth for the command surface: `(name, arg-hint, description)`. Published to
/// Telegram's "/" menu via `setMyCommands` and rendered into `/help`. The dispatcher arms in
/// `handle_command` must stay in sync with this list.
const SERVE_COMMANDS: &[(&str, &str, &str)] = &[
    ("help", "", "list commands"),
    ("new", "", "start a fresh conversation (drop context)"),
    ("resume", "", "how much context is kept"),
    (
        "sh",
        "<cmd>",
        "run a shell command now (dangerous-command floor still applies)",
    ),
    (
        "agent",
        "<task>",
        "run the agent autonomously (file edits / shell, no per-op ask)",
    ),
    (
        "cd",
        "<path>",
        "change THIS bot's working directory (affects /sh and the agent)",
    ),
    ("pwd", "", "show this bot's working directory"),
    (
        "approval",
        "<ask|smart|yolo>",
        "set THIS bot's approval level",
    ),
    (
        "ultimate",
        "",
        "toggle ultimate mode for this bot (max effort + prefer workflows)",
    ),
    (
        "effort",
        "<level>",
        "this bot's reasoning effort: auto | low | medium | high | xhigh | max | off",
    ),
    ("model", "[name]", "show or switch THIS bot's model"),
    (
        "memory",
        "[query]",
        "show profile / search; \"memory remember <fact>\" to save",
    ),
    ("tools", "", "list tool bundles"),
    (
        "status",
        "",
        "this bot's cwd · model · approval mode · effort · context size",
    ),
    ("bots", "", "list the bots this daemon hosts"),
    (
        "addbot",
        "<name> <token> [--own]",
        "host another bot from this daemon; --own gives it its own owner (Telegram only)",
    ),
    (
        "rmbot",
        "<name>",
        "stop hosting a bot (cannot remove \"default\")",
    ),
];

/// Build the `/help` reply from `SERVE_COMMANDS` so it always matches the app's "/" menu.
fn serve_help_text() -> String {
    let mut s = String::from("Aizen bot — control this machine from chat. Commands:\n");
    for (name, hint, desc) in SERVE_COMMANDS {
        if hint.is_empty() {
            s.push_str(&format!("/{name} — {desc}\n"));
        } else {
            s.push_str(&format!("/{name} {hint} — {desc}\n"));
        }
    }
    s.push_str(
        "\nAny other message → the agent (chats + uses tools). Default: destructive ops ask for \
         approval here (✓/✗ where supported). `/approval smart` auto-runs read-only shell; \
         `/approval yolo` pre-authorizes the rest after the hard floor.",
    );
    s
}

/// The command menu as `(name, description)` pairs — what a platform publishes to its slash menu.
fn command_menu() -> Vec<(String, String)> {
    SERVE_COMMANDS
        .iter()
        .map(|(n, _, d)| (n.to_string(), d.to_string()))
        .collect()
}

// ── per-lane settings ────────────────────────────────────────────────────────────────
//
// Each hosted bot ("route") gets its own working directory, model, effort, and approval tier. They
// live in `hostbot/lanes.json` rather than `cli-config.json` because that file is the MACHINE's
// config, shared with the REPL: writing it from a chat meant one bot's `/model` switched the model
// for every other bot AND for the user's terminal session.
//
// A lane with no stored value inherits the machine config, so an install that never used these
// commands behaves exactly as before.

/// This lane's working directory: its own `/cd`, else the process cwd.
fn lane_cwd(route: &str) -> std::path::PathBuf {
    store::load_lane(route)
        .cwd
        .filter(|p| p.is_dir()) // a directory deleted since it was pinned ⇒ fall back rather than fail every turn
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// This lane's model: its own `/model`, else the machine default.
fn lane_model(route: &str, fallback: &str) -> String {
    store::load_lane(route)
        .model
        .unwrap_or_else(|| fallback.to_string())
}

/// This lane's approval tier: its own `/approval`, else the machine setting.
fn lane_approval(route: &str) -> ApprovalMode {
    store::load_lane(route)
        .approval
        .and_then(|s| s.parse::<ApprovalMode>().ok())
        .unwrap_or_else(crate::approval_mode)
}

/// This lane's `reasoning_effort` for the wire, resolved the same three ways the global path does:
/// `Some(Some(tier))` send it, `Some(None)` omit it deliberately (`/effort off`), `None` no lane
/// opinion — fall through to the machine config.
#[allow(clippy::option_option)]
fn lane_effort(route: &str) -> Option<Option<String>> {
    let lane = store::load_lane(route);
    match lane.effort.as_deref() {
        None => None,
        Some("off") | Some("none") => Some(None),
        Some(tier) => Some(Some(tier.to_string())),
    }
}

/// Split text under a platform's UTF-16 limit, preferring newline boundaries so table records and
/// text-diagram rows stay intact. A single over-limit line falls back to scalar-safe splitting.
fn chunk_text(s: &str, max: usize) -> Vec<String> {
    if s.encode_utf16().count() <= max {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_units = 0usize;
    for segment in s.split_inclusive('\n') {
        let units = segment.encode_utf16().count();
        if units <= max {
            if cur_units + units > max && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                cur_units = 0;
            }
            cur.push_str(segment);
            cur_units += units;
            continue;
        }
        if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        let mut piece = String::new();
        let mut piece_units = 0usize;
        for ch in segment.chars() {
            let u = ch.len_utf16();
            if piece_units + u > max && !piece.is_empty() {
                out.push(std::mem::take(&mut piece));
                piece_units = 0;
            }
            piece.push(ch);
            piece_units += u;
        }
        cur = piece;
        cur_units = piece_units;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn plain_outbound(raw: &str, max: usize) -> Vec<crate::hostbot::platform::Outbound> {
    let shown = crate::ui::markdown::render_plain_blocks(raw);
    chunk_text(&shown, max)
        .into_iter()
        .map(crate::hostbot::platform::Outbound::plain)
        .collect()
}

/// Hard cap on messages retained in one serve session, so a long conversation can't grow without
/// bound. Generous — the mid-loop context guard handles within-turn pressure.
const SERVE_SESSION_MAX_MSGS: usize = 40;

/// Bound a serve session's history: drop the OLDEST whole turns (keeping the system prompt at [0])
/// until under `max`, always cutting at a `user` boundary so an assistant tool-call turn is never
/// split from its tool results (a dangling tool_call ⇒ a 400 on strict gateways).
fn cap_session(history: &mut Vec<Message>, max: usize) {
    let lead = crate::agent::compact::leading_system_count(history).max(1);
    while history.len() > max {
        let second_user = history
            .iter()
            .enumerate()
            .filter(|(i, m)| *i >= lead && m.role == "user")
            .nth(1)
            .map(|(i, _)| i);
        match second_user {
            Some(i) if i > lead => {
                history.drain(lead..i);
            }
            _ => break,
        }
    }
}

/// Run one serve turn over a PERSISTENT per-chat history, so follow-ups like "now fix it" keep
/// context. Seeds the system prompt (with memory + SOUL + persona) once per session, appends the
/// user task, drives the loop, learns passively, and bounds the history. A `clarify` yield leaves a
/// resumable history (the owner's next message is the answer).
/// Everything one lane contributes to a turn. Bundled into a struct because it is five values that
/// always travel together, and threading them as positional arguments through `run_serve_turn` made
/// it far too easy to pass another lane's by mistake.
struct LaneCtx {
    /// Sub-bot name — also the session key and the approval route.
    route: String,
    /// Chat id, `Display`ed (platform-agnostic: Telegram `i64`, Discord `u64`).
    chat: String,
    /// This lane's working directory.
    root: std::path::PathBuf,
    /// This lane's persona (`None` ⇒ the global one).
    persona: Option<String>,
    /// This lane's `reasoning_effort`, resolved (see `lane_effort`).
    #[allow(clippy::option_option)]
    effort: Option<Option<String>>,
}

#[allow(clippy::too_many_arguments)]
async fn run_serve_turn(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    history: &mut Vec<Message>,
    task: &str,
    approval_mode: ApprovalMode,
    platform_name: &str,
    lane: &LaneCtx,
    shared: &lane::LaneShared,
) -> Result<String> {
    let lead = crate::agent::compact::leading_system_count(history);
    // A brand-new or legacy-restored thread is a conversation boundary and may adopt pending
    // memory. Ordinary messages in an established thread must keep the active core byte-stable.
    // The bundle states THIS lane's cwd and wears THIS lane's persona.
    let mut bundle =
        crate::hostbot_prompt_bundle(model, &lane.root, lane.persona.clone(), lead < 2);
    bundle.dynamic.push_str(&format!(
        "\n<hostbot_surface platform=\"{platform_name}\">\n\
         Format the final answer for a narrow mobile chat. Lead with the result; keep headings and\n\
         paragraphs short; prefer bullets or compact key: value lists. Use tables only for genuinely\n\
         multi-column data. Avoid wide diagrams, box drawing, decorative emoji, and repeated summaries.\n\
         Keep Markdown code fences and URLs intact.\n\
         </hostbot_surface>\n"
    ));
    if lead < 2 {
        // Persisted legacy sessions had one flattened system prompt. Replace that stale prefix with
        // current lanes; never treat old dynamic identity/memory bytes as the stable cache lane.
        let tail = history.get(lead..).unwrap_or_default().to_vec();
        history.clear();
        history.push(Message::system(bundle.stable.clone()));
        if !bundle.dynamic.trim().is_empty() {
            history.push(Message::system(bundle.dynamic.clone()));
        }
        history.extend(tail);
    } else {
        // Fresh platform message: stable lane stays byte-identical; only dynamic lane refreshes.
        if bundle.dynamic.trim().is_empty() {
            history.remove(1);
        } else {
            history[1] = Message::system(bundle.dynamic.clone());
        }
    }
    history.push(Message::user(task.to_string()));

    // Default-ON lazy LSP (same as REPL/agent CLI) so remote coding turns get symbol tools.
    crate::agent::lsp::LSP.set_request_timeout(AgentConfig::default().lsp_request_timeout_secs);
    let _ = crate::agent::lsp::LSP.enable();
    let registry = agent::builtin::default_registry_with_task(
        http.clone(),
        base_url.to_string(),
        api_key.to_string(),
        model.to_string(),
        approval_mode,
        crate::resolve_ctx_window(model).0,
        Some(lane.root.clone()), // this bot's directory, not the process cwd
    )?;
    let turn_cancel = crate::core::cancel::TurnCancel::new();
    // Identity + approval routing ride on the context so a tool body — and the approval gate on the
    // driver — read THIS lane's values rather than whichever lane wrote a global last. The workspace
    // root and the effort tier have direct owners (`AgentConfig` and the chat closure below), so they
    // are not duplicated here.
    let exec_ctx =
        crate::core::exec_ctx::ExecutionContext::new(crate::core::convo::ConversationId::new(
            format!("{platform_name}:{}:{}", lane.route, lane.chat),
        ))
        .with_persona(lane.persona.clone())
        .with_approval_route(Some(crate::core::exec_ctx::ApprovalRoute {
            route: lane.route.clone(),
            chat: lane.chat.clone(),
        }));
    let cfg = AgentConfig {
        approval_mode,
        cancel: turn_cancel,
        exec_ctx,
        workspace_root: Some(lane.root.clone()),
        quiet: true,
        enable_verify_gate: false,
        context_window: crate::resolve_ctx_window(model).0,
        enable_lsp: crate::agent::lsp::LSP.is_enabled(),
        ..Default::default()
    };
    let http_ref = http;
    let base = base_url;
    let key = api_key;
    let model_ref = model;
    // Effort is passed EXPLICITLY, not read from the process-global override: two lanes may want
    // different tiers, and the global would stamp whichever armed it last onto both.
    let lane_effort = lane.effort.clone();
    let chat = move |msgs: Vec<Message>, defs: Vec<crate::core::types::ToolDef>| {
        let effort = match lane_effort.clone() {
            Some(inner) => inner,
            None => crate::core::cli_config::resolved_reasoning_effort(
                crate::core::cli_config::load().reasoning_effort,
            ),
        };
        async move {
            client::chat_with_tools_effort(http_ref, base, key, model_ref, &msgs, &defs, effort)
                .await
        }
    };
    let sum_ep = crate::summarizer_endpoint(base, key, model_ref);
    let summarize = move |msgs: Vec<Message>| {
        let ep = sum_ep.clone();
        async move {
            client::chat_with_tools(http_ref, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[])
                .await
                .map(|t| t.content.unwrap_or_default())
        }
    };
    let outcome =
        agent::run_agent_loop_compacting(chat, summarize, &cfg, &registry, history).await?;

    // The passive learner writes ONE global memory store, so lanes take turns rather than racing to
    // rewrite the same files. Short and off the critical path — the turn's answer is already formed.
    {
        let _learning = shared.learn.lock().await;
        crate::maybe_learn_memory(history);
    }
    cap_session(history, SERVE_SESSION_MAX_MSGS);

    if let StopReason::AwaitingInput(q) = &outcome.stop {
        return Ok(format!("❓ {q}"));
    }
    Ok(outcome
        .final_text
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "(the agent produced no answer)".to_string()))
}

// ── the generic daemon loop ──────────────────────────────────────────────────────────

/// One lane's worker: owns THIS conversation's history and answers its messages in order.
///
/// Everything that makes a lane independent lives here — its own history vector (never shared with
/// another lane), its own `/command` dispatch, and its own turn. Concurrency limits are applied
/// around the turn itself by [`lane::with_turn_guards`], not around the whole worker, so a lane
/// waiting on a permit still accepts and queues messages.
#[allow(clippy::too_many_arguments)]
async fn lane_worker<P: Platform>(
    platform: Arc<P>,
    shared: Arc<lane::LaneShared>,
    registry: Weak<lane::LaneRegistry<P>>,
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    default_model: String,
    route: String,
    chat: P::Chat,
    tx: Sender<Inbound<P::Chat>>,
    mut rx: mpsc::Receiver<String>,
    mut history: Vec<Message>,
) {
    while let Some(text) = rx.recv().await {
        let trimmed = text.trim().to_string();

        // A `/command` → the dispatcher. `Some(reply)` = handled; `None` = fall through to the agent.
        //
        // A leading `/` is NOT sufficient: an XPath, a POSIX path, or prose that merely starts with
        // one (`/help... abcd`) is a message, and used to be eaten here. We apply only the SHAPE
        // gate from `slash::classify`, not the whole verdict — this surface has its own vocabulary
        // (`/sh`, `/cd`, `/pwd`, `/bots`, …) that isn't in the REPL catalog, and `handle_command`'s
        // catch-all deliberately runs an unrecognized name as a SHELL command.
        let slash_name = trimmed.strip_prefix('/').map(|rest| {
            let mut parts = rest.splitn(2, char::is_whitespace);
            (
                parts.next().unwrap_or("").trim().to_string(),
                parts.next().unwrap_or("").trim().to_string(),
            )
        });
        if let Some((name, arg)) =
            slash_name.filter(|(n, _)| crate::features::slash::looks_like_name(n))
        {
            if let Some(reply) = handle_command(
                &*platform,
                &name,
                &arg,
                &mut history,
                &route,
                chat,
                &default_model,
                &registry,
                &tx,
            )
            .await
            {
                send_reply(&*platform, &route, chat, &reply).await;
                continue;
            }
        }

        // `/agent <task>` is explicitly autonomous; a plain message follows THIS BOT's approval tier
        // (its own `/approval`, else the machine default) — not a global one every bot shares.
        let (task, approval) = match trimmed.strip_prefix("/agent ") {
            Some(rest) => (rest.trim().to_string(), ApprovalMode::Yolo),
            None => (trimmed.clone(), lane_approval(&route)),
        };
        if task.is_empty() {
            continue;
        }

        let status = platform.start_status(&route, chat).await.ok().flatten();
        // Everything this bot contributes to the turn, resolved once per message so a `/cd` or
        // `/model` between turns takes effect on the next one.
        let ctx = LaneCtx {
            route: route.clone(),
            chat: chat.to_string(),
            root: lane_cwd(&route),
            persona: platform.persona_for(&route),
            effort: lane_effort(&route),
        };
        let model = lane_model(&route, &default_model);

        // Run the turn under the concurrency permit AND this root's gate. Lanes on different roots
        // overlap; lanes sharing a directory take turns (see `lane::RootGate` for why that is
        // mandatory rather than polite).
        let turn = lane::with_turn_guards(&shared, &ctx.root, async {
            // The approval-route global still exists for pre-context callers; the authoritative
            // copy rides on the turn's `ExecutionContext`, built inside `run_serve_turn`.
            if platform.supports_approval() {
                platform.set_approval_route(&route, chat);
            }
            let out = run_serve_turn(
                &http,
                &base_url,
                &api_key,
                &model,
                &mut history,
                &task,
                approval,
                platform.name(),
                &ctx,
                &shared,
            )
            .await;
            if platform.supports_approval() {
                platform.clear_approval_route();
            }
            out
        })
        .await;

        let failed = turn.is_err();
        let reply = turn.unwrap_or_else(|e| format!("error: {e}"));
        // Persist the updated history so a restart keeps this chat's context.
        let _ = store::save_session(platform.name(), &route, &chat.to_string(), &history);
        let send_failed = !send_reply(&*platform, &route, chat, &reply).await;
        let _ = platform
            .finish_status(&route, chat, status, failed || send_failed)
            .await;
    }
}

/// Render and deliver one reply. Returns false if a piece failed to send.
async fn send_reply<P: Platform>(platform: &P, route: &str, chat: P::Chat, reply: &str) -> bool {
    let rendered = platform.render_reply(reply);
    let outbound = if rendered.is_empty() {
        plain_outbound(reply, platform.message_max())
    } else {
        rendered
    };
    for piece in outbound {
        if let Err(e) = platform.send_outbound(route, chat, &piece).await {
            eprintln!("[{}] reply send failed: {e}", platform.name());
            return false;
        }
    }
    true
}

/// Drive one `Platform` to completion.
///
/// This loop is now only a ROUTER: it spawns the platform's listeners and hands each inbound message
/// to its `(bot, chat)` lane, which answers it independently. Previously this same loop ran the turn
/// inline, so one long build in one chat blocked every bot the daemon hosted.
async fn run_daemon<P: Platform>(platform: P) -> Result<()> {
    let platform = std::sync::Arc::new(platform);
    let (base_url, api_key, model) = crate::resolve_endpoint(None, None, None)
        .context("configure the model endpoint first (run `aizen config`)")?;
    let http = crate::http_client()?;

    let (tx, mut rx) = mpsc::channel::<Inbound<P::Chat>>(64);
    platform.start(tx.clone()).await?;

    // Restore persisted conversations for THIS platform (parse the chat id back via `Chat::from_str`).
    // Lanes start lazily on their first message, so this is a map of seed histories, not live tasks —
    // a daemon hosting many idle chats costs nothing until they speak.
    let gc = store::gc_sessions();
    if gc > 0 {
        eprintln!(
            "[{}] dropped {gc} session(s) past the retention window",
            platform.name()
        );
    }
    let mut seeds: HashMap<(String, P::Chat), Vec<Message>> = HashMap::new();
    for (route, chat_s, msgs) in store::load_sessions(platform.name()) {
        if let Ok(chat) = chat_s.parse::<P::Chat>() {
            seeds.insert((route, chat), msgs);
        }
    }
    let seeds = Arc::new(Mutex::new(seeds));

    let lanes = lane::LaneRegistry::<P>::new();
    eprintln!(
        "{}",
        style(format!(
            "aizen serve — {} live, up to {} turn(s) at once (Ctrl-C to stop)",
            platform.name(),
            lane::max_concurrent()
        ))
        .dim()
    );

    // Liveness for container/orchestrator probes (`aizen serve --health`). Stamped from INSIDE this
    // loop — a detached ticker would keep beating while the real loop was wedged, which is the one
    // failure a liveness probe exists to catch. The state now reflects the LANES: `busy` while any
    // turn is running, so a probe judges a long build by the busy deadline, not the idle one.
    health::beat(platform.name(), health::State::Idle);
    let mut beat_tick =
        tokio::time::interval(std::time::Duration::from_secs(health::BEAT_INTERVAL_SECS));
    beat_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    beat_tick.tick().await; // the first tick resolves immediately — consume it

    loop {
        let inbound = tokio::select! {
            biased;
            // Ctrl-C (a terminal) OR SIGTERM (systemd `stop`, `docker stop`, a k8s pod delete). Without
            // the SIGTERM arm an orchestrator's graceful stop killed us outright: no `kill_all`, so
            // spawned builds/language servers were orphaned, and no `clear()`, so the heartbeat stayed
            // behind and read as a live daemon.
            sig = shutdown_signal() => {
                eprintln!("\nshutting down… ({sig})");
                lanes.stop_all();
                crate::agent::process::kill_all();
                break;
            }
            _ = beat_tick.tick() => {
                // `busy` while ANY lane is mid-turn, so a long build is judged by the busy deadline
                // rather than the tight idle one — and the counts make a slow daemon diagnosable.
                let busy = lanes.shared.stats.busy();
                let state = if busy > 0 {
                    health::State::Busy
                } else {
                    health::State::Idle
                };
                health::beat_lanes(platform.name(), state, lanes.len(), busy);
                continue;
            }
            m = rx.recv() => match m { Some(m) => m, None => break },
        };
        let Inbound { route, chat, text } = inbound;

        // Hand off and go straight back to receiving. This return-immediately step is what makes the
        // daemon responsive: the turn happens on the lane's own task.
        let seed = seeds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(route.clone(), chat))
            .unwrap_or_default();
        let spawn_platform = platform.clone();
        let spawn_shared = lanes.shared.clone();
        let spawn_http = http.clone();
        let spawn_base = base_url.clone();
        let spawn_key = api_key.clone();
        let spawn_model = model.clone();
        let spawn_route = route.clone();
        let spawn_tx = tx.clone();
        lanes
            .dispatch(
                lane::LaneSpawn {
                    route: route.clone(),
                    chat,
                    spawn: Box::new(move |rx, registry| {
                        tokio::spawn(lane_worker(
                            spawn_platform,
                            spawn_shared,
                            registry,
                            spawn_http,
                            spawn_base,
                            spawn_key,
                            spawn_model,
                            spawn_route,
                            chat,
                            spawn_tx,
                            rx,
                            seed,
                        ))
                    }),
                },
                text,
            )
            .await;
    }

    platform.shutdown();
    // A stopped daemon must read as "not running", not as a stale live one — otherwise the next
    // probe judges a heartbeat nobody is refreshing and the verdict depends on which deadline the
    // last state happened to leave behind.
    health::clear();
    Ok(())
}

/// Dispatch a `/command`. Returns `Some(reply)` when we own it, or `None` to fall through to the
/// agent path (`/agent`, or an unknown `/foo`).
///
/// Runs INSIDE the lane, so `history` is this conversation's own and a `/new` here can never clear
/// another chat's context. Config toggles persist per lane to `hostbot/lanes.json`, not to the shared
/// `cli-config.json`. Multi-bot arms (`/bots /addbot /rmbot`) delegate to the platform and are gated
/// on `supports_multibot` (Discord replies "Telegram only").
#[allow(clippy::too_many_arguments)]
async fn handle_command<P: Platform>(
    platform: &P,
    name: &str,
    arg: &str,
    history: &mut Vec<Message>,
    route: &str,
    chat: P::Chat,
    default_model: &str,
    registry: &Weak<lane::LaneRegistry<P>>,
    tx: &Sender<Inbound<P::Chat>>,
) -> Option<String> {
    use crate::core::cli_config;
    let model = default_model;
    match name {
        "help" | "start" => Some(serve_help_text()),
        "new" | "reset" => {
            history.clear();
            store::drop_session(platform.name(), route, &chat.to_string());
            // Free this chat's browser session too (its page/@refs are part of the dropped context).
            #[cfg(feature = "browser")]
            crate::agent::browser::release(&crate::core::convo::ConversationId::new(format!(
                "{}:{}:{}",
                platform.name(),
                route,
                chat
            )));
            Some("🆕 started a fresh conversation — earlier context dropped.".to_string())
        }
        "resume" => {
            let turns = history.iter().filter(|m| m.role == "user").count();
            Some(if turns == 0 {
                "🧵 no active conversation — just send a message to start one.".to_string()
            } else {
                format!("🧵 continuing — {turns} message(s) of context kept. /new to start over.")
            })
        }
        // `/sh <cmd>` — run NOW (like the REPL `!cmd` escape). The hard safety floor still refuses
        // catastrophic commands; everything else runs without a per-op approval prompt.
        "sh" | "shell" | "run" => {
            let cmd = arg.trim();
            if cmd.is_empty() {
                return Some("usage: /sh <command>".to_string());
            }
            match crate::agent::cmd_guard::classify(cmd) {
                crate::agent::cmd_guard::Verdict::Blocked(reason) => {
                    Some(format!("✗ blocked by the safety floor: {reason}"))
                }
                _ => {
                    let c = cmd.to_string();
                    // In THIS bot's directory — several bots share the process, so the shell must
                    // run where this bot was told to work.
                    let dir = lane_cwd(route);
                    let out = tokio::task::spawn_blocking(move || {
                        crate::run_shell_escape_in(&c, Some(&dir))
                    })
                    .await
                    .unwrap_or_else(|e| format!("[task failed: {e}]"));
                    Some(format!("$ {cmd}\n{out}"))
                }
            }
        }
        // `/cd <path>` + `/pwd` — PER LANE. This deliberately does NOT call `set_current_dir`: the
        // daemon hosts several bots at once, and a process-wide cwd meant one bot's `/cd` silently
        // moved every other bot's edits (and made two lanes take the writer lease on one repo).
        // The path is stored on the lane and handed to the agent as its workspace root.
        "cd" => {
            let path = arg.trim();
            if path.is_empty() {
                return Some("usage: /cd <path>".to_string());
            }
            let p = std::path::Path::new(path);
            if !p.exists() {
                return Some(format!("✗ no such path: {path}"));
            }
            if !p.is_dir() {
                return Some(format!("✗ not a directory: {path}"));
            }
            // Canonicalize once, here: the lane root keys the workspace lease, and two spellings of
            // one directory must not read as two different repos.
            let resolved = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            match store::update_lane(route, |l| {
                l.cwd = Some(resolved.clone());
                Ok(())
            }) {
                Ok(()) => Some(format!("📁 cwd → {} (this bot only)", resolved.display())),
                Err(e) => Some(format!("✗ cd failed: {e}")),
            }
        }
        "pwd" => Some(format!("📁 {}", lane_cwd(route).display())),
        "approval" => {
            let requested = arg.split_whitespace().next().unwrap_or("status");
            if requested.is_empty() || matches!(requested, "status" | "st") {
                let lane = store::load_lane(route).approval;
                let source = match &lane {
                    Some(_) => "this bot",
                    None => "machine default",
                };
                return Some(format!(
                    "approval: {} ({source}) · ask=prompt · smart=read-only auto · yolo=pre-authorized",
                    lane_approval(route)
                ));
            }
            let mode = match requested.parse::<ApprovalMode>() {
                Ok(mode) => mode,
                Err(e) => return Some(format!("usage: /approval ask|smart|yolo ({e})")),
            };
            match store::update_lane(route, |l| {
                l.approval = Some(mode.to_string());
                Ok(())
            }) {
                Ok(()) => Some(format!("approval → {mode} (this bot only)")),
                Err(e) => Some(format!("approval: {e}")),
            }
        }
        "yolo" | "auto" | "yes" => {
            let mode = if lane_approval(route) == ApprovalMode::Yolo {
                ApprovalMode::Ask
            } else {
                ApprovalMode::Yolo
            };
            match store::update_lane(route, |l| {
                l.approval = Some(mode.to_string());
                Ok(())
            }) {
                Ok(()) => Some(format!(
                    "approval → {mode} (this bot only, legacy /yolo alias)"
                )),
                Err(e) => Some(format!("approval: {e}")),
            }
        }
        "smart" => {
            let mode = if lane_approval(route) == ApprovalMode::Smart {
                ApprovalMode::Ask
            } else {
                ApprovalMode::Smart
            };
            match store::update_lane(route, |l| {
                l.approval = Some(mode.to_string());
                Ok(())
            }) {
                Ok(()) => Some(format!(
                    "approval → {mode} (this bot only, legacy /smart alias)"
                )),
                Err(e) => Some(format!("approval: {e}")),
            }
        }
        "ultimate" | "ultra" => {
            let lane = store::load_lane(route);
            let now = !lane.ultimate.unwrap_or(false);
            match store::update_lane(route, |l| {
                l.ultimate = Some(now);
                l.effort = if now { Some("max".into()) } else { None };
                Ok(())
            }) {
                Ok(()) if now => Some("✦ ultimate ON for this bot — max reasoning effort every turn + prefers launching workflows. /ultimate again to turn it off.".to_string()),
                Ok(()) => Some("ultimate OFF for this bot — effort back to the machine default.".to_string()),
                Err(e) => Some(format!("ultimate: {e}")),
            }
        }
        "effort" => {
            let sub = arg.trim().to_ascii_lowercase();
            match sub.as_str() {
                "" | "status" | "st" => {
                    let lane = store::load_lane(route);
                    Some(match lane.effort.as_deref() {
                        None => format!(
                            "effort: machine default ({})",
                            cli_config::load()
                                .reasoning_effort
                                .unwrap_or_else(|| "auto".into())
                        ),
                        Some("off") | Some("none") => {
                            "effort: off for this bot (no reasoning_effort sent)".to_string()
                        }
                        Some(tier) => format!("effort: {tier} (this bot only)"),
                    })
                }
                "auto" | "on" | "clear" => match store::update_lane(route, |l| {
                    l.effort = None;
                    l.ultimate = None;
                    Ok(())
                }) {
                    Ok(()) => Some(
                        "effort cleared for this bot — back to the machine default.".to_string(),
                    ),
                    Err(e) => Some(format!("effort: {e}")),
                },
                "off" | "none" => match store::update_lane(route, |l| {
                    l.effort = Some("off".into());
                    Ok(())
                }) {
                    Ok(()) => {
                        Some("effort off for this bot — no reasoning_effort sent.".to_string())
                    }
                    Err(e) => Some(format!("effort: {e}")),
                },
                "low" | "medium" | "high" | "xhigh" | "max" => {
                    match store::update_lane(route, |l| {
                        l.effort = Some(sub.clone());
                        Ok(())
                    }) {
                        Ok(()) => Some(format!("effort pinned to {sub} (this bot only).")),
                        Err(e) => Some(format!("effort: {e}")),
                    }
                }
                other => Some(format!(
                    "usage: /effort auto|off|low|medium|high|xhigh|max|clear  (unknown '{other}')"
                )),
            }
        }
        "model" => {
            let want = arg.trim();
            if want.is_empty() {
                return Some(format!("model: {}", lane_model(route, model)));
            }
            // Per lane: switching the model in one bot must not switch it in every other bot, nor
            // in the owner's REPL — which is what writing `cli-config.json` from here used to do.
            let saved = store::update_lane(route, |l| {
                l.model = Some(want.to_string());
                Ok(())
            });
            history.clear(); // next turn rebuilds the system prompt with the new model/ctx-window
            store::drop_session(platform.name(), route, &chat.to_string());
            match saved {
                Ok(()) => Some(format!(
                    "🔄 model → {want} for this bot (context reset for this chat)"
                )),
                Err(e) => Some(format!("model: {e}")),
            }
        }
        "memory" | "mem" => {
            if let Some(rest) = arg
                .strip_prefix("remember")
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(match crate::memory::remember(rest) {
                    Ok(id) => format!("🧠 remembered ({id})"),
                    Err(e) => format!("memory: {e}"),
                })
            } else if arg.trim().is_empty() {
                Some("usage: /memory <query>  ·  /memory remember <fact>".to_string())
            } else {
                Some(
                    match crate::memory::search_scoped(
                        arg.trim(),
                        5,
                        &crate::memory::ScopeSel::default_view(),
                    ) {
                        Ok(hits) if hits.is_empty() => {
                            format!("(no memory matches '{}')", arg.trim())
                        }
                        Ok(hits) => {
                            let mut s = String::new();
                            for h in &hits {
                                let body: String = h.entry.body.chars().take(160).collect();
                                s.push_str(&format!(
                                    "• {} — {}\n",
                                    h.entry.name,
                                    body.replace('\n', " ")
                                ));
                            }
                            s.trim_end().to_string()
                        }
                        Err(e) => format!("memory: {e}"),
                    },
                )
            }
        }
        "tools" | "toolsets" => Some(crate::agent::toolsets::format_config_status()),
        "status" => {
            // Everything here is THIS bot's view: its own cwd, model, approval tier and effort,
            // with the machine default shown only where the lane has no opinion.
            let lane = store::load_lane(route);
            let cwd = lane_cwd(route).display().to_string();
            let approval = lane_approval(route);
            let effort = lane.effort.clone().unwrap_or_else(|| {
                cli_config::load()
                    .reasoning_effort
                    .unwrap_or_else(|| "auto".into())
            });
            let ultimate = if lane.ultimate.unwrap_or(false) {
                "on"
            } else {
                "off"
            };
            let model = lane_model(route, model);
            let turns = history.iter().filter(|m| m.role == "user").count();
            let lanes = registry
                .upgrade()
                .map(|r| {
                    format!(
                        "\nlanes:    {} live · {} busy",
                        r.len(),
                        r.shared.stats.busy()
                    )
                })
                .unwrap_or_default();
            Some(format!(
                "📊 aizen serve\nplatform: {}\nbot:      {route}\ncwd:      {cwd}\nmodel:    {model}\napproval: {approval}\neffort:   {effort} · ultimate {ultimate}\ncontext:  {turns} message(s) this chat{lanes}",
                platform.name()
            ))
        }
        // ── multi-bot management: hot add/remove extra bots (Telegram only) ──
        "bots" => {
            let mut bots = platform.list_bots();
            bots.sort_by(|a, b| a.name.cmp(&b.name));
            if bots.is_empty() {
                return Some(format!("🤖 {} — no hosted bots to list.", platform.name()));
            }
            let mut s = format!("🤖 {} bot(s) hosted:\n", bots.len());
            for b in bots {
                let tag = if b.name == "default" {
                    " (primary)"
                } else {
                    ""
                };
                // A health note (e.g. a 409 fight with another machine) is shown inline — the whole
                // point of tracking it is that the owner sees it without reading the host's log.
                let note = b
                    .note
                    .as_deref()
                    .map(|n| format!(" · ⚠ {n}"))
                    .unwrap_or_default();
                s.push_str(&format!(
                    "• {}{tag} — @{} · {} chat(s){note}\n",
                    b.name, b.username, b.chats
                ));
            }
            if platform.supports_multibot() {
                s.push_str("\n/addbot <name> <token> to add · /rmbot <name> to remove");
            }
            Some(s.trim_end().to_string())
        }
        "addbot" => {
            if !platform.supports_multibot() {
                return Some("✗ hosting extra bots is only supported on Telegram.".to_string());
            }
            // `--own` anywhere in the argument gives the new bot its OWN owner (it pairs with a
            // code) instead of inheriting yours — for a bot you're handing to somebody else.
            let own_owner = arg.split_whitespace().any(|w| w == "--own");
            let rest: Vec<&str> = arg.split_whitespace().filter(|w| *w != "--own").collect();
            let bot_name = rest.first().copied().unwrap_or("").to_string();
            let token = rest.get(1).copied().unwrap_or("").to_string();
            if bot_name.is_empty() || token.is_empty() {
                return Some("usage: /addbot <name> <token> [--own]".to_string());
            }
            match platform.add_bot(&bot_name, &token, own_owner, tx).await {
                Ok(username) if own_owner => Some(format!(
                    "✓ bot @{username} live as \"{bot_name}\" — give its owner the pairing code above; \
                     your own chats are NOT on it."
                )),
                Ok(username) => Some(format!("✓ bot @{username} live as \"{bot_name}\" — open it in Telegram and send /start.")),
                Err(e) => Some(format!("✗ {e}")),
            }
        }
        "rmbot" => {
            if !platform.supports_multibot() {
                return Some("✗ hosting extra bots is only supported on Telegram.".to_string());
            }
            let bot_name = arg.trim();
            if bot_name.is_empty() {
                return Some("usage: /rmbot <name>".to_string());
            }
            match platform.remove_bot(bot_name).await {
                Ok(()) => {
                    // Stop that bot's lanes too — their tasks would otherwise keep running with a
                    // bot that no longer exists, and a reply would have nowhere to go.
                    if let Some(r) = registry.upgrade() {
                        r.stop_route(bot_name);
                    }
                    store::drop_lane(bot_name); // its cwd/model/approval go with it
                    Some(format!("✓ stopped hosting \"{bot_name}\"."))
                }
                Err(e) => Some(format!("✗ {e}")),
            }
        }
        // `/agent` (autonomous run) and any unknown `/foo` → fall through to the agent path.
        _ => None,
    }
}

// ── platform entry points ──────────────────────────────────────────────────────────

/// `aizen serve` — host the Telegram platform (primary bot + any `/addbot`-hosted extras).
/// `wanted` restricts which extras THIS machine runs (`--bots a,b`); empty ⇒ all it is allowed to.
pub async fn run_serve(wanted: Vec<String>) -> Result<()> {
    let platform =
        platforms::telegram::TelegramPlatform::from_config_selecting(command_menu(), wanted)?;
    run_daemon(platform).await
}

/// `aizen discord serve` — host the Discord platform (one gateway, full command menu, no inline
/// approval so destructive ops are skipped).
pub async fn run_discord_serve() -> Result<()> {
    let platform = platforms::discord::DiscordPlatform::from_config()?;
    run_daemon(platform).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin `AIZEN_HOME` to a fresh tempdir so lane settings read/write there. Serialized on the
    /// crate-wide `TEST_HOME_LOCK`, like every other HOME-mutating test.
    fn with_temp_home<T>(tag: &str, f: impl FnOnce() -> T) -> T {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir =
            std::env::temp_dir().join(format!("aizen-daemon-lane-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("AIZEN_HOME", &dir);
        let out = f();
        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    // ── per-lane settings ────────────────────────────────────────────────────
    #[test]
    fn lane_cwd_is_per_bot_and_never_moves_the_process() {
        // THE bug this fixes: `/cd` used to call `set_current_dir`, so one bot's directory change
        // silently relocated every other bot's edits. Two lanes must resolve to two directories,
        // and the process cwd must be exactly where it started.
        with_temp_home("cwd", || {
            let before = std::env::current_dir().unwrap();
            let a = std::env::temp_dir().canonicalize().unwrap();
            let b = before.canonicalize().unwrap();
            store::update_lane("default", |l| {
                l.cwd = Some(a.clone());
                Ok(())
            })
            .unwrap();
            store::update_lane("work", |l| {
                l.cwd = Some(b.clone());
                Ok(())
            })
            .unwrap();

            assert_eq!(lane_cwd("default"), a);
            assert_eq!(lane_cwd("work"), b);
            assert_ne!(lane_cwd("default"), lane_cwd("work"));
            assert_eq!(
                std::env::current_dir().unwrap(),
                before,
                "storing a lane cwd must NOT move the process"
            );
        });
    }

    #[test]
    fn a_lane_with_no_cwd_uses_the_process_directory() {
        with_temp_home("cwd-default", || {
            assert_eq!(
                lane_cwd("never-set"),
                std::env::current_dir().unwrap(),
                "unset lanes behave exactly as before this feature"
            );
        });
    }

    #[test]
    fn a_deleted_lane_directory_falls_back_instead_of_failing_every_turn() {
        // A pinned directory can be removed while the daemon runs. Falling back beats answering
        // every later message with a path error the owner can only fix by editing JSON.
        with_temp_home("cwd-gone", || {
            let gone = std::env::temp_dir().join("aizen-lane-gone-does-not-exist");
            let _ = std::fs::remove_dir_all(&gone);
            store::update_lane("ghost", |l| {
                l.cwd = Some(gone.clone());
                Ok(())
            })
            .unwrap();
            assert_eq!(lane_cwd("ghost"), std::env::current_dir().unwrap());
        });
    }

    #[test]
    fn lane_model_and_approval_are_per_bot() {
        with_temp_home("model", || {
            store::update_lane("work", |l| {
                l.model = Some("model-w".into());
                l.approval = Some("yolo".into());
                Ok(())
            })
            .unwrap();
            assert_eq!(lane_model("work", "fallback"), "model-w");
            assert_eq!(
                lane_model("default", "fallback"),
                "fallback",
                "a model set on one bot must not leak to another"
            );
            assert_eq!(lane_approval("work"), ApprovalMode::Yolo);
            assert_ne!(
                lane_approval("default"),
                ApprovalMode::Yolo,
                "yolo on one bot must not pre-authorize destructive ops on another"
            );
        });
    }

    #[test]
    fn lane_effort_distinguishes_unset_from_deliberately_off() {
        // `None` = no lane opinion (use the machine config) vs `Some(None)` = this bot sends NO
        // reasoning_effort. Collapsing them would silently change the wire format for one bot.
        with_temp_home("effort", || {
            assert_eq!(lane_effort("fresh"), None, "unset ⇒ inherit");
            store::update_lane("off-bot", |l| {
                l.effort = Some("off".into());
                Ok(())
            })
            .unwrap();
            assert_eq!(lane_effort("off-bot"), Some(None), "off ⇒ omit the field");
            store::update_lane("max-bot", |l| {
                l.effort = Some("max".into());
                Ok(())
            })
            .unwrap();
            assert_eq!(lane_effort("max-bot"), Some(Some("max".to_string())));
        });
    }

    #[test]
    fn serve_commands_are_telegram_legal() {
        // Telegram requires command names 1–32 chars of [a-z0-9_]; the "/" menu registration fails
        // otherwise. Guard the whole table so a new command can't silently break the menu.
        assert!(!SERVE_COMMANDS.is_empty());
        for (name, _hint, desc) in SERVE_COMMANDS {
            assert!(
                (1..=32).contains(&name.len()),
                "command '{name}' length out of range"
            );
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "command '{name}' has an illegal char"
            );
            assert!(!desc.is_empty(), "command '{name}' needs a description");
        }
        assert!(
            SERVE_COMMANDS.iter().any(|(n, _, _)| *n == "addbot"),
            "/addbot must be in the menu"
        );
        assert!(
            SERVE_COMMANDS.iter().any(|(n, _, _)| *n == "rmbot"),
            "/rmbot must be in the menu"
        );
        assert!(
            SERVE_COMMANDS.iter().any(|(n, _, _)| *n == "bots"),
            "/bots must be in the menu"
        );
    }

    #[test]
    fn help_text_lists_every_command() {
        let help = serve_help_text();
        for (name, _hint, _desc) in SERVE_COMMANDS {
            assert!(help.contains(&format!("/{name}")), "help missing /{name}");
        }
    }

    #[test]
    fn chunk_text_splits_on_utf16_units_not_scalars() {
        // 'A' = 1 UTF-16 unit; '𝐀' (math bold) = 2. A cap-3 chunk fits 3 A's or 1 math-bold + fill.
        let s = "𝐀".repeat(3000);
        let chunks = chunk_text(&s, 3500);
        for c in &chunks {
            assert!(
                c.encode_utf16().count() <= 3500,
                "chunk exceeds the UTF-16 cap"
            );
        }
        assert!(
            chunks.len() > 1,
            "3000 astral chars = 6000 UTF-16 units must split"
        );
    }

    #[test]
    fn chunk_text_keeps_short_ascii_whole() {
        assert_eq!(chunk_text("hello", 3500), vec!["hello".to_string()]);
    }

    #[test]
    fn chunk_text_prefers_complete_lines() {
        let s = "first row\nsecond row\nthird row\n";
        let chunks = chunk_text(s, 22);
        assert_eq!(chunks.concat(), s);
        assert!(chunks.iter().all(|c| c.encode_utf16().count() <= 22));
        assert!(
            chunks[..chunks.len() - 1].iter().all(|c| c.ends_with('\n')),
            "{chunks:?}"
        );
    }

    #[test]
    fn outbound_reply_stacks_tables_but_keeps_diagrams() {
        let raw = "| File | State |\n|---|---|\n| a.rs | done |\n\n```diagram\nA --> B\n```";
        let shown = plain_outbound(raw, 3500);
        let shown = shown
            .iter()
            .map(|o| o.text.as_str())
            .collect::<Vec<_>>()
            .join("");
        assert!(shown.contains("File: a.rs\nState: done"), "{shown}");
        assert!(!shown.contains("|---|---|"), "{shown}");
        assert!(shown.contains("```diagram\nA --> B\n```"), "{shown}");
    }

    #[test]
    fn cap_session_drops_oldest_whole_turns_at_user_boundary() {
        let mut h = vec![Message::system("sys")];
        for i in 0..5 {
            h.push(Message::user(format!("u{i}")));
            h.push(Message::assistant(format!("a{i}")));
        }
        cap_session(&mut h, 5);
        assert!(h.len() <= 5);
        assert_eq!(h[0].role, "system", "system prompt is preserved at [0]");
        assert_eq!(h[1].role, "user", "history still starts at a user boundary");
    }

    #[test]
    fn cap_session_keeps_single_turn_even_if_over_cap() {
        let mut h = vec![
            Message::system("sys"),
            Message::user("u0"),
            Message::assistant("a0"),
        ];
        cap_session(&mut h, 2);
        assert_eq!(h.len(), 3, "a single turn is never split even if over cap");
    }
}
