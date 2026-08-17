//! Where a conversation LIVES: the on-disk session store under `~/.aizen/sessions/`.
//!
//! One concern only — turning a `Vec<Message>` into a durable, named, project-tagged file and back
//! again, plus the live snapshot that survives an abrupt terminal close. It knows nothing about how
//! sessions are picked or displayed; `/sessions` and the restore menus are presentation and stay in
//! the REPL layer. Anything that writes to `sessions/` should go through here so the naming,
//! provenance and crash-flush rules cannot fork.

use crate::core::types::Message;
use crate::core::{cli_config, config};
use crate::memory;
use crate::ui::{theme, tui};
use crate::{fmt_time_ago, refresh_prompt_lanes_for_thread_switch};
use anyhow::{Context, Result};
use console::style;
use std::sync::{Mutex, OnceLock};

pub(crate) fn sessions_dir() -> std::path::PathBuf {
    config::aizen_home().join("sessions")
}
pub(crate) fn sanitize_name(s: &str) -> String {
    let n: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(80)
        .collect();
    let n = n.trim_matches(['.', ' ']);
    // Windows device names resolve specially even with an extension (`CON.json`, `NUL.json`, …).
    // Prefix them so a saved/restored session can never target a device path.
    let upper = n.to_ascii_uppercase();
    let numbered_device = |prefix: &str| {
        upper
            .strip_prefix(prefix)
            .is_some_and(|d| d.len() == 1 && d.as_bytes()[0].is_ascii_digit() && d != "0")
    };
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || numbered_device("COM")
        || numbered_device("LPT");
    if n.is_empty() {
        "session".to_string()
    } else if reserved {
        format!("session_{n}")
    } else {
        n.to_string()
    }
}
/// Why a save-as name can't be used, or `None` if it can. Split out of the picker's arm so the rule
/// is testable without driving the interactive prompt.
///
/// Only `last` is refused, and only here: it is the retired legacy-pointer name, which
/// [`scan_sessions`] deliberately skips. Accepting it would print "saved" for a file the picker can
/// neither restore nor delete, and pin every later autosave to it. Not folded into
/// [`sanitize_name`], which must keep mapping `last` verbatim so the legacy pointer stays loadable
/// and re-homable.
pub(crate) fn session_save_name_error(raw: &str) -> Option<&'static str> {
    (sanitize_name(raw.trim()) == "last").then_some(
        "“last” is the retired pointer name — pick another (it would not show up in /sessions)",
    )
}

/// Suggest a human-readable session name from the conversation's first user turn, so the "Save as"
/// prompt comes PRE-FILLED with the topic (Enter to accept, or edit) instead of a blank box. A short
/// hyphenated slug of the first few meaningful words + a date suffix to keep same-topic saves distinct.
///
/// Two properties this has to hold, both learned from what was on disk:
///
/// **No credential ever becomes a filename.** A key pasted as the first line of a chat used to pass
/// straight through — any token of 2+ chars was kept — and the derived stem is not just written to
/// disk but PRINTED by `/sessions`. A real machine had a session file named after a 40-char
/// vendor-prefixed token. Secret-shaped tokens are dropped here, before the name exists.
/// This is a name-derivation guard only; it does not redact the transcript body.
///
/// **ASCII whole words.** Names are folded through [`core::slug`] like every other id, so a
/// Vietnamese topic reads as `nguoi-dung-giao-tiep` rather than carrying diacritics into a filename
/// that then differs by normalization form between platforms. Folding happens per word, so no word
/// is ever cut apart. Existing accented files stay loadable — [`sanitize_name`] is unchanged, so it
/// still maps an on-disk name to itself.
pub(crate) fn suggest_session_name(history: &[Message]) -> String {
    let date = chrono::Local::now().format("%m%d").to_string();
    let first = history
        .iter()
        .find(|m| m.role == "user")
        .and_then(|m| m.content.as_deref())
        .unwrap_or("");
    // Skip slash-command / leading noise; take the first line, lowercase, keep word-ish chars.
    let line = first
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let words: Vec<String> = line
        .split_whitespace()
        .filter(|w| !crate::core::slug::looks_like_credential(w))
        // Intra-word punctuation is not a word boundary: `don't` is one word, so the apostrophe is
        // deleted rather than folded to `-` (which would leave a one-letter `t` fragment — exactly
        // the shredding this pass exists to remove).
        .map(|w| w.replace(['\'', '\u{2019}', '\u{02BC}'], ""))
        .map(|w| crate::core::slug::slug_words(&w, 40))
        .filter(|w| w.chars().count() >= 2)
        .take(5)
        .collect();
    let slug = words.join("-");
    // Cap length so long first messages don't produce an unwieldy default.
    let slug = crate::core::slug::truncate_at_word(&slug, 40);
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        format!("chat-{date}")
    } else {
        format!("{slug}-{date}")
    }
}
/// Provenance stamped into every saved session so a file can answer "which project, which model,
/// when?" without the user cross-referencing anything. Every field is optional: a hand-edited or
/// pre-provenance file still parses, absent just means "unknown".
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct SessionMeta {
    /// Normalized canonical path key of the project root — the exact string `project_slug()`
    /// hashes, so "same project?" agrees byte-for-byte with zone identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) project_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) project_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) project_slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) updated: Option<String>,
}

