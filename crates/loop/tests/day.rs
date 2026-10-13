//! Wakes across a workday, against the scripted model and an in-memory store.

use std::sync::Arc;

use harness_core::{Clock, FakeClock, WakeReason};
use harness_loop::{ContextAssembler, DayError, WakeEnd, WakeLimits, Workday};
use harness_providers::{
    CachingStyle, CapabilityProfile, Message, Pricing, ProviderError, ScriptedProvider,
    ToolCallStyle,
};
use harness_store::{Store, WorkdayPolicy};
use harness_tools::{Policy, PolicyExecutor, SleepTool};
use serde_json::json;
use time::Duration;

fn profile() -> CapabilityProfile {
    CapabilityProfile {
        adapter: "anthropic".into(),
        model: "fake".into(),
        context_window: 200_000,
        tool_calls: ToolCallStyle::Parallel,
        caching: CachingStyle::ExplicitBreakpoints,
        structured_output: false,
        cache_ttl: None,
        pricing: Pricing {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 6.0,
        },
    }
}

fn assembler() -> ContextAssembler {
    ContextAssembler {
        profile: "main".into(),
        static_prefix: "You are Mara.".into(),
        memory_snapshot: "Nothing yet.".into(),
        max_output_tokens: 4096,
    }
}

struct Rig {
    store: Store,
    executor: PolicyExecutor,
    clock: Arc<FakeClock>,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::at_default_start());
    Rig {
        store: Store::open_in_memory().unwrap(),
        executor: PolicyExecutor::new(Policy::conservative(dir.path()), clock.clone())
            .with_tool(SleepTool),
        clock,
        _dir: dir,
    }
}

impl Rig {
    async fn wake(
        &mut self,
        provider: &ScriptedProvider,
        text: &str,
    ) -> Result<harness_loop::WakeOutcome, DayError> {
        let profile = profile();
        let assembler = assembler();
        let mut day = Workday {
            store: &mut self.store,
            provider,
            executor: &mut self.executor,
            assembler: &assembler,
            profile: &profile,
            clock: self.clock.as_ref(),
            limits: WakeLimits::default(),
        };
        day.wake(&WakeReason::NextLook, Message::user_text(text))
            .await
    }
}

fn sleeps() -> ScriptedProvider {
    ScriptedProvider::new().then_calls("", vec![("sleep", json!({"minutes": 120}))])
}

#[tokio::test]
async fn wakes_in_a_day_share_one_train_of_thought() {
    let mut r = rig();
    r.wake(&sleeps(), "morning").await.unwrap();
    r.clock.advance(Duration::hours(2));

    let second = sleeps();
    r.wake(&second, "afternoon").await.unwrap();

    // The second wake's request carries the first wake's conversation.
    let sent = &second.requests()[0].messages;
    assert_eq!(
        sent.len(),
        1 + 3 + 1,
        "memory, first wake, new wake message"
    );
    assert_eq!(sent[1], Message::user_text("morning"));
    assert_eq!(sent[4], Message::user_text("afternoon"));

    let day = r.store.current_day().unwrap().unwrap();
    assert_eq!(r.store.transcript(day.id).unwrap().len(), 6);
    let wakes = r.store.wakes_since(day.started_at).unwrap();
    assert_eq!(wakes.len(), 2);
    assert_eq!(wakes[1].end, "slept");
    assert_eq!(wakes[1].started_at, day.started_at + Duration::hours(2));
}

#[tokio::test]
async fn a_new_day_starts_fresh() {
    let mut r = rig();
    r.wake(&sleeps(), "monday").await.unwrap();
    let monday = r.store.current_day().unwrap().unwrap();
    r.store
        .end_day(monday.id, r.clock.now(), Some("handoff"))
        .unwrap();

    r.clock.advance(Duration::DAY);
    let tuesday = sleeps();
    r.wake(&tuesday, "tuesday").await.unwrap();
    let sent = &tuesday.requests()[0].messages;
    assert_eq!(sent.len(), 2, "memory and today's wake message only");
}

#[tokio::test]
async fn a_failed_model_call_keeps_the_transcript_and_the_spend() {
    let mut r = rig();
    let flaky = ScriptedProvider::new()
        .then_calls("", vec![("sleep", json!({"minutes": "soon"}))])
        .then_error(ProviderError::Unavailable("down".into()));
    let err = r.wake(&flaky, "wake").await.unwrap_err();
    assert!(err.to_string().contains("down"));

    let day = r.store.current_day().unwrap().unwrap();
    // wake message, the malformed call, its error result
    assert_eq!(r.store.transcript(day.id).unwrap().len(), 3);
    let wake = &r.store.wakes_since(day.started_at).unwrap()[0];
    assert_eq!(wake.end, "error");
    assert_eq!(wake.turns, 2, "the failed call counts as a turn");
    assert_eq!(wake.usage.input_tokens, 100);
    assert!(wake.cost_usd > 0.0);
    // The turn that did finish grew the context, and the workday policy sees it.
    assert_eq!(r.store.current_day().unwrap().unwrap().context_tokens, 120);
}

#[tokio::test]
async fn the_days_context_size_drives_an_early_evening() {
    let mut r = rig();
    r.wake(&sleeps(), "wake").await.unwrap();
    let day = r.store.current_day().unwrap().unwrap();
    // The scripted model reports 100 in and 20 out per call.
    assert_eq!(day.context_tokens, 120);
    let policy = WorkdayPolicy {
        max_context_tokens: 100,
        ..WorkdayPolicy::default()
    };
    assert!(policy.evening_due(&day, r.clock.now()).is_some());
    assert!(
        WorkdayPolicy::default()
            .evening_due(&day, r.clock.now())
            .is_none()
    );
}

#[tokio::test]
async fn sleep_outcome_is_returned() {
    let mut r = rig();
    let out = r.wake(&sleeps(), "wake").await.unwrap();
    assert_eq!(
        out.end,
        WakeEnd::Slept {
            until: r.clock.now() + Duration::hours(2)
        }
    );
}
