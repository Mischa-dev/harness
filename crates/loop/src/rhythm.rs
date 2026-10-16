//! The wake loop's state between wakes: when the agent looks next, whether
//! today has ended, and whether a breaker paused it. The daemon and the
//! simulator both drive wakes through this, so the simulator tests the
//! daemon's own rules.

use harness_store::{Store, StoreError, WorkdayPolicy};
use time::{Duration, OffsetDateTime};

use crate::day::DayError;
use crate::wake::{WakeEnd, WakeOutcome};

/// When a wake ends without the agent choosing a next look, it looks again
/// after this long.
pub const FALLBACK_NEXT_LOOK: Duration = Duration::hours(1);

/// How a wake left the loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterWake {
    /// The agent chose its next look.
    Slept,
    /// A breaker tripped. No more wakes until the owner resumes the agent.
    Paused { breaker: String },
    /// The wake ended without a chosen next look, so the fallback applies.
    /// `problem` says why, for the log.
    Fallback { problem: String },
}

#[derive(Debug, Clone)]
pub struct Rhythm {
    policy: WorkdayPolicy,
    next_look: OffsetDateTime,
    paused: Option<String>,
}

impl Rhythm {
    /// `first_look` is when the agent wakes if nothing arrives sooner.
    pub fn new(policy: WorkdayPolicy, first_look: OffsetDateTime) -> Self {
        Self {
            policy,
            next_look: first_look,
            paused: None,
        }
    }

    pub fn next_look(&self) -> OffsetDateTime {
        self.next_look
    }

    /// The breaker that paused the agent, if one did.
    pub fn paused(&self) -> Option<&str> {
        self.paused.as_deref()
    }

    /// Ends today before a wake at `at` if its evening is due, so the wake
    /// starts a fresh day. Returns whether a day ended.
    // The evening turn proper comes later; for now the day just ends.
    pub fn close_day_if_due(&self, store: &Store, at: OffsetDateTime) -> Result<bool, StoreError> {
        match store.current_day()? {
            Some(day) if self.policy.evening_due(&day, at).is_some() => {
                store.end_day(day.id, at, None)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Sets the next look from how a wake ended. `now` is when it ended.
    pub fn after(
        &mut self,
        result: &Result<WakeOutcome, DayError>,
        now: OffsetDateTime,
    ) -> AfterWake {
        let problem = match result {
            Ok(outcome) => match &outcome.end {
                WakeEnd::Slept { until } => {
                    // Never in the past: a wake that ran long still waits
                    // for the next event or tick.
                    self.next_look = (*until).max(now);
                    return AfterWake::Slept;
                }
                WakeEnd::Paused { breaker } => {
                    self.paused = Some(breaker.clone());
                    return AfterWake::Paused {
                        breaker: breaker.clone(),
                    };
                }
                other => format!("the wake ended {}", other.label()),
            },
            Err(err) => err.to_string(),
        };
        self.next_look = now + FALLBACK_NEXT_LOOK;
        AfterWake::Fallback { problem }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wake::LoopError;
    use harness_providers::{ProviderError, Usage};
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-01-05 09:00 UTC);

    fn ended(end: WakeEnd) -> Result<WakeOutcome, DayError> {
        Ok(WakeOutcome {
            end,
            turns: 1,
            usage: Usage::default(),
            cost_usd: 0.0,
            context_tokens: 0,
        })
    }

    #[test]
    fn the_agent_chooses_its_next_look() {
        let mut rhythm = Rhythm::new(WorkdayPolicy::default(), NOW);
        let until = NOW + Duration::hours(3);
        assert_eq!(
            rhythm.after(&ended(WakeEnd::Slept { until }), NOW),
            AfterWake::Slept
        );
        assert_eq!(rhythm.next_look(), until);
    }

    #[test]
    fn a_next_look_already_past_is_now() {
        let mut rhythm = Rhythm::new(WorkdayPolicy::default(), NOW);
        let late = NOW + Duration::hours(1);
        rhythm.after(&ended(WakeEnd::Slept { until: NOW }), late);
        assert_eq!(rhythm.next_look(), late);
    }

    #[test]
    fn anything_else_falls_back_to_an_hour() {
        for result in [
            ended(WakeEnd::NoSleep),
            ended(WakeEnd::TurnLimit),
            ended(WakeEnd::Malformed),
            Err(DayError::Loop(LoopError::Provider {
                source: ProviderError::RateLimited,
                progress: Default::default(),
            })),
        ] {
            let mut rhythm = Rhythm::new(WorkdayPolicy::default(), NOW);
            assert!(matches!(
                rhythm.after(&result, NOW),
                AfterWake::Fallback { .. }
            ));
            assert_eq!(rhythm.next_look(), NOW + FALLBACK_NEXT_LOOK);
            assert_eq!(rhythm.paused(), None);
        }
    }

    #[test]
    fn a_breaker_pauses_the_agent() {
        let mut rhythm = Rhythm::new(WorkdayPolicy::default(), NOW);
        let paused = ended(WakeEnd::Paused {
            breaker: "spend".into(),
        });
        assert!(matches!(
            rhythm.after(&paused, NOW),
            AfterWake::Paused { .. }
        ));
        assert_eq!(rhythm.paused(), Some("spend"));
    }

    #[test]
    fn the_day_ends_at_the_quiet_hour() {
        let store = Store::open_in_memory().unwrap();
        let rhythm = Rhythm::new(WorkdayPolicy::default(), NOW);
        store.start_day(NOW).unwrap();
        assert!(!rhythm.close_day_if_due(&store, NOW).unwrap());
        // The default quiet hour is 02:00 UTC.
        let next_night = datetime!(2026-01-06 02:00 UTC);
        assert!(rhythm.close_day_if_due(&store, next_night).unwrap());
        assert!(store.current_day().unwrap().is_none());
    }
}
