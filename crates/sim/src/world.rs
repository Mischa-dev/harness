//! The simulated world the agent acts in: a `message_send` tool that records
//! what it says, and the wake message that tells it what arrived.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use harness_core::WakeReason;
use harness_providers::ToolSpec;
use harness_tools::{Effect, Tool, ToolContext, ToolError, ToolOutput};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::scenario::Event;

/// A message the agent sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub at: OffsetDateTime,
    pub conversation: String,
    pub text: String,
}

/// Everything the agent has said, shared between the tool and the runner.
pub type Outbox = Arc<Mutex<Vec<Sent>>>;

/// `message_send(conversation, text)`. Declares a `SendMessage` effect, so
/// the executor's walls and message breaker apply as they would for real.
pub struct MessageSend {
    pub outbox: Outbox,
}

#[async_trait]
impl Tool for MessageSend {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "message_send".into(),
            description: "Post a message in a conversation: `dm` for your owner, or a \
                          channel such as `#general`."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "conversation": {"type": "string"},
                    "text": {"type": "string"}
                },
                "required": ["conversation", "text"]
            }),
        }
    }

    fn effects(&self, args: &Value) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![Effect::SendMessage {
            conversation: conversation(args)?.into(),
        }])
    }

    async fn run(&self, args: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("text must be a string".into()))?;
        if text.trim().is_empty() {
            return Err(ToolError::InvalidArguments("text is empty".into()));
        }
        self.outbox.lock().expect("outbox poisoned").push(Sent {
            at: ctx.clock.now(),
            conversation: conversation(args)?.into(),
            text: text.into(),
        });
        Ok(ToolOutput::text("Sent."))
    }
}

fn conversation(args: &Value) -> Result<&str, ToolError> {
    args.get("conversation")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidArguments("conversation must be a string".into()))
}

/// Marks a message the agent is expected to answer.
pub const TO_YOU: &str = ", to you:";

/// Why the agent is awake and what arrived since it last looked. The
/// stand-in for the state view, which comes in M2.
pub fn wake_message(now: OffsetDateTime, reason: &WakeReason, new: &[&Event]) -> String {
    let when = now.format(&Rfc3339).unwrap_or_else(|_| now.to_string());
    let why = match reason {
        WakeReason::Workspace { summary, .. } => summary.clone(),
        WakeReason::NextLook => "the time you chose to look again".into(),
        other => format!("{other:?}"),
    };
    let mut text = format!("Wake at {when}. Why: {why}.\n");
    if new.is_empty() {
        text.push_str("Nothing new since you last looked.");
    } else {
        text.push_str("New since you last looked:");
        for event in new {
            let marker = if event.addressed { TO_YOU } else { ":" };
            text.push_str(&format!(
                "\n- [{}] {}{marker} {}",
                event.conversation, event.from, event.text
            ));
        }
    }
    text
}
