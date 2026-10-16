//! The agent loop. A wake assembles the context, calls the model, runs every
//! tool call through the policy executor, and repeats until the agent calls
//! `sleep`.

pub mod context;
pub mod day;
pub mod rhythm;
pub mod wake;

pub use context::ContextAssembler;
pub use day::{DayError, Workday};
pub use rhythm::{AfterWake, FALLBACK_NEXT_LOOK, Rhythm};
pub use wake::{
    Checkpoint, CheckpointError, Interrupts, LoopError, NoCheckpoint, NoInterrupts, Progress,
    WakeEnd, WakeLimits, WakeOutcome, run_wake, run_wake_interruptible,
};
