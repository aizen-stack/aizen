//! Append-only audit of automatic learning writes (E0). Best-effort JSONL next to the store so
//! mistaken auto-supersedes can be inspected and manually reverted via `aizen memory supersede` /
//! `restore` — not a full undo engine yet.

use crate::core::config;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;

#[derive(Serialize, Default)]
pub struct AuditEvent<'a> {
    pub ts: String,
    pub session_id: &'a str,
    pub op: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_preview: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<&'a str>,
    /// The batch-reconciliation verdict that caused this write (`same`/`refine`/`contradict`/
    /// `unsure`). Present only on `reconcile`-driven ops — it is the one field that distinguishes a
    /// supersede a human asked for from one a model decided on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<&'a str>,
    /// The model's confidence in that verdict, so a later audit read can tell a 0.66 call (barely
    /// over the apply bar) from a 0.95 one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// `recall` only: how many facts the turn was shown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub injected: Option<u64>,
    /// `recall` only: how many of those the turn reported as load-bearing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used: Option<u64>,
}

/// One gated turn's recall outcome: `injected` facts shown, `used` of them load-bearing.
///
/// Logged per event rather than only totalled in `stats.jsonl` because the ratio alone cannot say
/// WHICH turns wasted their budget — a week at 0.20 is a number, but the lines behind it name the
/// queries whose recall missed, which is what actually calibrates the relevance gate and `k`.
/// Recorded on gated turns only, so the numerator and denominator always cover the same turns: a
/// turn whose secretary never ran has no `used` report, and counting its injections would deflate
/// the ratio with turns that were never asked the question.
pub fn recall(session_id: &str, injected: u64, used: u64) {
    append(AuditEvent {
        ts: ts_now(),
        session_id,
        op: "recall",
        injected: Some(injected),
        used: Some(used.min(injected)),
        ..Default::default()
    })
}

/// One reconciliation decision — logged for EVERY pair the batch pass judged, including the ones it
/// declined to act on. A log of only the applied writes cannot answer the question the audit exists
/// for ("why did my fact disappear / why did nothing happen"), and §8's third metric counts
/// contradictions *found* per week, not contradictions applied.
pub fn reconcile(session_id: &str, verdict: &str, confidence: f64, old_id: &str, outcome: &str) {
    append(AuditEvent {
        ts: ts_now(),
        session_id,
        op: "reconcile",
        old_id: Some(old_id),
        verdict: Some(verdict),
        confidence: Some(confidence),
        signal: Some(outcome),
        ..Default::default()
    })
}

/// A fact came back from the graveyard (`aizen memory revive`). The inverse of `supersede`, and the
/// reason an automatic `contradict` branch is allowed to exist — so it has to be as visible.
pub fn revive(session_id: &str, id: &str) {
    append(AuditEvent {
        ts: ts_now(),
        session_id,
        op: "revive",
        id: Some(id),
        ..Default::default()
    })
}

/// Public so the §8 health report reads the log from the one place that names it. A second literal
/// `"learning-audit.jsonl"` elsewhere would keep working until this one changed, and then the report
/// would silently count zero contradictions forever.
pub fn audit_path() -> std::path::PathBuf {
    config::cli_memory_dir().join("learning-audit.jsonl")
}

/// One NDJSON line per event; never fails the learn pipeline.
pub fn append(ev: AuditEvent<'_>) {
    let line = match serde_json::to_string(&ev) {
        Ok(s) => s,
        Err(_) => return,
    };
    let path = audit_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let _ = writeln!(f, "{line}");
}

pub fn ts_now() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}
