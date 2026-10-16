//! The provider layer. Everything outside this crate speaks the internal
//! format in [`message`]; provider-specific JSON stays in the adapters here.

pub mod adapters;
pub mod fake;
pub mod message;
pub mod profile;
pub mod provider;
pub mod transport;

pub use fake::ScriptedProvider;
pub use message::{
    ContentBlock, Message, ModelRequest, ModelResponse, Role, StopReason, ToolCall, ToolResult,
    ToolSpec, Usage,
};
pub use profile::{
    CachingStyle, CapabilityProfile, Pricing, ProfileError, Profiles, ToolCallStyle,
};
pub use provider::{Provider, ProviderError};
#[cfg(feature = "http")]
pub use transport::ReqwestTransport;
pub use transport::{CALLER_TOKEN_HEADER, HttpResponse, Transport, TransportError};