/// On-disk shape of a saved session: `{"version":2,"meta":{…},"messages":[…]}`. The
/// pre-provenance format was a bare `Vec<Message>` array — [`parse_session_bytes`] accepts both.
/// `version` and `meta` both default: a future writer may bump the version, and a hand-written or
/// partially-written file that still has `messages` is worth loading. Only a missing/unparsable
/// `messages` makes a file unreadable.
#[derive(serde::Deserialize)]
struct SessionFile {
    #[serde(default)]
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    meta: SessionMeta,
    messages: Vec<Message>,
}

/// Borrowed twin of [`SessionFile`] for writing, so every autosave doesn't clone the transcript.
#[derive(serde::Serialize)]
struct SessionFileRef<'a> {
    version: u32,
    meta: &'a SessionMeta,
    messages: &'a [Message],
}

/// Parse either session format, reporting WHY a file failed. `Err` = unreadable/corrupt (callers
/// surface that explicitly — a corrupt file must never masquerade as an empty conversation).
///
/// The reason is not decoration. A silently-swallowed `serde` error is exactly how a real bug hid
/// for months: our own `Serialize` wrote `content` as a multimodal parts array for any turn with a
/// pasted image, `Option<String>` refused to read it back, and every such conversation simply became
/// "(unreadable)" with nothing anywhere saying "invalid type: sequence". Naming the error makes the
/// next asymmetry visible the first time it happens instead of after 29 files.
pub(crate) fn parse_session_reason(
    bytes: &[u8],
) -> Result<(Vec<Message>, Option<SessionMeta>), String> {
    let envelope = match serde_json::from_slice::<SessionFile>(bytes) {
        Ok(f) => return Ok((f.messages, Some(f.meta))),
        Err(e) => e,
    };
    // Legacy bare-array format. Report the ENVELOPE's error when both fail: every file written since
    // the v2 format landed is an envelope, so that is the diagnosis that actually helps.
    match serde_json::from_slice::<Vec<Message>>(bytes) {
        Ok(m) => Ok((m, None)),
        Err(_) => Err(envelope.to_string()),
    }
}

/// [`parse_session_reason`] for the callers that only branch on success.
pub(crate) fn parse_session_bytes(bytes: &[u8]) -> Option<(Vec<Message>, Option<SessionMeta>)> {
    parse_session_reason(bytes).ok()
}

pub(crate) fn save_session(history: &[Message], name: &str, model: Option<&str>) -> Result<String> {
    // All three come from one cached identity lookup, so a file's key and slug can never disagree.
    write_session(
        history,
        name,
        SessionMeta {
            project_key: Some(config::project_key()),
            project_root: Some(config::project_root().display().to_string()),
            project_slug: Some(config::project_slug()),
            model: model
                .map(str::to_string)
                .or_else(|| cli_config::load().model),
            created: None,
            updated: None,
        },
    )
}

/// Re-home an UNATTRIBUTED transcript — the legacy `last.json` pointer copy — under a real slug
/// without inventing provenance for it. The pointer never recorded which project it came from, so
/// stamping the current one would assert on disk that another repo's conversation belongs here:
/// that lie then silences [`load_session`]'s cross-project warning, makes the file read as `here`
/// forever, and can never be undone (there is no original path left to restore). Absent fields are
/// the truth. Whatever the source DID record is carried through verbatim.
pub(crate) fn rehome_session(
    history: &[Message],
    name: &str,
    carried: Option<SessionMeta>,
    model: Option<&str>,
) -> Result<String> {
    let mut meta = carried.unwrap_or_default();
    meta.model = model.map(str::to_string).or(meta.model);
    write_session(history, name, meta)
}

/// How many saved sessions still carry a LEGACY zone slug — for `aizen zone migrate`'s plan.
/// Sessions are a flat pool keyed by the provenance INSIDE each file, so they are invisible to the
/// slug-directory sweep the rest of the migration does.
pub(crate) fn count_sessions_of_slug(legacy_slug: &str) -> usize {
    stat_sessions()
        .iter()
        .filter(|s| {
            read_session_row(&s.path)
                .1
                .and_then(|m| m.project_slug)
                .is_some_and(|sl| sl == legacy_slug)
        })
        .count()
}

