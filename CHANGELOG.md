# Changelog

All notable changes to **Aizen** (`aizen`) — the pure-Rust agentic coding CLI.

This repo was extracted from the NextGen monorepo at v0.1.0 (2026-06-27); the detailed pre-0.1.0
development log lives in that monorepo's history.

## [0.5.1] — 2026-07-30

### Added
- **Several aizen windows on one repository can now see each other: `/team` and `aizen team`.** Opening
  aizen in four terminals against one checkout used to leave `git diff` as the only record — the union
  of everyone's work, with no way to tell which window wrote which line, so the window doing the review
  could neither check one task nor commit only it. Each session now publishes what it is doing and
  which files it changed:
  - `/team status` lists every session in the repository — state, task, files touched, overlaps — and
    a startup line names the windows already working when a new one opens. A window whose process is
    gone reads as `abandoned` rather than silently lingering as "working": the owner holds an OS lock
    for its lifetime, so a lock a reader *can* take is proof the owner left.
  - `/team diff <session>` shows what one session changed, `-p` for the patch. Each file is measured
    from the pre-edit checkpoint of the turn that first touched it *in that session*, so the answer
    stays right even after another window overwrites the same file — the earlier pre-image is still a
    blob in that session's own checkpoint tree.
  - `/team claims` shows which session owns each changed path. Two sessions editing one file is
    **warned about once, for both sides, and never blocked** — the second writer is often the point.
  - `/team commit <session>` stages exactly that session's files, prints the review, and commits only
    after an explicit confirmation; `--dry-run` unstages again. It refuses while the session is still
    running unless `--force`, and says plainly when a file is shared, because git tracks content and
    not authorship: committing a shared file carries the other window's changes to it along.
  - `/team task <text>` pins this window's description; otherwise it is taken from the turn's prompt.
    `/team done` marks the window finished so a coordinator may commit it.
  Attribution is exact rather than inferred: the workspace writer lease is already held exclusively
  for the length of a turn, so two aizen sessions on one worktree are serialized at turn granularity.
  Edits made by an external editor, or by git run by hand *while* a turn is in flight, land inside that
  window and are attributed to whoever held the lease — documented rather than silently corrected.
- **Isolated worktrees for sessions that shouldn't share a tree: `aizen work new|list|remove`.** Each
  gets its own linked worktree and `aizen/<name>` branch, and shows up in `/team status` alongside
  shared-tree sessions. `work remove` refuses while a worktree holds uncommitted changes, unmerged
  commits, or a live session, and keeps the branch either way, so nothing it removes takes commits with
  it.
- **`team_status` tool**, so the agent can check who else is editing a file before it rewrites it.
- **A whole-file overwrite can no longer discard another window's work silently.** The CAS behind
  every write compares against a fingerprint taken microseconds earlier inside the same call, so it
  closes the read→write window and nothing wider. The torn cycle it cannot see is the one that
  matters across windows: session A reads a file, session B rewrites it a turn later, A writes from a
  stale idea of the content. `file_edit`/`multi_edit` survive that on their own — `old_string` is
  matched against a fresh read, so a rewritten region simply fails to match — but `file_write` had no
  anchor at all and its CAS passed against whatever was on disk. Every read now records what this
  session saw, and a full overwrite refuses when disk no longer matches it, naming the re-read as the
  recovery. A file this session never read is refused only when a *live* peer session claims it, so
  single-window flows that legitimately regenerate a file are untouched. `file_move --overwrite`,
  which has no CAS of its own, gets the same guard.
- **A sub-agent that writes no longer deadlocks against its own parent.** The workspace lease is held
  for the length of a turn, and both `LockFileEx` and `flock` conflict with a second handle in the
  *same* process — so once a turn had written anything, a delegated write-capable `task` waited out
  its 15-second timeout and failed with "workspace writer lease was not acquired" for an edit nothing
  was contending. Nested acquisition inside one process is now reference-counted: two nested holders
  are one writer as far as any other process can tell, and the OS lock is released only when the
  outermost handle drops, so cross-session exclusion is unchanged.
- **`/team commit` holds the lease while it stages, and commits only what was reviewed.** `git add`
  reads the working tree, so staging while a peer window is mid-turn could capture half of an
  in-flight edit. Staging now takes the lease for that step — released before the confirmation
  prompt, since blocking every window for as long as a human takes to decide is worse than the race
  it would close. The gap that leaves is closed by identity instead: the index is hashed at review
  time and the commit refuses if it no longer matches, so content nobody looked at cannot ride along.
- **A file two sessions both edited can now be committed for one of them alone.** Previously this was
  the documented limit of the feature: git tracks content, not authorship, so committing a shared file
  carried the other window's changes to it along, and all `/team commit` could do was warn. It is now
  separated instead. The starting point is the last commit — deliberately *not* this session's pre-edit
  checkpoint, which already contains the peer's work whenever the peer edited the file first — and only
  this session's own turns are replayed onto it, one three-way merge each. Those turns are recoverable
  because writes are already serialized: the workspace lease is held for a whole turn, so between one
  turn's pre-edit checkpoint and the next turn's anywhere in the worktree lies the work of exactly one
  session. The reconstruction is staged **index-only**: disk keeps the union, so the peer's live edits
  are never overwritten, and the commit still holds one session's version. Where the two genuinely
  collide, the file is refused with the reason rather than merged on a guess — and since git's merge
  granularity is the hunk rather than the line, edits on adjacent lines count as colliding. A refusal
  rolls the whole stage back, because a half-separated index is the one state where `--force` would
  commit a mix nobody chose. Checkpoints that have been pruned away are reported the same way: without
  a turn's pre-image there is no way to know what it changed, and guessing would silently drop work.
