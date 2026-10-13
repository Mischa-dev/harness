//! Capability profiles: one small record per model, loaded from config. This
//! and config are the only places a provider model name may appear.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::message::Usage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStyle {
    /// The model handles several tool calls in one turn reliably.
    Parallel,
    /// The loop asks for, and runs, one tool call per turn.
    Sequential,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CachingStyle {
    /// Anthropic-style explicit cache breakpoints.
    ExplicitBreakpoints,
    /// OpenAI-style automatic prefix caching.
    AutomaticPrefix,
    None,
}

/// List prices in US dollars per million tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Pricing {
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityProfile {
    /// Which adapter talks to it: `openai_compat`, `anthropic` or `openai_responses`.
    pub adapter: String,
    /// The provider's model id.
    pub model: String,
    pub context_window: u32,
    pub tool_calls: ToolCallStyle,
    pub caching: CachingStyle,
    #[serde(default)]
    pub structured_output: bool,
    /// Cache lifetime for explicit breakpoints: `5m` or `1h`. Unset uses the
    /// provider's default. `pricing.cache_write` must match the choice.
    #[serde(default)]
    pub cache_ttl: Option<String>,
    pub pricing: Pricing,
}

impl CapabilityProfile {
    pub fn cost_usd(&self, usage: &Usage) -> f64 {
        let p = &self.pricing;
        let per_token = |tokens: u64, price: f64| tokens as f64 * price / 1_000_000.0;
        per_token(usage.input_tokens, p.input)
            + per_token(usage.output_tokens, p.output)
            + per_token(usage.cache_read_tokens, p.cache_read)
            + per_token(usage.cache_write_tokens, p.cache_write)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("invalid profiles file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("profile {name:?}: {problem}")]
    Invalid { name: String, problem: String },
    #[error("no profile named {0:?}")]
    Unknown(String),
}

const ADAPTERS: &[&str] = &["openai_compat", "anthropic", "openai_responses"];

/// The set of profiles from config, keyed by the name roles refer to.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Profiles {
    #[serde(rename = "profile")]
    by_name: BTreeMap<String, CapabilityProfile>,
}

impl Profiles {
    /// Parses a TOML document of `[profile.<name>]` tables.
    pub fn from_toml(text: &str) -> Result<Self, ProfileError> {
        let profiles: Profiles = toml::from_str(text)?;
        for (name, profile) in &profiles.by_name {
            let invalid = |problem: &str| ProfileError::Invalid {
                name: name.clone(),
                problem: problem.into(),
            };
            if !ADAPTERS.contains(&profile.adapter.as_str()) {
                return Err(invalid("unknown adapter"));
            }
            if profile.model.trim().is_empty() {
                return Err(invalid("model is empty"));
            }
            if profile.context_window == 0 {
                return Err(invalid("context_window must be positive"));
            }
            if let Some(ttl) = &profile.cache_ttl {
                if !matches!(ttl.as_str(), "5m" | "1h") {
                    return Err(invalid("cache_ttl must be 5m or 1h"));
                }
            }
            let p = &profile.pricing;
            if [p.input, p.output, p.cache_read, p.cache_write]
                .iter()
                .any(|price| !price.is_finite() || *price < 0.0)
            {
                return Err(invalid("prices must be non-negative numbers"));
            }
        }
        Ok(profiles)
    }

    pub fn get(&self, name: &str) -> Result<&CapabilityProfile, ProfileError> {
        self.by_name
            .get(name)
            .ok_or_else(|| ProfileError::Unknown(name.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [profile.main]
        adapter = "anthropic"
        model = "example-mid"
        context_window = 200000
        tool_calls = "parallel"
        caching = "explicit_breakpoints"
        pricing = { input = 3.0, output = 15.0, cache_read = 0.3, cache_write = 6.0 }

        [profile.helper]
        adapter = "openai_compat"
        model = "example-small"
        context_window = 128000
        tool_calls = "sequential"
        caching = "automatic_prefix"
        structured_output = true
        pricing = { input = 0.5, output = 2.0 }
    "#;

    #[test]
    fn parses_profiles_from_config() {
        let profiles = Profiles::from_toml(SAMPLE).unwrap();
        let helper = profiles.get("helper").unwrap();
        assert_eq!(helper.tool_calls, ToolCallStyle::Sequential);
        assert!(helper.structured_output);
        assert_eq!(helper.pricing.cache_read, 0.0);
        assert!(matches!(
            profiles.get("missing"),
            Err(ProfileError::Unknown(_))
        ));
    }

    #[test]
    fn cost_counts_every_token_class() {
        let profiles = Profiles::from_toml(SAMPLE).unwrap();
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            cache_read_tokens: 2_000_000,
            cache_write_tokens: 500_000,
        };
        let cost = profiles.get("main").unwrap().cost_usd(&usage);
        assert!((cost - (3.0 + 1.5 + 0.6 + 3.0)).abs() < 1e-9);
    }

    #[test]
    fn rejects_unknown_adapter_and_negative_prices() {
        let bad_adapter = SAMPLE.replace("\"anthropic\"", "\"carrier-pigeon\"");
        assert!(matches!(
            Profiles::from_toml(&bad_adapter),
            Err(ProfileError::Invalid { .. })
        ));
        let negative = SAMPLE.replace("input = 0.5", "input = -0.5");
        assert!(matches!(
            Profiles::from_toml(&negative),
            Err(ProfileError::Invalid { .. })
        ));
    }
}
