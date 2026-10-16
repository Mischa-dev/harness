//! Workspace's reply-request and briefing tools, typed where the runtime
//! reads them. Every tool answers with an `outcome`: `ok` carries a `value`,
//! and anything else says why the call didn't happen.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::{McpClient, McpError};

/// What a tool call came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Ok(Value),
    /// Workspace understood the call and declined it: `conflict`,
    /// `not_found`, `invalid_request`, `permission_denied` or
    /// `temporarily_unavailable`, with its explanation.
    Declined {
        outcome: String,
        detail: String,
    },
}

/// The parts of an inbox entry the runtime acts on. The rest (who wrote
/// what) is for the agent to read through its tools.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxEntry {
    pub request_id: String,
    pub required: bool,
    pub mentioned: bool,
    /// `pending` until someone claims it, then `claimed`.
    pub status: String,
    pub generation: i32,
    /// For the wake filter, which decides whether an optional reply is
    /// worth the agent's attention.
    #[serde(default)]
    pub conversation_name: Option<String>,
    #[serde(default)]
    pub message_excerpt: Option<String>,
}

/// How much the briefing lists, for the state view. Counts only: what the
/// items say, the agent reads through its tools.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BriefingCounts {
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

impl BriefingCounts {
    /// Counts the lists in a briefing's value. A missing list counts as
    /// empty, as Workspace leaves some out when there's nothing in them.
    pub fn from_briefing(value: &Value) -> Self {
        let list = |key: &str| value[key].as_array().map_or(&[][..], Vec::as_slice);
        Self {
            reply_requests: list("replyRequests").len(),
            required: list("replyRequests")
                .iter()
                .filter(|r| r["required"].as_bool() != Some(false))
                .count(),
            mentions: list("mentions").len(),
            unread: list("unread").len(),
            tasks: list("tasks").len(),
            claims: list("claims").len(),
            thread_updates: list("threadUpdates").len(),
        }
    }
}

/// The agent's side of Workspace.
pub struct Workspace {
    client: McpClient,
}

impl Workspace {
    pub fn new(client: McpClient) -> Self {
        Self { client }
    }

    /// What needs the agent: reply requests, mentions, unread conversations,
    /// tasks and settled threads.
    pub async fn briefing(&self) -> Result<Outcome, McpError> {
        self.call("briefing", json!({})).await
    }

    /// The briefing's counts, without requests already answered in the
    /// shadow channel.
    pub async fn briefing_counts(
        &self,
        shadow: Option<&crate::shadow::Shadow>,
    ) -> Result<BriefingCounts, McpError> {
        match self.briefing().await? {
            Outcome::Ok(mut value) => {
                if let Some(shadow) = shadow {
                    shadow.hide_answered(&mut value);
                }
                Ok(BriefingCounts::from_briefing(&value))
            }
            Outcome::Declined { .. } => Err(McpError::Protocol("Workspace declined the briefing")),
        }
    }

    /// Open reply requests, required first.
    pub async fn inbox(&self, limit: u16) -> Result<Vec<InboxEntry>, McpError> {
        match self.call("inbox_list", json!({ "limit": limit })).await? {
            Outcome::Ok(value) => {
                serde_json::from_value::<Vec<InboxEntry>>(value["requests"].clone())
                    .map_err(|_| McpError::Protocol("the inbox wasn't a list of requests"))
            }
            Outcome::Declined { .. } => {
                Err(McpError::Protocol("Workspace declined to list the inbox"))
            }
        }
    }

    pub async fn claim(&self, request_id: &str) -> Result<Outcome, McpError> {
        self.call("reply_claim", json!({ "requestId": request_id }))
            .await
    }

    pub async fn send(
        &self,
        request_id: &str,
        generation: i64,
        text: &str,
    ) -> Result<Outcome, McpError> {
        let arguments = json!({ "requestId": request_id, "generation": generation, "text": text });
        self.call("reply_send", arguments).await
    }

