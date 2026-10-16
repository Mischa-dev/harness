//! `spawn_worker`, `worker_status` and `stop_worker`: the agent hands a
//! well-defined task to a coding CLI, checks on it, and stops it. Spawning
//! and stopping declare actions, so the policy executor can put them behind
//! the owner's yes; the worker's own CLI is declared as the command it runs.

use std::sync::Arc;

use async_trait::async_trait;
use harness_providers::ToolSpec;
use harness_store::{LockPath, WorkerRecord, WorkerState};
use harness_tools::{Effect, Tool, ToolContext, ToolError, ToolOutput};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;

use crate::kind::Kind;
use crate::runner::{MAX_BRIEF_CHARS, MAX_PATHS, Request, Workers};

/// How much of a finished worker's diff `worker_status` shows.
const MAX_DIFF_BYTES: usize = 20_000;
/// How many recent workers the overview lists.
const RECENT: usize = 8;

pub fn tools(workers: Arc<Workers>) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(SpawnWorker(workers.clone())),
        Box::new(WorkerStatus(workers.clone())),
        Box::new(StopWorker(workers)),
    ]
}

/// One line for a worker, as the tools and the state view show it.
pub fn describe(worker: &WorkerRecord) -> String {
    let since = worker
        .started_at
        .format(&Rfc3339)
        .unwrap_or_else(|_| worker.started_at.to_string());
    let brief = worker
        .brief
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let brief: String = brief.chars().take(80).collect();
    format!(
        "#{} {} in {}, {} since {since}: {brief}",
        worker.id,
        worker.kind,
        worker.repo,
        worker.state.as_str()
    )
}

struct SpawnWorker(Arc<Workers>);

impl SpawnWorker {
    fn request(&self, args: &Value) -> Result<Request, ToolError> {
        let text = |name: &str| args.get(name).and_then(Value::as_str);
        let kind_name = text("kind").unwrap_or_default();
        let kind = Kind::parse(kind_name).ok_or_else(|| {
            ToolError::InvalidArguments(format!("kind {kind_name:?} isn't a kind of worker"))
        })?;
        let paths = args
            .get("paths")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .map(|p| {
                        let p = p.as_str().unwrap_or_default();
                        LockPath::parse(p).ok_or_else(|| {
                            ToolError::InvalidArguments(format!(
                                "{p:?} isn't a path inside the repository"
                            ))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        Ok(Request {
            kind,
            repo: text("repo").unwrap_or_default().to_string(),
            brief: text("brief").unwrap_or_default().to_string(),
            paths,
            base: text("base").map(str::to_string),
        })
    }
}

#[async_trait]
impl Tool for SpawnWorker {
    fn spec(&self) -> ToolSpec {
        let kinds: Vec<&str> = self.0.kinds().map(Kind::as_str).collect();
        let repos: Vec<&str> = self.0.repos().collect();
        ToolSpec {
            name: "spawn_worker".into(),
            description: format!(
                "Hand a well-defined task to a coding worker: a fresh session in a git worktree \
                 of its own, on a new branch, that can change only the paths you name. It knows \
                 nothing but your brief, so give it the goal, the context, how to check the \
                 result, constraints, and what's out of scope. You're told when it ends; then \
                 judge its diff with worker_status. Kinds: {}. Repositories: {}.",
                kinds.join(", "),
                repos.join(", ")
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "description": kinds.join(" or ")},
                    "repo": {"type": "string"},
                    "brief": {
                        "type": "string",
                        "description": format!("At most {MAX_BRIEF_CHARS} characters.")
                    },
                    "paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": format!(
                            "Repo-relative files or directories it may change, 1 to {MAX_PATHS}; \
                             \".\" for the whole repository. No other worker may hold an \
                             overlapping path while it runs."
                        )
                    },
                    "base": {
                        "type": "string",
                        "description": "Commit or branch to start from; HEAD if left out."
                    }
                },
                "required": ["kind", "repo", "brief", "paths"]
            }),
        }
    }

