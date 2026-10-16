//! The agent's Workspace connection, and the polling fallback: a look at the
//! inbox at startup and every few minutes after, so a lost wakeup, or one
//! sent while the daemon was down, still wakes the agent.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use harness_core::{Clock, WakeReason};
use harness_workspace::{
    AccessToken, InboxWatch, McpClient, McpError, NewRequests, Shadow, Workspace,
};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::agent::{AgentError, WakeEvent};

/// Workspace answers in well under this; a hung call shouldn't hold a wake.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

const MIN_POLL_SECONDS: u64 = 30;
const MAX_POLL_SECONDS: u64 = 3600;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Workspace's MCP endpoint, such as `https://workspace.example/api/v1/mcp`.
    pub url: String,
    /// The agent's Workspace credential (`aw_ext_…`). The runtime holds it;
    /// workers never get it.
    pub token_file: PathBuf,
    /// How often to look at the inbox when no wakeup has come.
    #[serde(default = "default_poll_seconds")]
    pub poll_seconds: u64,
    /// Shadow mode: what the agent would say goes to a private channel
    /// instead. Leave it out and replies go where they're asked for.
    #[serde(default)]
    pub shadow: Option<ShadowConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowConfig {
    /// The private channel's Workspace conversation id.
    pub channel: String,
    /// Conversations where the agent answers for real, such as its owner's
    /// direct message with it.
    #[serde(default)]
    pub live: Vec<String>,
}

fn default_poll_seconds() -> u64 {
    300
}

impl WorkspaceConfig {
    pub fn poll_every(&self) -> Result<Duration, AgentError> {
        if !(MIN_POLL_SECONDS..=MAX_POLL_SECONDS).contains(&self.poll_seconds) {
            return Err(AgentError::Invalid(format!(
                "workspace.poll_seconds must be {MIN_POLL_SECONDS}–{MAX_POLL_SECONDS}"
            )));
        }
        Ok(Duration::from_secs(self.poll_seconds))
    }

    pub fn shadow(&self) -> Result<Option<Arc<Shadow>>, AgentError> {
        self.shadow
            .as_ref()
            .map(|shadow| {
                Shadow::new(&shadow.channel, &shadow.live)
                    .map(Arc::new)
                    .map_err(|e| AgentError::Invalid(format!("workspace.shadow: {e}")))
            })
            .transpose()
    }

    pub fn connect(&self) -> Result<Arc<Workspace>, AgentError> {
        let invalid = |e: McpError| AgentError::Invalid(format!("workspace: {e}"));
        let token = AccessToken::from_file(&self.token_file).map_err(invalid)?;
        let client = McpClient::new(&self.url, token, CALL_TIMEOUT).map_err(invalid)?;
        Ok(Arc::new(Workspace::new(client)))
    }
}

/// How often a waiting poller rereads the clock.
const TICK: Duration = Duration::from_secs(1);

/// Looks at the inbox now and every `every` after, by `clock`, sending a
/// wake event for requests the agent hasn't heard about. Ends when the agent
/// stops listening.
pub async fn poll(
    workspace: Arc<Workspace>,
    clock: Arc<dyn Clock>,
    every: Duration,
    shadow: Option<Arc<Shadow>>,
    events: mpsc::Sender<WakeEvent>,
) {
    let mut watch = InboxWatch::with_shadow(shadow);
    let mut next = clock.now();
    loop {
        let now = clock.now();
        if now < next {
            if events.is_closed() {
                return;
            }
            tokio::time::sleep((next - now).unsigned_abs().min(TICK)).await;
            continue;
        }
        let mut wait = every;
        match watch.check(&workspace).await {
            Ok(Some(new)) => {
                if events.send(event(new)).await.is_err() {
                    return;
                }
            }
            Ok(None) => {}
            Err(err) => {
                eprintln!("harness-daemon: couldn't look at the Workspace inbox: {err}");
                if let McpError::RateLimited {
                    retry_after_seconds,
                } = err
                {
                    wait = wait.max(Duration::from_secs(retry_after_seconds));
                }
            }
        }
        next = clock.now() + wait;
    }
}

/// Counts only. What the requests say, the agent reads through its tools.
pub fn event(new: NewRequests) -> WakeEvent {
    let NewRequests { count, pressing } = new;
    let requests = if count == 1 { "request" } else { "requests" };
    WakeEvent {
        reason: WakeReason::Workspace {
            summary: format!("{count} reply {requests} waiting in your Workspace inbox"),
            // A required reply or a mention wakes it now; optional group
            // replies wait for its next look.
            urgent: pressing > 0,
        },
        text: format!(
            "Workspace: {count} new reply {requests} waiting, {pressing} needing your answer \
             or mentioning you. Call briefing to see them."
        ),
    }
}
