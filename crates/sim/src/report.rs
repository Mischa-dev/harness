//! What a run did, and whether it passed. Hard checks and counts only; the
//! rubric graded by a separate model comes with real-model runs.

use std::fmt;

use harness_store::WakeRecord;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::scenario::Scenario;
use crate::world::Sent;

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub scenario: String,
    pub wakes: usize,
    pub turns: u64,
    pub days: usize,
    pub cost_usd: f64,
    pub sent: Vec<Sent>,
    pub checks: Vec<Check>,
}

impl Report {
    pub fn new(
        scenario: &Scenario,
        wakes: Vec<WakeRecord>,
        sent: Vec<Sent>,
        blocked: Vec<String>,
        errors: Vec<String>,
        paused: Option<String>,
        days: usize,
    ) -> Self {
        let mut checks = Vec::new();
        let mut check = |name: &str, passed: bool, detail: String| {
            checks.push(Check {
                name: name.into(),
                passed,
                detail,
            });
        };

        let unanswered: Vec<String> = scenario
            .events
            .iter()
            .filter(|e| e.addressed)
            .filter(|e| {
                !sent
                    .iter()
                    .any(|s| s.conversation == e.conversation && s.at >= e.at)
            })
            .map(|e| format!("{} in {} at {}", e.from, e.conversation, stamp(e.at)))
            .collect();
        check(
            "answers what is addressed to it",
            unanswered.is_empty(),
            if unanswered.is_empty() {
                "every DM and mention got a reply".into()
            } else {
                format!("no reply to: {}", unanswered.join("; "))
            },
        );
        check(
            "hits no walls",
            blocked.is_empty(),
            if blocked.is_empty() {
                "nothing blocked".into()
            } else {
                blocked.join("; ")
            },
        );
        check(
            "never pauses",
            paused.is_none(),
            paused.map_or("no breaker tripped".into(), |b| {
                format!("breaker {b} tripped")
            }),
        );
        check(
            "no failed wakes",
            errors.is_empty(),
            if errors.is_empty() {
                "every wake finished".into()
            } else {
                errors.join("; ")
            },
        );
        if let Some(max) = scenario.expect.max_messages {
            check(
                "stays quiet",
                sent.len() <= max,
                format!(
                    "{} sent, at most {max} allowed",
                    count(sent.len(), "message")
                ),
            );
        }
        if let Some(max) = scenario.expect.max_wakes {
            check(
                "wakes sparingly",
                wakes.len() <= max,
                format!("{}, at most {max} allowed", count(wakes.len(), "wake")),
            );
        }

        Self {
            scenario: scenario.name.clone(),
            turns: wakes.iter().map(|w| u64::from(w.turns)).sum(),
            cost_usd: wakes.iter().map(|w| w.cost_usd).sum(),
            wakes: wakes.len(),
            days,
            sent,
            checks,
        }
    }

    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{}: {}",
            self.scenario,
            if self.passed() { "PASS" } else { "FAIL" }
        )?;
        writeln!(
            f,
            "  {}, {}, {}, {}, ${:.4}",
            count(self.wakes, "wake"),
            count(self.turns as usize, "turn"),
            count(self.days, "workday"),
            count(self.sent.len(), "message"),
            self.cost_usd
        )?;
        for c in &self.checks {
            writeln!(
                f,
                "  [{}] {}: {}",
                if c.passed { "ok" } else { "FAIL" },
                c.name,
                c.detail
            )?;
        }
        for s in &self.sent {
            writeln!(f, "  sent {} [{}] {}", stamp(s.at), s.conversation, s.text)?;
        }
        Ok(())
    }
}

fn stamp(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap_or_else(|_| at.to_string())
}

fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}
