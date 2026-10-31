//! The part of rein that acts: the tools the model can call, and (next)
//! the loop that runs them. It never sees a provider's wire format, only
//! rein-llm's `Item`s and `Event`s.

pub mod agent;
pub mod tools;
