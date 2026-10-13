//! Wakes inside a workday: the day's transcript is loaded from the store, the
//! wake runs on it, and everything the wake added is saved, along with a
//! record of the wake itself.

use harness_core::{Clock, WakeReason};
use harness_providers::{CapabilityProfile, Message, Provider};
use harness_store::{Store, StoreError, WakeRecord};
use harness_tools::PolicyExecutor;

use crate::context::ContextAssembler;
use crate::wake::{LoopError, WakeLimits, WakeOutcome, run_wake};

#[derive(Debug, thiserror::Error)]
pub enum DayError {
    #[error(transparent)]
    Loop(#[from] LoopError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Everything a recorded wake needs, borrowed from the daemon's wiring.
pub struct Workday<'a> {
    pub store: &'a mut Store,
    pub provider: &'a dyn Provider,
    pub executor: &'a mut PolicyExecutor,
    pub assembler: &'a ContextAssembler,
    pub profile: &'a CapabilityProfile,
    pub clock: &'a dyn Clock,
    pub limits: WakeLimits,
}

impl Workday<'_> {
    /// Runs one wake in today's conversation, starting the day if none is
    /// open. What the wake added to the transcript is saved even when the
    /// model call fails partway, so the train of thought isn't lost.
    pub async fn wake(
        &mut self,
        reason: &WakeReason,
        wake_message: Message,
    ) -> Result<WakeOutcome, DayError> {
        let started_at = self.clock.now();
        let day = match self.store.current_day()? {
            Some(day) => day,
            None => self.store.start_day(started_at)?,
        };
        let mut transcript = self.store.transcript(day.id)?;
        let before = transcript.len();

        let result = run_wake(
            self.provider,
            self.executor,
            self.assembler,
            self.profile,
            &mut transcript,
            wake_message,
            self.limits,
        )
        .await;

        self.store.append(day.id, &transcript[before..])?;
        let (end, turns, usage, cost_usd, context_tokens) = match &result {
            Ok(outcome) => (
                outcome.end.label().to_string(),
                outcome.turns,
                outcome.usage,
                outcome.cost_usd,
                outcome.context_tokens,
            ),
            Err(LoopError::Provider {
                turns,
                usage,
                cost_usd,
                ..
            }) => (
                "error".to_string(),
                *turns,
                *usage,
                *cost_usd,
                day.context_tokens,
            ),
        };
        self.store.record_wake(
            &WakeRecord {
                day_id: day.id,
                reason: serde_json::to_string(reason).unwrap_or_default(),
                started_at,
                ended_at: self.clock.now(),
                end,
                turns: turns as u32,
                usage,
                cost_usd,
            },
            context_tokens,
        )?;
        Ok(result?)
    }
}
