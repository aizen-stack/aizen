# Changelog

All notable changes to **Aizen** (`aizen`) — the pure-Rust agentic coding CLI.

This repo was extracted from the NextGen monorepo at v0.1.0 (2026-06-27); the detailed pre-0.1.0
development log lives in that monorepo's history.

## [Unreleased]

## [0.6.1] — 2026-08-09

A maintenance release about the parts of the agent that were quietly doing the wrong thing: a stream
frame that could take a tool call down with it, a delegated workflow that resolved paths against the
wrong directory, and a mouse wheel that scrolled the input box instead of the transcript. It also
folds the tool surface down — one `file_edit` covering both the single and batch form, and two
checkpoint tools instead of four — which is fewer schemas on every request.

### Fixed
- **A duplicate key in one streamed frame no longer drops the tool call riding with it.** Gateways
  that mirror the reasoning channel into both `reasoning_content` and `reasoning` produced a serde
  `duplicate field` error, and the rejected frame took whatever `content` or `tool_calls` arrived in
  the same delta with it. The two spellings are now separate fields, read through one accessor, and
  any frame that still fails a strict parse is retried leniently through `serde_json::Value`
  (last-wins on duplicates) before being given up on.
- **Unparseable-frame warnings no longer bury the session.** A mis-modelled delta shape breaks every
  frame, so the old warn printed one line per streamed token over the live UI. Frames that survive
  neither parse attempt are keepalive/ping noise and are dropped silently; set `AIZEN_DEBUG_STREAM=1`
  to see them, capped at 3 per response and truncated to the informative head of the payload.
- **The mouse wheel keeps scrolling the transcript for the whole turn.** A tool that spawns a child
  process can reset the console input mode on Windows, dropping mouse capture mid-turn; from there
  the terminal's `alternateScroll` leaks wheel ticks through as `↑`/`↓` keys, so scrolling walked
  input history instead of the transcript. Capture was only re-asserted on the turn's trailing edge,
  which left the rest of the turn leaking — it is now pinned across the entire working window.
- **A delegated workflow resolves paths against its own lane root.** The model-callable `workflow`
  tool read the process working directory, so concurrent lanes could read, edit, and checkpoint the
  wrong project. The root and context window now arrive from the registry that built the tool.
- **Workflow synthesis cannot park a fan-out forever.** The final non-streaming synthesis call had no
  deadline — a socket that stays byte-alive but never completes has neither `read_timeout` nor an
  inter-event watchdog to catch it. It now runs under the same call deadline as every other
  background call, and reports a timeout as a failure rather than a cancel.
- **Eagerly-started tool calls get the same argument repair as the normal path.** The streaming
  fast-start path skipped schema repair, so a call the deferred executor would have fixed could fail
  with a missing-argument error purely because it started early. Both paths now share one prepare
  step, and a call whose repair is ambiguous or that changes safety classification falls back to the
  normal executor.

### Changed
- **`multi_edit` is gone; `file_edit` does both.** Pass `edits[]` instead of `old_string`/`new_string`
  to apply an ordered list of edits to one file in a single atomic write. One fewer schema on every
  request, and one fewer decision for the model to get wrong.
- **Four checkpoint tools collapse into two.** `checkpoint` takes an `action` of `save`, `rewind`, or
  `restore` (all approval-gated); the read-only `checkpoint_view` takes `diff` or `list` and stays out
  of the approval path. `aizen time …` and free-form `restore <id>` are unchanged for humans.
- **Sub-agent prompts carry role-scoped tool guidance** instead of the full top-level catalog, so a
  planner or reviewer child no longer pays for tools it was never granted.
- **Workflow children inherit the parent's context window** and the tool-result clearing that goes
  with it, rather than a default.

### Internal
- Tool registration and `classify_tool` are now checked against each other, so a tool cannot be
  registered while being invisible to toolset filtering — the gap that let `codebase_search`,
  `skill_forget`, `team_status`, `goal_complete`, and `bot_admin` bypass it.
- `aizen bench loop` counts what it claimed to measure: model calls, tool calls, repeated call
  signatures, nudges by kind, and context-management events. A reactive fake-model harness checks
  that a nudge changes the model's approach instead of merely being appended.
- The schema budget test measures the real top-level registry after delegation, persona, LSP, goal,
  and config filtering, with separate ceilings for the minimal and maximal surfaces.
- Workflow synthesis prompts have a total cap derived from the resolved context window, truncating
  deterministically and naming what it dropped.

## [0.6.0] — 2026-08-08

Aizen now treats a provider as one reusable configuration object instead of making users coordinate
an endpoint, key, model, role override, and specialist card separately. This release also restores
mouse-wheel transcript scrolling, makes Ctrl-C useful for copying inside the retained terminal, and
turns Time Machine's noisy per-edit snapshots into meaningful phase restore points.

### Added
- **Named provider profiles with one-pick manual failover.** A profile stores its name, endpoint,
  API key, default model, and optional context window. `/provider` switches directly from a masked,
  secret-safe list; `/provider add` and `/provider manage` expose Add, Use, Edit, Rename, and Delete
  in the same surface. Scriptable equivalents live under `aizen config provider
  add|edit|rename|use|list|remove`.
- **Provider-aware sub-agent routing.** Sub-agent default, Summarizer, Oracle, Apply, and individual
  specialist agents choose from the same saved provider list and may inherit the provider's default
  model or set a model override. `aizen agents set-provider <agent> <provider> [model]` provides the
  automation path; `--clear` returns to inheritance.
- **Safe provider dependency management.** Renaming a profile atomically updates the active provider,
  role assignments, and specialist routes. Removing a referenced provider is refused unless the CLI
  is given `--replace-with` or an explicit `--force` clear, so config cannot acquire dangling refs.
- **Immediate health feedback after a switch.** Activating a provider resets the health chip and runs
  a one-shot probe immediately instead of waiting for the periodic poll.

### Changed
- **`/config` is provider-first.** The old parallel Connection/Providers paths are replaced by one
  **Providers & connection** row. Editing the main model synchronizes it back into the active profile;
  legacy direct role endpoints, model-to-endpoint mappings, and card endpoint fields remain available
  as labelled advanced overrides.
- **Time Machine checkpoints follow work phases, not individual edits.** A checkpoint is stamped when
  a todo phase closes or an edited run passes verification. `last_good` therefore lands on a clean,
  understandable boundary instead of one of many nearly-identical intermediate snapshots; repeated
  verification failure also surfaces the existing rewind option once repair budget is running low.
- **Mouse and clipboard behavior match terminal expectations.** The wheel scrolls the transcript or
  active overlay again, except during a scrollbar-thumb drag. Drag-release still copies a selection;
  Ctrl-C copies the highlighted transcript text (or the current draft), and a second Ctrl-C within two
  seconds quits. The old floating right-click Copy menu is removed, leaving right-click available to
  the terminal on Windows.

### Fixed
- **Changing an endpoint no longer offers or retains the previous endpoint's API key.** Provider edits
  may keep the current key only when the normalized URL is unchanged; a different URL requires a new
  key, and cancelling any wizard step leaves the saved tuple untouched.
- **Specialist model overrides no longer lose the selected provider endpoint.** An explicit task model
  changes only the model while retaining the locally assigned provider URL/key, and legacy card
  endpoint metadata cannot overwrite a normal local provider route.
- **Provider displays cannot leak credentials.** API keys are always masked and URL userinfo is
  redacted across `/provider`, `/config`, `config show`, provider CLI output, and agent listings.
- **Atomic writes use one implementation.** The duplicate edit-tool writer was removed in favor of the
  shared persistence implementation, keeping crash-safe replacement and compare-before-write behavior
  consistent across the binary.
- **Markdown tables render even when the separator's column count differs from the header.** A wider
  header over a shorter delimiter row (`|---:|---|` beneath three columns) used to fail detection and
  dump raw pipes into the transcript; the delimiter row is now accepted on shape alone and the columns
  are reconciled, so the table draws as a box.

## [0.5.9] — 2026-08-07

One theme: **six places reported failure as success.** A sub-agent that never answered was labelled
`done`. A saved conversation the program itself had written was listed as `(unreadable)`. A duplicate
gate that had never once fired looked like a store with no duplicates in it. An abandoned lease
directory was never swept because the only sweeper refused to look at other projects' leases. In every
case the code path completed without error, so nothing upstream had any way to tell that nothing had
happened — which is why several of these survived for months under a green test suite.

### Fixed
- **Sub-agents returned empty "successfully", many times in a row.** Three independent bugs, each
  sufficient on its own, which is why fixing any one of them earlier never made the symptom go away.
  (1) When the transient-retry budget ran out on an empty HTTP 200, the ordinary turn loop `break`ed
  with an empty turn and fell through the done cascade into `StopReason::Done` — the caller received
  `"(sub-agent produced no final answer)"` *under a `done` header*, indistinguishable from a genuine
  silent success. A test named `..._falls_through_once_budget_is_spent` had pinned exactly this as
  correct behaviour ("the pre-fix behavior, so a persistently-broken provider still terminates"), so
  the suite was green the whole time. A spent budget is now an `Err`, which every caller already
  handled correctly. (2) Each retry re-sent a **byte-identical** request. A provider that just answered
  a request with silence usually answers it with silence again — this is the direct cause of "many
  times". From the second attempt a short `[recover]` re-prompt is appended, and rolled back LIFO on
  every early return so a retried turn can't leave a stray `user` message in history. (3) `RespMessage`
  (non-streaming) never read `reasoning_content`, though the streaming `Delta` has always had it via
  `#[serde(alias = "reasoning")]`. A provider that puts the whole answer in `reasoning_content` with
  `content: null` therefore deserialized to exactly the shape of an empty 200. **Only sub-agents were
  affected**, because they are the one caller running entirely on the non-streaming path — the same
  model answering the same way through the streaming path was fine. Retry budgets 4 → 6 in both the
  `task` tool and workflow children, and the backoff curve now selects the patient one (cap 30s) when
  `cfg.quiet` marks a lane nobody is watching, rather than the interactive 4s cap.
- **The first tool call of a stream could still dispatch with no arguments.** `snapshot` guarded
  partial arguments with `args_complete`, but `finish_indexed` — the flush path for the LAST call in a
  stream — only filtered on a non-empty *name*. A stream cut mid-arguments therefore dispatched with
  `args = ""`, which reads as `{}` and surfaces as "missing required argument". Same filter now applies
  on both paths.
- **`/sessions` listed conversations as `(unreadable)` — and the files were fine.** Not abrupt
  shutdown: all 29 session files on the reporting machine parsed as valid JSON, and the write path
  (`atomic_write` → temp + `sync_all` + `MoveFileExW(REPLACE_EXISTING|WRITE_THROUGH)`) is genuinely
  crash-safe. The bug was an asymmetry inside `Message`: the hand-written `Serialize` emits `content`
  as an OpenAI **parts array** for a user turn carrying images, while `Deserialize` accepted only
  `Option<String>`. Any conversation that ever had an image pasted into it was written in a shape the
  program could not read back. `content` now round-trips through a `ContentField` enum accepting all
  three legitimate wire shapes (absent, string, parts array) and recovers `images` back out of the
  parts, so **the two affected files on disk load again with no migration** — the data was never lost,
  only unparseable. Unknown future part kinds (audio, video, file) degrade to "dropped" rather than
  failing the whole message, so this exact class of bug cannot recur through a provider addition.
  Images still live outside `content` in memory, keeping the `chars/4` token HUD and auto-compact from
  counting multi-MB base64.
