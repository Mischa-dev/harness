//! Runs a scenario: the fake clock jumps from one moment that matters to the
//! next (a message addressed to the agent, or the look it chose when it went
//! to sleep), and the agent wakes there inside its workday, as it would under
//! the daemon.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use harness_core::{Clock, FakeClock, WakeReason};
use harness_loop::{ContextAssembler, WakeEnd, WakeLimits, Workday};
use harness_providers::{CapabilityProfile, ContentBlock, Message, Provider};
use harness_store::{Store, StoreError, WorkdayPolicy};
use harness_tools::{Policy, PolicyExecutor, SleepTool};
use time::Duration;

use crate::report::Report;
use crate::scenario::Scenario;
use crate::world::{MessageSend, Outbox, wake_message};

/// A safety net against a runaway agent or a bug in the runner.
const MAX_WAKES: usize = 2_000;

/// When the agent ends a wake without choosing a next look, the runner picks
/// one, as the daemon will.
const DEFAULT_NEXT_LOOK: Duration = Duration::hours(1);

/// Placeholder until the charter lands in `prompts/` (M1).
fn static_prefix(scenario: &Scenario) -> String {
    format!(
        "You are {}, a teammate on a small software team. Answer what is addressed \
         to you, stay quiet otherwise, and end every wake by calling `sleep`.",
        scenario.agent
    )
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("couldn't create the agent's working area: {0}")]
    Workdir(#[from] std::io::Error),
}

pub async fn run(
    scenario: &Scenario,
    provider: &dyn Provider,
    profile: &CapabilityProfile,
    profile_name: &str,
) -> Result<Report, RunError> {
    let clock = Arc::new(FakeClock::new(scenario.start));
    let workdir = tempfile::tempdir()?;
    let outbox: Outbox = Arc::new(Mutex::new(Vec::new()));
    let mut executor = PolicyExecutor::new(Policy::conservative(workdir.path()), clock.clone())
        .with_tool(SleepTool)
        .with_tool(MessageSend {
            outbox: outbox.clone(),
        });
    let mut store = Store::open_in_memory()?;
    let assembler = ContextAssembler {
        profile: profile_name.into(),
        static_prefix: static_prefix(scenario),
        memory_snapshot: String::new(),
        max_output_tokens: 4096,
    };
    let policy = WorkdayPolicy::default();

    let events = &scenario.events;
    let mut delivered = 0;
    let mut next_look = scenario.start;
    let mut days = BTreeSet::new();
    let mut errors = Vec::new();
    let mut paused = None;
    let mut wakes = 0;

    loop {
        let next_addressed = events[delivered..]
            .iter()
            .find(|e| e.addressed)
            .map(|e| e.at);
        let at = next_addressed.map_or(next_look, |t| t.min(next_look));
        if at >= scenario.end {
            break;
        }
        if wakes == MAX_WAKES {
            errors.push(format!(
                "stopped after {MAX_WAKES} wakes at {at}, before the scenario's end"
            ));
            break;
        }
        clock.set(at);

        // The evening turn proper comes later; for now the day just ends.
        if let Some(day) = store.current_day()? {
            if policy.evening_due(&day, at).is_some() {
                store.end_day(day.id, at, None)?;
            }
        }

        let arrived = events[delivered..]
            .iter()
            .take_while(|e| e.at <= at)
            .collect::<Vec<_>>();
        delivered += arrived.len();
        let for_agent = arrived.iter().filter(|e| e.addressed).count();
        let reason = if for_agent > 0 {
            WakeReason::Workspace {
                summary: format!(
                    "{for_agent} new message{} for you",
                    if for_agent == 1 { "" } else { "s" }
                ),
                urgent: arrived
                    .iter()
                    .any(|e| e.addressed && e.conversation == "dm"),
            }
        } else {
            WakeReason::NextLook
        };

        let result = Workday {
            store: &mut store,
            provider,
            executor: &mut executor,
            assembler: &assembler,
            profile,
            clock: clock.as_ref(),
            limits: WakeLimits::default(),
        }
        .wake(
            &reason,
            Message::user_text(wake_message(at, &reason, &arrived)),
        )
        .await;
        wakes += 1;
        if let Some(day) = store.current_day()? {
            days.insert(day.id);
        }

        next_look = match result {
            Ok(outcome) => match outcome.end {
                WakeEnd::Slept { until } => until,
                WakeEnd::Paused { breaker } => {
                    paused = Some(breaker);
                    break;
                }
                other => {
                    // The wake didn't end with a chosen next look.
                    errors.push(format!("wake at {at} ended {}", other.label()));
                    clock.now() + DEFAULT_NEXT_LOOK
                }
            },
            Err(err) => {
                errors.push(err.to_string());
                clock.now() + DEFAULT_NEXT_LOOK
            }
        };
    }

    let records = store.wakes_since(scenario.start)?;
    let mut blocked = Vec::new();
    for day in &days {
        for message in store.transcript(*day)? {
            for block in message.content {
                if let ContentBlock::ToolResult(result) = block {
                    if result.is_error && result.content.starts_with("blocked") {
                        blocked.push(result.content);
                    }
                }
            }
        }
    }
    let sent = outbox.lock().expect("outbox poisoned").clone();
    Ok(Report::new(
        scenario,
        records,
        sent,
        blocked,
        errors,
        paused,
        days.len(),
    ))
}
