//! `run_command`: one program with an argument array, run inside a
//! bubblewrap sandbox. Never a shell string: the model names the program
//! and each argument, and nothing parses them.
//!
//! The executor's walls judge the call first. The sandbox is the wall that
//! holds while the program runs, whatever it turns out to do:
//! - the work area is the only place it can write, mounted at `/work`;
//! - protected paths in it are read-only, and every path the walls say holds
//!   credentials is hidden, at any depth;
//! - the system's programs and libraries are read-only, and nothing else of
//!   the host is there: no home folders, no runtime state, no sockets;
//! - no network, its own process, IPC and host namespaces, a fresh `/tmp`,
//!   and an environment with only `PATH`, `HOME` and `LANG`;
//! - it's stopped after its time runs out, along with everything it started.
//!
//! Limits on memory and process counts are still to come.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use harness_providers::ToolSpec;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::policy::Policy;
use crate::tool::{Effect, Tool, ToolContext, ToolError, ToolOutput};

pub const DEFAULT_TIMEOUT_SECONDS: u64 = 120;
pub const MAX_TIMEOUT_SECONDS: u64 = 600;
/// What's kept of each stream: its start and its end.
const KEEP_HEAD: usize = 24_000;
const KEEP_TAIL: usize = 8_000;
/// How many entries of the work area are looked at for credentials before
/// giving up rather than running with some left in view.
const MAX_SCAN: usize = 200_000;
const MAX_HIDDEN: usize = 5_000;
/// How long to wait for the last output once the program has ended.
const DRAIN: Duration = Duration::from_secs(5);

/// Where the work area appears inside the sandbox.
const WORK: &str = "/work";
/// Read-only system paths, where they exist. `/etc` holds only what any
/// user on the host may read; `/nix/store` and `/run/current-system` are
/// where NixOS keeps its programs.
const SYSTEM: [&str; 9] = [
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/etc",
    "/nix/store",
    "/run/current-system",
];
const PATH: &str = "/run/current-system/sw/bin:/usr/local/bin:/usr/bin:/bin";

/// Whether `path`, a real path, is among the host folders the sandbox
/// shows. Whatever the runtime keeps from the agent must not be.
pub fn in_view(path: &Path) -> bool {
    SYSTEM.iter().any(|system| {
        let system = Path::new(system);
        path.starts_with(system)
            || system
                .canonicalize()
                .is_ok_and(|real| path.starts_with(real))
    })
}

pub struct ShellTool {
    policy: Policy,
    /// The work area's real path, found at startup.
    real_root: PathBuf,
    bwrap: PathBuf,
    /// More paths under the root that commands may read but not change.
    read_only: Vec<PathBuf>,
}

impl ShellTool {
    /// Calls are judged against `policy`; the sandbox mounts `real_root`,
    /// the work area's real path, and is made by the `bwrap` program.
    pub fn new(policy: Policy, real_root: PathBuf, bwrap: PathBuf) -> Self {
        Self {
            policy,
            real_root,
            bwrap,
            read_only: vec![],
        }
    }

    /// Keeps `rel`, a path under the root, read-only for commands, such as a
    /// folder only the runtime's own tools should change.
    pub fn read_only(mut self, rel: impl Into<PathBuf>) -> Self {
        self.read_only.push(rel.into());
        self
    }

