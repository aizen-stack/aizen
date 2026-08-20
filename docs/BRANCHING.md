# Branching and release process

The rules a change follows from a working tree to a published binary. They are enforced by GitHub
branch protection and CI, not by memory — if something here is only a convention, it says so.

## The two remotes (read this first)

| Remote | URL | Role |
|---|---|---|
| `public` | `github.com/aizen-stack/aizen` | **the live line.** `main`, `dev`, every PR, every release |
| `origin` | `github.com/dawnofcd/Aizen_agent` (private) | historical private mirror — **divergent history** |

`origin/main` and `public/main` do not share an ancestor any more. Never merge one into the other:
a cross-merge would replay years of unrelated history into the live line. All work described below
happens on `public`.

## Branches

| Branch | Cut from | Merges into | Protected |
|---|---|---|---|
| `main` | — | — | yes — release line, always tagged and shippable |
| `dev` | `main` | `main` (via a release branch) | yes — integration line, always green |
| `feat/*`, `fix/*`, `chore/*`, `docs/*`, `refactor/*` | `dev` | `dev` | no |
| `release/vX.Y.Z` | `dev` | `main` | no |
| `hotfix/vX.Y.Z` | `main` | `main` **and** `dev` | no |

`main` only ever moves through a release or a hotfix PR. Day-to-day work never targets it.

## Everyday flow

```bash
git fetch public
git switch -c fix/short-description public/dev     # always cut from dev, never from a stale local
# … work, commit …
cargo fmt && cargo clippy && cargo test --bin aizen   # all three green BEFORE you push
git push -u public fix/short-description
gh pr create --base dev --fill                     # base is dev — a PR into main will be rejected
```

A PR merges when: CI is green on every OS in the matrix, the CLA check passes, and the code owner
has approved. Squash-merge is the default so `dev` reads as one commit per change; a merge commit is
fine for a branch whose individual commits are worth keeping.

## Release

```bash
git switch -c release/v0.7.0 public/dev
# bump the version in Cargo.toml, update the changelog, no other change
git push -u public release/v0.7.0
gh pr create --base main --title "release: 0.7.0 — <the headline>"
# merge once CI is green, then tag the MERGE COMMIT on main:
git fetch public && git tag v0.7.0 public/main && git push public v0.7.0
git switch dev && git merge --ff-only public/main && git push public dev   # dev absorbs the bump
```

Tag from `main` after the merge, never from the release branch. `release.yml` fires on `v*` and
**only builds** — it does not run the test suite, so the tag must sit on a commit `ci.yml` already
proved green. A tag placed outside `main` has never been tested by CI (this happened for 0.6.5).

After publishing, check that the landing page and the install scripts point at the new version —
they are a separate deploy and drift on their own.

## Hotfix

A production break that cannot wait for the next release:

```bash
git switch -c hotfix/v0.7.1 public/main
# the smallest possible fix + its regression test
gh pr create --base main
```

After it merges and is tagged, open a second PR merging `main` back into `dev`, or the fix is lost
at the next release.

## What protection enforces

On both `main` and `dev`:

- pull request required — no direct pushes, including for the maintainer
- CI (`build + test` on ubuntu, macos and windows) must pass
- CLA check must pass
- code-owner approval required (see [CODEOWNERS](../.github/CODEOWNERS))
- branch must be up to date with its base before merging
- force-push and deletion blocked
- conversations must be resolved before merge

The `cla-signatures` branch stores the signature ledger and **must stay unprotected** — the CLA bot
writes to it directly and cannot open a PR against a protected branch.

## Local gates (not enforceable by GitHub)

CI runs the same commands, but finding it locally is minutes instead of a red PR:

```bash
cargo fmt
cargo clippy
cargo test --bin aizen        # slow on Windows — run it in the background, not a 120 s shell call
cargo build --release --bin aizen
```

The hard constraints a reviewer will reject a PR over are in [CLAUDE.md](../CLAUDE.md): pure Rust,
one static binary, no C/native dependency, rustls only, and no casual regression of startup time or
binary size.
