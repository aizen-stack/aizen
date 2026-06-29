//! External messaging integrations — the two-way `telegram` and `discord` bots plus
//! desktop `notify` notifications. These bridge the agent to the outside world.

pub mod discord;
pub mod notify;
pub mod telegram;
