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
    let dir = config::aizen_home().join("hostbot");
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

fn lanes_path() -> PathBuf {
    hostbot_dir().join("lanes.json")
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
    /// This bot answers to its OWN owner instead of inheriting the primary's allowlist.
    ///
    /// The default (`false`) is what a private-chat setup wants: a private chat id EQUALS the owner's
    /// user id, identical across every bot they own, so inheriting is both correct and convenient.
    /// Set `true` to hand a sub-bot to somebody else: it then boots with an empty allowlist in pairing
    /// mode and only the chat that answers its code can drive it. Note what this does NOT change —
    /// the bot still runs shell commands on THIS machine, so a paired stranger holds real power; it
    /// exists so the *primary* owner's chats aren't also exposed on that bot.
    #[serde(default)]
    pub own_owner: bool,
    /// Which host runs this bot, matched against `core::device` (hostname, or the stable device id).
    /// `None` ⇒ any host may run it (the single-machine default, and what every existing file says).
    /// Telegram allows exactly ONE `getUpdates` poller per token, so two machines starting the same
    /// bot is not a redundancy win — it's a 409 fight. Naming the host is how a fleet divides them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

/// Per-lane settings — one entry per hosted-bot route, INCLUDING the primary `"default"`.
///
/// Why this is a separate file from `bots.json`: that file is identity (token, owner, persona) and is
/// rewritten when bots are added or removed; this one is per-conversation *preferences* a chat
/// changes constantly via `/cd`, `/model`, `/effort`. Keeping them apart means a `/cd` can never
/// rewrite a token, and the primary bot — which has no `bots.json` entry at all — still gets lanes.
///
/// Every field is `None` ⇒ "inherit the process-wide config", so an absent file (every install before
/// this feature) behaves exactly as it did when these commands wrote `cli-config.json` globally.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct LaneSettings {
    /// Route this applies to (`"default"` for the primary bot).
    pub route: String,
    /// Working directory for this lane's agent + `/sh`. `None` ⇒ the process cwd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `low`|`medium`|`high`|`xhigh`|`max`, or `"off"` to send no effort field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ultimate: Option<bool>,
}

/// Load every lane's settings. Missing / corrupt file ⇒ `vec![]` (all lanes inherit the global config).
pub fn load_lanes() -> Vec<LaneSettings> {
    match std::fs::read_to_string(lanes_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// One lane's settings, or an all-`None` default when it has never set anything.
pub fn load_lane(route: &str) -> LaneSettings {
    load_lanes()
        .into_iter()
        .find(|l| l.route == route)
        .unwrap_or_else(|| LaneSettings {
            route: route.to_string(),
            ..LaneSettings::default()
        })
}

/// Read-modify-write ONE lane under the store lock, creating its entry if absent. Locked because
/// concurrent lanes each persist their own settings into the same file.
pub fn update_lane<T>(
    route: &str,
    mutate: impl FnOnce(&mut LaneSettings) -> Result<T>,
) -> Result<T> {
    let path = lanes_path();
    let lock_path = crate::core::workspace_txn::store_lock("hostbot", "lanes");
    let _lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &lock_path,
        std::time::Duration::from_secs(5),
    )?;
    let mut lanes: Vec<LaneSettings> = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    if !lanes.iter().any(|l| l.route == route) {
        lanes.push(LaneSettings {
            route: route.to_string(),
            ..LaneSettings::default()
        });
    }
    let entry = lanes
        .iter_mut()
        .find(|l| l.route == route)
        .expect("just inserted");
    let result = mutate(entry)?;
    let json = serde_json::to_string_pretty(&lanes)?;
    crate::core::persist::atomic_write_owner_only(&path, (json + "\n").as_bytes())?;
    Ok(result)
}

/// Forget a lane's settings (a removed bot). Best-effort: a failure leaves a harmless stale entry
/// that the next `update_lane` on that route would simply reuse.
pub fn drop_lane(route: &str) {
    let path = lanes_path();
    let lock_path = crate::core::workspace_txn::store_lock("hostbot", "lanes");
    let Ok(_lock) = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &lock_path,
        std::time::Duration::from_secs(5),
    ) else {
        return;
    };
    let mut lanes: Vec<LaneSettings> = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => return,
    };
    lanes.retain(|l| l.route != route);
    if let Ok(json) = serde_json::to_string_pretty(&lanes) {
        let _ = crate::core::persist::atomic_write_owner_only(&path, (json + "\n").as_bytes());
    }
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
///
/// Every mutation goes through the read-modify-write helpers in this module, which hold the store
/// lock across the whole change; a bare save would let a concurrent edit be overwritten. Kept as the
/// single place that knows the file's permissions and layout.
#[allow(dead_code)]
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

