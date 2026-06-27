# Changelog

All notable changes to **Aizen** (`aizen`, alias `ng`) — the pure-Rust agentic coding CLI.

This repo was extracted from the NextGen monorepo at v0.1.0 (2026-06-27); the detailed pre-0.1.0
development log lives in that monorepo's history.

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
