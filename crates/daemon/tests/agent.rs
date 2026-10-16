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
        workdir: dir.path().join("work"),
        profiles_file: write(dir.path(), "profiles.toml", PROFILES),
        profile: "main".into(),
        model_url: format!("http://{proxy}/anthropic"),
        caller_token_file: write(dir.path(), "caller-token", &format!("{TOKEN}\n")),
        charter_file: write(dir.path(), "charter.md", CHARTER),
        quiet_hour: 2,
        utc_offset_hours: 0,
        max_output_tokens: 1024,
        wakeups: None,
        workspace: None,
        memory_dir: None,
    };
    let start = datetime!(2026-01-05 09:00 UTC);
    let clock = Arc::new(FakeClock::new(start));
    let mut agent = Agent::from_config(&config, clock.clone()).unwrap();
    std::fs::write(
        dir.path().join("work/memory/MEMORY.md"),
        "- Mischa reviews PRs in the morning.",
    )
    .unwrap();
    // A restart doesn't wake it at once.
    assert_eq!(agent.rhythm.next_look(), start + time::Duration::minutes(5));

    clock.set(agent.rhythm.next_look());
    agent.wake(Vec::new()).await.unwrap();

    let (headers, body) = calls.lock().unwrap()[0].clone();
    assert_eq!(headers["x-api-key"], "sk-ant-secret");
    assert!(headers.get(CALLER_TOKEN_HEADER).is_none());
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(body.contains(CHARTER));
    assert!(body.contains("Mischa reviews PRs in the morning."));
    assert!(body.contains("\"memory_write\""));
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
        workdir: dir.path().join("work"),
        profiles_file: write(dir.path(), "profiles.toml", PROFILES),
        profile: "main".into(),
        model_url: "http://127.0.0.1:8787/anthropic".into(),
        caller_token_file: write(dir.path(), "caller-token", TOKEN),
        charter_file: dir.path().join("sk-pasted-here"),
        quiet_hour: 2,
        utc_offset_hours: 0,
        max_output_tokens: 1024,
        wakeups: None,
        workspace: None,
        memory_dir: None,
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
        workspace: None,
        memory: None,
        waiting: Vec::new(),
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

/// A model that, on its first call, has `event` arrive from outside, then
/// either hangs (as a slow model would) or goes back to sleep.
struct EventDuringCall {
    send: tokio::sync::mpsc::Sender<WakeEvent>,
    event: Mutex<Option<WakeEvent>>,
    hang: bool,
    then: ScriptedProvider,
    seen: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl harness_providers::Provider for EventDuringCall {
    async fn complete(
        &self,
        request: &harness_providers::ModelRequest,
    ) -> Result<harness_providers::ModelResponse, harness_providers::ProviderError> {
        self.seen
            .lock()
            .unwrap()
            .push(serde_json::to_string(&request.messages).unwrap());
        let event = self.event.lock().unwrap().take();
        if let Some(event) = event {
            self.send.send(event).await.unwrap();
            if self.hang {
                std::future::pending::<()>().await;
            }
        }
        self.then.complete(request).await
    }
}

fn dm(text: &str) -> WakeEvent {
    WakeEvent {
        reason: WakeReason::Workspace {
            summary: "a DM".into(),
            urgent: true,
        },
        text: text.into(),
    }
}

fn mention(text: &str) -> WakeEvent {
    WakeEvent {
        reason: WakeReason::Workspace {
            summary: "an @mention".into(),
            urgent: false,
        },
        text: text.into(),
    }
}

#[tokio::test(start_paused = true)]
async fn an_urgent_event_interrupts_a_slow_model_call() {
    let start = datetime!(2026-01-05 09:00 UTC);
    let clock = Arc::new(FakeClock::new(start));
    let (send, mut events) = tokio::sync::mpsc::channel(8);
    let model = Arc::new(EventDuringCall {
        send: send.clone(),
        event: Mutex::new(Some(dm("[dm] Mischa: prod is down"))),
        hang: true,
        then: ScriptedProvider::new().then_calls("", vec![("sleep", json!({"minutes": 60}))]),
        seen: Mutex::default(),
    });
    let mut agent = scripted(clock.clone(), ScriptedProvider::new());
    agent.provider = Box::new(Shared(model.clone()));
    send.send(dm("[dm] Mischa: standup notes?")).await.unwrap();

    agent
        .run(&mut events, tokio::time::sleep(Duration::from_secs(600)))
        .await
        .unwrap();

    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "the hung call gave way to the urgent DM");
    assert!(seen[1].contains("Urgent, arrived at"), "{}", seen[1]);
    assert!(seen[1].contains("- [dm] Mischa: prod is down"));
    let wakes = agent.store.wakes_since(start).unwrap();
    assert_eq!(wakes.len(), 1, "the DM joined the wake already running");
}

