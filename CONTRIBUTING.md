# Contributing to Aizen

Thanks for wanting to help. Aizen is **open source under the
[Apache License 2.0](LICENSE)** — you can use, modify, and redistribute it freely, including
commercially, and contributions are welcome under the terms below.

## Before you start

- **Search existing [issues](https://github.com/aizen-stack/aizen/issues)** before opening a new one —
  it may already be reported or in progress.
- For anything bigger than a small fix, **open an issue first** and describe what you want to do.
  A quick "here's the plan" saves everyone a rejected PR.
- Small, focused PRs get reviewed faster than large sweeping ones. One logical change per PR.

## Licensing of contributions (no CLA)

There is **no CLA to sign**. Aizen relies on the inbound=outbound rule in
**[Section 5 of the Apache License 2.0](LICENSE)**:

> Unless You explicitly state otherwise, any Contribution intentionally submitted for inclusion in
> the Work by You to the Licensor shall be under the terms and conditions of this License, without
> any additional terms or conditions.

In plain terms: **you keep the copyright to your contribution**, and by opening a PR you license it
to the project under Apache-2.0 — the same license everyone else receives. That includes the patent
grant in §3.

We ask you to certify authorship with the
[Developer Certificate of Origin](https://developercertificate.org/) by signing off each commit:

```bash
git commit -s -m "fix scrollbar drift"
```

That appends a `Signed-off-by:` line. It is a statement that you wrote the patch, or otherwise have
the right to submit it under Apache-2.0.

**Do not submit code you don't have the right to relicense** — no copy-paste from GPL/AGPL projects,
no code owned by an employer without their permission, and no LLM output you haven't reviewed and
can stand behind.

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
7. Push and open a PR against `aizen-stack/aizen:main`. Fill in the PR template.

## What gets merged

- ✅ Bug fixes with a regression test.
- ✅ Focused features that were discussed in an issue first.
- ✅ Docs, comments, and test-coverage improvements.
- ❌ Changes that add a C/native dependency or break the single-static-binary posture.
- ❌ Large unsolicited rewrites or style-only churn across unrelated files.
- ❌ Code you don't have the right to license under Apache-2.0, or commits without a DCO sign-off.

## Reporting security issues

**Do not open a public issue for a security vulnerability.** See the security policy in the
repository, or contact the maintainer privately. Give us a chance to fix it before disclosure.

## Code of conduct

By participating you agree to the [Code of Conduct](CODE_OF_CONDUCT.md). Be decent to each other.