- **Setup verifies the connection instead of trusting it.** `/config` → Connection and first-run setup
  now check each step against the live endpoint before accepting it:
  - A **provider picker** (OpenAI · Anthropic · OpenRouter · Groq · DeepSeek · Ollama · Custom
    gateway) fills in a base URL that already carries the right version path, plus a link to where
    that provider's keys live. The Custom-gateway row prints the endpoint you are already on when it
    matches no preset, so a self-hosted or proxy user sees their own URL instead of only everyone
    else's.
    Anthropic is reachable because it serves an OpenAI-compatible surface at
    `https://api.anthropic.com/v1/`; its native `GET /v1/models` wants `x-api-key` +
    `anthropic-version`, which is now sent alongside the Bearer token when the host is theirs.
  - A **base URL is probed before the key is asked for**, so a bad URL is never mistaken for a bad
    key. A 404 on `{base}/models` also gets diagnosed: when the URL has no `v<N>` segment, the `/v1`
    form is offered as the next default. `/v1beta` counts as versioned — suggesting `/v1beta/v1` would
    point at nothing.
  - An **API key is verified and re-asked on rejection**. Only a 401/403 re-prompts; an unreachable
    endpoint says so and offers to keep the key, since the key may be fine and only the network at
    fault.
  - **Web-search keys (Tavily, Jina) are verified with a real search.** A rejected key re-prompts, and
    an exhausted-quota 429 counts as rejected — a key that cannot serve a search is not "fine".
  - A **spinner** runs during each check, and each step reports its own verdict.
- **`/config` → Memory**, a section of its own: auto-learn, which retrieval tiers actually run, and
  which embedding model to use (or `auto`). It reports the tier that will really run rather than the
  config flag, since the cargo feature, `AIZEN_MEM_DENSE`, and whether a model is installed all get a
  vote. New `embed_model` config field, overridden by `AIZEN_EMBED_MODEL`. `config show` gained the
  matching Memory section and now lists the Jina key too.
- **Container hosting: `Dockerfile`, `docker-compose.yml`, and `deploy/k8s/`.** Two-stage build
  (`rust:1.96-slim-bookworm` → `debian:bookworm-slim`, glibc because the release binary is glibc-linked)
  producing a non-root image with the runtime the agent actually shells out to: `git`, `curl`, and
  `tini` as PID 1 to reap the builds, tests, and language servers it spawns. No port is published,
  because the daemon listens on nothing — Telegram is long-poll, Discord an outbound websocket — so
  the container needs no ingress and no public URL.
  The Kubernetes manifests are a StatefulSet with **one** replica and a ReadWriteOnce volume, and that
  is the only correct shape rather than a starting point: Telegram allows one `getUpdates` poller per
  token (a second gets 409 Conflict forever), the on-disk stores guard writes with local file locks
  that do not coordinate across pods, and turns are processed serially so approval routing stays
  race-free. Ships with a NetworkPolicy that denies all ingress and all *private* egress — including
  `169.254.169.254`, the cloud metadata endpoint that hands out node credentials. That policy is the
  layer that matters, because `net_guard`'s SSRF floor only covers the web tools and the agent also
  has a shell, where `curl` never passes through it.
- **`aizen serve --health`**, a liveness probe for container and orchestrator use. It reads a heartbeat
  file the daemon stamps from inside its own `select!` loop and exits 0 or 1. The beat comes from the
  loop itself on purpose: a detached ticker would keep beating while the loop was wedged, which is the
  one failure a liveness probe exists to catch. The heartbeat records `idle` vs `busy` rather than a
  bare timestamp, so a probe tight enough to notice a wedge (90s idle) does not restart a pod running
  a legitimate 10-minute build (1800s busy, both tunable via `AIZEN_HEALTH_MAX_*_SECS`). An unknown
  state falls back to plain freshness, so an old binary probing a newer daemon mid-rollout does not
  kill a healthy pod.
- **Reading a whole source file suggests the cheaper symbolic path.** A `file_read` that pulls an
  entire code file (a language server exists for its extension, LSP on, 400–2000 lines) now prepends
  one advisory line: if you need a single item, `lsp_document_symbols` then `read_symbol` costs a
  fraction and keeps the context lean, because that whole file is re-sent every turn. It is a hint,
  not a gate — the full content still follows, since reading the whole file is sometimes exactly
  right. It stays silent for prose files, when LSP is off, and below the threshold where the read is
  small enough not to matter.

### Changed
- **API keys are visible while you type them** (setup, Connection, and the search keys). A masked
  field hides a truncated paste or a stray newline, and the value is one keystroke from a plaintext
  config file either way — so the echo was buying nothing. Stored keys are still masked everywhere
  they are displayed back.
