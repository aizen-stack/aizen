# Aizen — the terminal-native coding agent that lives on your machine

**One static binary. No Node, no Python, no Docker, no cloud account.** Download `aizen`, point it
at any OpenAI-compatible endpoint, and you have a full agentic coding partner in your terminal — one
that reads and edits your code, runs your shell, verifies its own work, remembers how you like things
done, and keeps working when you leave (drive it from Telegram on your phone).

The command is **`aizen`**. This repository is the **download channel** — prebuilt binaries and the
install scripts. (The source lives in a separate private repo; this repo is generated.)

## Demo

https://github.com/dawnofcd/aizen/raw/main/liveaizen.mp4

## Why `aizen`

- **Zero-friction install.** A single self-contained executable — no runtime, no containers, no
  `npm i -g` dependency tree. Grab the binary, run it. Pure-Rust with rustls-only TLS, so there's no
  OpenSSL or native toolchain to fight.
- **Bring your own model.** Works with *any* OpenAI-style `/chat/completions` API — OpenAI,
  OpenRouter, a local llama.cpp/vLLM server, or an Anthropic-backed gateway. You pick the provider;
  you're never locked to one lab. Context window auto-detects; cost is tracked live.
- **It actually finishes tasks.** A real tool-using loop reads/edits files, runs shell, searches the
  web, and — crucially — **verifies before it claims done** (runs your typecheck/tests and fixes what
  broke). Parallel reads, serial + approval-gated writes.
- **It remembers you.** A self-learning memory brain, an evolving persona, a durable operating SOUL,
  and reusable skills mean the agent gets *more* useful over sessions instead of resetting every time.
- **It runs where you aren't.** `aizen serve` turns any Telegram (or Discord) bot into a remote
  control for the agent on your machine — approve risky edits from your phone with a tap. Host it 24/7
  as a systemd service on a VPS.
- **Safe by construction.** File/shell tools are confined to the working dir; a hard safety floor
  blocks catastrophic commands *even under auto-approve*; secrets are written owner-only and never
  printed. You stay in control of every destructive step.

## Install

**One line** — grabs the latest optimized binary for your OS and puts `aizen` on your PATH:

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/dawnofcd/aizen/main/install.ps1 | iex
```

```bash
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/dawnofcd/aizen/main/install.sh | sh
```

Then open a **new terminal** and run:

```bash
aizen config     # base URL → API key → pick a model
aizen            # launch the REPL — just type
```

That's it — after `aizen config`, every command works with **zero env vars**. Override the install
directory with `$env:AIZEN_INSTALL` (Windows) or `$AIZEN_INSTALL` (Unix). The Windows `.exe` is
unsigned — if SmartScreen warns, choose **More info → Run anyway**.

### Or download a binary by hand

Grab the asset for your platform from the [latest release](https://github.com/dawnofcd/aizen/releases/latest):

| Platform | Asset |
|---|---|
| Windows x86-64 | `aizen-<ver>-windows-x86_64.exe` |
| Linux x86-64 | `aizen-<ver>-linux-x86_64` |
| macOS Apple Silicon | `aizen-<ver>-macos-aarch64` |

On Linux/macOS make it executable: `chmod +x aizen-* && ./aizen-…`.

## Feature map

| Area | What you get |
| --- | --- |
| **Unified REPL** | One chat+agent loop (no mode switch), a live status HUD (model · tokens · turn · `% context` bar), a real line editor with history, image/vision input, and a retained full-frame TUI with responsive Markdown, tables, and diagrams. |
| **Agent loop** | Read/edit/glob/search/shell tools · parallel reads · approval-gated writes · a **verify gate** (auto typecheck/test + one fix turn) · `clarify`-don't-guess · sub-agent dispatch (`task`) · LSP-powered symbolic edits. |
| **Memory brain** | Offline, BM25-ranked, Unicode-aware (Vietnamese-safe) retrieval that **evolves from reuse** — no LLM, zero extra tokens. `#text` remembers a fact in one keystroke. |
| **Persona + SOUL + skills** | A swappable **persona** with evolving self-memory (Generative-Agents style), a durable **SOUL** identity above every persona/project, and **skills** the agent loads on demand and *learns* automatically after real work. |
| **Multi-agent** | `aizen workflow` fans out role-scoped sub-agents (mixture-of-agents) and merges the results; `/workflows` shows a live status registry of running tasks and workflows. |
| **Remote control** | `aizen serve` (Telegram) / `aizen discord serve` — full `/` command menu, multi-bot hosting from one daemon, per-chat context, phone approvals, systemd self-host. Plus OS-scheduled `aizen cron` runs. |
| **Extensibility** | **MCP** servers (stdio/HTTP, OAuth 2.1 sign-in for Linear/Notion/Slack/Gmail/Atlassian) · custom markdown **slash-command macros** · outbound notify channels. |
| **Web + browser** | `web_search` / `web_fetch` / `web_crawl` (katana-style crawler, SSRF-guarded) and opt-in **CDP browser tools** that drive a real Chrome/Edge — all pure-Rust, no headless engine bundled. |
| **Safety + recovery** | Workspace confinement · hard command floor (survives `/yolo`) · owner-only secret files · crash-recoverable Git checkpoints (`/timeline` · `/checkpoint`) · per-turn MCP schema pinning · per-conversation browser isolation. |

## Quick start

```bash
aizen config                     # interactive: endpoint, API key, model
aizen                            # REPL — just type
aizen agent "add a --version flag and update the help"   # one-shot task
aizen serve                      # run the Telegram/Discord bot surface
```

Configuration lives under your user config dir (`~/.aizen/`); API keys never leave your machine (they
go only to the endpoint you configure).

## License

MIT — see [LICENSE](LICENSE).
