//! Standalone user-facing capabilities, each wired to a top-level CLI subcommand: the
//! katana-style web `crawl`er, the git-backed `timemachine`, OS-scheduler `cron`, and
//! user-defined slash `commands`.

pub mod commands;
pub mod crawl;
pub mod cron;
pub mod timemachine;
