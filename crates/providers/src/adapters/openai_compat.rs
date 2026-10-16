//! OpenAI-compatible Chat Completions (`POST /chat/completions`), which
//! covers OpenRouter, NVIDIA NIM, local servers and most other vendors.
//!
//! These providers cache prefixes automatically, so the loop's breakpoints
//! are ignored. Tool arguments arrive as a JSON string; a string that doesn't
//! parse is passed through as-is, so the policy executor rejects it as a
//! malformed call and the model gets the error back.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use super::{join_url, status_error};
use crate::message::{
    ContentBlock, Message, ModelRequest, ModelResponse, Role, StopReason, ToolCall, Usage,
};
use crate::profile::{CapabilityProfile, ToolCallStyle};
use crate::provider::{Provider, ProviderError};
use crate::transport::Transport;

pub struct OpenAiCompatAdapter {
    profile: CapabilityProfile,
    url: String,
    transport: Arc<dyn Transport>,
}

impl OpenAiCompatAdapter {
    /// `base_url` includes the API version path, e.g. `.../v1`.
    pub fn new(profile: CapabilityProfile, base_url: &str, transport: Arc<dyn Transport>) -> Self {
        Self {
            profile,
            url: join_url(base_url, "chat/completions"),
            transport,
        }
    }

    pub fn request_body(&self, request: &ModelRequest) -> Value {
        let mut messages = Vec::new();
        if !request.system.is_empty() {
            messages.push(json!({"role": "system", "content": request.system}));
        }
        for message in &request.messages {
            push_message(&mut messages, message);
        }

        let mut body = Map::new();
        body.insert("model".into(), json!(self.profile.model));
        body.insert("max_tokens".into(), json!(request.max_output_tokens));
        body.insert("messages".into(), json!(messages));
        if !request.tools.is_empty() {
            let tools: Vec<Value> = request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            body.insert("tools".into(), json!(tools));
            // Only sent when needed: some compatible servers reject the field.
            if self.profile.tool_calls == ToolCallStyle::Sequential {
                body.insert("parallel_tool_calls".into(), json!(false));
            }
        }
        Value::Object(body)
    }

    pub fn parse_response(body: &str) -> Result<ModelResponse, ProviderError> {
        let value: Value =
            serde_json::from_str(body).map_err(|e| ProviderError::Parse(e.to_string()))?;
        let choice = value
            .pointer("/choices/0")
            .ok_or_else(|| ProviderError::Parse("response has no choices".into()))?;
        let message = choice
            .get("message")
            .ok_or_else(|| ProviderError::Parse("choice has no message".into()))?;

        let mut content = Vec::new();
        if let Some(text) = message.get("content").and_then(Value::as_str)
            && !text.is_empty()
        {
            content.push(ContentBlock::Text { text: text.into() });
        }
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let id = call.get("id").and_then(Value::as_str);
                let name = call.pointer("/function/name").and_then(Value::as_str);
                let (Some(id), Some(name)) = (id, name) else {
                    return Err(ProviderError::Parse("tool call without id or name".into()));
                };
                let raw = call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ProviderError::Parse("tool call without string arguments".into())
                    })?;
                let arguments = if raw.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.into()))
                };
                content.push(ContentBlock::ToolCall(ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments,
                }));
            }
        }

        let stop_reason = match choice.get("finish_reason").and_then(Value::as_str) {
            Some("stop") => StopReason::EndTurn,
            Some("tool_calls" | "function_call") => StopReason::ToolUse,
            Some("length") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::Refusal,
            _ => StopReason::Other,
        };

        let usage = value.get("usage").cloned().unwrap_or(json!({}));
        let prompt = usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cached = usage
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(prompt);
        Ok(ModelResponse {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason,
            usage: Usage {
                input_tokens: prompt - cached,
                output_tokens: usage
                    .get("completion_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cache_read_tokens: cached,
                cache_write_tokens: 0,
            },
        })
    }
}

