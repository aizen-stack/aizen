# Aizen — full reference

Everything the [README](../README.md) intentionally leaves out: the REPL surface, every command and
flag, self-hosting, MCP, the browser tools, and the safety model in detail.

> The README is the 5-minute version. This file is the manual.

---

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

The **mouse works in the box too**: click to put the caret on the character you clicked instead of
walking there with ←/→, and drag across the text to select it — the selection is copied to the
clipboard on release, and **Ctrl-C** copies it too (with nothing selected, Ctrl-C copies the whole
draft). Typing over a selection replaces it; Backspace/Del deletes it. On a draft longer than the box
the view only scrolls when the caret would leave it, so the text stays put under the cursor while you
move around in it.

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
| `/provider [name|add|manage]` | one-pick switch among saved providers; add/edit/rename/delete in the same manager |
| `/config` | provider-first settings: add/edit/switch connections, then assign providers/models to roles and specialists |
| `/memory [query]` | show your profile, or search memory |
| `/persona` | character the agent plays + its evolving self-memory: select · new · paste-to-create · view/reset self-memory |
| `/skills` | saved procedures the agent can load: list · view · new · delete |
| `/commands` | your **custom slash commands** — markdown macros you fire (see below) |
| `/mcp` | MCP lifecycle status: connected tools, generation, health, and per-turn schema pinning (see below) |
| `/browser` | browser profile / host-route / pinned-session status (`--features browser`) |
| `/apps` | connected apps & MCP catalog — Telegram/Discord/Slack/webhook notify + browser-sign-in MCP apps |
| `/telegram` | Telegram integration menu: setup · test · status · start daemon · disable |
| `/sessions` | saved conversations — restore · save · delete (the chat also auto-saves as `last`) |
| `/compact` | summarize older turns now to free context |
| `/approval [ask|smart|yolo]` | one approval setting: ask every time, auto-run read-only shell, or pre-authorize tools after the hard safety floor |
| `/timemachine` · `/checkpoint [note]` · `/diff` | `/timemachine` lists every crash-recoverable, worktree-scoped Git checkpoint and jumps back to the code **and** chat of the one you pick (one gesture, reversible); `/checkpoint` saves one now; `/diff` (or `aizen time diff`) shows what changed between two checkpoints, or `working` for the live tree. CLI: `aizen time doctor` inspects without touching the tree and reports loose objects once they pile up; `aizen time gc` compacts this repo's store (packs loose objects — a save does it automatically past 2,048); `aizen time gc --all` sweeps orphaned stores left by deleted/moved repos (dry-run by default, `--apply` moves them to a trash dir, which you then delete to reclaim the space) |
| `/update` | list every published version (the one you're running is marked) and install whichever you pick — newer or older, so the same command is the rollback |
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

**Reply visuals** — final answers can use responsive terminal tables and compact text diagrams. The
persisted mode is `auto` (default: only when it clarifies), `always` (every substantial final reply),
or `off`: choose it under `/config` → Display or run
`aizen config set --response-visuals <auto|always|off>`. Wide terminals get boxed tables; narrow
terminals fall back to stacked `key: value` records. Diagram fences preserve their monospace layout;
piped/CI output remains raw Markdown.

## Telegram — control `aizen` from your phone

`aizen serve` runs a long-lived daemon that listens on a Telegram bot (long-poll, no public URL): send
it a message → it runs the agent and replies; **destructive ops (file edits / shell) ask you to
approve from your phone** (inline ✓/✗). Replies use Telegram-native formatting: short bold headings,
clean lists, tappable safe links, copyable code blocks, and narrow stacked table records instead of raw
Markdown pipes. One temporary `✦ Đang xử lý…` status is removed when the final answer arrives; if rich
HTML is ever rejected, Aizen retries the same content as plain text. Pure-Rust (no teloxide), single binary.

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

**In-chat command menu**: the bot publishes a `/` menu (`setMyCommands`) so every control is one tap
away — `/sh <cmd>` (runs now; the `cmd_guard` floor still blocks catastrophic commands), `/cd` · `/pwd`,
`/approval` · `/ultimate` · `/effort`, `/model`, `/memory`, `/tools`, `/status`.

**Host more than one bot from one daemon**: from the primary bot, `/addbot <name> <token>` validates a
second @BotFather token and hot-spawns it into the running daemon (no restart); `/bots` lists them and
`/rmbot <name>` stops one. Extra bots share your `allowed_chat_ids` (a private chat id is your user id,
identical across all your bots), and a destructive-op approval always returns to the bot the request
came from. Extra bots persist in `telegram_bots` so a restart re-hosts them.

### Host it 24/7 on a Linux VPS

`aizen serve` is a foreground process — to keep the bot alive across logout, crashes, and reboots, run
it as a systemd service:

```bash
aizen telegram setup                    # once: token + your chat id
aizen serve --install --user --now      # write + enable the user unit, start now
```

`--install` writes a `Restart=always` unit (auto-restart on crash) with `network-online.target`
(waits for the network after a reboot); `--user` needs no root and calls `loginctl enable-linger` so it
survives logout. Drop `--now` to just write the unit and print the enable steps; omit `--user` for a
system unit (prints the `sudo` steps unless you're already root). `aizen serve --uninstall --user`
removes it. On Windows/macOS the command prints the NSSM / launchd equivalent. Note: the bot lives only
while the **VPS is on** — a powered-off VPS runs nothing (systemd restarts it the moment the VPS boots).

### Host it in Docker

Same daemon, packaged. Useful when you'd rather not install a toolchain on the host, or want the
agent's shell commands confined to a container:

```bash
cp .env.example .env       # AIZEN_API_KEY + AIZEN_TELEGRAM_TOKEN
docker compose up -d
docker compose logs -f     # a first run prints a pairing code here — send it to your bot
```

Two things to know about the image. There is **no published port**, because the daemon listens on
nothing — Telegram is long-poll, Discord an outbound websocket — so there's no ingress to expose or
firewall. And `tini` is PID 1: the agent spawns builds, test runners, and language servers, and
without a reaper those accumulate as zombies.

Two volumes matter. `aizen-home` (`/home/aizen/.aizen`) holds everything that must outlive the
container — the chat ids pairing wrote, sub-bot tokens, per-chat sessions, memory, the codebase index;
lose it and you lose owner pairing and all memory. `./workspace` is mounted at `/work` and is what the
agent edits, so mount only what you're willing to have edited (`:ro` for an audit-only run).

Health is `aizen serve --health`, wired as the image's `HEALTHCHECK`. It reads a heartbeat the daemon
stamps from inside its own event loop, and distinguishes idle from busy — so a probe tight enough to
notice a wedged loop won't restart a container that's ten minutes into a legitimate build. Raise
`AIZEN_HEALTH_MAX_BUSY_SECS` if your turns routinely run longer than 30 minutes.

Build without the dense retrieval tier by passing `FEATURES=` (empty) — a smaller image, at the cost
of the embedding-based memory tier.

### Host it on Kubernetes

Manifests in [`deploy/k8s/`](../deploy/k8s/), with the reasoning in
[`deploy/k8s/README.md`](../deploy/k8s/README.md):

```bash
kubectl apply -f deploy/k8s/namespace.yaml
kubectl -n aizen create secret generic aizen-secrets \
  --from-literal=AIZEN_API_KEY='sk-...' \
  --from-literal=AIZEN_TELEGRAM_TOKEN='123456:ABC-...'
kubectl apply -k deploy/k8s/          # configmap, statefulset, service, networkpolicy
kubectl -n aizen logs -f sts/aizen    # pairing code
```

It's a **StatefulSet with one replica**, and that's the finished shape rather than a starting point to
scale from. Telegram allows exactly one `getUpdates` poller per token — a second replica gets HTTP 409
forever, so it's one healthy pod plus one permanent crashloop, not double throughput. The on-disk
stores guard writes with local file locks that don't coordinate across pods (hence `ReadWriteOnce`, not
a shared volume). And turns run serially by design, which is what keeps approval routing race-free.

If that sounds like it buys little over systemd, that's the honest read: pick k8s when you already run
one and want its secret handling, node-failure rescheduling, and rollout mechanics — not for
throughput. To host more bots, use `/addbot` in an existing daemon rather than adding replicas.

The bundled `NetworkPolicy` is the part worth keeping even if you change everything else: it denies all
ingress and all *private* egress, including `169.254.169.254`, the cloud metadata endpoint that hands
out node credentials to any pod that asks. `net_guard`'s SSRF check covers the web tools, but the agent
also has a shell, and `curl` from there never passes through it — only the network layer catches both.
It needs a CNI that enforces NetworkPolicy (Calico, Cilium, Antrea); on a cluster without one the
object is accepted and enforces nothing. The manifest also sets `automountServiceAccountToken: false`,
since a shell that can read the projected token can talk to the API server as the pod.

## Configure

**ChatGPT Codex (experimental)** uses ChatGPT/Codex *consumer* OAuth (not the OpenAI Platform API key). Pick **ChatGPT Codex (experimental)** in `aizen config` → Providers & connection: it skips the API-key prompt and offers the browser sign-in in place of it, so no separate command is needed. The manual equivalents still work — `aizen auth login codex`, or `aizen config set --base-url https://chatgpt.com/backend-api/codex --api-key codex-oauth --model gpt-5.4-mini`. Model ids come from a shipped catalog, since the Codex backend has no `GET /models`. Tokens live in `~/.aizen/provider-tokens/codex.json` and `aizen config show` reports whether you are still signed in. Kill-switch: `AIZEN_DISABLE_CODEX=1`. **Risk:** private backend APIs may break or conflict with vendor terms; prefer Platform API keys / OpenRouter for supported production use. Logout: `aizen auth logout codex`.

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

### `aizen config` — provider-first setup (recommended)
Run it with no subcommand for the config dashboard. **Providers & connection** is the first row: add a
name, endpoint, API key, and default model once, then switch by choosing that named row. The same
manager supports Use, Edit, Rename, and Delete; API keys are masked in every list/display.
```bash
aizen config            # Providers & connection → Add provider
```
After this, `aizen chat`/`agent`/`workflow` work with **zero env vars**. Non-interactive equivalents:
```bash
aizen config set --base-url https://api.openai.com/v1 --api-key sk-... --model gpt-4o-mini
aizen config show       # API key masked
aizen config path
```

Save complete URL + key + model profiles when you have more than one compatible gateway, then switch
manually without restarting the REPL:

```bash
aizen config provider add primary --base-url https://api.openai.com/v1 --api-key sk-... --model gpt-4o-mini --use
aizen config provider add backup --base-url https://backup.example/v1 --api-key bk-... --model model-x
aizen config provider list
aizen config provider use backup
aizen config provider edit backup --base-url https://backup-2.example/v1 --api-key bk-2... --model model-y
aizen config provider rename backup secondary
```

Inside the REPL, `/provider` is the fast one-pick switcher. `/provider add` opens the add wizard,
`/provider manage` opens Use/Edit/Rename/Delete, and `/provider backup` switches directly. The next turn and health probe use the selected URL, key, and model. This is manual failover, not an
automatic retry/failover chain. `AIZEN_BASE_URL`, `AIZEN_API_KEY`, and `AIZEN_MODEL` still override the
saved selection; Aizen prints a note when those environment variables mask a switch.

Adding or re-pointing a provider starts from the preset list (OpenAI, ChatGPT Codex, Anthropic,
OpenRouter, Groq, DeepSeek, OpenCode, Ollama, or **Custom gateway** to type your own URL) — the same
list first-run setup shows, so a provider added in a later release is reachable without reinstalling.
The preset also supplies the default profile name, and its URL already carries the version suffix.

Changing an existing provider's endpoint asks for a new key instead of offering the previous
endpoint's credential. Cancelling any wizard step leaves the complete saved profile unchanged.

**OpenCode (free)** is a preset for OpenCode's zen gateway — a zero-cost way to try an agent before
bringing a key. The base URL is `https://opencode.ai/zen/v1` and the free tier's shared token is the
literal string `public` (type it at the key step; no sign-up anywhere). Its free models are the
`-free`-suffixed ids in the live list (`deepseek-v4-flash-free`, `mimo-v2.5-free`, `hy3-free`, …) plus
`big-pickle`; `aizen models` and the model pickers tag those rows with `· free` so a free-tier id
stands out from the paid ones in the same list. Expect free-tier rate limits — a 429 is retried with
the gateway's `Retry-After` like any other transient failure. The gateway reports no context
windows, so the HUD estimates them from the model name until you set one.

Sub-agent configuration uses the same provider list. In `/config` → **Sub-agents**, choose a saved
provider and either its default model or a model override for Sub-agent default, Summarizer, Oracle,
Apply, or an installed specialist. No endpoint/key is retyped. Scriptable specialist equivalent:

```bash
aizen agents set-provider code-reviewer backup              # provider default model
aizen agents set-provider code-reviewer backup model-y      # model override
aizen agents set-provider code-reviewer --clear             # inherit sub-agent default
```

Direct role URLs/keys, model→endpoint mappings, and endpoint fields in specialist cards remain
supported as advanced compatibility overrides. Environment variables remain highest precedence;
advanced overrides can therefore mask a provider selection and are labelled as such in `/config`.

Gateways differ in how they shape their streaming frames, and Aizen absorbs the differences quietly:
a frame it cannot read strictly is retried leniently, and whatever is still unreadable is keepalive
noise it drops without a word. If a new gateway ever *does* go quiet or lose tool calls on you, set
`AIZEN_DEBUG_STREAM=1` to print the offending frames (capped at 3 per response plus a total) — that
output is the useful thing to attach to a bug report.

### `aizen models` — list the provider's models
```bash
aizen models                       # GET {base}/models, marks your default
aizen config set --model <id>      # pick one as the default
```

The memory brain lives under `~/.aizen/cli-memory/` (override the root with `AIZEN_HOME`).
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
- **How tools reach the model** — the session's tool registry is the single source of truth. Its
  definitions (name, description, JSON Schema) are sent through the provider's **native** tool field:
  OpenAI-compatible gateways and Anthropic get `tools[{type:"function",function:{…}}]`, the ChatGPT
  Codex endpoint gets the same tools in the Responses dialect's flat shape. The system prompt carries
  only a compact **`# Tool routing`** map — capability → the exact tool names enabled *right now* —
  generated from that same registry, never the schemas. So `/tools`, `/lsp off`, `/apps`, a missing
  Telegram token or a build without `--features browser` all remove a tool from the prompt and the
  request together, and the model is never told about a tool it cannot call.
- **Parallel reads** — when a turn only reads (file_read/glob/memory), the calls run
  concurrently; any turn that edits or runs shell stays serial (and approval-gated).
- **Approval** — destructive tools (`file_edit`, `shell_run`) prompt before running. In the sticky
  REPL each one shows an inline **`[y]es · [n]o · [a]llow all this session`** prompt (the `[a]`
  choice is a session-scoped temporary Yolo grant, reset by `/clear`). `/approval` is the persisted
  three-level setting: `ask` prompts, `smart` auto-runs read-only-shaped shell, and `yolo` pre-authorizes
  all non-floor operations. Legacy `/smart` and `/yolo` aliases remain accepted. Non-TTY (CI/pipes) safely denies unless `--yes` is set;
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
- **Web research** — `web_search` (needs a free Tavily key — set `TAVILY_API_KEY`) finds pages; `web_fetch` GETs a URL and
  returns it as readable text (HTML reduced to prose, capped); `web_crawl` spiders a site from a
  seed URL (see `aizen crawl` below). Read-only; available to every role.

### `aizen workflow <spec.json>` — fan-out + synthesis
Run several role-scoped sub-agents concurrently (bounded to 5), then merge their results into
one answer (mixture-of-agents). See [examples/review.workflow.json](../examples/review.workflow.json):
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
aizen skill where                                                    # the three folders + counts
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
**`/mcp`** shows the manager/connection generation, sanitized health, pinned schema hash, and tools.

MCP schemas are **pinned for one agent run**. If a server emits `notifications/tools/list_changed`,
Aizen defers the new schema until the next fresh user message instead of mutating the tool registry
mid-run. Connection EOF/send/read/timeout poisons that connection: a read-only MCP call may reconnect
and replay once after confirming the schema is unchanged; a state-changing call is never replayed
after an ambiguous transport failure (its side effect may already have happened). HTTP 404 session
expiry and OAuth refresh retain their narrow one-retry behavior.

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
**existing** Chrome/Edge/Brave (it never bundles a browser). The legacy local setup still works:
```bash
chrome --remote-debugging-port=9222     # or msedge / brave; AIZEN_BROWSER_CDP overrides local
```

For multiple/local-or-remote CDP endpoints, create versioned `~/.aizen/browser.json`:
```json
{
  "schema": 1,
  "default_profile": "local",
  "profiles": {
    "local": { "provider": "cdp", "endpoint": "127.0.0.1:9222" },
    "work":  { "provider": "cdp", "endpoint": "https://cdp.example.com", "auth_env": "WORK_CDP_AUTH" }
  },
  "routes": { "*.corp.example": "work", "localhost": "local" }
}
```
`auth_env` is an **environment-variable name only**; credential values are never accepted in the
file, tool schemas, status output, or logs. When a profile sets `auth_env`, that value is attached as
the `Authorization` header on **both** the HTTP `/json` discovery request **and** the WebSocket upgrade,
so a CDP endpoint behind an auth proxy is reachable. `browser_navigate` resolves the URL host to a
profile and pins that session; snapshot/click/type/eval continue on the same profile. Switching profiles
drops the old websocket and invalidates its `@ref`s. Browser sessions are **keyed per conversation**
(REPL session, or `serve` platform:route:chat), so two chats never share a page, profile, or `@ref`s;
`/new`, session deletion, and hostbot route removal release a conversation's session. `/browser` shows
sanitized routing/session status; `/browser doctor` live-probes every profile without printing
credential values.

Tools: **`browser_navigate`** (open a URL), **`browser_snapshot`** (the page's accessibility tree as
`[@ref] role "name"` lines), **`browser_click`** / **`browser_type`** (act on a `@ref`), and
**`browser_eval`** (run JS — read DOM/state, await fetches). `browser_snapshot` may reconnect/retry
once after a transport failure; navigate/click/type/eval are never replayed after transport ambiguity.
Still a **pure-Rust static binary, no bundled browser/Node/Playwright/CEF**. An absent browser returns
an actionable error, not a crash.

### `aizen memory` — the self-learning brain
```bash
aizen memory add "prefer-pnpm" -t feedback -b "I prefer pnpm over npm"
aizen memory list [current|global|project|<zone>]   # narrow a long listing to one workspace view
aizen memory search "package manager" [--dimension tooling]
aizen memory profile [--json]      # derived preferences rollup (verbosity/tooling/stack/…)
aizen memory ask "which package manager should I use?"   # abstains rather than guessing
aizen memory learn "<a user turn>"  # free extraction → threat-scan → route → store
aizen memory frozen                 # the always-on prompt-prefix core
aizen memory style | review | as-of <date> | supersede <old> <new> | archive | restore <id> | compact
aizen memory where                  # the folders + counts, for editing or clearing out many at once
```

Ids are whole words. A fact named "Người dùng giao tiếp bằng tiếng Việt" files as
`nguoi-dung-giao-tiep-bang-tieng-viet` — accents are folded off each letter, and `-` marks a word
boundary and nothing else. An earlier slugifier tested one codepoint at a time, so every accented
letter became a separator and cut inside words: the same name came out `ng-i-d-ng-giao-ti-p…`. Stores
written by that version are re-slugged once, automatically, on the first run of a build that has this
— the old→new table is left in `cli-memory/.id-migration-<date>.tsv`, and graph edges are re-pointed
in the same pass. Set `AIZEN_NO_ID_MIGRATE=1` to skip it.

A fact's id comes from its display `name`, which is the first 60 characters of the fact — so that cut
has to land on a word too. It used to cut anywhere: 73 entries on one store had names ending in a
one- or two-letter fragment, and the id inherited it (`…-la-khe-uoc-lam-viec-l`, where `l` began
`lâu`). New facts back the cut up to the last word; the names already written are left alone, since
their bodies still hold the full text and a second automatic rewrite of a store belongs behind a
command you type, not a startup pass.

The same rule now governs every name derived from free text — memory ids, `#remember` captures,
persona self-memories, session saves, and the project zone key all share one implementation:

| Surface | Where | Migrated? |
|---|---|---|
| memory entry id | `cli-memory/entries/` | yes — `.id-migration-<date>.tsv` |
| `#remember` id | same | yes, same pass |
| persona self-memory | `personas/<slug>.self/` | yes — `.stem-migration-<persona>-<date>.tsv` |
| session save name | `sessions/` | no — existing files keep working, see below |
| project zone key | `skills/p/<slug>`, index | only if the checkout path is non-ASCII |

**Persona self-memories** get one extra thing: a short content hash on the end
(`ep-hoan-thien-landing-install-tabs-os-85ed`). Every episode body opens with its own type label, so a
stem taken from the first few words described the format rather than the memory — twelve files on one
store all read `ep-correction-user-redirected-me-todo`, separated only by a counter. The stem now skips
the label and carries a hash, so it identifies one memory.

**Session names** are derived, not migrated. New saves fold to ASCII whole words; files already saved
with accents keep their names and stay loadable, listable and deletable. Two guards on derivation:
credential-shaped tokens are dropped before the name exists (a key pasted as the first line of a chat
used to become the filename — and `/sessions` prints filenames), and a name is never cut mid-word.
The credential guard covers name derivation only: it does not redact what is inside a saved transcript,
so a key pasted into a chat is still in that file's message text.

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
