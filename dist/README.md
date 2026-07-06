# Aizen

**Aizen** is a single-binary, terminal-native **agentic coding CLI** — pure Rust, rustls-only,
OpenAI-compatible. Point it at any OpenAI-style `/chat/completions` endpoint (OpenAI, OpenRouter, a
local server, or an Anthropic-backed gateway) and you get a streaming chat + tool-using agent loop,
a self-learning memory brain, sub-agent dispatch, and lightweight multi-agent workflows — from one
static executable with no Node, no Python, and no external tools to install.

This repository distributes the **prebuilt `aizen` binaries** and the install scripts. The command
is **`aizen`**. (The source code lives in a separate private repository — this repo is the download
channel only.)

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
aizen            # launch the REPL
```

Override the install directory with `$env:AIZEN_INSTALL` (Windows) or `$AIZEN_INSTALL` (Unix). The
Windows `.exe` is unsigned — if SmartScreen warns, choose **More info → Run anyway**.

### Or download a binary by hand

Grab the asset for your platform from the [latest release](https://github.com/dawnofcd/aizen/releases/latest):

| Platform | Asset |
|---|---|
| Windows x86-64 | `aizen-<ver>-windows-x86_64.exe` |
| Linux x86-64 | `aizen-<ver>-linux-x86_64` |
| macOS Apple Silicon | `aizen-<ver>-macos-aarch64` |

On Linux/macOS make it executable: `chmod +x aizen-* && ./aizen-…`.

## What you get

- **Unified chat + agent REPL** — just type; a plain message is answered, a task that needs tools
  uses them. Sticky-footer TUI with a live slash palette, streaming markdown, and a `% context` HUD.
- **Self-learning memory brain** — one-fact-per-file markdown store, BM25 retrieval, passive
  learning, reuse-based ranking; fully offline.
- **Layered identity** — Soul (operating policy) → Persona (character + evolving self-memory) →
  user memory → skills.
- **Delegation** — `task` sub-agents (coder / tester / planner / reviewer) and `workflow` fan-out.
- **Extensible** — MCP servers/apps (GitHub, Notion, Slack, Linear…), user skills, custom slash
  commands.
- **Multi-surface** — interactive TUI, one-shot commands, Telegram/Discord bots (approvals to your
  phone), and OS-scheduled cron jobs.
- **Type-aware** — optional LSP navigation + diagnostics (rust-analyzer / pyright /
  typescript-language-server).
- **Safety-first** — a hard command floor that survives `/yolo`, tiered approval, an SSRF floor on
  web tools, and cwd-confined file/shell tools.

## Quick start

```bash
aizen config                     # interactive: endpoint, API key, model
aizen                            # REPL — just type
aizen "summarize this repo"      # one-shot
aizen serve                      # run the Telegram/Discord bot surface
```

Configuration lives under your user config dir; API keys never leave your machine (they go only to
the endpoint you configure).

## License

MIT — see [LICENSE](LICENSE).