    /// The bubblewrap arguments up to the program, for a command run in
    /// `dir` under the work area.
    fn sandbox(&self, dir: &Path) -> Result<Vec<OsString>, ToolError> {
        let mut argv: Vec<OsString> = [
            "--die-with-parent",
            "--new-session",
            "--unshare-all",
            "--clearenv",
            "--setenv",
            "PATH",
            PATH,
            "--setenv",
            "HOME",
            "/tmp",
            "--setenv",
            "LANG",
            "C.UTF-8",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        for path in SYSTEM {
            push(&mut argv, ["--ro-bind-try", path, path]);
        }
        let work = Path::new(WORK);
        push(
            &mut argv,
            [
                "--bind".as_ref(),
                self.real_root.as_os_str(),
                work.as_os_str(),
            ],
        );

        for rel in self.policy.protected_paths.iter().chain(&self.read_only) {
            let real = self.real_root.join(rel);
            let inside = work.join(rel);
            match std::fs::symlink_metadata(&real) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(ToolError::Failed(format!(
                        "{} is a link, so commands can't be kept from changing it; \
                         nothing ran",
                        rel.display()
                    )));
                }
                Ok(_) => push(
                    &mut argv,
                    ["--ro-bind".as_ref(), real.as_os_str(), inside.as_os_str()],
                ),
                // Not there yet: an empty, read-only stand-in keeps commands
                // from making it.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    push(&mut argv, ["--tmpfs".as_ref(), inside.as_os_str()]);
                    push(&mut argv, ["--remount-ro".as_ref(), inside.as_os_str()]);
                }
                Err(e) => {
                    return Err(ToolError::Failed(format!(
                        "couldn't check {}: {}; nothing ran",
                        rel.display(),
                        e.kind()
                    )));
                }
            }
        }

        for (rel, is_dir) in self.secrets()? {
            let inside = work.join(&rel);
            if is_dir {
                push(&mut argv, ["--tmpfs".as_ref(), inside.as_os_str()]);
                push(&mut argv, ["--remount-ro".as_ref(), inside.as_os_str()]);
            } else {
                push(
                    &mut argv,
                    [
                        "--ro-bind".as_ref(),
                        "/dev/null".as_ref(),
                        inside.as_os_str(),
                    ],
                );
            }
        }

        push(&mut argv, ["--dev", "/dev"]);
        push(&mut argv, ["--proc", "/proc"]);
        push(&mut argv, ["--tmpfs", "/tmp"]);
        // Nothing outside the work area and `/tmp` takes writes.
        push(&mut argv, ["--remount-ro", "/"]);
        push(&mut argv, ["--chdir".as_ref(), work.join(dir).as_os_str()]);
        Ok(argv)
    }

    /// Every path in the work area the walls say holds credentials, with
    /// whether it's a folder. Links are left alone: inside the sandbox they
    /// lead only to what's mounted there.
    fn secrets(&self) -> Result<Vec<(PathBuf, bool)>, ToolError> {
        let mut found = vec![];
        let mut seen = 0;
        let mut folders = vec![PathBuf::new()];
        while let Some(rel) = folders.pop() {
            let entries = std::fs::read_dir(self.real_root.join(&rel)).map_err(|e| {
                ToolError::Failed(format!(
                    "couldn't check {} for credentials: {}; nothing ran",
                    shown(&rel),
                    e.kind()
                ))
            })?;
            for entry in entries {
                seen += 1;
                if seen > MAX_SCAN {
                    return Err(ToolError::Failed(format!(
                        "the work area has more than {MAX_SCAN} files and folders, too many to \
                         check for credentials; nothing ran"
                    )));
                }
                let entry = entry.map_err(|e| {
                    ToolError::Failed(format!(
                        "couldn't check {} for credentials: {}; nothing ran",
                        shown(&rel),
                        e.kind()
                    ))
                })?;
                let path = rel.join(entry.file_name());
                let kind = entry.file_type().map_err(|e| {
                    ToolError::Failed(format!(
                        "couldn't check {} for credentials: {}; nothing ran",
                        path.display(),
                        e.kind()
                    ))
                })?;
                if kind.is_symlink() {
                    continue;
                }
                if self.policy.is_secret(&path) {
                    found.push((path, kind.is_dir()));
                    if found.len() > MAX_HIDDEN {
                        return Err(ToolError::Failed(format!(
                            "the work area has more than {MAX_HIDDEN} credential files to hide; \
                             nothing ran"
                        )));
                    }
                } else if kind.is_dir() {
                    folders.push(path);
                }
            }
        }
        Ok(found)
    }
}

fn push<S: AsRef<std::ffi::OsStr>>(argv: &mut Vec<OsString>, items: impl IntoIterator<Item = S>) {
    argv.extend(items.into_iter().map(|item| item.as_ref().to_os_string()));
}

fn shown(rel: &Path) -> String {
    if rel.as_os_str().is_empty() {
        "the work area".into()
    } else {
        rel.display().to_string()
    }
}