- **The serde error that hid it for months is no longer swallowed.** `parse_session_bytes` returned a
  bare `None` for every failure. It now has a `parse_session_reason` sibling returning the reason, and
  `/sessions` prints `(unreadable: <why>)` — the corrupt-vs-empty distinction is unchanged.
- **98 dead recovery leases, oldest two weeks old, that nothing could ever delete.** `clear()` only
  runs on a clean exit, and `scan_stale()` filters by `repo_scope`, so a lease from any other project
  was invisible to the only code that could remove it; every abrupt shutdown added one, and no
  `consumed-*` or `quarantine-*` had ever been cleaned. `sweep_expired()` is deliberately scope-blind
  (that *is* the leak) with safety from the lock plus a 7-day age check rather than from scope. The
  lock guard is dropped before `remove_dir_all` — it holds an open handle to `lease.lock` inside the
  directory, and Windows refuses to delete a directory containing an open file, so keeping it would
  have made the sweep a silent no-op on the platform where the leak was measured. Orphaned
  `.aizen-tmp-*` files in the sessions directory are swept at startup too.
- **The duplicate-memory gate had never fired once.** Measured over the real 360-fact store, every
  same-tier pair scored under both thresholds: 0 pairs ≥ 0.80, 0 pairs ≥ 0.55, median nearest-neighbour
  0.199 — while the store held five separate facts all saying the user writes Vietnamese. The gate was
  not too loose, it was **blind**: `best_match` was purely lexical, and the shared vocabulary is
  exactly the layer Vietnamese paraphrase varies (`người dùng` / `user` / `anh`, `giao tiếp` /
  `trao đổi`). Peak similarity between two facts stating the same thing was 0.44, under the 0.55 floor.
  Similarity is now the max of three measures, the third being a new `match_text` layer that folds
  accents and strips the pronoun-and-particle layer for comparison only. This is *not* the retrieval
  tokenizer — that one deliberately preserves diacritics so searching `cà phê` works, and a test pins
  it. `persona::self_mem` had already hit and fixed this same blindness one layer down; the fix is now
  factored out so the two cannot drift apart again.
- **`reconcile` reported 227 successful `confirm`s while retiring nothing.** `Action::Confirm` carried
  only a target, so the redundant half of a `same` pair was never touched — the log was counting
  *gestures*, not *effects*. The duplicate is now actually removed (`drop_redundant`), gated on the
  candidate's own confirmations, with cycle protection so a supersede chain inside one batch can't
  delete its own survivor. A failed removal is reported ("duplicate kept — <why>"), never swallowed.
- **`/config` mid-chat corrupted the frame.** The suspend mechanism was correct; the raw prints were
  not. Roughly 20 `eprintln!` calls on the `/config`, `/skills`, `/persona` and `/sessions` paths wrote
  behind the render thread's back — ratatui diffs against its own cell buffer, never sees foreign text,
  and so leaves it alive across later frames. All now route through `tui::note_line`. `Spinner::start`
  gated only on `is_terminal()`, so a spinner on a background thread outliving the suspend window
  scribbled `\r` + `clear_line` over a live frame; it is now inert while the retained renderer owns the
  screen (the narrated operation still runs and still prints its verdict). `Command::Resume` now sets
  `force_clear` like `Redraw` already did, and re-probes the terminal size on resume — while suspended
  no paint happens, so `COLS`/`ROWS` were stale and a window resize with `/config` open drew at the old
  width.

### Changed
- **The mouse wheel no longer scrolls the transcript.** A wheel tick moved the viewport out from under
  a drag-selection's anchor line, silently changing what releasing the button would copy — which is why
  selecting an earlier prompt to copy it kept producing the wrong text. Scrolling back is keyboard-only
  now: PageUp/PageDown, End to return to the live tail. Mouse capture stays enabled regardless; it is
  what stops the terminal's `alternateScroll` from leaking wheel ticks through as ↑/↓ and walking input
  history behind the user's back.
- **Drag-release copy now confirms.** It had always copied silently, so there was no way to tell it had
  worked; it now prints the same `· copied N chars` note the right-click Copy path does, and says so
  honestly when the platform has no clipboard (`arboard` is desktop-only, so Linux is a no-op).

### Added
- `aizen prompt-size` — byte breakdown of the fixed per-turn overhead (system prompt + tool schemas),
  with `--tools` for per-tool sizes and `--json` for machine output. Tool schemas and loop count were
  the two remaining token levers after an audit found prompt caching already correct.

## [0.5.8] — 2026-08-05

One theme: **a name is a handle, and four of the five places that build one were cutting words in
half.** Testing a name one codepoint at a time against `is_ascii_alphanumeric` makes every accented
letter fail, so it becomes a separator and splits the word it belonged to. Three quarters of a real
memory store had ids nobody could read, and the id is the only thing `memory show|edit|forget`
accepts. The accent is now folded off the letter *before* anything decides where a word ends, in one
shared helper, and names already on disk are recomputed once. Two things fell out of the same work: a
pasted API key can no longer become a filename, and `aizen where` says when a saved transcript still
contains one.

### Fixed
- **Memory ids were cut inside words, so 76% of a real store was unusable.** `slugify` tested one
  codepoint at a time against `is_ascii_alphanumeric`, and every accented letter failed the test and
  became a `-`. On a measured 243-entry store that left **185 (76%)** of ids looking like
  `ng-i-d-ng-giao-ti-p-b-ng-ti-ng-vi-t`; the worst were readable as no word at all
  (`ngd-ng-mucugihd-liv-ngukic`). That is not a cosmetic problem: the id is the only handle
  `memory show|edit|forget` accepts, so an id nobody can read is a record nobody can address without
  listing the whole store first. The bug was the ORDER, not the character set — the accent is now
  folded off the letter *before* anything decides where a word ends, so the same name files as
  `nguoi-dung-giao-tiep-bang-tieng-viet` and `-` means "word ended here" and nothing else. Ids stay
  ASCII. `đ`/`Đ` are handled explicitly, since they are letters of the Vietnamese alphabet rather
  than `d` plus a mark and no normalization form decomposes them — without that, `đường` would have
  folded to `uong`. Truncation now backs up to the last whole word: a blind cut turned `tieng` into
  `tien`, a different word, so a shortened id read as a fact about something else.
  `memory` and `#remember` share one folding helper, so the two id-producing paths cannot drift.
- **A learned fact's display name also ended mid-word — and the id is derived from it.** Found by
  reading the migrated store rather than the code, which is why it is worth stating separately:
  `fact_name` cut the head of a fact at 60 characters on a *char* boundary, so it never panicked, but
  it cut straight through words. **73 of the same 243 names** end in a one- or two-character fragment,
  and `slugify` faithfully carried that fragment into the filename —
  `agents-md-…-la-khe-uoc-lam-viec-l`, where the `l` is the first letter of `lâu`. Folding accents
  could not have caught this: by then the accent was gone and the orphan was legitimately ASCII. The
  cut now backs up to the last whitespace and drops the punctuation left at the seam, so `…giữ a,`
  no longer leaves a dangling comma in a listing either; a fact shorter than the cap keeps its final
  word. **The 73 names already on disk are not rewritten** — their bodies still hold the full text so
  recovering them is mechanical, but a second automatic pass over a user's data in one release should
  be seen before it runs.
- **The same defect was live in three more places; all four now share one implementation.** Five
  surfaces independently turn free text into a filename, and four of them had written the same broken
  loop. `core::slug` is now the single definition of "a name a human can read and retype", so they
  cannot drift apart again:
  - **Persona self-memory** (`personas/<slug>.self/`) was the worst hit in proportion — **45 of 89
    files (51%)** on a measured store, named `in-t-i-n-n-l-9` or `ep-work-handled-b-y-gi-3`. It stayed
    invisible longer than the memory store because `/persona self` renders bodies, not filenames.
  - **Session working memory** (`session_mem::slug_id`) — the ids never reach disk, but they are
    rendered into the prompt block the model reads.
  - **The project zone key** (`config::slug_fragment`, half of `dirname-hex8`) — latent rather than
    observed: a checkout under an accented path keyed a zone like `d-n-aizen-…`. An ASCII dirname
    produces the byte-identical fragment it always did, so no existing zone is re-keyed; for the
    accented minority `aizen zone migrate` now also probes the pre-fold spelling so the old directory
    is still found and merged.
- **Persona self-memory filenames are re-slugged once, automatically** — the same mechanical pass as
  the memory ids, recomputed from each file's own body (self-memories have no frontmatter `name`),
  with the old→new map written to `personas/.stem-migration-<persona>-<date>.tsv` before the first
  rename. `persona:<slug>/<id>` graph edges are re-pointed in the same pass; the measured store has
  none, but `note_insight_cofire` can write them at any time, so the pass re-points rather than
  assuming. Per-persona flag file, so a character created later still gets migrated on its own first
  launch. Shares the `AIZEN_NO_ID_MIGRATE=1` opt-out.
- **Twelve persona self-memories were indistinguishable from each other.** Every episode body opens
  with its own type label (`correction: user redirected me — "…"`) plus, for todo nags, identical
  `[todo-poke]` scaffolding — so a stem taken from the first five words described the FORMAT, not the
  memory. Twelve files on a real store read `ep-correction-user-redirected-me-todo`, separated only by
  a `-2`…`-12` counter. Widening the word count would not have helped (the shared prefix just gets
  longer); the stem now skips the label and filler words and carries a short content hash, so it is
  unique by construction and the `-N` counter is back to being a last resort.
- **A pasted API key could become a session filename, and filenames are printed.**
  `suggest_session_name` derives a name from the first line of the first user message and kept any
  token of 2+ characters, so a key pasted as the opening line became the name of the file — which
  `/sessions` renders on screen, `ls` shows, and every backup copies. A real machine had one: 40
  characters, vendor prefix, no separators left after sanitizing. Credential-shaped tokens are now
  dropped while the name is being derived, by vendor prefix (checked on the raw token, since stripping
  separators first would erase the evidence) and by shape (length plus the character-class mixing that
  random material shows and words do not). A key on its own falls back to the generic `chat-<date>`
  stem; with prose around it the topic survives and only the key is dropped. This guards name
  derivation only — it does not redact transcript bodies, and **4 of 27 transcripts on the measured
  machine still contain key-shaped strings in their message text.**
- **`aizen where` now says when saved transcripts contain keys.** The name guard above stops a key
  becoming a *filename*, but a saved session is a verbatim transcript and nothing redacts what is
  inside it. `/where` reports the file count and names the folder; it never prints a value, and it
  does not touch the files — editing a user's own conversation history is their call. The scan uses
  vendor key prefixes rather than the shape test that guards name derivation, and that distinction is
  load-bearing: measured over the same 27 files, the shape test matched 5170 tokens of which **4026
  were ISO timestamps** (long, mixed-case, letters and digits — indistinguishable from key material by
  shape alone), which would have flagged 23 of 27 files and taught the user to ignore the warning.
  Prefix matching flags 12 strings, all real keys, in 4 files.
