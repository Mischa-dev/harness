//! The wake loop, end to end: the agent wakes at its chosen look or on an
//! event, and its model calls go through the forwarding proxy.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use harness_core::{Clock, FakeClock, WakeReason};
use harness_daemon::agent::{Agent, AgentConfig, WakeEvent};
use harness_daemon::proxy::{Auth, CALLER_TOKEN_HEADER, CallerToken, Route, router};
use harness_loop::{ContextAssembler, Rhythm, WakeLimits};
use harness_providers::{ContentBlock, Profiles, ScriptedProvider};
use harness_store::{Store, WorkdayPolicy};
use harness_tools::{Policy, PolicyExecutor, SleepTool};
use serde_json::json;
use time::macros::datetime;

const TOKEN: &str = "caller-token-for-the-agent-tests-0123";
const CHARTER: &str = "You are Tamsin, a teammate on a small software team.";
const PROFILES: &str = r#"
[profile.main]
adapter = "anthropic"
model = "example-mid"
context_window = 200000
tool_calls = "parallel"
caching = "explicit_breakpoints"
pricing = { input = 3.0, output = 15.0, cache_read = 0.3, cache_write = 6.0 }
"#;

type Calls = Arc<Mutex<Vec<(HeaderMap, Bytes)>>>;

/// An Anthropic-shaped provider whose model always goes back to sleep for
/// three hours.
async fn sleepy_model(State(calls): State<Calls>, headers: HeaderMap, body: Bytes) -> String {
    calls.lock().unwrap().push((headers, body));
    json!({
        "content": [{"type": "tool_use", "id": "toolu_1", "name": "sleep", "input": {"minutes": 180}}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 1000, "output_tokens": 10}
    })
    .to_string()
}

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

