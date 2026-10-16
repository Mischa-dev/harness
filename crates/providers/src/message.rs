//! The internal message format. Adapters convert each provider's wire format
//! to and from these types; nothing else in the runtime sees provider JSON.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// The provider's id for this call, echoed back in the matching result.
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    /// A provider block the runtime doesn't interpret but must send back
    /// unchanged on later turns, such as an Anthropic `thinking` block. Only
    /// the adapter named by `provider` reads `data`; the others drop it.
    Opaque {
        provider: String,
        data: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// Creates a user message containing one text block.
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Creates an assistant message containing one text block.
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Iterates over tool calls in this message, ignoring other blocks.
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call),
            _ => None,
        })
    }
}

/// A tool as the model sees it. Schemas are kept simple and flat: one object
/// whose properties are primitives (PLAN.md, D13).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    /// The capability profile name, resolved to a provider model id by the adapter.
    pub profile: String,
    /// The static prefix: charter, SOUL and anything else that never changes within a day.
    pub system: String,
    pub tools: Vec<ToolSpec>,
    pub messages: Vec<Message>,
    /// Indices into `messages` after which a cache breakpoint belongs. Adapters
    /// with explicit breakpoints use them; automatic-prefix adapters ignore them.
    pub cache_breakpoints: Vec<usize>,
    pub max_output_tokens: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    /// The provider's safety system declined to answer.
    Refusal,
    Other,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Uncached input tokens.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl Usage {
    /// Adds another response's token counts to this total.
    pub fn add(&mut self, other: Usage) {
        // Usage is provider-controlled data. Saturate instead of allowing a
        // bogus counter to panic debug builds or wrap release builds.
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelResponse {
    pub message: Message,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_counters_saturate_on_untrusted_provider_values() {
        let mut usage = Usage {
            input_tokens: u64::MAX,
            output_tokens: u64::MAX,
            cache_read_tokens: u64::MAX,
            cache_write_tokens: u64::MAX,
        };
        usage.add(Usage {
            input_tokens: 1,
            output_tokens: 1,
            cache_read_tokens: 1,
            cache_write_tokens: 1,
        });
        assert_eq!(usage.input_tokens, u64::MAX);
        assert_eq!(usage.output_tokens, u64::MAX);
        assert_eq!(usage.cache_read_tokens, u64::MAX);
        assert_eq!(usage.cache_write_tokens, u64::MAX);
    }

    #[test]
    fn tool_calls_select_only_calls_and_preserve_order() {
        let first = ToolCall {
            id: "1".into(),
            name: "read".into(),
            arguments: json!({"path": "README.md"}),
        };
        let second = ToolCall {
            id: "2".into(),
            name: "sleep".into(),
            arguments: json!({"minutes": 60}),
        };
        let message = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "I'll check.".into(),
                },
                ContentBlock::ToolCall(first.clone()),
                ContentBlock::Opaque {
                    provider: "test".into(),
                    data: json!({"token": 1}),
                },
                ContentBlock::ToolCall(second.clone()),
            ],
        };

        assert_eq!(
            message.tool_calls().cloned().collect::<Vec<_>>(),
            vec![first, second]
        );
    }

    #[test]
    fn usage_add_accumulates_every_token_class() {
        let mut total = Usage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 3,
            cache_write_tokens: 4,
        };
        total.add(Usage {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 30,
            cache_write_tokens: 40,
        });
        assert_eq!(
            total,
            Usage {
                input_tokens: 11,
                output_tokens: 22,
                cache_read_tokens: 33,
                cache_write_tokens: 44
            }
        );
    }

    #[test]
    fn internal_messages_round_trip_without_losing_opaque_blocks() {
        let message = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Opaque {
                provider: "anthropic".into(),
                data: json!({"type": "thinking", "signature": "scrubbed"}),
            }],
        };
        let encoded = serde_json::to_string(&message).unwrap();
        assert_eq!(serde_json::from_str::<Message>(&encoded).unwrap(), message);
    }
}
