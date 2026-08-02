# Security Policy

## Reporting a vulnerability

**Please do not report security vulnerabilities through public GitHub issues.**

Aizen runs shell commands, edits files, makes network requests, and handles provider API keys, so
security reports are taken seriously.

Instead, report privately through one of:

- GitHub's [private vulnerability reporting](https://github.com/aizen-stack/aizen/security/advisories/new)
  (Security tab → Report a vulnerability), or
- a direct private message to the maintainer.

Please include:

- A description of the issue and its impact.
- Steps to reproduce (a minimal proof-of-concept if possible).
- The version / commit you tested against.

## What to expect

- An acknowledgement of your report as soon as the maintainer sees it.
- An honest assessment of severity and a fix timeline.
- Credit in the release notes when the fix ships, if you'd like it (or anonymity if you prefer).

## Scope

Aizen's threat model already documents several deliberate safety floors (a destructive-command
blocklist, an SSRF guard on the web tools, owner-only secret files, tool-output-as-data). Reports
that strengthen or bypass these are especially welcome. See the **Safety model** section of the
[README](README.md) for the current guarantees.

Please give the maintainer a reasonable chance to ship a fix before public disclosure.