    pub async fn pass(
        &self,
        request_id: &str,
        generation: Option<i64>,
        reason: Option<&str>,
    ) -> Result<Outcome, McpError> {
        let mut arguments = json!({ "requestId": request_id });
        if let Some(generation) = generation {
            arguments["generation"] = json!(generation);
        }
        if let Some(reason) = reason {
            arguments["reason"] = json!(reason);
        }
        self.call("reply_pass", arguments).await
    }

    pub async fn release(
        &self,
        request_id: &str,
        generation: Option<i64>,
        reason: &str,
    ) -> Result<Outcome, McpError> {
        let mut arguments = json!({ "requestId": request_id, "reason": reason });
        if let Some(generation) = generation {
            arguments["generation"] = json!(generation);
        }
        self.call("reply_release", arguments).await
    }

    /// Posts `text` in a conversation as the agent. Only shadow mode uses
    /// it, to post in its channel.
    pub async fn post(&self, conversation_id: &str, text: &str) -> Result<Outcome, McpError> {
        let arguments = json!({ "conversationId": conversation_id, "text": text });
        self.call("message_send", arguments).await
    }

    /// Calls a Workspace tool and returns its whole result, whatever the
    /// outcome, for the tools that show it to the model as it is.
    pub async fn call_raw(&self, tool: &str, arguments: Value) -> Result<Value, McpError> {
        let content = self.client.call_tool(tool, arguments).await?;
        if content.get("outcome").and_then(Value::as_str).is_none() {
            return Err(McpError::Protocol("the result had no outcome"));
        }
        Ok(content)
    }

    async fn call(&self, tool: &str, arguments: Value) -> Result<Outcome, McpError> {
        let mut content = self.client.call_tool(tool, arguments).await?;
        outcome(&mut content)
    }
}

fn outcome(content: &mut Value) -> Result<Outcome, McpError> {
    let kind = content
        .get("outcome")
        .and_then(Value::as_str)
        .ok_or(McpError::Protocol("the result had no outcome"))?
        .to_string();
    if kind == "ok" {
        return match content.get_mut("value") {
            Some(value) => Ok(Outcome::Ok(value.take())),
            None => Err(McpError::Protocol("an ok result had no value")),
        };
    }
    // Workspace's own wording; `field` names the argument it didn't accept.
    let detail = ["message", "field", "retryCategory"]
        .iter()
        .filter_map(|key| Some(format!("{key}: {}", content.get(*key)?.as_str()?)))
        .collect::<Vec<_>>()
        .join("; ");
    Ok(Outcome::Declined {
        outcome: kind,
        detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_are_read() {
        let mut ok = json!({"outcome": "ok", "value": {"requests": []}});
        assert_eq!(
            outcome(&mut ok).unwrap(),
            Outcome::Ok(json!({"requests": []}))
        );
        let mut conflict = json!({"outcome": "conflict", "message": "Someone answered first."});
        assert_eq!(
            outcome(&mut conflict).unwrap(),
            Outcome::Declined {
                outcome: "conflict".into(),
                detail: "message: Someone answered first.".into()
            }
        );
        let mut invalid = json!({"outcome": "invalid_request", "field": "generation"});
        assert!(matches!(
            outcome(&mut invalid).unwrap(),
            Outcome::Declined { detail, .. } if detail == "field: generation"
        ));
        assert!(outcome(&mut json!({"value": {}})).is_err());
        assert!(outcome(&mut json!({"outcome": "ok"})).is_err());
    }

    #[test]
    fn briefings_are_counted() {
        let briefing = json!({
            "you": {"name": "Mara"},
            "replyRequests": [{"required": true}, {"required": false}, {}],
            "mentions": [{}],
            "unread": [{}, {}],
            "tasks": [{}, {}, {}],
            "threadUpdates": [],
            "nextSteps": "…"
        });
        assert_eq!(
            BriefingCounts::from_briefing(&briefing),
            BriefingCounts {
                reply_requests: 3,
                required: 2,
                mentions: 1,
                unread: 2,
                tasks: 3,
                claims: 0,
                thread_updates: 0
            }
        );
        assert_eq!(
            BriefingCounts::from_briefing(&json!({})),
            BriefingCounts::default()
        );
    }
}