#[tokio::test(start_paused = true)]
async fn events_that_can_wait_get_the_next_wake() {
    let start = datetime!(2026-01-05 09:00 UTC);
    let clock = Arc::new(FakeClock::new(start));
    let (send, mut events) = tokio::sync::mpsc::channel(8);
    let model = Arc::new(EventDuringCall {
        send: send.clone(),
        event: Mutex::new(Some(mention("[#general] Ari: @Tamsin lunch?"))),
        hang: false,
        then: ScriptedProvider::new()
            .then_calls("", vec![("sleep", json!({"minutes": 60}))])
            .then_calls("", vec![("sleep", json!({"minutes": 60}))]),
        seen: Mutex::default(),
    });
    let mut agent = scripted(clock.clone(), ScriptedProvider::new());
    agent.provider = Box::new(Shared(model.clone()));
    send.send(dm("[dm] Mischa: hi")).await.unwrap();

    agent
        .run(&mut events, tokio::time::sleep(Duration::from_secs(600)))
        .await
        .unwrap();
    // The mention that came in during the DM's wake doesn't start a wake
    // of its own.
    assert_eq!(agent.store.wakes_since(start).unwrap().len(), 1);
    assert_eq!(agent.waiting.len(), 1);

    // An optional request found by the poller waits too.
    send.send(mention("[#general] Bo: @Tamsin anyone?"))
        .await
        .unwrap();
    agent
        .run(&mut events, tokio::time::sleep(Duration::from_secs(600)))
        .await
        .unwrap();
    assert_eq!(agent.store.wakes_since(start).unwrap().len(), 1);

    // At the look it chose, it reads both.
    clock.advance(time::Duration::hours(1));
    agent
        .run(&mut events, tokio::time::sleep(Duration::from_secs(600)))
        .await
        .unwrap();
    let wakes = agent.store.wakes_since(start).unwrap();
    assert_eq!(wakes.len(), 2);
    let seen = model.seen.lock().unwrap();
    assert!(seen[1].contains("- [#general] Ari: @Tamsin lunch?"));
    assert!(seen[1].contains("- [#general] Bo: @Tamsin anyone?"));
}

/// Lets a test keep a handle on the model it gives the agent.
struct Shared(Arc<EventDuringCall>);

#[async_trait::async_trait]
impl harness_providers::Provider for Shared {
    async fn complete(
        &self,
        request: &harness_providers::ModelRequest,
    ) -> Result<harness_providers::ModelResponse, harness_providers::ProviderError> {
        self.0.complete(request).await
    }
}

#[test]
fn the_next_look_and_a_pause_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite3");
    let now = datetime!(2026-01-05 09:00 UTC);
    let policy = WorkdayPolicy::default();
    let fresh = Store::open(&path).unwrap();
    assert_eq!(
        harness_daemon::agent::restore_rhythm(&fresh, policy, now)
            .unwrap()
            .next_look(),
        now + time::Duration::minutes(5)
    );
    let later = now + time::Duration::hours(20);
    fresh.set_next_look(later).unwrap();
    fresh.set_paused(Some("spend")).unwrap();
    drop(fresh);

    let store = Store::open(&path).unwrap();
    let rhythm = harness_daemon::agent::restore_rhythm(&store, policy, now).unwrap();
    assert_eq!(rhythm.next_look(), later);
    assert_eq!(rhythm.paused(), Some("spend"));
    // A look missed while the daemon was down is due now.
    let rhythm =
        harness_daemon::agent::restore_rhythm(&store, policy, later + time::Duration::hours(1))
            .unwrap();
    assert_eq!(rhythm.next_look(), later + time::Duration::hours(1));
}

