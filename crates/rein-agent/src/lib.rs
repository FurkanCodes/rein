//! The part of rein that acts: the tools the model can call, the loop
//! that runs them, and the conversation it keeps. It never sees a provider's wire format, only
//! rein-llm's `Item`s and `Event`s.

pub mod agent;
pub mod compact;
pub mod context;
mod files;
mod finding;
pub mod mentions;
pub mod peers;
pub mod permissions;
pub mod prompt;
pub mod redact;
pub mod search;
pub mod session;
pub mod tools;
pub mod undo;

/// The walls commands run inside: its own crate, so it can be built and
/// tested alone, on every system it supports.
pub use rein_sandbox as sandbox;
