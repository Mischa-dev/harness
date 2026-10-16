//! `schedule` and `unschedule`: the agent's own clock, beyond `sleep`. A
//! follow-up wakes it once with its note; a routine wakes it every day,
//! weekday or week at the same time (ARCHITECTURE.md, "Its own clock").

use std::sync::Arc;

use async_trait::async_trait;
use harness_providers::ToolSpec;
use harness_store::{Repeat, ScheduleBook, Scheduled};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime, UtcOffset};

use crate::tool::{Effect, Tool, ToolContext, ToolError, ToolOutput};

/// How many follow-ups and routines it can keep at once.
pub const MAX_SCHEDULED: usize = 30;
/// How many of those can be routines: the routines circuit breaker
/// (ARCHITECTURE.md), enforced here as a refusal.
pub const MAX_ROUTINES: usize = 20;
/// One note's limit, in characters.
pub const MAX_NOTE_CHARS: usize = 300;
/// How far ahead it can schedule.
pub const HORIZON: Duration = Duration::days(366);

/// The schedule, shared by both tools and the wake loop.
pub struct Schedule {
    pub book: Arc<ScheduleBook>,
    /// The owner's offset: routines keep the owner's days.
    pub offset: UtcOffset,
}

pub fn tools(schedule: Arc<Schedule>) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ScheduleTool(schedule.clone())),
        Box::new(UnscheduleTool(schedule)),
    ]
}

/// One line for an entry, as the tools and the state view show it.
pub fn describe(entry: &Scheduled, offset: UtcOffset) -> String {
    let when = when(entry.due_at, offset);
    let repeat = match entry.repeat {
        None => String::new(),
        Some(Repeat::Daily) => ", then every day".into(),
        Some(Repeat::Weekdays) => ", then every weekday".into(),
        Some(Repeat::Weekly) => ", then every week".into(),
    };
    format!("#{} at {when}{repeat}: {}", entry.id, entry.note)
}

fn when(at: OffsetDateTime, offset: UtcOffset) -> String {
    let local = at.to_offset(offset);
    let weekday = &local.weekday().to_string()[..3];
    let time = local.format(&Rfc3339).unwrap_or_else(|_| local.to_string());
    format!("{weekday} {time}")
}

struct ScheduleTool(Arc<Schedule>);

/// What a `schedule` call asks for, checked before anything is stored.
struct Request {
    note: String,
    due_at: OffsetDateTime,
    repeat: Option<Repeat>,
}

impl ScheduleTool {
    fn request(&self, args: &Value, now: OffsetDateTime) -> Result<Request, ToolError> {
        let invalid = |why: &str| ToolError::InvalidArguments(why.into());
        // One line, so a note can't pose as more of the state view.
        let note = args
            .get("note")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if note.is_empty() {
            return Err(invalid("note is empty"));
        }
        if note.chars().count() > MAX_NOTE_CHARS {
            return Err(invalid(&format!(
                "note is longer than {MAX_NOTE_CHARS} characters"
            )));
        }
        let repeat = match args.get("repeat").and_then(Value::as_str) {
            None => None,
            Some(text) => Some(
                Repeat::parse(text)
                    .ok_or_else(|| invalid("repeat must be daily, weekdays or weekly"))?,
            ),
        };
        let due_at = match (
            args.get("at").and_then(Value::as_str),
            args.get("in_minutes").and_then(Value::as_i64),
        ) {
            (Some(at), None) => OffsetDateTime::parse(at, &Rfc3339).map_err(|_| {
                invalid(
                    "at must be an RFC 3339 time with an offset, like 2026-10-23T15:00:00+02:00",
                )
            })?,
            (None, Some(minutes)) if (1..=HORIZON.whole_minutes()).contains(&minutes) => {
                now + Duration::minutes(minutes)
            }
            (None, Some(_)) => return Err(invalid("in_minutes must be from 1 to a year")),
            _ => return Err(invalid("give exactly one of at and in_minutes")),
        };
        if due_at <= now {
            return Err(invalid("that time has already passed"));
        }
        if due_at > now + HORIZON {
            return Err(invalid("that's more than a year away"));
        }
        let due_at = repeat.map_or(due_at, |r| r.settle(due_at, self.0.offset));
        Ok(Request {
            note,
            due_at,
            repeat,
        })
    }
}