- **`persona self` asked you to choose from a set it would not show you.** At the 40-insight cap the
  view said "retire one" and then printed 10 of the 40 — and it is the only place a self-memory id
  appears, so `persona forget <id>` had nothing to name. The hidden count is now always stated and
  `--all` prints every id. Bodies render through `elide` rather than a truncator that appends
  `[+73 chars]`: that suffix exists so a *model* reading a clipped tool result knows content was
  withheld, but in a listing it is noise the reader cannot act on and it costs the width the text
  needed.

### Changed
- **Session names are ASCII whole words, like every other id.** `suggest_session_name` used unicode
  `is_alphanumeric`, so it was never shredded — but it kept diacritics, which meant a filename that
  differs by normalization form between platforms. It now folds through the same helper, per word, so
  no word is cut apart. `sanitize_name` is deliberately unchanged: it also has to map an *existing*
  on-disk name to itself, and folding there would orphan the accented session files already saved.
- **Ids already on disk are re-slugged once, automatically.** The new rule only helps facts written
  from here on, so a store written by an older build is migrated on the first run of a build that has
  this. It needs no model and makes no guesses: the display `name` in each file's frontmatter was
  never mangled, only the filename, so the correct id is `slugify(name)` recomputed. The old→new
  table is written to `cli-memory/.id-migration-<date>.tsv` **before the first rename**, since it is
  the only route back to the previous names. `graph.tsv` endpoints are re-pointed in the same pass —
  renaming the files alone would leave every edge aimed at an id that no longer exists and the
  association layer would silently go dark. Cross-kind endpoints (`skill:`, `persona:`) pass through
  untouched, `archive/` revision suffixes (`-r1`) survive, and collisions get a numeric suffix rather
  than clobbering. Verified on a copy of a real store: 258 ids renamed, 187 edges re-pointed, **0
  dangling endpoints**, no files lost. `AIZEN_NO_ID_MIGRATE=1` skips it; a flag file stops a second
  pass. It renames files without asking — but not silently: the count and the map's path go to
  stderr, so a piped `memory list` stays machine-readable.

### Added
- **`aizen memory where` and `aizen skill where`** — print the folders, with a file count each.
  Nothing pointed a user at a directory before: the `memory list` footer named only the three
  per-id verbs (`show`/`edit`/`forget`), and a path appeared just once, in `memory show <id>`, one
  entry at a time. Opening the folder is the faster route when the job is editing or deleting many
  records at once, and skills made this worse by living in **three** roots that the `[project]` and
  `[repo]` tags only hinted at. A directory that does not exist yet says so, so absence never reads
  as emptiness. `persona list` gained the same pointer inline, since personas have only one folder.

## [0.5.7] — 2026-08-05

Two things this release is about. **A hosted daemon now answers several conversations at once**
instead of one at a time. And the **memory / skill / persona** trio — three stores that were
supposed to reinforce each other — stopped being write-only-and-growing: two of the three had
silently stopped recording, none could be curated, and the read path spent tokens on every turn
whether or not the content bore on the question.

### Fixed
- **One slow chat no longer freezes every bot the daemon hosts.** `aizen serve` ran a single turn at
  a time for the whole process, so a `cargo test` in one chat blocked every other chat and every
  other bot behind it, for minutes — hosting several bots was cosmetic. Each `(bot, chat)` now owns a
  queue and a worker (`src/hostbot/lane.rs`), and lanes on different working directories run in
  parallel. Three things stay serialized on purpose, because parallelising them corrupts state
  rather than speeding anything up: one turn at a time **per chat** (history is never written
  twice), one writer **per workspace root** (`WorkspaceWriterLease` treats a second acquisition of
  the same worktree as reentrant — correct for a parent and its sub-agent, silently fatal between
  two unrelated lanes), and memory learning (one global store). A semaphore caps total concurrent
  turns, since each can spawn a compiler.
- **Per-lane `/cd`, `/model`, `/effort`, `/approval`.** These wrote the machine-wide
  `cli-config.json`, so one chat's `/cd` moved every other chat. They now persist to
  `hostbot/lanes.json`, keyed by route, with every field optional — an absent file behaves exactly
  as before. Identity (`bots.json`: token, owner, persona) stays a separate file, so a `/cd` can
  never rewrite a token.
- **`--health` no longer reports the wrong daemon.** A Telegram and a Discord daemon on one machine
  shared a single heartbeat file and overwrote each other. Now one record per platform, tagged with
  the hostname and device id (heartbeats often land in a synced home directory, where a bare pid
  says nothing about whose daemon it describes), plus live/mid-turn lane counts. The pre-split path
  is still read, so a probe keeps working across a rolling upgrade.
- **Skills and persona insights had stopped being written at all** — skills since 2026-07-26,
  persona self-memory since 2026-07-28 (stuck at 40/40 episodes and 40/40 insights). Both come from
  the same single model call as memory facts, and the JSON skeleton in the extractor prompt declared
  all four sub-fields for `facts` but only the bare word `null` for `episode` and `skill`. The model
  had to guess five field names it had never been shown; the parser requires them and returns `None`
  when they are absent. Facts were never guessing, which is exactly why one of the three kept
  working. The unit test passed a fully-shaped object, so it verified the parser and not whether the
  prompt could teach that shape — the drift test added here reads the prompt itself.
- **A long turn no longer starves the extractor of the transcript.** The injected fact block was
  written first and the transcript got whatever budget survived. Measured on 107 real turns: 62% of
  the turns that clear the tool-call gate exceed the 6000-char shared-model budget, peak 26,702
  chars. The transcript now has a floor and the injected block loses its tail handles instead — a
  fact is re-derivable from the user turn and the block itself, a **procedure exists nowhere but the
  transcript**. Injected bodies are also capped at the same `MAX_FACT_CHARS` the facts themselves
  obey (the largest stored entry is 4511 chars and was being inlined whole).
- **Vietnamese near-duplicates stopped slipping past the persona dedup gate.** The stopword list was
  English-only, so on real Vietnamese insights the highest pairwise Jaccard was 0.15 against a 0.75
  threshold — the gate could not fire. One concept had accumulated 12 variants.
- **A test-teardown bug that framed an innocent file.** Six helpers restored the environment by
  *deleting* `USERPROFILE`/`HOME`. A process with no home makes `is_forbidden_root` false for every
  directory, so a later walk climbs past the user profile — and the failure surfaced in an unrelated
  test in a different module, only under a full parallel run. A Drop-based `EnvGuard` now restores
  the previous value, including restoring "was absent". The earlier diagnosis blaming a stray
  `~/package.json` was wrong; no file needs deleting.

### Added
- **The skill index is now chosen per turn instead of always-on.** Every applicable skill was named
  on every turn — applicability (right OS, tools present) is not relevance (does this bear on what
  was asked). A skill is named only when its `when:` trigger covers ≥34% of the query's tokens —
  deliberately the same constant as the memory recall gate, since it is the same question over the
  same tokenizer, and two different numbers would only mean one of them was unmeasured. Measured on
  the real store: 642 tokens/turn → 0 for "fix the flaky parse test", 0 for "why is the build slow",
  61 for "format the vietnamese thesis docx"; still fires at 0.75 for "push my local changes to
  github". The block rides the **user turn**, not the system prompt: the dynamic system lane has its
  own cache breakpoint, so a per-turn selection there would rewrite it every turn and re-bill the
  whole transcript after it. For the same reason persona's block is deliberately **not** gated.
  Sub-agents gate on their task text and fall back to the full index when nothing matches — a spawn
  has no cache to amortize against, so a broad index is pure cost there.
- **Curation verbs the three stores were missing.** `skill delete` is now a soft delete (archived,
  `skill restore <name>` brings it back — the function existed with no CLI surface); a `skill_forget`
  tool so the agent that learns a skill can also retire one; `persona forget <id>` / `unforget <id>`
  for self-memories, which had no delete at all and so could only fill up; `persona save` versions
  the previous card into `.archive/` instead of overwriting it, and `persona delete` archives the
  self-memory directory rather than `remove_dir_all`-ing it.
- **`/memory review` in the REPL.** `cmd_review` (list / promote / drop / clear) was fully
  implemented and reachable only from the CLI, so a 29-item queue sat untouched. Its output moved
  from `println!` to `tui::emit_line`, which is what makes it visible mid-chat under the retained
  renderer.
- **Cross-kind graph edges.** Endpoints take a namespace (`skill:`, `persona:`) so co-retrieval can
  link a skill or an insight to the facts it fired alongside. Memory ids are `[a-z0-9-]` slugs, so
  `:` cannot collide with one — verified against the real store (0 of 243 ids, 0 of 189 endpoints).
  Old `graph.tsv` files load unchanged, and `expand_with_graph` iterates real entries so a namespaced
  node it cannot resolve is skipped rather than corrupting recall. Graph-informed skill ranking is
  behind `AIZEN_SKILL_GRAPH_RANK`, **default off**, pending a bench.

### Changed
- **`memory list` and `skill list` are laid out for triage** — deciding what to fix or drop is the
  question these listings exist to answer, and neither could. `memory list` interleaved two types
  into 81 alternating headers and led every row with a 55-char slug; it now groups each type once,
  shortens ids to the first unique prefix (checked against the whole store, since `resolve_in`
  matches by substring — the printed handle always resolves), and adds a triage column that marks
  only the minority worth looking at: `used` (the model cited it), `cold` (never recalled), `low?`
  (the extractor was unsure). Blank for the unremarkable majority — an earlier cut printed `unused`
  on 206 of 243 rows, and a marker that fires on almost everything sorts nothing. `skill list` gains
  an aligned name column, a load count, `cold` for never-loaded, and a footer that names the commands
  that act on a row. Both footers name only commands that exist — the first draft of this hint
  advertised three that didn't.
- **`aizen memory list current` works.** The REPL has always taken the scope positionally, so that is
  the form muscle memory reaches for; the CLI accepted only `--scope` and answered a positional with
  `error: unexpected argument 'current' found`, which reads as "scopes aren't supported". Both forms
  are accepted now; passing both at once is a parse error rather than a precedence puzzle.
- **Session files are garbage-collected at startup.** A long-lived daemon kept one file per chat that
  ever messaged it, forever, each holding conversation text. Files untouched for 30 days are removed
  (`AIZEN_SESSION_TTL_DAYS`, `0` disables).

## [0.5.6] — 2026-08-03

A single bug wasted one tool round in most multi-step turns: the **first** call of a batch ran with
no arguments and came back `error: missing required string arg`, and the model then repeated the
identical call, which worked. It hit every tool family — `file_glob`, `file_read`, `web_search`,
`web_fetch` — because the cause was in the streaming layer they all pass through, not in any tool.

