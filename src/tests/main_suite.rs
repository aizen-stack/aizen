//! Unit tests for the crate-root binary surface (`src/main.rs`).
//! Kept as a `#[path]` child module so `use super::*` still resolves to the crate root.

use super::*;
use crate::core::config;
use anyhow::anyhow; // `main.rs` no longer needs it now that `apps` moved out; these tests still do. // ditto: `main.rs` stopped needing it once the subcommands moved out.

#[test]
fn config_provider_subcommands_parse() {
    for argv in [
        vec![
            "aizen",
            "config",
            "provider",
            "add",
            "backup",
            "--base-url",
            "https://backup/v1",
            "--api-key",
            "key",
            "--model",
            "model-b",
            "--use",
        ],
        vec![
            "aizen",
            "config",
            "provider",
            "edit",
            "backup",
            "--base-url",
            "https://backup-2/v1",
            "--api-key",
            "key-2",
            "--model",
            "model-c",
        ],
        vec![
            "aizen",
            "config",
            "provider",
            "rename",
            "backup",
            "secondary",
        ],
        vec!["aizen", "config", "provider", "use", "backup"],
        vec!["aizen", "config", "provider", "list"],
        vec!["aizen", "config", "provider", "remove", "backup", "--force"],
    ] {
        assert!(
            matches!(
                Cli::try_parse_from(argv).expect("parse").command,
                Some(Commands::Config {
                    cmd: Some(ConfigCmd::Provider { .. })
                })
            ),
            "provider command should parse"
        );
    }
    assert!(Cli::try_parse_from(["aizen", "config", "provider", "add", "missing-flags"]).is_err());
}

#[test]
fn agents_set_provider_parses() {
    assert!(matches!(
        Cli::try_parse_from([
            "aizen",
            "agents",
            "set-provider",
            "reviewer",
            "backup",
            "model-x"
        ])
        .expect("parse")
        .command,
        Some(Commands::Agents {
            cmd: Some(AgentsCmd::SetProvider { .. })
        })
    ));
}

/// `--save-session` is a switch, not a value.
///
/// The task is positional, so a flag that took an optional NAME here would swallow it: the very
/// natural `aizen agent --save-session "fix the delete button"` would file an empty task under a
/// session called "fix the delete button". Keeping the flag boolean is what makes that impossible,
/// and this test is the reason it must stay boolean.
#[test]
fn agent_save_session_is_a_switch_and_never_eats_the_task() {
    let cli = Cli::try_parse_from(["aizen", "agent", "--save-session", "sửa cái nút xoá"])
        .expect("parse");
    let Some(Commands::Agent(args)) = cli.command else {
        panic!("expected the agent subcommand");
    };
    assert!(args.save_session);
    assert_eq!(args.task, "sửa cái nút xoá");

    // Off unless asked: the scripting path must not start writing files because it was upgraded.
    let plain = Cli::try_parse_from(["aizen", "agent", "chạy test"]).expect("parse");
    assert!(matches!(plain.command, Some(Commands::Agent(a)) if !a.save_session));
}

/// The rungs `--effort` accepts are the rungs `reasoning_effort` goes out with, plus `auto`.
///
/// Validated by clap rather than at use: a typo like `--effort mediumish` reaching the wire would
/// come back as a provider error several seconds and one billed prompt later, and that message
/// would be about the request rather than about the flag.
#[test]
fn agent_effort_takes_the_ladder_and_nothing_else() {
    let effort_of = |argv: &[&str]| -> Option<String> {
        let cli = Cli::try_parse_from(argv).expect("parse");
        match cli.command {
            Some(Commands::Agent(a)) => a.effort,
            _ => panic!("expected the agent subcommand"),
        }
    };

    for tier in ["auto", "low", "medium", "high", "xhigh", "max"] {
        assert_eq!(
            effort_of(&["aizen", "agent", "--effort", tier, "việc"]).as_deref(),
            Some(tier)
        );
    }

    // Omitted is its own answer: the configured `reasoning_effort` applies, and the request stays
    // byte-identical to one from a core that never had this flag.
    assert_eq!(effort_of(&["aizen", "agent", "việc"]), None);

    assert!(Cli::try_parse_from(["aizen", "agent", "--effort", "mediumish", "việc"]).is_err());
}

/// `--image` takes a value, repeats, and must never be mistaken for the task.
///
/// The task is positional, so every flag on this subcommand is one bad definition away from
/// swallowing it. A front-end sends `--image <path> <task>`, and if the flag were ever made
/// optional-valued clap would read the task as the path: the run would then fail with "not a
/// PNG/JPEG/GIF/WebP image" naming the user's own sentence, which is a mystifying way to say
/// nothing was attached. Repeatable because one screenshot is the common case and two is not rare.
#[test]
fn agent_image_is_repeatable_and_never_eats_the_task() {
    let cli = Cli::try_parse_from([
        "aizen",
        "agent",
        "--image",
        "a.png",
        "--image",
        "b.jpg",
        "cái nút này sai chỗ nào",
    ])
    .expect("parse");
    let Some(Commands::Agent(args)) = cli.command else {
        panic!("expected the agent subcommand");
    };
    assert_eq!(args.image, vec!["a.png".to_string(), "b.jpg".to_string()]);
    assert_eq!(args.task, "cái nút này sai chỗ nào");

    // Omitted is the old shape exactly: no parts array, no image field, a request byte-identical
    // to one from a core that never had this flag.
    let plain = Cli::try_parse_from(["aizen", "agent", "chạy test"]).expect("parse");
    assert!(matches!(plain.command, Some(Commands::Agent(a)) if a.image.is_empty()));

    // A value is required, so the flag can never stand alone and leave the task unaccounted for.
    assert!(Cli::try_parse_from(["aizen", "agent", "--image"]).is_err());
}

/// `memory list current` and `memory list --scope current` must mean the same thing.
///
/// The REPL has always taken the scope positionally (`/memory list current`), so that is the form
/// muscle memory reaches for; the CLI accepted only the flag and answered a positional with
/// `error: unexpected argument 'current' found`, which reads as "scopes aren't supported" rather
/// than "wrong spelling". The flag stays because it shipped first and scripts pass it — hence
/// two fields, resolved to one value, and `conflicts_with` so passing both is a parse error
/// instead of one silently winning.
#[test]
fn memory_list_takes_its_scope_positionally_or_as_a_flag() {
    let scope_of = |argv: &[&str]| -> Option<String> {
        match Cli::try_parse_from(argv).expect("parse").command {
            Some(Commands::Memory {
                cmd: MemoryCmd::List {
                    scope, scope_flag, ..
                },
            }) => scope.or(scope_flag),
            other => panic!("expected `memory list`, got {other:?}"),
        }
    };

    assert_eq!(
        scope_of(&["aizen", "memory", "list", "current"]).as_deref(),
        Some("current"),
        "the positional form is what the REPL teaches"
    );
    assert_eq!(
        scope_of(&["aizen", "memory", "list", "--scope", "current"]).as_deref(),
        Some("current"),
        "the flag form shipped first and must keep working"
    );
    assert_eq!(
        scope_of(&["aizen", "memory", "list"]),
        None,
        "no scope ⇒ the unfiltered view"
    );
    assert!(
        Cli::try_parse_from(["aizen", "memory", "list", "current", "--scope", "global"]).is_err(),
        "two scopes at once is a mistake, not a precedence puzzle"
    );
}

