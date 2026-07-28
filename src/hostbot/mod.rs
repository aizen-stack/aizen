//! `hostbot` — the self-contained "host aizen as a chat bot" feature.
//!
//! One process hosts ONE chat platform (`aizen serve` = Telegram, `aizen discord serve` = Discord),
//! so the daemon ([`daemon::run_daemon`]) is generic over the [`platform::Platform`] contract and
//! monomorphized per platform — no `dyn`, no `async-trait` dep. Adding WhatsApp/Slack/Matrix later is
//! one new file under [`platforms`] that `impl Platform`, plus a one-line `pub mod`; the daemon never
//! changes.
//!
//! Data lives in its own directory `~/.aizen/hostbot/` ([`store`]): `bots.json` (extra bots hosted via
//! `/addbot`) and `sessions/` (one file per conversation, so a `Restart=always` self-host restart
//! keeps its context). The PRIMARY bot's identity stays in `cli-config.json` (it's the daemon's own
//! auth, set up with `aizen telegram/discord setup`). Self-host wiring (systemd) is in [`service`].

mod daemon;
pub mod platform;
pub mod platforms;
mod service;
pub mod store;

pub use daemon::{run_discord_serve, run_serve};
pub use service::run_serve_service;
