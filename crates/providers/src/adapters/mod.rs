//! Provider adapters. Each converts the internal format to one provider's
//! wire format and back. Provider JSON never leaves this module.

pub mod anthropic;
pub mod openai_compat;

use std::sync::Arc;

use serde_json::Value;

use crate::profile::CapabilityProfile;
use crate::provider::{Provider, ProviderError};
use crate::transport::{HttpResponse, Transport};

pub use anthropic::AnthropicAdapter;
pub use openai_compat::OpenAiCompatAdapter;

/// Builds the adapter a profile names, pointed at `base_url` (the broker's or
/// forwarding proxy's route for that provider).
pub fn for_profile(
    profile: &CapabilityProfile,
    base_url: &str,
    transport: Arc<dyn Transport>,
) -> Result<Box<dyn Provider>, ProviderError> {
    match profile.adapter.as_str() {
        "anthropic" => Ok(Box::new(AnthropicAdapter::new(
            profile.clone(),
            base_url,
            transport,
        ))),
        "openai_compat" => Ok(Box::new(OpenAiCompatAdapter::new(
            profile.clone(),
            base_url,
            transport,
        ))),
        other => Err(ProviderError::Rejected(format!(
            "no adapter for {other:?} yet"
        ))),
    }
}

/// Maps an HTTP status to the error the loop and breakers act on.
fn status_error(
    response: &HttpResponse,
    message: impl Fn(&Value) -> Option<String>,
) -> ProviderError {
    let detail = serde_json::from_str::<Value>(&response.body)
        .ok()
        .and_then(|v| message(&v))
        .unwrap_or_else(|| truncate(&response.body, 300));
    match response.status {
        429 => ProviderError::RateLimited,
        408 | 500..=599 => ProviderError::Unavailable(format!("{}: {detail}", response.status)),
        status => ProviderError::Rejected(format!("{status}: {detail}")),
    }
}

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_string(),
    }
}

fn join_url(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

#[cfg(test)]
pub(crate) mod testing {
    //! A transport that replays a fixture and records what it was sent.

    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::Value;

    use crate::transport::{HttpResponse, Transport, TransportError};

    #[derive(Debug, Clone, PartialEq)]
    pub struct Sent {
        pub url: String,
        pub headers: Vec<(String, String)>,
        pub body: Value,
    }

    pub struct Replay {
        response: HttpResponse,
        pub sent: Mutex<Vec<Sent>>,
    }

    impl Replay {
        pub fn new(status: u16, body: &str) -> Self {
            Self {
                response: HttpResponse {
                    status,
                    body: body.to_string(),
                },
                sent: Mutex::new(vec![]),
            }
        }

        pub fn last(&self) -> Sent {
            self.sent
                .lock()
                .unwrap()
                .last()
                .cloned()
                .expect("nothing sent")
        }
    }

    #[async_trait]
    impl Transport for Replay {
        async fn post_json(
            &self,
            url: &str,
            headers: &[(&str, &str)],
            body: &Value,
        ) -> Result<HttpResponse, TransportError> {
            self.sent.lock().unwrap().push(Sent {
                url: url.into(),
                headers: headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                body: body.clone(),
            });
            Ok(self.response.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profiles;

    #[test]
    fn builds_the_adapter_a_profile_names() {
        let profiles = Profiles::from_toml(
            r#"
            [profile.main]
            adapter = "anthropic"
            model = "m"
            context_window = 1000
            tool_calls = "parallel"
            caching = "explicit_breakpoints"
            pricing = { input = 1.0, output = 1.0 }

            [profile.later]
            adapter = "openai_responses"
            model = "m"
            context_window = 1000
            tool_calls = "parallel"
            caching = "automatic_prefix"
            pricing = { input = 1.0, output = 1.0 }
            "#,
        )
        .unwrap();
        let transport: Arc<dyn Transport> = Arc::new(testing::Replay::new(200, "{}"));
        assert!(
            for_profile(
                profiles.get("main").unwrap(),
                "http://proxy",
                transport.clone()
            )
            .is_ok()
        );
        assert!(matches!(
            for_profile(profiles.get("later").unwrap(), "http://proxy", transport),
            Err(ProviderError::Rejected(_))
        ));
    }
}