fn failed(e: harness_store::StoreError) -> ToolError {
    ToolError::Failed(format!("the schedule couldn't be saved: {e}"))
}

#[async_trait]
impl Tool for ScheduleTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "schedule".into(),
            description: "Set yourself a follow-up or a routine. A follow-up wakes you once, \
                          with your note, at the time you give. A routine (with repeat) wakes \
                          you at that time every day, weekday or week, until you unschedule it. \
                          Use it to keep your word on a follow-up you promised, or for a habit \
                          you want."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "note": {"type": "string", "description": "What to do or check then, for your future self"},
                    "at": {"type": "string", "description": "When, as an RFC 3339 time with an offset"},
                    "in_minutes": {"type": "integer", "description": "When, in minutes from now, instead of at"},
                    "repeat": {"type": "string", "enum": ["daily", "weekdays", "weekly"], "description": "Makes it a routine"}
                },
                "required": ["note"]
            }),
        }
    }

    fn effects(&self, _args: &Value) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![Effect::Action {
            name: "schedule".into(),
        }])
    }

    async fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let now = ctx.clock.now();
        let request = self.request(args, now)?;
        let book = &self.0.book;
        let scheduled = book.list().map_err(failed)?;
        if scheduled.len() >= MAX_SCHEDULED {
            return Err(ToolError::Failed(format!(
                "you already have {MAX_SCHEDULED} things scheduled; unschedule one first"
            )));
        }
        let routines = scheduled.iter().filter(|e| e.repeat.is_some()).count();
        if request.repeat.is_some() && routines >= MAX_ROUTINES {
            return Err(ToolError::Failed(format!(
                "you already have {MAX_ROUTINES} routines; unschedule one first"
            )));
        }
        let entry = book
            .add(&request.note, request.due_at, request.repeat, now)
            .map_err(failed)?;
        Ok(ToolOutput::text(format!(
            "Scheduled {}",
            describe(&entry, self.0.offset)
        )))
    }
}

struct UnscheduleTool(Arc<Schedule>);

