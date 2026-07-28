//! `~/.aizen/hostbot/` — the host-bot feature's own data directory, separate from `cli-config.json`.
//! Holds two things:
//!   * `bots.json` — the extra bots hosted by the Telegram daemon (`/addbot` writes here). The PRIMARY
//!     bot's identity stays in `cli-config.json` (it's the daemon's own auth, not a "hosted" bot).
//!   * `sessions/` — one self-describing JSON file per (platform, route, chat) conversation, so a daemon
//!     restart (the whole point of `Restart=always` self-host) keeps its context instead of losing it.
//!
//! Everything here is secret-bearing (bot tokens, conversation history) → the dir is 0700 and every
//! file 0600 on Unix (no-op on Windows, where the profile ACL governs), matching the token-cache
//! discipline used elsewhere.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::core::config;
use crate::core::types::Message;

/// `~/.aizen/hostbot/` — created + hardened to 0700 on first touch. Best-effort hardening (never fails
/// the caller); creation errors surface where the path is used to write.
pub fn hostbot_dir() -> PathBuf {
    let dir = config::nextgen_home().join("hostbot");
    let _ = std::fs::create_dir_all(&dir);
    config::harden_dir(&dir);
    dir
}

fn sessions_dir() -> PathBuf {
    let dir = hostbot_dir().join("sessions");
    let _ = std::fs::create_dir_all(&dir);
    config::harden_dir(&dir);
    dir
}

fn bots_path() -> PathBuf {
    hostbot_dir().join("bots.json")
}

// ── hosted bots (bots.json) ──────────────────────────────────────────────────────────

/// One EXTRA bot hosted by the daemon (moved here from `cli_config::TelegramBot`). Each has its own
/// @BotFather token but shares the owner's `allowed_chat_ids` (a private chat id == the owner's user
/// id, identical across all their bots) unless it carries its own override. Managed live via
/// `/addbot` / `/rmbot`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct HostedBot {
    /// Short unique label used by `/bots` / `/rmbot` (never the reserved name "default").
    pub name: String,
    /// This bot's @BotFather token (stored plaintext; the file is 0600 on Unix).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Per-bot allowlist override. Empty ⇒ inherit the primary bot's `allowed_chat_ids`.
    #[serde(default)]
    pub allowed_chat_ids: Vec<i64>,
    /// Persona this sub-bot wears (name of a persona card). `None` ⇒ the daemon's global persona (the
    /// "default" bot's voice). Only the `<persona>`/`<self>` blocks change per bot — `<user_memory>`
    /// (the frozen core) stays global, so the owner's memory is shared, "driven by default".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
}

/// Load the hosted-bot list. A missing / unreadable / corrupt file → `vec![]` (never fails — a fresh
/// install simply has no extra bots).
pub fn load_bots() -> Vec<HostedBot> {
    match std::fs::read_to_string(bots_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

pub fn update_bots<T>(mutate: impl FnOnce(&mut Vec<HostedBot>) -> Result<T>) -> Result<T> {
    let path = bots_path();
    let lock_path = crate::core::workspace_txn::store_lock("hostbot", "bots");
    let _lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &lock_path,
        std::time::Duration::from_secs(5),
    )?;
    let mut bots = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let result = mutate(&mut bots)?;
    let json = serde_json::to_string_pretty(&bots)?;
    crate::core::persist::atomic_write_owner_only(&path, (json + "\n").as_bytes())?;
    Ok(result)
}

/// Persist the hosted-bot list to `hostbot/bots.json`, hardened to owner-only.
pub fn save_bots(bots: &[HostedBot]) -> Result<()> {
    let path = bots_path();
    let lock_path = crate::core::workspace_txn::store_lock("hostbot", "bots");
    let _lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &lock_path,
        std::time::Duration::from_secs(5),
    )?;
    let json = serde_json::to_string_pretty(bots)?;
    crate::core::persist::atomic_write_owner_only(&path, (json + "\n").as_bytes())?;
    Ok(())
}

// ── persisted sessions (sessions/*.json) ───────────────────────────────────────────────

/// A conversation session on disk. Self-describing (carries its own keys) so we never parse identity
/// out of the file NAME — the filename is just a sanitized, collision-free handle.
#[derive(Debug, Serialize, Deserialize)]
struct SessionFile {
    platform: String,
    route: String,
    chat: String,
    messages: Vec<Message>,
}

/// Sanitize a component for use in a filename: keep `[a-z0-9_-]`, map everything else (incl. the `-`
/// of a negative chat id → kept; `.` path separators → `_`) so no value can escape the sessions dir.
/// Platform slugs + bot names are already `[a-z0-9_]`; a chat id is `-?\d+`; this is defense in depth.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn session_path(platform: &str, route: &str, chat: &str) -> PathBuf {
    let name = format!(
        "{}.{}.{}.json",
        sanitize(platform),
        sanitize(route),
        sanitize(chat)
    );
    sessions_dir().join(name)
}

/// Load every persisted session for `platform` → `(route, chat, messages)` triples. The daemon keys
/// its in-memory map by `(route, Chat)` after parsing `chat` via `Chat::from_str`. Corrupt / foreign
/// files are skipped silently (a partial write or another platform's file must not crash startup).
pub fn load_sessions(platform: &str) -> Vec<(String, String, Vec<Message>)> {
    let dir = sessions_dir();
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(s) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(sf) = serde_json::from_str::<SessionFile>(&s) else {
            continue;
        };
        if sf.platform != platform {
            continue; // another platform's session dir-mate
        }
        out.push((sf.route, sf.chat, sf.messages));
    }
    out
}

