//! Outbound `notify` notifications (one-way webhook sinks). The two-way `telegram`/`discord` bots
//! moved to the self-contained `hostbot` feature (`src/hostbot/`) — this module now holds only the
//! fire-and-forget notification channels.

pub mod notify;
