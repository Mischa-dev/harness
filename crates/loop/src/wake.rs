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
    /// The context size of the wake's last model call, in tokens. The
    /// workday policy ends the day early once this grows too large.
    pub context_tokens: u64,
}

impl WakeEnd {
    /// A short name for records and logs.
    pub fn label(&self) -> &'static str {
        match self {
            WakeEnd::Slept { .. } => "slept",
            WakeEnd::NoSleep => "no_sleep",
            WakeEnd::Paused { .. } => "paused",
            WakeEnd::Malformed => "malformed",
            WakeEnd::TurnLimit => "turn_limit",
        }
    }
}

/// What a wake has done so far.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Progress {
    pub turns: usize,
    pub usage: Usage,
    pub cost_usd: f64,
    /// The context size of the wake's last successful call.
    pub context_tokens: u64,
}

pub type CheckpointError = Box<dyn std::error::Error + Send + Sync>;

/// Where a wake is saved as it goes: tools may already have acted, and after
/// a restart the agent must see what it did.
pub trait Checkpoint {
    /// Saves the wake so far. `pending` is the results message of a batch of
    /// tool calls still running: calls that finished carry their results, the
    /// rest say the wake was cut off. The next save replaces it.
    fn save(
        &mut self,
        transcript: &[Message],
        pending: Option<&Message>,
        progress: &Progress,
    ) -> Result<(), CheckpointError>;
}

/// For callers that keep the wake in memory only.
pub struct NoCheckpoint;

impl Checkpoint for NoCheckpoint {
    fn save(
        &mut self,
        _: &[Message],
        _: Option<&Message>,
        _: &Progress,
    ) -> Result<(), CheckpointError> {
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LoopError {
    /// A model call failed. What the wake spent before it is kept for the
    /// budget.
    #[error("model call failed: {source}")]
    Provider {
        source: ProviderError,
        progress: Progress,
    },
    /// A turn couldn't be saved. The wake stops rather than act without a
    /// record.
    #[error("couldn't save the wake: {source}")]
    Checkpoint {
        source: CheckpointError,
        progress: Progress,
    },
}

impl LoopError {
    pub fn progress(&self) -> &Progress {
        match self {
            LoopError::Provider { progress, .. } | LoopError::Checkpoint { progress, .. } => {
                progress
            }
        }
    }
}

const SLEEP_REMINDER: &str =
    "(runtime) End the wake by calling `sleep` with when you want to look again.";

/// What a saved call says until it reports back. If the process dies, this is
/// what the agent finds after the restart.
const CUT_OFF: &str = "cut off: the wake stopped before this call reported back. \
     Check whether it took effect before trying it again.";

/// Runs one wake. `transcript` is the day's conversation; the wake message
/// and everything that follows are appended to it. `checkpoint` is handed the
/// transcript after every turn, and before each tool call runs.
// Each argument is a separate piece of the daemon's wiring; a struct would
// only rename them.
#[allow(clippy::too_many_arguments)]
pub async fn run_wake(
    provider: &dyn Provider,
    executor: &mut PolicyExecutor,
    assembler: &ContextAssembler,
    profile: &CapabilityProfile,
    transcript: &mut Vec<Message>,
    wake_message: Message,
    limits: WakeLimits,
    checkpoint: &mut dyn Checkpoint,
) -> Result<WakeOutcome, LoopError> {
    transcript.push(wake_message);
    let mut usage = Usage::default();
    let mut reminded = false;
    let mut malformed_last_turn = false;
    let mut context_tokens = 0;
    let progress = |turns, usage: Usage, context_tokens| Progress {
        turns,
        usage,
        cost_usd: profile.cost_usd(&usage),
        context_tokens,
    };
    let save = |checkpoint: &mut dyn Checkpoint,
                transcript: &[Message],
                pending: Option<&Message>,
                progress: Progress| {
        checkpoint
            .save(transcript, pending, &progress)
            .map_err(|source| LoopError::Checkpoint { source, progress })
    };

    // The wake message carries the event that woke the agent; keep it even if
    // the first call never returns.
    save(
        checkpoint,
        transcript,
        None,
        progress(0, usage, context_tokens),
    )?;

    let mut turns = 0;
    let end = loop {
        if turns == limits.max_turns {
            break WakeEnd::TurnLimit;
        }
        turns += 1;

        let request = assembler.assemble(executor.specs(), transcript);
        let response = match provider.complete(&request).await {
            Ok(response) => response,
            Err(source) => {
                return Err(LoopError::Provider {
                    source,
                    progress: progress(turns, usage, context_tokens),
                });
            }
        };
        usage.add(response.usage);
        let u = response.usage;
        let input_tokens = u
            .input_tokens
            .saturating_add(u.cache_read_tokens)
            .saturating_add(u.cache_write_tokens);
        context_tokens = input_tokens.saturating_add(u.output_tokens);
        if input_tokens == 0 {
            // The provider didn't report usage. Estimate, so the day's size
            // still grows toward its evening.
            context_tokens = context_tokens
                .saturating_add(estimate_tokens(&request))
                .saturating_add(estimate_tokens(&response.message));
        }
        let calls: Vec<ToolCall> = response.message.tool_calls().cloned().collect();
        transcript.push(response.message);

        if calls.is_empty() {
            if reminded {
                break WakeEnd::NoSleep;
            }
            reminded = true;
            transcript.push(Message::user_text(SLEEP_REMINDER));
            save(
                checkpoint,
                transcript,
                None,
                progress(turns, usage, context_tokens),
            )?;
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
            // Save before the call runs, so a crash mid-batch keeps the
            // results of the calls that already acted.
            let pending = results_message(
                results
                    .iter()
                    .cloned()
                    .chain(calls[i..].iter().map(|c| error(c, CUT_OFF))),
            );
            save(
                checkpoint,
                transcript,
                Some(&pending),
                progress(turns, usage, context_tokens),
            )?;
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
        // The next call will send these results, so they count toward the
        // context the workday policy watches; a big result can't hide.
        context_tokens = context_tokens.saturating_add(
            results
                .iter()
                .map(|r| r.content.len().div_ceil(BYTES_PER_TOKEN) as u64)
                .fold(0_u64, u64::saturating_add),
        );
        transcript.push(results_message(results));
        save(
            checkpoint,
            transcript,
            None,
            progress(turns, usage, context_tokens),
        )?;

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
        context_tokens,
    })
}

/// A rough size for text the model hasn't counted yet. Overestimating is the
/// safe side: it only brings the evening forward.
const BYTES_PER_TOKEN: usize = 3;

fn estimate_tokens(value: &impl serde::Serialize) -> u64 {
    serde_json::to_string(value).map_or(0, |json| json.len().div_ceil(BYTES_PER_TOKEN) as u64)
}

fn results_message(results: impl IntoIterator<Item = ToolResult>) -> Message {
    Message {
        role: Role::User,
        content: results.into_iter().map(ContentBlock::ToolResult).collect(),
    }
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
