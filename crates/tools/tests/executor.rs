//! The walls. A bug here is silent and costly, so every wall has a test that
//! proves a blocked call never reaches the tool.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use harness_core::FakeClock;
use harness_providers::{ToolCall, ToolSpec};
use harness_tools::{
    Decision, Effect, ExecOutcome, Policy, PolicyExecutor, Tool, ToolContext, ToolError, ToolOutput,
};
use serde_json::{Value, json};
use time::Duration;

/// A tool that declares one effect built from its arguments and counts how
/// often it actually ran.
struct Probe {
    name: &'static str,
    effect: fn(&Value) -> Effect,
    runs: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for Probe {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: String::new(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "program": {"type": "string"},
                    "args": {"type": "string"},
                    "to": {"type": "string"},
                    "action": {"type": "string"}
                }
            }),
        }
    }

    fn effects(&self, args: &Value) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![(self.effect)(args)])
    }

    async fn run(&self, _args: &Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("ran"))
    }
}

fn arg(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

struct Harness {
    exec: PolicyExecutor,
    clock: Arc<FakeClock>,
    runs: Arc<AtomicUsize>,
}

fn harness(root: &Path) -> Harness {
    let clock = Arc::new(FakeClock::at_default_start());
    let runs = Arc::new(AtomicUsize::new(0));
    let probe = |name, effect| Probe {
        name,
        effect,
        runs: runs.clone(),
    };
    let exec = PolicyExecutor::new(Policy::conservative(root), clock.clone())
        .with_tool(probe("read", |a| Effect::ReadPath(arg(a, "path").into())))
        .with_tool(probe("write", |a| Effect::WritePath(arg(a, "path").into())))
        .with_tool(probe("run", |a| Effect::RunCommand {
            program: arg(a, "program"),
            args: arg(a, "args")
                .split_whitespace()
                .map(String::from)
                .collect(),
        }))
        .with_tool(probe("send", |a| Effect::SendMessage {
            conversation: arg(a, "to"),
        }))
        .with_tool(probe("act", |a| Effect::Action {
            name: arg(a, "action"),
        }));
    Harness { exec, clock, runs }
}

fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: "c".into(),
        name: name.into(),
        arguments,
    }
}

fn is_deny(outcome: &ExecOutcome) -> bool {
    matches!(outcome, ExecOutcome::Blocked(Decision::Deny { .. }))
}

#[tokio::test]
async fn allowed_calls_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    let out = h
        .exec
        .execute(&call("write", json!({"path": "src/lib.rs"})))
        .await;
    assert_eq!(out, ExecOutcome::Ran(ToolOutput::text("ran")));
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn protected_paths_cannot_be_written_but_can_be_read() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    for path in [
        ".github/workflows/ci.yml",
        "tests/executor.rs",
        "./tests/x",
        "src/../tests/x",
    ] {
        let out = h.exec.execute(&call("write", json!({"path": path}))).await;
        assert!(is_deny(&out), "{path} should be protected, got {out:?}");
    }
    let read = h
        .exec
        .execute(&call("read", json!({"path": "tests/executor.rs"})))
        .await;
    assert!(matches!(read, ExecOutcome::Ran(_)));
    assert_eq!(h.runs.load(Ordering::SeqCst), 1, "only the read ran");
}

