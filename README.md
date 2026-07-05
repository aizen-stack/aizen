# `aizen` — Aizen agentic coding CLI

A single-binary, terminal-native coding agent: streaming chat, a tool-using agent loop, a
self-learning **memory brain**, sub-agent dispatch, and lightweight multi-agent workflows.
Pure Rust, rustls-only, OpenAI-compatible — point it at any OpenAI-style `/chat/completions`
endpoint (OpenAI, OpenRouter, a local server, or an Anthropic-backed gateway).

The command is **`aizen`**.

## Install

**One line** — grabs the latest optimized binary for your OS and puts `aizen` on your PATH:

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/dawnofcd/Aizen_agent/main/install.ps1 | iex
```

```bash
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/dawnofcd/Aizen_agent/main/install.sh | sh
```

Then open a new terminal and run `aizen config`. (Override the install dir with `$env:AIZEN_INSTALL`
on Windows or `$AIZEN_INSTALL` on Unix. The Windows `.exe` is unsigned — if SmartScreen warns, choose
*More info → Run anyway*.)

**Or download a binary by hand** from the [latest release](https://github.com/dawnofcd/Aizen_agent/releases/latest):

| Platform | Asset |
|---|---|
| Windows x86-64 | `aizen-<ver>-windows-x86_64.exe` |
| Linux x86-64 | `aizen-<ver>-linux-x86_64` |
| macOS Apple Silicon | `aizen-<ver>-macos-aarch64` |
| macOS Intel | `aizen-<ver>-macos-x86_64` |

On Linux/macOS: `chmod +x aizen-* && ./aizen-…`.

**Or build from source** (any platform with a Rust toolchain):

```bash
cargo install --git https://github.com/dawnofcd/Aizen_agent   # → aizen on your PATH
# or, from a clone:
cargo build --release      # → target/release/aizen (one static binary)
cargo test                 # unit + harness tests (no network)
```

The default build is a standalone binary with no native deps (pure-Rust, rustls-only TLS). Two
optional, off-by-default features: `--features dense` (semantic search — needs a C++ toolchain + a
local model) and `--features browser` (CDP browser tools — **stays a pure-Rust static binary, no C
toolchain**; see [browser](#browser-automation---features-browser)).

## Interactive REPL (just run `aizen`)

Run the binary with **no arguments** to land on a splash screen (block-art title + a bordered
panel of tools/commands) and a **unified chat+agent REPL** — there is no separate "chat" vs
"agent" mode: you just type. A plain message is answered; a task that needs tools uses them — one
loop. A status line shows the model, session tokens, turn count, and a **`% context` bar**
(green→yellow→red as the window fills).

```text
⚡ gpt-4o-mini  ·  ~1.2K/128K tok  ·  3 turns  ·  ███░░░░░░░ 27% ctx
╭──────────────────────────────────────────────────────────────────╮
│ ❯ refactor the parser and run the tests                           │
╰──────────────────────────────────────────────────────────────────╯
```

The chat box is a small line editor: type / Backspace / Del at the cursor, **←/→** move, **Home/End**
jump, **↑/↓** recall past prompts, Enter sends. A braille spinner (`⠹ thinking`) shows while the
model is responding, clearing the moment the first token streams.

**Attach an image** (vision) — two ways, because Ctrl-V can't be used (Windows Terminal intercepts
it for its own paste, so the keystroke never reaches `aizen`):
- **Ctrl-O** — grab a copied screenshot from the clipboard (Win+Shift+S, or "Copy image" in a
  browser). An `[1 img]` tag shows in the top border.
- **Drag an image file onto the window** — the terminal pastes the file path; press Enter and `aizen`
  turns image-file paths on the line into attachments (you can also type/paste a path). Real image
  files only — prose like `nope.png` that isn't a file stays as text.

Both send the image with your text to a **vision-capable** model. **Ctrl-X** removes the most recent
attachment (keeps your text); **Esc** clears the line and all attachments at once. Clipboard
screenshots are downscaled to ≤1568px and encoded inline; the token gauge ignores attachments (they
ride outside `content`). Clipboard grab is desktop-only (Windows/macOS); drag-drop/path works
everywhere.

The context window is **auto-detected** from the provider's `/models` when it reports one
(OpenRouter/LiteLLM-style gateways do; the bare OpenAI schema doesn't). When it's absent the bar
shows `ctx·est` and estimates by model name (Claude 200K · Gemini/GPT-4.1 1M · DeepSeek 64K · else
128K). Override it explicitly with `aizen config set --context-window <tokens>`.

**Slash commands** (the meta layer):

| command | does |
| --- | --- |
| `/help` | list commands |
| `/model` | list the provider's models (with context windows) + arrow-key pick one |
| `/config` | settings wizard: endpoint + key + model + context window + auto-compact |
| `/memory [query]` | show your profile, or search memory |
| `/persona` | character the agent plays + its evolving self-memory: select · new · paste-to-create · view/reset self-memory |
| `/skills` | saved procedures the agent can load: list · view · new · delete |
| `/commands` | your **custom slash commands** — markdown macros you fire (see below) |
| `/mcp` | MCP servers from `~/.aizen/mcp.json` — list connected tools (see below) |
| `/apps` | connected apps & MCP catalog — Telegram/Discord/Slack/webhook notify + browser-sign-in MCP apps |
| `/telegram` | Telegram integration menu: setup · test · status · start daemon · disable |
| `/sessions` | saved conversations — restore · save · delete (the chat also auto-saves as `last`) |
| `/compact` | summarize older turns now to free context |
| `/yolo` | toggle auto-approve (run file edits & shell without asking) — the hard safety floor still blocks catastrophic commands |
| `/smart` | toggle smart approval (auto-run read-only shell like `git status`/`cargo check`; writes still ask) |
| `/cost` | session token usage + a $ estimate (real provider usage when reported; set rates via `aizen config set --price-in/--price-out`) |
| `/clear` | fresh conversation · `/tokens` usage · `/quit` exit |

**Input shortcuts** — on a normally typed message (not with an image):

| type | does |
| --- | --- |
| `#<text>` | **remember** `<text>` as a durable memory fact in one keystroke (straight into the brain the agent reads) — sends no turn |
| `!<cmd>` | **shell escape** — run `<cmd>` and show its output (the hard safety floor still blocks catastrophic commands) — sends no turn |
| `@<path>` | inline that file's contents into your message (only when the file exists — `@handle` in prose is left alone) |
| `` !`<cmd>` `` | splice a **read-only** command's output into your message (same gate as custom commands) |