#[async_trait]
impl Tool for UnscheduleTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "unschedule".into(),
            description: "Cancel a follow-up or routine by its number.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "The number schedule gave it, like 3 for #3"}
                },
                "required": ["id"]
            }),
        }
    }

    fn effects(&self, _args: &Value) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![Effect::Action {
            name: "schedule".into(),
        }])
    }

    async fn run(&self, args: &Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let id = args
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| ToolError::InvalidArguments("id must be an integer".into()))?;
        match self.0.book.remove(id).map_err(failed)? {
            Some(entry) => Ok(ToolOutput::text(format!(
                "Unscheduled {}",
                describe(&entry, self.0.offset)
            ))),
            None => Err(ToolError::Failed(format!("nothing is scheduled as #{id}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::FakeClock;
    use time::macros::{datetime, offset};

    fn schedule() -> (Arc<Schedule>, FakeClock) {
        let schedule = Arc::new(Schedule {
            book: Arc::new(ScheduleBook::open_in_memory().unwrap()),
            offset: offset!(+2),
        });
        (schedule, FakeClock::new(datetime!(2026-10-16 09:00 UTC)))
    }

    async fn call(tool: &dyn Tool, args: Value, clock: &FakeClock) -> Result<String, ToolError> {
        tool.run(&args, &ToolContext { clock })
            .await
            .map(|out| out.content)
    }

    #[tokio::test]
    async fn follow_ups_and_routines_are_kept_and_cancelled() {
        let (schedule, clock) = schedule();
        let [set, unset] = <[Box<dyn Tool>; 2]>::try_from(tools(schedule.clone()))
            .ok()
            .unwrap();
        let shown = call(
            set.as_ref(),
            json!({"note": "did Mara\nlook at #42?", "in_minutes": 90}),
            &clock,
        )
        .await
        .unwrap();
        assert_eq!(
            shown,
            "Scheduled #1 at Fri 2026-10-16T12:30:00+02:00: did Mara look at #42?"
        );
        // Saturday at the owner's offset moves to Monday.
        let shown = call(
            set.as_ref(),
            json!({"note": "check CI", "at": "2026-10-17T09:00:00+02:00", "repeat": "weekdays"}),
            &clock,
        )
        .await
        .unwrap();
        assert_eq!(
            shown,
            "Scheduled #2 at Mon 2026-10-19T09:00:00+02:00, then every weekday: check CI"
        );
        assert_eq!(schedule.book.list().unwrap().len(), 2);
        let gone = call(unset.as_ref(), json!({"id": 1}), &clock)
            .await
            .unwrap();
        assert!(gone.starts_with("Unscheduled #1"), "{gone}");
        assert_eq!(
            call(unset.as_ref(), json!({"id": 1}), &clock).await,
            Err(ToolError::Failed("nothing is scheduled as #1".into()))
        );
    }

    #[tokio::test]
    async fn bad_times_and_notes_are_refused() {
        let (schedule, clock) = schedule();
        let [set, _] = <[Box<dyn Tool>; 2]>::try_from(tools(schedule.clone()))
            .ok()
            .unwrap();
        for args in [
            json!({"note": "  ", "in_minutes": 5}),
            json!({"note": "x".repeat(MAX_NOTE_CHARS + 1), "in_minutes": 5}),
            json!({"note": "x"}),
            json!({"note": "x", "in_minutes": 5, "at": "2026-10-17T09:00:00Z"}),
            json!({"note": "x", "in_minutes": 0}),
            json!({"note": "x", "in_minutes": i64::MAX}),
            json!({"note": "x", "at": "tomorrow"}),
            json!({"note": "x", "at": "2026-10-16T08:00:00Z"}),
            json!({"note": "x", "at": "2028-01-01T00:00:00Z"}),
            json!({"note": "x", "in_minutes": 5, "repeat": "hourly"}),
        ] {
            let result = call(set.as_ref(), args.clone(), &clock).await;
            assert!(
                matches!(result, Err(ToolError::InvalidArguments(_))),
                "{args}: {result:?}"
            );
        }
        assert!(schedule.book.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_schedule_has_a_cap() {
        let (schedule, clock) = schedule();
        let [set, _] = <[Box<dyn Tool>; 2]>::try_from(tools(schedule.clone()))
            .ok()
            .unwrap();
        for _ in 0..MAX_SCHEDULED {
            call(set.as_ref(), json!({"note": "x", "in_minutes": 5}), &clock)
                .await
                .unwrap();
        }
        let full = call(set.as_ref(), json!({"note": "x", "in_minutes": 5}), &clock).await;
        assert!(matches!(full, Err(ToolError::Failed(_))), "{full:?}");
    }

    #[tokio::test]
    async fn routines_have_a_lower_cap() {
        let (schedule, clock) = schedule();
        let [set, _] = <[Box<dyn Tool>; 2]>::try_from(tools(schedule.clone()))
            .ok()
            .unwrap();
        let routine = json!({"note": "x", "in_minutes": 5, "repeat": "daily"});
        for _ in 0..MAX_ROUTINES {
            call(set.as_ref(), routine.clone(), &clock).await.unwrap();
        }
        let full = call(set.as_ref(), routine, &clock).await;
        assert!(
            matches!(&full, Err(ToolError::Failed(why)) if why.contains("routines")),
            "{full:?}"
        );
        // A one-off follow-up still fits.
        call(set.as_ref(), json!({"note": "x", "in_minutes": 5}), &clock)
            .await
            .unwrap();
    }

    #[test]
    fn both_tools_declare_a_schedule_action() {
        let (schedule, _) = schedule();
        for tool in tools(schedule) {
            assert_eq!(
                tool.effects(&json!({})).unwrap(),
                vec![Effect::Action {
                    name: "schedule".into()
                }]
            );
        }
    }
}
