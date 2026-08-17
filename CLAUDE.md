# CLAUDE.md — project facts for coding agents

Read this before touching anything. It records the decisions an agent is most likely to get wrong
because older files, older releases, and stale memory say otherwise.

## What this project is

**Aizen** — a terminal-native agentic coding CLI. Pure Rust, shipped as **one static binary**, points
at any OpenAI-compatible `/chat/completions` endpoint. The command is `aizen`.

Hard constraints (a change that breaks one of these will be rejected):

- **Single static binary.** No C/native dependency, no external runtime. TLS is rustls-only — never
  reintroduce OpenSSL or anything needing a native toolchain.
- **Pure Rust.** If a crate pulls a C build, it doesn't go in.
- Startup time and binary size are features. Measured 2026-08-02: **10.8 ms** startup, **34.1 MB**
  binary. This is the project's main advantage over Claude Code (needs Node) and Hermes Agent
  (needs Python + ~2.4 GB) — do not regress it casually.

## License — Apache-2.0 (changed 2026-08-03)

**The project is Apache-2.0. It is NOT PolyForm Noncommercial anymore.** It is open source, and
commercial use is allowed.

- `LICENSE` is the verbatim Apache License 2.0 — the TERMS AND CONDITIONS section is a byte-exact
  copy of <https://www.apache.org/licenses/LICENSE-2.0.txt>. **Never reword, reformat, or "tidy" it**;
  GitHub's license detection and every corporate legal review depend on it matching exactly. Only the
  APPENDIX carries the filled-in `Copyright 2026 Aizen authors`.
- `Cargo.toml` uses `license = "Apache-2.0"` (the SPDX id), **not** `license-file`. Using
  `license-file` makes crates.io and GitHub report an unrecognized license.
- `NOTICE` must ship with any redistribution (Apache §4d). Keep it in sync when attribution changes.
- **There IS a CLA, as of 2026-08-17.** `CLA.md` (v2.0) is enforced by
  `.github/workflows/cla.yml`; contributors agree once by commenting the sign sentence on their PR.
  **`.github/workflows/dco.yml` was deleted and the DCO sign-off is no longer asked for** — don't tell
  contributors to `git commit -s`, and don't restore that workflow.
  - History, so nobody "fixes" this back and forth: a CLA v1.0 existed under PolyForm, was deleted at
    the Apache relicense (`586b7e9`) in favour of Apache-2.0 §5 + DCO, and was reinstated as v2.0 by
    maintainer decision. v2.0 differs from v1.0 in naming Apache-2.0 as the public license.
  - The point of §3 is the **commercial relicensing grant** — it is what keeps a paid edition
    possible. It is a broader ask than Apache-2.0 §5, so user-facing text must say so plainly rather
    than describing the CLA as a formality.
- Trademark: Apache §6 grants no rights to the "Aizen" name or logo. Forks may use the code, not the
  brand.
- Releases **up to and including v0.5.5** went out under PolyForm Noncommercial. That is history and
  cannot be retracted; everything from the relicensing commit onward is Apache-2.0. If you find a
  file still saying PolyForm, it is a leftover — fix it.

## Git remotes — two of them, don't mix them up

```
origin  → https://github.com/dawnofcd/Aizen_agent.git   (PRIVATE — full source, day-to-day work)
public  → https://github.com/dawnofcd/aizen.git         (PUBLIC — redirects to aizen-stack/aizen)
```

- The **canonical public repo is `aizen-stack/aizen`** (an org). `dawnofcd/aizen` still resolves via
  GitHub's redirect, but **write `aizen-stack/aizen` in all user-facing URLs, install scripts, and
  code** so nothing depends on a redirect.
- `dawnofcd/Aizen_agent` is private, so an anonymous fetch of it returns 404. That is expected — it
  is not a broken URL. **Never put it in user-facing docs**; the README used to tell people to
  `cargo install --git .../Aizen_agent`, which 404'd for everyone.
- Release binaries are published to `aizen-stack/aizen`. `src/features/update.rs` has
  `DEFAULT_REPO = "aizen-stack/aizen"` and `aizen update` reads releases from there — keep it aligned
  with `install.ps1` (`$Repo`) and `install.sh` (`repo=`).
- Never push to `main` on either remote without being asked. Branch, then push with `-u`.

## Layout worth knowing

| Path | What |
|---|---|
| `README.md` | the short landing page — install, why, what it does. **Keep it under ~110 lines**; details go to `docs/REFERENCE.md`, not here |
| `docs/REFERENCE.md` | the full manual: REPL surface, every command, self-hosting, MCP, browser, safety model |
| `src/features/update.rs` | self-update; `DEFAULT_REPO` lives here |
| `src/sandbox/` | OS sandbox under approval/cmd_guard: policy · runner · audit · per-platform backends (Landlock/seccomp · Job Object · Seatbelt). Every model/repo-influenced spawn goes through `runner::prepare_*` — never add a bare `Command::new` for those. `docs/SANDBOX.md` is the contract; keep its capability matrix honest |
| `install.ps1` / `install.sh` | one-line installers; repo slug is hardcoded in both |
| `CLA.md` + `.github/workflows/cla.yml` | the CLA and its bot (signatures land in the `cla-signatures` branch); replaced the DCO check |
| `dist/` | assets for the public download channel |
| `docs/` | design + audit notes |
| `bench-fixtures/` | fixtures for `aizen bench` |

Links inside `docs/REFERENCE.md` that point at repo-root paths need a `../` prefix — it lives one
level down.

## Build / verify

```bash
cargo check                                   # fast feedback
cargo build --release --bin aizen             # the shipped artifact
cargo test --bin aizen                        # must be green before pushing
cargo build --release --features dense --bin aizen   # semantic-retrieval tier (feature-gated)
cargo fmt && cargo clippy
```

Note: `cargo test` on Windows can be slow; the 120 s shell cap may kill it. Run long builds through a
background process, not a foreground shell call.

## Known distribution gaps (as of 2026-08-03)

Real numbers, not guesses — from the GitHub API:

- `aizen-stack/aizen`: 25 stars, 4 forks, **0 watchers**, Discussions **off**.
- v0.5.5 downloads: Windows 3, Linux 0, macOS 0.
- Windows `.exe` is **unsigned** → SmartScreen warns. macOS is **not notarized**.
- Not published to winget / scoop / Homebrew / crates.io / AUR. Apache-2.0 now unblocks the OSI-only
  ones (crates.io, AUR, Homebrew).
- The landing page (`aizen-stack.vercel.app`) advertised v0.4.5 while v0.5.5 was current — it is a
  separate deploy and drifts; check it when releasing.

## Working style the maintainer expects

- Vietnamese for conversation; English for code, comments, and docs.
- Evidence over assumption: measure, cite the tool output, and say plainly when something is
  unverified. Don't claim a command passed unless you ran it.
- Say what is weak about this project honestly — no sales pitch about our own tool.
- Business/licensing decisions are the maintainer's, not the agent's. Ask before changing one.
