//! Standalone user-facing capabilities, each wired to a top-level CLI subcommand: the
//! katana-style web `crawl`er, the git-backed `timemachine`, OS-scheduler `cron`, and
//! user-defined slash `commands`. `foreign_session` imports transcripts from other CLIs
//! (Claude Code, Codex) so a conversation started there can be resumed here.

pub mod commands;
pub mod coop;
pub mod crawl;
pub mod cron;
pub mod foreign_session;
pub mod slash;
pub mod slash_handlers;
pub mod timemachine;
pub mod update;
pub mod zones;
