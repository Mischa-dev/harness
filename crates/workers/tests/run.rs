//! Workers end to end, with small scripts standing in for the coding CLIs:
//! what they're given, what's recorded when they end, and every way they
//! can be stopped.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use harness_core::{Clock, FakeClock};
use harness_store::{LockPath, WorkerBook, WorkerState};
use harness_tools::ToolContext;
use harness_workers::{Finished, Kind, KindConfig, Request, Workers, WorkersConfig};
use serde_json::json;
use time::macros::datetime;
use tokio::sync::mpsc;

struct Setup {
    _dir: tempfile::TempDir,
    root: PathBuf,
    workers: Arc<Workers>,
    done: mpsc::UnboundedReceiver<Finished>,
    clock: Arc<FakeClock>,
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

/// A repository named `demo`, and a fake Claude Code that runs `script`.
fn setup(script: &str, max_running: usize) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let repo = root.join("demo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn one() -> u8 { 1 }\n").unwrap();
    std::fs::write(repo.join("README.md"), "# demo\n").unwrap();
    std::fs::create_dir_all(repo.join("tests")).unwrap();
    std::fs::write(repo.join("tests/it.rs"), "#[test]\nfn works() {}\n").unwrap();
    git(&repo, &["init", "--quiet", "--initial-branch=main"]);
    git(&repo, &["add", "--all"]);
    git(&repo, &["commit", "--quiet", "-m", "first"]);

    let program = root.join("fake-claude");
    std::fs::write(
        &program,
        format!("#!/bin/sh\nOUT={}\n{script}\n", root.display()),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();

    let config = WorkersConfig {
        repos: BTreeMap::from([("demo".to_string(), repo)]),
        worktrees_dir: None,
        max_running,
        protected: vec![".github".into(), "AGENTS.md".into()],
        claude_code: Some(KindConfig {
            program,
            model: None,
            timeout_minutes: 1,
            max_budget_usd: Some(1.0),
        }),
        codex: None,
    };
    let clock = Arc::new(FakeClock::new(datetime!(2026-10-16 22:00 UTC)));
    let (tx, done) = mpsc::unbounded_channel();
    let workers = Workers::new(
        Arc::new(WorkerBook::open_in_memory().unwrap()),
        clock.clone(),
        config,
        root.join("worktrees"),
        "Mara",
        tx,
    )
    .unwrap();
    Setup {
        _dir: dir,
        root,
        workers: Arc::new(workers),
        done,
        clock,
    }
}

fn request(paths: &[&str], brief: &str) -> Request {
    Request {
        kind: Kind::ClaudeCode,
        repo: "demo".into(),
        brief: brief.into(),
        paths: paths.iter().map(|p| LockPath::parse(p).unwrap()).collect(),
        base: None,
    }
}

const FIXES_IT: &str = r#"cat > "$OUT/brief.txt"
env > "$OUT/env.txt"
echo 'pub fn one() -> u8 { 2 }' > src/lib.rs
echo 'stray' > README.md
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"Fixed it.\nRan cargo test.","session_id":"s-1","total_cost_usd":0.25}'"#;

const SLEEPS: &str = "cat > /dev/null\nsleep 30";

