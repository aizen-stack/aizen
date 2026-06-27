//! User-defined slash commands — the prompt-side analog of the MCP client (the tool-side
//! extensibility). A **custom command** is a markdown prompt-macro the USER fires by name; when
//! invoked it expands a template and is submitted as a normal chat turn (so the full agent loop +
//! tools + memory apply). This is distinct from a **skill** (a procedure the AGENT pulls when
//! relevant) and a **persona** (a character overlay) — see `.claude/plans/260625-hermes-feature-
//! adoption/slash-commands.md` §4.
//!
//! Files live as human-editable markdown:
//! - global:  `~/.nextgen/commands/**/*.md`
//! - project: `./.nextgen/commands/**/*.md`  (git-checked-in → team distribution for free)
//!
//! Project files override global on a name collision. A subdirectory namespaces the command:
//! `commands/git/commit.md` → `/git:commit`.
//!
//! ```text
//! ---
//! description: Review the staged diff for bugs and risky changes
//! argument-hint: [path]
//! ---
//! Review this staged diff and flag bugs, security issues, and risky changes:
//! !`git diff --cached $ARGUMENTS`
//! ```
//!
//! Body templating at fire time:
//! - `$ARGUMENTS` → the full argument string after the command name.
//! - `$1`..`$9`   → positional args (whitespace-split).
//! - `@<path>`    → inline the file's contents (cwd-confined, read-only).
//! - `` !`cmd` `` → run a **read-only** shell command and splice its output (gated by the same
//!   `cmd_guard` floor as the agent; non-read-only / blocked commands are refused, never run).

use crate::memory::frontmatter;
use anyhow::{bail, Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use std::path::{Path, PathBuf};

/// One user-defined command parsed from a markdown file.
#[derive(Debug, Clone, PartialEq)]
pub struct CustomCommand {
    /// Invocation name (subdir-namespaced, e.g. `git:commit`).
    pub name: String,
    pub description: String,
    /// Shown next to the name in the picker (e.g. `<pr-number>`). May be empty.
    pub argument_hint: String,
    /// Optional per-command model override (parsed; advisory in v1 — not yet applied to the turn).
    pub model: Option<String>,
    /// The prompt template (post-frontmatter body).
    pub body: String,
    /// `project` or `global` — for display + collision precedence.
    pub source: &'static str,
}

/// `~/.nextgen/commands/`.
fn global_dir() -> PathBuf {
    crate::config::nextgen_home().join("commands")
}
/// `<repo-root>/.nextgen/commands/` — repo-root-aware (R4), so it loads even from a subdir.
fn project_dir() -> PathBuf {
    crate::config::project_nextgen_dir().join("commands")
}

/// All custom commands, project entries overriding global ones of the same name. Sorted by name.
pub fn list() -> Vec<CustomCommand> {
    let mut by_name: std::collections::BTreeMap<String, CustomCommand> = std::collections::BTreeMap::new();
    // Global first, then project — so project insertions overwrite global on collision.
    for (dir, source) in [(global_dir(), "global"), (project_dir(), "project")] {
        for cmd in load_dir(&dir, &dir, source) {
            by_name.insert(cmd.name.clone(), cmd);
        }
    }
    by_name.into_values().collect()
}

/// Whether any custom command exists (gates the picker/help section without a dir walk per render).
#[allow(dead_code)] // parallel to skill::has_any; reserved for a picker/help badge, not wired yet
pub fn has_any() -> bool {
    !list().is_empty()
}

/// Find one command by its (namespaced) name.
pub fn find(name: &str) -> Option<CustomCommand> {
    list().into_iter().find(|c| c.name == name)
}

/// Recursively load `*.md` under `dir`, namespacing by path relative to `base`.
fn load_dir(dir: &Path, base: &Path, source: &'static str) -> Vec<CustomCommand> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(load_dir(&path, base, source));
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            if let Some(cmd) = parse_file(&path, base, source) {
                out.push(cmd);
            }
        }
    }
    out
}

