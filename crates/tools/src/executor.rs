//! The one door every tool call goes through.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use harness_core::Clock;
use harness_providers::{ToolCall, ToolSpec};
use time::{Duration, OffsetDateTime};

use crate::policy::{Decision, Policy};
use crate::schema;
use crate::tool::{Effect, Tool, ToolContext, ToolError, ToolOutput};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecOutcome {
    /// The call passed every wall and the tool ran.
    Ran(ToolOutput),
    /// The call passed every wall and the tool reported a failure.
    Failed(String),
    /// The call didn't fit the tool: unknown name, bad arguments. Nothing ran.
    Malformed(String),
    /// A wall, an approval requirement or a breaker stopped the call. Nothing ran.
    Blocked(Decision),
}

/// The mandatory gateway from model-requested calls to tool execution.
///
/// It validates arguments, asks each tool to declare its effects, applies
/// walls and circuit breakers, and only then invokes the tool.
pub struct PolicyExecutor {
    policy: Policy,
    tools: BTreeMap<String, RegisteredTool>,
    clock: Arc<dyn Clock>,
    messages_sent: VecDeque<OffsetDateTime>,
    paused: Option<String>,
}

struct RegisteredTool {
    /// Tool schemas are immutable and can be expensive JSON trees. Build each
    /// one once instead of rebuilding it for every model turn and validation.
    spec: ToolSpec,
    tool: Box<dyn Tool>,
}

impl PolicyExecutor {
    /// Creates an executor with no registered tools.
    pub fn new(policy: Policy, clock: Arc<dyn Clock>) -> Self {
        Self {
            policy,
            tools: BTreeMap::new(),
            clock,
            messages_sent: VecDeque::new(),
            paused: None,
        }
    }

    /// Registers or replaces a tool by its specification name.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let spec = tool.spec();
        self.tools
            .insert(spec.name.clone(), RegisteredTool { spec, tool });
    }

    /// Registers a tool and returns the executor for builder-style wiring.
    pub fn with_tool(mut self, tool: impl Tool + 'static) -> Self {
        self.register(Box::new(tool));
        self
    }

    /// Returns model-facing specifications for all registered tools.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|tool| tool.spec.clone()).collect()
    }

    /// The breaker that paused the agent, if one has.
    pub fn paused(&self) -> Option<&str> {
        self.paused.as_deref()
    }

    /// Pauses for a breaker tripped outside the executor, such as the
    /// daemon's wake or spend breakers. Nothing runs until `resume`.
    pub fn pause(&mut self, breaker: &str) {
        self.paused = Some(breaker.to_string());
    }

    /// Only the owner resumes a paused agent; the daemon calls this on their command.
    pub fn resume(&mut self) {
        self.paused = None;
    }

    /// Validates, authorizes, charges, and executes one model-requested call.
    pub async fn execute(&mut self, call: &ToolCall) -> ExecOutcome {
        let effects = match self.tools.get(&call.name) {
            None => {
                return ExecOutcome::Malformed(format!("there is no tool named `{}`", call.name));
            }
            Some(registered) => {
                if let Err(problem) = schema::validate(&registered.spec.parameters, &call.arguments)
                {
                    return ExecOutcome::Malformed(problem);
                }
                match registered.tool.effects(&call.arguments) {
                    Ok(effects) => effects,
                    Err(e) => return ExecOutcome::Malformed(e.to_string()),
                }
            }
        };

        if let Some(breaker) = &self.paused {
            return ExecOutcome::Blocked(Decision::Tripped {
                breaker: breaker.clone(),
            });
        }
        for effect in &effects {
            match self.policy.check(effect) {
                Decision::Allow => {}
                blocked => return ExecOutcome::Blocked(blocked),
            }
        }
        if let Some(tripped) = self.charge_breakers(&effects) {
            return ExecOutcome::Blocked(tripped);
        }

        let tool = &self.tools[&call.name].tool;
        let ctx = ToolContext {
            clock: self.clock.as_ref(),
        };
        match tool.run(&call.arguments, &ctx).await {
            Ok(output) => ExecOutcome::Ran(output),
            Err(ToolError::InvalidArguments(problem)) => ExecOutcome::Malformed(problem),
            Err(ToolError::Failed(problem)) => ExecOutcome::Failed(problem),
        }
    }

    /// Counts the call against the breakers, or trips one and pauses the agent.
    fn charge_breakers(&mut self, effects: &[Effect]) -> Option<Decision> {
        let messages = effects
            .iter()
            .filter(|e| matches!(e, Effect::SendMessage { .. }))
            .count();
        if messages == 0 {
            return None;
        }
        let now = self.clock.now();
        while self
            .messages_sent
            .front()
            .is_some_and(|sent| now - *sent >= Duration::HOUR)
        {
            self.messages_sent.pop_front();
        }
        if self.messages_sent.len() + messages > self.policy.breakers.messages_per_hour {
            let breaker = "messages_per_hour".to_string();
            self.paused = Some(breaker.clone());
            return Some(Decision::Tripped { breaker });
        }
        self.messages_sent
            .extend(std::iter::repeat_n(now, messages));
        None
    }
}