/// Re-stamp every session recorded under `legacy_slug` with the CURRENT project identity — the
/// session leg of `aizen zone migrate`. Without it, a moved/re-cloned checkout (or a pre-fix
/// twin-zone population) left every one of the user's OWN transcripts reading as another project:
/// labeled `from <old dir>` in the picker and warned about on restore, permanently, because nothing
/// else in the migration touches provenance stored inside files.
///
/// `updated` is preserved: a bookkeeping rewrite must not make a stale conversation look freshly
/// used, exactly as memory retagging preserves its aging clock. (The file's mtime does move — std
/// has no portable way to set it, and adding a C dependency for cosmetics isn't worth it — so
/// `updated` stays the honest record of when the conversation itself last changed.)
pub(crate) fn retag_sessions_of_slug(legacy_slug: &str, on_error: &mut dyn FnMut(String)) -> usize {
    let key = config::project_key();
    let root = config::project_root().display().to_string();
    let slug = config::project_slug();
    let mut n = 0usize;
    for s in stat_sessions() {
        let Some((msgs, Some(mut meta))) = std::fs::read(&s.path)
            .ok()
            .and_then(|b| parse_session_bytes(&b))
        else {
            continue;
        };
        if meta.project_slug.as_deref() != Some(legacy_slug) {
            continue;
        }
        meta.project_key = Some(key.clone());
        meta.project_root = Some(root.clone());
        meta.project_slug = Some(slug.clone());
        let file = SessionFileRef {
            version: 2,
            meta: &meta,
            messages: &msgs,
        };
        let bytes = match serde_json::to_vec_pretty(&file) {
            Ok(mut b) => {
                b.push(b'\n');
                b
            }
            Err(e) => {
                on_error(format!("session {}: {e:#}", s.name));
                continue;
            }
        };
        match crate::core::persist::atomic_write(&s.path, &bytes)
            .and_then(|_| crate::core::persist::harden_owner_only_checked(&s.path))
        {
            Ok(_) => n += 1,
            Err(e) => on_error(format!("session {}: {e:#}", s.name)),
        }
    }
    n
}

/// Write a session file. `created` is preserved across the per-turn re-saves of one conversation
/// (existing file's stamp wins, then the caller's carried one, then now); `updated` is always now.
pub(crate) fn write_session(
    history: &[Message],
    name: &str,
    mut meta: SessionMeta,
) -> Result<String> {
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    config::harden_dir(&dir);
    let path = dir.join(format!("{}.json", sanitize_name(name)));
    let existing_created = std::fs::read(&path)
        .ok()
        .and_then(|b| parse_session_bytes(&b))
        .and_then(|(_, m)| m.and_then(|m| m.created));
    let now = chrono::Local::now().to_rfc3339();
    meta.created = existing_created
        .or(meta.created)
        .or_else(|| Some(now.clone()));
    meta.updated = Some(now);
    let file = SessionFileRef {
        version: 2,
        meta: &meta,
        messages: history,
    };
    let mut bytes = serde_json::to_vec_pretty(&file)?;
    bytes.push(b'\n');
    crate::core::persist::atomic_write(&path, &bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    // The transcript can contain pasted secrets / .env contents → owner-only.
    crate::core::persist::harden_owner_only_checked(&path)?;
    Ok(path.display().to_string())
}
pub(crate) fn load_session(history: &mut Vec<Message>, name: &str, model: &str) -> Result<usize> {
    let path = sessions_dir().join(format!("{}.json", sanitize_name(name)));
    let bytes = std::fs::read(&path).with_context(|| format!("no saved session '{name}'"))?;
    let (loaded, meta) = parse_session_reason(&bytes)
        .map_err(|why| anyhow::anyhow!("session '{name}' is unreadable: {why}"))?;
    *history = loaded;
    // Rebuild BOTH prompt lanes for the CURRENT project + model. The stable lane saved in the file
    // reflects wherever the session was recorded — replaying it verbatim in another checkout
    // grafted the OTHER project's <project_context>/frozen core onto this cwd: every tool ran here
    // while the model was told it was there. The splice keeps the conversation tail (and any
    // handoff seed) intact. Thread-switch resets (todos/cost/grants) are the CALLER's job — this
    // function only loads, so tests and backup paths can use it without mutating global state.
    refresh_prompt_lanes_for_thread_switch(history, model);
    // Cross-project restore is allowed but must be LOUD: name the source so "why does the model
    // think it's in the other repo?" never needs source-diving. Files without provenance stay
    // silent — there is nothing truthful to warn with.
    if let Some(theirs) = meta.as_ref().and_then(|m| m.project_key.as_ref()) {
        if *theirs != config::project_key() {
            let from = meta
                .as_ref()
                .and_then(|m| m.project_root.clone().or_else(|| m.project_slug.clone()))
                .unwrap_or_else(|| "unknown".to_string());
            // If the recorded directory is GONE, "another project" is the wrong accusation: the
            // overwhelmingly likely story is that this very checkout was moved or renamed, so the
            // conversation is the user's own history. Same facts, honest reading — and the phrasing
            // still says what was rebuilt, because the lane rewrite happened either way.
            let vanished = meta
                .as_ref()
                .and_then(|m| m.project_root.as_deref())
                .is_some_and(|r| !std::path::Path::new(r).exists());
            let headline = if vanished {
                format!("⚠ this session was saved at {from}, which no longer exists — moved or renamed project?")
            } else {
                format!("⚠ this session was saved in another project: {from}")
            };
            tui::emit_line(&style(headline).color256(theme::WARN).to_string());
            tui::emit_line(
                &style(format!(
                    "  system context rebuilt for the current project: {}",
                    config::project_root().display()
                ))
                .color256(theme::WARN)
                .to_string(),
            );
        }
    }
    // Continue autosaving into the SAME file we just restored (don't spawn a fresh slug next turn)
    // — EXCEPT the legacy `last` pointer copy: pinning the live slug to `last` would make every
    // later turn overwrite the pointer instead of a real conversation, so re-home it first.
    let slug = sanitize_name(name);
    if slug == "last" {
        let fresh = allocate_session_slug(history);
        // The transcript is already restored into `history` at this point, so a failed re-home must
        // not fail the restore — report it and leave the slug unpinned, which makes the next autosave
        // allocate a fresh name (and say so) instead of overwriting the pointer.
        match save_session(history, &fresh, Some(model)) {
            Ok(_) => set_session_slug(Some(fresh)),
            Err(e) => {
                set_session_slug(None);
                tui::emit_line(&format!(
                    "{} could not re-home the legacy `last` pointer: {e:#} — this chat will be saved under a new name on the next turn",
                    theme::warn("⚠")
                ));
            }
        }
    } else {
        set_session_slug(Some(slug));
    }
    // Keep the exit-flush snapshot in step with what was just restored, so an abrupt window close
    // right after re-saves this conversation, not a stale one.
    update_live_history(history);
    Ok(conversation_len(history))
}
static SESSION_SLUG: OnceLock<Mutex<Option<String>>> = OnceLock::new();

pub(crate) fn session_slug_slot() -> &'static Mutex<Option<String>> {
    SESSION_SLUG.get_or_init(|| Mutex::new(None))
}

