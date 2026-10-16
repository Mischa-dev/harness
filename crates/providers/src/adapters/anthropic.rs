//! Anthropic Messages (`POST /v1/messages`).
//!
//! The static prefix gets a cache breakpoint on the system prompt, which also
//! covers the tool definitions (the API renders tools, then system, then
//! messages). The loop's message breakpoints land on the last cacheable block
//! of each marked message. Thinking blocks come back as opaque blocks and are
//! replayed unchanged, as the API requires for later turns.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use super::{join_url, status_error};
use crate::message::{
    ContentBlock, Message, ModelRequest, ModelResponse, Role, StopReason, ToolCall, Usage,
};
use crate::profile::{CachingStyle, CapabilityProfile};
use crate::provider::{Provider, ProviderError};
use crate::transport::Transport;

pub const PROVIDER: &str = "anthropic";
const API_VERSION: &str = "2023-06-01";
/// The API allows four breakpoints per request; one goes on the system prompt.
const MAX_MESSAGE_BREAKPOINTS: usize = 3;

pub struct AnthropicAdapter {
    profile: CapabilityProfile,
    url: String,
    transport: Arc<dyn Transport>,
}

impl AnthropicAdapter {
    pub fn new(profile: CapabilityProfile, base_url: &str, transport: Arc<dyn Transport>) -> Self {
        Self {
            profile,
            url: join_url(base_url, "v1/messages"),
            transport,
        }
    }

    fn cache_control(&self) -> Option<Value> {
        if self.profile.caching != CachingStyle::ExplicitBreakpoints {
            return None;
        }
        let mut cc = json!({"type": "ephemeral"});
        if let Some(ttl) = &self.profile.cache_ttl {
            cc["ttl"] = json!(ttl);
        }
        Some(cc)
    }

    pub fn request_body(&self, request: &ModelRequest) -> Value {
        let cache_control = self.cache_control();
        let mut body = Map::new();
        body.insert("model".into(), json!(self.profile.model));
        body.insert("max_tokens".into(), json!(request.max_output_tokens));

        if !request.system.is_empty() {
            let mut block = json!({"type": "text", "text": request.system});
            if let Some(cc) = &cache_control {
                block["cache_control"] = cc.clone();
            }
            body.insert("system".into(), json!([block]));
        }

        if !request.tools.is_empty() {
            let tools: Vec<Value> = request
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect();
            body.insert("tools".into(), json!(tools));
        }

        let breakpoints: Vec<usize> = request
            .cache_breakpoints
            .iter()
            .rev()
            .take(MAX_MESSAGE_BREAKPOINTS)
            .copied()
            .collect();
        let messages: Vec<Value> = request
            .messages
            .iter()
            .enumerate()
            .filter_map(|(i, message)| {
                let mut blocks = message_blocks(message);
                if blocks.is_empty() {
                    return None;
                }
                if let (Some(cc), true) = (&cache_control, breakpoints.contains(&i))
                    && let Some(last) = blocks.iter_mut().rev().find(|b| is_cacheable(b))
                {
                    last["cache_control"] = cc.clone();
                }
                Some(json!({"role": role(message.role), "content": blocks}))
            })
            .collect();
        body.insert("messages".into(), json!(messages));

        Value::Object(body)
    }

    pub fn parse_response(body: &str) -> Result<ModelResponse, ProviderError> {
        let value: Value =
            serde_json::from_str(body).map_err(|e| ProviderError::Parse(e.to_string()))?;
        let blocks = value
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| ProviderError::Parse("response has no content array".into()))?;

        let mut content = Vec::with_capacity(blocks.len());
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => content.push(ContentBlock::Text {
                    text: str_field(block, "text")?.to_string(),
                }),
                Some("tool_use") => content.push(ContentBlock::ToolCall(ToolCall {
                    id: str_field(block, "id")?.to_string(),
                    name: str_field(block, "name")?.to_string(),
                    arguments: block.get("input").cloned().unwrap_or(json!({})),
                })),
                _ => content.push(ContentBlock::Opaque {
                    provider: PROVIDER.into(),
                    data: block.clone(),
                }),
            }
        }

        let stop_reason = match value.get("stop_reason").and_then(Value::as_str) {
            Some("end_turn" | "stop_sequence") => StopReason::EndTurn,
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            Some("refusal") => StopReason::Refusal,
            _ => StopReason::Other,
        };

        let usage = value.get("usage").cloned().unwrap_or(json!({}));
        let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
        Ok(ModelResponse {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason,
            usage: Usage {
                input_tokens: count("input_tokens"),
                output_tokens: count("output_tokens"),
                cache_read_tokens: count("cache_read_input_tokens"),
                cache_write_tokens: count("cache_creation_input_tokens"),
            },
        })
    }
}

