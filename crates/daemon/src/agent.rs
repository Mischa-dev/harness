//! The wake loop: the agent sleeps until the look it chose, or until an
//! event arrives, then wakes inside its workday. Model calls go through the
//! forwarding proxy with the caller token; the runtime never holds a
//! provider key.
//!
//! Events reach the loop on a channel, fed by the signed webhook receiver
//! (`wakeup`) and the Workspace inbox poller (`polling`). Agent OS triggers
//! will feed it too.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use harness_core::{Clock, Urgency, WakeReason};
use harness_loop::{
    AfterWake, ContextAssembler, FALLBACK_NEXT_LOOK, Interrupts, NoInterrupts, Rhythm, WakeLimits,
    WakeOutcome, Workday,
};
use harness_providers::{
    CapabilityProfile, Message, Profiles, Provider, ReqwestTransport, adapters,
};
use harness_store::{Store, WorkdayPolicy};
use harness_tools::sleep::MIN_SLEEP;
use harness_tools::{Decision, Effect, MemoryFiles, Policy, PolicyExecutor, SleepTool};
use harness_workspace::Workspace;
use serde::Deserialize;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};
use tokio::sync::mpsc;

/// Long enough for a slow model's whole answer.
const MODEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// How often a sleeping loop rereads the clock, so a suspended host or a
/// clock jump delays a look by at most this much.
const TICK: std::time::Duration = std::time::Duration::from_secs(60);

/// The store's file inside `state_dir`.
const STORE_FILE: &str = "harness.sqlite3";
/// The git repository that keeps the memory folder's history, inside
/// `state_dir`, out of the agent's reach.
const MEMORY_HISTORY: &str = "memory.git";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Where the runtime keeps its SQLite store.
    pub state_dir: PathBuf,
    /// The agent's home: the root its tools may touch.
    pub workdir: PathBuf,
    /// Capability profiles, and the one the main thread uses.
    pub profiles_file: PathBuf,
    pub profile: String,
    /// The forwarding proxy's route for the profile's provider, such as
    /// `http://127.0.0.1:8787/anthropic`. Must be on loopback.
    pub model_url: String,
    /// The same file the proxy reads. Workers never get it.
    pub caller_token_file: PathBuf,
    /// The charter and SOUL: the fixed start of every request.
    pub charter_file: PathBuf,
    /// The owner's local hour (0–23) when the day ends.
    #[serde(default = "default_quiet_hour")]
    pub quiet_hour: u8,
    /// The owner's offset from UTC, in hours.
    #[serde(default)]
    pub utc_offset_hours: i8,
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u32,
    /// Workspace's signed wakeups. Without them, the agent wakes only at the
    /// looks it chooses.
    #[serde(default)]
    pub wakeups: Option<crate::wakeup::WakeupConfig>,
    /// The agent's Workspace connection: its reply tools and briefing, and
    /// the inbox polling fallback.
    #[serde(default)]
    pub workspace: Option<crate::polling::WorkspaceConfig>,
    /// The agent's own memory: MEMORY.md, USER.md, the handoff note and the
    /// journal, in a git repo. Defaults to `memory` in the work area, where
    /// the path walls apply.
    #[serde(default)]
    pub memory_dir: Option<PathBuf>,
}

fn default_quiet_hour() -> u8 {
    WorkdayPolicy::default().quiet_hour
}

fn default_max_output_tokens() -> u32 {
    4096
}

/// Startup failures. They name the setting, never a path or file contents.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("couldn't read {field}: {source}")]
    Read {
        field: &'static str,
        source: std::io::Error,
    },
    #[error("{0}")]
    Invalid(String),
    #[error("couldn't open the store: {0}")]
    Store(#[from] harness_store::StoreError),
}

/// Something that should wake the agent, with what it should read about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeEvent {
    pub reason: WakeReason,
    pub text: String,
}