- `memory_auto_learn` moved from the Session section to Memory, so it is asked once, next to the
  retrieval knobs it feeds.

### Fixed
- **`aizen serve` ignored SIGTERM.** Only Ctrl-C was handled, and SIGTERM is how systemd, `docker stop`,
  and Kubernetes all ask a process to stop first — so the graceful path never ran and the daemon was
  killed outright some seconds later. Every child it had spawned (builds, test runners, language
  servers) was orphaned rather than reaped, and the heartbeat file was left behind claiming a live
  process. It now handles both, logs which signal arrived, kills the process tree, and clears the
  heartbeat before exiting. Windows has no SIGTERM; there `ctrl_c()` covers Ctrl-C and Ctrl-Break,
  which is what the platform offers.
- **The agent stopped mid-task instead of finishing it.** Three independent holes in the top-level
  loop, all of which surfaced the same way — partial work handed back with "say continue to carry
  on". A fix landed for sub-agents earlier; the loop the REPL actually drives still had all three.
  - **The step cap cut off runs that were still working.** Exhausting the budget granted one
    extension (25 → 50 steps) and then hard-stopped, synthesizing a "here's how far I got" summary.
    A large but *healthy* task — one producing new evidence every turn — was ended by a step counter
    rather than by anything about the task. It now grants itself another `max_iters` worth (up to
    `max_continuations`, default 3) when the run is neither stalled nor looping, injecting a
    `[continue]` turn that says to carry on from where it is rather than restart or re-plan. Bounded
    by `auto_extend_to + max_continuations × max_iters`: a run that goes flat or starts repeating
    itself loses the grant immediately, so "don't stop early" cannot become "never stop".
  - **The stall guard ended runs with work still declared open.** A few turns without new evidence
    stopped the run even when the model's own todo list still held pending items — abandoning
    exactly the work it had said was unfinished. With an open plan it now spends one bounded recovery
    (`max_stall_recoveries`, default 1) demanding a genuinely different approach; the ledger keeps
    what it had already ruled out, so a re-read cannot pose as fresh evidence. Going flat a second
    time still stops.
  - **One transient API error discarded the whole turn.** The top level ran with zero retries on the
    reasoning that the user is right there to re-ask — but a 429/5xx twenty steps in threw away all
    twenty. It now absorbs a small number (2, versus 4 for unwatched sub-agents, since each attempt
    prints a retry line and reads as progress rather than a hang). Permanent 4xx still fails fast.
  Continuation alone does not cover a model that stops early *on its own* — the loop sees a clean
  `Done` — so both system prompts gained a persistence clause: a large task is a reason to keep
  working, and a plan or an outline is not a result.
- **The Hugging Face cache was never scanned on Windows.** `scan_hf_cache` reached
  `~/.cache/huggingface/hub` only through `$HOME`, which does not exist on Windows outside a POSIX
  shell — so a model another tool had already downloaded sat there unseen. The home lookup is now
  `USERPROFILE` then `HOME` (matching `aizen_home()`), and the root list follows the precedence
  `huggingface_hub` documents: `HF_HUB_CACHE` → `HF_HOME/hub` → `XDG_CACHE_HOME/huggingface/hub` →
  `<home>/.cache/huggingface/hub`. The old `%LOCALAPPDATA%/huggingface/hub` entry is gone: HF uses
  `~/.cache` on all three platforms, so that path pointed at a directory nothing ever creates.
- **A dense build with no model no longer degrades recall.** `settings().enable_dense` tracked only
  the cargo feature, but with no model2vec model installed the embedder falls back to the
  non-semantic `HashEmbedder` — which then got fused into RRF. Worse, the query gate admits the dense
  tier precisely when lexical coverage is LOW, so the hash embedder was mixed in exactly on the
  ambiguous queries where it does the most damage (the P6 bench measured literal-slice precision
  dropping 0.667 → 0.200). Dense now defaults on only when the feature is built AND a model is
  actually present; `AIZEN_MEM_DENSE` still overrides both ways.
- **Model weights are loaded once per process instead of once per search.**
  `StaticModel::from_pretrained` reads and parses the entire `model.safetensors` (512 MB for
  `potion-multilingual-128M`), and the embedder was constructed on every search — both memory
  retrieval and code retrieval, so twice a turn. The load is now memoized on the weights file's
  identity (path + mtime + length). Discovery itself is still uncached, so a model downloaded
  mid-session is picked up on the next search, and a model replaced in place is reloaded rather than
  served stale.

- **`/import` could not be navigated.** `import` was missing from the one table that decides whether
  a slash command owns stdin, while `/sessions` — the same dialoguer picker — was listed. So the
  retained TUI's input thread kept the keyboard and the picker's arrow keys never reached it: with 240
  transcripts across 9 pages, the list could not be paged at all.