/// Parse one command file. The name is its path relative to `base`, minus `.md`, with the path
/// separator rendered as `:` (`git/commit.md` → `git:commit`).
fn parse_file(path: &Path, base: &Path, source: &'static str) -> Option<CustomCommand> {
    let raw = std::fs::read_to_string(path).ok()?;
    let rel = path.strip_prefix(base).unwrap_or(path).with_extension("");
    let name = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join(":");
    if name.is_empty() {
        return None;
    }
    let fm = frontmatter::parse(&raw);
    Some(CustomCommand {
        name,
        description: fm.get("description").unwrap_or("").to_string(),
        argument_hint: fm.get("argument-hint").or_else(|| fm.get("argument_hint")).unwrap_or("").to_string(),
        model: fm.get("model").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
        body: fm.body.trim().to_string(),
        source,
    })
}

/// Expand a command's body against `args` (the raw string typed after the command name).
pub fn expand(cmd: &CustomCommand, args: &str) -> Result<String> {
    let positionals: Vec<&str> = args.split_whitespace().collect();
    let mut out = cmd.body.clone();

    // `$ARGUMENTS` (whole) and `$1..$9` (positional). Do `$ARGUMENTS` first so a literal `$1` inside
    // the args text can't be re-substituted.
    out = out.replace("$ARGUMENTS", args.trim());
    for i in 1..=9 {
        let val = positionals.get(i - 1).copied().unwrap_or("");
        out = out.replace(&format!("${i}"), val);
    }

    out = expand_refs(&out)?;
    Ok(out.trim().to_string())
}

/// Expand `@<path>` file refs + `` !`cmd` `` read-only shell refs in arbitrary text. Shared by
/// custom-command bodies AND the REPL input-box affordance (`@file` / inline `` !`cmd` `` in a typed
/// message), so both behave identically. Pure of `$ARGUMENTS` (that's command-specific).
pub fn expand_refs(text: &str) -> Result<String> {
    let s = expand_files(text)?;
    expand_shell(&s)
}

