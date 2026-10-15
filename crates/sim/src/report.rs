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

        // A reply answers a message when it comes before the next message
        // addressed to the agent in the same conversation, so one late reply
        // can't cover an earlier message it ignored. Messages that arrive at
        // the same moment share a reply.
        let addressed: Vec<_> = scenario.events.iter().filter(|e| e.addressed).collect();
        let unanswered: Vec<String> = addressed
            .iter()
            .filter(|e| {
                let next = addressed
                    .iter()
                    .filter(|n| n.conversation == e.conversation && n.at > e.at)
                    .map(|n| n.at)
                    .min();
                !sent.iter().any(|s| {
                    s.conversation == e.conversation
                        && s.at >= e.at
                        && next.is_none_or(|next| s.at < next)
                })
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

        let cost_usd = wakes.iter().map(|w| w.cost_usd).sum::<f64>();
        // Iterator summation may produce negative zero for an empty run. Keep
        // machine-readable and human-readable reports free of that artifact.
        let cost_usd = if cost_usd == 0.0 { 0.0 } else { cost_usd };
        Self {
            scenario: scenario.name.clone(),
            turns: wakes.iter().map(|w| u64::from(w.turns)).sum(),
            cost_usd,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::{Event, Expect};
    use harness_providers::Usage;
    use time::Duration;
    use time::macros::datetime;

    fn scenario(events: Vec<Event>, expect: Expect) -> Scenario {
        Scenario {
            name: "coverage".into(),
            summary: "exercise report checks".into(),
            agent: "Mara".into(),
            start: datetime!(2026-01-05 09:00 UTC),
            end: datetime!(2026-01-06 09:00 UTC),
            events,
            expect,
        }
    }

    fn addressed(at: OffsetDateTime, conversation: &str, from: &str) -> Event {
        Event {
            at,
            conversation: conversation.into(),
            from: from.into(),
            text: "question".into(),
            addressed: true,
        }
    }

    fn sent(at: OffsetDateTime, conversation: &str) -> Sent {
        Sent {
            at,
            conversation: conversation.into(),
            text: "answer".into(),
        }
    }

    #[test]
    fn a_reply_cannot_cover_two_sequential_questions() {
        let start = datetime!(2026-01-05 09:00 UTC);
        let scenario = scenario(
            vec![
                addressed(start, "dm", "A"),
                addressed(start + Duration::hours(1), "dm", "B"),
            ],
            Expect::default(),
        );
        let report = Report::new(
            &scenario,
            vec![],
            vec![sent(start + Duration::hours(1), "dm")],
            vec![],
            vec![],
            None,
            1,
        );
        let answers = &report.checks[0];
        assert!(!answers.passed);
        assert!(answers.detail.contains("A in dm"));
        assert!(!answers.detail.contains("B in dm"));
    }

    #[test]
    fn one_reply_can_answer_messages_that_arrive_together() {
        let start = datetime!(2026-01-05 09:00 UTC);
        let scenario = scenario(
            vec![addressed(start, "dm", "A"), addressed(start, "dm", "B")],
            Expect::default(),
        );
        let report = Report::new(
            &scenario,
            vec![],
            vec![sent(start, "dm")],
            vec![],
            vec![],
            None,
            1,
        );
        assert!(report.checks[0].passed);
    }

    #[test]
    fn report_combines_usage_and_surfaces_every_failure_class() {
        let start = datetime!(2026-01-05 09:00 UTC);
        let scenario = scenario(
            vec![addressed(start, "dm", "Owner")],
            Expect {
                max_messages: Some(0),
                max_wakes: Some(0),
            },
        );
        let wake = WakeRecord {
            day_id: 1,
            reason: "next look".into(),
            started_at: start,
            ended_at: start,
            end: "paused".into(),
            turns: 3,
            usage: Usage::default(),
            cost_usd: 0.125,
        };
        let report = Report::new(
            &scenario,
            vec![
                wake.clone(),
                WakeRecord {
                    turns: 2,
                    cost_usd: 0.25,
                    ..wake
                },
            ],
            vec![sent(start, "#wrong")],
            vec!["blocked write".into()],
            vec!["provider unavailable".into()],
            Some("messages_per_hour".into()),
            2,
        );
        assert!(!report.passed());
        assert_eq!((report.wakes, report.turns, report.days), (2, 5, 2));
        assert!((report.cost_usd - 0.375).abs() < f64::EPSILON);
        assert_eq!(
            report.checks.iter().filter(|check| !check.passed).count(),
            6
        );
    }

    #[test]
    fn display_is_a_readable_summary_with_singular_counts() {
        let scenario = scenario(vec![], Expect::default());
        let report = Report::new(
            &scenario,
            vec![],
            vec![sent(scenario.start, "dm")],
            vec![],
            vec![],
            None,
            1,
        );
        let rendered = report.to_string();
        assert!(rendered.starts_with("coverage: PASS\n"));
        assert!(
            rendered.contains("0 wakes, 0 turns, 1 workday, 1 message, $0.0000"),
            "{rendered}"
        );
        assert!(rendered.contains("sent 2026-01-05T09:00:00Z [dm] answer"));
    }
}
