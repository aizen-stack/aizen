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

## Licensing of contributions (CLA required)

Aizen asks every contributor to agree to the **[Contributor License Agreement](CLA.md)** once,
before their first pull request can merge. This replaced the older DCO sign-off — you no longer need
`git commit -s`.

You agree by commenting one sentence on your PR. A bot posts the instructions automatically:

> I have read the CLA Document and I hereby sign the CLA

Signing covers all your future PRs, so you are only asked once.

**Read [§3](CLA.md#3-copyright-license-you-grant) before you agree — it is the part that matters.**
The short version, stated plainly rather than buried:

- **You keep the copyright** to your contribution. Nothing is assigned away.
- The public project stays under **[Apache-2.0](LICENSE)**, and your contribution reaches everyone
  else under that license.
- **But** you also grant the maintainer the right to license your contribution under *other* terms,
  **including proprietary commercial ones**. Concretely: a paid commercial edition of Aizen may
  include your code without a further fee or negotiation with you.

That last point is a broader grant than Apache-2.0 alone would give, and it is the whole reason a CLA
exists here instead of the inbound=outbound rule in Apache-2.0 §5. If you are not comfortable with
it, please don't sign — and please say so on the PR. We would rather hear the objection than lose the
contribution silently.

**Do not submit code you don't have the right to relicense** — no copy-paste from GPL/AGPL projects,
no code owned by an employer without their permission, and no LLM output you haven't reviewed and
can stand behind. §5 of the CLA is you representing exactly this.

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

1. **Fork** the repo and create a branch off `dev`, the integration branch (`git switch -c my-fix public/dev`). `main` is the release line and only accepts release/hotfix PRs — see [docs/BRANCHING.md](docs/BRANCHING.md).
2. Make your change. Match the surrounding code — naming, comment density, error handling.
3. **Add or update tests.** New behavior needs a test; a bug fix needs a test that would have caught it.
4. Run `cargo test --bin aizen` and make sure it's **green**.
5. Run `cargo fmt` and `cargo clippy` and clear anything you introduced.
6. Commit with a clear message (imperative mood: "fix scrollbar drift", not "fixed stuff").
7. Push and open a PR against `aizen-stack/aizen:dev`. Fill in the PR template.

## What gets merged

- ✅ Bug fixes with a regression test.
- ✅ Focused features that were discussed in an issue first.
- ✅ Docs, comments, and test-coverage improvements.
- ❌ Changes that add a C/native dependency or break the single-static-binary posture.
- ❌ Large unsolicited rewrites or style-only churn across unrelated files.
- ❌ Code you don't have the right to license under Apache-2.0, or a PR whose author has not agreed
  to the [CLA](CLA.md).

## Reporting security issues

**Do not open a public issue for a security vulnerability.** See the security policy in the
repository, or contact the maintainer privately. Give us a chance to fix it before disclosure.

## Code of conduct

By participating you agree to the [Code of Conduct](CODE_OF_CONDUCT.md). Be decent to each other.
