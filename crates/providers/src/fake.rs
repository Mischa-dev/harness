//! A scripted model for deterministic tests and the simulator. It replays
//! predetermined responses in order and records every request it was sent.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::message::{
    ContentBlock, Message, ModelRequest, ModelResponse, Role, StopReason, ToolCall, Usage,
};
use crate::provider::{Provider, ProviderError};

#[derive(Debug, Default)]
pub struct ScriptedProvider {
    script: Mutex<VecDeque<Result<ModelResponse, ProviderError>>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptedProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn then(self, response: ModelResponse) -> Self {
        self.script
            .lock()
            .expect("poisoned")
            .push_back(Ok(response));
        self
    }

    pub fn then_error(self, error: ProviderError) -> Self {
        self.script.lock().expect("poisoned").push_back(Err(error));
        self
    }

    /// Queues a turn where the model says `text` and makes the given tool calls.
    pub fn then_calls(self, text: &str, calls: Vec<(&str, serde_json::Value)>) -> Self {
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(ContentBlock::Text { text: text.into() });
        }
        let next_id = self.queued_len();
        for (i, (name, arguments)) in calls.into_iter().enumerate() {
            content.push(ContentBlock::ToolCall(ToolCall {
                id: format!("call_{next_id}_{i}"),
                name: name.into(),
                arguments,
            }));
        }
        self.then(ModelResponse {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 100,
                output_tokens: 20,
                ..Usage::default()
            },
        })
    }

    pub fn then_text(self, text: &str) -> Self {
        self.then(ModelResponse {
            message: Message::assistant_text(text),
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 100,
                output_tokens: 20,
                ..Usage::default()
            },
        })
    }

    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().expect("poisoned").clone()
    }

    pub fn remaining(&self) -> usize {
        self.script.lock().expect("poisoned").len()
    }

    fn queued_len(&self) -> usize {
        self.script.lock().expect("poisoned").len()
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        self.requests
            .lock()
            .expect("poisoned")
            .push(request.clone());
        self.script
            .lock()
            .expect("poisoned")
            .pop_front()
            .unwrap_or_else(|| Err(ProviderError::Unavailable("script exhausted".into())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> ModelRequest {
        ModelRequest {
            profile: "fake".into(),
            system: String::new(),
            tools: vec![],
            messages: vec![Message::user_text("hi")],
            cache_breakpoints: vec![],
            max_output_tokens: 100,
        }
    }

    #[tokio::test]
    async fn replays_in_order_and_records_requests() {
        let provider = ScriptedProvider::new()
            .then_calls("looking", vec![("sleep", json!({"minutes": 60}))])
            .then_text("done");
        let first = provider.complete(&request()).await.unwrap();
        assert_eq!(first.message.tool_calls().count(), 1);
        let second = provider.complete(&request()).await.unwrap();
        assert_eq!(second.stop_reason, StopReason::EndTurn);
        assert!(provider.complete(&request()).await.is_err());
        assert_eq!(provider.requests().len(), 3);
    }

    #[tokio::test]
    async fn call_ids_are_unique_across_turns() {
        let provider = ScriptedProvider::new()
            .then_calls("", vec![("a", json!({}))])
            .then_calls("", vec![("a", json!({}))]);
        let a = provider.complete(&request()).await.unwrap();
        let b = provider.complete(&request()).await.unwrap();
        let id_a = &a.message.tool_calls().next().unwrap().id;
        let id_b = &b.message.tool_calls().next().unwrap().id;
        assert_ne!(id_a, id_b);
    }
}
