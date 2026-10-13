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
    pub fn urgency(&self) -> Urgency {
        match self {
            WakeReason::Workspace { urgent, .. } if *urgent => Urgency::Now,
            WakeReason::Claim { importance, .. } if *importance >= 8 => Urgency::Now,
            WakeReason::Blocked { .. } => Urgency::Now,
            _ => Urgency::Queued,
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
}
