//! The agent's Workspace connection, end to end against a fake Workspace
//! MCP server: a reply request answered through the agent's tools, and the
//! polling fallback that wakes it when a wakeup was missed.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use harness_core::{FakeClock, WakeReason};
use harness_daemon::agent::{Agent, AgentConfig};
use harness_daemon::polling::{self, WorkspaceConfig};
use harness_providers::ScriptedProvider;
use serde_json::{Value, json};
use time::macros::datetime;

const WORKSPACE_TOKEN: &str = "aw_ext_agent-token-for-the-daemon-tests";
const PROFILES: &str = r#"
[profile.main]
adapter = "anthropic"
model = "example-mid"
context_window = 200000
tool_calls = "parallel"
caching = "explicit_breakpoints"
pricing = { input = 3.0, output = 15.0, cache_read = 0.3, cache_write = 6.0 }
"#;

#[derive(Default)]
struct Fake {
    inbox: Mutex<Vec<Value>>,
    calls: Mutex<Vec<(String, Value)>>,
}

async fn mcp(State(fake): State<Arc<Fake>>, headers: HeaderMap, body: String) -> Response {
    let authorized = headers.get("authorization").and_then(|v| v.to_str().ok())
        == Some(&format!("Bearer {WORKSPACE_TOKEN}"));
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let message: Value = serde_json::from_str(&body).unwrap();
    let result = match message["method"].as_str().unwrap() {
        "initialize" => {
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "fake"}})
        }
        "notifications/initialized" => return StatusCode::ACCEPTED.into_response(),
        "tools/call" => {
            let name = message["params"]["name"].as_str().unwrap().to_string();
            let arguments = message["params"]["arguments"].clone();
            fake.calls
                .lock()
                .unwrap()
                .push((name.clone(), arguments.clone()));
            let value = match name.as_str() {
                "inbox_list" => json!({"requests": fake.inbox.lock().unwrap().clone()}),
                "briefing" => json!({"replyRequests": fake.inbox.lock().unwrap().clone()}),
                "reply_claim" => json!({
                    "request": request("r-1", "claimed", true, 2),
                    "recentMessages": [{"author": "Mischa", "text": "Is the proxy merged?"}]
                }),
                "reply_send" => json!({"requestId": arguments["requestId"], "status": "answered"}),
                other => panic!("unexpected tool {other}"),
            };
            let content = json!({"outcome": "ok", "value": value});
            json!({"content": [], "structuredContent": content})
        }
        other => panic!("unexpected method {other}"),
    };
    let reply = json!({"jsonrpc": "2.0", "id": message["id"], "result": result});
    ([("mcp-session-id", "s-1")], axum::Json(reply)).into_response()
}

fn request(id: &str, status: &str, required: bool, generation: i32) -> Value {
    json!({
        "requestId": id, "required": required, "mentioned": false,
        "status": status, "generation": generation,
        "messageExcerpt": "Is the proxy merged?"
    })
}

