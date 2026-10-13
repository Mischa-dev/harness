//! When a workday ends. The agent gets its evening turn at the owner's quiet
//! hour, or earlier once the day's context passes a size limit
//! (ARCHITECTURE.md, "Workdays").

use time::{Duration, OffsetDateTime, Time, UtcOffset};

use crate::Day;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkdayPolicy {
    /// The owner's UTC offset, for reading the quiet hour.
    pub offset: UtcOffset,
    /// The local hour (0–23) at which the evening turn is due.
    pub quiet_hour: u8,
    /// Context size, in tokens, past which the evening turn comes early.
    pub max_context_tokens: u64,
}

impl Default for WorkdayPolicy {
    fn default() -> Self {
        Self {
            offset: UtcOffset::UTC,
            quiet_hour: 2,
            max_context_tokens: 80_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EveningReason {
    QuietHour,
    ContextFull,
}

impl WorkdayPolicy {
    /// The first quiet hour after the day started.
    pub fn quiet_hour_after(&self, started_at: OffsetDateTime) -> OffsetDateTime {
        let local = started_at.to_offset(self.offset);
        let hour = Time::from_hms(self.quiet_hour.min(23), 0, 0).expect("valid hour");
        let mut boundary = local.replace_time(hour);
        if boundary <= local {
            boundary += Duration::DAY;
        }
        boundary
    }

    /// Whether the day's evening turn is due now, and why.
    pub fn evening_due(&self, day: &Day, now: OffsetDateTime) -> Option<EveningReason> {
        if day.ended_at.is_some() {
            return None;
        }
        if day.context_tokens >= self.max_context_tokens {
            return Some(EveningReason::ContextFull);
        }
        if now >= self.quiet_hour_after(day.started_at) {
            return Some(EveningReason::QuietHour);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::{datetime, offset};

    fn day(started_at: OffsetDateTime, context_tokens: u64) -> Day {
        Day {
            id: 1,
            started_at,
            ended_at: None,
            handoff: None,
            context_tokens,
        }
    }

    #[test]
    fn the_evening_comes_at_the_next_local_quiet_hour() {
        let policy = WorkdayPolicy {
            offset: offset!(+2),
            quiet_hour: 2,
            max_context_tokens: 80_000,
        };
        // 09:00 UTC is 11:00 local; the next 02:00 local is 00:00 UTC tomorrow.
        let d = day(datetime!(2026-01-05 09:00 UTC), 0);
        assert_eq!(
            policy.quiet_hour_after(d.started_at),
            datetime!(2026-01-06 00:00 UTC)
        );
        assert_eq!(
            policy.evening_due(&d, datetime!(2026-01-05 23:59 UTC)),
            None
        );
        assert_eq!(
            policy.evening_due(&d, datetime!(2026-01-06 00:00 UTC)),
            Some(EveningReason::QuietHour)
        );
    }

    #[test]
    fn a_day_started_just_after_the_quiet_hour_runs_a_full_day() {
        let policy = WorkdayPolicy::default();
        let d = day(datetime!(2026-01-05 02:00 UTC), 0);
        assert_eq!(
            policy.quiet_hour_after(d.started_at),
            datetime!(2026-01-06 02:00 UTC)
        );
        let before = day(datetime!(2026-01-05 01:30 UTC), 0);
        assert_eq!(
            policy.quiet_hour_after(before.started_at),
            datetime!(2026-01-05 02:00 UTC)
        );
    }

    #[test]
    fn a_full_context_brings_the_evening_early() {
        let policy = WorkdayPolicy::default();
        let start = datetime!(2026-01-05 09:00 UTC);
        assert_eq!(policy.evening_due(&day(start, 79_999), start), None);
        assert_eq!(
            policy.evening_due(&day(start, 80_000), start),
            Some(EveningReason::ContextFull)
        );
    }

    #[test]
    fn an_ended_day_is_never_due() {
        let mut d = day(datetime!(2026-01-05 09:00 UTC), 100_000);
        d.ended_at = Some(datetime!(2026-01-06 02:00 UTC));
        assert_eq!(
            WorkdayPolicy::default().evening_due(&d, datetime!(2026-01-07 00:00 UTC)),
            None
        );
    }
}
