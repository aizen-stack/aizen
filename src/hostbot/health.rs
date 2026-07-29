//! Liveness for `aizen serve` — a heartbeat file the daemon touches and `aizen serve --health`
//! reads.
//!
//! WHY a file and not an HTTP port: the daemon deliberately listens on NOTHING. Telegram is
//! long-poll (`getUpdates`), Discord is an outbound gateway websocket — that's what lets it run
//! behind NAT with no public URL. Opening a port just so an orchestrator can `GET /healthz` would
//! throw that away (and hand an unauthenticated surface to anything sharing the network). A file in
//! `~/.aizen/hostbot/` is readable by an exec probe in the same container/VPS and by nobody else.
//!
//! WHY a state machine and not just a timestamp: an agent turn legitimately takes minutes (a build,
//! a test suite, a long tool chain). A plain "last touched" clock can't tell "wedged" from "working
//! hard", so a probe tuned to catch the first would kill the second. So the record carries a STATE
//! (`idle` while waiting for a message, `busy` while running a turn) plus the instant that state
//! began, and each state gets its own deadline: idle must refresh every tick, busy is given a long
//! leash. See [`Heartbeat::verdict`].
//!
//! The `idle` beat is emitted from INSIDE the daemon's `select!`, not from a detached ticker — a
//! ticker of its own would keep beating while the real loop was wedged, which is precisely the
//! failure a liveness probe exists to catch.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// How often the daemon re-stamps an `idle` heartbeat (seconds). The probe's default idle deadline
/// is a multiple of this, so a few missed ticks (a slow disk, a busy host) don't trip it.
pub const BEAT_INTERVAL_SECS: u64 = 15;

/// Default: an `idle` daemon that hasn't beaten in this long is considered wedged. Six intervals.
const DEFAULT_MAX_IDLE_SECS: u64 = 90;

/// Default: a `busy` daemon (running one agent turn) is given this long before it's called wedged.
/// Generous on purpose — a turn that builds and tests a large project is not a fault.
const DEFAULT_MAX_BUSY_SECS: u64 = 1800;

/// What the daemon is doing. Stored as a string so an unknown future state read by an older binary
/// degrades to "can't judge" rather than a parse failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Booting: listeners not up yet. Judged with the idle deadline.
    Starting,
    /// Waiting on the inbound channel — the beat must keep advancing.
    Idle,
    /// Running one agent turn. Long is normal; see [`DEFAULT_MAX_BUSY_SECS`].
    Busy,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::Idle => "idle",
            State::Busy => "busy",
        }
    }
}

/// The on-disk record. `ts` is when it was written; `since` is when the current STATE began — for a
/// long turn these diverge, and that gap is exactly what makes a busy daemon distinguishable from a
/// stalled one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    /// Unix seconds when this record was written.
    pub ts: u64,
    /// Unix seconds when the daemon entered `state`.
    #[serde(default)]
    pub since: u64,
    /// `starting` | `idle` | `busy` (see [`State`]).
    pub state: String,
    /// The daemon's pid — lets a probe tell "restarted" from "never stopped".
    pub pid: u32,
    /// Which chat platform this daemon hosts (`telegram` / `discord`).
    #[serde(default)]
    pub platform: String,
}