#[async_trait]
impl Provider for AnthropicAdapter {
    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        let body = self.request_body(request);
        let response = self
            .transport
            .post_json(&self.url, &[("anthropic-version", API_VERSION)], &body)
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

fn role(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

fn message_blocks(message: &Message) -> Vec<Value> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
            ContentBlock::ToolCall(call) => Some(json!({
                "type": "tool_use",
                "id": call.id,
                "name": call.name,
                "input": call.arguments,
            })),
            ContentBlock::ToolResult(result) => Some(json!({
                "type": "tool_result",
                "tool_use_id": result.call_id,
                "content": result.content,
                "is_error": result.is_error,
            })),
            ContentBlock::Opaque { provider, data } if provider == PROVIDER => Some(data.clone()),
            ContentBlock::Opaque { .. } => None,
        })
        .collect()
}

/// Thinking blocks can't carry a cache breakpoint; the block before them can.
fn is_cacheable(block: &Value) -> bool {
    !matches!(
        block.get("type").and_then(Value::as_str),
        Some("thinking" | "redacted_thinking")
    )
}

fn str_field<'a>(block: &'a Value, key: &str) -> Result<&'a str, ProviderError> {
    block
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::Parse(format!("block is missing `{key}`")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::testing::Replay;
    use crate::message::{ToolResult, ToolSpec};
    use crate::profile::{Pricing, ToolCallStyle};

    const TOOL_USE: &str = include_str!("../../fixtures/anthropic/tool_use_with_thinking.json");
    const END_TURN: &str = include_str!("../../fixtures/anthropic/end_turn.json");
    const REFUSAL: &str = include_str!("../../fixtures/anthropic/refusal.json");
    const OVERLOADED: &str = include_str!("../../fixtures/anthropic/error_overloaded.json");
    const INVALID: &str = include_str!("../../fixtures/anthropic/error_invalid_request.json");

    fn profile(caching: CachingStyle) -> CapabilityProfile {
        CapabilityProfile {
            adapter: "anthropic".into(),
            model: "fixture-model".into(),
            context_window: 200_000,
            tool_calls: ToolCallStyle::Parallel,
            caching,
            structured_output: false,
            cache_ttl: Some("1h".into()),
            pricing: Pricing::default(),
        }
    }

    fn adapter(caching: CachingStyle, replay: Arc<Replay>) -> AnthropicAdapter {
        AnthropicAdapter::new(profile(caching), "http://127.0.0.1:8787/anthropic/", replay)
    }

    fn request(messages: Vec<Message>, cache_breakpoints: Vec<usize>) -> ModelRequest {
        ModelRequest {
            profile: "main".into(),
            system: "You are Mara.".into(),
            tools: vec![ToolSpec {
                name: "sleep".into(),
                description: "End the wake.".into(),
                parameters: json!({"type": "object", "properties": {"minutes": {"type": "integer"}}}),
            }],
            messages,
            cache_breakpoints,
            max_output_tokens: 4096,
        }
    }

    #[tokio::test]
    async fn sends_the_messages_shape_without_credentials() {
        let replay = Arc::new(Replay::new(200, END_TURN));
        let a = adapter(CachingStyle::ExplicitBreakpoints, replay.clone());
        a.complete(&request(vec![Message::user_text("hi")], vec![]))
            .await
            .unwrap();

        let sent = replay.last();
        assert_eq!(sent.url, "http://127.0.0.1:8787/anthropic/v1/messages");
        assert_eq!(
            sent.headers,
            vec![("anthropic-version".to_string(), API_VERSION.to_string())]
        );
        assert!(
            sent.headers
                .iter()
                .all(|(k, _)| k != "x-api-key" && k != "authorization"),
            "keys are the proxy's job"
        );
        assert_eq!(sent.body["model"], "fixture-model");
        assert_eq!(sent.body["max_tokens"], 4096);
        assert_eq!(sent.body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(
            sent.body["system"][0],
            json!({
                "type": "text",
                "text": "You are Mara.",
                "cache_control": {"type": "ephemeral", "ttl": "1h"}
            })
        );
        assert!(sent.body.get("thinking").is_none());
        assert!(sent.body.get("temperature").is_none());
    }

    #[test]
    fn breakpoints_land_on_the_last_cacheable_block() {
        let a = adapter(
            CachingStyle::ExplicitBreakpoints,
            Arc::new(Replay::new(200, END_TURN)),
        );
        let mut thinking_then_call = AnthropicAdapter::parse_response(TOOL_USE).unwrap().message;
        // Move the thinking block last, so the breakpoint has to skip it.
        thinking_then_call.content.rotate_left(1);
        let body = a.request_body(&request(
            vec![
                Message::user_text("memory"),
                Message::user_text("wake"),
                thinking_then_call,
                Message::user_text("results"),
            ],
            vec![0, 2],
        ));
        let messages = body["messages"].as_array().unwrap();
        assert!(messages[0]["content"][0].get("cache_control").is_some());
        assert!(messages[1]["content"][0].get("cache_control").is_none());
        let marked: Vec<&str> = messages[2]["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b.get("cache_control").is_some())
            .map(|b| b["type"].as_str().unwrap())
            .collect();
        assert_eq!(marked, vec!["tool_use"]);
        assert!(messages[3]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn no_more_than_four_breakpoints() {
        let a = adapter(
            CachingStyle::ExplicitBreakpoints,
            Arc::new(Replay::new(200, END_TURN)),
        );
        let messages = (0..6)
            .map(|i| Message::user_text(format!("m{i}")))
            .collect();
        let body = a.request_body(&request(messages, vec![0, 1, 2, 3, 4, 5]));
        let text = body.to_string();
        assert_eq!(text.matches("cache_control").count(), 4);
        // The newest breakpoints win; the system prompt keeps its own.
        assert!(
            body["messages"][5]["content"][0]
                .get("cache_control")
                .is_some()
        );
        assert!(
            body["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );
    }

    #[test]
    fn no_cache_control_when_the_profile_does_not_cache() {
        let a = adapter(CachingStyle::None, Arc::new(Replay::new(200, END_TURN)));
        let body = a.request_body(&request(vec![Message::user_text("hi")], vec![0]));
        assert!(!body.to_string().contains("cache_control"));
    }

    #[test]
    fn parses_text_tool_use_thinking_and_cache_usage() {
        let response = AnthropicAdapter::parse_response(TOOL_USE).unwrap();
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert!(matches!(
            &response.message.content[0],
            ContentBlock::Opaque { provider, data } if provider == PROVIDER && data["type"] == "thinking"
        ));
        assert!(matches!(
            &response.message.content[1],
            ContentBlock::Text { text } if text.starts_with("Nothing urgent")
        ));
        let call = response.message.tool_calls().next().unwrap();
        assert_eq!(call.id, "toolu_fixture_01");
        assert_eq!(call.name, "sleep");
        assert_eq!(call.arguments["minutes"], 240);
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 312,
                output_tokens: 87,
                cache_read_tokens: 14200,
                cache_write_tokens: 1024,
            }
        );
    }

    #[test]
    fn replays_thinking_blocks_unchanged_and_drops_other_providers() {
        let a = adapter(
            CachingStyle::ExplicitBreakpoints,
            Arc::new(Replay::new(200, END_TURN)),
        );
        let raw: Value = serde_json::from_str(TOOL_USE).unwrap();
        let mut assistant = AnthropicAdapter::parse_response(TOOL_USE).unwrap().message;
        assistant.content.push(ContentBlock::Opaque {
            provider: "openai_responses".into(),
            data: json!({"type": "reasoning"}),
        });
        let results = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult(ToolResult {
                call_id: "toolu_fixture_01".into(),
                content: "Sleeping.".into(),
                is_error: false,
            })],
        };
        let body = a.request_body(&request(
            vec![Message::user_text("wake"), assistant, results],
            vec![],
        ));
        let replayed = &body["messages"][1]["content"];
        assert_eq!(replayed.as_array().unwrap().len(), 3);
        assert_eq!(replayed[0], raw["content"][0], "thinking block verbatim");
        assert_eq!(replayed[2]["type"], "tool_use");
        assert_eq!(
            body["messages"][2]["content"][0],
            json!({
                "type": "tool_result",
                "tool_use_id": "toolu_fixture_01",
                "content": "Sleeping.",
                "is_error": false
            })
        );
    }

    #[test]
    fn maps_stop_reasons() {
        assert_eq!(
            AnthropicAdapter::parse_response(END_TURN)
                .unwrap()
                .stop_reason,
            StopReason::EndTurn
        );
        let refusal = AnthropicAdapter::parse_response(REFUSAL).unwrap();
        assert_eq!(refusal.stop_reason, StopReason::Refusal);
        assert!(refusal.message.content.is_empty());
    }

    #[test]
    fn rejects_malformed_bodies() {
        assert!(matches!(
            AnthropicAdapter::parse_response("not json"),
            Err(ProviderError::Parse(_))
        ));
        assert!(matches!(
            AnthropicAdapter::parse_response(r#"{"content":[{"type":"tool_use","name":"x"}]}"#),
            Err(ProviderError::Parse(_))
        ));
    }

    #[tokio::test]
    async fn maps_http_errors() {
        let req = request(vec![Message::user_text("hi")], vec![]);
        let overloaded = adapter(CachingStyle::None, Arc::new(Replay::new(529, OVERLOADED)));
        assert!(matches!(
            overloaded.complete(&req).await,
            Err(ProviderError::Unavailable(m)) if m.contains("Overloaded")
        ));
        let limited = adapter(CachingStyle::None, Arc::new(Replay::new(429, OVERLOADED)));
        assert!(matches!(
            limited.complete(&req).await,
            Err(ProviderError::RateLimited)
        ));
        let invalid = adapter(CachingStyle::None, Arc::new(Replay::new(400, INVALID)));
        assert!(matches!(
            invalid.complete(&req).await,
            Err(ProviderError::Rejected(m)) if m.contains("roles must alternate")
        ));
    }
}
