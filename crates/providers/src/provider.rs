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

/// Completes provider-independent model requests.
///
/// Adapters for model endpoints own all provider wire-format conversion; the
/// scripted fake implements the same interface. Callers elsewhere in the
/// runtime deal only in the internal message types.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Sends one model request and returns its normalized response.
    async fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError>;
}
