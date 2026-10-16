//! The kinds of worker the runtime can run, and how each one's command line
//! is built and its answer read. Each is a coding CLI run headless in the
//! worker's worktree, with the brief on standard input so nothing in it can
//! be read as a flag.

use std::path::{Path, PathBuf};
use std::time::Duration;

use harness_store::LockPath;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Claude Code, `claude -p`, on the owner's Claude plan.
    ClaudeCode,
    /// Codex, `codex exec`, on the owner's ChatGPT plan.
    Codex,
}

impl Kind {
    pub const ALL: [Kind; 2] = [Kind::ClaudeCode, Kind::Codex];

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "claude_code" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude_code",
            Self::Codex => "codex",
        }
    }
}

/// How one kind runs, from the agent's config.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KindConfig {
    /// The CLI, by path or by name on `PATH`.
    pub program: PathBuf,
    /// The model, if not the CLI's default.
    #[serde(default)]
    pub model: Option<String>,
    /// How long a worker may run before it's stopped.
    #[serde(default = "default_timeout_minutes")]
    pub timeout_minutes: u32,
    /// Claude Code only: the most one worker may spend.
    #[serde(default)]
    pub max_budget_usd: Option<f64>,
}

fn default_timeout_minutes() -> u32 {
    60
}

impl KindConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(u64::from(self.timeout_minutes) * 60)
    }

    pub fn check(&self, kind: Kind) -> Result<(), String> {
        let name = kind.as_str();
        if self.program.as_os_str().is_empty() {
            return Err(format!("workers.{name}.program is empty"));
        }
        if self.timeout_minutes == 0 || self.timeout_minutes > 24 * 60 {
            return Err(format!(
                "workers.{name}.timeout_minutes must be between 1 and 1440"
            ));
        }
        if let Some(model) = &self.model
            && (model.is_empty() || model.starts_with('-') || model.contains(char::is_whitespace))
        {
            return Err(format!("workers.{name}.model isn't a model name"));
        }
        match (kind, self.max_budget_usd) {
            (_, None) => {}
            (Kind::ClaudeCode, Some(usd)) if usd.is_finite() && usd > 0.0 => {}
            (Kind::ClaudeCode, Some(_)) => {
                return Err(format!(
                    "workers.{name}.max_budget_usd must be a positive amount"
                ));
            }
            (Kind::Codex, Some(_)) => {
                return Err(format!(
                    "workers.{name}.max_budget_usd: Codex has no budget flag"
                ));
            }
        }
        Ok(())
    }
}

/// Built-in tools a Claude Code worker may use without asking. No shell
/// until worker sessions run inside the sandbox; anything else is refused,
/// since nobody is there to say yes.
const CLAUDE_TOOLS: &[&str] = &["Read", "Edit", "Write", "Glob", "Grep"];

/// The command line for a worker of `kind` in `dir`. `last_message` is a
/// file the worker may write its final answer to. Claude Code is told not
/// to edit `protected` paths; for every kind the runtime checks afterwards.
pub fn args(
    kind: Kind,
    config: &KindConfig,
    dir: &Path,
    last_message: &Path,
    protected: &[LockPath],
) -> Vec<String> {
    let mut args: Vec<String> = match kind {
        Kind::ClaudeCode => {
            let mut args = vec![
                "-p".into(),
                "--output-format".into(),
                "json".into(),
                "--permission-mode".into(),
                "dontAsk".into(),
                // No MCP servers: a worker has no team tools.
                "--strict-mcp-config".into(),
                "--allowedTools".into(),
                CLAUDE_TOOLS.join(","),
            ];
            if !protected.is_empty() {
                let rules: Vec<String> = protected
                    .iter()
                    .flat_map(|p| {
                        let p = p.as_str();
                        ["Edit", "Write"].into_iter().flat_map(move |tool| {
                            [format!("{tool}({p})"), format!("{tool}({p}/**)")]
                        })
                    })
                    .collect();
                args.extend(["--disallowedTools".into(), rules.join(",")]);
            }
            if let Some(usd) = config.max_budget_usd {
                args.extend(["--max-budget-usd".into(), format!("{usd}")]);
            }
            args
        }
        Kind::Codex => vec![
            "exec".into(),
            "--sandbox".into(),
            "workspace-write".into(),
            "--cd".into(),
            dir.display().to_string(),
            "--output-last-message".into(),
            last_message.display().to_string(),
        ],
    };
    if let Some(model) = &config.model {
        args.extend(["--model".into(), model.clone()]);
    }
    if kind == Kind::Codex {
        // Read the brief from standard input.
        args.push("-".into());
    }
    args
}

