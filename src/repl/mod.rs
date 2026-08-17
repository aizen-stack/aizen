//! The interactive loop, decomposed: what happens before the first prompt (`startup`), what a typed
//! line means (`input_pre`), the turn itself (`turn`), what follows it (`postturn`), and the
//! best-effort work running alongside it (`background`).
//!
//! `main.rs` keeps only the two loops that drive these — the retained-TUI one and the plain
//! fallback. Everything a turn does is shared between them, on purpose: the two used to hold
//! separate copies of this code and they drifted.

pub mod background;
pub mod input_pre;
pub mod postturn;
pub mod startup;
pub mod turn;
