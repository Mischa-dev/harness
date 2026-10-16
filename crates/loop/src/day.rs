//! Wakes inside a workday: the day's transcript is loaded from the store, the
//! wake runs on it, and everything the wake adds is saved after each turn,
//! along with a record of the wake itself.

use harness_core::{Clock, WakeReason};
use harness_providers::{CapabilityProfile, Message, Provider};
use harness_store::{Store, StoreError, WakeRecord};
use harness_tools::PolicyExecutor;

use crate::context::ContextAssembler;
use crate::wake::{
    Checkpoint, CheckpointError, Interrupts, LoopError, NoInterrupts, Progress, WakeLimits,
    WakeOutcome, run_wake_interruptible,
};

#[derive(Debug, thiserror::Error)]
pub enum DayError {
    #[error(transparent)]
    Loop(#[from] LoopError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("couldn't serialize the wake reason: {0}")]
    SerializeReason(#[from] serde_json::Error),
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

/// End state of a wake that hasn't finished. A wake still `running` after a
/// restart was cut off; its saved turns and spend stand.
const RUNNING: &str = "running";

impl Workday<'_> {
    /// Runs one wake in today's conversation, starting the day if none is
    /// open. The wake's record opens before the first model call, and each
    /// completed turn is saved with its spend, so neither the train of
    /// thought nor the budget loses more than the turn in flight, whether a
    /// model call fails or the process dies.
    pub async fn wake(
        &mut self,
        reason: &WakeReason,
        wake_message: Message,
    ) -> Result<WakeOutcome, DayError> {
        self.wake_interruptible(reason, wake_message, &mut NoInterrupts)
            .await
    }

    /// [`Self::wake`], taking urgent messages from `interrupts` as it goes.
    pub async fn wake_interruptible(
        &mut self,
        reason: &WakeReason,
        wake_message: Message,
        interrupts: &mut dyn Interrupts,
    ) -> Result<WakeOutcome, DayError> {
        let started_at = self.clock.now();
        let day = match self.store.current_day()? {
            Some(day) => day,
            None => self.store.start_day(started_at)?,
        };
        let mut transcript = self.store.transcript(day.id)?;
        let record = WakeRecord {
            day_id: day.id,
            reason: serde_json::to_string(reason)?,
            started_at,
            ended_at: started_at,
            end: RUNNING.into(),
            turns: 0,
            usage: Default::default(),
            cost_usd: 0.0,
        };
        let wake_id = self.store.begin_wake(&record)?;
        let mut saver = Saver {
            store: self.store,
            clock: self.clock,
            wake_id,
            record,
            saved: transcript.len(),
        };

        let result = run_wake_interruptible(
            self.provider,
            self.executor,
            self.assembler,
            self.profile,
            &mut transcript,
            wake_message,
            self.limits,
            &mut saver,
            interrupts,
        )
        .await;

        let (end, progress) = match &result {
            Ok(outcome) => (
                outcome.end.label(),
                Progress {
                    turns: outcome.turns,
                    usage: outcome.usage,
                    cost_usd: outcome.cost_usd,
                    context_tokens: outcome.context_tokens,
                },
            ),
            Err(err) => ("error", *err.progress()),
        };
        saver.record.end = end.into();
        // A pause was saved with the trip itself. Saving it again here could
        // undo an owner's resume that landed in between.
        saver.write(&transcript, None, &progress, None)?;
        Ok(result?)
    }
}

/// Saves a running wake into the store.
struct Saver<'a> {
    store: &'a mut Store,
    clock: &'a dyn Clock,
    wake_id: i64,
    record: WakeRecord,
    /// How much of the transcript is saved for good. A pending results
    /// message may sit after it.
    saved: usize,
}

impl Saver<'_> {
    fn write(
        &mut self,
        transcript: &[Message],
        pending: Option<&Message>,
        progress: &Progress,
        paused: Option<&str>,
    ) -> Result<(), StoreError> {
        self.record.ended_at = self.clock.now();
        self.record.turns = progress.turns as u32;
        self.record.usage = progress.usage;
        self.record.cost_usd = progress.cost_usd;
        // Starting at `saved` replaces the last pending message, if any.
        match paused {
            Some(breaker) => self.store.save_wake_paused(
                self.wake_id,
                self.saved,
                &transcript[self.saved..],
                &self.record,
                progress.context_tokens,
                breaker,
            )?,
            None => self.store.save_wake_with_pending(
                self.wake_id,
                self.saved,
                &transcript[self.saved..],
                pending,
                &self.record,
                progress.context_tokens,
            )?,
        }
        self.saved = transcript.len();
        Ok(())
    }
}

impl Checkpoint for Saver<'_> {
    fn save(
        &mut self,
        transcript: &[Message],
        pending: Option<&Message>,
        progress: &Progress,
    ) -> Result<(), CheckpointError> {
        Ok(self.write(transcript, pending, progress, None)?)
    }

    fn save_paused(
        &mut self,
        transcript: &[Message],
        progress: &Progress,
        breaker: &str,
    ) -> Result<(), CheckpointError> {
        Ok(self.write(transcript, None, progress, Some(breaker))?)
    }
}
