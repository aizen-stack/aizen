//! The `Platform` contract — the seam that makes "host a bot" independent of *which* chat platform.
//!
//! `aizen serve` (Telegram) and `aizen discord serve` (Discord) each run ONE platform per process, so
//! the daemon is generic over `P: Platform` and monomorphized per platform — no `dyn`, no `async-trait`
//! dep (plain AFIT, stable since Rust 1.75). Adding WhatsApp/Slack/Matrix later = one new file under
//! `platforms/` that `impl Platform`, plus a one-line `pub mod`. The daemon loop never changes.
//!
//! Platform-specific powers (inline approval buttons, multi-bot hosting) are trait methods with a
//! default "unsupported" impl: Telegram overrides them, Discord inherits the defaults, and the shared
//! command dispatcher gates on `supports_*` so `/addbot` is naturally Telegram-only.

use anyhow::{bail, Result};
use tokio::sync::mpsc::Sender;

/// One inbound message handed to the daemon loop. `route` is the sub-bot name it arrived on
/// ("default" for the primary / for platforms without multi-bot), used to send the reply back on the
/// SAME bot and to key the persisted session.
pub struct Inbound<C> {
    pub route: String,
    pub chat: C,
    pub text: String,
}

/// A hosted bot as shown by `/bots`.
pub struct BotInfo {
    pub name: String,
    pub username: String,
    pub chats: usize,
}

/// The contract a chat platform fulfils to be hosted by the daemon. `Send + Sync + 'static` so it can
/// live in an `Arc` shared with spawned listener tasks.
#[allow(async_fn_in_trait)] // generic (monomorphized) use only — no dyn, so no Send-bound footgun.
pub trait Platform: Send + Sync + 'static {
    /// The platform's chat/channel id type (Telegram `i64`, Discord `u64`). `Display`/`FromStr` let the
    /// session store round-trip it through a self-describing JSON file (no id type baked into a path).
    type Chat: Copy
        + Eq
        + std::hash::Hash
        + Send
        + Sync
        + std::fmt::Display
        + std::str::FromStr
        + 'static;

    /// Stable platform slug — the session-store namespace + `/status` label ("telegram" | "discord").
    fn name(&self) -> &'static str;

    /// Reply chunk cap in UTF-16 units (Telegram 4096 / Discord ~1900) — the daemon splits to fit.
    fn message_max(&self) -> usize;

    /// Spawn the listener(s) (poll loop / gateway). Each allowed inbound message → `tx`. Returns once
    /// listeners are launched (they run in the background); an error means the platform can't start.
    async fn start(&self, tx: Sender<Inbound<Self::Chat>>) -> Result<()>;

    /// Send a reply on sub-bot `route` to `chat`.
    async fn send(&self, route: &str, chat: Self::Chat, text: &str) -> Result<()>;

    // ── optional capabilities (Discord inherits the "no" defaults) ──────────────────────────────

    /// True if a destructive-op approval can be routed to this platform as an inline ✓/✗ prompt. When
    /// false the agent's approval gate auto-denies (the op is skipped) — there's no button to answer.
    fn supports_approval(&self) -> bool {
        false
    }
    /// Pin the approval route to `(route, chat)` before a turn so a prompt returns to the SAME bot.
    fn set_approval_route(&self, _route: &str, _chat: Self::Chat) {}
    /// Clear it after the turn so it never leaks to the next one.
    fn clear_approval_route(&self) {}

    /// True if this platform can host extra bots live (`/addbot` / `/rmbot`).
    fn supports_multibot(&self) -> bool {
        false
    }
    /// Validate `token`, persist + hot-spawn a new bot on `route`=`name`. Returns its @username.
    async fn add_bot(&self, _name: &str, _token: &str, _tx: &Sender<Inbound<Self::Chat>>) -> Result<String> {
        bail!("hosting extra bots is only supported on Telegram")
    }
    /// Stop + forget a hosted bot.
    async fn remove_bot(&self, _name: &str) -> Result<()> {
        bail!("hosting extra bots is only supported on Telegram")
    }
    /// Snapshot of currently hosted bots (for `/bots`).
    fn list_bots(&self) -> Vec<BotInfo> {
        vec![]
    }

    /// The persona a given sub-bot `route` should speak as, layered over the global `config.persona`
    /// for one turn. `None` ⇒ use the global persona (the primary "default" bot = the user's own agent
    /// identity). Only affects the `<persona>`/`<self>` blocks; `<user_memory>` stays global, so a
    /// hosted bot has its own character but shares the primary agent's memory. Default: no override.
    fn persona_for(&self, _route: &str) -> Option<String> {
        None
    }

    /// Called when the daemon loop exits (Ctrl-C, or returning to the REPL — `run_serve` is reachable
    /// from the landing menu, not only on process death). The platform aborts its listener tasks and
    /// clears any global state here so nothing keeps polling after the loop returns. Default no-op.
    fn shutdown(&self) {}
}
