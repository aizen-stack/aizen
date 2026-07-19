# Changelog

All notable changes to **Aizen** (`aizen`, alias `ng`) — the pure-Rust agentic coding CLI.

This repo was extracted from the NextGen monorepo at v0.1.0 (2026-06-27); the detailed pre-0.1.0
development log lives in that monorepo's history.

## [Unreleased]

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
