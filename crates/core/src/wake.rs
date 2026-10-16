//! Why the agent woke, and whether it can interrupt a wake already running.

use serde::{Deserialize, Serialize};

/// `Now` events interrupt a running wake at its next safe point. `Queued`
/// events wait for the current wake to end (ARCHITECTURE.md, "Interrupts").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    Now,
    Queued,
}

/// The things that wake the agent directly. Everything else goes through the
/// peripheral filter first and arrives here only as [`WakeReason::Filter`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WakeReason {
    /// A Workspace wakeup: a required reply, an @mention, a task assignment, an approval.
    Workspace { summary: String, urgent: bool },
    /// An Agent OS trigger aimed at the agent.
    Trigger { name: String },
    /// One of its own follow-ups or routines came due.
    FollowUp { schedule_id: String, note: String },
    /// One of its workers finished.
    WorkerFinished { worker_id: String },
    /// A quarantined claim scored 8+ in its area.
    Claim { claim_id: String, importance: u8 },
    /// A wall or circuit breaker blocked one of its actions.
    Blocked { reason: String },
    /// The peripheral filter decided it should look now.
    Filter { reason: String },
    /// The `next_look` it chose when it last went to sleep.
    NextLook,
}

impl WakeReason {
    /// Returns whether this reason may interrupt a wake already in progress.
    pub fn urgency(&self) -> Urgency {
        match self {
            WakeReason::Workspace { urgent, .. } if *urgent => Urgency::Now,
            WakeReason::Claim { importance, .. } if *importance >= 8 => Urgency::Now,
            WakeReason::Blocked { .. } => Urgency::Now,
            _ => Urgency::Queued,
        }
    }

    /// Whether an idle agent can leave this until its next look instead of
    /// waking for it. Only what ARCHITECTURE.md lists under "Waits for its
    /// next look" can: optional Workspace replies and low-importance news.
    /// Triggers, follow-ups, finished workers and the filter's "look now"
    /// wake it, though they don't interrupt a wake already running.
    pub fn can_wait_for_next_look(&self) -> bool {
        match self {
            WakeReason::Workspace { urgent, .. } => !urgent,
            WakeReason::Claim { importance, .. } => *importance < 8,
            _ => false,
        }
    }

    /// One line for the wake message, saying why the agent is awake.
    pub fn describe(&self) -> String {
        match self {
            WakeReason::Workspace { summary, .. } => summary.clone(),
            WakeReason::Trigger { name } => format!("the trigger {name} fired"),
            WakeReason::FollowUp { note, .. } => format!("your follow-up came due: {note}"),
            WakeReason::WorkerFinished { worker_id } => format!("worker {worker_id} finished"),
            WakeReason::Claim {
                claim_id,
                importance,
            } => format!("claim {claim_id} scored {importance} in your area"),
            WakeReason::Blocked { reason } => format!("one of your actions was blocked: {reason}"),
            WakeReason::Filter { reason } => reason.clone(),
            WakeReason::NextLook => "the time you chose to look again".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_urgent_events_interrupt() {
        let urgent_dm = WakeReason::Workspace {
            summary: "DM".into(),
            urgent: true,
        };
        let mention = WakeReason::Workspace {
            summary: "@mention".into(),
            urgent: false,
        };
        let big_claim = WakeReason::Claim {
            claim_id: "c1".into(),
            importance: 9,
        };
        let small_claim = WakeReason::Claim {
            claim_id: "c2".into(),
            importance: 5,
        };
        assert_eq!(urgent_dm.urgency(), Urgency::Now);
        assert_eq!(mention.urgency(), Urgency::Queued);
        assert_eq!(big_claim.urgency(), Urgency::Now);
        assert_eq!(small_claim.urgency(), Urgency::Queued);
        assert_eq!(WakeReason::NextLook.urgency(), Urgency::Queued);
    }

    #[test]
    fn only_optional_news_waits_for_the_next_look() {
        let mention = WakeReason::Workspace {
            summary: "optional group reply".into(),
            urgent: false,
        };
        let small_claim = WakeReason::Claim {
            claim_id: "c2".into(),
            importance: 5,
        };
        assert!(mention.can_wait_for_next_look());
        assert!(small_claim.can_wait_for_next_look());
        for wakes in [
            WakeReason::Trigger { name: "ci".into() },
            WakeReason::FollowUp {
                schedule_id: "s".into(),
                note: "n".into(),
            },
            WakeReason::WorkerFinished {
                worker_id: "w".into(),
            },
            WakeReason::Filter {
                reason: "look".into(),
            },
            WakeReason::Workspace {
                summary: "DM".into(),
                urgent: true,
            },
        ] {
            assert!(!wakes.can_wait_for_next_look(), "{wakes:?}");
        }
    }
}
