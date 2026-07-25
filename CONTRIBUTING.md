# Contributing to Aizen

Thanks for wanting to help. Aizen is **source-available under the
[PolyForm Noncommercial License 1.0.0](LICENSE)** — you can read, run, and modify it for any
noncommercial purpose, and contributions are welcome under the terms below.

## Before you start

- **Search existing [issues](https://github.com/dawnofcd/aizen/issues)** before opening a new one —
  it may already be reported or in progress.
- For anything bigger than a small fix, **open an issue first** and describe what you want to do.
  A quick "here's the plan" saves everyone a rejected PR.
- Small, focused PRs get reviewed faster than large sweeping ones. One logical change per PR.

## The CLA (required, one time)

Because Aizen is dual-licensed — noncommercial to the public, with a separate commercial license
available from the maintainer — every contributor must agree to the **[Contributor License
Agreement](CLA.md)** once, before their first PR can be merged.

You don't sign anything by hand. The first time you open a PR, a bot comments with a link; you reply
with the exact sentence it asks for, and your agreement is recorded. After that, all future PRs are
covered automatically.

In short, the CLA says: **you keep the copyright to your contribution, but you grant the maintainer a
broad license to use it — including in the commercial edition.** Read [CLA.md](CLA.md) for the full
text.

## Development setup

Aizen is a **pure-Rust single static binary** — no C toolchain, no external runtime deps.

```bash
# build
cargo build --release --bin aizen

# run the full test suite (should be green before you push)
cargo test --bin aizen

# the semantic-retrieval tier is behind a feature flag; build it if you touch that code
cargo build --release --features dense --bin aizen
```

Requirements:
- A recent stable Rust toolchain (`rustup update stable`).
- That's it. If a change would pull in a C dependency, it almost certainly won't be accepted —
  keeping the binary self-contained is a hard project constraint.

## Making a change

1. **Fork** the repo and create a branch off `main` (`git checkout -b my-fix`).
2. Make your change. Match the surrounding code — naming, comment density, error handling.
3. **Add or update tests.** New behavior needs a test; a bug fix needs a test that would have caught it.
4. Run `cargo test --bin aizen` and make sure it's **green**.
5. Run `cargo fmt` and `cargo clippy` and clear anything you introduced.
6. Commit with a clear message (imperative mood: "fix scrollbar drift", not "fixed stuff").
7. Push and open a PR against `dawnofcd/aizen:main`. Fill in the PR template.

## What gets merged

- ✅ Bug fixes with a regression test.
- ✅ Focused features that were discussed in an issue first.
- ✅ Docs, comments, and test-coverage improvements.
- ❌ Changes that add a C/native dependency or break the single-static-binary posture.
- ❌ Large unsolicited rewrites or style-only churn across unrelated files.
- ❌ Anything without the CLA agreed.

## Reporting security issues

**Do not open a public issue for a security vulnerability.** See the security policy in the
repository, or contact the maintainer privately. Give us a chance to fix it before disclosure.

## Code of conduct

By participating you agree to the [Code of Conduct](CODE_OF_CONDUCT.md). Be decent to each other.