pub(crate) fn set_session_slug(slug: Option<String>) {
    let slug = slug.map(|s| sanitize_name(&s));
    *session_slug_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = slug.clone();
    crate::core::recovery::set_session_name(slug);
}

pub(crate) fn current_session_slug() -> Option<String> {
    session_slug_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}
pub(crate) fn pretty_session_name(name: &str) -> String {
    name.replace('-', " ")
}

/// Remove `atomic_write` staging files abandoned by a killed process.
///
/// `persist::atomic_write` deletes its temp only when the write fails IN-PROCESS; a process killed
/// between the staged write and the rename leaves `.{name}.aizen-tmp-{pid}-{seq}` behind with nothing
/// to ever collect it. Harmless to readers (`stat_sessions` filters on the `.json` extension) but it
/// accumulates silently for the life of the install.
///
/// Age-gated because the name is not enough: another aizen window may be mid-write RIGHT NOW, and its
/// staging file is invisible to us. A minute is far longer than any staged write and far shorter than
/// the interval at which anyone would notice clutter. Best-effort — never blocks startup.
pub(crate) fn sweep_orphan_temps() {
    let Ok(rd) = std::fs::read_dir(sessions_dir()) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with('.') || !name.contains(".aizen-tmp-") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|md| md.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age.as_secs() > 60);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// One row of the session pool, as scanned from disk.
pub(crate) struct SessionInfo {
    pub(crate) name: String,
    /// Real conversation turns (leading system lanes excluded); `None` = unreadable/corrupt file —
    /// distinct from a readable empty one, so the picker can say "(unreadable)" instead of "0 msgs".
    pub(crate) msgs: Option<usize>,
    pub(crate) meta: Option<SessionMeta>,
    /// Modification time in Unix MILLIseconds. `None` = the filesystem wouldn't say (network share,
    /// FUSE mount, transient ACL error) — rendered as "age unknown" rather than posing as fresh.
    /// Milliseconds, not seconds, because two saves inside one second are routine (a `/handoff` and
    /// the seeded turn's autosave) and a second-granularity tie fell through to ALPHABETICAL order,
    /// which points the wrong way as often as not while the picker still claims "newest first".
    pub(crate) mtime_ms: Option<u64>,
    /// Saved from THIS project? `None` = no provenance (pre-provenance file, project unknown).
    pub(crate) here: Option<bool>,
}

