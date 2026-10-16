//! The wake filter in the daemon: while the agent sleeps with news waiting
//! for its next look, the helper model looks at that news and at the open
//! optional group replies, at most every couple of minutes. It can wake the
//! agent early, drop what isn't worth its time, and pass optional replies
//! the agent wouldn't add to. Passes go through the policy executor like
//! any other tool call.

use std::sync::Arc;
use std::time::Duration;

use harness_core::WakeReason;
use harness_loop::filter::{self, FilterAgent, FilterDecision, FilterInput, OptionalReply};
use harness_providers::{
    CapabilityProfile, ContentBlock, Profiles, Provider, ReqwestTransport, ToolCall, adapters,
};
use harness_tools::ExecOutcome;
use serde::Deserialize;
use time::OffsetDateTime;

use crate::agent::{Agent, AgentConfig, AgentError, WakeEvent};

/// The filter's instructions; product code, like the charter.
const PROMPT: &str = include_str!("../../../prompts/peripheral.md");

/// Batches news: the filter looks at most this often.
pub const FILTER_GAP: time::Duration = time::Duration::minutes(2);

/// A helper model answers fast; a slow one shouldn't hold up the loop.
const FILTER_TIMEOUT: Duration = Duration::from_secs(60);
/// Workspace answers in well under this.
const INBOX_TIMEOUT: Duration = Duration::from_secs(15);
/// How many open requests the filter looks at.
const INBOX_LIMIT: u16 = 50;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterConfig {
    /// The helper model's capability profile, from `profiles_file`.
    pub profile: String,
    /// The forwarding proxy's route for that profile's provider. Must be on
    /// loopback, like `model_url`.
    pub model_url: String,
}

/// The helper model and what it needs to look out for the agent.
pub struct WakeFilter {
    pub provider: Box<dyn Provider>,
    pub profile_name: String,
    pub profile: CapabilityProfile,
    pub system: String,
    pub agent: FilterAgent,
    pub last_look: Option<OffsetDateTime>,
}

impl WakeFilter {
    pub fn from_config(
        config: &AgentConfig,
        settings: &FilterConfig,
        profiles: &Profiles,
        timeout: Duration,
    ) -> Result<Self, AgentError> {
        let profile = profiles
            .get(&settings.profile)
            .map_err(|e| AgentError::Invalid(format!("filter.profile: {e}")))?
            .clone();
        let transport = ReqwestTransport::with_caller_token_file(
            timeout,
            &settings.model_url,
            &config.caller_token_file,
        )
        .map_err(|e| AgentError::Invalid(format!("filter.model_url: {e}")))?;
        let provider = adapters::for_profile(&profile, &settings.model_url, Arc::new(transport))
            .map_err(|e| AgentError::Invalid(format!("filter: {e}")))?;
        let identity = &config.identity;
        let agent = FilterAgent {
            name: identity.name.clone().unwrap_or_else(|| "the agent".into()),
            owner: identity.owner.clone().unwrap_or_else(|| "its owner".into()),
            responsibilities: identity.responsibilities.clone(),
        };
        Self::new(provider, &settings.profile, profile, agent)
    }

    pub fn new(
        provider: Box<dyn Provider>,
        profile_name: &str,
        profile: CapabilityProfile,
        agent: FilterAgent,
    ) -> Result<Self, AgentError> {
        let system = filter::system_prompt(PROMPT, &agent)
            .map_err(|e| AgentError::Invalid(format!("prompts/peripheral.md: {e}")))?;
        Ok(Self {
            provider,
            profile_name: profile_name.into(),
            profile,
            system,
            agent,
            last_look: None,
        })
    }
}

impl Agent {
    /// Whether the filter should look at the waiting news now: there's
    /// something it hasn't seen, all of it could wait, and it last looked
    /// long enough ago.
    pub fn filter_due(&self, now: OffsetDateTime) -> bool {
        let Some(filter) = &self.filter else {
            return false;
        };
        self.unjudged
            && !self.waiting.is_empty()
            && self
                .waiting
                .iter()
                .all(|e| e.reason.can_wait_for_next_look())
            && filter.last_look.is_none_or(|at| now - at >= FILTER_GAP)
    }

