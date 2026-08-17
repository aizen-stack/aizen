//! `aizen where` / `/where` — the identity report: which project this is, which slug it hashed to,
//! and exactly which directory every kind of state lives in.
//!
//! Shared verbatim by both surfaces so they can never disagree about where a fact was written. A
//! remote URL is redacted before display: identity output must never print an embedded credential.

use crate::core::session_store::sessions_dir;
use console::style;

/// Strip URL userinfo before display when it carries a password/token
/// (`https://user:TOKEN@host/…`) — remote URLs may embed credentials and the identity surfaces
/// must never print one. A plain username (`git@host:…`) is kept: it isn't a secret and losing
/// it would make the URL unrecognizable.
pub(crate) fn redact_remote_url(url: &str) -> String {
    let (scheme, rest) = match url.find("://") {
        Some(i) => url.split_at(i + 3),
        None => ("", url),
    };
    match rest.find('@') {
        Some(at) if rest[..at].contains(':') => format!("{scheme}***@{}", &rest[at + 1..]),
        _ => url.to_string(),
    }
}

/// How many `*.md` files a store directory holds, and a `(not created yet)` note when it doesn't
/// exist. Shared by both `where` reports so an absent folder never reads as an empty one.
fn dir_count_line(label: &str, p: &std::path::Path, unit: &str) -> String {
    if !p.exists() {
        return format!("  {label:<8}: {}   (not created yet)", p.display());
    }
    let n = std::fs::read_dir(p)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md"))
                .count()
        })
        .unwrap_or(0);
    format!("  {label:<8}: {}   {n} {unit}", p.display())
}

/// Where the memory store physically lives, per directory, with counts.
///
/// `memory list` names three commands and every one of them edits a SINGLE entry by id. Bulk work —
/// deleting forty near-duplicates, fixing a wrong word across many facts — is a file-manager job, and
/// until now the only place any path appeared was `memory show <id>`'s `file:` line, one entry at a
/// time. Naming the review dir matters most: 29 queued candidates sat there unreadable because
/// nothing said they were on disk at all.
pub(crate) fn memory_where_report() -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{}",
        dir_count_line("entries", &crate::core::config::entries_dir(), "fact(s)")
    );
    let _ = writeln!(
        s,
        "{}",
        dir_count_line("review", &crate::core::config::review_dir(), "awaiting")
    );
    let _ = writeln!(
        s,
        "{}",
        dir_count_line("archive", &crate::core::config::archive_dir(), "retired")
    );
    let graph = crate::core::config::graph_path();
    let edges = std::fs::read_to_string(&graph)
        .map(|r| r.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);
    let _ = writeln!(s, "  {:<8}: {}   {edges} edge(s)", "graph", graph.display());
    let _ = writeln!(
        s,
        "  {:<8}: {}",
        "core",
        crate::core::config::style_path().display()
    );
    let _ = write!(
        s,
        "{}",
        style(
            "\nEdit or delete files directly — they are plain markdown with a frontmatter header.\n\
             Re-run `aizen memory doctor` afterwards to catch anything left dangling."
        )
        .dim()
    );
    s
}

/// Where skills are read from — all three roots, because `skill list`'s `[project]`/`[repo]` tags
/// say which root a row came from without saying where that root is, and auto-learned skills land in
/// the zone dir whose slug (`p/admin-5296147b`) is not guessable from the project name.
pub(crate) fn skill_where_report() -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{}",
        dir_count_line("global", &crate::skills::skills_dir(), "skill(s)")
    );
    let _ = writeln!(
        s,
        "{}",
        dir_count_line("zone", &crate::skills::project_zone_dir(), "skill(s)")
    );
    let _ = writeln!(
        s,
        "  {}",
        style(format!(
            "         ↑ auto-learned skills for zone {}",
            crate::core::config::project_slug()
        ))
        .dim()
    );
    let _ = write!(
        s,
        "{}",
        dir_count_line("repo", &crate::skills::project_skills_dir(), "skill(s)")
    );
    s
}