/// Persist one session after a turn. `chat` is the `Display` form of the platform's chat id.
pub fn save_session(platform: &str, route: &str, chat: &str, messages: &[Message]) -> Result<()> {
    let path = session_path(platform, route, chat);
    let lock_key = format!("{platform}:{route}:{chat}");
    let lock_path = crate::core::workspace_txn::store_lock("hostbot_session", &lock_key);
    let _lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &lock_path,
        std::time::Duration::from_secs(5),
    )?;
    let sf = SessionFile {
        platform: platform.to_string(),
        route: route.to_string(),
        chat: chat.to_string(),
        messages: messages.to_vec(),
    };
    let json = serde_json::to_string(&sf)?;
    crate::core::persist::atomic_write_owner_only(&path, json.as_bytes())?;
    Ok(())
}

/// Delete one session's file (used by `/new`, `/model`, and when a bot is removed). Best-effort.
pub fn drop_session(platform: &str, route: &str, chat: &str) {
    let _ = std::fs::remove_file(session_path(platform, route, chat));
}

/// Drop every session belonging to a given route (sub-bot) — used by `/rmbot`. Best-effort.
pub fn drop_route_sessions(platform: &str, route: &str) {
    let prefix = format!("{}.{}.", sanitize(platform), sanitize(route));
    let dir = sessions_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            if name.starts_with(&prefix) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin `NEXTGEN_HOME` to a fresh tempdir so the store reads/writes there, not the real `~/.aizen`.
    /// Returns the guard dir (kept alive for the test's lifetime). Serialized via the SHARED
    /// `TEST_HOME_LOCK` — the whole crate uses it to keep `HOME`-mutating tests from racing. We set
    /// `NEXTGEN_HOME` (not `AIZEN_HOME`) to match every other test: `nextgen_home()` gives `AIZEN_HOME`
    /// HIGHER precedence, so a leaked `AIZEN_HOME` would override the `NEXTGEN_HOME` those tests set and
    /// break them; `NEXTGEN_HOME` is the shared convention each test overwrites at its own start.
    fn with_temp_home() -> (std::sync::MutexGuard<'static, ()>, tempdir_guard::TempDir) {
        let guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempdir_guard::TempDir::new();
        std::env::set_var("NEXTGEN_HOME", dir.path());
        (guard, dir)
    }

    #[test]
    fn hosted_bots_round_trip() {
        let (_g, _home) = with_temp_home();
        assert!(load_bots().is_empty(), "fresh store has no bots");
        let bots = vec![
            HostedBot {
                name: "work".into(),
                token: Some("t1".into()),
                allowed_chat_ids: vec![],
                persona: None,
            },
            HostedBot {
                name: "ops".into(),
                token: Some("t2".into()),
                allowed_chat_ids: vec![7, 9],
                persona: Some("Aria".into()),
            },
        ];
        save_bots(&bots).unwrap();
        let round = load_bots();
        assert_eq!(round.len(), 2);
        assert_eq!(round[0].name, "work");
        assert_eq!(round[0].persona, None);
        assert_eq!(round[1].allowed_chat_ids, vec![7, 9]);
        assert_eq!(round[1].persona.as_deref(), Some("Aria"));
    }

    #[test]
    fn session_save_load_drop() {
        let (_g, _home) = with_temp_home();
        let msgs = vec![Message::user("hello"), Message::assistant("hi")];
        save_session("telegram", "default", "42", &msgs).unwrap();
        save_session("telegram", "work", "-100", &msgs).unwrap();
        save_session("discord", "default", "999", &msgs).unwrap();

        let tg = load_sessions("telegram");
        assert_eq!(tg.len(), 2, "only telegram's two sessions, not discord's");
        assert!(tg.iter().any(|(r, c, _)| r == "default" && c == "42"));
        assert!(tg.iter().any(|(r, c, _)| r == "work" && c == "-100"));

        drop_session("telegram", "default", "42");
        let tg = load_sessions("telegram");
        assert_eq!(tg.len(), 1, "dropped one → one telegram session left");
        assert_eq!(tg[0].0, "work");

        drop_route_sessions("telegram", "work");
        assert!(
            load_sessions("telegram").is_empty(),
            "route drop removed the last telegram session"
        );
        assert_eq!(
            load_sessions("discord").len(),
            1,
            "discord session untouched"
        );
    }

    #[test]
    fn sanitize_blocks_path_escape() {
        assert_eq!(sanitize("../etc"), "___etc");
        assert_eq!(sanitize("a.b/c"), "a_b_c");
        assert_eq!(sanitize("-100"), "-100"); // a negative chat id survives intact
        assert_eq!(sanitize("work_bot"), "work_bot");
    }
}

/// A tiny self-contained tempdir for tests (no `tempfile` dep — keeps the pure-Rust posture). Creates
/// a uniquely-named dir under the OS temp dir and removes it on drop.
#[cfg(test)]
mod tempdir_guard {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!(
                "aizen-hostbot-test-{}-{}",
                std::process::id(),
                n
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).expect("create tempdir");
            TempDir(p)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