    fn effects(&self, args: &Value) -> Result<Vec<Effect>, ToolError> {
        let request = self.request(args)?;
        let program = self.0.program(request.kind).ok_or_else(|| {
            ToolError::InvalidArguments(format!("no {} workers are set up", request.kind.as_str()))
        })?;
        Ok(vec![
            Effect::Action {
                name: "spawn_worker".into(),
            },
            Effect::RunCommand {
                program: program.display().to_string(),
                args: vec![],
            },
        ])
    }

    async fn run(&self, args: &Value, _: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let record = self
            .0
            .spawn(self.request(args)?)
            .await
            .map_err(ToolError::Failed)?;
        let paths: Vec<String> = record.paths.iter().map(ToString::to_string).collect();
        Ok(ToolOutput::text(format!(
            "Started worker #{} ({} in {}) on branch {}, holding {}. You'll be told when it ends.",
            record.id,
            record.kind,
            record.repo,
            record.branch,
            paths.join(", ")
        )))
    }
}

struct WorkerStatus(Arc<Workers>);

#[async_trait]
impl Tool for WorkerStatus {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "worker_status".into(),
            description: "Without `worker`: your running and recent workers. With it: that \
                          worker's brief, state and report, and once it has ended, its diff \
                          and any changes outside the paths it held."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {"worker": {"type": "integer"}}
            }),
        }
    }

    fn effects(&self, _: &Value) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![])
    }

    async fn run(&self, args: &Value, _: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let failed = |e: harness_store::StoreError| ToolError::Failed(e.to_string());
        let Some(id) = args.get("worker").and_then(Value::as_i64) else {
            let recent = self.0.book.recent(RECENT).map_err(failed)?;
            if recent.is_empty() {
                return Ok(ToolOutput::text("You haven't spawned any workers."));
            }
            let lines: Vec<String> = recent
                .iter()
                .map(|w| format!("- {}", describe(w)))
                .collect();
            return Ok(ToolOutput::text(lines.join("\n")));
        };
        let worker = self
            .0
            .book
            .get(id)
            .map_err(failed)?
            .ok_or_else(|| ToolError::Failed(format!("there's no worker #{id}")))?;
        let mut text = format!(
            "{}\nBranch {} from {}, worktree {}\nBrief:\n{}",
            describe(&worker),
            worker.branch,
            worker.base,
            worker.worktree,
            worker.brief
        );
        if worker.state == WorkerState::Running {
            let paths: Vec<String> = worker.paths.iter().map(ToString::to_string).collect();
            text.push_str(&format!("\n\nHolding: {}", paths.join(", ")));
            return Ok(ToolOutput::text(text));
        }
        if let Some(summary) = &worker.summary {
            text.push_str(&format!("\n\nIts report:\n{summary}"));
        }
        let tree = crate::Worktree {
            repo: std::path::PathBuf::new(),
            dir: worker.worktree.clone().into(),
            branch: worker.branch.clone(),
            base: worker.base.clone(),
        };
        if tree.dir.exists() {
            match tokio::task::spawn_blocking(move || tree.diff(MAX_DIFF_BYTES)).await {
                Ok(Ok(diff)) => text.push_str(&format!("\n\nDiff:\n{diff}")),
                Ok(Err(e)) => text.push_str(&format!("\n\nIts diff couldn't be read: {e}")),
                Err(e) => text.push_str(&format!("\n\nIts diff couldn't be read: {e}")),
            }
        } else {
            text.push_str("\n\nIts worktree is gone.");
        }
        Ok(ToolOutput::text(text))
    }
}

struct StopWorker(Arc<Workers>);

#[async_trait]
impl Tool for StopWorker {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "stop_worker".into(),
            description: "Stop a running worker. Its worktree and changes so far stay for \
                          worker_status."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {"worker": {"type": "integer"}},
                "required": ["worker"]
            }),
        }
    }

    fn effects(&self, _: &Value) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![Effect::Action {
            name: "stop_worker".into(),
        }])
    }

    async fn run(&self, args: &Value, _: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let id = args
            .get("worker")
            .and_then(Value::as_i64)
            .ok_or_else(|| ToolError::InvalidArguments("worker must be a number".into()))?;
        self.0
            .stop(id)
            .map(ToolOutput::text)
            .map_err(ToolError::Failed)
    }
}
