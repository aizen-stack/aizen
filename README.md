<div align="center">

<img width="200" height="200" alt="aizen" src="https://github.com/user-attachments/assets/4e38d4f9-29af-4a97-af0e-2c7dd7bdf697" />

### The terminal-native coding agent that actually *lives* on your machine.

**One static binary. No Node. No Python. No Docker. No cloud account.**

Point it at any OpenAI-compatible endpoint and you have a coding partner that reads and edits your
code, runs your shell, verifies its own work, and remembers how *you* like things.

<br/>

[![Latest release](https://img.shields.io/github/v/release/aizen-stack/aizen?style=for-the-badge&label=release&color=6c5ce7)](https://github.com/aizen-stack/aizen/releases/latest)
[![License: Apache 2.0](https://img.shields.io/badge/license-Apache%202.0-00b894?style=for-the-badge)](LICENSE)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-e17055?style=for-the-badge&logo=rust&logoColor=white)](https://www.rust-lang.org/)

![Windows](https://img.shields.io/badge/Windows-0078D6?style=flat-square&logo=windows&logoColor=white)
![Linux](https://img.shields.io/badge/Linux-333?style=flat-square&logo=linux&logoColor=white)
![macOS](https://img.shields.io/badge/macOS%20(Apple%20Silicon)-000?style=flat-square&logo=apple&logoColor=white)
![Zero deps](https://img.shields.io/badge/runtime%20deps-0-brightgreen?style=flat-square)
![34 MB](https://img.shields.io/badge/binary-34%20MB-6c5ce7?style=flat-square)
![10 ms](https://img.shields.io/badge/startup-10%20ms-6c5ce7?style=flat-square)

</div>

https://github.com/user-attachments/assets/45bbdfc8-09a3-4995-870f-eb92452743c9

---

## Install

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/aizen-stack/aizen/main/install.ps1 | iex
```

```bash
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/aizen-stack/aizen/main/install.sh | sh
```

Then open a new terminal:

```bash
aizen config     # base URL → API key → pick a model
aizen            # land in the REPL and start typing
```

That's the whole setup. No env vars, no config file to hand-edit.

<sub>Prefer to do it by hand? Grab a binary from the
[latest release](https://github.com/aizen-stack/aizen/releases/latest) — or build it yourself with
`cargo install --git https://github.com/aizen-stack/aizen`. Upgrade or roll back any time with
`aizen update`. The Windows `.exe` is unsigned, so SmartScreen will ask: *More info → Run anyway*.</sub>

## Why Aizen

|  | |
|---|---|
| **One file, no runtime** | 34 MB static binary, ~10 ms cold start. No Node, no Python, no 2 GB virtualenv. Runs on a 512 MB VPS, a scratch container, a CI runner, or a Pi. |
| **Bring your own model** | Any OpenAI-style `/chat/completions` endpoint — OpenAI, OpenRouter, a local llama.cpp/vLLM, an Anthropic gateway. Never locked to one lab. |
| **It finishes the job** | Reads, edits, runs your shell — then **verifies before claiming done**: it runs your typecheck and tests, and fixes what it broke. |
| **It remembers you** | An offline, BM25-ranked memory brain that learns from reuse — plus a persona, a durable SOUL identity, and skills it writes for itself after real work. |
| **It runs where you aren't** | `aizen serve` drives the agent from Telegram or Discord and asks your phone to approve risky edits. Host it on systemd, Docker, or Kubernetes — behind NAT, no inbound port. |
| **Safe by construction** | Tools are confined to the working directory, secrets are owner-only and never printed, and a hard command floor refuses catastrophic commands **even under auto-approve**. |

## What it can do

```
  aizen agent "fix the failing parse test"

  ⚙ search_files  "fn parse_config"        3 hits
  ⚙ file_read     src/config.rs            142 lines
  ⚙ multi_edit    src/config.rs            3 edits
  ⚙ shell_run     cargo test               ✓ 0 failed · 1.18s
                                           verify gate passed
```

| **Unified REPL** | One chat + agent loop, no mode switch. Live HUD: model · tokens · turn · `% context`. Markdown, tables, diagrams, image input. |
| **Agent loop** | Parallel reads, approval-gated writes, LSP-powered symbolic edits, sub-agent dispatch, and a verify gate that must pass before "done". |
| **Multi-agent** | `aizen workflow` fans out role-scoped sub-agents and synthesises one answer. |
| **Web + browser** | Search, fetch, and a katana-style crawler — all SSRF-guarded. Opt-in CDP tools drive a real Chrome. |
| **Extensible** | MCP servers (stdio/HTTP, OAuth 2.1), markdown slash-command macros, outbound notify channels. |
| **Recoverable** | Git-backed checkpoints — `/timemachine` rewinds a bad turn. |

**→ [Full reference](docs/REFERENCE.md)** — every command, the REPL surface, self-hosting, MCP,
browser tools, and the safety model in detail.

## Comparison

> **Benchmark environment for performance rows**
>
> * Windows 11 Pro x64
> * Same machine and disk
> * Same benchmark session
> * Measured on the listed versions only
> * `—` means not benchmarked, not unsupported

| Feature                       |                                **Aizen**                               |          **OpenCode**          |                **Aider**               |           **OpenHands**          |                   **Hermes Agent**                  |
| ----------------------------- | :--------------------------------------------------------------------: | :----------------------------: | :------------------------------------: | :------------------------------: | :-------------------------------------------------: |
| Open Source                   |                                    ✅                                   |                ✅               |                    ✅                   |                 ✅                |                          ✅                          |
| License                       |                               Apache-2.0                               |               MIT              |               Apache-2.0               |                MIT               |                         MIT                         |
| Runtime                       |                                **Rust**                                |        TypeScript + Bun        |                 Python                 |              Python              |                        Python                       |
| Standalone Binary             |                             ✅ Single binary                            |      ✅ Precompiled binary      |                    ❌                   |                 ❌                |                          ❌                          |
| Runtime Dependencies          |                                **None**                                |              None              |                 Python                 |          Python / Docker         |           Python + Node + additional tools          |
| Install Size*                 |                              **33.2 MiB**                              |            167.2 MiB           |                    —                   |                 —                |                       2.25 GiB                      |
| Startup Time*                 |                                **13 ms**                               |             563 ms             |                    —                   |                 —                |                        843 ms                       |
| Peak RAM*                     |                              **14.2 MiB**                              |            194.1 MiB           |                    —                   |                 —                |                       76.9 MiB                      |
| Model Support                 |                     Any OpenAI-compatible endpoint                     | 75+ providers and local models |             Multi-provider             |          Multi-provider          |          300+ models / compatible endpoints         |
| Persistent Memory             |                       ✅ **BM25 memory subsystem**                      |       ❌ Rules files only       | ❌ No native long-term memory subsystem | ✅ Agent memory / workspace state | ✅ **FTS5 session search + curated memory + Honcho** |
| Memory Structure              | Bi-temporal facts, learned profile, abstaining Q&A, co-retrieval graph |    `AGENTS.md` / rules files   |     Repository map and chat history    |     Workspace and agent state    |  Curated memories, session retrieval and user model |
| Memory Across Sessions        |                                    ✅                                   |                ❌               |    ⚠️ Session / repository dependent   |                 ✅                |                          ✅                          |
| Durable Agent Identity        |                         ✅ `SOUL.md` + personas                         |                ❌               |                    ❌                   |   ⚠️ Configurable agent prompts  |                     ✅ `SOUL.md`                     |
| Codebase Retrieval            |                     ✅ BM25 memory + codebase index                     |  ⚠️ File search, grep and LSP  |            ✅ Repository map            |        ✅ Workspace search        |                ✅ FTS5 session search                |
| LSP Integration               |                      ✅ **Built in and default ON**                     |    ⚠️ Built in, default OFF    |    ❌ No native LSP semantic editing    |     ⚠️ Environment dependent     |                ✅ Multiple LSP servers               |
| Semantic Symbol Lookup        |                                    ✅                                   |           ⚠️ Partial           |                    ❌                   |                ⚠️                |                          ⚠️                         |
| Semantic Code Editing         |                 ✅ **Replace or insert code by symbol**                 |    ⚠️ Primarily diagnostics    |       ❌ Text / diff-based editing      |         ⚠️ Tool-dependent        |                ⚠️ Diagnostics-focused               |
| Symbol-Level Replace          |                           ✅ `symbol_replace`                           |           ❌ / limited          |                    ❌                   |                ⚠️                |                          ❌                          |
| Post-Edit Diagnostics         |                                    ✅                                   |      ✅ When LSP is enabled     |          ⚠️ Via external tools         |                 ✅                |                          ✅                          |
| Verify Before Completion      |                 ✅ Enforced typecheck gate in agent loop                |     ⚠️ Prompt / hook driven    |             ⚠️ Model driven            |          ⚠️ Agent driven         |                ⚠️ Agent / hook driven               |
| Sub-Agents                    |                                    ✅                                   |                ✅               |                    ❌                   |                 ✅                |                          ✅                          |
| Parallel Multi-Agent Workflow |                              ✅ `workflow`                              |       ⚠️ Task sub-agents       |                    ❌                   |                 ✅                |                          ✅                          |
| Multi-Window Coordination     |                   ✅ Status, diff, claims and commits                   |                ❌               |                    ❌                   |        ⚠️ Workspace based        |            ⚠️ Kanban / task coordination            |
| Git Worktree per Task         |                                    ✅                                   |                ❌               |                    ❌                   |                ⚠️                |                          ⚠️                         |
| MCP                           |                       ✅ Stdio / HTTP + OAuth 2.1                       |            ✅ + OAuth           |          ⚠️ Version dependent          |                 ✅                |                  ✅ + tool filtering                 |
| Checkpoints / Undo            |                        ✅ Git-backed time machine                       |      ✅ `/undo` and `/redo`     |          ✅ Git-based workflow          |                 ✅                |                    ✅ `/rollback`                    |
| Browser Automation            |                       ✅ Optional CDP integration                       |       ⚠️ Via MCP / tools       |                ❌ Native                |                 ✅                |                     ✅ Playwright                    |
| Scheduled Tasks               |                   ✅ OS scheduler, no daemon required                   |                ❌               |                    ❌                   |      ⚠️ Deployment dependent     |                   ✅ Built-in cron                   |
| Remote Control                |                          Telegram and Discord                          |    ❌ Native chat integration   |                    ❌                   |    Web UI / remote deployment    |               20+ messaging platforms               |
| HTTP API / Server             |                                    ❌                                   |                ✅               |                    ❌                   |                 ✅                |                          ✅                          |
| Desktop / Web UI              |                             ❌ Terminal only                            |     ✅ Desktop, IDE and web     |             ❌ Terminal only            |             ✅ Web UI             |                  ✅ Desktop and web                  |
| Fixed Prompt Overhead*        |                           ≈14.6k tokens fresh                          |          ≈7.8k tokens          |                    —                   |                 —                |                    ≈16.8k tokens                    |
| Advertised Tools*             |                                 **43**                                 |               12               |                    —                   |                 —                |                          28                         |
| Average Schema Size / Tool*   |                                **901 B**                               |             1,905 B            |                    —                   |                 —                |                       1,636 B                       |

> **Notes**
>
> * Performance and prompt measurements marked with `*` are local measurements from the tested versions and configurations.
> * Aizen's prompt grows as persona, memory and skills accumulate.
> * OpenCode has the smallest measured fixed prompt, while Aizen exposes a much larger built-in tool surface.
> * “Semantic editing” means modifying code using symbols or language-server knowledge, not simply asking the model to search and replace text.
> * Feature support may change between releases. Keep version numbers next to the benchmark in the README.


## Contributing

Issues and PRs are welcome. There is **no CLA** — contributions come in under Apache-2.0 §5, and we
only ask you to sign off your commits (`git commit -s`). See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

**[Apache License 2.0](LICENSE)** — open source, commercial use allowed. Keep the license and
copyright notices, state your changes, and pass along the [NOTICE](NOTICE) file. Includes an express
patent grant (§3).

"Aizen" and the logo are trademarks of the Aizen authors; §6 grants no trademark rights, so a fork
must not present itself as Aizen. Releases up to v0.5.5 shipped under PolyForm Noncommercial;
everything after is Apache-2.0.