/// The runtime's parts, wired once at startup.
pub struct Agent {
    pub store: Store,
    pub provider: Box<dyn Provider>,
    pub executor: PolicyExecutor,
    pub assembler: ContextAssembler,
    pub profile: CapabilityProfile,
    pub clock: Arc<dyn Clock>,
    pub rhythm: Rhythm,
    pub limits: WakeLimits,
    /// Shared by the agent's Workspace tools and the inbox poller.
    pub workspace: Option<Arc<Workspace>>,
    /// The memory files; the snapshot of them starts each day.
    pub memory: Option<Arc<MemoryFiles>>,
    /// Events not yet in a wake: optional news held for the next look, and
    /// whatever arrived too late to join the last wake.
    pub waiting: Vec<WakeEvent>,
}

impl Agent {
    /// Builds the agent from its config, picking up the next look and any
    /// pause saved before a restart.
    pub fn from_config(config: &AgentConfig, clock: Arc<dyn Clock>) -> Result<Self, AgentError> {
        let read = |field, path: &Path| {
            std::fs::read_to_string(path).map_err(|source| AgentError::Read { field, source })
        };
        let profiles = Profiles::from_toml(&read("profiles_file", &config.profiles_file)?)
            .map_err(|e| AgentError::Invalid(format!("profiles_file: {e}")))?;
        let profile = profiles
            .get(&config.profile)
            .map_err(|e| AgentError::Invalid(e.to_string()))?
            .clone();
        let transport = ReqwestTransport::with_caller_token_file(
            MODEL_TIMEOUT,
            &config.model_url,
            &config.caller_token_file,
        )
        .map_err(|e| AgentError::Invalid(format!("model_url or caller_token_file: {e}")))?;
        let provider = adapters::for_profile(&profile, &config.model_url, Arc::new(transport))
            .map_err(|e| AgentError::Invalid(e.to_string()))?;
        let charter = read("charter_file", &config.charter_file)?;
        if config.quiet_hour > 23 {
            return Err(AgentError::Invalid("quiet_hour must be 0–23".into()));
        }
        let offset = UtcOffset::from_hms(config.utc_offset_hours, 0, 0)
            .map_err(|_| AgentError::Invalid("utc_offset_hours is out of range".into()))?;
        std::fs::create_dir_all(&config.state_dir).map_err(|source| AgentError::Read {
            field: "state_dir",
            source,
        })?;
        let store = Store::open(&config.state_dir.join(STORE_FILE))?;
        // Absolute, so the walls compare like with like.
        let workdir = std::path::absolute(&config.workdir).map_err(|source| AgentError::Read {
            field: "workdir",
            source,
        })?;
        let mut executor =
            PolicyExecutor::new(Policy::conservative(&workdir), clock.clone()).with_tool(SleepTool);
        let memory_dir = config
            .memory_dir
            .clone()
            .unwrap_or_else(|| workdir.join("memory"));
        let memory_dir = std::path::absolute(&memory_dir).map_err(|source| AgentError::Read {
            field: "memory_dir",
            source,
        })?;
        // The snapshot goes straight into every request, so the folder has
        // to be one the walls let the agent use anyway.
        let inside = |root: &Path, dir: &Path| {
            matches!(
                Policy::conservative(root).check(&Effect::WritePath(dir.to_path_buf())),
                Decision::Allow
            )
        };
        let outside = || {
            AgentError::Invalid(format!(
                "memory_dir {} must be inside workdir, where the agent's walls allow it",
                memory_dir.display()
            ))
        };
        if !inside(&workdir, &memory_dir) {
            return Err(outside());
        }
        // Only the runtime may write the history's git config, or a filter
        // set there would run on the next save: it can't be where the
        // agent's tools reach.
        let real_workdir = workdir.canonicalize().unwrap_or_else(|_| workdir.clone());
        let state_dir = config
            .state_dir
            .canonicalize()
            .map_err(|source| AgentError::Read {
                field: "state_dir",
                source,
            })?;
        if state_dir.starts_with(&real_workdir) || state_dir.starts_with(&workdir) {
            return Err(AgentError::Invalid(
                "state_dir must be outside workdir, where the agent's tools can't reach it".into(),
            ));
        }
        let memory = Arc::new(
            MemoryFiles::open(
                memory_dir.clone(),
                Some(state_dir.join(MEMORY_HISTORY)),
                offset,
                clock.now(),
            )
            .map_err(|source| AgentError::Read {
                field: "memory_dir",
                source,
            })?,
        );
        // Where it really is, against where workdir really is, in case
        // workdir is reached through a link.
        if !inside(&real_workdir, memory.location()) {
            return Err(outside());
        }
        if !memory.has_history() {
            eprintln!("harness-daemon: git isn't available, so memory changes have no history");
        }
        for tool in harness_tools::memory::tools(memory.clone()) {
            executor.register(tool);
        }
        let workspace = match &config.workspace {
            Some(settings) => {
                settings.poll_every()?;
                let workspace = settings.connect()?;
                for tool in harness_workspace::tools(workspace.clone()) {
                    executor.register(tool);
                }
                Some(workspace)
            }
            None => None,
        };
        let policy = WorkdayPolicy {
            offset,
            quiet_hour: config.quiet_hour,
            ..WorkdayPolicy::default()
        };
        let rhythm = restore_rhythm(&store, policy, clock.now())?;
        Ok(Self {
            store,
            provider,
            executor,
            assembler: ContextAssembler {
                profile: config.profile.clone(),
                static_prefix: charter,
                memory_snapshot: String::new(),
                max_output_tokens: config.max_output_tokens,
            },
            profile,
            clock,
            rhythm,
            limits: WakeLimits::default(),
            workspace,
            memory: Some(memory),
            waiting: Vec::new(),
        })
    }