/// `~/.aizen/hostbot/heartbeat.json`. Same 0700 dir as the bot tokens and sessions.
pub fn heartbeat_path() -> PathBuf {
    super::store::hostbot_dir().join("heartbeat.json")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Env-overridable deadline for an `idle` daemon (`AIZEN_HEALTH_MAX_IDLE_SECS`).
pub fn max_idle_secs() -> u64 {
    env_secs("AIZEN_HEALTH_MAX_IDLE_SECS").unwrap_or(DEFAULT_MAX_IDLE_SECS)
}

/// Env-overridable deadline for a `busy` daemon (`AIZEN_HEALTH_MAX_BUSY_SECS`). Raise it if your
/// turns routinely run long (big test suites); lower it only if you'd rather restart than wait.
pub fn max_busy_secs() -> u64 {
    env_secs("AIZEN_HEALTH_MAX_BUSY_SECS").unwrap_or(DEFAULT_MAX_BUSY_SECS)
}

fn env_secs(var: &str) -> Option<u64> {
    std::env::var(var)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
}

/// Stamp the current state. Best-effort by design: a daemon must never die because it couldn't
/// write a health file. `since` is carried forward while the state is unchanged, so a `busy` record
/// keeps pointing at when the turn actually started.
pub fn beat(platform: &str, state: State) {
    let now = now_secs();
    let since = match read() {
        Some(prev) if prev.state == state.as_str() && prev.pid == std::process::id() => {
            if prev.since == 0 {
                now
            } else {
                prev.since
            }
        }
        _ => now,
    };
    let hb = Heartbeat {
        ts: now,
        since,
        state: state.as_str().to_string(),
        pid: std::process::id(),
        platform: platform.to_string(),
    };
    if let Ok(bytes) = serde_json::to_vec_pretty(&hb) {
        let _ = crate::core::persist::atomic_write_owner_only(&heartbeat_path(), &bytes);
    }
}

/// Read the record, or `None` when absent/corrupt (no daemon has run, or it's mid-write).
pub fn read() -> Option<Heartbeat> {
    let s = std::fs::read_to_string(heartbeat_path()).ok()?;
    serde_json::from_str(&s).ok()
}

/// Remove the record on a clean exit, so a stopped daemon reads as "not running" rather than as a
/// stale live one. Best-effort.
pub fn clear() {
    let _ = std::fs::remove_file(heartbeat_path());
}

impl Heartbeat {
    /// Judge this record against `now`. `Ok(msg)` = healthy, `Err(msg)` = the probe should fail.
    /// Pure so the whole decision table is testable without a daemon or a clock.
    pub fn verdict(&self, now: u64, max_idle: u64, max_busy: u64) -> Result<String, String> {
        // A clock that jumped backwards (NTP step, a container with a skewed clock) must not read as
        // "fresh forever" — but it also isn't evidence of a wedge, so treat age as 0 and pass.
        let age = now.saturating_sub(self.ts);
        match self.state.as_str() {
            "busy" => {
                let running = now.saturating_sub(if self.since == 0 { self.ts } else { self.since });
                if running > max_busy {
                    Err(format!(
                        "busy on one turn for {running}s (limit {max_busy}s) — pid {} looks wedged",
                        self.pid
                    ))
                } else {
                    Ok(format!("busy {running}s (pid {})", self.pid))
                }
            }
            "idle" | "starting" => {
                if age > max_idle {
                    Err(format!(
                        "last {} beat was {age}s ago (limit {max_idle}s) — pid {} is not looping",
                        self.state, self.pid
                    ))
                } else {
                    Ok(format!("{} {age}s ago (pid {})", self.state, self.pid))
                }
            }
            // An unknown state from a NEWER binary: we can't judge it, and guessing "dead" would
            // restart a healthy daemon during a rolling upgrade. Fall back to plain freshness.
            other => {
                if age > max_busy {
                    Err(format!("unknown state '{other}', last beat {age}s ago"))
                } else {
                    Ok(format!("state '{other}', {age}s ago (pid {})", self.pid))
                }
            }
        }
    }
}

/// `aizen serve --health`: print one line and return whether the daemon is healthy. The caller turns
/// that into the process exit status (0 / 1) an `exec` probe reads. No daemon ⇒ `false`, because
/// "nothing is running" is exactly what a liveness probe must catch.
pub fn run_health_check() -> bool {
    let (line, ok) = match read() {
        None => (
            format!(
                "aizen serve: no heartbeat at {} — daemon not running",
                heartbeat_path().display()
            ),
            false,
        ),
        Some(hb) => {
            let platform = if hb.platform.is_empty() {
                "unknown".to_string()
            } else {
                hb.platform.clone()
            };
            match hb.verdict(now_secs(), max_idle_secs(), max_busy_secs()) {
                Ok(msg) => (format!("aizen serve [{platform}] healthy — {msg}"), true),
                Err(msg) => (format!("aizen serve [{platform}] UNHEALTHY — {msg}"), false),
            }
        }
    };
    if ok {
        println!("{line}");
    } else {
        eprintln!("{line}");
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hb(state: &str, ts: u64, since: u64) -> Heartbeat {
        Heartbeat {
            ts,
            since,
            state: state.to_string(),
            pid: 4242,
            platform: "telegram".into(),
        }
    }

    #[test]
    fn a_fresh_idle_beat_is_healthy_and_a_stale_one_is_not() {
        let h = hb("idle", 1000, 1000);
        assert!(h.verdict(1010, 90, 1800).is_ok(), "10s old is fine");
        let err = h.verdict(1200, 90, 1800).unwrap_err();
        assert!(err.contains("not looping"), "{err}");
        assert!(err.contains("200s"), "the age is reported: {err}");
    }

    #[test]
    fn a_long_turn_is_healthy_because_busy_gets_its_own_leash() {
        // The exact failure this state machine exists to prevent: a turn that legitimately runs for
        // 10 minutes must not be judged by the 90s idle deadline and restarted mid-build.
        let h = hb("busy", 1600, 1000);
        assert!(
            h.verdict(1600, 90, 1800).is_ok(),
            "600s into a turn is working, not wedged"
        );
        let err = h.verdict(3000, 90, 1800).unwrap_err();
        assert!(err.contains("wedged"), "{err}");
        assert!(err.contains("2000s"), "reports how long it's been stuck: {err}");
    }

    #[test]
    fn busy_is_measured_from_when_the_turn_started_not_when_it_was_stamped() {
        // `ts` keeps advancing only if something re-stamps; the turn's age must come from `since`,
        // else a single stamp at turn start would look 0s old forever.
        let h = hb("busy", 1000, 1000);
        assert!(h.verdict(4000, 90, 1800).is_err(), "3000s > 1800s limit");
        // A record with no `since` (older binary) falls back to `ts` rather than reading as age 0.
        let legacy = hb("busy", 1000, 0);
        assert!(legacy.verdict(4000, 90, 1800).is_err());
    }

    #[test]
    fn starting_is_judged_like_idle_so_a_boot_that_never_finishes_fails() {
        let h = hb("starting", 1000, 1000);
        assert!(h.verdict(1030, 90, 1800).is_ok());
        assert!(
            h.verdict(1500, 90, 1800).is_err(),
            "a daemon stuck in `starting` is not alive"
        );
    }

    #[test]
    fn a_backwards_clock_does_not_fail_the_probe() {
        // Container clock steps backwards (NTP correction). `saturating_sub` gives age 0 — we must
        // not restart a healthy daemon over a clock artifact.
        let h = hb("idle", 5000, 5000);
        assert!(h.verdict(1000, 90, 1800).is_ok());
    }

    #[test]
    fn an_unknown_future_state_falls_back_to_freshness_instead_of_failing() {
        // A rolling upgrade can have an OLD binary probing a NEW daemon. Guessing "dead" there would
        // restart a healthy pod, so an unrecognized state is judged on plain freshness.
        let h = hb("draining", 1000, 1000);
        assert!(h.verdict(1100, 90, 1800).is_ok(), "unknown but fresh → pass");
        assert!(h.verdict(9000, 90, 1800).is_err(), "unknown and ancient → fail");
    }

    #[test]
    fn beat_round_trips_and_carries_since_across_a_same_state_restamp() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("aizen-hb-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("AIZEN_HOME", &tmp);

        beat("telegram", State::Busy);
        let first = read().expect("heartbeat written");
        assert_eq!(first.state, "busy");
        assert_eq!(first.platform, "telegram");
        assert_eq!(first.pid, std::process::id());
        assert!(first.since > 0, "since is stamped");

        // Re-stamping the SAME state keeps the original `since` — that's what makes "how long has
        // this turn been running" answerable at all.
        beat("telegram", State::Busy);
        assert_eq!(read().unwrap().since, first.since);

        // A state CHANGE resets it.
        beat("telegram", State::Idle);
        let idle = read().unwrap();
        assert_eq!(idle.state, "idle");
        assert!(idle.since >= first.since);

        clear();
        assert!(read().is_none(), "clear() makes it read as not running");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
