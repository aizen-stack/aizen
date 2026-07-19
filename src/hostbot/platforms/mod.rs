//! Chat-platform implementations of the [`Platform`](super::platform::Platform) contract. Each is one
//! self-contained file; adding WhatsApp/Slack/Matrix later is a new file here + one `pub mod` line.

pub mod telegram;
pub mod discord;