#[tokio::test]
async fn a_worker_gets_its_brief_and_no_credentials_and_its_end_is_recorded() {
    let mut s = setup(FIXES_IT, 4);
    let started = s
        .workers
        .spawn(request(&["src"], "Make one() return 2."))
        .await
        .unwrap();
    assert_eq!(started.state, WorkerState::Running);
    assert!(started.branch.starts_with("harness/worker-demo-"));

    let finished = s.done.recv().await.unwrap();
    assert_eq!(finished.id, started.id);
    assert_eq!(
        finished.line,
        format!(
            "Worker #{} (claude_code in demo) finished: Fixed it.",
            started.id
        )
    );

    let brief = std::fs::read_to_string(s.root.join("brief.txt")).unwrap();
    assert!(brief.starts_with("You're a worker for Mara"), "{brief}");
    assert!(
        brief.contains("Change only these paths:\n\n- src\n"),
        "{brief}"
    );
    assert!(
        brief.trim_end().ends_with("Make one() return 2."),
        "{brief}"
    );

    // Only the kept variables, plus what sh sets itself.
    let env = std::fs::read_to_string(s.root.join("env.txt")).unwrap();
    for line in env.lines() {
        let key = line.split('=').next().unwrap();
        assert!(
            [
                "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TERM", "TMPDIR", "PWD",
                "SHLVL", "_", "OLDPWD"
            ]
            .contains(&key),
            "{key} leaked into the worker"
        );
    }

    let record = s.workers.book.get(started.id).unwrap().unwrap();
    assert_eq!(record.state, WorkerState::Finished);
    assert_eq!(record.paths, vec![LockPath::parse("src").unwrap()]);
    assert_eq!(record.session.as_deref(), Some("s-1"));
    assert!(s.workers.book.running().unwrap().is_empty());
    let summary = record.summary.unwrap();
    assert!(
        summary.starts_with("Fixed it.\nRan cargo test."),
        "{summary}"
    );
    assert!(summary.contains("(It cost $0.25.)"), "{summary}");
    assert!(
        summary.ends_with("2 paths changed. Outside its locked paths: README.md."),
        "{summary}"
    );

    // The agent sees the report and the diff.
    let status = &harness_workers::tools::tools(s.workers.clone())[1];
    let shown = status
        .run(
            &json!({"worker": started.id}),
            &ToolContext {
                clock: s.clock.as_ref(),
            },
        )
        .await
        .unwrap()
        .content;
    assert!(shown.contains("+pub fn one() -> u8 { 2 }"), "{shown}");
    assert!(shown.contains("Its report:\nFixed it."), "{shown}");
    // The repository's own checkout is untouched.
    let main = std::fs::read_to_string(s.root.join("demo/src/lib.rs")).unwrap();
    assert!(main.contains("{ 1 }"));
}

#[tokio::test]
async fn a_worker_that_changes_ci_or_deletes_tests_fails_whatever_it_says() {
    let script = r#"cat > /dev/null
mkdir -p .github
echo 'on: push' > .github/ci.yml
rm tests/it.rs
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"All green now."}'"#;
    let mut s = setup(script, 4);
    let started = s
        .workers
        .spawn(request(&["."], "Make CI pass."))
        .await
        .unwrap();
    let finished = s.done.recv().await.unwrap();
    assert_eq!(
        finished.line,
        format!(
            "Worker #{} (claude_code in demo) failed: It changed protected files: .github/ci.yml. \
             It deleted tests: tests/it.rs. Don't accept this as it is.",
            started.id
        )
    );
    let record = s.workers.book.get(started.id).unwrap().unwrap();
    assert_eq!(record.state, WorkerState::Failed);
    assert!(record.summary.unwrap().contains("All green now."));
}

#[tokio::test]
async fn a_worker_that_fails_is_recorded_as_failed() {
    let mut s = setup("cat > /dev/null\necho 'out of ideas' >&2\nexit 3", 4);
    let started = s.workers.spawn(request(&["src"], "Try.")).await.unwrap();
    let finished = s.done.recv().await.unwrap();
    assert!(
        finished.line.contains("failed: It exited with"),
        "{}",
        finished.line
    );
    let record = s.workers.book.get(started.id).unwrap().unwrap();
    assert_eq!(record.state, WorkerState::Failed);
    let summary = record.summary.unwrap();
    assert!(summary.contains("out of ideas"), "{summary}");
}

