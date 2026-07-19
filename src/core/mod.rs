//! Cross-cutting foundation shared by every other module: the wire/data `types`, the
//! endpoint `config` + CLI-state `cli_config`, and the outbound `net_guard`. No business
//! logic lives here — this is the layer everything else is allowed to depend on.

pub mod approval;
pub mod cli_config;
pub mod config;
pub mod effort;
pub mod net_guard;
pub mod persist;
pub mod repo_lock;
pub mod types;
