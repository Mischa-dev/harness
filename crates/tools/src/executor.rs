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

pub struct PolicyExecutor {
    policy: Policy,
    tools: BTreeMap<String, Box<dyn Tool>>,
    clock: Arc<dyn Clock>,
    messages_sent: VecDeque<OffsetDateTime>,
    paused: Option<String>,
}

impl PolicyExecutor {
    pub fn new(policy: Policy, clock: Arc<dyn Clock>) -> Self {
        Self {
            policy,
            tools: BTreeMap::new(),
            clock,
            messages_sent: VecDeque::new(),
            paused: None,
        }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.spec().name, tool);
    }

    pub fn with_tool(mut self, tool: impl Tool + 'static) -> Self {
        self.register(Box::new(tool));
        self
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    /// The breaker that paused the agent, if one has.
    pub fn paused(&self) -> Option<&str> {
        self.paused.as_deref()
    }

    /// Only the owner resumes a paused agent; the daemon calls this on their command.
    pub fn resume(&mut self) {
        self.paused = None;
    }

    pub async fn execute(&mut self, call: &ToolCall) -> ExecOutcome {
        let effects = match self.tools.get(&call.name) {
            None => {
                return ExecOutcome::Malformed(format!("there is no tool named `{}`", call.name));
            }
            Some(tool) => {
                if let Err(problem) = schema::validate(&tool.spec().parameters, &call.arguments) {
                    return ExecOutcome::Malformed(problem);
                }
                match tool.effects(&call.arguments) {
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
        if let Some(tripped) = self.check_breakers(&effects) {
            return ExecOutcome::Blocked(tripped);
        }

        let tool = &self.tools[&call.name];
        let ctx = ToolContext {
            clock: self.clock.as_ref(),
        };
        match tool.run(&call.arguments, &ctx).await {
            Ok(output) => {
                self.record_breaker_effects(&effects);
                ExecOutcome::Ran(output)
            }
            Err(ToolError::InvalidArguments(problem)) => ExecOutcome::Malformed(problem),
            Err(ToolError::Failed(problem)) => ExecOutcome::Failed(problem),
        }
    }

    /// Checks whether a call would exceed a breaker, without charging effects
    /// that may still fail to run.
    fn check_breakers(&mut self, effects: &[Effect]) -> Option<Decision> {
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
        None
    }

    /// Only successful effects count. A malformed or failed send did not put a
    /// message into the world and must not consume the agent's hourly allowance.
    fn record_breaker_effects(&mut self, effects: &[Effect]) {
        let messages = effects
            .iter()
            .filter(|effect| matches!(effect, Effect::SendMessage { .. }))
            .count();
        self.messages_sent
            .extend(std::iter::repeat_n(self.clock.now(), messages));
    }
}
