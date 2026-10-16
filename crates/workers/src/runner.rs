//! Running workers: each one is a coding CLI in its own worktree, holding
//! the paths it may change, with a time limit, and no team credentials. Its
//! environment is cleared down to what the CLI needs to find its tools and
//! the owner's login, so no Workspace token or model key reaches it.
//!
//! When a worker ends, for whatever reason, the runtime records what it said
//! and what it changed, gives its paths back and tells the agent.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use harness_core::Clock;
use harness_store::{LockPath, NewWorker, WorkerBook, WorkerRecord, WorkerState};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::kind::{self, Kind, KindConfig};
use crate::worktree::{Worktree, deleted_tests, outside_locks, protected_changes};

/// The brief around the agent's words; product code, like the charter.
const PROMPT: &str = include_str!("../../../prompts/worker.md");

/// The concurrent workers circuit breaker (ARCHITECTURE.md), enforced as a
/// refusal.
pub const DEFAULT_MAX_RUNNING: usize = 4;
/// A brief's limit, in characters.
pub const MAX_BRIEF_CHARS: usize = 20_000;
/// The most paths one worker may lock.
pub const MAX_PATHS: usize = 20;
/// How long a lock lasts without renewal. A running worker renews every
/// third of it.
pub const LEASE: time::Duration = time::Duration::minutes(5);
/// How much of a worker's output is read, and of its answer kept.
const MAX_OUTPUT_BYTES: u64 = 1 << 20;
const MAX_SUMMARY_CHARS: usize = 4_000;
/// What a worker's process keeps of the runtime's environment: enough to
/// find programs and the owner's CLI login, nothing else.
const KEPT_ENV: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TERM", "TMPDIR",
];

/// `[agent.workers]` in the daemon's config.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkersConfig {
    /// Repositories workers may work in, by the name the agent uses.
    #[serde(default)]
    pub repos: BTreeMap<String, PathBuf>,
    /// Where worktrees go. Defaults to `worktrees` in the state directory.
    #[serde(default)]
    pub worktrees_dir: Option<PathBuf>,
    #[serde(default = "default_max_running")]
    pub max_running: usize,
    /// Paths no worker may change, in any repository: CI, review rules,
    /// agent instructions. A worker that changes one fails, whatever else
    /// it did.
    #[serde(default = "default_protected")]
    pub protected: Vec<String>,
    #[serde(default)]
    pub claude_code: Option<KindConfig>,
    #[serde(default)]
    pub codex: Option<KindConfig>,
}

fn default_max_running() -> usize {
    DEFAULT_MAX_RUNNING
}

fn default_protected() -> Vec<String> {
    [
        ".github",
        ".gitlab-ci.yml",
        ".claude",
        ".codex",
        "AGENTS.md",
        "CLAUDE.md",
        "CODEOWNERS",
        "REVIEW.md",
    ]
    .map(String::from)
    .to_vec()
}

impl WorkersConfig {
    pub fn check(&self) -> Result<(), String> {
        if self.max_running == 0 || self.max_running > 16 {
            return Err("workers.max_running must be between 1 and 16".into());
        }
        if self.repos.is_empty() {
            return Err("workers.repos must name at least one repository".into());
        }
        for (name, path) in &self.repos {
            let plain = name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
            if name.is_empty() || name.starts_with('.') || !plain {
                return Err(format!("workers.repos: {name:?} isn't a plain name"));
            }
            if !path.is_absolute() {
                return Err(format!("workers.repos.{name} must be an absolute path"));
            }
        }
        for path in &self.protected {
            if LockPath::parse(path).is_none_or(|p| p.as_str().is_empty()) {
                return Err(format!(
                    "workers.protected: {path:?} isn't a path inside a repository"
                ));
            }
        }
        if self.claude_code.is_none() && self.codex.is_none() {
            return Err("workers needs a [claude_code] or [codex] section".into());
        }
        for kind in Kind::ALL {
            if let Some(config) = self.kind(kind) {
                config.check(kind)?;
            }
        }
        Ok(())
    }

    /// The protected paths, as lock paths.
    pub fn protected_paths(&self) -> Vec<LockPath> {
        self.protected
            .iter()
            .filter_map(|p| LockPath::parse(p))
            .collect()
    }

    fn kind(&self, kind: Kind) -> Option<&KindConfig> {
        match kind {
            Kind::ClaudeCode => self.claude_code.as_ref(),
            Kind::Codex => self.codex.as_ref(),
        }
    }
}

/// A worker that ended, for the agent's next wake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub id: i64,
    /// One line on how it ended.
    pub line: String,
}

/// What the agent asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub kind: Kind,
    pub repo: String,
    pub brief: String,
    pub paths: Vec<LockPath>,
    /// The commit or branch to start from; the repository's `HEAD` if none.
    pub base: Option<String>,
}