#[tokio::test(start_paused = true)]
async fn a_paused_agent_waits_for_its_owner() {
    let start = datetime!(2026-01-05 09:00 UTC);
    let clock = Arc::new(FakeClock::new(start));
    let model = ScriptedProvider::new().then_calls("", vec![("sleep", json!({"minutes": 60}))]);
    let mut agent = scripted(clock.clone(), model);
    agent.store.set_paused(Some("spend")).unwrap();
    agent.rhythm = agent.rhythm.clone().with_pause(Some("spend".into()));
    clock.advance(time::Duration::hours(3));
    let (send, mut events) = tokio::sync::mpsc::channel(8);
    send.send(dm("[dm] Mischa: you there?")).await.unwrap();

    agent
        .run(&mut events, tokio::time::sleep(Duration::from_secs(600)))
        .await
        .unwrap();
    assert!(agent.store.wakes_since(start).unwrap().is_empty());

    // The owner lifts the pause; the agent notices and wakes.
    agent.store.set_paused(None).unwrap();
    agent
        .run(&mut events, tokio::time::sleep(Duration::from_secs(600)))
        .await
        .unwrap();
    assert_eq!(agent.store.wakes_since(start).unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn events_that_need_a_look_wake_an_idle_agent() {
    let start = datetime!(2026-01-05 09:00 UTC);
    let clock = Arc::new(FakeClock::new(start));
    let model = ScriptedProvider::new().then_calls("", vec![("sleep", json!({"minutes": 60}))]);
    let mut agent = scripted(clock.clone(), model);
    let (send, mut events) = tokio::sync::mpsc::channel(8);
    // A trigger doesn't interrupt a running wake, but it doesn't wait
    // three hours for the next look either.
    send.send(WakeEvent {
        reason: WakeReason::Trigger {
            name: "ci-failed".into(),
        },
        text: "Agent OS: CI failed on main.".into(),
    })
    .await
    .unwrap();

    agent
        .run(&mut events, tokio::time::sleep(Duration::from_secs(600)))
        .await
        .unwrap();
    let wakes = agent.store.wakes_since(start).unwrap();
    assert_eq!(wakes.len(), 1);
    assert!(wakes[0].reason.contains("ci-failed"), "{}", wakes[0].reason);
}

#[tokio::test(start_paused = true)]
async fn each_day_starts_from_what_the_agent_remembers() {
    let dir = tempfile::tempdir().unwrap();
    let config = AgentConfig {
        state_dir: dir.path().join("state"),
        workdir: dir.path().join("work"),
        profiles_file: write(dir.path(), "profiles.toml", PROFILES),
        profile: "main".into(),
        model_url: "http://127.0.0.1:8787/anthropic".into(),
        caller_token_file: write(dir.path(), "caller-token", TOKEN),
        charter_file: write(dir.path(), "charter.md", CHARTER),
        quiet_hour: 2,
        utc_offset_hours: 0,
        max_output_tokens: 1024,
        wakeups: None,
        workspace: None,
        memory_dir: Some(dir.path().join("work/mind")),
    };
    let start = datetime!(2026-01-05 09:00 UTC);
    let clock = Arc::new(FakeClock::new(start));
    let mut agent = Agent::from_config(&config, clock.clone()).unwrap();
    // The agent saves a memory during its first wake, then sleeps.
    agent.provider = Box::new(
        ScriptedProvider::new()
            .then_calls(
                "",
                vec![(
                    "memory_write",
                    json!({"file": "memory", "content": "- Ship the memory tools."}),
                )],
            )
            .then_calls("", vec![("sleep", json!({"minutes": 60}))])
            .then_calls("", vec![("sleep", json!({"minutes": 60}))])
            .then_calls("", vec![("sleep", json!({"minutes": 60}))]),
    );

    agent.wake(Vec::new()).await.unwrap();
    assert!(
        agent
            .assembler
            .memory_snapshot
            .contains("## MEMORY.md\n(empty)")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("work/mind/MEMORY.md")).unwrap(),
        "- Ship the memory tools.\n"
    );

    // Later the same day, even after a restart, the snapshot hasn't moved,
    // so the cache holds.
    drop(agent);
    let mut agent = Agent::from_config(&config, clock.clone()).unwrap();
    agent.provider = Box::new(
        ScriptedProvider::new()
            .then_calls("", vec![("sleep", json!({"minutes": 60}))])
            .then_calls("", vec![("sleep", json!({"minutes": 60}))]),
    );
    clock.advance(time::Duration::hours(2));
    agent.wake(Vec::new()).await.unwrap();
    assert!(
        agent
            .assembler
            .memory_snapshot
            .contains("## MEMORY.md\n(empty)")
    );

    // The next day starts with what it wrote.
    clock.set(datetime!(2026-01-06 09:00 UTC));
    agent.wake(Vec::new()).await.unwrap();
    assert!(
        agent
            .assembler
            .memory_snapshot
            .contains("## MEMORY.md\n- Ship the memory tools."),
        "{}",
        agent.assembler.memory_snapshot
    );
}

#[test]
fn memory_must_live_inside_the_work_area() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(elsewhere.path().join("MEMORY.md"), "not the agent's").unwrap();
    let workdir = dir.path().join("work");
    std::fs::create_dir(&workdir).unwrap();
    let config = AgentConfig {
        state_dir: dir.path().join("state"),
        workdir,
        profiles_file: write(dir.path(), "profiles.toml", PROFILES),
        profile: "main".into(),
        model_url: "http://127.0.0.1:8787/anthropic".into(),
        caller_token_file: write(dir.path(), "caller-token", TOKEN),
        charter_file: write(dir.path(), "charter.md", CHARTER),
        quiet_hour: 2,
        utc_offset_hours: 0,
        max_output_tokens: 1024,
        wakeups: None,
        workspace: None,
        memory_dir: Some(elsewhere.path().to_path_buf()),
    };
    let clock = Arc::new(FakeClock::new(datetime!(2026-01-05 09:00 UTC)));
    let Err(err) = Agent::from_config(&config, clock) else {
        panic!("a memory folder outside workdir was accepted");
    };
    assert!(err.to_string().contains("inside workdir"), "{err}");
}