#[tokio::test]
async fn paths_outside_the_work_area_are_denied() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    for path in ["/etc/passwd", "../outside", "src/../../outside"] {
        for tool in ["read", "write"] {
            let out = h.exec.execute(&call(tool, json!({"path": path}))).await;
            assert!(is_deny(&out), "{tool} {path} should be denied, got {out:?}");
        }
    }
    let absolute_inside = dir.path().join("notes.md");
    let out = h
        .exec
        .execute(&call(
            "write",
            json!({"path": absolute_inside.to_str().unwrap()}),
        ))
        .await;
    assert!(matches!(out, ExecOutcome::Ran(_)));
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_cannot_launder_a_protected_or_outside_path() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".github")).unwrap();
    std::os::unix::fs::symlink(dir.path().join(".github"), dir.path().join("innocent")).unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
    let mut h = harness(dir.path());

    let laundered = h
        .exec
        .execute(&call("write", json!({"path": "innocent/ci.yml"})))
        .await;
    assert!(is_deny(&laundered), "got {laundered:?}");
    let escaped = h
        .exec
        .execute(&call("read", json!({"path": "escape/secret"})))
        .await;
    assert!(is_deny(&escaped), "got {escaped:?}");
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn forbidden_commands_are_denied_by_program_name() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    let cases = [
        ("bash", "-c ls"),
        ("/usr/bin/env", "python"),
        ("git", "push --force origin main"),
        ("/run/current-system/sw/bin/git", "push origin main -f"),
        ("git", "push --force=true origin main"),
        ("git", "push --force-with-lease=main:deadbeef origin main"),
        ("git", "push -uf origin main"),
        ("git", "push origin +main:main"),
    ];
    for (program, args) in cases {
        let out = h
            .exec
            .execute(&call("run", json!({"program": program, "args": args})))
            .await;
        assert!(
            is_deny(&out),
            "{program} {args} should be denied, got {out:?}"
        );
    }
    let ok = h
        .exec
        .execute(&call(
            "run",
            json!({"program": "git", "args": "push origin my-branch"}),
        ))
        .await;
    assert!(matches!(ok, ExecOutcome::Ran(_)));
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn credential_and_repository_metadata_paths_are_protected() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    for path in [
        ".git/config",
        ".ssh/authorized_keys",
        ".aws/credentials",
        ".config/gcloud/credentials.db",
        ".npmrc",
        ".pypirc",
        ".netrc",
        ".env.local",
    ] {
        for tool in ["read", "write"] {
            let out = h.exec.execute(&call(tool, json!({"path": path}))).await;
            assert!(is_deny(&out), "{tool} {path} should be denied, got {out:?}");
        }
    }
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn walled_actions_need_approval() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    for action in ["merge", "publish", "spend", "contact_outside"] {
        let out = h
            .exec
            .execute(&call("act", json!({"action": action})))
            .await;
        assert!(
            matches!(out, ExecOutcome::Blocked(Decision::NeedsApproval { .. })),
            "{action}: {out:?}"
        );
    }
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_message_breaker_trips_pauses_and_stays_paused() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    let send = call("send", json!({"to": "dm"}));
    for _ in 0..30 {
        assert!(matches!(h.exec.execute(&send).await, ExecOutcome::Ran(_)));
        h.clock.advance(Duration::minutes(1));
    }
    let tripped = h.exec.execute(&send).await;
    assert!(matches!(
        tripped,
        ExecOutcome::Blocked(Decision::Tripped { .. })
    ));
    assert_eq!(h.exec.paused(), Some("messages_per_hour"));

    // Paused means everything stops, not just messages, even after the window passes.
    h.clock.advance(Duration::hours(2));
    let read = h
        .exec
        .execute(&call("read", json!({"path": "README.md"})))
        .await;
    assert!(matches!(
        read,
        ExecOutcome::Blocked(Decision::Tripped { .. })
    ));
    assert_eq!(h.runs.load(Ordering::SeqCst), 30);

    h.exec.resume();
    assert!(matches!(h.exec.execute(&send).await, ExecOutcome::Ran(_)));
}

#[tokio::test]
async fn the_message_window_slides() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    let send = call("send", json!({"to": "dm"}));
    for _ in 0..30 {
        assert!(matches!(h.exec.execute(&send).await, ExecOutcome::Ran(_)));
    }
    h.clock.advance(Duration::HOUR);
    assert!(matches!(h.exec.execute(&send).await, ExecOutcome::Ran(_)));
    assert_eq!(h.exec.paused(), None);
}

#[tokio::test]
async fn malformed_calls_never_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    let unknown = h.exec.execute(&call("rm_rf", json!({}))).await;
    assert!(matches!(unknown, ExecOutcome::Malformed(_)));
    let bad_args = h.exec.execute(&call("write", json!({"path": 7}))).await;
    assert!(matches!(bad_args, ExecOutcome::Malformed(_)));
    let not_object = h.exec.execute(&call("write", json!("src/lib.rs"))).await;
    assert!(matches!(not_object, ExecOutcome::Malformed(_)));
    assert_eq!(h.runs.load(Ordering::SeqCst), 0);
}
