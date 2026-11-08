//! The part of rein that acts: the tools the model can call, the loop
//! that runs them, and the conversation it keeps. It never sees a provider's wire format, only
//! rein-llm's `Item`s and `Event`s.

pub mod agent;
pub mod background;
pub mod check;
pub mod claude;
pub mod compact;
pub mod context;
mod files;
mod finding;
pub mod hooks;
pub mod image;
pub mod memory;
pub mod mentions;
pub mod pages;
pub mod patch;
pub mod peers;
pub mod permissions;
pub mod prompt;
pub mod redact;
pub mod search;
pub mod session;
pub mod skills;
pub mod tools;
pub mod undo;
pub mod web;

/// The walls commands run inside: its own crate, so it can be built and
/// tested alone, on every system it supports.
pub use rein_sandbox as sandbox;