#[async_trait]
impl Provider for OpenAiCompatAdapter {
    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        let body = self.request_body(request);
        let response = self
            .transport
            .post_json(&self.url, &[], &body)
            .await
            .map_err(|e| ProviderError::Unavailable(e.to_string()))?;
        if response.status != 200 {
            return Err(status_error(&response, |v| {
                v.pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(String::from)
            }));
        }
        Self::parse_response(&response.body)
    }
}

/// One internal message can become several wire messages: tool results each
/// get their own `tool` message, ahead of any text in the same turn.
fn push_message(out: &mut Vec<Value>, message: &Message) {
    let text: Vec<&str> = message
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let text = text.join("\n\n");

    match message.role {
        Role::Assistant => {
            let calls: Vec<Value> = message
                .tool_calls()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": match &call.arguments {
                                Value::String(raw) => raw.clone(),
                                other => other.to_string(),
                            },
                        }
                    })
                })
                .collect();
            if text.is_empty() && calls.is_empty() {
                return;
            }
            let mut wire = json!({
                "role": "assistant",
                "content": if text.is_empty() { Value::Null } else { json!(text) },
            });
            if !calls.is_empty() {
                wire["tool_calls"] = json!(calls);
            }
            out.push(wire);
        }
        Role::User => {
            for block in &message.content {
                if let ContentBlock::ToolResult(result) = block {
                    let content = if result.is_error {
                        format!("error: {}", result.content)
                    } else {
                        result.content.clone()
                    };
                    out.push(json!({
                        "role": "tool",
                        "tool_call_id": result.call_id,
                        "content": content,
                    }));
                }
            }
            if !text.is_empty() {
                out.push(json!({"role": "user", "content": text}));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::testing::Replay;
    use crate::message::{ToolResult, ToolSpec};
    use crate::profile::{CachingStyle, Pricing};

    const TOOL_CALLS: &str = include_str!("../../fixtures/openai_compat/tool_calls.json");
    const STOP: &str = include_str!("../../fixtures/openai_compat/stop.json");
    const RATE_LIMITED: &str = include_str!("../../fixtures/openai_compat/error_rate_limited.json");

    fn adapter(tool_calls: ToolCallStyle, replay: Arc<Replay>) -> OpenAiCompatAdapter {
        let profile = CapabilityProfile {
            adapter: "openai_compat".into(),
            model: "fixture-model".into(),
            context_window: 128_000,
            tool_calls,
            caching: CachingStyle::AutomaticPrefix,
            structured_output: false,
            cache_ttl: None,
            pricing: Pricing::default(),
        };
        OpenAiCompatAdapter::new(profile, "http://127.0.0.1:8787/openrouter/v1", replay)
    }

    fn request(messages: Vec<Message>) -> ModelRequest {
        ModelRequest {
            profile: "helper".into(),
            system: "You are Mara.".into(),
            tools: vec![ToolSpec {
                name: "sleep".into(),
                description: "End the wake.".into(),
                parameters: json!({"type": "object", "properties": {"minutes": {"type": "integer"}}}),
            }],
            messages,
            cache_breakpoints: vec![0],
            max_output_tokens: 1024,
        }
    }

    #[tokio::test]
    async fn sends_chat_completions_without_credentials() {
        let replay = Arc::new(Replay::new(200, STOP));
        let a = adapter(ToolCallStyle::Parallel, replay.clone());
        a.complete(&request(vec![Message::user_text("hi")]))
            .await
            .unwrap();
        let sent = replay.last();
        assert_eq!(
            sent.url,
            "http://127.0.0.1:8787/openrouter/v1/chat/completions"
        );
        assert!(sent.headers.is_empty(), "keys are the proxy's job");
        assert_eq!(sent.body["model"], "fixture-model");
        assert_eq!(
            sent.body["messages"][0],
            json!({"role": "system", "content": "You are Mara."})
        );
        assert_eq!(sent.body["tools"][0]["function"]["name"], "sleep");
        assert!(sent.body.get("parallel_tool_calls").is_none());
        assert!(!sent.body.to_string().contains("cache_control"));
    }

    #[test]
    fn sequential_profiles_turn_off_parallel_calls() {
        let a = adapter(ToolCallStyle::Sequential, Arc::new(Replay::new(200, STOP)));
        let body = a.request_body(&request(vec![Message::user_text("hi")]));
        assert_eq!(body["parallel_tool_calls"], false);
    }

    #[test]
    fn converts_a_tool_round_trip() {
        let a = adapter(ToolCallStyle::Parallel, Arc::new(Replay::new(200, STOP)));
        let mut assistant = OpenAiCompatAdapter::parse_response(TOOL_CALLS)
            .unwrap()
            .message;
        assistant.content.push(ContentBlock::Opaque {
            provider: "anthropic".into(),
            data: json!({"type": "thinking"}),
        });
        let results = Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult(ToolResult {
                    call_id: "call_fixture_01".into(),
                    content: "sent".into(),
                    is_error: false,
                }),
                ContentBlock::ToolResult(ToolResult {
                    call_id: "call_fixture_02".into(),
                    content: "malformed call".into(),
                    is_error: true,
                }),
                ContentBlock::Text {
                    text: "(runtime) note".into(),
                },
            ],
        };
        let body = a.request_body(&request(vec![
            Message::user_text("wake"),
            assistant,
            results,
        ]));
        let messages = body["messages"].as_array().unwrap();
        // system, wake, assistant, two tool results, the runtime note
        assert_eq!(messages.len(), 6);
        let assistant = &messages[2];
        assert_eq!(assistant["content"], "Replying, then sleeping.");
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"],
            r#"{"text":"On it.","to":"dm"}"#
        );
        assert_eq!(
            assistant["tool_calls"][1]["function"]["arguments"], "{\"minutes\": 60",
            "a malformed call is sent back as the model wrote it"
        );
        assert!(!body.to_string().contains("thinking"));
        assert_eq!(
            messages[3],
            json!({"role": "tool", "tool_call_id": "call_fixture_01", "content": "sent"})
        );
        assert_eq!(messages[4]["content"], "error: malformed call");
        assert_eq!(
            messages[5],
            json!({"role": "user", "content": "(runtime) note"})
        );
    }

    #[test]
    fn parses_tool_calls_and_keeps_bad_arguments_for_the_executor() {
        let response = OpenAiCompatAdapter::parse_response(TOOL_CALLS).unwrap();
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        let calls: Vec<&ToolCall> = response.message.tool_calls().collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].arguments, json!({"to": "dm", "text": "On it."}));
        assert_eq!(calls[1].arguments, Value::String("{\"minutes\": 60".into()));
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 920,
                output_tokens: 64,
                cache_read_tokens: 14080,
                cache_write_tokens: 0,
            }
        );
    }

    #[test]
    fn rejects_tool_calls_without_arguments() {
        let missing = r#"{"choices":[{"message":{"tool_calls":[{"id":"call_1","function":{"name":"sleep"}}]},"finish_reason":"tool_calls"}]}"#;
        assert!(matches!(
            OpenAiCompatAdapter::parse_response(missing),
            Err(ProviderError::Parse(problem)) if problem.contains("arguments")
        ));
    }

    #[test]
    fn parses_plain_text_without_cache_details() {
        let response = OpenAiCompatAdapter::parse_response(STOP).unwrap();
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert_eq!(response.message.content.len(), 1);
        assert_eq!(response.usage.input_tokens, 20);
        assert_eq!(response.usage.cache_read_tokens, 0);
    }

    #[tokio::test]
    async fn maps_http_errors() {
        let a = adapter(
            ToolCallStyle::Parallel,
            Arc::new(Replay::new(429, RATE_LIMITED)),
        );
        assert!(matches!(
            a.complete(&request(vec![Message::user_text("hi")])).await,
            Err(ProviderError::RateLimited)
        ));
        let b = adapter(
            ToolCallStyle::Parallel,
            Arc::new(Replay::new(502, "<html>bad gateway</html>")),
        );
        assert!(matches!(
            b.complete(&request(vec![Message::user_text("hi")])).await,
            Err(ProviderError::Unavailable(m)) if m.contains("bad gateway")
        ));
    }
}
