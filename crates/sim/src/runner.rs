//! Runs a scenario: the fake clock jumps from one moment that matters to the
//! next (a message addressed to the agent, or the look it chose when it went
//! to sleep), and the agent wakes there inside its workday, as it would under
//! the daemon.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use harness_core::{Clock, FakeClock, WakeReason};
use harness_loop::{AfterWake, CharterValues, ContextAssembler, Rhythm, WakeLimits, Workday};
use harness_providers::{CapabilityProfile, ContentBlock, Message, Provider};
use harness_store::{Store, StoreError, WorkdayPolicy};
use harness_tools::memory::MemoryFiles;
use harness_tools::{Policy, PolicyExecutor, SleepTool};

use crate::report::Report;
use crate::scenario::Scenario;
use crate::world::{MessageSend, Outbox, wake_message};

/// A safety net against a runaway agent or a bug in the runner.
const MAX_WAKES: usize = 2_000;

/// The real charter, so a real-model run is judged on what the agent will
/// actually be told.
const CHARTER: &str = include_str!("../../../prompts/teammate-charter.md");

fn static_prefix(scenario: &Scenario) -> String {
    CharterValues {
        agent_name: scenario.agent.clone(),
        owner_name: "Mischa".into(),
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
}

/// Runs `scenario` through the real workday, wake loop, store, and policy
/// executor, then returns its scored report.
pub async fn run(
    scenario: &Scenario,
    provider: &dyn Provider,
    profile: &CapabilityProfile,
    profile_name: &str,
) -> Result<Report, RunError> {
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
    let mut days = BTreeSet::new();
    let mut errors = Vec::new();
    let mut wakes = 0;

    loop {
        let next_addressed = events[delivered..]
            .iter()
            .find(|e| e.addressed)
            .map(|e| e.at);
        let next_look = rhythm.next_look();
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

        rhythm.close_day_if_due(&store, at)?;
        if store.current_day()?.is_none() {
            assembler.memory_snapshot = memory.snapshot()?;
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
    Ok(Report::new(
        scenario,
        records,
        sent,
        blocked,
        errors,
        rhythm.paused().map(str::to_string),
        days.len(),
    ))
}