/// One session file as seen WITHOUT reading it. Statting a directory is cheap; deserializing every
/// multi-MB transcript in it is not — and the startup hint runs before the first prompt is even
/// accepted, so it must not pay for the whole pool just to name one conversation.
pub(crate) struct SessionStat {
    pub(crate) name: String,
    pub(crate) path: std::path::PathBuf,
    pub(crate) mtime_ms: Option<u64>,
    /// Sort key: mtime clamped to now. A stamp in the FUTURE (clock skew from a VM resume, a
    /// pre-NTP boot, a dual-boot clock) would otherwise pin one file to the top of every launch's
    /// offer forever. `None` (filesystem wouldn't say) sorts last.
    pub(crate) recency: Option<u64>,
}

/// The pool, newest first, without reading any transcript. One ordering, shared by the hint and the
/// picker, so the two surfaces can never disagree about which conversation is the newest. The legacy
/// `last.json` pointer is a duplicate COPY of some conversation, not a session of its own — it never
/// appears as a row.
pub(crate) fn stat_sessions() -> Vec<SessionStat> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(sessions_dir()) {
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Some(name) = path
                .file_stem()
                .and_then(|x| x.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if name == "last" {
                continue;
            }
            let mtime_ms = e
                .metadata()
                .ok()
                .and_then(|md| md.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64);
            out.push(SessionStat {
                name,
                path,
                mtime_ms,
                recency: None,
            });
        }
    }
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    for s in &mut out {
        s.recency = s.mtime_ms.map(|ms| ms.min(now_ms));
    }
    // Milliseconds, not seconds: two saves inside one second are routine (a `/handoff` and the
    // seeded turn's autosave), and a second-granularity tie fell through to ALPHABETICAL order,
    // which points the wrong way as often as not while the prompt still says "newest first". Name
    // order remains the last resort so the sort is total and stable.
    out.sort_by(|a, b| b.recency.cmp(&a.recency).then_with(|| a.name.cmp(&b.name)));
    out
}

/// Read one row's transcript-derived fields. `(None, None)` = unreadable/corrupt.
pub(crate) fn read_session_row(path: &std::path::Path) -> (Option<usize>, Option<SessionMeta>) {
    match std::fs::read(path)
        .ok()
        .and_then(|b| parse_session_bytes(&b))
    {
        Some((m, meta)) => (Some(conversation_len(&m)), meta),
        None => (None, None),
    }
}

/// Read the whole pool, newest first — for the `/sessions` picker, which shows every row and so
/// genuinely needs every file parsed. The startup hint uses [`most_recent_session`] instead, which
/// parses lazily in the same order.
pub(crate) fn scan_sessions() -> Vec<SessionInfo> {
    let here_key = config::project_key();
    stat_sessions()
        .into_iter()
        .map(|s| {
            let (msgs, meta) = read_session_row(&s.path);
            let here = meta
                .as_ref()
                .and_then(|m| m.project_key.as_ref())
                .map(|k| *k == here_key);
            SessionInfo {
                name: s.name,
                msgs,
                meta,
                mtime_ms: s.mtime_ms,
                here,
            }
        })
        .collect()
}

/// Age of a session file for the picker. Distinct from [`fmt_time_ago`], which was written for
/// `/init --status` and maps both 0 and future stamps to "just now" — for a session row that would
/// print "just now" on an unreadable mtime and on a clock-skewed file, i.e. exactly the two cases
/// the user needs told apart from a genuinely fresh save.
pub(crate) fn fmt_session_age(mtime_ms: Option<u64>) -> String {
    let Some(ms) = mtime_ms else {
        return "age unknown".to_string();
    };
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    if ms > now_ms.saturating_add(60_000) {
        return "future timestamp (clock skew)".to_string();
    }
    fmt_time_ago((ms / 1000).max(1)) // .max(1): second 0 is fmt_time_ago's "unknown" sentinel
}

/// Age in at most three cells: `now`, `5m`, `19h`, `62d`, `2y`, or `?` when the mtime is unreadable.
///
/// For a LIST, not a status line. `fmt_session_age` spells the unit out, which is right when one age
/// stands alone but wrong down a column: `19 hour(s) ago` against `1 min ago` is a six-character
/// jitter sitting directly in front of the subject, so nothing lines up and the eye has to re-find
/// the text on every row. A clock-skewed file reads `now` rather than announcing the skew — in a
/// 240-row picker that sentence is longer than the row it describes, and the file still sorts first.
pub(crate) fn fmt_session_age_compact(mtime_ms: Option<u64>) -> String {
    let Some(ms) = mtime_ms else {
        return "?".to_string();
    };
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let secs = now_ms.saturating_sub(ms) / 1000;
    match secs {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        86_400..=31_535_999 => format!("{}d", secs / 86_400),
        _ => format!("{}y", secs / 31_536_000),
    }
}

