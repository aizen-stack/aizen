# cla-signatures

Storage branch for the CLA bot. **Do not delete, do not protect, do not merge into `main`.**

`.github/workflows/cla.yml` runs [`contributor-assistant/github-action`][action], which records
every contributor's CLA agreement in `cla/signatures.json` **on this branch**. The action writes
that file with the built-in `GITHUB_TOKEN` — it does *not* create the branch itself. If this branch
is missing, `repos.createOrUpdateFileContents` fails with `Branch cla-signatures not found`, and the
`cla` check then fails on **every** pull request no matter who signs, because the error path ends in
`throw new Error("Committers of pull request N have to sign the CLA")`.

That is exactly what happened between the CLA landing (`a4fe1f6`) and this branch being created:
the check was red on every run, including for contributors who had posted the sign sentence.

Nothing here is source code, and this branch shares no history with `main`.

[action]: https://github.com/contributor-assistant/github-action