/// Days a session file is kept after its last write before startup GC removes it.
const SESSION_TTL_DAYS: u64 = 30;

/// Remove session files untouched for `SESSION_TTL_DAYS`, returning how many were dropped.
///
/// A long-lived daemon accumulates one file per chat that ever messaged it, forever — each holding
/// conversation text. This bounds both the directory and how long a stranger's message is retained.
/// Best-effort and silent: `mtime` is unavailable on some filesystems, and a daemon must not fail to
/// start over housekeeping. Override the window with `AIZEN_SESSION_TTL_DAYS` (`0` disables GC).
pub fn gc_sessions() -> usize {
    let days = std::env::var("AIZEN_SESSION_TTL_DAYS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(SESSION_TTL_DAYS);
    if days == 0 {
        return 0;
    }
    let max_age = std::time::Duration::from_secs(days * 24 * 60 * 60);
    let Ok(entries) = std::fs::read_dir(sessions_dir()) else {
        return 0;
    };
    let mut dropped = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(age) = entry
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|t| t.elapsed().map_err(std::io::Error::other))
        else {
            continue; // unknown mtime (or a clock skew making it "in the future") ⇒ keep it
        };
        if age > max_age && std::fs::remove_file(&path).is_ok() {
            dropped += 1;
        }
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin `AIZEN_HOME` to a fresh tempdir so the store reads/writes there, not the real `~/.aizen`.
    /// Returns the guard dir (kept alive for the test's lifetime). Serialized via the SHARED
    /// `TEST_HOME_LOCK` — the whole crate uses it to keep `HOME`-mutating tests from racing. We set
    /// `AIZEN_HOME` (not `AIZEN_HOME`) to match every other test: `aizen_home()` gives `AIZEN_HOME`
    /// HIGHER precedence, so a leaked `AIZEN_HOME` would override the `AIZEN_HOME` those tests set and
    /// break them; `AIZEN_HOME` is the shared convention each test overwrites at its own start.
    fn with_temp_home() -> (std::sync::MutexGuard<'static, ()>, tempdir_guard::TempDir) {
        let guard = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempdir_guard::TempDir::new();
        std::env::set_var("AIZEN_HOME", dir.path());
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
                own_owner: false,
                host: None,
            },
            HostedBot {
                name: "ops".into(),
                token: Some("t2".into()),
                allowed_chat_ids: vec![7, 9],
                persona: Some("Aria".into()),
                own_owner: true,
                host: Some("vps-2".into()),
            },
        ];
        save_bots(&bots).unwrap();
        let round = load_bots();
        assert_eq!(round.len(), 2);
        assert_eq!(round[0].name, "work");
        assert_eq!(round[0].persona, None);
        assert!(!round[0].own_owner);
        assert_eq!(round[0].host, None);
        assert_eq!(round[1].allowed_chat_ids, vec![7, 9]);
        assert_eq!(round[1].persona.as_deref(), Some("Aria"));
        assert!(round[1].own_owner, "own_owner survives a round trip");
        assert_eq!(round[1].host.as_deref(), Some("vps-2"));
    }

    #[test]
    fn a_bots_file_written_before_these_fields_existed_still_loads() {
        // Back-compat is the whole reason both new fields are `#[serde(default)]`: an install from
        // before this change must keep hosting its bots, with the old inherit-the-owner behaviour.
        let (_g, _home) = with_temp_home();
        let legacy = r#"[{"name":"work","token":"t1","allowed_chat_ids":[5]}]"#;
        std::fs::write(bots_path(), legacy).unwrap();
        let round = load_bots();
        assert_eq!(round.len(), 1, "legacy file parses");
        assert_eq!(round[0].name, "work");
        assert!(
            !round[0].own_owner,
            "absent own_owner means inherit, the pre-existing behaviour"
        );
        assert_eq!(
            round[0].host, None,
            "absent host means any machine may run it"
        );
    }

    #[test]
    fn a_lane_with_no_entry_inherits_everything() {
        // No lanes.json (every install before this feature) ⇒ all-None ⇒ every command falls back to
        // the process-wide config exactly as it did when these wrote `cli-config.json`.
        let (_g, _home) = with_temp_home();
        let lane = load_lane("default");
        assert_eq!(lane.route, "default");
        assert_eq!(lane.cwd, None);
        assert_eq!(lane.model, None);
        assert_eq!(lane.effort, None);
        assert_eq!(lane.approval, None);
        assert_eq!(lane.ultimate, None);
    }

    #[test]
    fn lane_settings_are_per_route_and_do_not_bleed() {
        // The bug this prevents: bot A's `/cd` moving bot B's working directory, which is what a
        // process-wide `set_current_dir` did.
        let (_g, _home) = with_temp_home();
        update_lane("default", |l| {
            l.cwd = Some(PathBuf::from("/srv/projA"));
            l.model = Some("model-a".into());
            Ok(())
        })
        .unwrap();
        update_lane("work", |l| {
            l.cwd = Some(PathBuf::from("/srv/projB"));
            Ok(())
        })
        .unwrap();

        assert_eq!(load_lane("default").cwd, Some(PathBuf::from("/srv/projA")));
        assert_eq!(load_lane("work").cwd, Some(PathBuf::from("/srv/projB")));
        assert_eq!(
            load_lane("work").model,
            None,
            "a model pinned on `default` must NOT apply to `work`"
        );
        assert_eq!(load_lanes().len(), 2);
    }

    #[test]
    fn update_lane_edits_in_place_without_dropping_siblings() {
        let (_g, _home) = with_temp_home();
        update_lane("a", |l| {
            l.model = Some("m1".into());
            Ok(())
        })
        .unwrap();
        update_lane("b", |l| {
            l.model = Some("m2".into());
            Ok(())
        })
        .unwrap();
        update_lane("a", |l| {
            l.effort = Some("max".into());
            Ok(())
        })
        .unwrap();

        let a = load_lane("a");
        assert_eq!(a.model.as_deref(), Some("m1"), "existing field preserved");
        assert_eq!(a.effort.as_deref(), Some("max"), "new field added");
        assert_eq!(
            load_lane("b").model.as_deref(),
            Some("m2"),
            "sibling intact"
        );
    }

    #[test]
    fn drop_lane_removes_only_that_route() {
        let (_g, _home) = with_temp_home();
        update_lane("keep", |l| {
            l.model = Some("m".into());
            Ok(())
        })
        .unwrap();
        update_lane("gone", |l| {
            l.model = Some("m".into());
            Ok(())
        })
        .unwrap();
        drop_lane("gone");
        let lanes = load_lanes();
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].route, "keep");
    }

    #[test]
    fn gc_keeps_fresh_sessions_and_can_be_disabled() {
        // Files just written are far inside any retention window, so GC must not touch them — an
        // over-eager sweep here silently erases live conversations.
        let (_g, _home) = with_temp_home();
        let msgs = vec![Message::user("hi")];
        save_session("telegram", "default", "1", &msgs).unwrap();
        save_session("telegram", "work", "2", &msgs).unwrap();
        assert_eq!(gc_sessions(), 0, "fresh files survive");
        assert_eq!(load_sessions("telegram").len(), 2);

        // `0` disables GC entirely, for an operator who wants unbounded history.
        std::env::set_var("AIZEN_SESSION_TTL_DAYS", "0");
        assert_eq!(gc_sessions(), 0);
        std::env::remove_var("AIZEN_SESSION_TTL_DAYS");
        assert_eq!(
            load_sessions("telegram").len(),
            2,
            "nothing was dropped either way"
        );
    }

    #[test]
    fn gc_drops_sessions_past_the_window() {
        // A daemon running for months otherwise keeps one file per chat that ever messaged it,
        // forever — each holding conversation text.
        let (_g, _home) = with_temp_home();
        save_session("telegram", "old", "9", &[Message::user("ancient")]).unwrap();
        assert_eq!(load_sessions("telegram").len(), 1);
        // A one-day window plus a backdated mtime is the portable way to test this without waiting.
        std::env::set_var("AIZEN_SESSION_TTL_DAYS", "1");
        let path = session_path("telegram", "old", "9");
        let two_days_ago =
            std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 24 * 60 * 60);
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(two_days_ago).unwrap();
        drop(f);

        assert_eq!(gc_sessions(), 1, "the stale file was dropped");
        assert!(load_sessions("telegram").is_empty());
        std::env::remove_var("AIZEN_SESSION_TTL_DAYS");
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
