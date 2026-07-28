//! Cross-cutting foundation shared by every other module: the wire/data `types`, the
//! endpoint `config` + CLI-state `cli_config`, and the outbound `net_guard`. No business
//! logic lives here — this is the layer everything else is allowed to depend on.

pub mod approval;
pub mod cancel;
pub mod cli_config;
pub mod config;
pub mod convo;
pub mod device;
pub mod effort;
pub mod exec_ctx;
pub mod gitx;
pub mod net_guard;
pub mod persist;
pub mod proctree;
pub mod recovery;
pub mod repo_lock;
pub mod steer;
pub mod types;
pub mod workspace_txn;
