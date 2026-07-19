# Aizen — Complete Feature Specification

> **Purpose of this document.** A full, accurate inventory of everything Aizen does, written as a
> hand-off brief for designing **product documentation** and a **UI/visual interface**. Every feature
> below is verified against the source (`src/`), not guessed. Use §2 (Surfaces) and §4–§5 (TUI + visual
> language) to drive screen/IA design; use §3 (Feature catalog) and §7 (Command reference) to drive docs.

---

## 1. What Aizen is

**Aizen** (binary `aizen`) is a **single-binary, provider-agnostic agentic coding CLI**. It talks to any **OpenAI-compatible** endpoint (OpenAI, Anthropic via compatible gateway, local servers, etc.), and bundles an unusually deep set of capabilities into one fast, pure-Rust static executable — no Node, no Python, no external `rg`/browser engine required.

The product's three-line pitch (from the splash): *"One fast binary: chat · tools · automation · a memory that learns you."*

What makes it distinctive (the parts worth featuring in docs/marketing):

- **A self-learning memory brain** — a markdown knowledge base that passively learns durable facts about the user/project from each turn, ranks them by reuse, and injects the important ones into every prompt.
- **A layered identity model** — Soul (operating policies) → Persona (character) → Self-memory (what the character has become) → User-memory → Skills, each a distinct, optional prompt block.
- **Multi-surface** — the same agent runs as an interactive TUI, a one-shot command, a Telegram/Discord bot, and an OS-scheduled job.
- **Safety-first autonomy** — a hard command floor that can never be bypassed, plus tiered approval (interactive, session-wide, or routed to your phone).
- **Extensible** — MCP servers/apps (GitHub, Notion, Slack, Linear…), user skills, and custom slash commands.

**Mental model for a new user:** *"It's a coding agent in my terminal that remembers me, can role-play a persona, plugs into my apps, and can be left running unattended — safely."*

---

## 2. Surfaces (the "shapes" the product takes)

Aizen is the same engine exposed through five distinct surfaces. Each needs its own design treatment.

| Surface | Entry | Character | Design implication |
|---|---|---|---|
| **Interactive TUI / REPL** | `aizen agent` (or bare `aizen` after setup) | Sticky-footer chat with live slash palette, HUD, streaming markdown | The flagship screen — most UI work lives here (§4) |
| **Landing menu** | bare `aizen` (first run / not configured) | Branded splash + arrow-key setup wizard | First impression; onboarding flow (§4.2) |
| **One-shot commands** | `aizen chat`, `aizen memory …`, `aizen crawl`, etc. | Plain stdin/stdout, pipe-safe, no chrome | Docs-driven; clean text output, scriptable |
| **Always-on daemons (bots)** | `aizen serve` (Telegram), `aizen discord serve` | Headless; chat from your phone/Discord, approvals via inline buttons | Mobile/remote UX; setup wizards |
| **Scheduled jobs** | `aizen cron add …` (OS scheduler) | Unattended; logs to file | Config + log-viewing UX |

A key cross-cutting fact for design: **output adapts to context** — rich ANSI/markdown when attached to a TTY, plain raw text when piped/in CI. The UI must look great in a terminal *and* degrade cleanly.

---

## 3. Feature catalog (by domain)

### A. Chat & the agentic loop

**One-shot chat — `aizen chat`**
- Streaming chat completion against an OpenAI-compatible endpoint. Prompt from `--prompt`/arg or stdin.
- Resolution order for every connection field: **CLI flag → env var (`AIZEN_BASE_URL` / `AIZEN_API_KEY` / `AIZEN_MODEL`) → saved config**.
- Streams tokens live with a braille "thinking" spinner; renders markdown on a TTY, raw on a pipe.