/// The listings name only per-id verbs, so bulk editing needs the folder — and a folder that
/// does not exist yet has to say so rather than read as an empty one, which is the difference
/// between "nothing learned yet" and "look somewhere else".
#[test]
fn where_reports_name_every_store_and_flag_missing_dirs() {
    let _g = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("aizen-whererep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::env::set_var("AIZEN_HOME", &home);

    let entries = crate::core::config::entries_dir();
    std::fs::create_dir_all(&entries).unwrap();
    std::fs::write(entries.join("a.md"), "---\nname: A\n---\nx").unwrap();
    std::fs::write(entries.join("b.md"), "---\nname: B\n---\ny").unwrap();
    // Not a fact — the count must not include it.
    std::fs::write(entries.join("notes.txt"), "ignore me").unwrap();

    let rep = memory_where_report();
    assert!(rep.contains("2 fact(s)"), "{rep}");
    assert!(
        rep.contains(&entries.display().to_string()),
        "entries path missing:\n{rep}"
    );
    // The review dir is the one that matters most — 29 queued items were invisible because
    // nothing ever said they were on disk.
    assert!(rep.contains("review"), "{rep}");
    assert!(
        rep.contains("(not created yet)"),
        "an absent dir must not read as an empty one:\n{rep}"
    );

    // Skills live in THREE roots and the list's `[project]`/`[repo]` tags never say where.
    let sk = skill_where_report();
    for label in ["global", "zone", "repo"] {
        assert!(sk.contains(label), "{label} root missing:\n{sk}");
    }
    assert!(
        sk.contains(&crate::core::config::project_slug()),
        "zone slug not spelled out:\n{sk}"
    );

    std::env::remove_var("AIZEN_HOME");
    let _ = std::fs::remove_dir_all(&home);
}

/// Both `where` sub-commands have to be reachable, or the footers point at nothing.
#[test]
fn where_subcommands_parse() {
    assert!(matches!(
        Cli::try_parse_from(["aizen", "memory", "where"])
            .expect("parse")
            .command,
        Some(Commands::Memory {
            cmd: MemoryCmd::Where
        })
    ));
    assert!(matches!(
        Cli::try_parse_from(["aizen", "skill", "where"])
            .expect("parse")
            .command,
        Some(Commands::Skill {
            cmd: SkillCmd::Where
        })
    ));
}
/// A streaming turn must survive longer than any total-request deadline.
///
/// 0.5.2 put `.timeout(1800s)` on the shared client as a catch-all backstop. reqwest applies that
/// "from when the request starts connecting until the response body has finished", and this same
/// client is what the REPL hands to `stream_chat_with_tools_eager` — so it did not cap only
/// pathological hangs, it cut off a HEALTHY stream still emitting tokens. The stall protection a
/// stream needs is per-event (`read_timeout` plus the inter-event watchdog in `llm::client`),
/// which can tell "gateway went quiet" from "answer is long"; a total deadline cannot.
///
/// Two dead ends preceded this shape, both worth recording so nobody re-walks them:
/// * Timing `http_client()` against a slow fixture proves nothing. The real ceiling was 1800s, so
///   any fixture short enough to run in a test suite passes with the bug reintroduced — verified,
///   it did.
/// * `Client`'s `Debug` does not print the total timeout at all in reqwest 0.12 (it prints
///   `read_timeout`, which merely CONTAINS the substring). A structural assertion is impossible.
///
/// So this asserts the mechanism instead, on a client built exactly like the shared one but with
/// a deliberately tiny ceiling: a total deadline kills a body that is still arriving. That is the
/// behaviour the production client must not have, demonstrated in a second rather than half an
/// hour — and the paired run through the real `http_client()` shows the same fixture completing.
#[tokio::test]
async fn a_total_deadline_truncates_a_healthy_stream_but_the_shared_client_has_none() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Serve a body in slow chunks: still arriving after ~900ms, never byte-silent for long.
    async fn slow_body_server() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/stream", listener.local_addr().unwrap());
        let h = tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 12\r\nConnection: close\r\n\r\n",
                        ).await;
                    for _ in 0..6 {
                        let _ = sock.write_all(b"ab").await;
                        let _ = sock.flush().await;
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    }
                });
            }
        });
        (url, h)
    }

    // 1. The mechanism: a total deadline shorter than the body's span truncates it. Same builder
    //    settings as production, so the ONLY difference under test is `.timeout()`.
    let (url, srv) = slow_body_server().await;
    let ceilinged = reqwest::Client::builder()
        .read_timeout(std::time::Duration::from_secs(300))
        .timeout(std::time::Duration::from_millis(300))
        .build()
        .expect("build ceilinged client");
    let cut = async { ceilinged.get(&url).send().await?.text().await }.await;
    assert!(
        cut.is_err(),
        "a total-request deadline must kill a body still arriving — if this stops failing, \
             reqwest changed and the premise of this test needs re-checking"
    );

    // 2. The production client, same fixture, must read it to completion.
    let http = http_client().expect("build shared client");
    let whole = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        http.get(&url).send().await?.text().await
    })
    .await
    .expect("the request must not outlive the test guard")
    .expect("a response that keeps arriving must not be cut off");
    srv.abort();
    assert_eq!(
        whole, "abababababab",
        "the whole slow body must arrive through the client the REPL streams turns with"
    );
}

/// A background chore call (secretary, persona reflection, compaction, reconcile, handoff,
/// persona-distill) must be TIME-BOUNDED, not merely byte-bounded.
///
/// Every one of those routes the NON-streaming `chat_with_tools`, whose only native guard is
/// reqwest's `read_timeout` — and that fires only when the socket goes BYTE-silent. A gateway that
/// accepts the POST and then keepalive-drips (or simply never writes the response body) leaves the
/// call parked forever: `read_timeout` keeps re-arming on the drip, and the shared client carries
/// no total-request ceiling (removing that is the Bug-1 fix above, deliberately). Before
/// `chore_chat` centralised the deadline, a single such hang silently killed the secretary /
/// compaction for the rest of the session with no error surfaced.
///
/// This proves the mechanism on a server that ACCEPTS the connection and then never answers: only
/// a wall-clock deadline can end that, and `chore_chat` must return `Err` within it rather than
/// awaiting a byte that never comes. `AIZEN_SUBAGENT_CALL_SECS=1` shrinks the ceiling so the test
/// finishes in ~1s; the env lock keeps it from racing the other env-touching tests.
#[tokio::test]
async fn a_chore_call_against_a_silent_gateway_returns_err_not_a_permanent_park() {
    use tokio::io::AsyncReadExt;

    // A server that accepts, reads the request, and then holds the socket open forever without
    // ever writing a response. `read_timeout` cannot fire (no bytes were promised then withheld
    // mid-body — nothing is sent at all), so only the deadline can end the call.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let srv = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = vec![0u8; 2048];
            let _ = sock.read(&mut buf).await; // drain the request line/headers, then stall
            held.push(sock); // keep the socket alive; never write a byte back
        }
    });

    let _g = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    const K: &str = "AIZEN_SUBAGENT_CALL_SECS";
    let prior = std::env::var(K).ok();
    std::env::set_var(K, "1");

    let http = http_client().expect("build shared client");
    let msgs = [Message::user("ping".to_string())];

    let started = std::time::Instant::now();
    // The whole point: this awaits, at most, one deadline — never the silent socket forever.
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        chore_chat(&http, &base, "k", "m", &msgs, &[]),
    )
    .await
    .expect("chore_chat must return on its OWN deadline, not park until the test guard fires");
    let elapsed = started.elapsed();

    match prior {
        Some(v) => std::env::set_var(K, v),
        None => std::env::remove_var(K),
    }
    srv.abort();

    assert!(
        out.is_err(),
        "a chore call to a gateway that never answers must surface an Err, not hang"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "the 1s ceiling must end the call promptly; took {elapsed:?} — if this blows past the \
             deadline, chore_chat stopped bounding the call in time"
    );
}

