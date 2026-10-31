//! The part of rein that acts: the tools the model can call, the loop
//! that runs them, and the conversation it keeps. It never sees a provider's wire format, only
//! rein-llm's `Item`s and `Event`s.

pub mod agent;
pub mod compact;
pub mod context;
pub mod permissions;
pub mod prompt;
pub mod session;
pub mod tools;
