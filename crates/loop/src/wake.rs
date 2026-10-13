//! One wake: model turns and tool calls until the agent sleeps, or something
//! stops it.

use harness_providers::{
    CapabilityProfile, ContentBlock, Message, Provider, ProviderError, Role, ToolCall,
    ToolCallStyle, ToolResult, Usage,
};
use harness_tools::{Control, Decision, ExecOutcome, PolicyExecutor};
use time::OffsetDateTime;

use crate::context::ContextAssembler;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeLimits {
    /// Model turns before the wake is cut off. Long work belongs to workers.
    pub max_turns: usize,
}

impl Default for WakeLimits {
    fn default() -> Self {
        Self { max_turns: 25 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeEnd {
    /// The agent called `sleep`.
    Slept { until: OffsetDateTime },
    /// The agent stopped talking without calling `sleep`, even after a reminder.
    /// The daemon picks the next look.
    NoSleep,
    /// A circuit breaker tripped. The agent stays paused until the owner resumes it.
    Paused { breaker: String },
    /// The model sent malformed tool calls on two turns in a row.
    Malformed,
    /// The wake hit [`WakeLimits::max_turns`].
    TurnLimit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WakeOutcome {
    pub end: WakeEnd,
    pub turns: usize,
    pub usage: Usage,
    pub cost_usd: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum LoopError {
    #[error("model call failed: {0}")]
    Provider(#[from] ProviderError),
}

const SLEEP_REMINDER: &str =
    "(runtime) End the wake by calling `sleep` with when you want to look again.";

/// Runs one wake. `transcript` is the day's conversation; the wake message
/// and everything that follows are appended to it.
pub async fn run_wake(
    provider: &dyn Provider,
    executor: &mut PolicyExecutor,
    assembler: &ContextAssembler,
    profile: &CapabilityProfile,
    transcript: &mut Vec<Message>,
    wake_message: Message,
    limits: WakeLimits,
) -> Result<WakeOutcome, LoopError> {
    transcript.push(wake_message);
    let mut usage = Usage::default();
    let mut reminded = false;
    let mut malformed_last_turn = false;

    let mut turns = 0;
    let end = loop {
        if turns == limits.max_turns {
            break WakeEnd::TurnLimit;
        }
        turns += 1;

        let request = assembler.assemble(executor.specs(), transcript);
        let response = provider.complete(&request).await?;
        usage.add(response.usage);
        let calls: Vec<ToolCall> = response.message.tool_calls().cloned().collect();
        transcript.push(response.message);

        if calls.is_empty() {
            if reminded {
                break WakeEnd::NoSleep;
            }
            reminded = true;
            transcript.push(Message::user_text(SLEEP_REMINDER));
            continue;
        }

        let mut results = Vec::with_capacity(calls.len());
        let mut sleep_until = None;
        let mut paused = None;
        let mut malformed = false;
        for (i, call) in calls.iter().enumerate() {
            if paused.is_some() {
                results.push(error(call, "not run: the agent is paused"));
                continue;
            }
            if i > 0 && profile.tool_calls == ToolCallStyle::Sequential {
                results.push(error(call, "not run: make one tool call per turn"));
                continue;
            }
            match executor.execute(call).await {
                ExecOutcome::Ran(output) => {
                    if let Some(Control::Sleep { until }) = output.control {
                        sleep_until = Some(until);
                    }
                    results.push(ok(call, output.content));
                }
                ExecOutcome::Failed(problem) => results.push(error(call, problem)),
                ExecOutcome::Malformed(problem) => {
                    malformed = true;
                    results.push(error(
                        call,
                        format!("malformed call: {problem}. Fix the call and try again."),
                    ));
                }
                ExecOutcome::Blocked(Decision::Tripped { breaker }) => {
                    results.push(error(call, format!("blocked: breaker {breaker} tripped")));
                    paused = Some(breaker);
                }
                ExecOutcome::Blocked(Decision::NeedsApproval { reason }) => {
                    results.push(error(call, format!("not run: {reason}")));
                }
                ExecOutcome::Blocked(Decision::Deny { reason }) => {
                    results.push(error(call, format!("blocked by a wall: {reason}")));
                }
                ExecOutcome::Blocked(Decision::Allow) => {
                    unreachable!("the executor runs allowed calls")
                }
            }
        }
        transcript.push(Message {
            role: Role::User,
            content: results.into_iter().map(ContentBlock::ToolResult).collect(),
        });

        if let Some(breaker) = paused {
            break WakeEnd::Paused { breaker };
        }
        if let Some(until) = sleep_until {
            break WakeEnd::Slept { until };
        }
        if malformed && malformed_last_turn {
            break WakeEnd::Malformed;
        }
        malformed_last_turn = malformed;
    };

    Ok(WakeOutcome {
        end,
        turns,
        cost_usd: profile.cost_usd(&usage),
        usage,
    })
}

fn ok(call: &ToolCall, content: String) -> ToolResult {
    ToolResult {
        call_id: call.id.clone(),
        content,
        is_error: false,
    }
}

fn error(call: &ToolCall, content: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: call.id.clone(),
        content: content.into(),
        is_error: true,
    }
}