/// The identity card — one honest surface for the questions that previously had none: which
/// root am I in, which zone does my memory go to, which git binary runs, where do sessions live.
/// Shared verbatim by `aizen where` (println) and `/where` (tui::emit_line).
pub(crate) fn where_report() -> String {
    use std::fmt::Write as _;
    let root = crate::core::config::project_root();
    let slug = crate::core::config::project_slug();
    let home = crate::core::config::aizen_home();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".into());
    let mut s = String::new();
    let _ = writeln!(s, "project root : {}", root.display());
    if let Ok(over) = std::env::var("AIZEN_PROJECT_ROOT") {
        if !over.trim().is_empty() {
            let _ = writeln!(
                s,
                "               (root forced by AIZEN_PROJECT_ROOT={})",
                over.trim()
            );
        }
    }
    let _ = writeln!(
        s,
        "cwd          : {cwd}   (identity follows the root, fixed at launch)"
    );
    let _ = writeln!(
        s,
        "zone slug    : {slug}   (keys memory scope · skills · codebase index · frozen core)"
    );
    if let Some(url) = crate::core::config::git_remote_origin(&root) {
        let _ = writeln!(
            s,
            "git remote   : {}   (informational — no longer part of the identity key)",
            redact_remote_url(&url)
        );
    }
    match crate::core::gitx::git_exe() {
        Some(p) => {
            let _ = writeln!(s, "git          : {}", p.display());
        }
        None => {
            let _ = writeln!(
                s,
                "git          : NOT FOUND — identity uses the nearest .git marker (or this folder); time-machine checkpoints are off"
            );
        }
    }
    if let Some(note) = crate::core::gitx::resolution_note() {
        if crate::core::gitx::git_exe().is_some() {
            let _ = writeln!(s, "               ({note})");
        }
    }
    let zone_dir = crate::skills::project_zone_dir();
    let idx = crate::core::config::codebase_index_path(&slug);
    let exists = |p: &std::path::Path| {
        if p.exists() {
            ""
        } else {
            "   (not created yet)"
        }
    };
    let _ = writeln!(s, "home         : {}", home.display());
    let _ = writeln!(
        s,
        "memory store : {}",
        crate::core::config::cli_memory_dir().display()
    );
    let _ = writeln!(
        s,
        "skills zone  : {}{}",
        zone_dir.display(),
        exists(&zone_dir)
    );
    let _ = writeln!(s, "codebase idx : {}{}", idx.display(), exists(&idx));
    let _ = writeln!(s, "sessions     : {}", sessions_dir().display());
    if let Some(n) = sessions_with_secrets() {
        let _ = writeln!(
            s,
            "⚠ secrets     : {n} saved transcript(s) contain credential-shaped text — a key pasted into a chat is stored verbatim. Open the folder above and edit or delete them."
        );
    }
    if let Some(l) = crate::features::zones::quick_legacy_probe() {
        let _ = writeln!(
            s,
            "⚠ legacy zone : {l} — data from the old slug keying; `aizen zone migrate` shows what would merge (--apply to do it)"
        );
    }
    s.trim_end().to_string()
}

/// How many saved transcripts hold credential-shaped text, or `None` when none do.
///
/// Names are guarded at derivation (see [`suggest_session_name`]), but a key pasted into a chat is
/// still in that file's message text: a saved session is a verbatim transcript, and nothing redacts
/// it on the way to disk. Deleting or rewriting a user's own conversation history is not a call this
/// tool makes on its own, so `/where` reports the count and names the folder — the number is
/// actionable, and the values are never printed.
///
/// Uses the vendor-prefix test, NOT the shape test that guards name derivation. Measured on the 27
/// real transcripts here: prefix matches 12 strings, all real keys; shape matched 5170, of which 4026
/// were ISO timestamps (long, mixed-case, letters and digits — indistinguishable from key material by
/// shape alone). A warning that fires on every file teaches the user to ignore it.
///
/// Counts FILES, not occurrences: the useful signal is "which files do I need to open".
fn sessions_with_secrets() -> Option<usize> {
    let rd = std::fs::read_dir(sessions_dir()).ok()?;
    let n = rd
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .filter(|e| {
            std::fs::read_to_string(e.path()).is_ok_and(|raw| {
                raw.split(|c: char| c.is_whitespace() || matches!(c, '"' | ',' | '\\'))
                    .any(crate::core::slug::has_vendor_key_prefix)
            })
        })
        .count();
    (n > 0).then_some(n)
}
