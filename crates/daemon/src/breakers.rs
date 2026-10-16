//! Circuit breakers the daemon keeps itself, on top of the executor's
//! message breaker: wakes in an hour and model spend in a day. They're set
//! high on purpose (ARCHITECTURE.md, "Circuit breakers"): if one trips,
//! something is probably broken, so the agent pauses until its owner
//! resumes it, and the owner hears why.

use serde::Deserialize;
use time::{Duration, OffsetDateTime, Time};

use crate::agent::Agent;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BreakerConfig {
    /// Messages the agent posts in any hour.
    #[serde(default = "default_messages_per_hour")]
    pub messages_per_hour: usize,
    /// Wakes in any hour.
    #[serde(default = "default_wakes_per_hour")]
    pub wakes_per_hour: usize,
    /// Model spend in the owner's day, in US dollars, counting the wake
    /// filter's.
    #[serde(default = "default_spend_per_day_usd")]
    pub spend_per_day_usd: f64,
    /// A Workspace conversation id, such as the owner's direct message with
    /// the agent, where the runtime says when a breaker pauses it.
    #[serde(default)]
    pub notify: Option<String>,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            messages_per_hour: default_messages_per_hour(),
            wakes_per_hour: default_wakes_per_hour(),
            spend_per_day_usd: default_spend_per_day_usd(),
            notify: None,
        }
    }
}

fn default_messages_per_hour() -> usize {
    30
}

fn default_wakes_per_hour() -> usize {
    30
}

fn default_spend_per_day_usd() -> f64 {
    15.0
}

impl BreakerConfig {
    pub fn check(&self) -> Result<(), String> {
        if self.messages_per_hour == 0 || self.wakes_per_hour == 0 {
            return Err("breakers: limits must be at least 1".into());
        }
        if !(self.spend_per_day_usd.is_finite() && self.spend_per_day_usd > 0.0) {
            return Err("breakers.spend_per_day_usd must be a positive amount".into());
        }
        if let Some(id) = &self.notify {
            let ok = !id.is_empty()
                && id.len() <= 64
                && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
            if !ok {
                return Err("breakers.notify must be a Workspace conversation id".into());
            }
        }
        Ok(())
    }
}

/// A breaker that tripped, and what it saw.
#[derive(Debug, Clone, PartialEq)]
pub struct Tripped {
    pub breaker: String,
    pub detail: String,
}

impl Agent {
    /// Checks the daemon's breakers before a wake at `now`.
    pub fn check_breakers(
        &self,
        now: OffsetDateTime,
    ) -> Result<Option<Tripped>, harness_store::StoreError> {
        let wakes = self.store.wakes_since(now - Duration::HOUR)?.len();
        if wakes >= self.breakers.wakes_per_hour {
            return Ok(Some(Tripped {
                breaker: "wakes_per_hour".into(),
                detail: format!("{wakes} wakes in the last hour"),
            }));
        }
        let spend = self.store.spend_since(self.local_midnight(now))?;
        let limit = self.breakers.spend_per_day_usd;
        if spend >= limit {
            return Ok(Some(Tripped {
                breaker: "spend_per_day".into(),
                detail: format!(
                    "${spend:.2} of model spend today, at or over the ${limit:.2} limit"
                ),
            }));
        }
        Ok(None)
    }

    /// Pauses the agent for a breaker the daemon tripped: saved, so a
    /// restart keeps it, until the owner resumes it.
    pub async fn trip(&mut self, tripped: Tripped) -> Result<(), harness_store::StoreError> {
        self.store.set_paused(Some(&tripped.breaker))?;
        self.rhythm.pause(&tripped.breaker);
        self.executor.pause(&tripped.breaker);
        self.tell_owner_paused(&tripped.breaker, &tripped.detail)
            .await;
        Ok(())
    }

    /// Tells the owner the agent paused, where the config says to.
    pub async fn tell_owner_paused(&self, breaker: &str, detail: &str) {
        eprintln!("agent paused by the {breaker} breaker: {detail}");
        let (Some(conversation), Some(workspace)) = (&self.breakers.notify, &self.workspace) else {
            return;
        };
        let text = format!(
            "I've paused myself: the {breaker} circuit breaker tripped ({detail}). That usually \
             means something is broken, like a loop. Nothing runs until you resume me with \
             `harness-daemon resume`."
        );
        if let Err(e) = workspace.post(conversation, &text).await {
            eprintln!("harness-daemon: couldn't tell the owner about the pause: {e}");
        }
    }

    fn local_midnight(&self, now: OffsetDateTime) -> OffsetDateTime {
        now.to_offset(self.offset).replace_time(Time::MIDNIGHT)
    }
}
