//! Session working memory (L2): temporary, in-process notes for the current session.
//!
//! Inferred / mid-confidence facts land here first so they do **not** pollute the durable
//! long-tail store or the always-on frozen core. Explicit remember / `#…` / manual add still
//! write durable entries. End of session → drop (no auto-promote by default).
//!
//! Inject is optional and budget-capped (`session_mem_max_tokens`); empty → no prompt tag.

use crate::memory::render::{est_tokens, sanitize_body};
use std::time::{SystemTime, UNIX_EPOCH};

/// Kind of session note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionNoteKind {
    /// Working scratch (tool/context notes).
    Working,
    /// Candidate fact inferred this session (may be promoted later by explicit path).
    Candidate,
}

impl SessionNoteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionNoteKind::Working => "working",
            SessionNoteKind::Candidate => "candidate",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionNote {
    pub id: String,
    pub body: String,
    pub kind: SessionNoteKind,
    /// `None` = global intent; `Some(slug)` = about current project zone.
    pub scope: Option<String>,
    /// 1–10 salience for inject ranking.
    pub importance: u8,
    pub created_ms: u128,
}

/// In-process session store. Not persisted across process exit.
#[derive(Debug, Default, Clone)]
pub struct SessionMem {
    notes: Vec<SessionNote>,
}

/// Hard cap on notes held (evict lowest importance / oldest).
const NOTE_CAP: usize = 32;
/// Min importance to appear in the prompt block.
const INJECT_MIN: u8 = 5;
/// Max lines in the prompt block.
const INJECT_MAX_LINES: usize = 5;

impl SessionMem {
    pub fn new() -> Self {
        Self { notes: Vec::new() }
    }

    pub fn list(&self) -> &[SessionNote] {
        &self.notes
    }

    pub fn is_empty(&self) -> bool {
        self.notes.is_empty()
    }

    pub fn clear(&mut self) {
        self.notes.clear();
    }

    /// Add or near-dup-reinforce a note. Near-dup (case-insensitive body) bumps importance.
    pub fn note(
        &mut self,
        body: &str,
        kind: SessionNoteKind,
        scope: Option<String>,
        importance: u8,
    ) -> String {
        let body = body.trim();
        if body.is_empty() {
            return String::new();
        }
        let importance = importance.clamp(1, 10);
        if let Some(existing) = self
            .notes
            .iter_mut()
            .find(|n| n.body.eq_ignore_ascii_case(body) && n.scope == scope)
        {
            existing.importance = existing.importance.max(importance);
            // Prefer Candidate over Working if re-seen as a fact.
            if kind == SessionNoteKind::Candidate {
                existing.kind = SessionNoteKind::Candidate;
            }
            return existing.id.clone();
        }
        let id = slug_id(body);
        let created_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        self.notes.push(SessionNote {
            id: id.clone(),
            body: body.to_string(),
            kind,
            scope,
            importance,
            created_ms,
        });
        self.evict_if_needed();
        id
    }

    fn evict_if_needed(&mut self) {
        if self.notes.len() <= NOTE_CAP {
            return;
        }
        // Drop lowest importance, then oldest.
        self.notes.sort_by(|a, b| {
            b.importance
                .cmp(&a.importance)
                .then(b.created_ms.cmp(&a.created_ms))
        });
        self.notes.truncate(NOTE_CAP);
    }

    /// Notes that could be promoted to durable (high-importance candidates). Default policy
    /// does **not** auto-promote — callers only use this for inspection / future CLI.
    pub fn candidates_for_promote(&self) -> Vec<&SessionNote> {
        self.notes
            .iter()
            .filter(|n| n.kind == SessionNoteKind::Candidate && n.importance >= 7)
            .collect()
    }

