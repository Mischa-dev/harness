//! Runs a scenario: the fake clock jumps from one moment that matters to the
//! next (a message addressed to the agent, the look it chose when it went to
//! sleep, or the wake filter's next look at the news), and the agent wakes
//! there inside its workday, as it would under the daemon. Each wake opens
//! with the daemon's state view.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use harness_core::{Clock, FakeClock, WakeReason};
use harness_loop::filter::{self, FilterAgent, FilterInput};
use harness_loop::state::{Budget, StateFacts, WorkspaceCounts};
use harness_loop::{AfterWake, CharterValues, ContextAssembler, Rhythm, WakeLimits, Workday};
use harness_providers::{CapabilityProfile, ContentBlock, Message, Provider};
use harness_store::{ScheduleBook, Store, StoreError, WorkdayPolicy};
use harness_tools::memory::MemoryFiles;
use harness_tools::{Policy, PolicyExecutor, Schedule, SleepTool};
use time::{OffsetDateTime, Time, UtcOffset};

use crate::report::{FilterStats, Report};
use crate::scenario::{Event, Scenario};
use crate::world::{MessageSend, Outbox, wake_message};

/// A safety net against a runaway agent or a bug in the runner.
const MAX_WAKES: usize = 2_000;
/// The same, for the wake filter's looks.
const MAX_FILTER_LOOKS: usize = 20_000;

/// The owner the charter names, and whose time the state view shows.
const OWNER: &str = "Mischa";
/// The daemon's default model spend limit for a day.
const SPEND_PER_DAY_USD: f64 = 15.0;

/// The wake filter's instructions, as the daemon gives them.
const PERIPHERAL: &str = include_str!("../../../prompts/peripheral.md");

/// The helper model that runs the wake filter.
pub struct Helper<'a> {
    pub provider: &'a dyn Provider,
    pub profile: &'a CapabilityProfile,
    pub profile_name: &'a str,
}

/// The real charter, so a real-model run is judged on what the agent will
/// actually be told.
const CHARTER: &str = include_str!("../../../prompts/teammate-charter.md");

fn static_prefix(scenario: &Scenario) -> String {
    CharterValues {
        agent_name: scenario.agent.clone(),
        owner_name: OWNER.into(),
        responsibilities: vec![],
    }
    .render(CHARTER)
    .expect("the charter's placeholders are all filled")
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("couldn't create the agent's working area: {0}")]
    Workdir(#[from] std::io::Error),
    #[error("message outbox is unavailable")]
    OutboxUnavailable,
    #[error("prompts/peripheral.md: {0}")]
    Prompt(String),
}