/// Short human label for where a foreign or unlabeled session came from — the picker/hint suffix.
/// Takes the meta rather than the row so the `last.json` re-home path (which has no row) can use it.
///
/// A recorded root that no longer EXISTS is flagged as such rather than presented as a live sibling
/// project: the usual cause is this checkout being moved or renamed, in which case a bare
/// "from <old dir>" is the picker calling the user's own history someone else's.
pub(crate) fn session_origin_label(meta: Option<&SessionMeta>) -> String {
    let root = meta.and_then(|m| m.project_root.as_deref());
    let named = root
        .and_then(|r| std::path::Path::new(r).file_name().and_then(|n| n.to_str()))
        .map(str::to_string)
        .or_else(|| meta.and_then(|m| m.project_slug.clone()));
    match named {
        Some(n) if root.is_some_and(|r| !std::path::Path::new(r).exists()) => {
            format!("from {n} (path gone)")
        }
        Some(n) => format!("from {n}"),
        None => "project unknown".to_string(),
    }
}

/// The session to offer for bare `/resume` and the startup hint: the newest one saved FROM THIS
/// project. Only when this project has none does it fall back to the pool's newest, labeled with
/// its origin — a cross-project resume must be a visible choice, never a trap.
///
/// Returns `(slug, conversation_turns, origin_label)`; the label is `None` for a same-project
/// offer. `None` overall when nothing restorable has been saved yet.
pub(crate) fn most_recent_session() -> Option<(String, usize, Option<String>)> {
    let here_key = config::project_key();
    // Parse in newest-first order and STOP at the first same-project hit, rather than deserializing
    // the whole pool to name one conversation. This runs before the first prompt is accepted, and a
    // long-lived pool is dozens of multi-MB transcripts (autosave per turn + a slug per handoff) —
    // on an AV-scanned or cloud-synced profile dir the eager scan was seconds of silent dead time.
    let stats = stat_sessions();
    let mut rows: Vec<(usize, usize, Option<SessionMeta>)> = Vec::new(); // (index, turns, meta)
    let mut best: Option<usize> = None; // index into `rows` of the best tier seen so far
    let tier = |meta: &Option<SessionMeta>| match meta.as_ref().and_then(|m| m.project_key.as_ref())
    {
        // Three tiers, not two. A file with NO provenance is not evidence of a foreign project — on
        // the first launch after upgrading EVERY file is keyless, so folding `None` in with
        // `Some(false)` made the whole pool "project unknown" and left the prefer-this-project rule
        // dead until each file had been resumed once. Unlabeled ranks between mine and theirs.
        Some(k) if *k == here_key => 0u8,
        None => 1,
        Some(_) => 2,
    };
    for (i, s) in stats.iter().enumerate() {
        let (msgs, meta) = read_session_row(&s.path);
        let Some(turns) = msgs else { continue }; // unreadable/corrupt — never offered
        let t = tier(&meta);
        rows.push((i, turns, meta));
        if best.is_none_or(|b| t < tier(&rows[b].2)) {
            best = Some(rows.len() - 1);
        }
        if t == 0 {
            break; // newest same-project file — nothing later in the order can beat it
        }
    }
    if let Some(b) = best {
        let (i, turns, meta) = &rows[b];
        // Only a file that PROVES it came from elsewhere gets the origin suffix. `None` provenance
        // has nothing truthful to say — the same rule `load_session` applies to its warning.
        let label = (tier(meta) == 2).then(|| session_origin_label(meta.as_ref()));
        return Some((stats[*i].name.clone(), *turns, label));
    }
    // Nothing restorable in the pool — which is NOT the same as an empty pool: one stray unparsable
    // `.json` in the dir used to make this fallback unreachable, hiding a perfectly readable
    // pointer-era transcript from the hint AND (since `last` is never a row) from the picker too.
    // Pre-provenance pool where only the shared `last.json` copy ever existed: re-home that
    // transcript into a real named file once, so it shows up in /sessions from now on.
    let bytes = std::fs::read(sessions_dir().join("last.json")).ok()?;
    let (msgs, carried) = parse_session_bytes(&bytes)?;
    if !msgs.iter().any(|m| m.role == "user") {
        return None;
    }
    let fresh = allocate_session_slug(&msgs);
    // Carry the pointer's own meta through rather than stamping THIS project onto it: the pointer
    // was project-blind, so claiming it as ours would be a lie that also silences load_session's
    // cross-project warning forever. Unattributed → offered as "project unknown", honestly.
    let label = carried
        .as_ref()
        .and_then(|m| m.project_key.as_ref())
        .map_or_else(
            || Some("project unknown".to_string()),
            |k| (*k != config::project_key()).then(|| session_origin_label(carried.as_ref())),
        );
    // Re-homing is a convenience, not a precondition: if the dir is unwritable the transcript
    // is still READABLE, so keep offering it under the legacy `last` name rather than pretending
    // there is nothing to resume. `load_session` re-homes on restore (and reports if that fails).
    match rehome_session(&msgs, &fresh, carried, None) {
        Ok(_) => Some((fresh, conversation_len(&msgs), label)),
        Err(_) => Some(("last".to_string(), conversation_len(&msgs), label)),
    }
}