/// What a worker said when it ended.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Answer {
    pub text: String,
    /// Whether the CLI says it failed.
    pub is_error: bool,
    /// The CLI's session, for steering later.
    pub session: Option<String>,
    pub cost_usd: Option<f64>,
}

/// Reads a worker's answer from its standard output and, for Codex, the
/// last-message file.
pub fn answer(kind: Kind, stdout: &str, last_message: Option<&str>) -> Answer {
    match kind {
        Kind::ClaudeCode => {
            // The JSON result is the last line; anything before it is noise.
            let parsed = stdout
                .lines()
                .rev()
                .find_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
                .filter(|v| v.is_object());
            match parsed {
                Some(v) => Answer {
                    text: v["result"].as_str().unwrap_or_default().to_string(),
                    is_error: v["is_error"].as_bool().unwrap_or(true)
                        || v["subtype"].as_str().is_some_and(|s| s != "success"),
                    session: v["session_id"].as_str().map(str::to_string),
                    cost_usd: v["total_cost_usd"].as_f64(),
                },
                None => Answer {
                    text: stdout.trim().to_string(),
                    is_error: true,
                    ..Answer::default()
                },
            }
        }
        Kind::Codex => {
            let text = last_message
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| stdout.trim());
            Answer {
                text: text.to_string(),
                is_error: last_message.is_none_or(|t| t.trim().is_empty()),
                ..Answer::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> KindConfig {
        KindConfig {
            program: "claude".into(),
            model: None,
            timeout_minutes: 60,
            max_budget_usd: Some(2.5),
        }
    }

    #[test]
    fn claude_code_runs_headless_without_mcp_or_a_shell() {
        let protected = [LockPath::parse(".github").unwrap()];
        let args = args(
            Kind::ClaudeCode,
            &config(),
            Path::new("/w"),
            Path::new("/x"),
            &protected,
        );
        let joined = args.join(" ");
        assert!(joined.starts_with("-p --output-format json --permission-mode dontAsk"));
        assert!(joined.contains("--strict-mcp-config"));
        assert!(joined.contains("--allowedTools Read,Edit,Write,Glob,Grep"));
        assert!(joined.contains(
            "--disallowedTools Edit(.github),Edit(.github/**),Write(.github),Write(.github/**)"
        ));
        assert!(joined.ends_with("--max-budget-usd 2.5"));
        assert!(!joined.contains("Bash"));
    }

    #[test]
    fn codex_runs_in_its_sandbox_and_reads_the_brief_from_stdin() {
        let config = KindConfig {
            program: "codex".into(),
            model: Some("gpt-x".into()),
            max_budget_usd: None,
            ..config()
        };
        let args = args(
            Kind::Codex,
            &config,
            Path::new("/w/1"),
            Path::new("/s/1.txt"),
            &[LockPath::parse(".github").unwrap()],
        );
        assert_eq!(
            args,
            [
                "exec",
                "--sandbox",
                "workspace-write",
                "--cd",
                "/w/1",
                "--output-last-message",
                "/s/1.txt",
                "--model",
                "gpt-x",
                "-"
            ]
        );
    }

    #[test]
    fn configs_are_checked() {
        assert!(config().check(Kind::ClaudeCode).is_ok());
        assert!(config().check(Kind::Codex).is_err(), "codex has no budget");
        for bad in [
            KindConfig {
                timeout_minutes: 0,
                ..config()
            },
            KindConfig {
                model: Some("--dangerously".into()),
                ..config()
            },
            KindConfig {
                max_budget_usd: Some(f64::NAN),
                ..config()
            },
            KindConfig {
                program: "".into(),
                ..config()
            },
        ] {
            assert!(bad.check(Kind::ClaudeCode).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn claude_answers_are_read_from_the_json_result() {
        let out = "warming up\n{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\
                   \"result\":\"Fixed it.\",\"session_id\":\"s-1\",\"total_cost_usd\":0.42}\n";
        assert_eq!(
            answer(Kind::ClaudeCode, out, None),
            Answer {
                text: "Fixed it.".into(),
                is_error: false,
                session: Some("s-1".into()),
                cost_usd: Some(0.42),
            }
        );
        let limit = "{\"subtype\":\"error_max_budget_usd\",\"is_error\":false}";
        assert!(answer(Kind::ClaudeCode, limit, None).is_error);
        let garbage = answer(Kind::ClaudeCode, "segfault", None);
        assert!(garbage.is_error);
        assert_eq!(garbage.text, "segfault");
    }

    #[test]
    fn codex_answers_come_from_the_last_message() {
        let done = answer(Kind::Codex, "log lines", Some("Done.\n"));
        assert_eq!(done.text, "Done.");
        assert!(!done.is_error);
        let none = answer(Kind::Codex, "crashed", None);
        assert_eq!(none.text, "crashed");
        assert!(none.is_error);
    }
}
