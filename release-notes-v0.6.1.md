## Aizen v0.6.1 — the quiet wrong answers

A maintenance release about parts of the agent that were failing without saying so: a single streamed frame could take a tool call down with it, a delegated workflow resolved paths against the wrong directory, and the mouse wheel scrolled the input box instead of the transcript. It also folds the tool surface down, which means fewer schemas on every request.

### Fixed

- **A duplicate key in one streamed frame no longer drops the tool call riding with it.** Gateways that mirror the reasoning channel into both `reasoning_content` and `reasoning` produced a serde `duplicate field` error, and the rejected frame took whatever `content` or `tool_calls` arrived in the same delta with it. The two spellings are separate fields now, and a frame that still fails a strict parse is retried leniently before being given up on.
- **Unparseable-frame warnings no longer bury the session.** A mis-modelled delta shape breaks *every* frame, so the old warning printed one line per streamed token over the live UI. Those frames are keepalive noise and are dropped silently; `AIZEN_DEBUG_STREAM=1` shows them, capped at 3 per response.
- **The mouse wheel keeps scrolling the transcript for the whole turn.** A tool that spawns a child process can reset the console input mode on Windows, dropping mouse capture mid-turn — from there the terminal leaks wheel ticks through as arrow keys, so scrolling walked input history. Capture is now pinned across the entire working window instead of only being restored at the end of the turn.
- **A delegated workflow resolves paths against its own lane root**, not the process working directory, so concurrent lanes can no longer read, edit, or checkpoint the wrong project.
- **Workflow synthesis cannot park a fan-out forever.** The final synthesis call now runs under the same deadline as every other background call.
- **Eagerly-started tool calls get the same argument repair as the normal path**, so a call no longer fails with a missing-argument error purely because it started early.

### Changed

- **`multi_edit` is gone; `file_edit` does both.** Pass `edits[]` instead of `old_string`/`new_string` for an ordered list of edits applied to one file in a single atomic write.
- **Four checkpoint tools collapse into two.** `checkpoint` takes `action=save|rewind|restore` (approval-gated); read-only `checkpoint_view` takes `diff|list` and stays out of the approval path. `aizen time …` is unchanged.
- **Sub-agent prompts carry role-scoped tool guidance** instead of the full catalog, and workflow children inherit the parent's context window.

### Compatibility

- `multi_edit`, `checkpoint_rewind`, `checkpoint_list`, `checkpoint_restore`, and `checkpoint_diff` are no longer registered as tool names. Nothing in a saved session or config refers to them; only a hand-written script calling those names directly would need updating.
- Human-facing commands (`aizen time save|list|restore|undo|redo`, `/diff`) are unchanged.

See `CHANGELOG.md` and `docs/REFERENCE.md` for the full detail.
