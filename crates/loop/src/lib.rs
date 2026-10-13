//! The agent loop. A wake assembles the context, calls the model, runs every
//! tool call through the policy executor, and repeats until the agent calls
//! `sleep`.

pub mod context;
pub mod wake;

pub use context::ContextAssembler;
pub use wake::{LoopError, WakeEnd, WakeLimits, WakeOutcome, run_wake};
