use async_trait::async_trait;

use crate::message::{ModelRequest, ModelResponse};

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("rate limited by the provider")]
    RateLimited,
    #[error("provider unavailable: {0}")]
    Unavailable(String),
    #[error("could not parse provider response: {0}")]
    Parse(String),
    #[error("request rejected: {0}")]
    Rejected(String),
}

/// One model endpoint. Adapters (OpenAI-compatible, Anthropic Messages,
/// OpenAI Responses) implement this; so does the scripted fake.
#[async_trait]
pub trait Provider: Send + Sync {
    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError>;
}