### Fixed
- **A streamed tool call is no longer dispatched before its arguments finish arriving.** The
  accumulator that reassembles `delta.tool_calls[]` treated "a fragment landed on a different slot"
  as proof the previous call was complete. That rule breaks in two shapes that are common in
  practice: a provider that omits `index` (the field is `#[serde(default)]`, so every fragment
  reports slot 0), and an opening frame that carries only `id`+`name` with `arguments: ""`. Either
  way a call was snapshotted with empty or half-written JSON. `""` then parsed as a perfectly valid
  `{}`, so eager execution started the tool with **no arguments at all** — while the transcript row
  rendered from the completed call and displayed the correct target. That asymmetry is exactly why
  the failure looked impossible: `web_fetch github.com → missing required string arg 'url'` is a row
  whose target came from one object and whose result came from another. A call is now only completed
  when its arguments parse as a whole JSON object, and argument fragments that carry no `id` (per
  spec, they don't) follow the call that is actively streaming rather than defaulting to slot 0.
- **Eager execution refuses a call with empty arguments.** Independent of the above, as a guard
  against other provider shapes. Eager start is an optimisation, not an obligation: a call whose
  arguments haven't arrived is not a call the model asked to run empty, and running it can only
  produce an error round. It falls through to normal execution with complete arguments. Skipping it
  deliberately does **not** trip the eager barrier — one deferred call must not cost every later call
  its head start.

### Added
- **Schema-driven argument repair for every tool.** Some models send the right value under a wrong
  key, or wrap the whole object. Rather than an alias table per tool — which cannot keep up with ~40
  builtins plus LSP, skills, and MCP — repair reads the tool's own `parameters()` schema at the one
  chokepoint every dispatch passes through, so **new tools are covered the day they are added**.
  Three rules, all shape-only, none inventing content: unwrap a single wrapper (`{"input":{…}}`,
  `{"args":{…}}`, `{"parameters":{…}}`, …) when the inside satisfies the schema; map a stray key to a
  missing one **only** when exactly one required key is absent and exactly one undeclared string key
  is present (`{"q":…}` → `{"query":…}`, `{"file":…}` → `{"path":…}`); and coerce a number, bool, or
  single-element array into the string the schema asks for. Anything ambiguous is left alone —
  guessing wrong is worse than the error. Every repair prints (`→ file_read: arg 'file' → 'path'`),
  so a model that keeps calling a tool wrong stays visible instead of being silently patched over.
  A registry-wide test asserts that every tool's own valid shape passes through untouched.
- **Missing-argument errors now carry the schema.** `error: missing required string arg 'pattern'`
  told the model nothing about what the right key was called. It now reads
  `missing required arg 'pattern' for file_glob — required: pattern (string). You sent:
  {"glb":"**/*.rs"}. Call again using those exact key names.` — tool name, the required list, and
  the arguments actually sent, which is what a retry needs to succeed on the second attempt rather
  than the third.
- **`web_search` declares `q`.** The handler had always accepted `q` as an alias for `query`, but the
  schema set `additionalProperties: false` without declaring it, so a strict gateway rejected the
  call before the tolerance could apply.

## [0.5.5] — 2026-08-02

Supersedes 0.5.4, which shipped the OSC 8 hyperlink layer and the bottom-of-transcript spinner in a
state that misfired in ordinary use: links attached themselves to Vietnamese prose, and the working
line drew a caret that read as a stuck second cursor. Both are fixed here, alongside two features
0.5.4 did not have. **0.5.4 remains published and downloadable** — its binaries are unchanged; this
is the version to install.

### Fixed
- **A line that merely STARTS with `/` is no longer swallowed as a command.** Every dispatch surface
  did `strip_prefix('/')` and treated the remainder as a command name, so an XPath
  (`/html/body/div[2]`), a POSIX path (`/usr/bin/python`), and prose (`/help... abcd`) were all
  answered with "unknown command" instead of reaching the model. A single shared `slash::classify`
  now decides, and all three surfaces (retained TUI, plain REPL, Telegram host bot) call it. The
  shape rule — must start with a letter, then only `[A-Za-z0-9_:-]` — rejects every false-positive
  class actually hit, deterministically rather than by guessing. A near-miss (`/claer`) prints
  `did you mean /clear?` and **stops**: auto-running the nearest match would let one slipped
  keystroke wipe a conversation. A command that takes no arguments followed by ≥2 words of prose
  (`/model của aizen là gì?`) is read as a question about the model, not a request to open the
  picker. The host bot applies only the shape gate, not the whole verdict, because its catch-all
  deliberately runs an unrecognized name as a shell command and it has its own vocabulary
  (`/sh`, `/cd`, `/pwd`) that isn't in the REPL catalog.
- **Hyperlinks no longer attach to text that merely looks like a path.** 0.5.4 started a path scan at
  any `/`, so every Vietnamese `và/hoặc` in the transcript grew a `file:///hoặc` link, as did
  `and/or`, `parser/lexer`, `input/output` and `15.000/kg`. Two rules fix it: a path may only START
  at a word boundary, and a path-shaped candidate is only linked **if that file actually exists on
  disk** — existence is the one signal that separates a path from prose, because their shapes are
  identical. Resolution is cached (bounded, short-TTL) so a steady screen costs zero syscalls at
  render rate. UNC paths (`\\server\share`, `//server/share`) are refused in the shape layer, before
  any syscall: probing one opens an SMB connection to a host named by model output, which can leak
  Windows credentials. A URL split across two wrapped rows is rejoined and both halves are linked.
- **The working line no longer draws a caret of its own.** It rendered a `▏` imitating a text cursor,
  which misread badly: the terminal's real cursor is already visible a few rows below in the input
  box (ratatui calls `show_cursor` on any frame that sets a cursor position, so the `Hide` at session
  entry lasts exactly one frame), and when the caption was empty between tool steps the imitation
  collapsed onto the spinner as `✦ ▏` — a second cursor apparently stuck to the glyph. The real one
  stays: the input box genuinely accepts queued messages and Alt+↵ steering while a turn runs, so a
  cursor there is correct. One cursor on screen is the right number.

### Added
- **`/config` → Sub-agents & roles.** Model, base URL, and API key for all four roles
  (`subagent_default`, `summarizer`, `oracle`, `apply`) were reachable only by CLI flags — and only
  `subagent_default` had any — with no way to read back what was stored. There is now a menu for
  each, plus editors for the model→endpoint registry and per-specialist pins. The URL step **probes**
  the endpoint instead of trusting it (a typo'd gateway used to surface as a failed dispatch in the
  middle of an unrelated task) and, when the probe fails, says why and offers to save anyway. The key
  step offers `env:VAR` indirection first, so a credential need never touch disk. When a probe
  reaches a live endpoint the model step becomes a picker over that endpoint's real model list.
- **`aizen config show` prints the roles it is holding.** A `Sub-agents & roles` section lists all
  four roles and the registry. Literal keys are masked and `env:VAR` references print the variable
  NAME with `✓`/`✗ (unset)` — enough to spot a forgotten `export` without the value ever appearing.
  URL userinfo is redacted. The `oracle` row also shows self-review on/off, because configuring that
  role is what switches self-review on.
- **CLI flags for the three roles that had none:** `--summarizer-model/-base-url/-api-key-ref`,
  and the same trio for `--oracle-` and `--apply-`. An empty value clears; clearing the last field of
  the last role drops the `roles` object entirely rather than leaving `{}` behind.
- **An agent card can carry its own gateway.** `base_url:` and `api_key_ref:` in a specialist's
  frontmatter override the model→endpoint registry — the registry says where a model generally
  lives, the card says where THIS specialist calls it, and the more specific statement wins.
  `api_key_ref` is honoured **only** as `env:VAR`: cards live in `.claude/agents/`, a directory
  people commit, so a literal key written there is treated as absent rather than used. An `env:VAR`
  that isn't exported leaves the inherited key in place instead of blanking it — an unset variable is
  a forgotten `export`, and sending an empty Authorization header would turn that into an opaque 401.
- **The working spinner blooms instead of blinking.** The two-frame `✦⇄✧` pulse read as a blink; it
  is now an eight-frame bloom `✦ ✶ ✷ ✹ ✺ ✹ ✷ ✶` that opens and closes back to the brand mark, so
  every turn starts on `✦`. Every frame is one display cell — the caption sits immediately to its
  right, so a double-width frame would shove the whole line sideways once per cycle. That constraint
  rules out `✳` and `✴`, which are `Emoji=Yes` and get drawn two cells by an emoji font even though
  their East_Asian_Width is Narrow; a width measurement does not catch this, so the test checks emoji
  membership instead.

## [0.5.4] — 2026-08-01

### Fixed
- **Live streaming now renders identically to replayed sessions.** The retained TUI previously used
  a separate `render_retained` renderer (pulldown-cmark) for assistant blocks while they streamed,
  but `MarkdownStream` for replay — so bold, inline code, and the moonlight gutter all looked
  different live than when you reopened the session. Both paths now share a single `MarkdownStream`
  renderer; the dead `render_retained` (~230 lines) and its orphaned pulldown-cmark import are removed.
- **Input box no longer hides text after a large paste.** When ≥5 lines were pasted the box
  collapsed to a static `"↵ N lines pasted"` chip, hiding anything typed afterward. It now shows a
  compact `↵N ·` prefix alongside a live window around the cursor — paste then keep typing normally.

### Added
- **Working spinner moves to the transcript bottom (Claude-CLI style).** The `✦⇄✧` brand-pulse
  spinner and a typewriter caption now appear at the bottom of the chat transcript while the agent
  works — `✦ Reading retained.rs  12s · Esc to stop`. The caption shows the current tool action
  ("Reading …", "Run cargo test") or a whimsical verb ("Pondering…", "Distilling…") between steps,
  typing out one character per tick. The old HUD working pill is removed; the HUD stays calm always.
- **Ultimate mode gold input box.** Activating `/ultimate` recolours the `❯` prompt arrow and both
  framing rules to gold, tying the visual cue to the `✦ ultimate` chip. Reverts to moonlight silver
  when deactivated.
- **`@` file picker overlay.** Typing `@` in the input box opens a file-completion overlay (mirrors
  the `/` command palette): ↑↓ to navigate, Tab or Enter to complete, Esc to close. Shows the top 12
  matching files from the project filtered by the prefix after `@`. The underlying `@file` inline
  expansion on submit was already present; this adds the real-time picker UI.
- **OSC 8 hyperlinks in the transcript.** URLs (`https://…`) and file paths with known extensions
  (`src/foo.rs`, relative paths containing `/`) are wrapped in OSC 8 terminal hyperlinks after each
  draw — Ctrl+Click opens the URL in the browser or the file in Explorer (Windows Terminal 1.19+,
  WezTerm, iTerm2). Injected post-draw via `backend_mut()` so ratatui's cell model is unaffected.

## [0.5.3] — 2026-08-01

### Fixed
- **The shared HTTP client no longer caps a healthy streamed turn.** 0.5.2 added `.timeout(1800s)` to
  the shared client as a catch-all backstop. reqwest applies that "from when the request starts
  connecting until the response body has finished" — and this same client drives the REPL's streamed
  turns, so it did not cap only pathological hangs, it cut off a long-but-healthy answer still emitting
  tokens. The stall protection a stream actually needs is per-event (`read_timeout` plus the inter-event
  watchdog in `llm::client`), which can tell "gateway went quiet" from "answer is long"; a total deadline
  cannot. The ceiling is removed, and a regression test proves the shared client reads a slow body to
  completion while a deliberately-ceilinged control client truncates it.
- **A large `stdin` write to a git subprocess can no longer park while `transaction.lock` is held.** The
  Time Machine's piped git helper wrote the whole payload from the calling thread before entering its
  deadline loop; on a payload past the OS pipe buffer (~64 KB) that `write_all` blocks until the child
  drains stdin, and if the child never does, the calling thread parks *while holding the transaction
  lock* — the "edit blocked for 15s" strand. The write now runs on a spawned thread joined under the
  same deadline, so a child that never reads can no longer freeze the lock holder.
- **The Time Machine timeout message no longer claims "nothing was changed."** A git operation that
  timed out mid-run may well have changed something; the bail now says the work may be partially done and
  points at `aizen time doctor` to check, instead of a false all-clear.

### Changed
- **Every non-streaming background/chore model call is now bounded in wall-clock time.** The secretary,
  persona reflection, all four compaction/summarize closures (serve, sticky REPL, plain REPL, `/compact`),
  the self-review oracle, memory reconcile (both the background and CLI paths), `/handoff`, the
  persona-distill call, and the aside `?` worker each routed the non-streaming `chat_with_tools`, whose
  only native guard is reqwest's `read_timeout` — and that fires only when the socket goes BYTE-silent. A
  gateway that accepts the POST and then keepalive-drips (or never writes the body at all) left the call
  parked forever, silently killing that feature for the rest of the session with no error surfaced. A new
  `chore_chat` helper wraps all of them in the same wall-clock deadline a sub-agent gets
  (`subagent_call_timeout`, 300s, `AIZEN_SUBAGENT_CALL_SECS` to change). The two MAIN agent-loop `chat`
  closures are deliberately left unwrapped — a flat cap there would re-introduce the streamed-turn bug
  above on a legitimately long turn.
- **Interactive turns now retry fast and report decisively; `/goal` still retries forever.** The ordinary
  REPL branch shared `/goal`'s backoff, whose 30s ceiling meant a late retry could sit for half a minute
  before reporting — the "it hangs" feeling. A new `interactive_backoff_ms` (base 300ms, cap 4s) backs
  off fast, so a ~10-attempt chain lands around ~30–40s total with no single wait hitting 30s, and the
  interactive retry budget is raised to 10 (a `⟳ transient: retry 7/10` counter, then a clear error) at
  the two interactive REPL sites only. `/goal` keeps `goal_backoff_ms` (cap 30s) and its infinite
  transient retries — it runs long with nobody watching. Sub-agent (4) and background/quiet (2) budgets
  are unchanged, and permanent 4xx still reports immediately without wasting a retry.

## [0.5.2] — 2026-07-30

### Added
- **A delegated run now has a deadline, not just a step budget.** Every budget in the agent loop counted
  STEPS, and steps are not time: a `task` dispatch is bounded at 150 steps by default (480 at
  `max_steps: 80`), but each step may legitimately spend minutes — a 300s model call, a 120s
  `shell_run` — so a dispatch always finished with no answer to *by when*. `AgentConfig::deadline` and
  `StopReason::Deadline` answer it: one hour per dispatch by default, `AIZEN_SUBAGENT_WALL_SECS` to
  change it (floor 60s, `0` opts out), applied to `task` children and workflow children alike. Three
  details are what make it a real ceiling rather than a suggestion. It is checked at the **loop
  boundary**, not imposed as an outer `tokio::time::timeout` — an outer timeout drops the future
  wherever it happens to be suspended, mid-`spawn_blocking` with a write already on disk, discarding
  both the transcript and any record of what changed; reaching the boundary means the current step
  completes, is recorded, and the run ends the same orderly way Esc does. The budget is one absolute
  instant computed **once per dispatch**, so the continuation loop re-enters with the same deadline
  instead of three budgets multiplying into three times the ceiling. And a deadline is never resumable,
  because a continuation would restart the clock. The cost, sized for deliberately: overshoot is bounded
  by the longest single step, not by zero. No synthesis call on that path either — writing a summary
  would cost one more model call on top of an already-blown ceiling, so the caller labels the partial
  work from the stop reason instead.
- **`/workflows stop <#id|name>` ends one sub-agent and leaves the rest running.** Esc is
  all-or-nothing by design, so a fan-out with one child wedged behind a slow model call left no option
  but to kill the whole turn. Cancellation is now a tree: `TurnCancel::child()` derives a token the
  parent can cancel but which cannot cancel the parent, so Esc still cascades to every descendant while
  a targeted stop reaches exactly one row. A numeric needle is an exact id match (`#3` never also stops
  `#30`); anything else is a case-insensitive substring of a row's name or label. A row that matched but
  published no stop handle is reported as such rather than as "nothing matched", which would send you
  hunting for a typo that isn't there.
- **The `/workflows` panel refreshes itself while it is open.** A one-shot snapshot froze the elapsed
  times at whatever second you pressed the key, so a fan-out you were watching looked stuck. The body is
  now republished every 900ms without re-opening the overlay, which preserves your scroll position; a
  generation counter retires the refresher as soon as the panel closes or another overlay replaces it, so
  two panels can never write the same surface. Long runs read `2h05m` instead of `125m00s`, and run ids
  print as the short `#3` you can actually retype.
- **`/workflows` and `/workflows stop` work *during* a turn.** The REPL's turn `select!` polls only the
  turn future and the cancel channel, so a queued slash command isn't dequeued until the turn ends —
  which makes this particular command useless twice over: a stop that lands after its target finished is
  no stop, and a live activity panel you can only open once the fan-out is over has nothing to show. Both
  are now serviced on the input thread, which is safe precisely because they touch nothing the REPL owns.
- **`?` prefix: a side question that does not disturb the turn in flight.** `? what does this flag do`
  is answered by a separate long-lived worker from a read-only snapshot of the live conversation, in one
  tool-less call, printed dim beside the main stream. It never mutates history, never arms cancel, never
  flips the working flag — the running turn is oblivious. The endpoint is resolved per question, so a
  `/model` switch mid-session is honored. Refused or unavailable, the text falls through as an ordinary
  message with the marker stripped, so nothing is ever swallowed.
- **Right-click a highlight to copy it.** Drag-release already copied, but silently — no way to tell it
  happened and no way to ask again without re-dragging. The menu only appears when there is a selection
  to copy, and the confirmation line reports what actually landed: `copy_to_os_clipboard` returns a
  result now, so on a platform where the clipboard is a no-op a button you deliberately clicked says so
  instead of claiming success.

### Changed
- **The concurrent sub-agent cap is derived from the machine instead of pinned at 5.** The old constant
  was one number for every box. The gate now bands off the core count (2..=16), `AIZEN_MAX_SUBAGENTS` or
  `max_parallel_subagents` override it, and only a 64 disaster-ceiling is absolute. The per-call cap on
  `workflow` rises 5 → 32 to match: the model requests as many tasks as the work needs, and the gate
  bounds how many run *at once* — extra tasks queue into the next chunk rather than being refused.
- **A sub-agent spawn line says what the child was sent to do.** `task` and `workflow` had no arm in the
  tool-target formatter, so they fell through to the generic argument dump and rendered a clip of escaped
  JSON (`{"prompt":"Investigate the retai…`). A child runs quiet for minutes and only its merged result
  comes back, so that line is the only place its subject ever appears. It now reads `coder · cwd-audit`
  — the model's own `label` when it passed one, else the prompt's opening line — and the role is spelled
  out rather than left implicit, because read-only versus write-capable is the one thing the *who* half
  carries. A fan-out names its first two children and marks the rest. The `/workflows` board uses the
  same clipper, so one task cannot appear under two different names on two surfaces.
- **A workflow child's model call has the same deadline as a `task` child's.** None of a child's budgets
  count time, and `join_all` has no per-task timeout, so one call that never returned stalled the entire
  chunk — every sibling done and the fan-out still producing nothing. An elapsed call is now an ordinary
  error that lands as a failed task while its siblings and the synthesis carry on.

### Fixed
- **A `>` steer typed while a turn was starting up fell into the post-turn queue.** Two flags disagreed
  for the whole of a turn's prep: `turn_in_flight()` reads the armed cancel token and was already true,
  while the steering mailbox was armed ~150 lines later, just before the working flag flipped. The `>`
  prefix asks the mailbox, so a steer typed during `@file` expansion, prompt-lane rebuild, or retrieval
  was refused and queued — and retrieval is the slow part, so the window was widest on exactly the big
  tasks worth steering. The mailbox is now opened in the same breath as the cancel token. What makes that
  safe is a guard: prep has three early exits (an unconfigured endpoint, a `#remember`/`!shell` input, an
  Esc during prep) that never reach the end-of-turn disarm, and a mailbox left armed with no turn behind
  it would accept steers into a slot nothing drains — silently eating input, strictly worse than queueing
  it. The guard closes the mailbox on every exit path and re-queues anything the turn never took.
- **An edit could fail with "resource is busy … transaction.lock" while no other aizen was running and
  the lock file was absent from disk.** All three observations were consistent, and none of them pointed
  at the cause: an OS advisory lock lives on an open **handle**, not on the path, so it is invisible in a
  directory listing, deleting the file cannot release it, and it can be held by a stranded thread inside
  the very process reporting the error. The holder was an unbounded internal `git` child from an earlier
  turn — `Command::output()` blocks until every pipe reaches EOF, and on Windows a grandchild that
  outlives its parent keeps the inherited write end open, so EOF may never arrive. Every git spawn in the
  Time Machine is now deadline-bounded (120s, `AIZEN_GIT_OP_TIMEOUT_SECS`) with its process tree
  contained, which converts a permanent strand into an ordinary error that unwinds through `Drop` and
  frees the lock. The stdin-fed spawns start their pipe drains *before* writing, closing a second latent
  deadlock where a child emitting more than one pipe buffer while we were still writing would block us
  and itself forever. Lock acquisition observes the turn's cancel token, so Esc during contention returns
  at once instead of waiting out the 15s timeout. And when contention does happen, the error explains
  where the lock actually lives and what to do — a fail-closed gate must not also be a dead end.
- **Checkpoint reads no longer corrupt non-UTF-8 files.** The bounded runner decoded child output
  lossily, which is correct for a shell command's text but silently rewrites every invalid byte to U+FFFD
  — and `git cat-file blob` output *is* file content, as `-z` output is NUL-delimited paths. Bounded
  execution now has a byte-exact form, and the lossy one is a thin wrapper over it so a fix reaches both.
- **A request that never returns can no longer hang for the life of the process.** `read_timeout` only
  fires when the socket goes byte-silent, so a gateway that keeps a connection warm with keepalives or a
  trickle of SSE comments could hold a request open indefinitely without tripping it. A 1800s absolute
  ceiling now sits under every request as a backstop — far larger than any legitimate turn, so it never
  fights a long streamed answer.
- **`/import` offered transcripts from unrelated sibling projects.** The project filter treated an
  ancestor of the checkout as a match, which reads symmetric and is wrong: the project root is already
  resolved to the checkout's toplevel, so a shallower recorded directory lies outside the boundary by
  construction and can only name marker-less parents. Measured on this developer's own history, 18
  transcripts were offered for one checkout of which 9 were ancestor over-matches — six recorded in the
  home directory, three in a parent folder holding five unrelated siblings. The cost was not just a noisy
  picker: importing an over-matched transcript stamps this project's provenance onto a conversation from
  another one, and that stamp cannot be undone.

## [0.5.1] — 2026-07-30

### Added
- **Several aizen windows on one repository can now see each other: `/team` and `aizen team`.** Opening
  aizen in four terminals against one checkout used to leave `git diff` as the only record — the union
  of everyone's work, with no way to tell which window wrote which line, so the window doing the review
  could neither check one task nor commit only it. Each session now publishes what it is doing and
  which files it changed:
  - `/team status` lists every session in the repository — state, task, files touched, overlaps — and
    a startup line names the windows already working when a new one opens. A window whose process is
    gone reads as `abandoned` rather than silently lingering as "working": the owner holds an OS lock
    for its lifetime, so a lock a reader *can* take is proof the owner left.
  - `/team diff <session>` shows what one session changed, `-p` for the patch. Each file is measured
    from the pre-edit checkpoint of the turn that first touched it *in that session*, so the answer
    stays right even after another window overwrites the same file — the earlier pre-image is still a
    blob in that session's own checkpoint tree.
  - `/team claims` shows which session owns each changed path. Two sessions editing one file is
    **warned about once, for both sides, and never blocked** — the second writer is often the point.
  - `/team commit <session>` stages exactly that session's files, prints the review, and commits only
    after an explicit confirmation; `--dry-run` unstages again. It refuses while the session is still
    running unless `--force`, and says plainly when a file is shared, because git tracks content and
    not authorship: committing a shared file carries the other window's changes to it along.
  - `/team task <text>` pins this window's description; otherwise it is taken from the turn's prompt.
    `/team done` marks the window finished so a coordinator may commit it.
  Attribution is exact rather than inferred: the workspace writer lease is already held exclusively
  for the length of a turn, so two aizen sessions on one worktree are serialized at turn granularity.
  Edits made by an external editor, or by git run by hand *while* a turn is in flight, land inside that
  window and are attributed to whoever held the lease — documented rather than silently corrected.
- **Isolated worktrees for sessions that shouldn't share a tree: `aizen work new|list|remove`.** Each
  gets its own linked worktree and `aizen/<name>` branch, and shows up in `/team status` alongside
  shared-tree sessions. `work remove` refuses while a worktree holds uncommitted changes, unmerged
  commits, or a live session, and keeps the branch either way, so nothing it removes takes commits with
  it.
- **`team_status` tool**, so the agent can check who else is editing a file before it rewrites it.
- **A whole-file overwrite can no longer discard another window's work silently.** The CAS behind
  every write compares against a fingerprint taken microseconds earlier inside the same call, so it
  closes the read→write window and nothing wider. The torn cycle it cannot see is the one that
  matters across windows: session A reads a file, session B rewrites it a turn later, A writes from a
  stale idea of the content. `file_edit`/`multi_edit` survive that on their own — `old_string` is
  matched against a fresh read, so a rewritten region simply fails to match — but `file_write` had no
  anchor at all and its CAS passed against whatever was on disk. Every read now records what this
  session saw, and a full overwrite refuses when disk no longer matches it, naming the re-read as the
  recovery. A file this session never read is refused only when a *live* peer session claims it, so
  single-window flows that legitimately regenerate a file are untouched. `file_move --overwrite`,
  which has no CAS of its own, gets the same guard.
- **A sub-agent that writes no longer deadlocks against its own parent.** The workspace lease is held
  for the length of a turn, and both `LockFileEx` and `flock` conflict with a second handle in the
  *same* process — so once a turn had written anything, a delegated write-capable `task` waited out
  its 15-second timeout and failed with "workspace writer lease was not acquired" for an edit nothing
  was contending. Nested acquisition inside one process is now reference-counted: two nested holders
  are one writer as far as any other process can tell, and the OS lock is released only when the
  outermost handle drops, so cross-session exclusion is unchanged.
- **`/team commit` holds the lease while it stages, and commits only what was reviewed.** `git add`
  reads the working tree, so staging while a peer window is mid-turn could capture half of an
  in-flight edit. Staging now takes the lease for that step — released before the confirmation
  prompt, since blocking every window for as long as a human takes to decide is worse than the race
  it would close. The gap that leaves is closed by identity instead: the index is hashed at review
  time and the commit refuses if it no longer matches, so content nobody looked at cannot ride along.
- **A file two sessions both edited can now be committed for one of them alone.** Previously this was
  the documented limit of the feature: git tracks content, not authorship, so committing a shared file
  carried the other window's changes to it along, and all `/team commit` could do was warn. It is now
  separated instead. The starting point is the last commit — deliberately *not* this session's pre-edit
  checkpoint, which already contains the peer's work whenever the peer edited the file first — and only
  this session's own turns are replayed onto it, one three-way merge each. Those turns are recoverable
  because writes are already serialized: the workspace lease is held for a whole turn, so between one
  turn's pre-edit checkpoint and the next turn's anywhere in the worktree lies the work of exactly one
  session. The reconstruction is staged **index-only**: disk keeps the union, so the peer's live edits
  are never overwritten, and the commit still holds one session's version. Where the two genuinely
  collide, the file is refused with the reason rather than merged on a guess — and since git's merge
  granularity is the hunk rather than the line, edits on adjacent lines count as colliding. A refusal
  rolls the whole stage back, because a half-separated index is the one state where `--force` would
  commit a mix nobody chose. Checkpoints that have been pruned away are reported the same way: without
  a turn's pre-image there is no way to know what it changed, and guessing would silently drop work.
- **Setup verifies the connection instead of trusting it.** `/config` → Connection and first-run setup
  now check each step against the live endpoint before accepting it:
  - A **provider picker** (OpenAI · Anthropic · OpenRouter · Groq · DeepSeek · Ollama · Custom
    gateway) fills in a base URL that already carries the right version path, plus a link to where
    that provider's keys live. The Custom-gateway row prints the endpoint you are already on when it
    matches no preset, so a self-hosted or proxy user sees their own URL instead of only everyone
    else's.
    Anthropic is reachable because it serves an OpenAI-compatible surface at
    `https://api.anthropic.com/v1/`; its native `GET /v1/models` wants `x-api-key` +
    `anthropic-version`, which is now sent alongside the Bearer token when the host is theirs.
  - A **base URL is probed before the key is asked for**, so a bad URL is never mistaken for a bad
    key. A 404 on `{base}/models` also gets diagnosed: when the URL has no `v<N>` segment, the `/v1`
    form is offered as the next default. `/v1beta` counts as versioned — suggesting `/v1beta/v1` would
    point at nothing.
  - An **API key is verified and re-asked on rejection**. Only a 401/403 re-prompts; an unreachable
    endpoint says so and offers to keep the key, since the key may be fine and only the network at
    fault.
  - **Web-search keys (Tavily, Jina) are verified with a real search.** A rejected key re-prompts, and
    an exhausted-quota 429 counts as rejected — a key that cannot serve a search is not "fine".
  - A **spinner** runs during each check, and each step reports its own verdict.
- **`/config` → Memory**, a section of its own: auto-learn, which retrieval tiers actually run, and
  which embedding model to use (or `auto`). It reports the tier that will really run rather than the
  config flag, since the cargo feature, `AIZEN_MEM_DENSE`, and whether a model is installed all get a
  vote. New `embed_model` config field, overridden by `AIZEN_EMBED_MODEL`. `config show` gained the
  matching Memory section and now lists the Jina key too.
- **Container hosting: `Dockerfile`, `docker-compose.yml`, and `deploy/k8s/`.** Two-stage build
  (`rust:1.96-slim-bookworm` → `debian:bookworm-slim`, glibc because the release binary is glibc-linked)
  producing a non-root image with the runtime the agent actually shells out to: `git`, `curl`, and
  `tini` as PID 1 to reap the builds, tests, and language servers it spawns. No port is published,
  because the daemon listens on nothing — Telegram is long-poll, Discord an outbound websocket — so
  the container needs no ingress and no public URL.
  The Kubernetes manifests are a StatefulSet with **one** replica and a ReadWriteOnce volume, and that
  is the only correct shape rather than a starting point: Telegram allows one `getUpdates` poller per
  token (a second gets 409 Conflict forever), the on-disk stores guard writes with local file locks
  that do not coordinate across pods, and turns are processed serially so approval routing stays
  race-free. Ships with a NetworkPolicy that denies all ingress and all *private* egress — including
  `169.254.169.254`, the cloud metadata endpoint that hands out node credentials. That policy is the
  layer that matters, because `net_guard`'s SSRF floor only covers the web tools and the agent also
  has a shell, where `curl` never passes through it.
- **`aizen serve --health`**, a liveness probe for container and orchestrator use. It reads a heartbeat
  file the daemon stamps from inside its own `select!` loop and exits 0 or 1. The beat comes from the
  loop itself on purpose: a detached ticker would keep beating while the loop was wedged, which is the
  one failure a liveness probe exists to catch. The heartbeat records `idle` vs `busy` rather than a
  bare timestamp, so a probe tight enough to notice a wedge (90s idle) does not restart a pod running
  a legitimate 10-minute build (1800s busy, both tunable via `AIZEN_HEALTH_MAX_*_SECS`). An unknown
  state falls back to plain freshness, so an old binary probing a newer daemon mid-rollout does not
  kill a healthy pod.
- **Reading a whole source file suggests the cheaper symbolic path.** A `file_read` that pulls an
  entire code file (a language server exists for its extension, LSP on, 400–2000 lines) now prepends
  one advisory line: if you need a single item, `lsp_document_symbols` then `read_symbol` costs a
  fraction and keeps the context lean, because that whole file is re-sent every turn. It is a hint,
  not a gate — the full content still follows, since reading the whole file is sometimes exactly
  right. It stays silent for prose files, when LSP is off, and below the threshold where the read is
  small enough not to matter.

### Changed
- **API keys are visible while you type them** (setup, Connection, and the search keys). A masked
  field hides a truncated paste or a stray newline, and the value is one keystroke from a plaintext
  config file either way — so the echo was buying nothing. Stored keys are still masked everywhere
  they are displayed back.
- `memory_auto_learn` moved from the Session section to Memory, so it is asked once, next to the
  retrieval knobs it feeds.

### Fixed
- **`aizen serve` ignored SIGTERM.** Only Ctrl-C was handled, and SIGTERM is how systemd, `docker stop`,
  and Kubernetes all ask a process to stop first — so the graceful path never ran and the daemon was
  killed outright some seconds later. Every child it had spawned (builds, test runners, language
  servers) was orphaned rather than reaped, and the heartbeat file was left behind claiming a live
  process. It now handles both, logs which signal arrived, kills the process tree, and clears the
  heartbeat before exiting. Windows has no SIGTERM; there `ctrl_c()` covers Ctrl-C and Ctrl-Break,
  which is what the platform offers.
- **The agent stopped mid-task instead of finishing it.** Three independent holes in the top-level
  loop, all of which surfaced the same way — partial work handed back with "say continue to carry
  on". A fix landed for sub-agents earlier; the loop the REPL actually drives still had all three.
  - **The step cap cut off runs that were still working.** Exhausting the budget granted one
    extension (25 → 50 steps) and then hard-stopped, synthesizing a "here's how far I got" summary.
    A large but *healthy* task — one producing new evidence every turn — was ended by a step counter
    rather than by anything about the task. It now grants itself another `max_iters` worth (up to
    `max_continuations`, default 3) when the run is neither stalled nor looping, injecting a
    `[continue]` turn that says to carry on from where it is rather than restart or re-plan. Bounded
    by `auto_extend_to + max_continuations × max_iters`: a run that goes flat or starts repeating
    itself loses the grant immediately, so "don't stop early" cannot become "never stop".
  - **The stall guard ended runs with work still declared open.** A few turns without new evidence
    stopped the run even when the model's own todo list still held pending items — abandoning
    exactly the work it had said was unfinished. With an open plan it now spends one bounded recovery
    (`max_stall_recoveries`, default 1) demanding a genuinely different approach; the ledger keeps
    what it had already ruled out, so a re-read cannot pose as fresh evidence. Going flat a second
    time still stops.
  - **One transient API error discarded the whole turn.** The top level ran with zero retries on the
    reasoning that the user is right there to re-ask — but a 429/5xx twenty steps in threw away all
    twenty. It now absorbs a small number (2, versus 4 for unwatched sub-agents, since each attempt
    prints a retry line and reads as progress rather than a hang). Permanent 4xx still fails fast.
  Continuation alone does not cover a model that stops early *on its own* — the loop sees a clean
  `Done` — so both system prompts gained a persistence clause: a large task is a reason to keep
  working, and a plan or an outline is not a result.
- **The Hugging Face cache was never scanned on Windows.** `scan_hf_cache` reached
  `~/.cache/huggingface/hub` only through `$HOME`, which does not exist on Windows outside a POSIX
  shell — so a model another tool had already downloaded sat there unseen. The home lookup is now
  `USERPROFILE` then `HOME` (matching `aizen_home()`), and the root list follows the precedence
  `huggingface_hub` documents: `HF_HUB_CACHE` → `HF_HOME/hub` → `XDG_CACHE_HOME/huggingface/hub` →
  `<home>/.cache/huggingface/hub`. The old `%LOCALAPPDATA%/huggingface/hub` entry is gone: HF uses
  `~/.cache` on all three platforms, so that path pointed at a directory nothing ever creates.
- **A dense build with no model no longer degrades recall.** `settings().enable_dense` tracked only
  the cargo feature, but with no model2vec model installed the embedder falls back to the
  non-semantic `HashEmbedder` — which then got fused into RRF. Worse, the query gate admits the dense
  tier precisely when lexical coverage is LOW, so the hash embedder was mixed in exactly on the
  ambiguous queries where it does the most damage (the P6 bench measured literal-slice precision
  dropping 0.667 → 0.200). Dense now defaults on only when the feature is built AND a model is
  actually present; `AIZEN_MEM_DENSE` still overrides both ways.
- **Model weights are loaded once per process instead of once per search.**
  `StaticModel::from_pretrained` reads and parses the entire `model.safetensors` (512 MB for
  `potion-multilingual-128M`), and the embedder was constructed on every search — both memory
  retrieval and code retrieval, so twice a turn. The load is now memoized on the weights file's
  identity (path + mtime + length). Discovery itself is still uncached, so a model downloaded
  mid-session is picked up on the next search, and a model replaced in place is reloaded rather than
  served stale.

- **`/import` could not be navigated.** `import` was missing from the one table that decides whether
  a slash command owns stdin, while `/sessions` — the same dialoguer picker — was listed. So the
  retained TUI's input thread kept the keyboard and the picker's arrow keys never reached it: with 240
  transcripts across 9 pages, the list could not be paged at all.
- **Every `/import` row described the harness instead of the conversation.** The subject was taken
  from the first `user` line, but the foreign CLIs write their own text into user turns — so rows read
  `<local-command-caveat>Caveat: The messages below were gener…` (Claude, after any `/compact` or `!`
  command) or `<environment_context> <cwd>C:\Users\…` (Codex, which leads every session with one).
  Envelope turns are now skipped to find what the person actually typed. The row also dropped the turn
  count and the `from <dir>` note that repeated identically on all 240 rows — a subject you can
  recognize is the only column that helps you pick, so it gets the width.

### Added
- `AIZEN_EMBED_MODEL` accepts an absolute path to a model directory, not just a name to look up under
  `~/.aizen/models/`. For a model on a shared drive or outside both search trees.

### Changed
- **One brand, everywhere: every `NEXTGEN_*` / `NG_*` name is now `AIZEN_*`.** The rebrand had only
  reached the paths — the environment variables, the internal API and a pile of doc comments still
  said `nextgen`, so the CLI documented one name and read another. Renamed: `NEXTGEN_HOME` →
  `AIZEN_HOME` (already the preferred spelling, now the only one), `NG_PROJECT_ROOT` →
  `AIZEN_PROJECT_ROOT`, `NG_EMBED_MODEL` → `AIZEN_EMBED_MODEL`, `NG_NO_SCOPE` → `AIZEN_NO_SCOPE`,
  `NG_NO_GRAPH` → `AIZEN_NO_GRAPH`, `NG_GRAPH_EXPAND` → `AIZEN_GRAPH_EXPAND`, `NG_MCP_REGISTRY` →
  `AIZEN_MCP_REGISTRY`, and the per-role / per-model families `NG_<ROLE>_{MODEL,BASE_URL,API_KEY}`
  and `NG_MODEL_<ID>_{BASE_URL,API_KEY}` → `AIZEN_*`.

  **Breaking, deliberately with no alias.** A silent fallback is how a rebrand survives for years:
  the old name keeps working, so nothing ever moves off it. If you had one of these set, rename it —
  an unset variable falls back to the documented default rather than failing quietly.

  Also gone: the `~/.nextgen` and `./.nextgen` legacy-directory fallbacks. These were never reachable
  from a released build — the earliest tag (v0.4.0) already defaults to `.aizen` — so they were
  migration code for a directory no shipped version ever created.
- Internal rename with no user-visible effect: `nextgen_home()` → `aizen_home()`,
  `project_nextgen_dir()` → `project_aizen_dir()`, and the on-disk temp/stash prefixes used during
  atomic writes (`.ng-tmp.` / `.ng-stash.` → `.aizen-tmp.` / `.aizen-stash.`). These temp files exist
  only between a write and its rename, so no cleanup step needs the old spelling.

## [0.5.0] — 2026-07-28

Memory stops being a write-only pile: facts are placed on a tier/anchor axis, recall itself into the
turn, fade instead of accumulating, and every destructive step has a reverse gear. Also the first
in-place upgrade path — `aizen update` / `/update` — so a release no longer means re-running the
installer.

Cut from `feature/memory-tiers`. Contains everything in 0.4.9 (which soaked on its own branch and was
never merged to `main`).

### Added
- **`aizen update` and `/update` — see every published version and install the one you pick.** One
  flag-free command: the picker lists the releases with the running build marked `● installed now`,
  and newer / older / pre-release spelled out on each row, so upgrading and rolling back a bad
  release are the same gesture. The new binary lands on disk immediately while **the session you ran
  it from keeps working on the old one** — the running executable is renamed aside and the download
  takes its place, so nothing is swapped out from under a live process; the new version takes effect
  in the next terminal. Startup does a silent check (cached 24h, `AIZEN_NO_UPDATE_CHECK=1` or
  `update_check: false` to switch off) and mentions a newer version in one dim line.
- **`aizen memory health`** — per-week table over `stats.jsonl` (store growth, saturation, how much
  injected memory actually got used, contradictions found) plus a verdict, and an explicit refusal to
  conclude anything from under four weeks of data.
- **`aizen memory doctor`** — the tier/anchor axis fails quietly by construction: an unanchored place
  fact, an anchor whose directory is gone, or a `supersededBy` naming a purged id all look healthy in
  `memory list` and just subtract from recall. `doctor` names them.
- **`aizen memory reconcile`** (dry-run by default) and **`memory list --superseded`** — the
  graveyard is now browsable, with what replaced each row and the revive command spelled out.
- **`aizen memory revive <id>`** — clears both halves of a supersession (the retired side's pointer
  *and* any live fact's forward `supersedes:` claim), since either one alone keeps the fact hidden.

### Changed
- **`/timemachine` is one command instead of four.** Typing it opens the checkpoint list — `▸` marks
  where you are, each row carries the id, how long ago, the label, and whether picking it rewinds
  `code + chat` or `code only` — and picking a row puts you back in that code and that conversation
  in one gesture. The `pick`/`restore`/`menu` arguments and the Files / Task / Both sub-menu are
  gone: what a checkpoint restores is a property of the checkpoint, not a question to answer twice.
  Still reversible (the pre-restore tree is auto-snapshotted and the live chat is saved to its own
  session file first), Esc still leaves without touching anything, and `/timeline` · `/tm` remain as
  aliases. `aizen time …` on the CLI is unchanged.
- **Memory places facts on a tier/anchor axis** instead of one hashed scope slug: `tier` says what a
  fact is *about* (the person, this machine, a directory tree) and `anchor` says where a place fact
  *applies* (a normalized absolute path, matched segment-safe, nearest ancestor wins). The read
  predicate reads the same axis, so "true here" is decided one way in one place, and an unresolvable
  orphan fails closed rather than leaking into every directory.
- **Relevant facts arrive in the turn without the model calling a tool for them.** Labelled by axis
  (about you / this machine / here) behind a relevance gate, budget-packed, and skipped entirely when
  the selection hasn't changed — a second copy of a fact already in the transcript is just a rival
  claim with nothing marking which is newer. Being *shown* a fact earns nothing; only reporting it as
  used does.
- **Reinforcement saturates instead of growing without bound.** Strength is keyed to confirmations on
  a capped ladder, the idle clock reads *last used* rather than any bookkeeping rewrite, and a fact
  the user typed by hand keeps a floor under its salience instead of being permanently halved for
  never having been searched for.
- **Faded facts are set aside, not deleted** — and the first sweep on any store only *previews* what
  it would move. An upgrade that quietly starts relocating someone's facts on the strength of a
  brand-new formula isn't something they can consent to afterwards.
- **Per-partition memory caps** are bucketed by tier/anchor. Every fact written on the new axis
  carries no scope slug, so a scope-keyed bucket had put the whole store in one pool — one chatty
  project evicting every other project's facts is exactly what per-zone caps exist to prevent.

### Fixed
- **Four hard-delete windows on user data closed.** Persona self-memory pruning, `memory review
  --clear`, and skill writes all moved to archive-or-atomic-write; a crash mid-write could previously
  truncate a skill to zero bytes on any `skill_load`.
- **Retiring a fact no longer drops frontmatter keys this build doesn't model** — supersession
  rebuilt a fixed record shape, so anything unrecognized was silently lost. It edits the field map
  now, like updates do.
- **Restoring an archived fact keeps its id**, and errors asking for `--as <new-id>` on collision
  instead of silently landing as `<id>-2`: a fact that answers searches while being invisible to
  every pointer aimed at it.
- Contradictions phrased in different words are caught by a batched pass off the hot path (at most
  one model call, at most 12 pairs) with asymmetric rails: nothing destructive below a confidence
  floor, and a fact confirmed twice by the user goes to review at *any* confidence rather than being
  overruled automatically. Same-chain ties break on recency, and the survivor re-anchors to the
  common ancestor so resolving *content* never quietly narrows *where* a fact applies.

## [0.4.9] — 2026-07-26

Version-only release cut from `release/v0.4.9`: the 0.4.8 tag had already been published from the
pre-identity-work tree, so the work listed under 0.4.8 below actually shipped as the 0.4.9 binary. It
soaked on its own branch and reaches `main` as part of 0.5.0.

## [0.4.8] — 2026-07-26

### Fixed
- **A project's memory, skills and index no longer fork in two depending on whether `git` was on
  PATH** — the zone key was hashed from the git remote URL when git could be found and from the raw
  path when it couldn't, so the same checkout answered to two different zones from one launch to the
  next and half the user's memory went missing without a word. The key is now the normalized
  canonical project path only; the remote URL is informational. `aizen zone migrate` shows what a
  legacy zone holds (dry-run by default) and merges it on `--apply`, including saved conversations,
  which are keyed by provenance inside each file and so were invisible to a per-directory sweep.
- **A missing `git` no longer blocks editing** — `git` not being on PATH was treated as a hard
  checkpoint failure, which refused every edit rather than degrading. It is now benign: checkpoints
  switch off with one warning and work continues. `git` is resolved once through a central resolver
  (`AIZEN_GIT`, then PATH, then the usual install locations) so a GUI-installed git is found even
  when the shell can't see it.
- **`/resume` no longer grafts one project's context onto another** — sessions live in one flat pool,
  so restoring offered whichever conversation was written last, from any project, and replayed its
  stale system lane into the current one. Session files now record their origin (project key/root/
  slug, model, created/updated); the startup hint and bare `/resume` prefer this project's newest
  conversation and label a cross-project offer with `from <dir>`; restoring rebuilds both prompt
  lanes for the current project. `/handoff` rotates to a fresh file instead of overwriting the
  conversation it summarized.
- **The `/sessions` picker says what each row is** — turn count, age, origin project and a `● current`
  marker, newest first, with a confirmation before overwriting another conversation's file and
  `(unreadable)` for a corrupt one instead of a plausible-looking empty row.
- **A failing autosave is no longer silent** — it warns once per failure streak and reports recovery,
  so a conversation that is not reaching disk says so.

### Added
- `aizen where` and `/where` — print the project root, zone slug, resolved `git`, and the paths
  backing memory, skills, the codebase index and sessions, so which zone is in effect is checkable
  rather than inferred.

### Changed
- **Telegram replies are now native, compact HTML instead of raw Markdown** — headings, emphasis,
  inline/fenced code, safe links, lists and quotes use Telegram's supported formatting; Markdown
  tables become narrow stacked records rather than raw pipes or terminal boxes. Rendering is parsed
  with the existing pure-Rust Markdown engine, escapes raw HTML/unsafe links, chunks only at balanced
  block boundaries, and retries once as equivalent plain text if Telegram rejects rich markup.
- **Telegram working status no longer clutters the chat** — a single `✦ Đang xử lý…` message is
  deleted after the final reply is delivered (or edited to a concise success/error state when delete
  or delivery fails). Hostbot turns also receive a mobile-only response contract: lead with the result,
  keep paragraphs/headings short, prefer bullets, and avoid wide diagrams or decorative repetition.
  Discord remains plain text in this scope.

## [0.4.4] — 2026-07-22

### Changed
- **Transcript visuals redesigned around structured events** — tool calls, the plan checklist, edit
  diffs and the verify line are no longer pre-styled strings blindly emitted; they flow through the
  UI as typed events so the retained backend can lay them out by width and update lines/panels in
  place. Every surface renders from the same layout code, so classic/plain/one-shot read identically
  (degrading only where an append-only surface can't update in place).
  - **Tool-call line** opens with `⚙ <tool_name>   <target>` — the raw tool name in moonlight, its
    target dim silver — and drops the result to an indented `└ <digest> · <time>` line **beneath**
    it: the digest tinted by outcome (dim while running, green on ok, salmon on error) and carrying
    the wall-clock run time (`· 940ms` under a second, `· 1.2s` above). Replaces the old
    `◆ <verb> (tool)` footnote shape; the earlier right-aligned-digest variant is gone (the digest
    now always sits below the call, so a long result never collides with the target).
  - **Plan panel** (`todo_write`) is a boxed checklist — header `☑ done/total · plan`, then ✓/▸/○
    rows — that **updates in place** under retained instead of re-printing a fresh `todos:` block on
    every call. Classic re-prints the box; an emptied list removes the panel.
  - **Edit diffs** render inside a rounded `diff · <path>  +A −D` box (added = green `+`, removed =
    salmon `−`) instead of loose indented lines.
  - **Verify gate** success is a green `✓ <cmd> — <detail>` line.
  - **Footer** — working state shows a live pill `✶ working · Ns · Esc to stop`; the HUD row carries
    `model · ~<used>/<max> tok · <n> turns · <mode>`; the empty prompt shows a right-aligned
    `↵ send · Tab complete` hint. The mode chip keeps its colour (`⚡ yolo` gold, `◆ smart` moonlight).

### Fixed
- **Retained TUI kept no colour** — every non-assistant line (the `❯` user echo, tool anchors, the
  green/salmon edit diff) was run through `strip_ansi_codes` and then repainted one flat grey, so
  you couldn't tell your own message from the model's reply or read an edit's `+`/`−` at a glance.
  The retained backend now keeps SGR colour codes (dropping only cursor moves / erases) and parses
  them into styled ratatui spans at draw time; uncoloured text is unchanged.
- **User chat lines now read as yours** — the whole `❯ …` echo takes the moonlight accent (was just
  the arrow), in both a live turn and a `/sessions` restore replay, so it stands apart from the
  model's grey reply.
- **Empty / failed API turns are surfaced loudly** — a blank turn (rate-limit swallowed into an
  empty 200, content filter, or a gateway that closed the stream early) printed a dim grey aside
  that read like idle. It's now a `⚠ empty reply:` warning naming the likely cause, in both the
  sticky and plain REPLs.
- **Messages typed while the agent works no longer get swallowed** — paste-coalescing keyed off how
  long `read_key` blocked, but a busy output stream makes a hand-typed key arrive pre-buffered, so
  the Enter meant to queue a message became a literal newline and the message sat in the draft
  forever. Detection now measures the gap between successive key *arrivals*, folding the slow
  repaint into the gap so a real keystroke is never mistaken for a paste.
- **Image-attachment chip missing in the retained TUI** — pressing Ctrl-O (clipboard screenshot) or
  dropping an image file attached it correctly (the send carried the image), but the retained input
  box never drew the pending count, so there was no visible confirmation. `draw_footer` now renders
  the `[Nimg]` chip (matching the classic renderer), reserving its width from the typing budget and
  offsetting the caret so it never overlaps the typed text.

### Changed
- **Tool-call anchor glyph** — replaced the florette `✿` with a solid diamond `◆` (moonlight
  silver): a cleaner, more basic mark. The result digest under it still carries state colour
  (green when done, salmon on error).
- **Sun logo reads as a sun at terminal resolution** — the brand's 32 thin-ray chrysanthemum sun
  is faithful in the high-res sixel image, but a low-res braille character grid (the logo on
  launch) shattered those thin lances into scattered `░▒▓` dots that looked like noise.
  `petal_mask` is now resolution-aware: sixel (≥128px) keeps the true 16+16 thin rays; the char
  grid draws a bolder 12-ray sun (thicker lances, larger solid hub).

### Removed
- **Retained idle "3D" spinning-sun animation** — dropped entirely to keep the TUI light: no
  per-frame render thread wakeups when idle, and the now-dead `tui_performance` / `idle_animation`
  config knobs are gone with it. The landing splash logo (static sun) is unchanged.

## [0.4.2] — 2026-07-21

### Fixed
- **Markdown tables no longer dump raw pipes** — the delimiter-row detector required 3+ dashes per
  cell, so the short separators models routinely emit (`|--|`, `|-|`) failed table detection and the
  raw `| … |` rows printed verbatim. Now accepts any GFM-valid delimiter (≥1 hyphen, optional `:`).
- **Code / `diagram` fences render as a true rectangle** — the top and bottom borders were capped
  near 56 columns while body content ran to the full terminal width, so wide content sprawled past a
  narrow frame. All three render paths (streaming, retained, unterminated-fence fallback) now share
  one capped width for the top border, every body row (padded and closed with a right rule), and the
  bottom border, measured by display width rather than byte length.

### Changed
- **Tool-call anchor glyph** — replaced `⏺` (force-rendered as a blue emoji disc by most terminals,
  ignoring the theme color) with the monochrome florette `✿`, which takes the moonlight-silver accent
  and echoes the Aizen chrysanthemum-sun mark.

### Added
- **Retained TUI foundation + runtime safety** — alternate-screen single-owner renderer, structured
  transcript streaming, full-message Markdown cache, retained overlays/panel status, local frame
  metrics, and Mermaid-to-Unicode fallback; classic/plain remain rollback paths.
- **Static/dynamic prompt lanes + crash recovery** — stable project/environment prefix stays cacheable;
  volatile identity/memory refreshes only at fresh user-turn boundaries. Owner-only recovery leases
  restore safe history and prefill interrupted drafts without auto-submitting or replaying tools.
- **MCP lifecycle hardening** — poisoned transport reconnect, one read-only retry only, destructive
  no-replay after ambiguous failure, manager/connection generations, per-turn schema pinning, and
  deferred `tools/list_changed` refresh at the next user turn. MCP trust writes are atomic owner-only.
- **Browser profiles and routing** (`--features browser`) — versioned `~/.aizen/browser.json`, named CDP
  profiles, host routes, environment-reference-only auth attached to HTTP discovery AND the WebSocket
  upgrade, per-conversation session isolation (LRU-capped, released on `/new`/route removal), ref
  invalidation, sanitized `/browser` status, read-only snapshot retry, and no replay of
  navigate/click/type/eval.
- **Terminal-native reply visuals** — configurable `auto|always|off` final-answer contract, responsive
  Markdown tables (boxed wide / stacked narrow), and topology-safe `diagram`/`ascii`/`flow` fences.
  One-shot chat and workflow synthesis share the renderer; Telegram/Discord receive plain stacked
  tables with newline-aware UTF-16 chunking.

## [0.4.1] — 2026-07-20

### Added
- **Harness persistence P0** — incomplete session todos now auto-poke the top-level loop before an
  early text-only exit; confidence spikes at `Done` trigger one evidence re-check; quantifiable
  optimization goals are reframed into metric → baseline → iterate loops. The deterministic loop
  eval suite covers todo-poke, confidence-gate, and hill-climb behavior.
- **Symbolic edit tools** — `symbol_replace` / `symbol_insert` rewrite or insert relative to a
  named symbol via the language-server outline range (Serena-style; no `old_string` thrash).
- **`/workflows` multi-agent status** — process-global live registry of `task` / `workflow` /
  workflow children (phase, elapsed, detail) + sub-agent slot gate (`active/cap`). Open mid-turn
  to watch fan-outs; aliases `/wf`, `/workflow`. Empty state explains how to launch multi-agent work.
- **`StopReason::Cancelled`** — cooperative cancel mid-loop (Esc); nested `task` / workflow children
  stop at the next boundary instead of running to max_iters.

### Changed
- **LSP default ON (lazy)** — manager arms at session start; language servers still spawn only on
  first symbol query. `/lsp off` reclaims RAM; tools reappear after `/lsp on`.
- System prompt prefers outline/definition + symbolic edit over dumping whole files / grep for
  semantic code questions.
- **Multi-agent hardening** — CLI `aizen workflow` shares singular-writer + `SubagentSlot` +
  orchestration Track with the tool path; workflow children use task-like budgets (15/30 steps);
  synthesis truncates child summaries; sub-agents get LSP nav + coder symbolic edit; ultimate
  prompt prefers real `workflow` fan-out/verify.
- **Sticky `/sessions` repair** — the conversation picker now suspends the sticky footer and parks
  the background keyboard reader before dialoguer owns the terminal. Restore/save/delete and
  confirmation no longer corrupt the footer or merge menu lines.

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
  profile + dialectic, and an opt-in fuzzy/dense tier (`AIZEN_MEM_FUZZY` / `AIZEN_MEM_DENSE`).
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
