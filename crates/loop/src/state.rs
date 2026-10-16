//! The state view: a few lines at the start of every wake saying what a good
//! teammate would weigh before deciding what to do. It's information, not a
//! gate; the agent decides what it means.
//!
//! Every line is built from counts and the runtime's own records, never from
//! what teammates wrote, so nothing in it can pose as an instruction.

use harness_store::RoleSpend;
use time::format_description::FormatItem;
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

/// What's waiting in Workspace, counted from the agent's briefing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkspaceCounts {
    pub reply_requests: usize,
    /// Of those, the ones that need an answer rather than allow a pass.
    pub required: usize,
    pub mentions: usize,
    /// Conversations with unread messages.
    pub unread: usize,
    pub tasks: usize,
    pub claims: usize,
    /// Threads whose status or outcome changed lately.
    pub thread_updates: usize,
}

const LOCAL_TIME: &[FormatItem<'static>] =
    format_description!("[weekday repr:short] [year]-[month]-[day] [hour]:[minute]");

/// What the view is built from, gathered at the start of a wake.
#[derive(Debug, Clone, PartialEq)]
pub struct StateFacts {
    pub at: OffsetDateTime,
    /// The owner's offset from UTC, for the time the view shows.
    pub offset: UtcOffset,
    pub owner: Option<String>,
    /// `None` without a Workspace connection; an error says why the
    /// briefing couldn't be read.
    pub workspace: Option<Result<WorkspaceCounts, String>>,
    /// Earlier wakes in today's workday.
    pub wakes_today: usize,
    pub budget: Budget,
    /// Its follow-ups and routines, soonest first, one line each.
    pub schedule: Vec<String>,
    pub shadow: bool,
}

/// Model spend in the owner's day, against the spend breaker's limit.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Budget {
    pub limit_usd: f64,
    pub roles: Vec<RoleSpend>,
}

impl Budget {
    fn line(&self) -> String {
        let spent = self.roles.iter().fold(0.0, |sum, r| sum + r.cost_usd);
        let mut line = format!(
            "- Budget: ${spent:.2} of today's ${:.2} model spend",
            self.limit_usd
        );
        if self.roles.len() > 1 {
            let parts: Vec<String> = self
                .roles
                .iter()
                .map(|r| format!("{} ${:.2}", role_name(&r.role), r.cost_usd))
                .collect();
            line.push_str(&format!(" ({})", parts.join(" · ")));
        }
        line
    }
}

/// The owner's spend report, for `harness-daemon spend`: today, in their
/// day, and the last seven days, each by role.
pub fn spend_report(
    store: &harness_store::Store,
    now: OffsetDateTime,
    offset: UtcOffset,
    limit_usd: f64,
) -> Result<String, harness_store::StoreError> {
    let midnight = now.to_offset(offset).replace_time(time::Time::MIDNIGHT);
    let mut lines = vec![];
    for (heading, since) in [
        (format!("Today, of your ${limit_usd:.2} limit"), midnight),
        ("The last 7 days".to_string(), now - time::Duration::days(7)),
    ] {
        let roles = store.spend_by_role(since)?;
        let total = roles.iter().fold(0.0, |sum, r| sum + r.cost_usd);
        lines.push(format!("{heading}: ${total:.2}"));
        for role in &roles {
            let calls = if role.role == "main" {
                plural(role.calls, "wake", "wakes")
            } else {
                plural(role.calls, "call", "calls")
            };
            lines.push(format!(
                "  {}: ${:.2} over {calls}",
                role_name(&role.role),
                role.cost_usd
            ));
        }
    }
    Ok(lines.join("\n"))
}

fn role_name(role: &str) -> String {
    match role {
        "main" => "main thread".into(),
        other => other.replace('_', " "),
    }
}

/// How much of the schedule the view lists.
const SCHEDULE_SHOWN: usize = 5;

impl StateFacts {
    pub fn render(&self) -> String {
        let local = self.at.to_offset(self.offset);
        let time = local
            .format(LOCAL_TIME)
            .unwrap_or_else(|_| local.to_string());
        let whose = match &self.owner {
            Some(owner) => format!("{owner}'s time"),
            None => "your owner's time".to_string(),
        };
        let mut lines = vec![format!("State, {time} ({whose})")];
        match &self.workspace {
            None => {}
            Some(Err(problem)) => lines.push(format!(
                "- Workspace: couldn't read the briefing ({problem})"
            )),
            Some(Ok(counts)) => lines.push(format!("- Workspace: {}", workspace_line(counts))),
        }
        let wakes = plural(self.wakes_today, "earlier wake", "earlier wakes");
        lines.push(format!("- Today so far: {wakes}"));
        lines.push(self.budget.line());
        match self.schedule.len() {
            0 => lines.push("- Your schedule: nothing set".into()),
            n => {
                lines.push(format!("- Your schedule ({n}):"));
                for entry in self.schedule.iter().take(SCHEDULE_SHOWN) {
                    lines.push(format!("  - {entry}"));
                }
                if n > SCHEDULE_SHOWN {
                    lines.push(format!("  - and {} more", n - SCHEDULE_SHOWN));
                }
            }
        }
        if self.shadow {
            lines.push(
                "- Shadow mode is on: what you say outside your live conversations goes to \
                 your shadow channel for your owner to read, not to the people there."
                    .into(),
            );
        }
        lines.join("\n")
    }
}