/// `@<path>` → the file's contents, fenced. Confined to the cwd subtree. Only triggers at a word
/// boundary (so `a@b.com` e-mails are left alone), and **only when the file actually exists** — a
/// `@word` that isn't a readable file (e.g. `@everyone` in prose) passes through unchanged, so the
/// affordance never corrupts a normal message.
fn expand_files(s: &str) -> Result<String> {
    static RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?:^|\s)@([^\s`]+)").unwrap());
    // Canonicalize so `confine`'s `starts_with(base)` check (which canonicalizes the target) sees a
    // matching prefix — a bare cwd may differ from its canonical form (UNC/case) and falsely escape.
    let root = std::env::current_dir().context("resolving cwd")?.canonicalize().context("canonicalizing cwd")?;
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for cap in RE.captures_iter(s) {
        let whole = cap.get(0).unwrap();
        let path = cap.get(1).unwrap().as_str();
        // Keep any leading whitespace the outer match consumed.
        let lead = &whole.as_str()[..whole.as_str().len() - path.len() - 1];
        out.push_str(&s[last..whole.start()]);
        out.push_str(lead);
        match crate::agent::builtin::confine(&root, path, true).and_then(|p| Ok(std::fs::read_to_string(p)?)) {
            Ok(content) => {
                out.push_str(&format!("\n```{path}\n{}\n```\n", content.trim_end()));
            }
            // Not a readable file → leave the literal `@path` untouched (don't corrupt prose like
            // `@everyone`; the agent still sees the mention verbatim).
            Err(_) => out.push_str(&format!("@{path}")),
        }
        last = whole.end();
    }
    out.push_str(&s[last..]);
    Ok(out)
}

/// `` !`cmd` `` → the command's stdout, fenced. ONLY read-only commands (per `cmd_guard`) run;
/// blocked or non-read-only commands are refused with the reason (never executed silently).
fn expand_shell(s: &str) -> Result<String> {
    static RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"!`([^`]+)`").unwrap());
    let root = std::env::current_dir().context("resolving cwd")?.canonicalize().context("canonicalizing cwd")?;
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for cap in RE.captures_iter(s) {
        let whole = cap.get(0).unwrap();
        let command = cap.get(1).unwrap().as_str().trim();
        out.push_str(&s[last..whole.start()]);
        match crate::agent::cmd_guard::classify(command) {
            crate::agent::cmd_guard::Verdict::Allow => {
                let result = run_capture(command, &root);
                out.push_str(&format!("\n```$ {command}\n{}\n```\n", result.trim_end()));
            }
            crate::agent::cmd_guard::Verdict::Blocked(reason) => {
                bail!("custom command: refusing to run `!{command}` — blocked by the safety floor: {reason}");
            }
            crate::agent::cmd_guard::Verdict::Ask => {
                bail!(
                    "custom command: `!`{command}`` is not auto-runnable (only read-only commands like \
                     git diff/status/log, ls, cat are). Run it yourself and `@`-attach the output, or \
                     narrow the command."
                );
            }
        }
        last = whole.end();
    }
    out.push_str(&s[last..]);
    Ok(out)
}

/// Run a read-only shell command and capture stdout (best-effort, bounded). Mirrors `ShellRun`'s
/// platform shell + lossy-decode so non-English Windows output isn't dropped.
fn run_capture(command: &str, dir: &Path) -> String {
    use std::process::{Command, Stdio};
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(format!("chcp 65001>nul & {command}"));
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    cmd.current_dir(dir).stdout(Stdio::piped()).stderr(Stdio::null());
    match cmd.output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(e) => format!("[command failed: {e}]"),
    }
}

/// Human-readable list for `/commands` and `/help`. `None` when none are defined.
pub fn summary() -> Option<String> {
    let cmds = list();
    if cmds.is_empty() {
        return None;
    }
    let mut s = String::from("Your custom commands (drop a markdown file in ~/.aizen/commands/ or ./.aizen/commands/):\n");
    for c in &cmds {
        let hint = if c.argument_hint.is_empty() { String::new() } else { format!(" {}", c.argument_hint) };
        let desc = if c.description.is_empty() { String::new() } else { format!("  —  {}", c.description) };
        s.push_str(&format!("  /{}{}  [{}]{}\n", c.name, hint, c.source, desc));
    }
    Some(s.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(body: &str) -> CustomCommand {
        CustomCommand {
            name: "t".into(),
            description: String::new(),
            argument_hint: String::new(),
            model: None,
            body: body.into(),
            source: "global",
        }
    }

    #[test]
    fn expands_arguments_and_positionals() {
        let c = cmd("PR #$1 by $2 — all: $ARGUMENTS");
        assert_eq!(expand(&c, "42 alice extra").unwrap(), "PR #42 by alice — all: 42 alice extra");
    }

    #[test]
    fn missing_positionals_become_empty() {
        let c = cmd("a=$1 b=$2");
        assert_eq!(expand(&c, "only").unwrap(), "a=only b=");
    }

    #[test]
    fn at_file_inlines_and_nonfile_stays_literal() {
        // Reference this very test file via a relative path from cwd (the crate root in tests).
        let c = cmd("see @Cargo.toml here");
        let out = expand(&c, "").unwrap();
        assert!(out.contains("```Cargo.toml"), "should fence the file; got:\n{out}");
        assert!(out.contains("aizen"), "should inline Cargo.toml contents");

        // A `@word` that isn't a readable file passes through unchanged (doesn't corrupt prose).
        let miss = cmd("ping @everyone now");
        assert_eq!(expand(&miss, "").unwrap(), "ping @everyone now");
        let nofile = cmd("@no/such/file.xyz");
        assert_eq!(expand(&nofile, "").unwrap(), "@no/such/file.xyz");
    }

    #[test]
    fn email_like_at_is_left_alone() {
        // `@` only triggers a file ref at a word boundary (start / after whitespace). In `a@b.com`
        // the `@` follows `a`, so it's NOT matched — emails / handles pass through untouched.
        let c = cmd("ping a@b.com");
        assert_eq!(expand(&c, "").unwrap(), "ping a@b.com");
    }

    #[test]
    fn shell_readonly_runs_but_blocked_refuses() {
        // A read-only command (git status-shaped) is allowed; an `rm -rf /` is blocked.
        let blocked = cmd("danger: !`rm -rf /`");
        let e = expand(&blocked, "").unwrap_err().to_string();
        assert!(e.contains("safety floor"), "rm -rf / must be refused; got: {e}");

        // A clearly non-read-only but non-blocked command → Ask → refused (not auto-run).
        let ask = cmd("!`npm install left-pad`");
        assert!(expand(&ask, "").unwrap_err().to_string().contains("not auto-runnable"));
    }
}
