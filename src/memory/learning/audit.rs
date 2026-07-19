//! Append-only audit of automatic learning writes (E0). Best-effort JSONL next to the store so
//! mistaken auto-supersedes can be inspected and manually reverted via `aizen memory supersede` /
//! `restore` — not a full undo engine yet.

use crate::core::config;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;

#[derive(Serialize)]
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
}

fn audit_path() -> std::path::PathBuf {
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