async fn workspace() -> (Arc<Fake>, SocketAddr) {
    let fake = Arc::new(Fake::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/api/v1/mcp", post(mcp))
        .with_state(fake.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (fake, addr)
}

fn write(dir: &Path, name: &str, text: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

fn config(dir: &Path, workspace: SocketAddr) -> AgentConfig {
    AgentConfig {
        state_dir: dir.join("state"),
        workdir: dir.to_path_buf(),
        profiles_file: write(dir, "profiles.toml", PROFILES),
        profile: "main".into(),
        model_url: "http://127.0.0.1:8787/anthropic".into(),
        caller_token_file: write(dir, "caller-token", "caller-token-for-the-daemon-tests"),
        charter_file: write(dir, "charter.md", "You are Tamsin."),
        quiet_hour: 2,
        utc_offset_hours: 0,
        max_output_tokens: 1024,
        wakeups: None,
        workspace: Some(WorkspaceConfig {
            url: format!("http://{workspace}/api/v1/mcp"),
            token_file: write(dir, "workspace-token", &format!("{WORKSPACE_TOKEN}\n")),
            poll_seconds: 300,
        }),
    }
}

#[tokio::test]
async fn the_agent_answers_a_reply_request_through_its_tools() {
    let (fake, addr) = workspace().await;
    fake.inbox
        .lock()
        .unwrap()
        .push(request("r-1", "pending", true, 1));
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(datetime!(2026-01-05 09:00 UTC)));
    let mut agent = Agent::from_config(&config(dir.path(), addr), clock).unwrap();
    agent.provider = Box::new(
        ScriptedProvider::new()
            .then_calls("", vec![("briefing", json!({}))])
            .then_calls("", vec![("reply_claim", json!({"request_id": "r-1"}))])
            .then_calls(
                "",
                vec![(
                    "reply_send",
                    json!({"request_id": "r-1", "generation": 2, "text": "Yes, this morning."}),
                )],
            )
            .then_calls("", vec![("sleep", json!({"minutes": 60}))]),
    );

    let event = polling::event(harness_workspace::NewRequests {
        count: 1,
        pressing: 1,
    });
    agent.wake(vec![event]).await.unwrap();

    let calls: Vec<String> = fake
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    assert_eq!(calls, ["briefing", "reply_claim", "reply_send"]);
    assert_eq!(
        fake.calls.lock().unwrap()[2].1,
        json!({"requestId": "r-1", "generation": 2, "text": "Yes, this morning."})
    );
}

#[test]
fn bad_workspace_settings_stop_startup_without_quoting_them() {
    let dir = tempfile::tempdir().unwrap();
    let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
    let clock = Arc::new(FakeClock::new(datetime!(2026-01-05 09:00 UTC)));
    let mut config = config(dir.path(), addr);

    let mut too_often = config.clone();
    too_often.workspace.as_mut().unwrap().poll_seconds = 5;
    let err = Agent::from_config(&too_often, clock.clone()).err().unwrap();
    assert!(err.to_string().contains("poll_seconds"), "{err}");

    let mut plain_http = config.clone();
    plain_http.workspace.as_mut().unwrap().url = "http://workspace.example/api/v1/mcp".into();
    assert!(Agent::from_config(&plain_http, clock.clone()).is_err());

    config.workspace.as_mut().unwrap().token_file = dir.path().join("aw_ext_pasted-here");
    let err = Agent::from_config(&config, clock).err().unwrap();
    assert!(err.to_string().contains("token file"), "{err}");
    assert!(!err.to_string().contains("aw_ext"), "{err}");
}

#[tokio::test]
async fn the_poller_wakes_the_agent_for_requests_it_missed() {
    let (fake, addr) = workspace().await;
    fake.inbox.lock().unwrap().extend([
        request("r-1", "pending", true, 1),
        request("r-2", "claimed", true, 3),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new(datetime!(2026-01-05 09:00 UTC)));
    let agent = Agent::from_config(&config(dir.path(), addr), clock).unwrap();
    let (send, mut events) = tokio::sync::mpsc::channel(8);
    let poller = tokio::spawn(polling::poll(
        agent.workspace.clone().unwrap(),
        Duration::from_millis(20),
        send,
    ));

    // The look at startup finds the request that's waiting.
    let first = events.recv().await.unwrap();
    assert!(matches!(
        first.reason,
        WakeReason::Workspace { urgent: true, .. }
    ));
    assert!(first.text.contains("1 new reply request"));
    // An optional group request arrives later; it can wait for a look.
    fake.inbox
        .lock()
        .unwrap()
        .push(request("r-3", "pending", false, 1));
    let second = events.recv().await.unwrap();
    assert!(matches!(
        second.reason,
        WakeReason::Workspace { urgent: false, .. }
    ));
    // Nothing from the requests themselves reaches the event.
    assert!(!format!("{first:?}{second:?}").contains("proxy merged"));
    // Nothing new: no more events, however many looks.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(events.try_recv().is_err());

    drop(events);
    tokio::time::timeout(Duration::from_secs(5), poller)
        .await
        .expect("the poller stops when the agent stops listening")
        .unwrap();
}