- **Every `/import` row described the harness instead of the conversation.** The subject was taken
  from the first `user` line, but the foreign CLIs write their own text into user turns — so rows read
  `<local-command-caveat>Caveat: The messages below were gener…` (Claude, after any `/compact` or `!`
  command) or `<environment_context> <cwd>C:\Users\…` (Codex, which leads every session with one).
  Envelope turns are now skipped to find what the person actually typed. The row also dropped the turn
  count and the `from <dir>` note that repeated identically on all 240 rows — a subject you can
  recognize is the only column that helps you pick, so it gets the width.

### Added
- `AIZEN_EMBED_MODEL` accepts an absolute path to a model directory, not just a name to look up under
  `~/.aizen/models/`. For a model on a shared drive or outside both search trees.

### Changed
- **One brand, everywhere: every `NEXTGEN_*` / `NG_*` name is now `AIZEN_*`.** The rebrand had only
  reached the paths — the environment variables, the internal API and a pile of doc comments still
  said `nextgen`, so the CLI documented one name and read another. Renamed: `NEXTGEN_HOME` →
  `AIZEN_HOME` (already the preferred spelling, now the only one), `NG_PROJECT_ROOT` →
  `AIZEN_PROJECT_ROOT`, `NG_EMBED_MODEL` → `AIZEN_EMBED_MODEL`, `NG_NO_SCOPE` → `AIZEN_NO_SCOPE`,
  `NG_NO_GRAPH` → `AIZEN_NO_GRAPH`, `NG_GRAPH_EXPAND` → `AIZEN_GRAPH_EXPAND`, `NG_MCP_REGISTRY` →
  `AIZEN_MCP_REGISTRY`, and the per-role / per-model families `NG_<ROLE>_{MODEL,BASE_URL,API_KEY}`
  and `NG_MODEL_<ID>_{BASE_URL,API_KEY}` → `AIZEN_*`.

  **Breaking, deliberately with no alias.** A silent fallback is how a rebrand survives for years:
  the old name keeps working, so nothing ever moves off it. If you had one of these set, rename it —
  an unset variable falls back to the documented default rather than failing quietly.

  Also gone: the `~/.nextgen` and `./.nextgen` legacy-directory fallbacks. These were never reachable
  from a released build — the earliest tag (v0.4.0) already defaults to `.aizen` — so they were
  migration code for a directory no shipped version ever created.
- Internal rename with no user-visible effect: `nextgen_home()` → `aizen_home()`,
  `project_nextgen_dir()` → `project_aizen_dir()`, and the on-disk temp/stash prefixes used during
  atomic writes (`.ng-tmp.` / `.ng-stash.` → `.aizen-tmp.` / `.aizen-stash.`). These temp files exist
  only between a write and its rename, so no cleanup step needs the old spelling.

## [0.5.0] — 2026-07-28

Memory stops being a write-only pile: facts are placed on a tier/anchor axis, recall itself into the
turn, fade instead of accumulating, and every destructive step has a reverse gear. Also the first
in-place upgrade path — `aizen update` / `/update` — so a release no longer means re-running the
installer.

Cut from `feature/memory-tiers`. Contains everything in 0.4.9 (which soaked on its own branch and was
never merged to `main`).

### Added
- **`aizen update` and `/update` — see every published version and install the one you pick.** One
  flag-free command: the picker lists the releases with the running build marked `● installed now`,
  and newer / older / pre-release spelled out on each row, so upgrading and rolling back a bad
  release are the same gesture. The new binary lands on disk immediately while **the session you ran
  it from keeps working on the old one** — the running executable is renamed aside and the download
  takes its place, so nothing is swapped out from under a live process; the new version takes effect
  in the next terminal. Startup does a silent check (cached 24h, `AIZEN_NO_UPDATE_CHECK=1` or
  `update_check: false` to switch off) and mentions a newer version in one dim line.
- **`aizen memory health`** — per-week table over `stats.jsonl` (store growth, saturation, how much
  injected memory actually got used, contradictions found) plus a verdict, and an explicit refusal to
  conclude anything from under four weeks of data.
- **`aizen memory doctor`** — the tier/anchor axis fails quietly by construction: an unanchored place
  fact, an anchor whose directory is gone, or a `supersededBy` naming a purged id all look healthy in
  `memory list` and just subtract from recall. `doctor` names them.
- **`aizen memory reconcile`** (dry-run by default) and **`memory list --superseded`** — the
  graveyard is now browsable, with what replaced each row and the revive command spelled out.