struct Call {
    program: String,
    args: Vec<String>,
    dir: PathBuf,
    timeout: u64,
}

fn parse(args: &Value) -> Result<Call, ToolError> {
    let program = args
        .get("program")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if program.is_empty() {
        return Err(ToolError::InvalidArguments("program can't be empty".into()));
    }
    let list = args
        .get("args")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| item.as_str().unwrap_or_default().to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if std::iter::once(program)
        .chain(list.iter().map(String::as_str))
        .any(|part| part.contains('\0'))
    {
        return Err(ToolError::InvalidArguments(
            "the program and arguments can't contain NUL characters".into(),
        ));
    }
    let dir = PathBuf::from(args.get("dir").and_then(Value::as_str).unwrap_or(""));
    if !dir
        .components()
        .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(ToolError::InvalidArguments(
            "dir must be a folder in the work area, named relative to it".into(),
        ));
    }
    let timeout = match args.get("timeout_seconds") {
        None => DEFAULT_TIMEOUT_SECONDS,
        Some(value) => match value.as_u64() {
            Some(seconds @ 1..=MAX_TIMEOUT_SECONDS) => seconds,
            _ => {
                return Err(ToolError::InvalidArguments(format!(
                    "timeout_seconds must be between 1 and {MAX_TIMEOUT_SECONDS}"
                )));
            }
        },
    };
    Ok(Call {
        program: program.into(),
        args: list,
        dir,
        timeout,
    })
}

#[async_trait]
impl Tool for ShellTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_command".into(),
            description: format!(
                "Run one program in a sandbox on your computer, for small things: a build, a \
                 test, a quick look. Give the program and each argument separately; there's no \
                 shell, so pipes, globs and variables aren't expanded. It runs in the work area \
                 (at {WORK}), which is the only place it can change. Protected paths are \
                 read-only, credentials are hidden, and there's no network. Bigger work belongs \
                 to workers."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "program": {"type": "string", "description": "The program, such as `cargo`"},
                    "args": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Its arguments, one item each"
                    },
                    "dir": {
                        "type": "string",
                        "description": "The folder to run in, relative to the work area"
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "description": format!(
                            "When to stop it, from 1 to {MAX_TIMEOUT_SECONDS} \
                             (default {DEFAULT_TIMEOUT_SECONDS})"
                        )
                    }
                },
                "required": ["program"]
            }),
        }
    }

    fn effects(&self, args: &Value) -> Result<Vec<Effect>, ToolError> {
        let call = parse(args)?;
        Ok(vec![
            Effect::RunCommand {
                program: call.program,
                args: call.args,
            },
            Effect::ReadPath(self.policy.root.join(&call.dir)),
            // It may change anything in the work area the sandbox leaves
            // writable.
            Effect::WritePath(self.policy.root.clone()),
        ])
    }

    async fn run(&self, args: &Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let call = parse(args)?;
        let mut argv = self.sandbox(&call.dir)?;
        argv.push("--".into());
        argv.push(call.program.clone().into());
        argv.extend(call.args.iter().map(OsString::from));

        let mut child = tokio::process::Command::new(&self.bwrap)
            .args(&argv)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ToolError::Failed(format!("couldn't start the sandbox: {}", e.kind())))?;
        let stdout = tokio::spawn(capture(child.stdout.take()));
        let stderr = tokio::spawn(capture(child.stderr.take()));

        let limit = Duration::from_secs(call.timeout);
        let ending = match tokio::time::timeout(limit, child.wait()).await {
            Ok(Ok(status)) => match status.code() {
                Some(code) => format!("exit code {code}"),
                None => "it was stopped by a signal".to_string(),
            },
            Ok(Err(e)) => {
                let _ = child.start_kill();
                format!("couldn't wait for it: {}", e.kind())
            }
            Err(_) => {
                // The sandbox takes everything it started down with it.
                let _ = child.start_kill();
                let _ = child.wait().await;
                let s = if call.timeout == 1 { "" } else { "s" };
                format!(
                    "stopped after {} second{s}: it was still running",
                    call.timeout
                )
            }
        };

        let mut content = ending;
        for (name, task) in [("stdout", stdout), ("stderr", stderr)] {
            let text = match tokio::time::timeout(DRAIN, task).await {
                Ok(Ok(captured)) => captured.render(),
                _ => "(couldn't read it)".to_string(),
            };
            if !text.is_empty() {
                content.push_str(&format!("\n{name}:\n{text}"));
            }
        }
        Ok(ToolOutput::text(content))
    }
}