**Context window + auto-compact** live in **`/config`** (so the settings stay in one place). The
window drives the `% context` HUD (auto-detected from `/models` when the provider reports it, else
estimated by model name, else whatever you type). Auto-compact (default **80%**, the `⊟ 80%` marker
on the status line) summarizes older turns into one dense note when usage crosses the threshold,
keeping the last few turns verbatim — the cut is always at a user-message boundary (no orphan tool
results). `/compact` forces it now. Both also settable non-interactively:
`aizen config set --context-window <tokens> --compact-threshold <0–95>` (`0` = off).

The REPL needs a real terminal; piped/CI stdin prints a hint and exits (`AIZEN_MENU=1` forces it).

**Icons** — the TUI uses a curated glyph set. Pick the style in `/config` or `aizen config set --icons
<emoji|nerd|off>` (persisted): `emoji` (default — renders everywhere, no font install), `nerd`
(dev-style Nerd Font glyphs — **only render if your terminal's font is a patched Nerd Font** like
"Cascadia Code NF", else you'll see boxes), or `off` (plain text). One-off override: `AIZEN_NERD=1` /
`AIZEN_NO_ICONS=1`. (A CLI can't bundle a font the terminal will use — `nerd` needs the font set in the
terminal itself.)

## Telegram — control `aizen` from your phone

`aizen serve` runs a long-lived daemon that listens on a Telegram bot (long-poll, no public URL): send
it a message → it runs the agent and replies; **destructive ops (file edits / shell) ask you to
approve from your phone** (inline ✓/✗). Pure-Rust (no teloxide), single binary.

```bash
aizen telegram setup     # paste the @BotFather token, message the bot to capture your chat id
aizen telegram test      # send a test message
aizen serve              # start listening (Ctrl-C to stop)
```
In a chat: a plain message runs read-only-safe (destructive ops prompt you here); prefix `/agent `
to run fully autonomously. **Follow-ups keep context** — "now fix it" works because each chat's
conversation is carried across messages (memory + SOUL + persona seeded once per session). `/new`
(or `/reset`) starts fresh; `/resume` reports how much context is kept. If the agent needs to
disambiguate it asks (the `clarify` tool) and your next message is the answer. Token lives in
`~/.aizen/cli-config.json` (or `AIZEN_TELEGRAM_TOKEN`); only `allowed_chat_ids` may talk to it. The
agent can also call `telegram_send` / `telegram_ask`.

## Configure

All network commands read three settings, as flags or env vars:

| Env var | Flag | Meaning |
| --- | --- | --- |
| `AIZEN_BASE_URL` | `--base-url` | OpenAI-compatible base, e.g. `https://api.openai.com/v1` |
| `AIZEN_API_KEY` | `--api-key` | Bearer token for the endpoint |
| `AIZEN_MODEL` | `-m, --model` | Model id, e.g. `gpt-4o-mini` |

Resolution precedence per command: explicit `--flag` > `AIZEN_*` env var > saved config (below).

```bash
export AIZEN_BASE_URL=https://api.openai.com/v1
export AIZEN_API_KEY=sk-...
export AIZEN_MODEL=gpt-4o-mini
```

### `aizen config` — interactive setup (recommended)
Run it with no subcommand for a guided setup: it asks for the base URL + API key, **fetches the
model list from the provider, and lets you pick one**, then saves to `~/.aizen/cli-config.json`.
```bash
aizen config            # interactive: base URL → key → pick a model → saved
```
After this, `aizen chat`/`agent`/`workflow` work with **zero env vars**. Non-interactive equivalents:
```bash
aizen config set --base-url https://api.openai.com/v1 --api-key sk-... --model gpt-4o-mini
aizen config show       # API key masked
aizen config path
```

### `aizen models` — list the provider's models
```bash
aizen models                       # GET {base}/models, marks your default
aizen config set --model <id>      # pick one as the default
```

The memory brain lives under `~/.aizen/cli-memory/` (override the root with `AIZEN_HOME`; the legacy
`NEXTGEN_HOME` is still honored, and a pre-rebrand `~/.nextgen` is auto-migrated on first run).
Memory commands are fully offline — no creds needed.

Retrieval is **Unicode-aware**: the lexical tokenizer NFC-normalizes before lowercasing and
splits on `\p{L}\p{N}_`, so Vietnamese (and any accented script) is matched whole instead of
being shredded to ASCII fragments. This is pure-Rust and adds no dependency to the static binary.
Measured on the recall bench, Vietnamese paraphrase recall@5 went 0.00 → 1.00 with literal/English
recall unchanged. Verify with `aizen bench memory --split all`.

Ranking is **BM25** (Okapi k1=1.2, b=0.75, floored IDF over the active corpus + length
normalization) — term rarity and doc length now shape relevance, so concise on-point facts beat
verbose keyword-stuffed ones. A pure-Rust Jaro-Winkler fuzzy bridge for typo'd query terms is
implemented + unit-tested but **off by default** (on the current corpus it adds candidate noise
without a recall gain; one flag from on).

The store **evolves from reuse** — no LLM, zero extra tokens. Every fact the agent retrieves into
context is reinforced (at most once/day); ranking is `bm25 · decay · salience`, where reused facts
decay slower (`half_life·(1+ln1p(reinforced))`) and gain salience (`0.5 + 0.3·reuse + 0.2·recency`,
capped so BM25 stays dominant), and the always-on frozen core is packed salience-greedy so the
prompt prefix holds the facts you actually use. This is provable, not marketing: `aizen bench memory
--evolution` runs a 6-session reuse simulation and **fails** unless recall@5 climbs ≥5%/session
until it plateaus.

## Commands

### `aizen chat` — one-shot streaming chat
```bash
aizen chat -p "explain this error: ..."
echo "summarize this" | aizen chat        # prompt from stdin
```

### `aizen agent` — the tool-using loop
The model reads/edits files, runs shell, and uses memory to finish a task end-to-end.
```bash
aizen agent "add a --version flag and update the help text"
aizen agent --yes "fix the failing test in src/parse.rs"   # pre-approve file/shell ops
aizen agent --max-iters 40 "..."                            # raise the step cap
```
Behavior worth knowing:
- **Parallel reads** — when a turn only reads (file_read/glob/memory), the calls run
  concurrently; any turn that edits or runs shell stays serial (and approval-gated).
- **Approval** — destructive tools (`file_edit`, `shell_run`) prompt before running. In the sticky
  REPL each one shows an inline **`[y]es · [n]o · [a]llow all this session`** prompt (the `[a]`
  choice is a session-scoped soft `/yolo`, reset by `/clear`); `/yolo` still pre-approves everything,
  `/smart` auto-runs read-only-shaped shell. Non-TTY (CI/pipes) safely denies unless `--yes` is set;
  under `aizen serve` the prompt is routed to your phone. The hard `cmd_guard` floor blocks catastrophic
  commands underneath all of these.
- **Verify gate** — after an editing run, a fast typecheck (`cargo check` / a `typecheck`
  npm script / `npx tsc --noEmit`) runs once before the agent reports done; on failure the
  errors are fed back for one fix turn. Skips silently for unrecognized projects.
- **Sub-agents** — the agent can call the `task` tool to delegate a self-contained sub-task to
  a fresh role-scoped sub-agent (`coder`/`tester`/`planner`/`reviewer`). Single depth: a
  sub-agent cannot spawn further sub-agents.
- **Clarify, don't guess** — when a choice is genuinely ambiguous and a wrong guess would waste
  real work, the agent calls `clarify` to ask ONE question; the turn pauses and your next message
  is the answer (in the REPL, the plain prompt, or over Telegram — no stdin contention with the
  input box). For low-stakes choices it assumes and states rather than stalling.
- **Web research** — `web_search` (no-key DuckDuckGo) finds pages; `web_fetch` GETs a URL and
  returns it as readable text (HTML reduced to prose, capped); `web_crawl` spiders a site from a
  seed URL (see `aizen crawl` below). Read-only; available to every role.

### `aizen workflow <spec.json>` — fan-out + synthesis
Run several role-scoped sub-agents concurrently (bounded to 5), then merge their results into
one answer (mixture-of-agents). See [examples/review.workflow.json](examples/review.workflow.json):
```bash
aizen workflow examples/review.workflow.json
```
Spec shape:
```jsonc
{
  "name": "review-changes",
  "tasks": [ { "id": "bugs", "role": "reviewer", "prompt": "...", "model": "optional-per-task" }, ... ],
  "synthesis": { "model": "optional-override", "prompt": "optional merge instruction" }
}
```
Roles set each sub-agent's tools (coder = read/edit/shell, tester = shell no edit,
planner/reviewer = read-only). A failed task never aborts the workflow — its result is captured
and the synthesis still runs. The synthesis uses `AIZEN_MODEL` unless `synthesis.model` overrides it.
**Model diversity (mixture-of-agents):** each task may set its own `model` (e.g. a cheap model
scouts, a strong one reviews) — else the workflow default. `--trace <path>` writes a JSON audit of
the fan-out (per-task model + outcome + the synthesis model).