    /// Render a short `<session_memory>` block, or `None` when empty / budget 0 / nothing qualifies.
    pub fn prompt_block(&self, max_tokens: usize) -> Option<String> {
        if max_tokens == 0 || self.notes.is_empty() {
            return None;
        }
        let mut ranked: Vec<&SessionNote> = self
            .notes
            .iter()
            .filter(|n| n.importance >= INJECT_MIN)
            .collect();
        if ranked.is_empty() {
            return None;
        }
        ranked.sort_by(|a, b| {
            b.importance
                .cmp(&a.importance)
                .then(b.created_ms.cmp(&a.created_ms))
        });

        let header = "<session_memory>\n";
        let footer = "</session_memory>";
        let mut budget = max_tokens.saturating_sub(est_tokens(header) + est_tokens(footer));
        let mut parts: Vec<String> = Vec::new();
        for n in ranked.into_iter().take(INJECT_MAX_LINES) {
            let zone = match n.scope.as_deref() {
                Some(z) => format!(" [p:{z}]"),
                None => String::new(),
            };
            let body = sanitize_body(&n.body);
            let body: String = body.chars().take(160).collect();
            let line = format!("- ({}){} {}", n.kind.as_str(), zone, body.trim());
            let cost = est_tokens(&line) + 1;
            if cost > budget {
                break;
            }
            budget -= cost;
            parts.push(line);
        }
        if parts.is_empty() {
            return None;
        }
        Some(format!("{header}{}\n{footer}", parts.join("\n")))
    }
}

fn slug_id(body: &str) -> String {
    let mut s = String::new();
    let mut last_dash = true;
    for c in body.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            s.push('-');
            last_dash = true;
        }
        if s.len() >= 40 {
            break;
        }
    }
    let s = s.trim_matches('-');
    if s.is_empty() {
        format!("n-{}", simple_hash(body))
    } else {
        format!("{s}-{:04x}", simple_hash(body) as u16)
    }
}

fn simple_hash(s: &str) -> u32 {
    let mut h: u32 = 2166136261;
    for b in s.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(16777619);
    }
    h
}

/// Process-wide REPL session memory (one per CLI process).
pub fn process_session_mem() -> std::sync::MutexGuard<'static, SessionMem> {
    use std::sync::{Mutex, OnceLock};
    static MEM: OnceLock<Mutex<SessionMem>> = OnceLock::new();
    MEM.get_or_init(|| Mutex::new(SessionMem::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Clear the process session memory (call on `/new`, rebuild_system, session end).
pub fn clear_process_session_mem() {
    process_session_mem().clear();
}

/// Snapshot the inject block under the configured budget (best-effort).
pub fn process_prompt_block(max_tokens: usize) -> Option<String> {
    process_session_mem().prompt_block(max_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_and_near_dup_bumps_importance() {
        let mut m = SessionMem::new();
        let id1 = m.note("prefers pnpm", SessionNoteKind::Candidate, None, 4);
        let id2 = m.note("prefers pnpm", SessionNoteKind::Candidate, None, 7);
        assert_eq!(id1, id2);
        assert_eq!(m.list().len(), 1);
        assert_eq!(m.list()[0].importance, 7);
    }

    #[test]
    fn prompt_block_empty_when_low_importance() {
        let mut m = SessionMem::new();
        m.note("scratch", SessionNoteKind::Working, None, 3);
        assert!(m.prompt_block(300).is_none());
    }

    #[test]
    fn prompt_block_emits_under_budget() {
        let mut m = SessionMem::new();
        m.note(
            "deploy uses fly.io",
            SessionNoteKind::Candidate,
            Some("aizen-abc".into()),
            8,
        );
        let block = m.prompt_block(300).expect("block");
        assert!(block.contains("<session_memory>"));
        assert!(block.contains("deploy uses fly"));
        assert!(block.contains("[p:aizen-abc]"));
        assert!(est_tokens(&block) <= 300);
    }

    #[test]
    fn clear_drops_all() {
        let mut m = SessionMem::new();
        m.note("x", SessionNoteKind::Working, None, 9);
        m.clear();
        assert!(m.is_empty());
        assert!(m.prompt_block(300).is_none());
    }

    #[test]
    fn cap_evicts_low_importance() {
        let mut m = SessionMem::new();
        for i in 0..(NOTE_CAP + 5) {
            m.note(
                &format!("note number {i} unique body here"),
                SessionNoteKind::Working,
                None,
                if i < 5 { 1 } else { 8 },
            );
        }
        assert!(m.list().len() <= NOTE_CAP);
        // high-importance survivors present
        assert!(m.list().iter().any(|n| n.importance >= 8));
    }

    #[test]
    fn candidates_for_promote_filters() {
        let mut m = SessionMem::new();
        m.note("low", SessionNoteKind::Candidate, None, 5);
        m.note("high", SessionNoteKind::Candidate, None, 8);
        m.note("work", SessionNoteKind::Working, None, 9);
        let c = m.candidates_for_promote();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].body, "high");
    }
}