/// The start and end of a stream, and how much of the middle was dropped.
#[derive(Default)]
struct Captured {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    dropped: usize,
}

impl Captured {
    fn add(&mut self, mut bytes: &[u8]) {
        let room = KEEP_HEAD - self.head.len();
        let first = bytes.len().min(room);
        self.head.extend_from_slice(&bytes[..first]);
        bytes = &bytes[first..];
        self.tail.extend(bytes);
        let over = self.tail.len().saturating_sub(KEEP_TAIL);
        self.tail.drain(..over);
        self.dropped += over;
    }

    fn render(&self) -> String {
        let mut text = String::from_utf8_lossy(&self.head).into_owned();
        if self.dropped > 0 {
            text.push_str(&format!("\n[... {} bytes not shown ...]\n", self.dropped));
        }
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        text.push_str(&String::from_utf8_lossy(&tail));
        text
    }
}

async fn capture(stream: Option<impl AsyncRead + Unpin>) -> Captured {
    let mut captured = Captured::default();
    let Some(mut stream) = stream else {
        return captured;
    };
    let mut buf = vec![0; 8192];
    while let Ok(n @ 1..) = stream.read(&mut buf).await {
        captured.add(&buf[..n]);
    }
    captured
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{ExecOutcome, PolicyExecutor};
    use crate::policy::Decision;
    use harness_core::FakeClock;
    use harness_providers::ToolCall;
    use std::sync::Arc;

    /// Bubblewrap, if it works here. CI sets `HARNESS_REQUIRE_SANDBOX` so
    /// these tests can't quietly skip there.
    fn bwrap() -> Option<PathBuf> {
        let works = |path: &Path| {
            std::process::Command::new(path)
                .args(["--ro-bind", "/", "/", "--unshare-all", "--", "true"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        let found = std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join("bwrap"))
                .find(|path| path.is_file() && works(path))
        });
        if found.is_none() {
            assert!(
                std::env::var_os("HARNESS_REQUIRE_SANDBOX").is_none(),
                "bubblewrap isn't working here"
            );
            eprintln!("skipping: bubblewrap isn't working here");
        }
        found
    }

    fn area() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn tool(dir: &Path, bwrap: PathBuf) -> ShellTool {
        let real = dir.canonicalize().unwrap();
        ShellTool::new(Policy::conservative(dir), real, bwrap)
    }

    async fn run(tool: &ShellTool, args: Value) -> String {
        let clock = FakeClock::at_default_start();
        tool.run(&args, &ToolContext { clock: &clock })
            .await
            .unwrap()
            .content
    }

    #[test]
    fn credentials_in_system_folders_would_be_in_view() {
        assert!(in_view(Path::new("/etc/harness/token")));
        assert!(in_view(Path::new("/usr/share/secret")));
        assert!(!in_view(Path::new(
            "/run/credentials/harness-agent.service/token"
        )));
        assert!(!in_view(Path::new("/var/lib/harness")));
        assert!(!in_view(Path::new("/etcetera/token")));
    }

    #[tokio::test]
    async fn runs_a_program_and_reports_what_happened() {
        let Some(bwrap) = bwrap() else { return };
        let dir = area();
        let shell = tool(dir.path(), bwrap);
        let out = run(
            &shell,
            json!({"program": "echo", "args": ["hello", "$HOME *"]}),
        )
        .await;
        assert_eq!(out, "exit code 0\nstdout:\nhello $HOME *\n");
        let out = run(&shell, json!({"program": "ls", "args": ["no-such-file"]})).await;
        assert!(out.starts_with("exit code 2\nstderr:\n"), "{out}");
    }

    #[tokio::test]
    async fn commands_change_only_the_work_area() {
        let Some(bwrap) = bwrap() else { return };
        let dir = area();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let outside = area();
        std::fs::write(outside.path().join("private"), "not yours").unwrap();
        let shell = tool(dir.path(), bwrap);

        let out = run(
            &shell,
            json!({"program": "touch", "args": ["made"], "dir": "src"}),
        )
        .await;
        assert_eq!(out, "exit code 0");
        assert!(dir.path().join("src/made").exists());

        let elsewhere = outside.path().join("private");
        let out = run(&shell, json!({"program": "cat", "args": [elsewhere]})).await;
        assert!(out.starts_with("exit code 1"), "{out}");
        assert!(!out.contains("not yours"));
        for path in ["/usr/new", "/etc/new", "/new"] {
            let out = run(&shell, json!({"program": "touch", "args": [path]})).await;
            assert!(out.starts_with("exit code 1"), "{path}: {out}");
        }
        // Scratch space works, and is gone afterwards.
        let out = run(
            &shell,
            json!({"program": "touch", "args": ["/tmp/scratch"]}),
        )
        .await;
        assert_eq!(out, "exit code 0");
        let out = run(&shell, json!({"program": "ls", "args": ["/tmp"]})).await;
        assert_eq!(out, "exit code 0");
    }

    #[tokio::test]
    async fn credentials_are_hidden_at_any_depth() {
        let Some(bwrap) = bwrap() else { return };
        let dir = area();
        let root = dir.path();
        std::fs::write(root.join(".env"), "TOKEN=top").unwrap();
        std::fs::create_dir_all(root.join("pkg/web/.git")).unwrap();
        std::fs::write(root.join("pkg/web/.git/config"), "TOKEN=git").unwrap();
        std::fs::write(root.join("pkg/web/.env.local"), "TOKEN=local").unwrap();
        std::fs::write(root.join("pkg/web/notes"), "fine to read").unwrap();
        let shell = tool(root, bwrap);

        for file in [".env", "pkg/web/.git/config", "pkg/web/.env.local"] {
            let out = run(&shell, json!({"program": "cat", "args": [file]})).await;
            assert!(!out.contains("TOKEN"), "{file}: {out}");
            assert!(!out.starts_with("exit code 0"), "{file}: {out}");
        }
        let out = run(&shell, json!({"program": "cat", "args": ["pkg/web/notes"]})).await;
        assert!(out.contains("fine to read"), "{out}");
        // Nor can a command replace them.
        let out = run(
            &shell,
            json!({"program": "touch", "args": ["pkg/web/.git/hooks"]}),
        )
        .await;
        assert!(out.starts_with("exit code 1"), "{out}");
        assert!(!root.join("pkg/web/.git/hooks").exists());
    }

    #[tokio::test]
    async fn protected_paths_are_read_only_even_before_they_exist() {
        let Some(bwrap) = bwrap() else { return };
        let dir = area();
        std::fs::create_dir(dir.path().join("tests")).unwrap();
        std::fs::write(dir.path().join("tests/check"), "keep").unwrap();
        std::fs::create_dir(dir.path().join("memory")).unwrap();
        let shell = tool(dir.path(), bwrap).read_only("memory");

        let out = run(&shell, json!({"program": "cat", "args": ["tests/check"]})).await;
        assert!(out.contains("keep"), "{out}");
        for path in [
            "tests/check",
            "tests/new",
            ".github/workflows",
            "memory/MEMORY.md",
        ] {
            let out = run(&shell, json!({"program": "touch", "args": [path]})).await;
            assert!(out.starts_with("exit code 1"), "{path}: {out}");
        }
        let out = run(&shell, json!({"program": "rm", "args": ["-rf", "tests"]})).await;
        assert!(out.starts_with("exit code 1"), "{out}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("tests/check")).unwrap(),
            "keep"
        );
        assert!(!dir.path().join("memory/MEMORY.md").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_protected_path_that_is_a_link_stops_the_command() {
        let Some(bwrap) = bwrap() else { return };
        let dir = area();
        let outside = area();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("tests")).unwrap();
        let shell = tool(dir.path(), bwrap);
        let clock = FakeClock::at_default_start();
        let err = shell
            .run(&json!({"program": "true"}), &ToolContext { clock: &clock })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("tests is a link"), "{err}");
    }

    #[tokio::test]
    async fn the_environment_holds_nothing_from_the_runtime() {
        let Some(bwrap) = bwrap() else { return };
        let dir = area();
        let shell = tool(dir.path(), bwrap);
        // Cargo runs tests with many variables of its own set.
        assert!(std::env::vars().any(|(name, _)| name.starts_with("CARGO")));
        let out = run(&shell, json!({"program": "env"})).await;
        let mut names: Vec<_> = out
            .lines()
            .skip(2)
            .filter_map(|line| line.split_once('=').map(|(name, _)| name))
            .collect();
        names.sort();
        // PWD is bubblewrap's own, for the folder it starts in.
        assert_eq!(names, ["HOME", "LANG", "PATH", "PWD"], "{out}");
    }

    #[tokio::test]
    async fn a_command_that_runs_too_long_is_stopped() {
        let Some(bwrap) = bwrap() else { return };
        let dir = area();
        let shell = tool(dir.path(), bwrap);
        let out = run(
            &shell,
            json!({"program": "sleep", "args": ["30"], "timeout_seconds": 1}),
        )
        .await;
        assert_eq!(out, "stopped after 1 second: it was still running");
    }

    #[tokio::test]
    async fn long_output_keeps_its_start_and_end() {
        let Some(bwrap) = bwrap() else { return };
        let dir = area();
        let shell = tool(dir.path(), bwrap);
        let out = run(&shell, json!({"program": "seq", "args": ["1", "200000"]})).await;
        assert!(
            out.starts_with("exit code 0\nstdout:\n1\n2\n"),
            "{}",
            &out[..40]
        );
        assert!(out.contains("bytes not shown"));
        assert!(out.ends_with("199999\n200000\n"));
        assert!(out.len() < KEEP_HEAD + KEEP_TAIL + 200);
    }

    #[tokio::test]
    async fn the_walls_judge_the_call_before_anything_runs() {
        let dir = area();
        // A program that would fail loudly if it were ever started.
        let shell = ShellTool::new(
            Policy::conservative(dir.path()),
            dir.path().to_path_buf(),
            PathBuf::from("/nonexistent/bwrap"),
        );
        let mut executor = PolicyExecutor::new(
            Policy::conservative(dir.path()),
            Arc::new(FakeClock::at_default_start()),
        );
        executor.register(Box::new(shell));
        let call = |arguments: Value| ToolCall {
            id: "1".into(),
            name: "run_command".into(),
            arguments,
        };

        let out = executor
            .execute(&call(json!({"program": "bash", "args": ["-c", "true"]})))
            .await;
        assert!(
            matches!(out, ExecOutcome::Blocked(Decision::Deny { .. })),
            "{out:?}"
        );
        let out = executor
            .execute(&call(json!({"program": "git", "args": ["push", "-f"]})))
            .await;
        assert!(
            matches!(out, ExecOutcome::Blocked(Decision::Deny { .. })),
            "{out:?}"
        );
        let out = executor
            .execute(&call(json!({"program": "ls", "dir": ".git"})))
            .await;
        assert!(
            matches!(out, ExecOutcome::Blocked(Decision::Deny { .. })),
            "{out:?}"
        );
        for arguments in [
            json!({"program": "ls", "dir": "../elsewhere"}),
            json!({"program": "ls", "dir": "/etc"}),
            json!({"program": ""}),
            json!({"program": "ls", "args": ["a\0b"]}),
            json!({"program": "ls", "timeout_seconds": 0}),
            json!({"program": "ls", "timeout_seconds": 601}),
            json!({"program": "ls", "args": "-l"}),
        ] {
            let out = executor.execute(&call(arguments.clone())).await;
            assert!(
                matches!(out, ExecOutcome::Malformed(_)),
                "{arguments}: {out:?}"
            );
        }
        // Allowed calls reach the tool, which reports the missing sandbox.
        let out = executor.execute(&call(json!({"program": "ls"}))).await;
        assert_eq!(
            out,
            ExecOutcome::Failed("couldn't start the sandbox: entity not found".into())
        );
    }
}