#[cfg(unix)]
#[test]
fn a_linked_workdir_still_holds_the_memory() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real-home");
    std::fs::create_dir(&real).unwrap();
    let linked = dir.path().join("home");
    std::os::unix::fs::symlink(&real, &linked).unwrap();
    let config = AgentConfig {
        state_dir: dir.path().join("state"),
        workdir: linked,
        profiles_file: write(dir.path(), "profiles.toml", PROFILES),
        profile: "main".into(),
        model_url: "http://127.0.0.1:8787/anthropic".into(),
        caller_token_file: write(dir.path(), "caller-token", TOKEN),
        charter_file: write(dir.path(), "charter.md", CHARTER),
        quiet_hour: 2,
        utc_offset_hours: 0,
        max_output_tokens: 1024,
        wakeups: None,
        workspace: None,
        memory_dir: None,
    };
    let clock = Arc::new(FakeClock::new(datetime!(2026-01-05 09:00 UTC)));
    if let Err(err) = Agent::from_config(&config, clock) {
        panic!("{err}");
    }
    assert!(real.join("memory/journal").is_dir());
}

#[test]
fn the_state_folder_stays_out_of_the_agents_reach() {
    let dir = tempfile::tempdir().unwrap();
    let config = AgentConfig {
        state_dir: dir.path().join("work/.state"),
        workdir: dir.path().join("work"),
        profiles_file: write(dir.path(), "profiles.toml", PROFILES),
        profile: "main".into(),
        model_url: "http://127.0.0.1:8787/anthropic".into(),
        caller_token_file: write(dir.path(), "caller-token", TOKEN),
        charter_file: write(dir.path(), "charter.md", CHARTER),
        quiet_hour: 2,
        utc_offset_hours: 0,
        max_output_tokens: 1024,
        wakeups: None,
        workspace: None,
        memory_dir: None,
    };
    let clock = Arc::new(FakeClock::new(datetime!(2026-01-05 09:00 UTC)));
    let Err(err) = Agent::from_config(&config, clock) else {
        panic!("a state folder inside workdir was accepted");
    };
    assert!(err.to_string().contains("outside workdir"), "{err}");
    assert!(!dir.path().join("work/.state/memory.git").exists());
}

#[cfg(unix)]
#[test]
fn a_link_out_of_the_work_area_is_refused_before_anything_is_made() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, work.join("out")).unwrap();
    let config = AgentConfig {
        state_dir: dir.path().join("state"),
        workdir: work.clone(),
        profiles_file: write(dir.path(), "profiles.toml", PROFILES),
        profile: "main".into(),
        model_url: "http://127.0.0.1:8787/anthropic".into(),
        caller_token_file: write(dir.path(), "caller-token", TOKEN),
        charter_file: write(dir.path(), "charter.md", CHARTER),
        quiet_hour: 2,
        utc_offset_hours: 0,
        max_output_tokens: 1024,
        wakeups: None,
        workspace: None,
        memory_dir: Some(work.join("out/memory")),
    };
    let clock = Arc::new(FakeClock::new(datetime!(2026-01-05 09:00 UTC)));
    assert!(Agent::from_config(&config, clock).is_err());
    assert!(
        !elsewhere.join("memory").exists(),
        "nothing was made outside"
    );
    assert!(!dir.path().join("state/memory.git").exists());
}

#[cfg(unix)]
#[test]
fn a_new_workdir_behind_a_link_is_made_where_the_link_leads() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, dir.path().join("home")).unwrap();
    let config = AgentConfig {
        state_dir: dir.path().join("state"),
        workdir: dir.path().join("home/agent"),
        profiles_file: write(dir.path(), "profiles.toml", PROFILES),
        profile: "main".into(),
        model_url: "http://127.0.0.1:8787/anthropic".into(),
        caller_token_file: write(dir.path(), "caller-token", TOKEN),
        charter_file: write(dir.path(), "charter.md", CHARTER),
        quiet_hour: 2,
        utc_offset_hours: 0,
        max_output_tokens: 1024,
        wakeups: None,
        workspace: None,
        memory_dir: None,
    };
    let clock = Arc::new(FakeClock::new(datetime!(2026-01-05 09:00 UTC)));
    if let Err(err) = Agent::from_config(&config, clock) {
        panic!("{err}");
    }
    assert!(real.join("agent/memory/journal").is_dir());
}