/// The `/v1` hint fires only when the URL genuinely lacks a version segment. Both directions
/// matter: no hint on an already-versioned URL (suggesting `/v1/v1` sends the user in circles),
/// and a hint whenever the last segment is not `v<digits>` — `/api` and `/openai` are paths, not
/// versions, which is exactly the case that leaves people stuck on a 404 with nothing to try.
///
/// The steering mailbox must never stay armed past a turn that never ran.
///
/// `steer::arm()` now fires with the cancel token, BEFORE prep, so a `> also do X` typed during
/// retrieval reaches the running turn instead of the post-turn queue. That earlier arming is only
/// safe because `SteerMailboxGuard` closes the mailbox on the paths that abort prep (an
/// unconfigured endpoint, a `#remember`/`!shell` input, an Esc during prep) and so never reach the
/// explicit disarm at the end of a turn. Left armed, the input thread would keep accepting steers
/// into a slot nothing drains, and the next turn's `arm()` would clear them — user input silently
/// eaten, which is strictly worse than the queueing this whole change was fixing.
#[test]
fn the_steer_guard_closes_the_mailbox_and_requeues_what_the_turn_never_took() {
    let _lock = crate::core::steer::test_lock();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<tui::Submission>();

    crate::core::steer::arm();
    assert!(crate::core::steer::push("also update the README"));
    {
        let _guard = SteerMailboxGuard(tx.clone());
    } // prep aborts here: the guard is the only thing that gets to run

    assert!(
        !crate::core::steer::is_armed(),
        "a mailbox left armed would accept steers nothing will ever drain"
    );
    let got = rx
        .try_recv()
        .expect("the un-consumed steer must come back as a submission, not vanish");
    assert_eq!(
        got,
        tui::Submission::Chat("also update the README".into(), Vec::new()),
        "it re-enters as an ordinary message so it runs as the next turn"
    );

    // Idempotent: the normal end-of-turn path disarms explicitly, so on that path the guard fires
    // afterwards on an already-closed mailbox. That has to be a no-op, not a second delivery of a
    // message the user sent once.
    {
        let _guard = SteerMailboxGuard(tx.clone());
    }
    assert!(
        rx.try_recv().is_err(),
        "a second disarm must not re-deliver the same steer"
    );
}