pub struct Workers {
    pub book: Arc<WorkerBook>,
    clock: Arc<dyn Clock>,
    config: WorkersConfig,
    worktrees: PathBuf,
    agent_name: String,
    /// How a running worker is told to stop.
    stops: Mutex<HashMap<i64, oneshot::Sender<()>>>,
    done: mpsc::UnboundedSender<Finished>,
    count: AtomicU64,
}

impl Workers {
    /// Workers as `config` says, reporting to `done` when each one ends.
    pub fn new(
        book: Arc<WorkerBook>,
        clock: Arc<dyn Clock>,
        config: WorkersConfig,
        worktrees: PathBuf,
        agent_name: &str,
        done: mpsc::UnboundedSender<Finished>,
    ) -> Result<Self, String> {
        config.check()?;
        harness_loop::charter::render(PROMPT, &prompt_values(agent_name, "x", "x", "x"))
            .map_err(|e| format!("prompts/worker.md: {e}"))?;
        Ok(Self {
            book,
            clock,
            config,
            worktrees,
            agent_name: agent_name.to_string(),
            stops: Mutex::new(HashMap::new()),
            done,
            count: AtomicU64::new(0),
        })
    }

    pub fn repos(&self) -> impl Iterator<Item = &str> {
        self.config.repos.keys().map(String::as_str)
    }