#[tokio::test]
async fn overlapping_paths_and_too_many_workers_are_refused() {
    let mut s = setup(SLEEPS, 2);
    let first = s.workers.spawn(request(&["src"], "One.")).await.unwrap();
    let clash = s
        .workers
        .spawn(request(&["src/lib.rs"], "Two."))
        .await
        .unwrap_err();
    assert!(
        clash.contains(&format!("worker #{} holds src", first.id)),
        "{clash}"
    );
    // The refused worker left no worktree behind.
    let trees = std::fs::read_dir(s.root.join("worktrees"))
        .unwrap()
        .filter(|e| e.as_ref().unwrap().path().is_dir())
        .count();
    assert_eq!(trees, 1);

    s.workers
        .spawn(request(&["README.md"], "Three."))
        .await
        .unwrap();
    let full = s
        .workers
        .spawn(request(&["docs"], "Four."))
        .await
        .unwrap_err();
    assert!(full.contains("2 workers are running already"), "{full}");

    for _ in 0..2 {
        let running = s.workers.book.running().unwrap();
        s.workers.stop(running[0].id).unwrap();
        let finished = s.done.recv().await.unwrap();
        assert!(
            finished.line.contains("stopped: Stopped by the agent."),
            "{}",
            finished.line
        );
    }
    assert!(s.workers.book.running().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_worker_past_its_time_limit_is_stopped() {
    let mut s = setup(SLEEPS, 4);
    let started = s
        .workers
        .spawn(request(&["src"], "Take forever."))
        .await
        .unwrap();
    let finished = s.done.recv().await.unwrap();
    assert!(
        finished
            .line
            .ends_with("stopped: Stopped: it ran past its 1-minute limit."),
        "{}",
        finished.line
    );
    let record = s.workers.book.get(started.id).unwrap().unwrap();
    assert_eq!(record.state, WorkerState::Stopped);
}

#[tokio::test]
async fn bad_requests_are_refused_before_anything_starts() {
    let s = setup(SLEEPS, 4);
    let mut other_repo = request(&["src"], "x");
    other_repo.repo = "elsewhere".into();
    assert!(
        s.workers
            .spawn(other_repo)
            .await
            .unwrap_err()
            .contains("these are: demo")
    );
    assert!(s.workers.spawn(request(&[], "x")).await.is_err());
    assert!(s.workers.spawn(request(&["src"], "  ")).await.is_err());
    let mut codex = request(&["src"], "x");
    codex.kind = Kind::Codex;
    assert!(
        s.workers
            .spawn(codex)
            .await
            .unwrap_err()
            .contains("no codex workers")
    );
    let mut bad_base = request(&["src"], "x");
    bad_base.base = Some("--upload-pack=evil".into());
    assert!(s.workers.spawn(bad_base).await.is_err());
    assert!(s.workers.book.running().unwrap().is_empty());
    assert!(s.workers.stop(99).unwrap_err().contains("no worker #99"));
}

#[tokio::test]
async fn workers_left_running_by_a_restart_are_ended_and_reported() {
    let mut s = setup(SLEEPS, 4);
    let new = harness_store::NewWorker {
        kind: "claude_code".into(),
        repo: "demo".into(),
        base: "abc".into(),
        branch: "harness/worker-old".into(),
        worktree: "/nowhere".into(),
        brief: "Old work.".into(),
    };
    let old = s
        .workers
        .book
        .start(
            &new,
            &[LockPath::parse("src").unwrap()],
            time::Duration::minutes(5),
            s.clock.now(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(s.workers.recover().unwrap(), 1);
    let finished = s.done.recv().await.unwrap();
    assert_eq!(finished.id, old.id);
    assert!(
        finished.line.contains("failed: The runtime restarted"),
        "{}",
        finished.line
    );
    // Its path is free again.
    let next = s.workers.spawn(request(&["src"], "Again.")).await.unwrap();
    s.workers.stop(next.id).unwrap();
    s.done.recv().await.unwrap();
}

#[test]
fn spawning_and_stopping_are_declared_actions() {
    let s = setup(SLEEPS, 4);
    let tools = harness_workers::tools::tools(s.workers.clone());
    let spawn = tools[0]
        .effects(&json!({"kind": "claude_code", "repo": "demo", "brief": "x", "paths": ["src"]}))
        .unwrap();
    assert!(spawn.contains(&harness_tools::Effect::Action {
        name: "spawn_worker".into()
    }));
    assert!(
        tools[0]
            .effects(&json!({"kind": "codex", "repo": "demo", "brief": "x", "paths": ["src"]}))
            .is_err()
    );
    assert!(
        tools[0]
            .effects(
                &json!({"kind": "claude_code", "repo": "demo", "brief": "x", "paths": ["../x"]})
            )
            .is_err()
    );
    assert!(tools[1].effects(&json!({})).unwrap().is_empty());
    // Steering names a worker that exists.
    assert!(
        tools[2]
            .effects(&json!({"worker": 1, "message": "x"}))
            .is_err()
    );
    assert_eq!(
        tools[3].effects(&json!({"worker": 1})).unwrap(),
        vec![harness_tools::Effect::Action {
            name: "stop_worker".into()
        }]
    );
    assert!(
        tools[0]
            .spec()
            .description
            .contains("Kinds: claude_code. Repositories: demo.")
    );
}

/// A fake CLI that counts its runs, keeps what each one was given, and sets
/// one() to the run's number plus one.
fn turns(session: bool) -> String {
    let session = if session {
        r#","session_id":"s-1""#
    } else {
        ""
    };
    format!(
        r#"n=$(cat "$OUT/n" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$OUT/n"
cat > "$OUT/brief-$n.txt"
echo "$@" > "$OUT/args-$n.txt"
echo "pub fn one() -> u8 {{ $((n+1)) }}" > src/lib.rs
printf '{{"type":"result","subtype":"success","is_error":false,"result":"Turn %s done."{session}}}\n' "$n""#
    )
}

#[tokio::test]
async fn a_steered_worker_continues_its_session_in_the_same_worktree() {
    let mut s = setup(&turns(true), 4);
    let first = s
        .workers
        .spawn(request(&["src"], "Make one() return 2."))
        .await
        .unwrap();
    s.done.recv().await.unwrap();
    let args = std::fs::read_to_string(s.root.join("args-1.txt")).unwrap();
    assert!(!args.contains("--resume"), "{args}");

    let steer = &harness_workers::tools::tools(s.workers.clone())[2];
    let out = steer
        .run(
            &json!({"worker": first.id, "message": "Make it 3, and run the tests."}),
            &ToolContext {
                clock: s.clock.as_ref(),
            },
        )
        .await
        .unwrap()
        .content;
    assert!(out.contains("turn 2"), "{out}");
    let again = s.done.recv().await.unwrap();
    assert_eq!(again.id, first.id);
    assert!(
        again.line.ends_with("finished: Turn 2 done."),
        "{}",
        again.line
    );

    let args = std::fs::read_to_string(s.root.join("args-2.txt")).unwrap();
    assert!(args.contains("--resume s-1"), "{args}");
    let brief = std::fs::read_to_string(s.root.join("brief-2.txt")).unwrap();
    assert!(
        brief.starts_with("Mara has read your last report"),
        "{brief}"
    );
    assert!(brief.contains("Turn 1 done."), "{brief}");
    assert!(brief.contains("Make it 3, and run the tests."), "{brief}");
    assert!(!brief.contains("You're a worker for Mara"), "{brief}");

    let record = s.workers.book.get(first.id).unwrap().unwrap();
    assert_eq!(record.state, WorkerState::Finished);
    assert_eq!(record.turns, 2);
    assert!(record.summary.unwrap().starts_with("Turn 2 done."));
    let lib = std::fs::read_to_string(Path::new(&record.worktree).join("src/lib.rs")).unwrap();
    assert_eq!(lib.trim(), "pub fn one() -> u8 { 3 }");
    assert!(s.workers.book.running().unwrap().is_empty());
}

#[tokio::test]
async fn a_worker_without_a_session_gets_its_first_brief_again() {
    let mut s = setup(&turns(false), 4);
    let first = s
        .workers
        .spawn(request(&["src"], "Make one() return 2."))
        .await
        .unwrap();
    s.done.recv().await.unwrap();
    s.workers.steer(first.id, "Make it 3.").await.unwrap();
    s.done.recv().await.unwrap();
    let args = std::fs::read_to_string(s.root.join("args-2.txt")).unwrap();
    assert!(!args.contains("--resume"), "{args}");
    let brief = std::fs::read_to_string(s.root.join("brief-2.txt")).unwrap();
    assert!(brief.starts_with("You're a worker for Mara"), "{brief}");
    assert!(brief.contains("Make one() return 2."), "{brief}");
    assert!(brief.contains("Turn 1 done."), "{brief}");
    assert!(
        brief
            .trim_end()
            .ends_with("end with the same short report."),
        "{brief}"
    );
}

#[tokio::test]
async fn running_or_unknown_workers_cant_be_steered() {
    let mut s = setup(SLEEPS, 4);
    let running = s.workers.spawn(request(&["src"], "Sleep.")).await.unwrap();
    let refused = s.workers.steer(running.id, "More.").await.unwrap_err();
    assert!(refused.contains("still running"), "{refused}");
    assert!(s.workers.steer(running.id + 1, "More.").await.is_err());
    s.workers.stop(running.id).unwrap();
    s.done.recv().await.unwrap();
    assert!(s.workers.steer(running.id, "  ").await.is_err());
    // Someone else holds its path now: it waits.
    let other = s
        .workers
        .spawn(request(&["src/lib.rs"], "Sleep."))
        .await
        .unwrap();
    let refused = s.workers.steer(running.id, "More.").await.unwrap_err();
    assert!(
        refused.contains(&format!("worker #{} holds", other.id)),
        "{refused}"
    );
    s.workers.stop(other.id).unwrap();
    s.done.recv().await.unwrap();
}