/// Count of real conversation turns (excluding the leading system lanes) — for the resume hint, so
/// it reports what the user recognizes as "messages" rather than raw vector length.
pub(crate) fn conversation_len(history: &[Message]) -> usize {
    history
        .len()
        .saturating_sub(crate::agent::compact::leading_system_count(history))
}

pub(crate) async fn autosave_session(
    history: &[Message],
    _http: &reqwest::Client,
    _base_url: &str,
    _api_key: &str,
    model: &str,
) {
    autosave_last(history, Some(model));
}

pub(crate) fn delete_session(name: &str) -> Result<()> {
    let path = sessions_dir().join(format!("{}.json", sanitize_name(name)));
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    Ok(())
}
/// Pick a distinct on-disk slug for a brand-new (unnamed) conversation: the topic suggestion, plus a
/// numeric suffix if a session with that name already exists. Without this every unnamed chat collided
/// on the shared `last` slug and overwrote the previous one, so `/sessions` only ever showed the latest.
pub(crate) fn allocate_session_slug(history: &[Message]) -> String {
    let base = sanitize_name(&suggest_session_name(history));
    let dir = sessions_dir();
    if !dir.join(format!("{base}.json")).exists() {
        return base;
    }
    for n in 2..1000 {
        let cand = format!("{base}-{n}");
        if !dir.join(format!("{cand}.json")).exists() {
            return cand;
        }
    }
    base
}

/// A process-global snapshot of the live conversation, kept fresh so the Windows console control
/// handler (window ✕ / logoff / shutdown) can flush the current chat to disk from its own thread
/// before the process is killed. The main thread never reads it back — it's write-for-the-handler.
static LIVE_HISTORY: OnceLock<Mutex<Vec<Message>>> = OnceLock::new();

pub(crate) fn live_history_slot() -> &'static Mutex<Vec<Message>> {
    LIVE_HISTORY.get_or_init(|| Mutex::new(Vec::new()))
}

/// Refresh the snapshot the exit-flush path saves. Called after every user push (mid-turn safety) and
/// at the end of each autosave (so the snapshot is always at least as new as what's on disk).
pub(crate) fn update_live_history(history: &[Message]) {
    *live_history_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = history.to_vec();
}

/// Persist the live conversation on any exit — graceful (/quit, Ctrl-D) or abrupt (window ✕ via the
/// Windows console handler). Safe to call from a foreign thread: it only does synchronous file I/O.
pub(crate) fn flush_live_session_on_exit() {
    // Route the notices for teardown: the "· saving as" line is pointless when the render thread may
    // already be gone, and a failure must go to stderr instead of the TUI. Cleared on the way out so
    // this can't permanently mute a process that keeps running (the unix SIGHUP handler and the
    // test suite both call this without exiting).
    EXIT_FLUSHING.store(true, std::sync::atomic::Ordering::Relaxed);
    let snapshot = live_history_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    // No REPL model label on this thread — `save_session` falls back to the configured model.
    autosave_last(&snapshot, None);
    append_memory_stats_sample();
    EXIT_FLUSHING.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Append this session's §8 sample to `cli-memory/stats.jsonl`.
///
/// Runs on the exit path rather than per turn for two reasons: the populations are a directory scan
/// (three `load_*` calls) that has no business inside a turn, and one line per session keeps the
/// series readable by hand. The in-process counters are cumulative across the session, so a single
/// line at the end loses nothing but the intra-session shape — which no metric asks about.
///
/// Wholly best-effort. A failure to read the store means no sample, never a failed exit; and
/// `stats::append` itself declines to write when the session ran zero turns.
pub(crate) fn append_memory_stats_sample() {
    let all = memory::store::load_all().unwrap_or_default();
    let live = memory::bloat::supersede::active(&all).len();
    let archived = memory::bloat::caps::list_archive()
        .unwrap_or_default()
        .len();
    let review = memory::store::load_from(&crate::core::config::review_dir())
        .unwrap_or_default()
        .len();
    memory::stats::append(live, archived, all.len().saturating_sub(live), review);
}

/// Set while the exit-flush path is writing, so autosave stays silent during teardown.
static EXIT_FLUSHING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Mid-turn progress hook hnaded to the agent loop as a plain `fn` pointer (so `AgentConfig` keeps
/// its `Clone + Debug` derives and stays `dyn`-free).
///
/// Without this the exit-flush snapshot only advanced when a turn STARTED or FINISHED: the loop owns
/// `history` mutably for the whole turn, so a terminal closed mid-turn saved the user's question and
/// threw away every assistant reply and tool result the turn had already produced. The loop calls
/// this at each iteration boundary — the same point steering drains, where history is guaranteed
/// coherent (no `tool_calls` awaiting results) — so what lands on disk is always a valid transcript.
/// Memory-only by design; the actual file write happens once, on exit.
pub(crate) fn publish_live_history(history: &[Message]) {
    update_live_history(history);
}

/// Catch the terminal window being closed (✕), user logoff and system shutdown so the live chat is
/// flushed to `/sessions` before the OS terminates us. Ctrl-C / Ctrl-Break are deliberately left to
/// the existing in-app cancel handling (we return FALSE for them, changing nothing).
///
/// Both halves are needed because "the user closed the terminal" is a different OS event per
/// platform: a console control event on Windows, `SIGHUP` (pty hangup) or `SIGTERM` on unix. Without
/// the unix half, closing a terminal there killed the process with no flush at all and the whole
/// conversation was lost — the exact failure this handler exists to prevent.
pub(crate) fn install_exit_flush_handler() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::BOOL;
        use windows_sys::Win32::System::Console::{
            SetConsoleCtrlHandler, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
        };
        unsafe extern "system" fn handler(ctrl_type: u32) -> BOOL {
            if matches!(
                ctrl_type,
                CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT
            ) {
                // Never let a panic unwind across the FFI boundary into the OS caller.
                let _ = std::panic::catch_unwind(flush_live_session_on_exit);
            }
            0 // FALSE — let the system proceed with default termination in all cases.
        }
        unsafe {
            let _ = SetConsoleCtrlHandler(Some(handler), 1);
        }
    }
    // Unix: closing the terminal emulator hangs up the pty (`SIGHUP`); a session manager or
    // `kill` sends `SIGTERM`. Default disposition for both is immediate termination, so the
    // transcript needs saving from the handler task. Watched on the tokio runtime (async signal
    // handling, no `unsafe`), then we exit ourselves — the default action is what the sender asked
    // for, so restoring the terminal and leaving is the honest response to it.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        for kind in [SignalKind::hangup(), SignalKind::terminate()] {
            if let Ok(mut sig) = signal(kind) {
                tokio::spawn(async move {
                    if sig.recv().await.is_some() {
                        flush_live_session_on_exit();
                        // Give the alt-screen back before dying, else the user's shell is left in
                        // raw mode with no cursor.
                        tui::deactivate();
                        std::process::exit(0);
                    }
                });
            }
        }
    }
}