**The agent loop — `aizen agent <task>`**
- A lean 6-step state machine per turn: **call model (with tools) → classify (a non-empty `tool_calls[]` is the signal, *not* `finish_reason`) → execute each tool (validate → gate destructive ops → truncate result → feed errors back) → append results → check convergence → loop.**
- **Iteration caps:** default 25 steps (`--max-iters`), with a **one-time auto-extend to ~50** when the model is near the cap and asked to wrap up. Hitting the wall ends with a clear `MaxIters` stop.
- **Divergence guard:** if the model repeats the exact same tool calls two turns running, it gets one recovery nudge, then stops (`Divergence`).
- **Context guard:** near ~90% of the context window, a one-time "wrap up now" nudge is injected.
- **Verify gate (post-edit typecheck):** after a successful destructive edit and before declaring "done," Aizen auto-runs the project's check (`cargo check`; for Node, `tsc --noEmit` / typecheck script; silent no-op for unknown projects, 90s timeout). On failure it injects the compiler errors and grants one fix turn. This is the "done but broken" catcher.
- **CLI flags:** `--yes`/`-y` (pre-authorize destructive tools), `--max-iters`, `--model`/`-m`, `--base-url`, `--api-key`.

**Sub-agent delegation — the `task` tool**
- The agent can dispatch a **focused sub-agent** with a fresh context and a role-scoped toolset for one self-contained sub-task; only the sub-agent's final text returns to the parent.
- **Roles:** `coder` (read/edit/shell), `tester` (shell, no edits), `planner`/`reviewer` (read-only). Sub-agents can't recurse (no `task` tool inside a task) and inherit the parent's `--yes`.

**Multi-agent workflow — `aizen workflow <spec.json>`**
- **Mixture-of-agents fan-out:** runs a set of role-scoped sub-agents concurrently (each with its own `role`, `prompt`, and optional per-task `model`), then a **synthesis pass** merges their outputs into one deliverable.
- Per-task model diversity (cheap models scout, a strong model judges/synthesizes). Errors in one task don't abort siblings.
- **Spec shape:** `{name, tasks:[{id,role,prompt,model?}], synthesis?:{model?,prompt?}}`. Flags: `--trace <file>` writes a JSON audit (per-task model + status + iters + summary), `--yes`, `--model`.

---

### B. The agent's tools (its capabilities)

Every tool is one clear capability. Tools are either **read-only** (run freely, often in parallel) or **destructive** (approval-gated). Each call renders as a `⏺ tool_name(arg)` event line with an informative `⎿ result` digest under it (lines read, matches found, `+adds −dels`, exit code) — edits also show a compact colour diff. The full output goes to the model; only the digest reaches the terminal.

**Memory (read-only):**
- `memory_search` — recall a specific fact (query + limit).
- `memory_profile` — the aggregated user profile (verbosity, autonomy, tooling, stack…) with confidence + cited facts.
- `memory_ask` — answer one question about the user; **abstains rather than guessing**.