    pub fn kinds(&self) -> impl Iterator<Item = Kind> + '_ {
        Kind::ALL
            .into_iter()
            .filter(|k| self.config.kind(*k).is_some())
    }

    pub fn program(&self, kind: Kind) -> Option<&Path> {
        self.config.kind(kind).map(|c| c.program.as_path())
    }

    /// Workers the book says are running but no process here runs: the
    /// runtime restarted under them. Each is ended as failed and reported,
    /// so its paths come back and the agent hears. Its worktree stays for
    /// the agent to look at.
    pub fn recover(&self) -> Result<usize, harness_store::StoreError> {
        let orphans = self.book.running()?;
        for worker in &orphans {
            let summary = "The runtime restarted while it ran. Its worktree is still there.";
            if self
                .book
                .end(worker.id, WorkerState::Failed, summary, self.clock.now())?
            {
                let _ = self.done.send(Finished {
                    id: worker.id,
                    line: line(worker, WorkerState::Failed, summary),
                });
            }
        }
        Ok(orphans.len())
    }

    /// Starts a worker, or says why not.
    pub async fn spawn(self: &Arc<Self>, request: Request) -> Result<WorkerRecord, String> {
        let config = self
            .config
            .kind(request.kind)
            .ok_or_else(|| format!("no {} workers are set up", request.kind.as_str()))?
            .clone();
        let repo_path = self
            .config
            .repos
            .get(&request.repo)
            .ok_or_else(|| {
                let names: Vec<_> = self.repos().collect();
                format!(
                    "{:?} isn't a repository workers can use; these are: {}",
                    request.repo,
                    names.join(", ")
                )
            })?
            .clone();
        let brief = request.brief.trim();
        if brief.is_empty() || brief.chars().count() > MAX_BRIEF_CHARS {
            return Err(format!(
                "the brief must be 1 to {MAX_BRIEF_CHARS} characters"
            ));
        }
        if request.paths.is_empty() || request.paths.len() > MAX_PATHS {
            return Err(format!("name 1 to {MAX_PATHS} paths the worker may change"));
        }
        let running = self.book.running().map_err(|e| e.to_string())?;
        if running.len() >= self.config.max_running {
            return Err(format!(
                "{} workers are running already, the most at once; wait for one or stop one",
                running.len()
            ));
        }

        let now = self.clock.now();
        let name = format!(
            "{}-{}-{}",
            request.repo,
            now.unix_timestamp(),
            self.count.fetch_add(1, Ordering::Relaxed)
        );
        let dir = self.worktrees.join(&name);
        let branch = format!("harness/worker-{name}");
        let base = request.base.clone().unwrap_or_else(|| "HEAD".into());
        std::fs::create_dir_all(&self.worktrees)
            .map_err(|e| format!("couldn't create the worktrees directory: {e}"))?;
        let tree = {
            let (repo_path, dir, branch) = (repo_path.clone(), dir.clone(), branch.clone());
            tokio::task::spawn_blocking(move || Worktree::create(&repo_path, &dir, &branch, &base))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?
        };
        let new = NewWorker {
            kind: request.kind.as_str().into(),
            repo: request.repo.clone(),
            base: tree.base.clone(),
            branch: tree.branch.clone(),
            worktree: tree.dir.display().to_string(),
            brief: brief.to_string(),
        };
        let started = self
            .book
            .start(&new, &request.paths, LEASE, self.clock.now());
        let record = match started {
            Ok(Ok(record)) => record,
            Ok(Err(conflict)) => {
                let _ = tree.remove();
                return Err(format!("{conflict}; wait for it, or stop it"));
            }
            Err(e) => {
                let _ = tree.remove();
                return Err(e.to_string());
            }
        };
        let paths = record
            .paths
            .iter()
            .map(|p| {
                format!(
                    "- {}",
                    if p.as_str().is_empty() {
                        "."
                    } else {
                        p.as_str()
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let protected = self
            .config
            .protected
            .iter()
            .map(|p| format!("- {p}"))
            .collect::<Vec<_>>()
            .join("\n");
        let text = harness_loop::charter::render(
            PROMPT,
            &prompt_values(&self.agent_name, &paths, &protected, brief),
        )
        .map_err(|e| format!("prompts/worker.md: {e}"))?;
        let (stop_tx, stop_rx) = oneshot::channel();
        self.stops
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(record.id, stop_tx);
        let this = self.clone();
        let (kind, worker) = (request.kind, record.clone());
        tokio::spawn(async move { this.run(kind, config, worker, tree, text, stop_rx).await });
        Ok(record)
    }

    /// Asks a running worker to stop. A worker the book has as running
    /// with no process here is ended on the spot.
    pub fn stop(&self, id: i64) -> Result<String, String> {
        let sender = self
            .stops
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
        if let Some(sender) = sender {
            let _ = sender.send(());
            return Ok(format!("Stopping worker #{id}. You'll hear when it has."));
        }
        match self.book.get(id).map_err(|e| e.to_string())? {
            None => Err(format!("there's no worker #{id}")),
            Some(w) if w.state != WorkerState::Running => {
                Err(format!("worker #{id} has already {}", w.state.as_str()))
            }
            Some(w) => {
                let summary = "Stopped by the agent; no process was running it.";
                self.book
                    .end(w.id, WorkerState::Stopped, summary, self.clock.now())
                    .map_err(|e| e.to_string())?;
                Ok(format!(
                    "Worker #{id} had no process running; it's marked stopped."
                ))
            }
        }
    }

    async fn run(
        self: Arc<Self>,
        kind: Kind,
        config: KindConfig,
        worker: WorkerRecord,
        tree: Worktree,
        brief: String,
        stop: oneshot::Receiver<()>,
    ) {
        let last_message = self
            .worktrees
            .join(format!("{}.last-message.txt", worker.id));
        let (mut state, summary) = match self
            .supervise(kind, &config, &worker, &tree, &brief, &last_message, stop)
            .await
        {
            Ok(outcome) => outcome,
            Err(problem) => (WorkerState::Failed, problem),
        };
        let _ = std::fs::remove_file(&last_message);
        let mut summary = summary;
        let changes = {
            let tree = tree.clone();
            tokio::task::spawn_blocking(move || tree.changes()).await
        };
        match changes {
            Ok(Ok(changes)) => {
                let outside: Vec<_> = outside_locks(&changes, &worker.paths)
                    .iter()
                    .map(|c| c.path.clone())
                    .collect();
                summary.push_str(&format!(
                    "\n\n{} changed.",
                    plural(changes.len(), "path", "paths")
                ));
                if !outside.is_empty() {
                    summary.push_str(&format!(
                        " Outside its locked paths: {}.",
                        outside.join(", ")
                    ));
                }
                // Walls first, so the wake's one line says so.
                let mut walls = vec![];
                let protected = protected_changes(&changes, &self.config.protected_paths());
                if !protected.is_empty() {
                    let names: Vec<_> = protected.iter().map(|c| c.path.as_str()).collect();
                    walls.push(format!("It changed protected files: {}.", names.join(", ")));
                }
                let deleted = deleted_tests(&changes);
                if !deleted.is_empty() {
                    let names: Vec<_> = deleted.iter().map(|c| c.path.as_str()).collect();
                    walls.push(format!("It deleted tests: {}.", names.join(", ")));
                }
                if !walls.is_empty() {
                    state = WorkerState::Failed;
                    summary = format!(
                        "{} Don't accept this as it is.\n\n{summary}",
                        walls.join(" ")
                    );
                }
            }
            Ok(Err(e)) => summary.push_str(&format!("\n\nIts changes couldn't be read: {e}")),
            Err(e) => summary.push_str(&format!("\n\nIts changes couldn't be read: {e}")),
        }
        self.stops
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&worker.id);
        match self.book.end(worker.id, state, &summary, self.clock.now()) {
            Ok(true) => {
                let _ = self.done.send(Finished {
                    id: worker.id,
                    line: line(&worker, state, &summary),
                });
            }
            Ok(false) => {}
            Err(e) => eprintln!(
                "harness: couldn't record the end of worker #{}: {e}",
                worker.id
            ),
        }
    }

    /// Runs the worker's process to its end, renewing its locks, and says
    /// how it ended.
    #[allow(clippy::too_many_arguments)]
    async fn supervise(
        &self,
        kind: Kind,
        config: &KindConfig,
        worker: &WorkerRecord,
        tree: &Worktree,
        brief: &str,
        last_message: &Path,
        mut stop: oneshot::Receiver<()>,
    ) -> Result<(WorkerState, String), String> {
        let mut command = tokio::process::Command::new(&config.program);
        command
            .args(kind::args(
                kind,
                config,
                &tree.dir,
                last_message,
                &self.config.protected_paths(),
            ))
            .current_dir(&tree.dir)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        for key in KEPT_ENV {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("couldn't start {}: {e}", config.program.display()))?;
        let pid = child.id();
        // Written alongside, so a worker that never reads its brief can't
        // hold up the time limit or a stop.
        if let Some(mut stdin) = child.stdin.take() {
            let brief = brief.to_string();
            tokio::spawn(async move {
                let _ = stdin.write_all(brief.as_bytes()).await;
            });
        }
        let stdout = child
            .stdout
            .take()
            .map(|out| tokio::spawn(read_capped(out)));
        let stderr = child
            .stderr
            .take()
            .map(|err| tokio::spawn(read_capped(err)));

        let limit = tokio::time::sleep(config.timeout());
        tokio::pin!(limit);
        let renew_every = (LEASE / 3_i32).unsigned_abs();
        let mut renew =
            tokio::time::interval_at(tokio::time::Instant::now() + renew_every, renew_every);
        let ended = loop {
            tokio::select! {
                status = child.wait() => break Ok(status.map_err(|e| e.to_string())?),
                _ = &mut stop => break Err("Stopped by the agent.".to_string()),
                () = &mut limit => break Err(format!(
                    "Stopped: it ran past its {}-minute limit.",
                    config.timeout_minutes
                )),
                _ = renew.tick() => match self.book.renew(worker.id, LEASE, self.clock.now()) {
                    Ok(true) => {}
                    Ok(false) => break Err("Stopped: it lost its path locks.".to_string()),
                    Err(e) => break Err(format!("Stopped: its locks couldn't be renewed ({e}).")),
                },
            }
        };
        let status = match ended {
            Ok(status) => status,
            Err(why) => {
                kill_group(pid);
                let _ = child.kill().await;
                return Ok((WorkerState::Stopped, why));
            }
        };
        let stdout = match stdout {
            Some(handle) => handle.await.unwrap_or_default(),
            None => vec![],
        };
        let stderr = match stderr {
            Some(handle) => handle.await.unwrap_or_default(),
            None => vec![],
        };
        let last = std::fs::read(last_message).ok().map(|bytes| {
            String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_OUTPUT_BYTES as usize)])
                .into_owned()
        });
        let answer = kind::answer(kind, &String::from_utf8_lossy(&stdout), last.as_deref());
        let mut text = cut(&answer.text, MAX_SUMMARY_CHARS);
        if text.is_empty() {
            text = cut(String::from_utf8_lossy(&stderr).trim(), 1_000);
        }
        if let Some(cost) = answer.cost_usd {
            text.push_str(&format!("\n\n(It cost ${cost:.2}.)"));
        }
        if status.success() && !answer.is_error {
            Ok((WorkerState::Finished, text))
        } else {
            Ok((
                WorkerState::Failed,
                format!("It exited with {status}.\n\n{text}"),
            ))
        }
    }
}

fn prompt_values<'a>(
    agent: &'a str,
    paths: &'a str,
    protected: &'a str,
    brief: &'a str,
) -> [(&'static str, &'a str); 4] {
    [
        ("agent_name", agent),
        ("paths", paths),
        ("protected", protected),
        ("brief", brief),
    ]
}

/// Kills the worker's whole process group: the CLI and anything it ran.
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

async fn read_capped(reader: impl tokio::io::AsyncRead + Unpin) -> Vec<u8> {
    let mut out = Vec::new();
    let _ = reader.take(MAX_OUTPUT_BYTES).read_to_end(&mut out).await;
    out
}

/// One line on how a worker ended, for the wake message.
fn line(worker: &WorkerRecord, state: WorkerState, summary: &str) -> String {
    let first = summary
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no report");
    format!(
        "Worker #{} ({} in {}) {}: {}",
        worker.id,
        worker.kind,
        worker.repo,
        state.as_str(),
        cut(first, 200)
    )
}

fn cut(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}