/// Best-effort auto-save of the live conversation (called after each turn and on exit) so you can
/// always come back to it via `/sessions` without ever running an explicit save. A brand-new chat is
/// given its own distinct file on first save so it never overwrites another unnamed conversation.
pub(crate) fn autosave_last(history: &[Message], model: Option<&str>) {
    if history.iter().any(|m| m.role == "user") {
        let name = match current_session_slug() {
            Some(n) => n,
            None => {
                let slug = allocate_session_slug(history);
                set_session_slug(Some(slug.clone()));
                // Name the file OUT LOUD the moment it exists: "which file is THIS conversation
                // being written to?" must be answerable from the screen, not from source.
                if !EXIT_FLUSHING.load(std::sync::atomic::Ordering::Relaxed) {
                    tui::emit_line(&style(format!("· saving as “{slug}”")).dim().to_string());
                }
                slug
            }
        };
        // "auto-saves as you go" is a PROMISE (/help says so). When the write fails — full disk,
        // ACL damage, OneDrive/AV lock on ~/.aizen — swallowing it meant the user worked for hours
        // believing the transcript was on disk and found nothing to resume. Say it once per failure
        // streak (not every turn), and say it again after a recovery so the state is never stale.
        match save_session(history, &name, model) {
            Ok(_) => {
                if AUTOSAVE_BROKEN.swap(false, std::sync::atomic::Ordering::Relaxed)
                    && !EXIT_FLUSHING.load(std::sync::atomic::Ordering::Relaxed)
                {
                    tui::emit_line(
                        &style("· autosave recovered — this conversation is being saved again")
                            .dim()
                            .to_string(),
                    );
                }
            }
            Err(e) => {
                if !AUTOSAVE_BROKEN.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    let msg = format!(
                        "⚠ autosave failed: {e:#} — this conversation is NOT being saved. Fix the path or use /sessions to save elsewhere."
                    );
                    // The TUI may already be torn down on the exit-flush path; stderr still lands.
                    if EXIT_FLUSHING.load(std::sync::atomic::Ordering::Relaxed) {
                        eprintln!("{msg}");
                    } else {
                        tui::emit_line(&style(msg).color256(theme::WARN).to_string());
                    }
                }
            }
        }
        update_live_history(history);
        let _ = crate::core::recovery::checkpoint_history(
            history,
            None,
            crate::core::recovery::RecoveryPhase::Idle,
        );
    }
}

/// Latch so a persistent autosave failure warns ONCE per streak instead of every turn (and reports
/// once when writes start working again).
static AUTOSAVE_BROKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
