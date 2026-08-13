//! Model-provider clients.
//!
//! `legacy_client` is the original OpenAI-compatible chat-completions implementation. `client` is
//! a compatibility facade that keeps that public API intact while routing the first-party ChatGPT
//! Codex backend through its Responses/OAuth transport.

#[path = "client.rs"]
pub(crate) mod legacy_client;
pub mod chatgpt;
#[path = "client_facade.rs"]
pub mod client;
