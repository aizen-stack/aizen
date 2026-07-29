//! What THIS session last saw on disk, per path — the missing half of conflict detection.
//!
//! [`crate::core::persist::compare_and_atomic_write`] compares against a fingerprint taken moments
//! earlier inside one tool call, so it closes the read→write window against non-cooperating writers
//! and nothing else. The window it cannot close is the interesting one: a session reads a file in
//! turn 1, another window rewrites it in turn 2, and the first session writes in turn 3 from a
//! three-turn-old idea of the content. Every lock is honored at every instant and the read-modify-
//! write is still torn.
//!
//! `file_edit` / `multi_edit` survive that on their own: `old_string` is matched against a FRESH read,
//! so a rewritten region simply fails to match. `file_write` has no such anchor — it is a whole-file
//! overwrite whose CAS passes against whatever is on disk right now, so the other window's work
//! vanishes with no error. This ledger is the anchor: record the fingerprint at every read, and let a
//! full overwrite refuse when what is on disk is no longer what this session read.
//!
//! Scope is deliberately per-PROCESS, not persisted: it answers "has the ground moved under THIS
//! session since it looked", and a fresh process has not looked at anything yet.

use crate::core::persist::FileFingerprint;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Ceiling on remembered paths. A long session reading thousands of files must not grow this without
/// bound; evicting the oldest observation only ever costs a conflict check, never correctness of a
/// write that does happen.
const MAX_TRACKED: usize = 8192;

/// path → (fingerprint observed, monotonic sequence for eviction).
static SEEN: Mutex<Option<HashMap<PathBuf, (FileFingerprint, u64)>>> = Mutex::new(None);

fn with_map<T>(f: impl FnOnce(&mut HashMap<PathBuf, (FileFingerprint, u64)>) -> T) -> T {
    let mut guard = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(HashMap::new))
}

/// Canonicalize for identity, falling back to the path as given: two spellings of one file
/// (`./x`, `x`, a symlink) must share one observation, but an unreadable path is still worth keying.
fn key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Record what a read (or a completed write) saw at `path`. Called by every tool that observes file
/// content, including after a successful write — the bytes we just wrote ARE what this session now
/// knows to be there, and forgetting that would make a session's own second write look like someone
/// else's interference.
pub fn note(path: &Path, fingerprint: &FileFingerprint) {
    let k = key(path);
    with_map(|m| {
        let seq = crate::core::persist::unique_sequence();
        if m.len() >= MAX_TRACKED && !m.contains_key(&k) {
            if let Some(oldest) = m
                .iter()
                .min_by_key(|(_, (_, s))| *s)
                .map(|(p, _)| p.clone())
            {
                m.remove(&oldest);
            }
        }
        m.insert(k, (fingerprint.clone(), seq));
    });
}

/// What this session last saw at `path`, if it ever looked.
pub fn observed(path: &Path) -> Option<FileFingerprint> {
    let k = key(path);
    with_map(|m| m.get(&k).map(|(fp, _)| fp.clone()))
}

/// Drop an observation — for a path that was moved or removed, whose old identity is now meaningless.
pub fn forget(path: &Path) {
    let k = key(path);
    with_map(|m| m.remove(&k));
}

/// Why a full-file overwrite is being refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stale {
    /// This session read the file, and what is on disk no longer matches that read.
    ChangedSinceRead,
    /// This session never read the file, and another live session has changed it.
    NeverReadAndClaimed { by: String },
}

/// Should a whole-file overwrite of `path` be refused?
///
/// `current` is the fingerprint just taken by the caller. Two refusals, both narrow:
///
/// 1. This session HAS an observation and disk no longer matches it — someone changed the file after
///    we looked. Always a refusal: the overwrite would be computed from content that no longer
///    exists, and the tool has no other line of defense.
/// 2. This session has NO observation, the file already exists, and a live peer session claims it.
///    Blind-overwriting a file another window is actively editing is the one case where "never read
///    it" is not innocent. Without a claiming peer this stays allowed, so single-session flows that
///    legitimately regenerate a file are untouched.
pub fn overwrite_conflict(path: &Path, current: &FileFingerprint) -> Option<Stale> {
    if !current.exists {
        return None;
    }
    match observed(path) {
        Some(seen) if &seen != current => Some(Stale::ChangedSinceRead),
        Some(_) => None,
        None => crate::features::coop::live_peer_claim_at(path)
            .map(|by| Stale::NeverReadAndClaimed { by }),
    }
}

/// The error text a refused overwrite reports. Actionable on purpose: the model's next move is to
/// read the file and redo the edit against what is actually there.
pub fn conflict_message(path: &str, stale: &Stale) -> String {
    match stale {
        Stale::ChangedSinceRead => format!(
            "overwrite conflict: {path} changed on disk after this session read it; nothing was \
             written. Another aizen window (or an external editor) got there first — re-read the \
             file and redo the change against its current content, or use file_edit to change only \
             the part you mean to."
        ),
        Stale::NeverReadAndClaimed { by } => format!(
            "overwrite conflict: {path} is being edited by session {by} and this session has not \
             read it; nothing was written. A whole-file write would discard that window's work — \
             read the file first, then use file_edit so both sets of changes survive."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(bytes: &[u8]) -> FileFingerprint {
        FileFingerprint::for_bytes(bytes)
    }

    fn temp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "aizen-readledger-{tag}-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ));
        std::fs::write(&p, b"seed").unwrap();
        p
    }

    #[test]
    fn an_unread_file_is_not_a_conflict_without_a_claiming_peer() {
        let p = temp("unread");
        assert_eq!(overwrite_conflict(&p, &fp(b"seed")), None);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn a_file_changed_after_this_session_read_it_is_refused() {
        let p = temp("changed");
        note(&p, &fp(b"seed"));
        // Disk now holds something else — exactly the cross-session overwrite this guards.
        assert_eq!(
            overwrite_conflict(&p, &fp(b"someone else's work")),
            Some(Stale::ChangedSinceRead)
        );
        // Re-reading clears it: the session now knows what is there.
        note(&p, &fp(b"someone else's work"));
        assert_eq!(overwrite_conflict(&p, &fp(b"someone else's work")), None);
        forget(&p);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn a_creation_is_never_a_conflict() {
        let missing = std::env::temp_dir().join(format!(
            "aizen-readledger-absent-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ));
        assert_eq!(
            overwrite_conflict(&missing, &FileFingerprint::missing()),
            None,
            "creating a file cannot clobber anything"
        );
    }

    #[test]
    fn observations_are_evicted_oldest_first_without_losing_the_newest() {
        let p = temp("evict");
        note(&p, &fp(b"seed"));
        assert!(observed(&p).is_some());
        forget(&p);
        assert!(observed(&p).is_none(), "forget must drop the observation");
        let _ = std::fs::remove_file(p);
    }
}