/// Runs `scenario` through the real workday, wake loop, store, and policy
/// executor, with the wake filter on `helper` if there is one, then returns
/// its scored report.
pub async fn run(
    scenario: &Scenario,
    provider: &dyn Provider,
    profile: &CapabilityProfile,
    profile_name: &str,
    helper: Option<Helper<'_>>,
) -> Result<Report, RunError> {
    let filter_agent = FilterAgent {
        name: scenario.agent.clone(),
        owner: OWNER.into(),
        responsibilities: vec![],
    };
    let filter_system = match &helper {
        Some(_) => filter::system_prompt(PERIPHERAL, &filter_agent).map_err(RunError::Prompt)?,
        None => String::new(),
    };
    let clock = Arc::new(FakeClock::new(scenario.start));
    let workdir = tempfile::tempdir()?;
    let state_dir = tempfile::tempdir()?;
    let outbox: Outbox = Arc::new(Mutex::new(Vec::new()));
    let mut executor = PolicyExecutor::new(Policy::conservative(workdir.path()), clock.clone())
        .with_tool(SleepTool)
        .with_tool(MessageSend {
            outbox: outbox.clone(),
        });
    // Memory as the daemon wires it: the tools, and a snapshot that each
    // new day starts from.
    let policy = WorkdayPolicy::default();
    let memory = Arc::new(MemoryFiles::open(
        workdir.path().join("memory"),
        Some(state_dir.path().join("memory.git")),
        policy.offset,
        scenario.start,
    )?);
    for tool in harness_tools::memory::tools(memory.clone()) {
        executor.register(tool);
    }
    for tool in harness_tools::files::tools(Policy::conservative(workdir.path())) {
        executor.register(tool);
    }
    let schedule = Arc::new(Schedule {
        book: Arc::new(ScheduleBook::open_in_memory()?),
        offset: policy.offset,
    });
    for tool in harness_tools::schedule::tools(schedule.clone()) {
        executor.register(tool);
    }
    let mut store = Store::open_in_memory()?;
    let mut assembler = ContextAssembler {
        profile: profile_name.into(),
        static_prefix: static_prefix(scenario),
        memory_snapshot: String::new(),
        max_output_tokens: 4096,
    };
    let mut rhythm = Rhythm::new(policy, scenario.start);

    let events = &scenario.events;
    let mut delivered = 0;
    // News the agent hasn't seen yet: it arrived, and the filter kept it.
    let mut waiting: Vec<&Event> = Vec::new();
    let mut days = BTreeSet::new();
    let mut errors = Vec::new();
    let mut wakes = 0;
    let mut stats = FilterStats::default();
    let mut last_filter: Option<OffsetDateTime> = None;

    loop {
        let next_addressed = events[delivered..]
            .iter()
            .find(|e| e.addressed)
            .map(|e| e.at);
        let next_look = match schedule.book.next_due()? {
            Some(due) => due.min(rhythm.next_look()),
            None => rhythm.next_look(),
        };
        // The filter looks when news arrives, at most every FILTER_GAP.
        let next_filter = helper
            .as_ref()
            .and(events.get(delivered))
            .map(|e| last_filter.map_or(e.at, |last| e.at.max(last + filter::FILTER_GAP)));
        let at = [next_addressed, next_filter]
            .into_iter()
            .flatten()
            .fold(next_look, OffsetDateTime::min);
        if at >= scenario.end {
            break;
        }
        if wakes == MAX_WAKES || stats.looks == MAX_FILTER_LOOKS {
            errors.push(format!(
                "stopped after {wakes} wakes and {} filter looks at {at}, before the \
                 scenario's end",
                stats.looks
            ));
            break;
        }
        clock.set(at);

        let arrived = events[delivered..]
            .iter()
            .take_while(|e| e.at <= at)
            .collect::<Vec<_>>();
        delivered += arrived.len();
        let for_agent = arrived.iter().filter(|e| e.addressed).count();
        let urgent = arrived
            .iter()
            .any(|e| e.addressed && e.conversation == "dm");
        waiting.extend(arrived);

        // Nothing addressed and no look due: only the filter looks.
        let mut filter_woke = None;
        if for_agent == 0 && at < next_look {
            let Some(helper) = &helper else {
                continue;
            };
            last_filter = Some(at);
            stats.looks += 1;
            let input = FilterInput {
                agent: filter_agent.clone(),
                watching_for: filter::watching_for(&store)?,
                next_look: rhythm.next_look(),
                events: waiting.iter().map(|e| event_line(e)).collect(),
                replies: vec![],
            };
            match filter::look(helper.provider, &input, helper.profile_name, &filter_system).await {
                Ok((decision, usage)) => {
                    let cost = helper.profile.cost_usd(&usage);
                    store.record_helper_call(at, "wake_filter", cost)?;
                    stats.cost_usd += cost;
                    let before = waiting.len();
                    decision.keep_only(&mut waiting);
                    stats.dropped += before - waiting.len();
                    if !decision.wake_now {
                        continue;
                    }
                    stats.early_wakes += 1;
                    filter_woke = Some(decision.wake_reason());
                }
                Err(e) => {
                    errors.push(format!("wake filter at {at}: {e}"));
                    continue;
                }
            }
        }

        rhythm.close_day_if_due(&store, at)?;
        if store.current_day()?.is_none() {
            assembler.memory_snapshot = memory.snapshot()?;
        }
        let due = schedule.book.take_due(at, schedule.offset)?;
        let reason = if for_agent > 0 {
            WakeReason::Workspace {
                summary: format!(
                    "{for_agent} new message{} for you",
                    if for_agent == 1 { "" } else { "s" }
                ),
                urgent,
            }
        } else if let Some(first) = due.first() {
            WakeReason::FollowUp {
                schedule_id: first.id.to_string(),
                note: first.note.clone(),
            }
        } else if let Some(reason) = filter_woke {
            reason
        } else {
            WakeReason::NextLook
        };
        let mut message = wake_message(at, &reason, &waiting);
        if !due.is_empty() {
            message.push_str("\nCame due on your schedule:");
            for entry in &due {
                let line = harness_tools::schedule::describe(entry, schedule.offset);
                message.push_str(&format!("\n- {line}"));
            }
        }
        let state = StateFacts {
            at,
            offset: schedule.offset,
            owner: Some(OWNER.into()),
            workspace: Some(Ok(workspace_counts(&waiting))),
            wakes_today: match store.current_day()? {
                Some(day) => store.wakes_since(day.started_at)?.len(),
                None => 0,
            },
            budget: Budget {
                limit_usd: SPEND_PER_DAY_USD,
                roles: store.spend_by_role(local_midnight(at, schedule.offset))?,
            },
            schedule: schedule
                .book
                .list()?
                .iter()
                .map(|entry| harness_tools::schedule::describe(entry, schedule.offset))
                .collect(),
            workers: None,
            shadow: false,
        };
        message.push_str("\n\n");
        message.push_str(&state.render());
        waiting.clear();

        let result = Workday {
            store: &mut store,
            provider,
            executor: &mut executor,
            assembler: &assembler,
            profile,
            clock: clock.as_ref(),
            limits: WakeLimits::default(),
        }
        .wake(&reason, Message::user_text(message))
        .await;
        wakes += 1;
        if let Some(day) = store.current_day()? {
            days.insert(day.id);
        }

        match rhythm.after(&result, clock.now()) {
            AfterWake::Slept => {}
            AfterWake::Paused { .. } => break,
            AfterWake::Fallback { problem } => errors.push(format!("wake at {at}: {problem}")),
        }
    }

    let records = store.wakes_since(scenario.start)?;
    let mut blocked = Vec::new();
    for day in &days {
        for message in store.transcript(*day)? {
            for block in message.content {
                if let ContentBlock::ToolResult(result) = block
                    && result.is_error
                    && result.content.starts_with("blocked")
                {
                    blocked.push(result.content);
                }
            }
        }
    }
    let sent = outbox
        .lock()
        .map_err(|_| RunError::OutboxUnavailable)?
        .clone();
    let mut report = Report::new(
        scenario,
        records,
        sent,
        blocked,
        errors,
        rhythm.paused().map(str::to_string),
        days.len(),
    );
    report.filter = helper.map(|_| stats);
    Ok(report)
}

/// The filter's view of an event: who said what, where, on one line.
fn event_line(event: &Event) -> String {
    let text = event.text.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("[{}] {}: {text}", event.conversation, event.from)
}

/// What the state view's Workspace line would count for the news a wake
/// brings: what's addressed to the agent asks for a reply, and each
/// conversation with news has unread messages.
fn workspace_counts(news: &[&Event]) -> WorkspaceCounts {
    let addressed = news.iter().filter(|e| e.addressed).count();
    let mut conversations: Vec<&str> = news.iter().map(|e| e.conversation.as_str()).collect();
    conversations.sort_unstable();
    conversations.dedup();
    WorkspaceCounts {
        reply_requests: addressed,
        required: addressed,
        mentions: news
            .iter()
            .filter(|e| e.addressed && e.conversation != "dm")
            .count(),
        unread: conversations.len(),
        ..WorkspaceCounts::default()
    }
}

fn local_midnight(at: OffsetDateTime, offset: UtcOffset) -> OffsetDateTime {
    at.to_offset(offset).replace_time(Time::MIDNIGHT)
}
