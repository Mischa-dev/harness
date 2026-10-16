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
        let text = std::fs::read_to_string(path).map_err(ConfigError::Read)?;
        toml::from_str(&text).map_err(|e| ConfigError::Invalid(where_it_fails(&text, &e)))
    }
}

/// Where a config file fails to parse, and nothing more: toml's message
/// quotes the offending line and serde's can quote a value, either of which
/// may be a key someone pasted inline.
fn where_it_fails(text: &str, err: &toml::de::Error) -> String {
    let Some(span) = err.span() else {
        return "the file doesn't parse".to_string();
    };
    let before = &text[..span.start.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.chars().rev().take_while(|&c| c != '\n').count() + 1;
    format!("the file doesn't parse at line {line}, column {column}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_text(text: &str) -> Result<Config, ConfigError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.toml");
        std::fs::write(&path, text).unwrap();
        Config::load(&path)
    }

    #[test]
    fn parse_errors_never_echo_the_file() {
        for text in [
            // A key pasted under a field that doesn't exist.
            "[proxy]\nlisten = \"127.0.0.1:8700\"\nsecret = \"sk-super-secret\"\n",
            // A key pasted where a table belongs.
            "proxy = \"sk-super-secret\"\n",
            // A key pasted as the wrong type.
            "[proxy]\nlisten = \"127.0.0.1:8700\"\nroute = \"sk-super-secret\"\n",
            // A key that breaks the syntax.
            "[proxy]\nsk-super-secret\n",
        ] {
            let err = load_text(text).unwrap_err().to_string();
            assert!(!err.contains("sk-super"), "{err}");
            assert!(err.contains("line"), "{err}");
        }
    }

    #[test]
    fn read_errors_never_echo_the_path() {
        let err = Config::load(Path::new("sk-super-secret")).unwrap_err();
        assert!(!err.to_string().contains("sk-super"), "{err}");
    }

    #[test]
    fn locations_count_from_one() {
        let err = load_text("[proxy]\nlisten = \"127.0.0.1:8700\"\nsecret = 1\n").unwrap_err();
        assert!(err.to_string().contains("line 3, column 1"), "{err}");
    }
}