fn workspace_line(counts: &WorkspaceCounts) -> String {
    let mut parts = vec![];
    if counts.reply_requests > 0 {
        let mut part = plural(counts.reply_requests, "reply request", "reply requests");
        if counts.required > 0 {
            part.push_str(&format!(" ({} needing an answer)", counts.required));
        }
        parts.push(part);
    }
    for (count, one, many) in [
        (counts.mentions, "@mention", "@mentions"),
        (
            counts.unread,
            "conversation with unread messages",
            "conversations with unread messages",
        ),
        (counts.tasks, "task", "tasks"),
        (counts.claims, "claim", "claims"),
        (
            counts.thread_updates,
            "thread that changed lately",
            "threads that changed lately",
        ),
    ] {
        if count > 0 {
            parts.push(plural(count, one, many));
        }
    }
    if parts.is_empty() {
        "nothing waiting".to_string()
    } else {
        parts.join(" · ")
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::{datetime, offset};

    fn facts() -> StateFacts {
        StateFacts {
            at: datetime!(2026-10-16 22:24 UTC),
            offset: offset!(+2),
            owner: Some("Mischa".into()),
            workspace: Some(Ok(WorkspaceCounts {
                reply_requests: 2,
                required: 1,
                mentions: 1,
                unread: 0,
                tasks: 3,
                claims: 0,
                thread_updates: 1,
            })),
            wakes_today: 4,
            budget: Budget {
                limit_usd: 15.0,
                roles: vec![RoleSpend {
                    role: "main".into(),
                    calls: 4,
                    cost_usd: 0.4234,
                }],
            },
            schedule: vec![],
            shadow: false,
        }
    }

    #[test]
    fn the_view_says_what_is_waiting_and_what_today_cost() {
        assert_eq!(
            facts().render(),
            "State, Sat 2026-10-17 00:24 (Mischa's time)\n\
             - Workspace: 2 reply requests (1 needing an answer) · 1 @mention · 3 tasks · \
             1 thread that changed lately\n\
             - Today so far: 4 earlier wakes\n\
             - Budget: $0.42 of today's $15.00 model spend\n\
             - Your schedule: nothing set"
        );
    }

    #[test]
    fn the_budget_shows_each_role_once_there_are_several() {
        let role = |role: &str, cost_usd| RoleSpend {
            role: role.into(),
            calls: 1,
            cost_usd,
        };
        let budget = Budget {
            limit_usd: 15.0,
            roles: vec![role("main", 3.05), role("wake_filter", 0.051)],
        };
        assert_eq!(
            budget.line(),
            "- Budget: $3.10 of today's $15.00 model spend (main thread $3.05 · wake filter $0.05)"
        );
    }

    #[test]
    fn the_spend_report_counts_the_owners_day_and_the_week() {
        let store = harness_store::Store::open_in_memory().unwrap();
        let now = datetime!(2026-10-16 22:24 UTC);
        // 23:00 the day before at +2, so yesterday for the owner.
        store
            .record_helper_call(datetime!(2026-10-16 21:00 UTC), "wake_filter", 1.0)
            .unwrap();
        store
            .record_helper_call(datetime!(2026-10-16 22:10 UTC), "wake_filter", 0.25)
            .unwrap();
        assert_eq!(
            spend_report(&store, now, offset!(+2), 15.0).unwrap(),
            "Today, of your $15.00 limit: $0.25\n\
             \x20 wake filter: $0.25 over 1 call\n\
             The last 7 days: $1.25\n\
             \x20 wake filter: $1.25 over 2 calls"
        );
    }

    #[test]
    fn the_view_lists_the_soonest_of_the_schedule() {
        let busy = StateFacts {
            schedule: (1..=7)
                .map(|i| format!("#{i} at some time: check {i}"))
                .collect(),
            ..facts()
        };
        let shown = busy.render();
        assert!(shown.contains("- Your schedule (7):\n  - #1 at some time: check 1\n"));
        assert!(shown.contains("  - #5 at some time: check 5\n  - and 2 more"));
        assert!(!shown.contains("#6"));
    }

    #[test]
    fn quiet_broken_and_shadowed_views() {
        let quiet = StateFacts {
            workspace: Some(Ok(WorkspaceCounts::default())),
            wakes_today: 1,
            budget: Budget {
                limit_usd: 15.0,
                roles: vec![],
            },
            ..facts()
        };
        assert!(quiet.render().contains("- Workspace: nothing waiting\n"));
        assert!(quiet.render().contains("- Today so far: 1 earlier wake\n"));
        assert!(
            quiet
                .render()
                .contains("- Budget: $0.00 of today's $15.00 model spend\n")
        );

        let broken = StateFacts {
            workspace: Some(Err("timed out".into())),
            ..facts()
        };
        assert!(
            broken
                .render()
                .contains("- Workspace: couldn't read the briefing (timed out)")
        );

        let alone = StateFacts {
            workspace: None,
            owner: None,
            shadow: true,
            ..facts()
        };
        let shown = alone.render();
        assert!(!shown.contains("Workspace"), "{shown}");
        assert!(shown.contains("(your owner's time)"));
        assert!(shown.ends_with("not to the people there."));
    }
}
