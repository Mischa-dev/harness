//! The walls. [`Policy::check`] is a pure function of the policy and one
//! effect; the executor adds the stateful parts (breakers and pause).

use std::path::{Component, Path, PathBuf};

use crate::tool::Effect;

/// A command rule matches when the program's file name equals `program` and
/// every one of `args` appears somewhere in the argument list. An empty
/// `args` forbids the program outright.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRule {
    pub program: String,
    pub args: Vec<String>,
}

impl CommandRule {
    pub fn program(program: &str) -> Self {
        Self {
            program: program.into(),
            args: vec![],
        }
    }

    pub fn with_args(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
        }
    }

    fn matches(&self, program: &str, args: &[String]) -> bool {
        program_name(program) == self.program
            && self.args.iter().all(|wanted| args.contains(wanted))
    }
}

/// Starting values from ARCHITECTURE.md, "Circuit breakers". Set high on
/// purpose: tripping one means something is broken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakerLimits {
    pub messages_per_hour: usize,
}

impl Default for BreakerLimits {
    fn default() -> Self {
        Self {
            messages_per_hour: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// The agent's working area. Paths outside it can't be read or written.
    pub root: PathBuf,
    /// Paths under `root` that can be read but never written.
    pub protected_paths: Vec<PathBuf>,
    /// Credential-bearing paths that the agent can neither read nor write.
    pub secret_paths: Vec<PathBuf>,
    pub forbidden_commands: Vec<CommandRule>,
    /// Actions that need the owner's yes before they run.
    pub approval_required: Vec<String>,
    pub breakers: BreakerLimits,
}

impl Policy {
    /// A conservative starting policy: CI and tests are protected, shells and
    /// other ways of hiding a command are forbidden, and anything that leaves
    /// the team needs a yes.
    pub fn conservative(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            protected_paths: [".github", "tests"].iter().map(PathBuf::from).collect(),
            secret_paths: [
                ".env",
                ".git",
                ".ssh",
                ".aws",
                ".config/gcloud",
                ".npmrc",
                ".pypirc",
                ".netrc",
            ]
            .iter()
            .map(PathBuf::from)
            .collect(),
            forbidden_commands: vec![
                CommandRule::program("sh"),
                CommandRule::program("bash"),
                CommandRule::program("zsh"),
                CommandRule::program("dash"),
                CommandRule::program("fish"),
                CommandRule::program("env"),
                CommandRule::program("xargs"),
                CommandRule::program("sudo"),
                CommandRule::program("eval"),
                CommandRule::with_args("git", &["push", "--force"]),
                CommandRule::with_args("git", &["push", "-f"]),
                CommandRule::with_args("git", &["push", "--force-with-lease"]),
            ],
            approval_required: ["merge", "publish", "spend", "contact_outside"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            breakers: BreakerLimits::default(),
        }
    }

    pub fn check(&self, effect: &Effect) -> Decision {
        match effect {
            Effect::ReadPath(path) => match self.resolve(path) {
                Some(rel) if self.secret_path(&rel).is_some() => {
                    Decision::deny(format!("{} contains credentials", path.display()))
                }
                Some(_) => Decision::Allow,
                None => Decision::deny(format!("{} is outside the work area", path.display())),
            },
            Effect::WritePath(path) => match self.resolve(path) {
                None => Decision::deny(format!("{} is outside the work area", path.display())),
                Some(rel) => match self
                    .protected_paths
                    .iter()
                    .chain(self.secret_paths.iter())
                    .find(|p| rel.starts_with(p) || env_variant(&rel, p))
                {
                    Some(p) => Decision::deny(format!("{} is protected", p.display())),
                    None => Decision::Allow,
                },
            },
            Effect::RunCommand { program, args } => {
                if is_dangerous_git_push(program, args) {
                    return Decision::deny("git force push is not allowed".into());
                }
                match self
                    .forbidden_commands
                    .iter()
                    .find(|rule| rule.matches(program, args))
                {
                    Some(rule) if rule.args.is_empty() => {
                        Decision::deny(format!("running {} is not allowed", rule.program))
                    }
                    Some(rule) => Decision::deny(format!(
                        "{} {} is not allowed",
                        rule.program,
                        rule.args.join(" ")
                    )),
                    None => Decision::Allow,
                }
            }
            Effect::SendMessage { .. } => Decision::Allow,
            Effect::Action { name } => {
                if self.approval_required.iter().any(|a| a == name) {
                    Decision::NeedsApproval {
                        reason: format!("{name} needs the owner's yes"),
                    }
                } else {
                    Decision::Allow
                }
            }
        }
    }

    fn secret_path<'a>(&'a self, path: &Path) -> Option<&'a PathBuf> {
        self.secret_paths
            .iter()
            .find(|protected| path.starts_with(protected) || env_variant(path, protected))
    }

    /// Resolves `path` to a path relative to `root`, or `None` if it falls
    /// outside. Checked lexically first, then again after following symlinks
    /// through the deepest part of the path that exists.
    fn resolve(&self, path: &Path) -> Option<PathBuf> {
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let lexical = normalize(&joined)?;
        let root = normalize(&self.root)?;
        let rel = lexical.strip_prefix(&root).ok()?.to_path_buf();

        // A symlink inside the root can point outside it, or into a protected
        // path under another name. Resolve what exists and check again.
        if let (Ok(real_root), Some(real)) = (root.canonicalize(), canonicalize_existing(&lexical))
        {
            let real_rel = real.strip_prefix(&real_root).ok()?.to_path_buf();
            if real_rel != rel {
                return Some(real_rel);
            }
        }
        Some(rel)
    }
}

fn env_variant(path: &Path, protected: &Path) -> bool {
    protected == Path::new(".env")
        && path
            .components()
            .next()
            .and_then(|part| part.as_os_str().to_str())
            .is_some_and(|name| name.starts_with(".env."))
}

/// Git accepts long options with `=`, combined short flags, and options before
/// the subcommand. Match those forms rather than relying on exact argv items.
fn is_dangerous_git_push(program: &str, args: &[String]) -> bool {
    if program_name(program) != "git" {
        return false;
    }
    let Some(push_at) = args.iter().position(|arg| arg == "push") else {
        return false;
    };
    args[push_at + 1..].iter().any(|arg| {
        arg == "--force"
            || arg.starts_with("--force=")
            || arg == "--force-with-lease"
            || arg.starts_with("--force-with-lease=")
            || (arg.starts_with('-')
                && !arg.starts_with("--")
                && arg[1..].chars().any(|flag| flag == 'f'))
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny {
        reason: String,
    },
    NeedsApproval {
        reason: String,
    },
    /// A circuit breaker tripped; the agent is paused until the owner resumes it.
    Tripped {
        breaker: String,
    },
}

impl Decision {
    fn deny(reason: String) -> Self {
        Decision::Deny { reason }
    }
}

fn program_name(program: &str) -> &str {
    Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program)
}

/// Removes `.` and resolves `..` without touching the file system. Returns
/// `None` if `..` would climb above the start of the path.
fn normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() || out.as_os_str().is_empty() {
                    return None;
                }
            }
            other => out.push(other),
        }
    }
    Some(out)
}

/// Canonicalizes the deepest existing ancestor of `path` and re-appends the rest.
fn canonicalize_existing(path: &Path) -> Option<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = existing.canonicalize() {
            let mut out = real;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return Some(out);
        }
        rest.push(existing.file_name()?.to_os_string());
        if !existing.pop() {
            return None;
        }
    }
}