### `aizen crawl <url>` — katana-style web crawler
BFS over HTTP from a seed URL: extracts links from HTML (`href`/`src`/`action`) and endpoints
from JS (regex over quoted paths/URLs), follows the in-scope, unseen ones up to a depth/page cap.
Pure Rust — no headless browser, no passive sources (those would break the single binary).
```bash
aizen crawl https://example.com                       # depth 2, same host, ≤200 URLs
aizen crawl https://example.com --depth 1 --max-pages 50 --show-source
aizen crawl https://example.com --scope subs          # also follow *.example.com subdomains
aizen crawl https://example.com --json                # [{url, depth, via}]
```
Only GET requests; scope defaults to the seed host (`--scope subs` for subdomains); `--max-pages`
is a hard ceiling. Also exposed to the agent as the `web_crawl` tool.

### `/persona` — a character that evolves
A **persona** is *who the agent is* — a third identity layer alongside **user_memory** (who you are)
and **skills** (how to do things). Cards live as human-editable markdown under
`~/.aizen/personas/<name>.md` (frontmatter `name`/`role`/`voice` + body). The active one renders
into a `<persona>` block in the system prompt; switching applies to the current chat **in place**
(no lost history).

Three ways to make one in **`/persona`**: **New** (type name/role/voice + a multi-line body),
**Paste a character prompt → auto-create** (paste any character/system prompt — the model distills
it into a structured card for you), or just select an existing one. (Pasting a character prompt as a
normal chat message only role-plays for that one turn — it isn't saved, doesn't survive `/clear`, and
isn't cached. Use paste-to-create to make it persistent.)

A fourth way needs no menu: **just ask in chat.** "Create a persona named Mira, a wry noir
detective, and be her" → the agent fills in name/role/voice/backstory and calls the **`persona_create`**
tool (approval-gated, writes the card). By default it switches to the new character, which goes live
from your **next message** (the switch happens at the turn boundary so the prefix cache stays warm).

**It grows like a human** (Generative-Agents pattern, on by default when a persona is active):
- **Self-memory (`<self>`)** — after each turn the character records a *free* episode of what it
  lived through, importance-scored (corrections + real work + substance score higher). The top
  experiences (`importance × recency`, insights weighted up) are injected as a `<self>` block so the
  character carries its past forward. Stored per-persona under `~/.aizen/personas/<slug>.self/`.
- **Reflection** — once enough formative experience piles up, one model call distills recent episodes
  into durable first-person **insights** (`🌱 … reflected — +N insight(s)`). This is what makes the
  character deepen across sessions instead of resetting.

`/persona` also has: **View self-memory** (insights + recent episodes), **Reset self-memory**, and an
**Evolution ON/OFF** toggle. Off → a frozen character. Toggle persists via `persona_evolve`
(`aizen config set --persona-evolve false`). Honest framing: this is a *consistent, accumulating,
self-reflecting character* — not raised model intelligence.

Scriptable, fully offline (no creds) — handy for setup and for inspecting what the model sees:
```bash
aizen persona new Aria -r "a sharp mentor" -v "concise, warm" -b "You value clarity."  # body via -b or stdin
aizen persona use Aria / clear                         # set / clear the active persona
aizen persona list / show <name>                        # list (● active, with self-memory counts) / show a card
aizen persona self [name]                               # view accumulated insights + recent episodes
aizen persona remember "what I just lived through"      # record a free episode (auto-scored importance)
aizen persona block                                     # print the <persona> + <self> blocks the model sees
```

### `aizen soul` — the agent's operating identity
Where a **persona** is a swappable costume, the **SOUL** is *who the agent is operationally* — durable
values and policies that hold across EVERY persona and project (e.g. "always run tests before claiming
done", "reply in Vietnamese", "never push without asking"). It lives at `~/.aizen/SOUL.md`
(**HOME only, never cwd** — so a cloned repo can't silently rewrite the agent's rules) and renders into
an `<agent_identity>` block **above** `<persona>` in the system prompt, reaching chat / agent / serve /
workflow alike. The body is sanitized + secret/injection-scanned before injection (fail-closed: a
poisoned line drops the whole block).
```bash
aizen soul set -b "Always run tests before saying done. Never push without asking."  # body via -b or stdin
aizen soul show        # print the <agent_identity> the model actually sees
aizen soul path        # ~/.aizen/SOUL.md — edit directly in any editor
aizen soul clear
```

### `aizen skill` — reusable procedures (skills)
A **skill** is a saved step-by-step playbook (deploy the VPS, cut a release, triage logs) — distinct
from **memory** (facts/preferences). Skills live as human-editable markdown under `~/.aizen/skills/`.
A compact index (`name: when`) is injected into the agent's system prompt (`<skills>`); the agent
pulls a skill's full steps on demand with the **`skill_load`** tool, and can persist a new one with
**`skill_save`** (approval-gated). Manage them from the REPL with **`/skills`** (list · view · new ·
**fetch from URL** · delete), or the CLI:
```bash
aizen skill add deploy-vps -d "ship over SSH" -w "asked to deploy"   # body from --body or stdin
aizen skill fetch https://example.com/deploy-vps.md                  # pull a shared skill from a URL
aizen skill list / show <name> / delete <name>
```

Optional frontmatter narrows when a skill shows in the index: **`requires:`** (tool names — hidden
unless every one is in the live tool surface, so a `browser_*` skill is silent when browser tools
aren't built) and **`platforms:`** (`linux`/`macos`/`windows`, or `unix`/`posix` — hidden off-OS).

**Self-learning** — after a completed multi-step task, the REPL distills a *generalizable* procedure
into a new skill automatically (conservative: skips one-offs and duplicates; prints `↯ learned skill
'…'`). It fires when a turn did real work (**≥4 tool calls**) **or recovered from a dead end** (a tool
errored, then a later call succeeded — that hard-won path is worth saving). It's like memory's
self-learning, but for how-to. Toggle in `/config` (default on) or `aizen config set --auto-skill-learn false`.

### Custom slash commands — markdown macros you fire
Where a **skill** is something the *agent* pulls when relevant, a **custom command** is a prompt-macro
*you* fire by name. Drop a markdown file in `~/.aizen/commands/` (global) or `./.aizen/commands/`
(project — git-check it in to share with your team); a subdir namespaces it (`git/commit.md` →
`/git:commit`). Project files win over global on a name clash. List them with **`/commands`**; they
also show in the bare-`/` picker and `/help`.

```markdown
---
description: Review the staged diff for bugs and risky changes
argument-hint: [path]
---
Review this staged diff and flag bugs, security issues, and risky changes:
!`git diff --cached $ARGUMENTS`
```

The body is expanded at fire time, then submitted as a normal chat turn (full agent loop + tools +
memory apply):
- **`$ARGUMENTS`** → everything you typed after the command; **`$1`..`$9`** → positional words.
- **`@<path>`** → inlines that file's contents (confined to the working dir; only at a word boundary,
  so emails/handles pass through).
- **`` !`cmd` ``** → splices a shell command's output, but **only read-only commands run** — it goes
  through the same safety floor as the agent, so a blocked or write/network command is refused, never
  executed silently.

### MCP servers (`/mcp`) — bring your own tools
`aizen` can use tools from any [Model Context Protocol](https://modelcontextprotocol.io) server. Declare
them in `~/.aizen/mcp.json` (the same `mcpServers` shape Claude Desktop uses) over **stdio** (a
local child process) or **HTTP** (a remote endpoint):

```json
{
  "mcpServers": {
    "filesystem": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] },
    "remote":     { "url": "https://example.com/mcp", "headers": { "Authorization": "Bearer …" },
                    "include": ["search", "fetch"] }
  }
}
```

At startup `aizen` connects each enabled server, lists its tools, and exposes each one to the agent as
**`mcp_<server>_<tool>`** (per-server `include`/`exclude` filters which). External tools are
**approval-gated by default** (unless the server marks a tool read-only). A pure-Rust client — no
Node/Python MCP SDK, no extra runtime; the single static binary is preserved. `aizen mcp list` or
**`/mcp`** shows what's connected.

**OAuth sign-in apps** — the marquee SaaS servers (Linear, Notion, Slack, Gmail/Google, Atlassian)
are OAuth-only. `aizen` speaks the full OAuth 2.1 (PKCE) flow: an entry with `"auth": "oauth"` triggers a
browser sign-in (`aizen apps login <key>`, or automatically when you `aizen apps add` one), caches the token
at `~/.aizen/mcp-tokens/<key>.json` (0600), and refreshes it transparently. No API key to paste — you
authenticate with the real vendor; servers without dynamic client registration take an `"oauth": {
"client_id": "…" }` block.

```json
{ "mcpServers": { "linear": { "url": "https://mcp.linear.app/mcp", "auth": "oauth" } } }
```

**Connect apps without editing JSON** — `aizen apps` is a curated catalog over the official MCP registry:
`aizen apps list` (featured + connected), `aizen apps search <kw>`, `aizen apps add <key|name>` (picks a server,
prompts only for real secrets, signs you in if it's OAuth), `aizen apps info <key>` (config with secrets
masked + a live tool probe), `aizen apps login <key>`, `aizen apps remove <key>`. Also the `/apps` TUI hub.
LOCAL-FIRST: a self-hostable package (runs on your machine with your keys) beats a hosted gateway.

### Browser automation (`--features browser`)
Build with `cargo build --release --features browser` to give the agent five CDP tools that drive an
**existing** local Chrome/Edge/Brave (it never bundles a browser). Launch one with remote debugging,
then just ask — "open localhost:3000 and tell me why the login button does nothing":
```bash
chrome --remote-debugging-port=9222     # or msedge / brave; override host with AIZEN_BROWSER_CDP
aizen --features… # (already built) → "go to http://localhost:3000 and debug the login form"
```
Tools: **`browser_navigate`** (open a URL), **`browser_snapshot`** (the page's accessibility tree as
`[@ref] role "name"` lines), **`browser_click`** / **`browser_type`** (act on a `@ref`), and
**`browser_eval`** (run JS — read DOM/state, await fetches). Still a **pure-Rust static binary, no C
toolchain**: CDP's local endpoint is plain `ws://`, so the WebSocket client carries no TLS. An absent
browser returns an actionable error, not a crash.

### `aizen memory` — the self-learning brain
```bash
aizen memory add "prefer-pnpm" -t feedback -b "I prefer pnpm over npm"
aizen memory list
aizen memory search "package manager" [--dimension tooling]
aizen memory profile [--json]      # derived preferences rollup (verbosity/tooling/stack/…)
aizen memory ask "which package manager should I use?"   # abstains rather than guessing
aizen memory learn "<a user turn>"  # free extraction → threat-scan → route → store
aizen memory frozen                 # the always-on prompt-prefix core
aizen memory style | review | as-of <date> | supersede <old> <new> | archive | restore <id> | compact
```

### `aizen bench` — anti-oracle benches
```bash
aizen bench memory [--split gate|tune|all] [--hybrid]   # retrieval recall vs a baseline
aizen bench memory --evolution                          # multi-session reuse gate (≥5%/session lift)
aizen bench profile                                     # golden set for the profile rollup
aizen bench dialectic                                   # golden set incl. abstain-when-unknown
```

## Exit codes
`0` success · `1` error (bad args, network/HTTP failure, a bench gate FAIL). The agent loop
returns `0` even if it stops on the step limit or divergence — it prints the reason to stderr.

## Safety model
File and shell tools are confined to the working directory (a path-traversal guard rejects
escapes). Destructive ops are approval-gated (non-TTY safe-deny; `--yes` to pre-authorize, which
applies transitively to sub-agents). Beneath approval sits a **hard safety floor** — a deterministic
blocklist (`rm -rf /` incl. GNU long flags like `rm --recursive --force /`, `mkfs`, `dd of=/dev/…`,
fork bombs, `curl|sh`, `format C:`, …) that runs *before* the `/yolo` short-circuit, so catastrophic
commands are refused **even under auto-approve** (and the same check applies to background `process
start` + `` !`cmd` `` in custom commands). The web tools (`web_fetch`/`web_crawl`/`aizen crawl`) carry an
**SSRF floor**: a URL that resolves to a loopback/private/link-local address (incl. the cloud
metadata endpoint `169.254.169.254`) is refused — set `AIZEN_ALLOW_PRIVATE_NET=1` to allow
local/internal targets. Long-lived secret files (`cli-config.json`, OAuth/MCP token caches, saved
sessions) are written owner-only (0600) on Unix. MCP and other external tools are
destructive-by-default. `shell_run` is wall-clock capped at 120s. Tool results and file contents are
treated as data, never as instructions.

## Remote control & notifications
`aizen serve` / `aizen discord serve` run long-lived daemons that drive the agent from Telegram or a
Discord bot (pure-Rust gateways, no SDKs); destructive ops ask for approval from your phone. `aizen
cron` schedules unattended runs (model pinned at create time). Outbound `notify` channels (Discord /
Slack / generic webhook) and the two-way bots are all managed from the **`/apps`** hub.
