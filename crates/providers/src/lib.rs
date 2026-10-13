//! The provider layer. Everything outside this crate speaks the internal
//! format in [`message`]; provider-specific JSON stays in the adapters here.

pub mod fake;
pub mod message;
pub mod profile;
pub mod provider;

pub use fake::ScriptedProvider;
pub use message::{
    ContentBlock, Message, ModelRequest, ModelResponse, Role, StopReason, ToolCall, ToolResult,
    ToolSpec, Usage,
};
pub use profile::{
    CachingStyle, CapabilityProfile, Pricing, ProfileError, Profiles, ToolCallStyle,
};
pub use provider::{Provider, ProviderError};
