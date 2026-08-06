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

<img width="1536" height="1024" alt="image" src="https://github.com/user-attachments/assets/7ae1cf10-5785-462d-b6ff-b784470c9fc5" />


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
