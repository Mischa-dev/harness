use std::path::PathBuf;

use async_trait::async_trait;
use harness_core::Clock;
use harness_providers::ToolSpec;
use serde_json::Value;
use time::OffsetDateTime;

/// What a tool call would do to the world. Every tool declares the effects of
/// a call before it runs, and the policy executor judges those effects. A
/// tool that does something it didn't declare is a bug in that tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    ReadPath(PathBuf),
    WritePath(PathBuf),
    /// A process run with an argument array, never a shell string.
    RunCommand {
        program: String,
        args: Vec<String>,
    },
    /// A message posted somewhere a person or agent will read it.
    SendMessage {
        conversation: String,
    },
    /// An action behind a wall, such as `merge`, `publish` or `spend`.
    Action {
        name: String,
    },
}

/// What the loop needs to know after a tool runs, beyond its text output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// The agent ended its wake and wants to look again at `until`.
    Sleep { until: OffsetDateTime },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub control: Option<Control>,
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            control: None,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ToolError {
    /// The arguments don't make sense for this tool. Shown to the model.
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    /// The tool ran and failed. Shown to the model.
    #[error("{0}")]
    Failed(String),
}

pub struct ToolContext<'a> {
    /// The runtime clock for time-dependent tool behavior.
    pub clock: &'a dyn Clock,
}

/// A model-callable operation whose effects can be authorized before it runs.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Describes the tool and its argument schema to the model.
    fn spec(&self) -> ToolSpec;

    /// The effects this call would have. Called before the policy check, so
    /// it must not touch the world.
    fn effects(&self, args: &Value) -> Result<Vec<Effect>, ToolError>;

    /// Performs an already validated and authorized call.
    async fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError>;
}