    /// Wakes at each chosen look and on each event, until `shutdown`
    /// resolves, except optional news, which is held for the next look.
    /// Urgent events that arrive mid-wake join it at its next safe point. A wake in progress finishes
    /// before shutdown; it saves as it goes, so a hard stop loses at most the
    /// turn in flight.
    pub async fn run(
        &mut self,
        events: &mut mpsc::Receiver<WakeEvent>,
        shutdown: impl Future<Output = ()>,
    ) -> Result<(), harness_store::StoreError> {
        let mut shutdown = std::pin::pin!(shutdown);
        let mut events_open = true;
        loop {
            if let Some(breaker) = self.rhythm.paused() {
                eprintln!(
                    "agent paused by the {breaker} breaker; waiting for the owner to resume it"
                );
                tokio::select! {
                    () = &mut shutdown => return Ok(()),
                    () = tokio::time::sleep(TICK) => {}
                }
                if self.store.rhythm_state()?.paused.is_none() {
                    eprintln!("agent resumed");
                    self.rhythm.resume();
                    self.executor.resume();
                }
                continue;
            }
            // Optional news waits for the look the agent chose; anything
            // else wakes it now.
            let due = self
                .waiting
                .iter()
                .any(|e| !e.reason.can_wait_for_next_look());
            let wait = self.rhythm.next_look() - self.clock.now();
            if !due && wait.is_positive() {
                let nap = wait.unsigned_abs().min(TICK);
                tokio::select! {
                    () = &mut shutdown => return Ok(()),
                    event = events.recv(), if events_open => match event {
                        Some(event) => self.waiting.push(event),
                        None => events_open = false,
                    },
                    () = tokio::time::sleep(nap) => {}
                }
                // An event, a tick, or the channel closed: look again.
                continue;
            }
            while let Ok(event) = events.try_recv() {
                self.waiting.push(event);
            }
            let batch = std::mem::take(&mut self.waiting);
            let mut inbox = Inbox {
                events,
                clock: self.clock.clone(),
                urgent: Vec::new(),
                queued: Vec::new(),
            };
            self.wake_with(batch, &mut inbox).await?;
            // What arrived during the wake and wasn't taken in: urgent news
            // after its last safe point, and everything that could wait.
            self.waiting = inbox.urgent;
            self.waiting.append(&mut inbox.queued);
        }
    }