**Files:**
- `file_read` — read a file or a 1-based line range (whole-file budget ~2000 lines / 200 KB, else head+tail preview). *Read-only.*
- `file_glob` — list files by glob (`src/**/*.rs`); skips `target`/`node_modules`/hidden; capped at 200. *Read-only.*
- `search_files` — regex content search honoring `.gitignore` (ripgrep's own walker, built in); `path:line: text` output. *Read-only.*
- `file_edit` — exact-string replace or create; whitespace-tolerant single fallback; shows a before→after diff. *Destructive.*
- `multi_edit` — an ordered list of edits applied atomically to one file. *Destructive.*

**Shell & processes:**
- `shell_run` — run a command, return stdout/stderr + exit code (120s timeout, UTF-8, cwd-confined). *Destructive (gated).*
- `process` — background processes: `start`/`poll`/`wait`/`log`/`kill`/`write` (stdin); returns a `proc_<id>` handle; pool of 16. *start/kill gated.*

**Web research (read-only):**
- `web_search` — keyed web search (Tavily primary, Jina fallback; needs an API key), capped results.
- `web_fetch` — fetch a URL → readable text (strips scripts/styles, 20K-char cap).
- `web_crawl` — map a site from a seed (depth ≤3, scope strict/subs).
- All three enforce an **SSRF floor** (loopback/private/link-local refused).

**Coordination & interaction:**
- `task` — dispatch a sub-agent (see §A).
- `todo_write` — maintain a visible multi-step checklist (✓ done / ▸ in-progress / ○ pending), rendered in the scroll region with a `☑ N/total` status. Whole-list-replace semantics.
- `clarify` — ask **one** focused question; pauses the turn (`AwaitingInput`) — the user's next message is the answer. Top-level only.

**Extensibility tools:**
- `skill_load` / `skill_save` — load/save reusable procedures (only advertised when skills exist).
- `persona_create` — mint/switch a persona.
- `notify` — broadcast a status line to configured channels (only when channels exist).
- `checkpoint` — save a time-machine restore point before a risky change.
- `checkpoint_rewind` — run-scoped recovery only (`last_good` / `pre_edit`, max 2 per agent run). Free-form `time restore <id>` stays human/CLI.
- **MCP tools** — every connected MCP server's tools appear as `mcp_<server>_<tool>`, destructive-by-default unless the server marks them read-only.

---

### C. Safety & approval (cross-cutting)

- **Hard command floor (`cmd_guard`)** — an unconditional blocklist that **`/yolo` can never bypass**: `rm -rf /`, `mkfs`, raw `dd` to a device, fork bombs, `curl … | sh`, `chmod -R 777 /`, Windows `format C:` / `del /s C:\`, MBR wipes. Matches the whole command string so chaining can't hide a blocked op.
- **Unified approval level (`/approval ask|smart|yolo`):**
  - **Ask** — interactive destructive tools prompt; non-TTY safely denies.
  - **Smart** — auto-run *read-only-shaped* shell (e.g. `ls`, `git status`, `cargo check`); writes/installs/deletes still ask.
  - **Yolo** — pre-authorize tools after the hard floor; `--yes` and `AIZEN_YES` resolve to this level.
  - Legacy `/smart` and `/yolo` aliases remain accepted for compatibility.
  - **Daemon (phone)** — in Ask/Smart, remaining approvals route to **Telegram inline ✓/✗ buttons** (5-min timeout → auto-deny).
  - **Non-TTY, no channel** — **safe-deny** (scripts/CI never silently run destructive ops).

---

### D. Memory brain — `aizen memory …`

The headline differentiator. **One fact = one markdown file** under `~/.aizen/cli-memory/entries/<id>.md` with YAML frontmatter (name, description, `type`, created/updated, and learned-fact metadata: source, confidence, reinforced count, sessions, validTo, supersededBy).

**Four fact types** (drive ranking priority): `user` (durable preferences), `feedback` (high-confidence corrections), `project` (task-scoped), `reference` (background, default).

**Subcommands:**
| Command | What it does |
|---|---|
| `add <name> [-d desc] [-t type] [-b body]` | Manually create one fact (body from stdin if omitted). |
| `list` | All active memories `[type] name — desc`; notes hidden/superseded count. |
| `show <id|name>` | Full content of one memory. |
| `search <query> [-k N] [--dimension …]` | Ranked lexical retrieval; `score [type/dimension] name`. |
| `frozen [--rebuild]` | Show the always-on prompt-prefix block (with token/entry counts). |
| `learn [text] [--yes] [--dry-run]` | Passive ingest of a turn → extract/sanitize/scan/route/store, with a report. |
| `style` | Show the learned user-style profile (STYLE.md). |
| `profile [--json]` | Derived user profile per dimension (language, verbosity, autonomy, tooling, stack, frustrations) with confidence + cited facts. |
| `ask <question> [--json]` | Dialectic Q&A about the user; **abstains** when evidence is insufficient or the question is counterfactual. |
| `review [--promote id] [--clear]` | Manage the mid-confidence review queue. |
| `as-of <YYYY-MM-DD>` | Bi-temporal: what was true on a given date. |
| `supersede <old> <new>` | Mark a fact replaced (history kept, not deleted). |
| `archive` / `restore <id>` | List LRU-evicted memories / bring one back. |
| `compact` | Enforce the inferred-fact LRU cap (archive oldest). |

**Key concepts to document:**
- **Three memory layers (token-lean):**
  1. **L1 Always-on frozen core** (`<user_memory>`) — **STYLE.md + global user prefs only**. Project-zone facts never spend the prefix budget. Built fresh at **session start**, then **immutable mid-session** (prefix-cache stability). Cap ~800 tokens (chars/4). Stored **per-repo** at `~/.aizen/cli-memory/core/active/<project-slug>.md` so repo A's core never injects into repo B.
  2. **L2 Session working memory** (`<session_memory>`, optional) — temporary in-process notes for the current session. Inferred / mid-confidence facts park here first (not durable). Cap ~300 tokens; empty → no tag. Cleared on `/new` / `/reset` / handoff / rebuild.
  3. **L3 Durable long-tail** (`entries/*.md`) — zone-tagged (`scope: <slug>` or global). Retrieved via `memory_search` (default = current workspace + global). Project knowledge lives **here**, not in always-on.
- **Passive auto-learning** (on by default): free regex extraction (zero token cost) → sanitize → **threat-scan** → **consolidate** → **route**:
  - **Explicit** (`remember` / correction / `#…`) → durable L3 (and STYLE when confirmed).
  - **Inferred** preferences / one-shots → L2 session only (no permanent pollution).
  - Style CorePromote still confirmation-gated (non-TTY / denied → no STYLE write).
- **Retrieval & ranking:** a BM25 **lexical floor** (always on), an optional **fuzzy** Jaro-Winkler bridge (`AIZEN_MEM_FUZZY=1`), and a **dense semantic** tier via RRF fusion — **on by default on `--features dense` release builds** (the model2vec backend), off on a default build (only the non-semantic fallback embedder ships there). The dense tier is **query-gated**: it fuses only when the top lexical hit covers <60% of the query tokens (paraphrase / cross-lingual), so a confident literal match keeps BM25's precision. Run `aizen memory model-download` once to fetch the ~30 MB model into `~/.aizen/models/`. Override with `AIZEN_MEM_DENSE=0/1`. Final score = `BM25 × decay × salience` — **facts rise/sink on reuse and reinforcement, not age alone**. Workspace scope filter runs **before** BM25 so IDF is computed on the in-view zone only.
- **Anti-bloat:** LRU caps + archive (recoverable), recency decay (inferred facts only), dedup-on-write, supersede. Deliberate (manual) facts never auto-evict.
- **Derived views are free/local** (no LLM): `profile` and `ask` compute deterministically and cite their basis; `ask` has a hard **abstain firewall** for unknowns.

---

### E. Personas & self-memory — `aizen persona …`

A **persona** is a character the agent role-plays — a markdown "character card" (`~/.aizen/personas/<name>.md`, frontmatter: name/role/voice + body). The active persona is injected as a `<persona>` block.

**Self-memory evolution (lean Generative-Agents × MemoryBank × CoALA):** the active persona keeps a **formative-only memory stream**. Episodes are **event-gated** (correction / preference / remember / substantial tool work / explicit `persona remember`) — small-talk and passive turns write nothing. Bodies are free typed notes (`correction:…`, `preference:…`, `work:…`), near-deduped against a recent window + existing insights, capped ~40. When formative importance piles up, **one reflection call** distills 1–3 durable character/relationship insights (coding-task trivia is rejected). The always-on `<self>` block (~700 tokens) is **insight-first**; only hot episodes (importance ≥ 6) may join. Gated by `persona_evolve` (default on).

**Subcommands:** `list` (● marks active, shows insight/episode counts), `show <name>`, `new <name> [--role --voice --body]`, `use <name>`, `clear`, `self [name]` (view the stream; "primed to reflect" badge), `remember <text> [--importance]` (record an episode, no model call), `block` (print the assembled `<persona>`+`<self>` the model sees).

---

### F. Soul — `aizen soul …`

The **durable operating identity** — values/policies that hold across **every** persona and project (e.g. "always run tests before claiming done," "reply in Vietnamese"). Stored at `~/.aizen/SOUL.md` (**HOME-only by design** — a project-local soul would let a cloned repo silently rewrite the agent's rules). Injected as `<agent_identity>`.

**Fail-closed safety:** sanitized (control-char strip + tag-breakout neutralize), **per-line threat-scanned** (a single poisoned line rejects the whole block), and truncated to ~400 tokens. Subcommands: `show` (default), `set [--body]`, `clear`, `path`.

---

### G. Skills & custom commands

**Skills — `aizen skill …`** — reusable named **procedures** (markdown playbooks the *agent* loads on demand). Stored at `~/.aizen/skills/<name>.md` (project `./.aizen/skills/` overrides). Frontmatter: name/description/`when`, plus optional `requires:` (hidden unless those tools are present) and `platforms:` (hidden unless OS matches). A compact `<skills>` index (name → when) is injected into the prompt; the agent pulls the full body via `skill_load`.
- Subcommands: `list`, `show`, `add [--description --when --body]`, `delete`, `fetch <url> [--name]`, `search <keywords> [--limit]` and `install <owner/name>` from the **agentskill.sh marketplace** (results show quality + security scores; install is approval-gated as third-party content).

**Custom slash commands** (`./.aizen/commands/**.md` or `~/.aizen/commands/`) — user-defined **prompt-macros** the *user* fires by name (`/git:commit`). Subdirectories namespace them (`commands/git/commit.md` → `/git:commit`); project overrides global. Frontmatter: `description`, `argument-hint`. Template expansion at fire time: `$ARGUMENTS`, `$1..$9`, `@<path>` (inline a cwd-confined file), and `` !`cmd` `` (splice a **read-only** shell command's output, gated by the same `cmd_guard` floor — destructive commands are refused, never run).

---

### H. Apps & MCP — `aizen apps …` / `aizen mcp …`

Connect third-party tools via the **Model Context Protocol** registry. Config lives in `~/.aizen/mcp.json` (project `./.aizen/mcp.json` loaded only after explicit **trust**).

**Featured catalog:** GitHub, Notion, Slack, Linear, Spotify, Google/Gmail, … Three transports: **local (stdio)** (npx/uvx/docker on your machine, your keys), **static-token remote** (header auth), and **OAuth remote** (real OAuth 2.1 PKCE browser sign-in; tokens cached `0600` at `~/.aizen/mcp-tokens/`).

**`aizen apps` subcommands:** `list` (featured + custom, ✓/○ connection badges), `search <keywords>`, `add <name>` (interactive: pick a server with a ★-recommended local-first default, runtime-on-PATH check, explicit third-party/OAuth confirm gates, masked-secret review before writing), `info <key>` (config + **live probe** of the tools it exposes), `login <key>` (OAuth), `remove <key>`.

**`aizen mcp` subcommands:** `list` (connected servers + tools), `login <name>`, `trust` / `untrust` (the supply-chain gate for project-local servers).

---

### I. Channels — bots & notifications

**Telegram daemon — `aizen serve`** — the long-lived bot: long-polls Telegram, runs the agent per message, keeps **persistent per-chat history** (follow-ups like "now fix it" keep context, capped ~40 msgs). In-chat commands: `/help`·`/start`, `/new`·`/reset`, `/resume`, and `/agent <task>` (autonomous mode). **Destructive-op approvals route to inline ✓/✗ buttons on your phone.** Replies chunked to ≤3500 chars.
- Setup — `aizen telegram setup` (paste BotFather token → message the bot to capture your chat id), `test`, `show`. Allowed-chat allowlist (empty = deny everyone).

**Discord bot — `aizen discord …`** — two-way gateway bot (`setup`/`test`/`serve`/`show`/`disable`); needs the privileged MESSAGE_CONTENT intent; channel allowlist; same in-chat commands; replies chunked to ≤1900 chars. (Inline-button approvals not yet wired — use `/agent` for autonomous edits.)

**Notifications (`notify` tool + config)** — one-way outbound to **Discord webhook / Slack / generic webhook**; `broadcast` posts to all configured channels. Useful for unattended runs ("report progress"). Env overrides: `AIZEN_DISCORD_WEBHOOK`, `AIZEN_SLACK_WEBHOOK`, `AIZEN_WEBHOOK_URL`.

---

### J. Standalone capabilities

**Web crawler — `aizen crawl <url…>`** — katana-style BFS: extracts links from HTML and endpoints from JS. Flags: `--depth`, `--max-pages`, `--scope strict|subs`, `--concurrency`, `--timeout`, `--json`, `--show-source`. SSRF floor applies.

**Time machine — `aizen time …`** — crash-recoverable Git snapshots of the current repository's Git-visible tree. Snapshots and metadata live in a **private store under `~/.aizen/timemachine/<repo-id>/`** (bare object store with a sealed alternates pointer into the source repo + per-worktree ledger/journal/chat); the source `.git` is never written by Time Machine. Metadata is fail-closed and atomically persisted behind an OS cross-process lock; internal Git runs disable hooks/fsmonitor/external filters; restore saves a preimage, verifies the resulting tree, and preserves a recovery journal on interruption. Ignored paths, paths outside the repository, nested repositories and unsafe reparse/junction targets are **not silently promised as covered**. Commands: `save [label]`, `list`, `restore <id>`, branch-aware `undo`/`redo`, `prune [--keep N]`, `doctor [--json] [--repair]`, `gc`, `clear`. The agent creates an operation-scoped checkpoint only after approval and blocks a protected edit if that checkpoint fails; after each successful edit it stamps `last_good`. When an approach is cascading-broken, the agent may call **`checkpoint_rewind`** (`target=last_good|pre_edit`, max 2 rewinds per run) — free-form `restore <id>` stays human/CLI. Retention cap `timemachine_keep` (default 50).

**Scheduled jobs — `aizen cron …`** — register agent tasks with the **OS scheduler** (Windows Task Scheduler / Unix crontab) — no daemon. `add <name> --schedule <daily@HH:MM|hourly|Nm|Nh> --task "…"`, `list`, `remove`. Runs unattended (`auto_approve`, hard floor still applies), pins the model at creation, logs each run to `~/.aizen/cron/<name>.log`.

---

### K. Config, models & provider

**Config — `aizen config`** — interactive setup wizard (base URL + key + pick a model from the live `/models` list) or `set` / `show` (key masked) / `path`. Saved at `~/.aizen/config.json`.

**Config fields:** `base_url`, `api_key`, `model`, `model_context_window` (override), `compact_threshold_pct` (auto-compact at %, default 80, 0=off), `auto_skill_learn`, `memory_auto_learn`, `persona_evolve`, `persona` (active), `timemachine_keep`, `timemachine_max_files`, `timemachine_max_bytes`, `timemachine_max_file_bytes`, `price_in`/`price_out` (enable `/cost` USD estimate), `icons` (emoji/nerd/off), `onboarded`.

**Models — `aizen models`** — lists provider models (with context windows when advertised). Feeds the `/model` picker.

**Provider client** — OpenAI-compatible streaming + tool-calling. **Auto-detects Anthropic models** and inserts prompt-cache breakpoints (free, warms the cache). A **process-global cost meter** accumulates tokens across every call (real provider usage when reported, else chars/4 estimate), surfaced via `/cost`.

---

### L. Benchmarks — `aizen bench …`

Internal quality gates (useful for a "trust/quality" docs section, not end-user UI): `memory` (anti-oracle recall, with `--split`, `--hybrid`, `--evolution`), `profile` (B2 golden set), `dialectic` (B3 Q&A incl. abstain-when-unknown).

---

## 4. The interactive TUI (detailed — primary UI surface)

A **sticky-footer REPL**: the input box is pinned to the bottom; the agent's work streams in a scroll region above it; a one-line HUD sits between.

### 4.1 In-chat slash commands (live-filtered palette, ~7 rows, Tab/↑↓ to pick)
`/help` · `/model` (pick model, shows context windows) · `/sessions` (save/restore/delete chats) · `/timeline` (`/tm`) and `/checkpoint` (`/cp`,`/snapshot`) — time machine · `/compact` (compress context) · `/memory` (`/mem`) · `/persona` (`/character`) · `/skills` · `/apps` · `/mcp` · `/commands` (custom) · `/telegram` (`/tg`) · `/serve` · `/config` (`/setup`) · `/approval ask|smart|yolo` (legacy aliases still accepted) · `/cost` (`/usage`) · `/tokens` · `/clear` (`/new`,`/reset`) · `/quit` (`/exit`,`/q`). Plus any user **custom commands**.

### 4.2 Landing & onboarding (bare `aizen`)
Branded **splash** (sun logo via sixel where supported, else braille; block-art "AIZEN" wordmark, tagline "ARTIFICIAL INTELLIGENCE AGENT"), a one-time welcome ("about 30 seconds"), then the **setup wizard**: base URL → API key (hidden) → model picker (live list with context windows, or a custom-id option) → optional compact threshold → optional messaging-app connect. The full splash also renders a **capabilities panel** (tool groups + command list + "N tools · M commands").

### 4.3 HUD / status line
One line, e.g. `opus-4-8 · ~1.2K/200K tok · 5 turns · 42% ctx · ⚡ yolo` — model (gold) · tokens/context-window · turn count · % context used · mode badge (`⚡ yolo` auto-approve, `◆ smart` read-only-auto, or none).

### 4.4 Streaming output
Live markdown rendering: syntax-highlighted code fences, gold headings/bullets, italic blockquotes with a faint bar, horizontal rules. A continuous **gold left gutter `▌`** marks assistant lines (vs `❯` user echoes and `⏺`/`⎿` tool call+result traces). Pipes/CI get raw text.

### 4.5 Input, images, keybindings
- **Image attach:** `Ctrl-O` grabs a clipboard screenshot as a vision attachment; `Ctrl-X` drops the latest; a gold `[2img]` badge shows the count. Multi-line pastes collapse to a chip (`↵ 3 lines pasted · first line…`).
- **Keys:** Enter submit · Esc cancel running turn / clear draft · Ctrl-C cancel · Ctrl-D quit-if-empty · Tab complete slash command · ↑/↓ palette nav or history · Home/End/←/→ edit.
- **Working indicator:** a moonlight star-spinner pill with a cycling verb, live elapsed clock and output-token counter — `✻ Đang nghiền ngẫm… · 23s · ↑1.2K tok · Esc`.
- **Approval modal:** inline `[y]es · [a]llow all this session · [n]o`.

---

## 5. Visual design language (for the UI designer)

- **One signature accent: warm gold-noir** (256-color `178`). Used for borders (`╭╮╰╯│─`), command/section/item names, the assistant gutter, spinners, badges. This single-accent restraint is the brand's visual signature — keep it.
- **Semantic colors only where they carry meaning:** green = ok/`✓`/`● on`, red = error/`○ off`, blue = links/model names/URLs, amber = warnings, dim grey = secondary text.
- **Three icon tiers** (user-selectable via `icons` config / `AIZEN_NERD` / `AIZEN_NO_ICONS`): **emoji** (default; 🧠 memory, 📘 skills, 📂 files, 💻 shell, 🌐 web, 🎭 persona, 🤝 delegate, 🔌 mcp, 🧩 apps…), **Nerd Font** (crisp PUA glyphs), or **off** (plain `•`).
- **Layout:** monospace grid, content truncated to box width (never wraps), aligned columns, boxed panels with gold borders.
- **Identity marks:** sun logo + "AIZEN" silver-gradient block wordmark on the splash.
- **Adaptive chrome:** full ANSI on a TTY, plain text when piped — the visual system must have a clean no-color fallback.

---

## 6. The layered prompt / identity model (the core mental model)

The system prompt is assembled in this fixed order (each block **optional**, kept lean & prefix-cache-stable):

1. **Base** (static instructions)
2. `<environment>` (cwd, OS, date, model)
3. `<agent_identity>` — **Soul** (durable operating policies)
4. `<persona>` — the active character
5. `<self>` — the persona's accumulated episodes + insights
6. `<user_memory>` — STYLE + global user prefs only (always-on frozen core; per-repo cache)
7. `<session_memory>` — optional temporary notes for this session (inferred facts; cleared on `/new`)
8. `<skills>` — the compact skill index

This stack — *operating policy → character → lived experience → who the user is → session scratch → how to do things* — is the conceptual heart of Aizen and worth a dedicated diagram in the docs.

---

## 7. Command reference (quick index)

| Command | Summary |
|---|---|
| `aizen` | Landing menu / interactive REPL (after setup). |
| `aizen chat [--prompt]` | One-shot streaming chat. |
| `aizen agent <task> [--yes --max-iters -m]` | Run the agentic loop. |
| `aizen workflow <spec.json> [--trace --yes -m]` | Multi-agent fan-out + synthesis. |
| `aizen memory <add\|list\|show\|search\|frozen\|learn\|style\|profile\|ask\|review\|as-of\|supersede\|archive\|restore\|compact>` | The memory brain. |
| `aizen skill <list\|show\|add\|delete\|fetch\|search\|install>` | Reusable procedures. |
| `aizen persona <list\|show\|new\|use\|clear\|self\|remember\|block>` | Characters + self-memory. |
| `aizen soul <show\|set\|clear\|path>` | Durable operating identity. |
| `aizen config [set\|show\|path]` · `aizen models` | Endpoint setup / list models. |
| `aizen apps <list\|search\|add\|info\|login\|remove>` · `aizen mcp <list\|login\|trust\|untrust>` | Apps & MCP. |
| `aizen serve` · `aizen telegram <setup\|test\|show>` · `aizen discord <setup\|test\|serve\|show\|disable>` | Bots. |
| `aizen crawl <url…>` | Website crawler. |
| `aizen time <save\|list\|restore\|undo\|redo\|prune\|doctor\|gc\|clear>` | Crash-recoverable Time Machine with private external store. |
| `aizen cron <add\|list\|remove>` | OS-scheduled jobs. |
| `aizen bench <memory\|profile\|dialectic>` | Quality benchmarks. |

---

## 8. Storage & config map (paths)

Home root: **`~/.aizen/`** (auto-migrated from legacy `~/.nextgen/`). Project-local overrides: **`./.aizen/`** (git repo root).

| Path | Holds |
|---|---|
| `~/.aizen/config.json` | Endpoint + behavior settings |
| `~/.aizen/SOUL.md` | Operating identity |
| `~/.aizen/cli-memory/entries/*.md` | Memory facts (one per file; zone via `scope:`) |
| `~/.aizen/cli-memory/STYLE.md` | Learned user-style core (always-on) |
| `~/.aizen/cli-memory/core/active/<slug>.md` | Per-repo frozen-core cache |
| `~/.aizen/cli-memory/{review,archive,embed-cache}/` | Review queue · LRU archive · dense cache |
| `~/.aizen/personas/<name>.md` · `<slug>.self/` | Persona cards · self-memory streams |
| `~/.aizen/skills/*.md` | Saved skills |
| `~/.aizen/commands/**/*.md` | Custom slash commands |
| `~/.aizen/mcp.json` · `mcp-tokens/*.json` | MCP servers · cached OAuth tokens |
| `~/.aizen/cron/<name>.{json,log}` | Scheduled jobs + run logs |
| `~/.aizen/timemachine/<repo-id>/store.git` + `worktrees/<wt-id>/` | Private Time Machine store (ledger/journal/chat + sealed object store); source `.git` is never written |

---

## 9. Hand-off notes for the designer

**Docs (information architecture suggestion):**
1. *Getting started* — install (one binary), `aizen config`, first chat.
2. *The agent* — loop, tools, approval/safety, verify gate.
3. *Memory brain* — the killer feature; lead with "it learns you," then the subcommand reference.
4. *Identity* — Soul → Persona → Self-memory, with the layered-prompt diagram (§6).
5. *Extending* — Skills, custom commands, Apps/MCP.
6. *Automation* — Telegram/Discord bots, cron, notifications.
7. *Tooling* — time machine, crawler.
8. *Reference* — full command/flag tables, config fields, storage paths.

**UI screens that most need design love:**
- The **TUI** (§4): scroll region + sticky input + HUD + slash palette + approval modal + working pill + image chips. This is the product.
- The **landing/onboarding** splash + setup wizard (first impression).
- **Pickers** (arrow-key Select): model, persona, app/transport, session, timeline.
- **Status/HUD** micro-typography (model · tokens · % · mode badge).
- A **capabilities map** graphic (tool groups + the layered identity stack).

**Tone to carry into visuals:** fast, single-binary, "noir gold" restraint, safety-first, and "it remembers you." Avoid multi-color clutter — the one-accent discipline *is* the brand.
