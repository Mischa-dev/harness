//! Tools and the policy executor in front of all of them. Every tool call the
//! agent makes goes through [`PolicyExecutor::execute`]; there is no other
//! way to run a tool. Walls are checked here, in code, before anything runs.

pub mod executor;
pub mod files;
pub mod memory;
pub mod policy;
pub mod schedule;
pub mod schema;
pub mod shell;
pub mod sleep;
pub mod tool;

pub use executor::{ExecOutcome, PolicyExecutor};
pub use memory::MemoryFiles;
pub use policy::{BreakerLimits, CommandRule, Decision, Policy};
pub use schedule::Schedule;
pub use shell::ShellTool;
pub use sleep::SleepTool;
pub use tool::{Control, Effect, Tool, ToolContext, ToolError, ToolOutput};