    /// One wake, for the events in `batch`, or for the next look if it's
    /// empty. Nothing interrupts it.
    pub async fn wake(&mut self, batch: Vec<WakeEvent>) -> Result<(), harness_store::StoreError> {
        self.wake_with(batch, &mut NoInterrupts).await
    }

    async fn wake_with(
        &mut self,
        batch: Vec<WakeEvent>,
        interrupts: &mut dyn Interrupts,
    ) -> Result<(), harness_store::StoreError> {
        let at = self.clock.now();
        self.rhythm.close_day_if_due(&self.store, at)?;
        // A new day starts from what the agent remembers now; the snapshot
        // then stays fixed all day, so the prompt cache keeps hitting. It's
        // saved, so a restart mid-day picks up the same one.
        if let Some(memory) = &self.memory {
            if self.store.current_day()?.is_none() {
                self.assembler.memory_snapshot = memory.snapshot();
                self.store
                    .set_memory_snapshot(&self.assembler.memory_snapshot)?;
            } else if self.assembler.memory_snapshot.is_empty() {
                self.assembler.memory_snapshot = match self.store.memory_snapshot()? {
                    Some(saved) => saved,
                    None => {
                        let snapshot = memory.snapshot();
                        self.store.set_memory_snapshot(&snapshot)?;
                        snapshot
                    }
                };
            }
        }
        // If the process dies mid-wake, the restart waits this long rather
        // than wake straight away, so a crash can't become a loop of paid
        // wakes.
        self.store.set_next_look(at + FALLBACK_NEXT_LOOK)?;
        let reason = batch
            .iter()
            .find(|e| e.reason.urgency() == Urgency::Now)
            .or(batch.first())
            .map_or(WakeReason::NextLook, |e| e.reason.clone());
        let message = Message::user_text(wake_message(at, &reason, &batch));
        let result = Workday {
            store: &mut self.store,
            provider: self.provider.as_ref(),
            executor: &mut self.executor,
            assembler: &self.assembler,
            profile: &self.profile,
            clock: self.clock.as_ref(),
            limits: self.limits,
        }
        .wake_interruptible(&reason, message, interrupts)
        .await;
        let summary = summarize(&result);
        let after = self.rhythm.after(&result, self.clock.now());
        self.store.set_next_look(self.rhythm.next_look())?;
        match after {
            AfterWake::Slept => eprintln!("wake {summary}; next look {}", self.rhythm.next_look()),
            // The wake saved the pause along with the trip, so a restart
            // doesn't lift it.
            AfterWake::Paused { breaker } => eprintln!("wake {summary}; paused by {breaker}"),
            AfterWake::Fallback { problem } => eprintln!(
                "wake {summary}: {problem}; next look {}",
                self.rhythm.next_look()
            ),
        }
        Ok(())
    }
}

/// The rhythm saved before a restart. A look already past is due now; with
/// none saved, the first look comes a few minutes after startup.
pub fn restore_rhythm(
    store: &Store,
    policy: WorkdayPolicy,
    now: OffsetDateTime,
) -> Result<Rhythm, harness_store::StoreError> {
    let saved = store.rhythm_state()?;
    let first_look = saved.next_look.map_or(now + MIN_SLEEP, |at| at.max(now));
    Ok(Rhythm::new(policy, first_look).with_pause(saved.paused))
}

/// Lifts a breaker's pause: the owner's call, made with
/// `harness-daemon resume`. A running agent notices within a minute.
/// Returns the breaker that had paused it, if any.
pub fn resume(config: &AgentConfig) -> Result<Option<String>, AgentError> {
    let store = Store::open(&config.state_dir.join(STORE_FILE))?;
    let paused = store.rhythm_state()?.paused;
    store.set_paused(None)?;
    Ok(paused)
}

