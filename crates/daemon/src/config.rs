//! The daemon's config file. It holds no secrets: keys are named by file.

use std::path::Path;

use serde::Deserialize;

use crate::proxy::{ConfigError, ProxyConfig};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub proxy: ProxyConfig,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        toml::from_str(&text).map_err(|e| ConfigError::Invalid(e.to_string()))
    }
}
