<div align="center">

# ⚡ Aizen

### The terminal-native coding agent that actually *lives* on your machine.

**One static binary. No Node. No Python. No Docker. No cloud account.**
Point it at any OpenAI-compatible endpoint and you've got a full agentic coding partner in your
terminal — one that reads and edits your code, runs your shell, verifies its own work, remembers how
*you* like things, and keeps grinding after you walk away.

<br/>

[![Latest release](https://img.shields.io/github/v/release/dawnofcd/aizen?style=for-the-badge&label=release&color=6c5ce7)](https://github.com/dawnofcd/aizen/releases/latest)
[![License: PolyForm Noncommercial](https://img.shields.io/badge/license-PolyForm%20Noncommercial-00b894?style=for-the-badge)](LICENSE)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-e17055?style=for-the-badge&logo=rust&logoColor=white)](https://www.rust-lang.org/)

![Platforms](https://img.shields.io/badge/Windows-0078D6?style=flat-square&logo=windows&logoColor=white)
![Linux](https://img.shields.io/badge/Linux-333?style=flat-square&logo=linux&logoColor=white)
![macOS](https://img.shields.io/badge/macOS%20(Apple%20Silicon)-000?style=flat-square&logo=apple&logoColor=white)
![Zero deps](https://img.shields.io/badge/runtime%20deps-0-brightgreen?style=flat-square)
![Single binary](https://img.shields.io/badge/install-1%20binary-6c5ce7?style=flat-square)

<br/>

**`irm https://raw.githubusercontent.com/dawnofcd/aizen/main/install.ps1 | iex`** &nbsp;·&nbsp; Windows
**`curl -fsSL https://raw.githubusercontent.com/dawnofcd/aizen/main/install.sh | sh`** &nbsp;·&nbsp; Linux / macOS

</div>

---

> The command is **`aizen`**. This repo is the **download channel** — prebuilt binaries + install
> scripts only. The source lives in a separate private repo; everything here is generated.

## 📖 Table of contents

- [Why Aizen](#-why-aizen)
- [See it move](#-see-it-move)
- [Install in 60 seconds](#-install-in-60-seconds)
- [Quick start](#-quick-start)
- [What's in the box](#-whats-in-the-box)
- [Security posture](#-security-posture)
- [Requirements](#-requirements)
- [FAQ](#-faq)
- [License](#-license)

## 🎯 Why Aizen

Most "AI coding tools" are a cloud subscription wearing a CLI. Aizen is the opposite: **a single
executable you own**, wired to a model *you* choose, that treats finishing the task as the whole job.

| | |
|---|---|
| 🪶 **Zero-friction install** | One self-contained executable — no runtime, no containers, no `npm i -g` dependency swamp. Pure-Rust with rustls-only TLS, so there's no OpenSSL or native toolchain to fight. Grab the binary, run it. |
| 🔌 **Bring your own model** | Works with *any* OpenAI-style `/chat/completions` API — OpenAI, OpenRouter, a local llama.cpp / vLLM server, or an Anthropic-backed gateway. You pick the provider; you're never locked to one lab. Context window auto-detects, cost is tracked live. |
| ✅ **It actually finishes** | A real tool-using loop reads/edits files, runs shell, searches the web — and **verifies before it says "done"** (runs your typecheck/tests, fixes what broke). Parallel reads; serial, approval-gated writes. |
| 🧠 **It remembers you** | A self-learning memory brain, an evolving persona, a durable operating **SOUL**, and reusable skills — so the agent gets *more* useful over time instead of resetting every session. |
| 📱 **It runs where you don't** | `aizen serve` turns a Telegram (or Discord) bot into a remote for the agent on your machine — approve risky edits from your phone with a tap. Self-host 24/7 as a systemd service on a VPS. |
| 🛡️ **Safe by construction** | Tools are confined to the working dir; a hard safety floor blocks catastrophic commands *even under auto-approve*; secrets are written owner-only and never printed. You own every destructive step. |

## 🎬 See it move

https://github.com/dawnofcd/aizen/raw/main/liveaizen.mp4

## ⚡ Install in 60 seconds

**One line** — grabs the latest optimized binary for your OS and drops `aizen` on your PATH:

```powershell
# Windows (PowerShell — not cmd)
irm https://raw.githubusercontent.com/dawnofcd/aizen/main/install.ps1 | iex
```

```bash
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/dawnofcd/aizen/main/install.sh | sh
```

Then open a **new terminal**:

```bash
aizen config     # base URL → API key → pick a model
aizen            # launch the REPL — just type
```

That's it. After `aizen config`, every command runs with **zero env vars**. Override the install dir
with `$env:AIZEN_INSTALL` (Windows) or `$AIZEN_INSTALL` (Unix). The Windows `.exe` is unsigned — if
SmartScreen warns, hit **More info → Run anyway**.

<details>
<summary><b>Prefer to grab a binary by hand?</b></summary>

<br/>

Download the asset for your platform from the
[latest release](https://github.com/dawnofcd/aizen/releases/latest):

| Platform | Asset |
|---|---|
| Windows x86-64 | `aizen-<ver>-windows-x86_64.exe` |
| Linux x86-64 | `aizen-<ver>-linux-x86_64` |
| macOS Apple Silicon | `aizen-<ver>-macos-aarch64` |

On Linux/macOS, make it executable: `chmod +x aizen-* && ./aizen-…`

</details>

## 🚀 Quick start

```bash
aizen config                                             # interactive: endpoint, API key, model
aizen                                                    # REPL — just type
aizen agent "add a --version flag and update the help"   # one-shot task
aizen serve                                              # run the Telegram / Discord bot surface
```

Config lives under your user config dir (`~/.aizen/`). API keys **never leave your machine** — they
go only to the endpoint you configure.

## 📦 What's in the box

| Area | What you get |
| --- | --- |
| **Unified REPL** | One chat+agent loop (no mode switch), a live status HUD (model · tokens · turn · `% context` bar), a real line editor with history, image/vision input, and a retained full-frame TUI with responsive Markdown, tables, and diagrams. |
| **Agent loop** | Read / edit / glob / search / shell tools · parallel reads · approval-gated writes · a **verify gate** (auto typecheck/test + one fix turn) · `clarify`-don't-guess · sub-agent dispatch (`task`) · LSP-powered symbolic edits. |
| **Memory brain** | Offline, BM25-ranked, Unicode-aware (Vietnamese-safe) retrieval that **evolves from reuse** — no LLM, zero extra tokens. `#text` remembers a fact in one keystroke. |
| **Persona + SOUL + skills** | A swappable **persona** with evolving self-memory (Generative-Agents style), a durable **SOUL** identity above every persona/project, and **skills** the agent loads on demand and *learns* automatically after real work. |
| **Multi-agent** | `aizen workflow` fans out role-scoped sub-agents (mixture-of-agents) and merges results; `/workflows` shows a live registry of running tasks and workflows. |
| **Remote control** | `aizen serve` (Telegram) / `aizen discord serve` — full `/` command menu, multi-bot hosting from one daemon, per-chat context, phone approvals, systemd self-host. Plus OS-scheduled `aizen cron` runs. |
| **Extensibility** | **MCP** servers (stdio / HTTP, OAuth 2.1 sign-in for Linear / Notion / Slack / Gmail / Atlassian) · custom markdown **slash-command macros** · outbound notify channels. |
| **Web + browser** | `web_search` / `web_fetch` / `web_crawl` (katana-style crawler, SSRF-guarded) and opt-in **CDP browser tools** that drive a real Chrome/Edge — all pure-Rust, no headless engine bundled. |
| **Safety + recovery** | Workspace confinement · hard command floor (survives `/yolo`) · owner-only secret files · crash-recoverable Git checkpoints (`/timeline` · `/checkpoint`) · per-turn MCP schema pinning · per-conversation browser isolation. |

## 🛡️ Security posture

- **Least privilege by default.** File and shell tools are confined to the working directory. Writes
  are serial and approval-gated; reads run in parallel.
- **A floor that `/yolo` can't lower.** Even under auto-approve, a hard safety floor refuses
  catastrophic commands (recursive wipes, disk-level ops, and friends).
- **Secrets stay secret.** API keys and tokens are written **owner-only** (0600 / owner-rights ACL)
  and are never printed to logs or the transcript. They travel only to the endpoint you configure.
- **Recoverable by design.** Git-backed checkpoints (`/timeline`, `/checkpoint`) mean an experiment
  is always one command from a clean rollback.

## 🧩 Requirements

- A 64-bit **Windows**, **Linux**, or **Apple-Silicon macOS** machine.
- An **OpenAI-compatible** chat endpoint + API key (or a local server like llama.cpp / vLLM).
- …that's the list. No runtime, no package manager, no container.

## ❓ FAQ

<details>
<summary><b>Do I need an OpenAI account?</b></summary>
No. Any OpenAI-compatible <code>/chat/completions</code> endpoint works — a commercial provider, a
gateway, or a model running locally. You configure the base URL and key once.
</details>

<details>
<summary><b>Where does my code / data go?</b></summary>
Only to the endpoint you point Aizen at. There's no Aizen cloud, no telemetry account, no middleman.
Keys are stored owner-only on your machine.
</details>

<details>
<summary><b>Is it really a single binary?</b></summary>
Yes — pure-Rust, statically linked, rustls-only TLS. No Node, Python, Docker, or system OpenSSL. One
file on your PATH.
</details>

<details>
<summary><b>Windows SmartScreen flagged the download?</b></summary>
The <code>.exe</code> is unsigned. Choose <b>More info → Run anyway</b>. (Or build from source in the
private repo if you'd rather.)
</details>

<details>
<summary><b>Can it keep working while I'm away?</b></summary>
Yep — <code>aizen serve</code> exposes the agent through a Telegram/Discord bot so you can drive it
(and approve risky steps) from your phone. Run it as a systemd service to keep it alive 24/7.
</details>

## 📄 License

**PolyForm Noncommercial 1.0.0** — see [LICENSE](LICENSE). Free for personal, research, and other noncommercial use; commercial use requires a separate license from the author.

<div align="center">
<br/>
<sub>Made for people who live in the terminal. ⚡</sub>
</div>
