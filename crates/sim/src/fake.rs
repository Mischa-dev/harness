//! The fake teammate: a rule-following stand-in for a model, so scenarios run
//! fast, free and the same every time. It answers what's addressed to it,
//! ignores the rest, and sleeps for a few hours. It tests the harness, not
//! judgment: real models take the exam with `--model`.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use harness_providers::{
    ContentBlock, Message, ModelRequest, ModelResponse, Provider, ProviderError, Role, StopReason,
    ToolCall, Usage,
};
use serde_json::json;

use crate::world::TO_YOU;

/// How long the fake sleeps between looks.
pub const LOOK_EVERY_MINUTES: i64 = 8 * 60;

#[derive(Debug, Default)]
pub struct FakeTeammate {
    calls: AtomicUsize,
}

#[async_trait]
impl Provider for FakeTeammate {
    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        let n = self.calls.fetch_add(1, Ordering::Relaxed);
        let mut content = Vec::new();
        let mut call = |name: &str, arguments| {
            content.push(ContentBlock::ToolCall(ToolCall {
                id: format!("fake_{n}_{}", content.len()),
                name: name.into(),
                arguments,
            }));
        };
        for conversation in addressed_conversations(request) {
            call(
                "message_send",
                json!({"conversation": conversation, "text": "On it."}),
            );
        }
        call("sleep", json!({"minutes": LOOK_EVERY_MINUTES}));

        let input = serde_json::to_string(request).map_or(0, |s| s.len() / 4) as u64;
        Ok(ModelResponse {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: input,
                output_tokens: 30,
                ..Usage::default()
            },
        })
    }
}

/// Conversations with a message addressed to the agent in the newest wake
/// message, in order, each once.
fn addressed_conversations(request: &ModelRequest) -> Vec<String> {
    let Some(last) = request.messages.last() else {
        return vec![];
    };
    let mut found: Vec<String> = Vec::new();
    for block in &last.content {
        let ContentBlock::Text { text } = block else {
            continue;
        };
        for line in text.lines().filter(|line| line.contains(TO_YOU)) {
            let conversation = line
                .strip_prefix("- [")
                .and_then(|rest| rest.split_once(']'))
                .map(|(conversation, _)| conversation.to_string());
            if let Some(c) = conversation
                && !found.contains(&c)
            {
                found.push(c);
            }
        }
    }
    found
}