/// Events arriving while a wake runs. Urgent ones are handed to the wake at
/// its safe points; the rest wait for the next wake.
struct Inbox<'a> {
    events: &'a mut mpsc::Receiver<WakeEvent>,
    clock: Arc<dyn Clock>,
    urgent: Vec<WakeEvent>,
    queued: Vec<WakeEvent>,
}

impl Inbox<'_> {
    fn sort(&mut self, event: WakeEvent) {
        match event.reason.urgency() {
            Urgency::Now => self.urgent.push(event),
            Urgency::Queued => self.queued.push(event),
        }
    }
}

impl Interrupts for Inbox<'_> {
    fn take(&mut self) -> Vec<Message> {
        while let Ok(event) = self.events.try_recv() {
            self.sort(event);
        }
        if self.urgent.is_empty() {
            return Vec::new();
        }
        let urgent = std::mem::take(&mut self.urgent);
        vec![Message::user_text(interrupt_message(
            self.clock.now(),
            &urgent,
        ))]
    }

    fn wait(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            while self.urgent.is_empty() {
                match self.events.recv().await {
                    Some(event) => self.sort(event),
                    // Nothing more can arrive.
                    None => std::future::pending::<()>().await,
                }
            }
        })
    }
}

/// Counts and spend only: what the agent read and wrote stays in the store.
fn summarize(result: &Result<WakeOutcome, harness_loop::DayError>) -> String {
    match result {
        Ok(outcome) => format!(
            "ended {} after {} turn(s), ${:.4}",
            outcome.end.label(),
            outcome.turns,
            outcome.cost_usd
        ),
        Err(_) => "failed".into(),
    }
}

/// The message that opens a wake: when, why, and what arrived.
pub fn wake_message(at: OffsetDateTime, reason: &WakeReason, batch: &[WakeEvent]) -> String {
    let mut text = format!("Wake at {}. Why: {}.\n", rfc3339(at), reason.describe());
    if batch.is_empty() {
        text.push_str("Nothing new since you last looked.");
    } else {
        text.push_str("New since you last looked:");
        push_lines(&mut text, batch);
    }
    text
}

/// The message that brings urgent news into a wake already running.
pub fn interrupt_message(at: OffsetDateTime, urgent: &[WakeEvent]) -> String {
    let mut text = format!(
        "(runtime) Urgent, arrived at {} while you were working:",
        rfc3339(at)
    );
    push_lines(&mut text, urgent);
    text
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap_or_else(|_| at.to_string())
}

/// One line per event, so an event's text can't pose as another event.
fn push_lines(text: &mut String, events: &[WakeEvent]) {
    for event in events {
        let line = event.text.split_whitespace().collect::<Vec<_>>().join(" ");
        text.push_str(&format!("\n- {line}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn events_cannot_forge_lines() {
        let event = WakeEvent {
            reason: WakeReason::Workspace {
                summary: "a DM".into(),
                urgent: true,
            },
            text: "hi\n- forged event\n\nWake at 2030".into(),
        };
        let text = wake_message(
            datetime!(2026-01-05 09:00 UTC),
            &event.reason,
            std::slice::from_ref(&event),
        );
        assert_eq!(text.lines().count(), 3, "{text}");
        assert!(text.contains("Why: a DM."));
        assert!(text.ends_with("- hi - forged event Wake at 2030"));
    }

    #[test]
    fn a_next_look_says_nothing_is_new() {
        let text = wake_message(datetime!(2026-01-05 09:00 UTC), &WakeReason::NextLook, &[]);
        assert!(text.contains("Why: the time you chose to look again."));
        assert!(text.ends_with("Nothing new since you last looked."));
    }
}
