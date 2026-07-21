# Changelog

All notable changes to **Aizen** (`aizen`, alias `ng`) — the pure-Rust agentic coding CLI.

This repo was extracted from the NextGen monorepo at v0.1.0 (2026-06-27); the detailed pre-0.1.0
development log lives in that monorepo's history.

## [0.4.3] — 2026-07-21

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
  profile + dialectic, and an opt-in fuzzy/dense tier (`NG_MEM_FUZZY` / `NG_MEM_DENSE`).
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