    /// One look by the filter at what's waiting. A filter that fails or
    /// gives an answer it can't read changes nothing: the news waits for
    /// the next look, as it would without a filter.
    pub async fn filter_waiting(&mut self) -> Result<(), harness_store::StoreError> {
        let now = self.clock.now();
        self.unjudged = false;
        let replies = self.optional_replies().await;
        let input = FilterInput {
            agent: self
                .filter
                .as_ref()
                .map(|f| f.agent.clone())
                .unwrap_or_default(),
            watching_for: self.watching_for()?,
            next_look: self.rhythm.next_look(),
            events: self.waiting.iter().map(|e| one_line(&e.text)).collect(),
            replies,
        };
        let Some(filter) = self.filter.as_mut() else {
            return Ok(());
        };
        filter.last_look = Some(now);
        let look = filter::look(
            filter.provider.as_ref(),
            &input,
            &filter.profile_name,
            &filter.system,
        );
        let decision = match tokio::time::timeout(FILTER_TIMEOUT, look).await {
            Ok(Ok((decision, usage))) => {
                let cost = filter.profile.cost_usd(&usage);
                self.store.record_helper_call(now, "wake_filter", cost)?;
                decision
            }
            Ok(Err(e)) => {
                eprintln!("harness-daemon: the wake filter failed: {e}");
                return Ok(());
            }
            Err(_) => {
                eprintln!("harness-daemon: the wake filter took too long");
                return Ok(());
            }
        };
        self.apply(decision).await;
        Ok(())
    }

    async fn apply(&mut self, decision: FilterDecision) {
        for request_id in &decision.pass {
            let call = ToolCall {
                id: format!("filter-pass-{request_id}"),
                name: "reply_pass".into(),
                arguments: serde_json::json!({
                    "request_id": request_id,
                    "reason": "the wake filter judged this not worth a reply"
                }),
            };
            match self.executor.execute(&call).await {
                ExecOutcome::Ran(_) => {}
                other => {
                    eprintln!("harness-daemon: the wake filter's pass didn't go through: {other:?}")
                }
            }
        }
        let mut index = 0;
        self.waiting.retain(|_| {
            let kept = decision.keep.contains(&index);
            index += 1;
            kept
        });
        eprintln!(
            "wake filter: {}; kept {}, passed {}",
            if decision.wake_now {
                "wake now"
            } else {
                "let it sleep"
            },
            self.waiting.len(),
            decision.pass.len()
        );
        if decision.wake_now {
            let reason = if decision.reason.is_empty() {
                "the wake filter thought what's waiting is worth a look now".to_string()
            } else {
                format!(
                    "the wake filter thought this worth a look now: {}",
                    decision.reason
                )
            };
            self.waiting.push(WakeEvent {
                text: "The wake filter woke you early for what's above.".into(),
                reason: WakeReason::Filter { reason },
            });
        }
    }

    /// Open optional group replies, if there's a Workspace to ask. Required
    /// ones and mentions always wake the agent, so the filter never sees
    /// them; nor what shadow mode already answered.
    async fn optional_replies(&self) -> Vec<OptionalReply> {
        let Some(workspace) = &self.workspace else {
            return vec![];
        };
        let entries = match tokio::time::timeout(INBOX_TIMEOUT, workspace.inbox(INBOX_LIMIT)).await
        {
            Ok(Ok(entries)) => entries,
            Ok(Err(e)) => {
                eprintln!("harness-daemon: the wake filter couldn't read the inbox: {e}");
                return vec![];
            }
            Err(_) => return vec![],
        };
        entries
            .into_iter()
            .filter(|e| !e.required && !e.mentioned && e.status == "pending")
            .filter(|e| {
                !self
                    .shadow
                    .as_ref()
                    .is_some_and(|s| s.is_shadowed(&e.request_id))
            })
            .map(|e| OptionalReply {
                request_id: e.request_id,
                conversation: e.conversation_name.unwrap_or_default(),
                excerpt: e.message_excerpt.unwrap_or_default(),
            })
            .collect()
    }

    /// What the agent said it's watching for when it last went to sleep
    /// today.
    fn watching_for(&self) -> Result<Option<String>, harness_store::StoreError> {
        let Some(day) = self.store.current_day()? else {
            return Ok(None);
        };
        let transcript = self.store.transcript(day.id)?;
        Ok(transcript
            .iter()
            .rev()
            .flat_map(|m| m.content.iter().rev())
            .find_map(|block| match block {
                ContentBlock::ToolCall(call) if call.name == "sleep" => Some(
                    call.arguments
                        .get("watching_for")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                ),
                _ => None,
            })
            .flatten())
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