- **`aizen memory revive <id>`** — clears both halves of a supersession (the retired side's pointer
  *and* any live fact's forward `supersedes:` claim), since either one alone keeps the fact hidden.

### Changed
- **`/timemachine` is one command instead of four.** Typing it opens the checkpoint list — `▸` marks
  where you are, each row carries the id, how long ago, the label, and whether picking it rewinds
  `code + chat` or `code only` — and picking a row puts you back in that code and that conversation
  in one gesture. The `pick`/`restore`/`menu` arguments and the Files / Task / Both sub-menu are
  gone: what a checkpoint restores is a property of the checkpoint, not a question to answer twice.
  Still reversible (the pre-restore tree is auto-snapshotted and the live chat is saved to its own
  session file first), Esc still leaves without touching anything, and `/timeline` · `/tm` remain as
  aliases. `aizen time …` on the CLI is unchanged.
- **Memory places facts on a tier/anchor axis** instead of one hashed scope slug: `tier` says what a
  fact is *about* (the person, this machine, a directory tree) and `anchor` says where a place fact
  *applies* (a normalized absolute path, matched segment-safe, nearest ancestor wins). The read
  predicate reads the same axis, so "true here" is decided one way in one place, and an unresolvable
  orphan fails closed rather than leaking into every directory.
- **Relevant facts arrive in the turn without the model calling a tool for them.** Labelled by axis
  (about you / this machine / here) behind a relevance gate, budget-packed, and skipped entirely when
  the selection hasn't changed — a second copy of a fact already in the transcript is just a rival
  claim with nothing marking which is newer. Being *shown* a fact earns nothing; only reporting it as
  used does.
- **Reinforcement saturates instead of growing without bound.** Strength is keyed to confirmations on
  a capped ladder, the idle clock reads *last used* rather than any bookkeeping rewrite, and a fact
  the user typed by hand keeps a floor under its salience instead of being permanently halved for
  never having been searched for.
- **Faded facts are set aside, not deleted** — and the first sweep on any store only *previews* what
  it would move. An upgrade that quietly starts relocating someone's facts on the strength of a
  brand-new formula isn't something they can consent to afterwards.
- **Per-partition memory caps** are bucketed by tier/anchor. Every fact written on the new axis
  carries no scope slug, so a scope-keyed bucket had put the whole store in one pool — one chatty
  project evicting every other project's facts is exactly what per-zone caps exist to prevent.

### Fixed
- **Four hard-delete windows on user data closed.** Persona self-memory pruning, `memory review
  --clear`, and skill writes all moved to archive-or-atomic-write; a crash mid-write could previously
  truncate a skill to zero bytes on any `skill_load`.
- **Retiring a fact no longer drops frontmatter keys this build doesn't model** — supersession
  rebuilt a fixed record shape, so anything unrecognized was silently lost. It edits the field map
  now, like updates do.
- **Restoring an archived fact keeps its id**, and errors asking for `--as <new-id>` on collision
  instead of silently landing as `<id>-2`: a fact that answers searches while being invisible to
  every pointer aimed at it.
- Contradictions phrased in different words are caught by a batched pass off the hot path (at most
  one model call, at most 12 pairs) with asymmetric rails: nothing destructive below a confidence
  floor, and a fact confirmed twice by the user goes to review at *any* confidence rather than being
  overruled automatically. Same-chain ties break on recency, and the survivor re-anchors to the
  common ancestor so resolving *content* never quietly narrows *where* a fact applies.

## [0.4.9] — 2026-07-26

Version-only release cut from `release/v0.4.9`: the 0.4.8 tag had already been published from the
pre-identity-work tree, so the work listed under 0.4.8 below actually shipped as the 0.4.9 binary. It
soaked on its own branch and reaches `main` as part of 0.5.0.

## [0.4.8] — 2026-07-26

### Fixed
- **A project's memory, skills and index no longer fork in two depending on whether `git` was on
  PATH** — the zone key was hashed from the git remote URL when git could be found and from the raw
  path when it couldn't, so the same checkout answered to two different zones from one launch to the
  next and half the user's memory went missing without a word. The key is now the normalized
  canonical project path only; the remote URL is informational. `aizen zone migrate` shows what a
  legacy zone holds (dry-run by default) and merges it on `--apply`, including saved conversations,
  which are keyed by provenance inside each file and so were invisible to a per-directory sweep.
- **A missing `git` no longer blocks editing** — `git` not being on PATH was treated as a hard
  checkpoint failure, which refused every edit rather than degrading. It is now benign: checkpoints
  switch off with one warning and work continues. `git` is resolved once through a central resolver
  (`AIZEN_GIT`, then PATH, then the usual install locations) so a GUI-installed git is found even
  when the shell can't see it.
- **`/resume` no longer grafts one project's context onto another** — sessions live in one flat pool,
  so restoring offered whichever conversation was written last, from any project, and replayed its
  stale system lane into the current one. Session files now record their origin (project key/root/
  slug, model, created/updated); the startup hint and bare `/resume` prefer this project's newest
  conversation and label a cross-project offer with `from <dir>`; restoring rebuilds both prompt
  lanes for the current project. `/handoff` rotates to a fresh file instead of overwriting the
  conversation it summarized.
- **The `/sessions` picker says what each row is** — turn count, age, origin project and a `● current`
  marker, newest first, with a confirmation before overwriting another conversation's file and
  `(unreadable)` for a corrupt one instead of a plausible-looking empty row.
- **A failing autosave is no longer silent** — it warns once per failure streak and reports recovery,
  so a conversation that is not reaching disk says so.

### Added
- `aizen where` and `/where` — print the project root, zone slug, resolved `git`, and the paths
  backing memory, skills, the codebase index and sessions, so which zone is in effect is checkable
  rather than inferred.

### Changed
- **Telegram replies are now native, compact HTML instead of raw Markdown** — headings, emphasis,
  inline/fenced code, safe links, lists and quotes use Telegram's supported formatting; Markdown
  tables become narrow stacked records rather than raw pipes or terminal boxes. Rendering is parsed
  with the existing pure-Rust Markdown engine, escapes raw HTML/unsafe links, chunks only at balanced
  block boundaries, and retries once as equivalent plain text if Telegram rejects rich markup.
- **Telegram working status no longer clutters the chat** — a single `✦ Đang xử lý…` message is
  deleted after the final reply is delivered (or edited to a concise success/error state when delete
  or delivery fails). Hostbot turns also receive a mobile-only response contract: lead with the result,
  keep paragraphs/headings short, prefer bullets, and avoid wide diagrams or decorative repetition.
  Discord remains plain text in this scope.

## [0.4.4] — 2026-07-22

### Changed
- **Transcript visuals redesigned around structured events** — tool calls, the plan checklist, edit
  diffs and the verify line are no longer pre-styled strings blindly emitted; they flow through the
  UI as typed events so the retained backend can lay them out by width and update lines/panels in
  place. Every surface renders from the same layout code, so classic/plain/one-shot read identically
  (degrading only where an append-only surface can't update in place).
  - **Tool-call line** opens with `⚙ <tool_name>   <target>` — the raw tool name in moonlight, its
    target dim silver — and drops the result to an indented `└ <digest> · <time>` line **beneath**
    it: the digest tinted by outcome (dim while running, green on ok, salmon on error) and carrying
    the wall-clock run time (`· 940ms` under a second, `· 1.2s` above). Replaces the old
    `◆ <verb> (tool)` footnote shape; the earlier right-aligned-digest variant is gone (the digest
    now always sits below the call, so a long result never collides with the target).
  - **Plan panel** (`todo_write`) is a boxed checklist — header `☑ done/total · plan`, then ✓/▸/○
    rows — that **updates in place** under retained instead of re-printing a fresh `todos:` block on
    every call. Classic re-prints the box; an emptied list removes the panel.
  - **Edit diffs** render inside a rounded `diff · <path>  +A −D` box (added = green `+`, removed =
    salmon `−`) instead of loose indented lines.
  - **Verify gate** success is a green `✓ <cmd> — <detail>` line.
  - **Footer** — working state shows a live pill `✶ working · Ns · Esc to stop`; the HUD row carries
    `model · ~<used>/<max> tok · <n> turns · <mode>`; the empty prompt shows a right-aligned
    `↵ send · Tab complete` hint. The mode chip keeps its colour (`⚡ yolo` gold, `◆ smart` moonlight).

### Fixed
- **Retained TUI kept no colour** — every non-assistant line (the `❯` user echo, tool anchors, the
  green/salmon edit diff) was run through `strip_ansi_codes` and then repainted one flat grey, so
  you couldn't tell your own message from the model's reply or read an edit's `+`/`−` at a glance.
  The retained backend now keeps SGR colour codes (dropping only cursor moves / erases) and parses
  them into styled ratatui spans at draw time; uncoloured text is unchanged.
- **User chat lines now read as yours** — the whole `❯ …` echo takes the moonlight accent (was just
  the arrow), in both a live turn and a `/sessions` restore replay, so it stands apart from the
  model's grey reply.
- **Empty / failed API turns are surfaced loudly** — a blank turn (rate-limit swallowed into an
  empty 200, content filter, or a gateway that closed the stream early) printed a dim grey aside
  that read like idle. It's now a `⚠ empty reply:` warning naming the likely cause, in both the
  sticky and plain REPLs.
- **Messages typed while the agent works no longer get swallowed** — paste-coalescing keyed off how
  long `read_key` blocked, but a busy output stream makes a hand-typed key arrive pre-buffered, so
  the Enter meant to queue a message became a literal newline and the message sat in the draft
  forever. Detection now measures the gap between successive key *arrivals*, folding the slow
  repaint into the gap so a real keystroke is never mistaken for a paste.
- **Image-attachment chip missing in the retained TUI** — pressing Ctrl-O (clipboard screenshot) or
  dropping an image file attached it correctly (the send carried the image), but the retained input
  box never drew the pending count, so there was no visible confirmation. `draw_footer` now renders
  the `[Nimg]` chip (matching the classic renderer), reserving its width from the typing budget and
  offsetting the caret so it never overlaps the typed text.

### Changed
- **Tool-call anchor glyph** — replaced the florette `✿` with a solid diamond `◆` (moonlight
  silver): a cleaner, more basic mark. The result digest under it still carries state colour
  (green when done, salmon on error).
- **Sun logo reads as a sun at terminal resolution** — the brand's 32 thin-ray chrysanthemum sun
  is faithful in the high-res sixel image, but a low-res braille character grid (the logo on
  launch) shattered those thin lances into scattered `░▒▓` dots that looked like noise.
  `petal_mask` is now resolution-aware: sixel (≥128px) keeps the true 16+16 thin rays; the char
  grid draws a bolder 12-ray sun (thicker lances, larger solid hub).

### Removed
- **Retained idle "3D" spinning-sun animation** — dropped entirely to keep the TUI light: no
  per-frame render thread wakeups when idle, and the now-dead `tui_performance` / `idle_animation`
  config knobs are gone with it. The landing splash logo (static sun) is unchanged.

## [0.4.2] — 2026-07-21

### Fixed
- **Markdown tables no longer dump raw pipes** — the delimiter-row detector required 3+ dashes per
  cell, so the short separators models routinely emit (`|--|`, `|-|`) failed table detection and the
  raw `| … |` rows printed verbatim. Now accepts any GFM-valid delimiter (≥1 hyphen, optional `:`).
- **Code / `diagram` fences render as a true rectangle** — the top and bottom borders were capped
  near 56 columns while body content ran to the full terminal width, so wide content sprawled past a
  narrow frame. All three render paths (streaming, retained, unterminated-fence fallback) now share
  one capped width for the top border, every body row (padded and closed with a right rule), and the
  bottom border, measured by display width rather than byte length.

### Changed
- **Tool-call anchor glyph** — replaced `⏺` (force-rendered as a blue emoji disc by most terminals,
  ignoring the theme color) with the monochrome florette `✿`, which takes the moonlight-silver accent
  and echoes the Aizen chrysanthemum-sun mark.

### Added
- **Retained TUI foundation + runtime safety** — alternate-screen single-owner renderer, structured
  transcript streaming, full-message Markdown cache, retained overlays/panel status, local frame
  metrics, and Mermaid-to-Unicode fallback; classic/plain remain rollback paths.
- **Static/dynamic prompt lanes + crash recovery** — stable project/environment prefix stays cacheable;
  volatile identity/memory refreshes only at fresh user-turn boundaries. Owner-only recovery leases
  restore safe history and prefill interrupted drafts without auto-submitting or replaying tools.
- **MCP lifecycle hardening** — poisoned transport reconnect, one read-only retry only, destructive
  no-replay after ambiguous failure, manager/connection generations, per-turn schema pinning, and
  deferred `tools/list_changed` refresh at the next user turn. MCP trust writes are atomic owner-only.
- **Browser profiles and routing** (`--features browser`) — versioned `~/.aizen/browser.json`, named CDP
  profiles, host routes, environment-reference-only auth attached to HTTP discovery AND the WebSocket
  upgrade, per-conversation session isolation (LRU-capped, released on `/new`/route removal), ref
  invalidation, sanitized `/browser` status, read-only snapshot retry, and no replay of
  navigate/click/type/eval.
- **Terminal-native reply visuals** — configurable `auto|always|off` final-answer contract, responsive
  Markdown tables (boxed wide / stacked narrow), and topology-safe `diagram`/`ascii`/`flow` fences.
  One-shot chat and workflow synthesis share the renderer; Telegram/Discord receive plain stacked
  tables with newline-aware UTF-16 chunking.

## [0.4.1] — 2026-07-20

### Added
- **Harness persistence P0** — incomplete session todos now auto-poke the top-level loop before an
  early text-only exit; confidence spikes at `Done` trigger one evidence re-check; quantifiable
  optimization goals are reframed into metric → baseline → iterate loops. The deterministic loop
  eval suite covers todo-poke, confidence-gate, and hill-climb behavior.
- **Symbolic edit tools** — `symbol_replace` / `symbol_insert` rewrite or insert relative to a
  named symbol via the language-server outline range (Serena-style; no `old_string` thrash).
- **`/workflows` multi-agent status** — process-global live registry of `task` / `workflow` /
  workflow children (phase, elapsed, detail) + sub-agent slot gate (`active/cap`). Open mid-turn
  to watch fan-outs; aliases `/wf`, `/workflow`. Empty state explains how to launch multi-agent work.
- **`StopReason::Cancelled`** — cooperative cancel mid-loop (Esc); nested `task` / workflow children
  stop at the next boundary instead of running to max_iters.

### Changed
- **LSP default ON (lazy)** — manager arms at session start; language servers still spawn only on
  first symbol query. `/lsp off` reclaims RAM; tools reappear after `/lsp on`.
- System prompt prefers outline/definition + symbolic edit over dumping whole files / grep for
  semantic code questions.
- **Multi-agent hardening** — CLI `aizen workflow` shares singular-writer + `SubagentSlot` +
  orchestration Track with the tool path; workflow children use task-like budgets (15/30 steps);
  synthesis truncates child summaries; sub-agents get LSP nav + coder symbolic edit; ultimate
  prompt prefers real `workflow` fan-out/verify.
- **Sticky `/sessions` repair** — the conversation picker now suspends the sticky footer and parks
  the background keyboard reader before dialoguer owns the terminal. Restore/save/delete and
  confirmation no longer corrupt the footer or merge menu lines.

## [0.4.0] — 2026-07-19

### Added
- **Config hub menu** — `aizen config` is a sectioned dashboard (edit one field and save) for
  configured installs; first-run still walks the linear wizard.
- **5-tier effort + `/ultimate`** — effort scale includes `xhigh`/`max`; `/ultimate` = max effort +
  orchestrate-by-default (ultracode analogue); optional adaptive difficulty→effort routing.
- **`file_move` tool** — rename/move file or directory (`overwrite` / `create_dirs`, cross-fs
  fallback); arms the post-edit verify gate.
- **Hostbot (Telegram + Discord)** — multi-bot self-host daemon (`aizen serve`, `/addbot`/`/rmbot`,
  pairing-code owner capture, per-sub-bot persona, `bot_admin` tool, systemd `--install`);
  two-way bots moved out of `channels/` into `src/hostbot/`.
- **Agent run-scoped Time Machine recovery** — auto-anchors `pre_edit` (before first edit) and
  `last_good` (after each successful edit), plus tool `checkpoint_rewind`
  (`target=last_good|pre_edit`, max 2 rewinds per run). Verify-gate failures hint at rewind when an
  approach is cascading-broken. Free-form `aizen time restore <id>` remains human-driven.
- **TUI polish** — sticky sessions menu, pure-print slash text overlay, clearer session-delete
  flow, shimmer on the working-verb line; Windows console mode restored on exit.
- **File discovery rewrite** — bounded parallel `file_glob` / smarter `search_files` / tighter
  `repo_map` (node+wall budgets, no junction loops, fuzzy+proximity ranking, UTF-16 detection).

### Changed
- **Time Machine hardened** — fail-closed versioned ledger, atomic writes, OS cross-process lock,
  recovery journals, CAS refs, per-linked-worktree namespaces. Snapshots live in a **private store
  under `~/.aizen/timemachine/<repo-id>/`** (no longer writes into the source repo's `.git`).
  Internal Git disables hooks/fsmonitor/external filters; restore saves a preimage and verifies the
  tree. Added `aizen time doctor [--json] [--repair]` / `aizen time gc`.
- **Unified approval** — `/approval ask|smart|yolo` replaces the overlapping `/smart` + `/yolo`
  toggles (aliases still accepted). `--yes`/`AIZEN_YES` still mean Yolo; hard `cmd_guard` floor is
  non-overridable.
- **Evolutionary persona self-memory** (lean Generative-Agents × MemoryBank × CoALA × A-MEM):
  event-gated episodes (skip small-talk), typed free notes, near-dup + insight-cover dedup,
  formative-only reflection, insight-first `<self>` injection.
- **Repo + session scoped memory (token-lean):** always-on `<user_memory>` = **STYLE + global
  prefs only**; per-repo frozen-core cache (`cli-memory/core/active/<slug>.md`); inferred facts park
  in **session working memory** (L2, cleared on `/new`); durable long-tail stays zone-tagged via
  `memory_search`. Default core budget 800 tok; session inject cap 300 tok.
- Workflow slot accounting: one subagent slot per concurrent child (`acquire_up_to`), not one per
  whole call; live inline progress for the tool path.
- Edit ladder R5.5: blank-line-insensitive matching rung.

## [0.1.0] — 2026-06-27 — first public release

The first tagged cut of the Aizen CLI: a single pure-Rust static binary (rustls-only TLS, no C/C++
deps in the default build), OpenAI-compatible — point it at any `/chat/completions` endpoint.

### Highlights
- **Unified chat + agent REPL** with a sticky pinned input box, live `% context` HUD, streaming
  Markdown render, multi-line paste coalescing, and image (vision) input.
- **Tool-calling agent loop** — native `tool_calls[]` with divergence self-resolve, one-shot
  auto-extend, a mid-loop context guard, parallel read-only tool batches, and a post-edit verify
  gate (`cargo check` / `tsc`).
- **Self-learning memory brain** (the moat) — BM25 lexical floor with NFC/Vietnamese-aware
  tokenization, reuse-driven evolution, anti-bloat (dedup/supersede/decay/caps), theory-of-mind
  profile + dialectic, and an opt-in fuzzy/dense tier (`AIZEN_MEM_FUZZY` / `AIZEN_MEM_DENSE`).
- **MCP client** (stdio + Streamable-HTTP) with **OAuth 2.1 (PKCE) sign-in apps** (Linear, Notion,
  Slack, Gmail, Atlassian, …) and a curated `aizen apps` catalog over the official registry.
- **Remote control & notifications** — Telegram + Discord two-way bots (`aizen serve` /
  `aizen discord serve`), Discord/Slack/webhook outbound `notify`, and daemon-free `aizen cron`.
- **Skills, personas, SOUL, custom slash commands, time machine** (git snapshots), and a
  katana-style web crawler.

### Safety
- Per-action approval in the TUI (`[y]es · [n]o · [a]llow all this session`), `/yolo` / `/smart`
  tiers, and a hard `cmd_guard` floor (incl. GNU long-flag `rm` root-deletes) that holds even under
  `/yolo`.
- SSRF floor on the web tools (refuses loopback / private / link-local / cloud-metadata targets;
  opt out with `AIZEN_ALLOW_PRIVATE_NET=1`).
- `confine()` cwd jail on file/shell tools; long-lived secret files (config, OAuth/MCP token caches,
  sessions) written owner-only (0600) on Unix.

### Notes
- Optional features (off by default): `--features dense` (semantic embeddings; needs a C++
  toolchain + a local model) and `--features browser` (CDP browser tools, stays pure-Rust).
- Home/data root: `~/.aizen` (override with `AIZEN_HOME`; legacy `NEXTGEN_HOME` honored, and a
  pre-rebrand `~/.nextgen` is auto-migrated on first run).
