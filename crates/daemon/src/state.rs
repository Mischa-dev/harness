//! The state view: a few lines at the start of every wake saying what a good
//! teammate would weigh before deciding what to do. It's information, not a
//! gate; the agent decides what it means.
//!
//! Every line is built from counts and the runtime's own records, never from
//! what teammates wrote, so nothing in it can pose as an instruction.

use harness_workspace::BriefingCounts;
use time::format_description::FormatItem;
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

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
    pub workspace: Option<Result<BriefingCounts, String>>,
    /// Earlier wakes today, and what they cost.
    pub wakes_today: usize,
    pub spend_today: f64,
    pub shadow: bool,
}

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
        lines.push(format!(
            "- Today so far: {wakes} · ${:.2} of model spend",
            self.spend_today
        ));
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

fn workspace_line(counts: &BriefingCounts) -> String {
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
            workspace: Some(Ok(BriefingCounts {
                reply_requests: 2,
                required: 1,
                mentions: 1,
                unread: 0,
                tasks: 3,
                claims: 0,
                thread_updates: 1,
            })),
            wakes_today: 4,
            spend_today: 0.4234,
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
             - Today so far: 4 earlier wakes · $0.42 of model spend"
        );
    }

    #[test]
    fn quiet_broken_and_shadowed_views() {
        let quiet = StateFacts {
            workspace: Some(Ok(BriefingCounts::default())),
            wakes_today: 1,
            spend_today: 0.0,
            ..facts()
        };
        assert!(quiet.render().contains("- Workspace: nothing waiting\n"));
        assert!(quiet.render().contains("1 earlier wake ·"));

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