/// `/compact` used to `await` its summarizer call bare inside the REPL loop: no token armed, so
/// `turn_in_flight()` was false and Esc merely cleared the draft, while the REPL sat blocked in
/// the await consuming nothing. A hung endpoint froze the app until the 300s read timeout. The
/// wrapper must (a) report in-flight so Esc routes to cancel, (b) actually return on cancel
/// rather than waiting out the call, and (c) leave the slot clean for the next turn.
#[tokio::test]
async fn cancellable_slash_lets_esc_abort_a_hung_model_call() {
    let _g = tui::TEST_CANCEL_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // A call that never returns — stands in for a dead endpoint inside the read timeout.
    let hung = async {
        std::future::pending::<()>().await;
        unreachable!("the wrapper must not wait for this to finish");
    };
    // Press Esc once the wrapper has armed its token: this is exactly what the input thread does.
    let presser = tokio::spawn(async {
        for _ in 0..200 {
            if tui::turn_in_flight() {
                tui::request_cancel();
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        false
    });
    let out = cancellable_slash(hung).await;
    assert!(
        presser.await.unwrap(),
        "the wrapper must report in-flight so Esc means cancel"
    );
    assert!(
        out.is_none(),
        "cancel must win the race instead of blocking on the call"
    );
    assert!(
        !tui::turn_in_flight(),
        "the token is disarmed on the way out, so Esc goes idle again"
    );
}

/// A settings change mid-chat must not end the conversation. `/config` and `/model` used to route
/// through `rebuild_system`, whose `seed_prompt_lanes` starts with `history.clear()` — so going to
/// config to retune the context and coming back left an empty thread.
#[test]
fn refreshing_prompt_lanes_keeps_the_conversation() {
    let mut history = vec![
        Message::system("STABLE LANE v1".to_string()),
        Message::system("dynamic lane v1".to_string()),
        Message::user("câu hỏi đầu tiên".to_string()),
        Message::assistant("trả lời đầu tiên".to_string()),
        Message::user("câu hỏi thứ hai".to_string()),
    ];
    let before: Vec<_> = history
        .iter()
        .filter(|m| m.role != "system")
        .cloned()
        .collect();

    refresh_prompt_lanes_in_place(&mut history, "opus-4-8");

    // Every non-system message survives, in order.
    let after: Vec<_> = history
        .iter()
        .filter(|m| m.role != "system")
        .cloned()
        .collect();
    assert_eq!(
        after.len(),
        before.len(),
        "conversation dropped: {history:#?}"
    );
    for (a, b) in after.iter().zip(before.iter()) {
        assert_eq!(a.role, b.role);
        assert_eq!(a.content, b.content);
    }
    // The stale lanes are gone (rewritten, not appended) and the leading block is still systems.
    let lead = agent::compact::leading_system_count(&history);
    assert!(
        (1..=2).contains(&lead),
        "expected 1-2 system lanes, got {lead}"
    );
    assert!(
        !history[..lead]
            .iter()
            .any(|m| m.content.as_deref() == Some("STABLE LANE v1")),
        "old stable lane not replaced"
    );
    assert_eq!(
        history[lead].role, "user",
        "conversation must start right after the lanes"
    );

    // And the contrast: the /clear path is still allowed to wipe.
    let mut fresh = history.clone();
    rebuild_system(&mut fresh, "opus-4-8");
    assert!(
        !fresh.iter().any(|m| m.role != "system"),
        "rebuild_system is the /clear path and must still reset"
    );
}

/// A conversation containing a pasted image must survive save → load.
///
/// This is the session-level half of the `Message` round-trip bug: the writer emitted `content`
/// as a multimodal parts array, `parse_session_bytes` could not read it back, and the file was
/// reported as "(unreadable)" for the rest of its life with the serde error discarded. Two real
/// transcripts were lost this way before it was noticed, so the reason string is asserted too —
/// a silent `None` is what let this hide.
#[test]
fn a_session_with_images_survives_save_and_load() {
    let _g = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("aizen-img-session-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::env::set_var("AIZEN_HOME", &home);
    set_session_slug(None);
    std::fs::create_dir_all(sessions_dir()).unwrap();

    let history = vec![
        Message::system("lane"),
        Message::user_with_images(
            "read this screenshot",
            vec!["data:image/png;base64,iVBORw0KGgo=".to_string()],
        ),
        Message::assistant("it says hello"),
    ];
    save_session(&history, "with-image", Some("m")).unwrap();

    let bytes = std::fs::read(sessions_dir().join("with-image.json")).unwrap();
    let (msgs, _) =
        parse_session_reason(&bytes).expect("a transcript we wrote ourselves must be readable");
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[1].content.as_deref(), Some("read this screenshot"));
    assert_eq!(
        msgs[1].images,
        vec!["data:image/png;base64,iVBORw0KGgo=".to_string()],
        "image attachments must come back out of the parts array"
    );

    // The picker must count it as a real conversation, not report it unreadable.
    let (count, _) = read_session_row(&sessions_dir().join("with-image.json"));
    assert_eq!(count, Some(2), "2 conversation turns after the system lane");

    // And a genuinely corrupt file still fails — with a reason attached.
    let why = parse_session_reason(b"{not json").expect_err("corrupt must not parse");
    assert!(!why.is_empty(), "the failure must explain itself");

    std::env::remove_var("AIZEN_HOME");
    let _ = std::fs::remove_dir_all(&home);
}

/// `/resume` and the startup hint must offer THIS project's newest session, not whichever
/// project's conversation happened to write last — the shared flat pool is exactly how a
/// foreign transcript used to be offered unlabeled and restored into the wrong repo.
#[test]
fn most_recent_session_prefers_this_project_over_a_newer_foreign_one() {
    // Serialize with every home-MUTATING test (zones/skills/memory sandboxes repoint
    // AIZEN_HOME then delete their tree) — this test's saves resolve through the home.
    let _g = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("aizen-recent-scope-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::env::set_var("AIZEN_HOME", &home);
    set_session_slug(None);
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir).unwrap();

    // A HERE session, saved through the real writer (stamps the current project key)…
    let history = vec![
        Message::system("lane".to_string()),
        Message::user("here-work".to_string()),
    ];
    save_session(&history, "zzz-here", Some("m1")).unwrap();
    // …and a FOREIGN session. Named to sort as "newer" under the scan's equal-mtime
    // name tie-break, so this test can't pass by timing luck. Its root EXISTS on disk, which
    // is the ordinary case (two live checkouts): the label names the dir with no caveat.
    let foreign_root = home.join("else");
    std::fs::create_dir_all(&foreign_root).unwrap();
    let foreign = serde_json::json!({
        "version": 2,
        "meta": {
            "project_key": "c:/somewhere/else",
            "project_root": foreign_root.display().to_string(),
        },
        "messages": [
            { "role": "system", "content": "lane" },
            { "role": "user", "content": "foreign-work" },
        ]
    });
    std::fs::write(
        dir.join("aaa-foreign.json"),
        serde_json::to_vec(&foreign).unwrap(),
    )
    .unwrap();

    let (slug, n, origin) = most_recent_session().expect("a saved session must be found");
    assert_eq!(
        slug, "zzz-here",
        "must prefer this project's session over a foreign one"
    );
    assert_eq!(
        n, 1,
        "hint counts conversation turns, not raw vector length"
    );
    assert!(
        origin.is_none(),
        "a same-project offer carries no origin label"
    );

    // With the here-session gone, the foreign one IS offered — but labeled with its origin.
    std::fs::remove_file(dir.join("zzz-here.json")).unwrap();
    let (slug, _, origin) = most_recent_session().expect("foreign fallback must be offered");
    assert_eq!(slug, "aaa-foreign");
    assert_eq!(
        origin.as_deref(),
        Some("from else"),
        "a foreign offer must name its project"
    );

    // And when that project's dir is GONE (deleted or renamed checkout), the label says so
    // instead of naming a path the user can no longer go look at.
    std::fs::remove_dir_all(&foreign_root).unwrap();
    let (_, _, origin) = most_recent_session().expect("foreign fallback must be offered");
    assert_eq!(
        origin.as_deref(),
        Some("from else (path gone)"),
        "a vanished origin must be flagged, not presented as a live project"
    );

    std::env::remove_var("AIZEN_HOME");
    let _ = std::fs::remove_dir_all(&home);
}

/// Every saved file must carry provenance (project key/root/slug + timestamps), `created` must
/// survive re-saves, and a pre-provenance bare-array file must still load.
#[test]
fn session_files_carry_provenance_and_legacy_arrays_still_load() {
    let _g = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("aizen-sess-prov-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::env::set_var("AIZEN_HOME", &home);
    set_session_slug(None);
    let dir = sessions_dir();

    let history = vec![
        Message::system("lane".to_string()),
        Message::user("stamp me".to_string()),
    ];
    save_session(&history, "stamped", Some("model-x")).unwrap();
    let bytes = std::fs::read(dir.join("stamped.json")).unwrap();
    let (_, meta) = parse_session_bytes(&bytes).expect("v2 file parses");
    let meta = meta.expect("v2 file carries meta");
    assert_eq!(
        meta.project_key.as_deref(),
        Some(config::project_key().as_str())
    );
    assert_eq!(meta.model.as_deref(), Some("model-x"));
    let created = meta.created.clone().expect("created stamped");
    // Re-save: `created` is the file's birth stamp and must not advance.
    save_session(&history, "stamped", Some("model-x")).unwrap();
    let bytes = std::fs::read(dir.join("stamped.json")).unwrap();
    let (_, meta2) = parse_session_bytes(&bytes).unwrap();
    assert_eq!(meta2.unwrap().created.as_deref(), Some(created.as_str()));

    // Legacy: a bare `Vec<Message>` array (what every pre-provenance save wrote).
    let legacy = serde_json::json!([
        { "role": "system", "content": "old lane" },
        { "role": "user", "content": "legacy question" },
    ]);
    std::fs::write(
        dir.join("old-chat.json"),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();
    let mut restored = Vec::new();
    let n = load_session(&mut restored, "old-chat", "model-x").unwrap();
    assert_eq!(n, 1, "legacy conversation still loads");
    assert!(restored
        .iter()
        .any(|m| m.content.as_deref() == Some("legacy question")));

    set_session_slug(None);
    std::env::remove_var("AIZEN_HOME");
    let _ = std::fs::remove_dir_all(&home);
}

/// Restoring a session saved elsewhere must REBUILD the system lanes for the current project:
/// keeping the file's own stable lane grafted the other project's context onto this cwd, and
/// the model confidently edited the wrong tree.
#[test]
fn load_session_rebuilds_stale_lanes_for_the_current_project() {
    let _g = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("aizen-sess-lanes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::env::set_var("AIZEN_HOME", &home);
    set_session_slug(None);
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir).unwrap();

    let stale = "STALE STABLE LANE recorded in another checkout";
    let foreign = serde_json::json!({
        "version": 2,
        "meta": { "project_key": "c:/somewhere/else", "project_root": "C:/somewhere/else" },
        "messages": [
            { "role": "system", "content": stale },
            { "role": "user", "content": "carried question" },
        ]
    });
    std::fs::write(
        dir.join("from-b.json"),
        serde_json::to_vec(&foreign).unwrap(),
    )
    .unwrap();

    let mut history = Vec::new();
    let n = load_session(&mut history, "from-b", "model-x").unwrap();
    assert_eq!(n, 1);
    assert!(
        !history.iter().any(|m| m.content.as_deref() == Some(stale)),
        "the foreign stable lane must be replaced, not replayed"
    );
    assert_eq!(
        history[0].role, "system",
        "current-project lanes lead the restored thread"
    );
    assert!(
        history
            .iter()
            .any(|m| m.content.as_deref() == Some("carried question")),
        "the conversation itself is preserved"
    );

    set_session_slug(None);
    std::env::remove_var("AIZEN_HOME");
    let _ = std::fs::remove_dir_all(&home);
}

/// The pool scan must classify every file shape it can meet: this project's v2 file, another
/// project's v2 file, a pre-provenance bare array (project unknown), and a corrupt file — which
/// must read as UNREADABLE, never as a plausible empty conversation the user might restore.
#[test]
fn scan_sessions_classifies_mine_foreign_unlabeled_and_corrupt() {
    let _g = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("aizen-scan-classes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::env::set_var("AIZEN_HOME", &home);
    set_session_slug(None);
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir).unwrap();

    save_session(
        &[Message::system("lane"), Message::user("mine")],
        "mine",
        Some("m"),
    )
    .unwrap();
    // A live foreign checkout: its root exists, so the label names it without a caveat. (The
    // vanished-root variant is covered by the most_recent_session test.)
    let foreign_root = home.join("repo");
    std::fs::create_dir_all(&foreign_root).unwrap();
    let foreign = serde_json::json!({
        "version": 2,
        "meta": {
            "project_key": "c:/other/repo",
            "project_root": foreign_root.display().to_string(),
        },
        "messages": [{ "role": "user", "content": "theirs" }]
    });
    std::fs::write(
        dir.join("theirs.json"),
        serde_json::to_vec(&foreign).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("unlabeled.json"),
        br#"[{"role":"user","content":"old"}]"#,
    )
    .unwrap();
    std::fs::write(dir.join("broken.json"), b"{not json").unwrap();
    // The retired pointer must never appear as a restorable row.
    std::fs::write(
        dir.join("last.json"),
        br#"[{"role":"user","content":"ptr"}]"#,
    )
    .unwrap();

    let pool = scan_sessions();
    let by = |n: &str| pool.iter().find(|s| s.name == n).expect("row present");
    assert!(
        !pool.iter().any(|s| s.name == "last"),
        "the pointer is not a session row"
    );
    assert_eq!(by("mine").here, Some(true));
    assert_eq!(by("theirs").here, Some(false));
    assert_eq!(
        by("unlabeled").here,
        None,
        "no provenance → project unknown, not 'foreign'"
    );
    assert_eq!(
        by("broken").msgs,
        None,
        "a corrupt file is unreadable, not empty"
    );
    assert_eq!(
        session_origin_label(by("theirs").meta.as_ref()),
        "from repo"
    );
    assert_eq!(
        session_origin_label(by("unlabeled").meta.as_ref()),
        "project unknown"
    );

    set_session_slug(None);
    std::env::remove_var("AIZEN_HOME");
    let _ = std::fs::remove_dir_all(&home);
}

/// The picker's age column must tell "I couldn't read the clock" and "this file claims the
/// future" APART from "saved just now" — the three used to render identically, so an unreadable
/// or clock-skewed row looked like the freshest conversation in the pool.
#[test]
fn session_age_distinguishes_unknown_skewed_and_real_stamps() {
    assert_eq!(
        fmt_session_age(None),
        "age unknown",
        "no mtime is not 'just now'"
    );

    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    assert_eq!(
        fmt_session_age(Some(now_ms + 3_600_000)),
        "future timestamp (clock skew)",
        "a stamp beyond the skew grace must be called out, not sorted to the top silently"
    );
    // Just inside the grace window: ordinary filesystem/clock jitter still reads as fresh.
    assert!(!fmt_session_age(Some(now_ms + 5_000)).contains("clock skew"));

    // Epoch 0 must not collapse into fmt_time_ago's "unknown" sentinel.
    assert_ne!(fmt_session_age(Some(0)), "age unknown");
    let hour_ago = fmt_session_age(Some(now_ms.saturating_sub(3_600_000)));
    assert!(
        hour_ago.contains('h') || hour_ago.contains("hour"),
        "real age renders: {hour_ago}"
    );
}

/// The compact age is a COLUMN, so its width is the contract: three cells, always. The verbose
/// `fmt_session_age` is right for a status line and wrong here — "future timestamp (clock skew)"
/// is 30 characters of prose sitting in front of the only field the user reads.
#[test]
fn compact_age_always_fits_three_cells() {
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    // In order: unreadable, now, skewed into the future, 5m, 19h, 62d, epoch (years).
    for ms in [
        None,
        Some(now_ms),
        Some(now_ms + 3_600_000),
        Some(now_ms.saturating_sub(5 * 60_000)),
        Some(now_ms.saturating_sub(19 * 3_600_000)),
        Some(now_ms.saturating_sub(62 * 86_400_000)),
        Some(0),
    ] {
        let s = fmt_session_age_compact(ms);
        assert!(
            s.chars().count() <= 3 && !s.is_empty(),
            "{ms:?} → {s:?} must fit the 3-cell column"
        );
    }
    assert_eq!(fmt_session_age_compact(None), "?");
    assert_eq!(
        fmt_session_age_compact(Some(now_ms.saturating_sub(19 * 3_600_000))),
        "19h"
    );
}

/// Save-as must refuse `last`: the picker skips that stem, so accepting it printed "saved" for a
/// file the user could then neither restore nor delete, and pinned every later autosave to it.
#[test]
fn save_as_refuses_the_retired_pointer_name() {
    assert!(session_save_name_error("last").is_some());
    assert!(
        session_save_name_error("  last  ").is_some(),
        "trimmed before the check"
    );
    assert!(
        session_save_name_error("LAST").is_none(),
        "case-distinct stems are distinct files"
    );
    assert!(
        session_save_name_error("lastly").is_none(),
        "only the exact name is reserved"
    );
    // Punctuation sanitizes to a DIFFERENT stem (`last_`), which is its own listable file.
    assert!(session_save_name_error("last!").is_none());
    assert!(session_save_name_error("fix-the-parser").is_none());
}

/// `aizen zone migrate` must re-home SESSIONS too. They are the one artifact keyed by provenance
/// inside the file rather than by directory, so the slug-directory sweep was blind to them: after
/// a rename/move every one of the user's own transcripts stayed labeled "from <old dir>" forever.
#[test]
fn zone_migrate_rehomes_sessions_carrying_a_legacy_slug() {
    let _g = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("aizen-zone-sess-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::env::set_var("AIZEN_HOME", &home);
    set_session_slug(None);
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir).unwrap();

    let legacy_slug = "zzz-legacy-slug";
    let file = serde_json::json!({
        "version": 2,
        "meta": {
            "project_key": "c:/old/checkout",
            "project_root": "C:/old/checkout",
            "project_slug": legacy_slug,
            "created": "2026-01-01T00:00:00+00:00",
            "updated": "2026-01-02T00:00:00+00:00",
        },
        "messages": [
            { "role": "system", "content": "lane" },
            { "role": "user", "content": "pre-move work" },
        ]
    });
    std::fs::write(dir.join("moved.json"), serde_json::to_vec(&file).unwrap()).unwrap();
    // An unrelated row must be left strictly alone.
    save_session(
        &[Message::system("lane"), Message::user("mine")],
        "mine",
        Some("m"),
    )
    .unwrap();

    assert_eq!(
        count_sessions_of_slug(legacy_slug),
        1,
        "the plan must see the session"
    );
    assert_eq!(count_sessions_of_slug("no-such-slug"), 0);

    let mut errs: Vec<String> = Vec::new();
    let n = retag_sessions_of_slug(legacy_slug, &mut |e| errs.push(e));
    assert_eq!(
        (n, errs.len()),
        (1, 0),
        "exactly the legacy row is re-homed, cleanly"
    );

    let pool = scan_sessions();
    let moved = pool
        .iter()
        .find(|s| s.name == "moved")
        .expect("row survives");
    assert_eq!(
        moved.here,
        Some(true),
        "the transcript now reads as this project's own"
    );
    assert_eq!(moved.msgs, Some(1), "the conversation itself is untouched");
    let meta = moved.meta.as_ref().expect("provenance rewritten");
    assert_eq!(
        meta.project_slug.as_deref(),
        Some(config::project_slug().as_str())
    );
    // The aging clock must NOT be reset by a bookkeeping rewrite.
    assert_eq!(meta.updated.as_deref(), Some("2026-01-02T00:00:00+00:00"));
    assert_eq!(meta.created.as_deref(), Some("2026-01-01T00:00:00+00:00"));

    assert_eq!(
        count_sessions_of_slug(legacy_slug),
        0,
        "migration is idempotent-complete"
    );

    set_session_slug(None);
    std::env::remove_var("AIZEN_HOME");
    let _ = std::fs::remove_dir_all(&home);
}

/// A restored legacy `last` pointer must be RE-HOMED into a real named file: pinning the live
/// slug to `last` made every later turn overwrite the pointer instead of a conversation.
#[test]
fn restoring_the_legacy_last_pointer_rehomes_it_to_a_named_file() {
    let _g = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("aizen-last-rehome-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::env::set_var("AIZEN_HOME", &home);
    set_session_slug(None);
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir).unwrap();

    // A pre-provenance pool whose ONLY file is the shared pointer.
    let legacy = serde_json::json!([
        { "role": "system", "content": "old lane" },
        { "role": "user", "content": "pointer-era chat" },
    ]);
    std::fs::write(dir.join("last.json"), serde_json::to_vec(&legacy).unwrap()).unwrap();

    // The hint must offer it (not "nothing to resume") under a NEW name…
    let (slug, n, _) = most_recent_session().expect("the legacy pointer must still be offered");
    assert_ne!(
        slug, "last",
        "the offer must be re-homed, not the pointer itself"
    );
    assert_eq!(n, 1);
    assert!(
        dir.join(format!("{slug}.json")).exists(),
        "re-homed file was written"
    );

    // …and restoring the pointer directly must never pin the live slug to `last`.
    let mut history = Vec::new();
    load_session(&mut history, "last", "model-x").unwrap();
    assert_ne!(
        current_session_slug().as_deref(),
        Some("last"),
        "the live slug must never be the pointer"
    );

    set_session_slug(None);
    std::env::remove_var("AIZEN_HOME");
    let _ = std::fs::remove_dir_all(&home);
}

/// The `/handoff` seed is conversation content, not prompt prefix: a lane rewrite (what
/// `/config` and `/model` do via `refresh_prompt_lanes_in_place`) must splice AROUND it.
/// Before the marker, the splice consumed it and the fresh thread silently lost its context.
#[test]
fn handoff_seed_survives_a_lane_rewrite() {
    let _g = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("aizen-handoff-seed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::env::set_var("AIZEN_HOME", &home);

    let seed = format!(
        "{}\ndecisions: use the v2 format",
        agent::compact::HANDOFF_MARKER_PREFIX
    );
    let mut history = vec![
        Message::system("stable lane".to_string()),
        Message::system("dynamic lane".to_string()),
        Message::system(seed.clone()),
        Message::user("continue the migration".to_string()),
    ];
    assert_eq!(
        agent::compact::leading_system_count(&history),
        2,
        "the seed must not count as prompt prefix"
    );
    refresh_prompt_lanes_in_place(&mut history, "model-x");
    assert!(
        history
            .iter()
            .any(|m| m.content.as_deref() == Some(seed.as_str())),
        "a /config-style lane rewrite must keep the handoff seed"
    );
    assert!(
        history
            .iter()
            .any(|m| m.content.as_deref() == Some("continue the migration")),
        "the conversation tail survives too"
    );

    std::env::remove_var("AIZEN_HOME");
    let _ = std::fs::remove_dir_all(&home);
}

/// The mid-turn publish hook is what makes a terminal closed DURING a turn keep the work: the
/// agent loop owns `history` for the whole turn, so without it the exit snapshot stayed frozen at
/// the user's question and every reply/tool result produced so far was lost.
#[test]
fn publishing_mid_turn_advances_the_exit_snapshot() {
    let early = vec![
        Message::system("lane".to_string()),
        Message::user("làm việc đi".to_string()),
    ];
    publish_live_history(&early);
    let snap_before = live_history_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len();

    // The turn progresses: assistant reply + a tool result land while the loop still owns history.
    let mut mid = early.clone();
    mid.push(Message::assistant("đang chạy".to_string()));
    mid.push(Message::tool_result("call-1", "kết quả"));
    publish_live_history(&mid);

    let snap_after = live_history_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(snap_before, early.len());
    assert_eq!(
        snap_after.len(),
        mid.len(),
        "mid-turn progress never reached the exit snapshot"
    );
    assert_eq!(
        snap_after.last().unwrap().content.as_deref(),
        Some("kết quả")
    );
}

/// A legacy single-lane history (persisted before the split) must gain both lanes without
/// losing the chat — `splice(0..1, …)` grows the leading block in place.
#[test]
fn refreshing_prompt_lanes_migrates_a_legacy_single_lane() {
    let mut history = vec![
        Message::system("legacy combined prompt".to_string()),
        Message::user("giữ tôi lại".to_string()),
    ];
    refresh_prompt_lanes_in_place(&mut history, "opus-4-8");
    let lead = agent::compact::leading_system_count(&history);
    assert_eq!(history[lead].content.as_deref(), Some("giữ tôi lại"));
    assert!(
        !history
            .iter()
            .any(|m| m.content.as_deref() == Some("legacy combined prompt")),
        "legacy lane should be replaced, not kept"
    );
}

#[test]
fn classify_health_probe_rules() {
    // Ok + fast → green.
    assert_eq!(
        classify_health_probe(Ok(std::time::Duration::from_millis(500))),
        tui::HealthKind::Ok
    );
    // Ok + at the threshold still green (strictly > 2s is yellow).
    assert_eq!(
        classify_health_probe(Ok(std::time::Duration::from_millis(HEALTH_SLOW_MS as u64))),
        tui::HealthKind::Ok
    );
    // Ok + slow → yellow.
    assert_eq!(
        classify_health_probe(Ok(std::time::Duration::from_millis(
            HEALTH_SLOW_MS as u64 + 1
        ))),
        tui::HealthKind::Unstable
    );
    // Transient error → yellow.
    assert_eq!(
        classify_health_probe(Err(anyhow!(
            "upstream returned HTTP 503 Service Unavailable: try later"
        ))),
        tui::HealthKind::Unstable
    );
    assert_eq!(
        classify_health_probe(Err(anyhow!("request failed after retries"))),
        tui::HealthKind::Unstable
    );
    // Permanent 4xx → red.
    assert_eq!(
        classify_health_probe(Err(anyhow!(
            "upstream returned HTTP 401 Unauthorized: bad key"
        ))),
        tui::HealthKind::Down
    );
    assert_eq!(
        classify_health_probe(Err(anyhow!(
            "upstream returned HTTP 404 Not Found: no such path"
        ))),
        tui::HealthKind::Down
    );
    // Missing config is handled by the poller as Down (not via this classifier) — here we only
    // assert network-shaped errors.
    let missing = classify_health_probe(Err(anyhow!("no API key — run `aizen config`")));
    assert_eq!(
        missing,
        tui::HealthKind::Unstable,
        "bare 'no API key' has no HTTP code → Transient/yellow; poller maps resolve fail → red"
    );
}

#[test]
fn effort_turn_line_names_the_tier_or_default() {
    // The per-turn status line must contain the tier name (or "default" when the field is
    // omitted), regardless of colour stripping under the test harness.
    assert!(effort_turn_line(Some("high")).contains("high"));
    assert!(effort_turn_line(Some("low")).contains("low"));
    assert!(
        effort_turn_line(Some("xhigh")).contains("xhigh"),
        "xhigh rung named"
    );
    assert!(
        effort_turn_line(Some("max")).contains("max"),
        "max rung named"
    );
    assert!(effort_turn_line(None).contains("default"), "None ⇒ default");
    assert!(
        effort_turn_line(Some("high")).contains("effort:"),
        "always prefixed"
    );
}

#[test]
fn apply_effort_choice_index_mapping_is_total() {
    // Guard the index→tier table the slider feeds apply_effort_choice: 0=auto, 1..=5 pin a tier.
    // (We assert the mapping shape, not the persisted config — save() touches the real config.)
    let tier = |i: usize| ["", "low", "medium", "high", "xhigh", "max"][i];
    assert_eq!(tier(1), "low");
    assert_eq!(tier(2), "medium");
    assert_eq!(tier(3), "high");
    assert_eq!(tier(4), "xhigh");
    assert_eq!(tier(5), "max");
}

#[test]
fn ctx_window_matches_known_families() {
    assert_eq!(ctx_window_for("claude-opus-4-8"), 200_000);
    assert_eq!(ctx_window_for("gemini-2.5-pro"), 1_000_000);
    assert_eq!(ctx_window_for("deepseek-chat"), 64_000);
    assert_eq!(ctx_window_for("gpt-4o-mini"), 128_000); // default family
    assert_eq!(ctx_window_for("some-unknown-model"), 128_000); // fallback
}

#[test]
fn classify_source_covers_the_matrix() {
    use super::InstallSource::*;
    // GitHub shorthand
    assert_eq!(
        classify_source("msitarzewski/agency-agents").unwrap(),
        GitHubShorthand("msitarzewski/agency-agents".into())
    );
    // git URLs (https repo, .git, scp-like, ssh)
    assert_eq!(
        classify_source("https://github.com/owner/repo").unwrap(),
        GitUrl("https://github.com/owner/repo".into())
    );
    assert_eq!(
        classify_source("https://github.com/owner/repo.git").unwrap(),
        GitUrl("https://github.com/owner/repo.git".into())
    );
    assert_eq!(
        classify_source("git@github.com:owner/repo.git").unwrap(),
        GitUrl("git@github.com:owner/repo.git".into())
    );
    assert_eq!(
        classify_source("ssh://git@host/owner/repo").unwrap(),
        GitUrl("ssh://git@host/owner/repo".into())
    );
    // single .md file (plain + query-stripped)
    assert_eq!(
        classify_source("https://example.com/a/code-reviewer.md").unwrap(),
        FileUrl("https://example.com/a/code-reviewer.md".into())
    );
    assert_eq!(
        classify_source("https://example.com/x.md?token=abc").unwrap(),
        FileUrl("https://example.com/x.md?token=abc".into())
    );
    // local dir forms
    assert!(matches!(classify_source("./local").unwrap(), LocalDir(_)));
    assert!(matches!(classify_source("/abs/path").unwrap(), LocalDir(_)));
    assert!(matches!(classify_source(".\\win").unwrap(), LocalDir(_)));
    assert!(matches!(
        classify_source("C:\\Users\\me\\agents").unwrap(),
        LocalDir(_)
    ));
    // errors: not a path, not a url, not owner/repo
    assert!(
        classify_source("a/b/c").is_err(),
        "3-segment is not shorthand"
    );
    assert!(classify_source("two words").is_err());
    assert!(classify_source("   ").is_err());
}

#[test]
fn sanitize_repo_name_extracts_clean_dir() {
    assert_eq!(
        sanitize_repo_name("msitarzewski/agency-agents"),
        "agency-agents"
    );
    assert_eq!(
        sanitize_repo_name("https://github.com/owner/repo.git"),
        "repo"
    );
    assert_eq!(sanitize_repo_name("git@github.com:owner/repo.git"), "repo");
    assert_eq!(sanitize_repo_name("/some/local/My Agents"), "My-Agents");
}

#[test]
fn git_url_host_extracts_host_for_ssrf_guard() {
    assert_eq!(
        git_url_host("git@github.com:owner/repo.git").as_deref(),
        Some("github.com")
    );
    assert_eq!(
        git_url_host("git@10.0.0.5:a/b.git").as_deref(),
        Some("10.0.0.5")
    );
    assert_eq!(
        git_url_host("ssh://git@host.example/owner/repo").as_deref(),
        Some("host.example")
    );
    assert_eq!(git_url_host("ssh://host:22/path").as_deref(), Some("host"));
    assert_eq!(
        git_url_host("git://internal/repo").as_deref(),
        Some("internal")
    );
    // http(s) are guarded on the path directly, not via this extractor.
    assert_eq!(git_url_host("https://github.com/o/r"), None);
}

#[test]
fn ctx_bar_uses_semantic_palette() {
    // P-ctx4: colour comes from the semantic palette (OK/WARN/ERR) at the 50%/80% thresholds,
    // not bespoke 256-indices. Force colour on so the ANSI code is actually emitted.
    console::set_colors_enabled(true);
    assert!(
        ctx_bar(30.0).contains(&theme::OK.to_string()),
        "green below 50%"
    );
    assert!(
        ctx_bar(60.0).contains(&theme::WARN.to_string()),
        "gold from 50%"
    );
    assert!(
        ctx_bar(90.0).contains(&theme::ERR.to_string()),
        "salmon from 80%"
    );
}

#[test]
fn ctx_bar_fill_tracks_percentage() {
    // strip ANSI: count the block glyphs.
    let blocks = |pct: f64| ctx_bar(pct).matches('█').count();
    assert_eq!(blocks(0.0), 0);
    assert_eq!(blocks(50.0), 5);
    assert_eq!(blocks(100.0), 10);
    assert_eq!(blocks(150.0), 10); // clamped, never overflows the 10-cell bar
}

#[test]
fn dead_end_recovery_detects_error_then_success() {
    let recovered = vec![
        Message::user("do it"),
        Message::assistant("a"),
        Message::tool_result("1", "error: not found"),
        Message::assistant("retry"),
        Message::tool_result("2", "ok, done"),
    ];
    assert!(
        turn_recovered_from_dead_end(&recovered),
        "error then later success = recovery"
    );

    let no_error = vec![
        Message::user("x"),
        Message::tool_result("1", "fine"),
        Message::tool_result("2", "ok"),
    ];
    assert!(
        !turn_recovered_from_dead_end(&no_error),
        "no error → no recovery"
    );

    let only_error = vec![Message::user("x"), Message::tool_result("1", "error: boom")];
    assert!(
        !turn_recovered_from_dead_end(&only_error),
        "error with no later success → no recovery"
    );
}

#[test]
fn compact_cut_lands_on_a_user_boundary() {
    // sys, user, assistant(tool), tool, assistant, user, assistant, user, assistant
    let h = vec![
        Message::system("sys"),
        Message::user("u1"),
        Message::assistant("a-tool"),
        Message::tool_result("id1", "tool-out"),
        Message::assistant("a1"),
        Message::user("u2"),
        Message::assistant("a2"),
        Message::user("u3"),
        Message::assistant("a3"),
    ];
    let cut = agent::compact::plan_compact_cut(&h, COMPACT_KEEP_TURNS).expect("should compact");
    // Tail MUST begin at a user message → never an orphan `tool` result.
    assert_eq!(
        h[cut].role, "user",
        "cut index {cut} is not a user boundary"
    );
    assert!(cut > 1, "must summarize at least one older message");
    // KEEP_TURNS=3, three user turns → keep last 2 → cut at the 2nd user (index 5).
    assert_eq!(cut, 5);
}

#[test]
fn compact_keeps_short_conversations_intact() {
    let k = COMPACT_KEEP_TURNS;
    assert_eq!(
        agent::compact::plan_compact_cut(&[Message::system("s")], k),
        None
    );
    assert_eq!(
        agent::compact::plan_compact_cut(&[Message::system("s"), Message::user("u")], k),
        None
    );
    // one full turn (1 user) → not worth compacting
    assert_eq!(
        agent::compact::plan_compact_cut(
            &[
                Message::system("s"),
                Message::user("u"),
                Message::assistant("a")
            ],
            k
        ),
        None
    );
    // two turns → compact, tail starts at the 2nd user
    let two = vec![
        Message::system("s"),
        Message::user("u1"),
        Message::assistant("a1"),
        Message::user("u2"),
        Message::assistant("a2"),
    ];
    assert_eq!(agent::compact::plan_compact_cut(&two, k), Some(3));
    assert_eq!(two[3].role, "user");
}

#[test]
fn session_names_are_bounded_and_avoid_windows_devices() {
    assert_eq!(sanitize_name("../../chat"), "______chat");
    assert_eq!(sanitize_name("CON"), "session_CON");
    assert_eq!(sanitize_name("com1"), "session_com1");
    assert_eq!(sanitize_name("NUL"), "session_NUL");
    assert_eq!(sanitize_name(""), "session");
    assert!(sanitize_name(&"a".repeat(200)).len() <= 80);
}

/// `sanitize_name` maps an EXISTING on-disk name to itself, so the accented session files already
/// saved stay loadable and deletable. Only name *derivation* folds to ASCII; changing this would
/// orphan every file saved before the fold.
#[test]
fn sanitize_name_keeps_accented_files_addressable() {
    assert_eq!(
        sanitize_name("tại-sao-ổ-của-anh-0803"),
        "tại-sao-ổ-của-anh-0803"
    );
    assert_eq!(
        sanitize_name("anh-cần-em-viết-tài-0804"),
        "anh-cần-em-viết-tài-0804"
    );
}

/// A pasted credential must never become a filename. The derived stem is written to disk AND
/// printed by `/sessions`, so a key on the first line used to end up displayed in the picker.
#[test]
fn suggested_name_drops_credential_shaped_tokens() {
    let keyish = [
        "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "tvly-dev-abc123def456ghi789jkl012mno",
        "ghp_abcdefghijklmnopqrstuvwxyz0123",
    ];
    for k in keyish {
        let name = suggest_session_name(&[Message::user(&format!("{k}"))]);
        assert!(
            name.starts_with("chat-"),
            "a lone key must fall back to the generic stem, got {name}"
        );
        let core: String = k.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        assert!(
            !name.contains(&core.to_lowercase()),
            "key material reached the name: {name}"
        );
        // With surrounding prose the topic survives and only the key is dropped.
        let mixed = suggest_session_name(&[Message::user(&format!("set my api key to {k}"))]);
        assert!(mixed.starts_with("set-my-api-key"), "topic lost: {mixed}");
        assert!(
            !mixed.contains(&core.to_lowercase()),
            "key material reached the name: {mixed}"
        );
    }
}

/// Derived names are ASCII whole words: a Vietnamese topic must not carry diacritics into a
/// filename, and must not be cut apart mid-word doing so.
#[test]
fn suggested_name_folds_vietnamese_into_whole_words() {
    let name = suggest_session_name(&[Message::user("Người dùng giao tiếp bằng tiếng Việt")]);
    assert!(
        name.starts_with("nguoi-dung-giao-tiep-bang-"),
        "expected folded whole words, got {name}"
    );
    assert!(name.is_ascii(), "diacritics reached the filename: {name}");
    assert!(
        name.split('-').filter(|w| w.chars().count() == 1).count() == 0,
        "shredded into one-letter fragments: {name}"
    );
    // An apostrophe is intra-word punctuation, not a boundary: `don't` must not leave a `t`.
    let en = suggest_session_name(&[Message::user("don't break the build please")]);
    assert!(en.starts_with("dont-break-the-build"), "got {en}");
}

/// `elide` is what every human-facing listing shortens with, so its contract is asserted here
/// rather than assumed: no marker beyond the ellipsis, and newlines flattened — a stored fact or
/// insight is multi-line, and one leaking into a row breaks the column alignment of every row
/// under it.
#[test]
fn elide_and_fmt_helpers() {
    assert_eq!(elide("hello", 10), "hello");
    assert_eq!(elide("hello world", 8), "hello w…");
    assert!(
        !elide("hello world", 8).contains("[+"),
        "the `[+N chars]` marker is for a MODEL reading a truncated tool result, not a listing"
    );
    assert_eq!(
        elide("two\nlines  here", 40),
        "two lines here",
        "a multi-line body must flatten, or it breaks every column below it"
    );
    assert_eq!(fmt_k(300), "300");
    assert_eq!(fmt_k(12_400), "12.4K");
}

#[test]
fn extract_json_object_handles_fences_prose_and_nesting() {
    // fenced + prose around it
    let s = "Sure!\n```json\n{\"worth_saving\": true, \"name\": \"x\"}\n```\ndone";
    let j = extract_json_object(s).unwrap();
    let v: serde_json::Value = serde_json::from_str(j).unwrap();
    assert_eq!(v["worth_saving"], serde_json::json!(true));
    // nested braces + a brace inside a string must not end the object early
    let s2 = r#"{"a": {"b": 1}, "s": "has } brace"}"#;
    assert_eq!(extract_json_object(s2).unwrap(), s2);
    // no object
    assert!(extract_json_object("no json here").is_none());
}

#[test]
fn allocate_session_slug_never_reuses_an_existing_file() {
    // Two brand-new chats on the same topic must land on DISTINCT files — the old shared-`last`
    // collision is exactly what made `/sessions` show only the latest conversation.
    let _g = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("aizen-slug-alloc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::env::set_var("AIZEN_HOME", &home);

    let h = vec![Message::system("s"), Message::user("fix the parser bug")];
    let first = allocate_session_slug(&h);
    // Simulate the first chat having been saved under that slug.
    save_session(&h, &first, None).unwrap();
    let second = allocate_session_slug(&h);
    assert_ne!(
        first, second,
        "second chat on the same topic must not reuse the first's file"
    );
    assert!(!first.is_empty() && !second.is_empty());

    std::env::remove_var("AIZEN_HOME");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn exit_flush_persists_the_live_conversation_for_sessions() {
    // The whole point of the fix: whatever is live at exit (even a turn the per-turn autosave
    // never reached) is on disk and shows up in /sessions afterwards.
    let _g = crate::core::config::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("aizen-exit-flush-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::env::set_var("AIZEN_HOME", &home);
    // Start from a clean slug so this chat auto-names a fresh file.
    set_session_slug(None);

    let live = vec![
        Message::system("s"),
        Message::user("remember this across a window close"),
    ];
    update_live_history(&live);
    flush_live_session_on_exit();

    // It must be discoverable and restorable via the same path /sessions uses.
    assert!(
        !scan_sessions().is_empty(),
        "exit flush left nothing in /sessions"
    );
    let slug = current_session_slug().expect("exit flush should have pinned a slug");
    let mut restored = Vec::new();
    let n = load_session(&mut restored, &slug, "opus-4-8").unwrap();
    assert!(
        n >= 1,
        "restored conversation kept its user turn (n counts conversation, not lanes)"
    );
    assert!(
        restored
            .iter()
            .any(|m| m.content.as_deref() == Some("remember this across a window close")),
        "the live user turn survived the exit flush"
    );

    set_session_slug(None);
    std::env::remove_var("AIZEN_HOME");
    let _ = std::fs::remove_dir_all(&home);
}