fn write(dir: &Path, name: &str, text: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

#[tokio::test]
async fn the_agent_calls_its_model_through_the_proxy() {
    let calls: Calls = Arc::default();
    let upstream = serve(
        Router::new()
            .route("/v1/messages", post(sleepy_model))
            .with_state(calls.clone()),
    )
    .await;
    let route = Route::new(
        "anthropic",
        &format!("http://{upstream}"),
        Auth::XApiKey,
        "sk-ant-secret",
    )
    .unwrap();
    let proxy = serve(
        router(
            vec![route],
            CallerToken::new(TOKEN).unwrap(),
            Duration::from_secs(5),
        )
        .unwrap(),
    )
    .await;

    let dir = tempfile::tempdir().unwrap();
    let config = AgentConfig {
        state_dir: dir.path().join("state"),
        workdir: dir.path().to_path_buf(),
        profiles_file: write(dir.path(), "profiles.toml", PROFILES),
        profile: "main".into(),
        model_url: format!("http://{proxy}/anthropic"),
        caller_token_file: write(dir.path(), "caller-token", &format!("{TOKEN}\n")),
        charter_file: write(dir.path(), "charter.md", CHARTER),
        quiet_hour: 2,
        utc_offset_hours: 0,
        max_output_tokens: 1024,
    };
    let start = datetime!(2026-01-05 09:00 UTC);
    let clock = Arc::new(FakeClock::new(start));
    let mut agent = Agent::from_config(&config, clock.clone()).unwrap();
    // A restart doesn't wake it at once.
    assert_eq!(agent.rhythm.next_look(), start + time::Duration::minutes(5));

    clock.set(agent.rhythm.next_look());
    agent.wake(Vec::new()).await.unwrap();

    let (headers, body) = calls.lock().unwrap()[0].clone();
    assert_eq!(headers["x-api-key"], "sk-ant-secret");
    assert!(headers.get(CALLER_TOKEN_HEADER).is_none());
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(body.contains(CHARTER));
    assert!(body.contains("the time you chose to look again"));
    assert_eq!(
        agent.rhythm.next_look(),
        clock.now() + time::Duration::hours(3)
    );
    let wakes = agent.store.wakes_since(start).unwrap();
    assert_eq!(wakes.len(), 1);
    assert_eq!(wakes[0].end, "slept");
    assert!(wakes[0].cost_usd > 0.0);
}

#[test]
fn a_missing_charter_is_named_without_its_path() {
    let dir = tempfile::tempdir().unwrap();
    let config = AgentConfig {
        state_dir: dir.path().join("state"),
        workdir: dir.path().to_path_buf(),
        profiles_file: write(dir.path(), "profiles.toml", PROFILES),
        profile: "main".into(),
        model_url: "http://127.0.0.1:8787/anthropic".into(),
        caller_token_file: write(dir.path(), "caller-token", TOKEN),
        charter_file: dir.path().join("sk-pasted-here"),
        quiet_hour: 2,
        utc_offset_hours: 0,
        max_output_tokens: 1024,
    };
    let clock = Arc::new(FakeClock::new(datetime!(2026-01-05 09:00 UTC)));
    let err = Agent::from_config(&config, clock.clone()).err().unwrap();
    assert!(err.to_string().contains("charter_file"), "{err}");
    assert!(!err.to_string().contains("sk-pasted"), "{err}");

    // The model's route must be the proxy on this machine.
    let remote = AgentConfig {
        model_url: "https://api.example.com/anthropic".into(),
        charter_file: write(dir.path(), "charter.md", CHARTER),
        ..config
    };
    assert!(Agent::from_config(&remote, clock).is_err());
}

/// An agent on a scripted model, with its next look three hours away.
fn scripted(clock: Arc<FakeClock>, model: ScriptedProvider) -> Agent {
    let workdir = std::env::temp_dir();
    let profile = Profiles::from_toml(PROFILES)
        .unwrap()
        .get("main")
        .unwrap()
        .clone();
    Agent {
        store: Store::open_in_memory().unwrap(),
        provider: Box::new(model),
        executor: PolicyExecutor::new(Policy::conservative(&workdir), clock.clone())
            .with_tool(SleepTool),
        assembler: ContextAssembler {
            profile: "main".into(),
            static_prefix: CHARTER.into(),
            memory_snapshot: String::new(),
            max_output_tokens: 1024,
        },
        profile,
        rhythm: Rhythm::new(
            WorkdayPolicy::default(),
            clock.now() + time::Duration::hours(3),
        ),
        clock,
        limits: WakeLimits::default(),
    }
}

// Paused tokio time: the loop's naps pass instantly, and the fake clock
// alone decides when a look is due.
#[tokio::test(start_paused = true)]
async fn an_event_wakes_the_agent_before_its_next_look() {
    let start = datetime!(2026-01-05 09:00 UTC);
    let clock = Arc::new(FakeClock::new(start));
    let model = ScriptedProvider::new().then_calls("", vec![("sleep", json!({"minutes": 60}))]);
    let mut agent = scripted(clock.clone(), model);
    let (send, mut events) = tokio::sync::mpsc::channel(8);
    send.send(WakeEvent {
        reason: WakeReason::Workspace {
            summary: "a DM from Mischa".into(),
            urgent: true,
        },
        text: "[dm] Mischa: is the proxy merged?".into(),
    })
    .await
    .unwrap();

    agent
        .run(&mut events, tokio::time::sleep(Duration::from_secs(600)))
        .await
        .unwrap();

    let wakes = agent.store.wakes_since(start).unwrap();
    assert_eq!(wakes.len(), 1, "one wake for the event, none since");
    assert!(wakes[0].reason.contains("a DM from Mischa"));
    let day = agent.store.current_day().unwrap().unwrap();
    let first = agent.store.transcript(day.id).unwrap()[0].clone();
    let ContentBlock::Text { text } = &first.content[0] else {
        panic!("the wake message is text");
    };
    assert!(
        text.contains("- [dm] Mischa: is the proxy merged?"),
        "{text}"
    );
    assert_eq!(agent.rhythm.next_look(), start + time::Duration::hours(1));
}

#[tokio::test(start_paused = true)]
async fn the_agent_wakes_when_its_look_comes_due() {
    let start = datetime!(2026-01-05 09:00 UTC);
    let clock = Arc::new(FakeClock::new(start));
    let model = ScriptedProvider::new().then_calls("", vec![("sleep", json!({"minutes": 60}))]);
    let mut agent = scripted(clock.clone(), model);
    let (_send, mut events) = tokio::sync::mpsc::channel(8);

    // Nothing is due yet, so the loop only naps.
    agent
        .run(&mut events, tokio::time::sleep(Duration::from_secs(600)))
        .await
        .unwrap();
    assert!(agent.store.wakes_since(start).unwrap().is_empty());

    // Three hours on, the look it chose is due.
    clock.advance(time::Duration::hours(3));
    agent
        .run(&mut events, tokio::time::sleep(Duration::from_secs(600)))
        .await
        .unwrap();
    let wakes = agent.store.wakes_since(start).unwrap();
    assert_eq!(wakes.len(), 1);
    assert!(wakes[0].reason.contains("next_look"), "{}", wakes[0].reason);
}
