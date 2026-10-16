//! The loop against the scripted model: fast, deterministic, free.

use std::sync::Arc;

use async_trait::async_trait;
use harness_core::{Clock, FakeClock};
use harness_loop::{ContextAssembler, NoCheckpoint, WakeEnd, WakeLimits, run_wake};
use harness_providers::{
    CachingStyle, CapabilityProfile, ContentBlock, Message, Pricing, ProviderError,
    ScriptedProvider, ToolCallStyle, ToolResult, ToolSpec,
};
use harness_tools::{
    Effect, Policy, PolicyExecutor, SleepTool, Tool, ToolContext, ToolError, ToolOutput,
};
use serde_json::{Value, json};
use time::Duration;

struct Send;

#[async_trait]
impl Tool for Send {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "message_send".into(),
            description: String::new(),
            parameters: json!({
                "type": "object",
                "properties": {"to": {"type": "string"}, "text": {"type": "string"}},
                "required": ["to", "text"]
            }),
        }
    }

    fn effects(&self, args: &Value) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![Effect::SendMessage {
            conversation: args["to"].as_str().unwrap_or_default().into(),
        }])
    }

    async fn run(&self, _args: &Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("sent"))
    }
}

struct Write;

#[async_trait]
impl Tool for Write {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".into(),
            description: String::new(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        }
    }

    fn effects(&self, args: &Value) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![Effect::WritePath(
            args["path"].as_str().unwrap_or_default().into(),
        )])
    }

    async fn run(&self, _args: &Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("written"))
    }
}

fn profile(tool_calls: ToolCallStyle) -> CapabilityProfile {
    CapabilityProfile {
        adapter: "anthropic".into(),
        model: "fake".into(),
        context_window: 200_000,
        tool_calls,
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

struct World {
    clock: Arc<FakeClock>,
    executor: PolicyExecutor,
    transcript: Vec<Message>,
    _dir: tempfile::TempDir,
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::at_default_start());
    let executor = PolicyExecutor::new(Policy::conservative(dir.path()), clock.clone())
        .with_tool(SleepTool)
        .with_tool(Send)
        .with_tool(Write);
    World {
        clock,
        executor,
        transcript: vec![],
        _dir: dir,
    }
}

impl World {
    async fn wake(
        &mut self,
        provider: &ScriptedProvider,
        style: ToolCallStyle,
        limits: WakeLimits,
    ) -> harness_loop::WakeOutcome {
        run_wake(
            provider,
            &mut self.executor,
            &assembler(),
            &profile(style),
            &mut self.transcript,
            Message::user_text("Wake: Mischa sent you a DM."),
            limits,
            &mut NoCheckpoint,
        )
        .await
        .unwrap()
    }
}

fn results(message: &Message) -> Vec<&ToolResult> {
    message
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult(r) => Some(r),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn replies_then_sleeps() {
    let mut w = world();
    let provider = ScriptedProvider::new()
        .then_calls(
            "",
            vec![("message_send", json!({"to": "dm", "text": "On it."}))],
        )
        .then_calls("", vec![("sleep", json!({"minutes": 180}))]);
    let out = w
        .wake(&provider, ToolCallStyle::Parallel, WakeLimits::default())
        .await;

    assert_eq!(
        out.end,
        WakeEnd::Slept {
            until: w.clock.now() + Duration::hours(3)
        }
    );
    assert_eq!(out.turns, 2);
    // wake, call, result, call, result
    assert_eq!(w.transcript.len(), 5);
    assert_eq!(results(&w.transcript[2])[0].content, "sent");
    assert_eq!(provider.remaining(), 0);
}

#[tokio::test]
async fn a_wall_is_reported_to_the_model_and_the_wake_continues() {
    let mut w = world();
    let provider = ScriptedProvider::new()
        .then_calls(
            "",
            vec![("write", json!({"path": ".github/workflows/ci.yml"}))],
        )
        .then_calls("", vec![("sleep", json!({"minutes": 60}))]);
    let out = w
        .wake(&provider, ToolCallStyle::Parallel, WakeLimits::default())
        .await;

    assert!(matches!(out.end, WakeEnd::Slept { .. }));
    let blocked = results(&w.transcript[2])[0];
    assert!(blocked.is_error);
    assert!(
        blocked.content.contains("blocked by a wall"),
        "{}",
        blocked.content
    );
}

#[tokio::test]
async fn one_malformed_turn_is_forgiven_two_end_the_wake() {
    let mut w = world();
    let recovers = ScriptedProvider::new()
        .then_calls("", vec![("sleep", json!({"minutes": "later"}))])
        .then_calls("", vec![("sleep", json!({"minutes": 30}))]);
    let out = w
        .wake(&recovers, ToolCallStyle::Parallel, WakeLimits::default())
        .await;
    assert!(matches!(out.end, WakeEnd::Slept { .. }));
    assert!(results(&w.transcript[2])[0].content.contains("malformed"));

    let mut w = world();
    let keeps_failing = ScriptedProvider::new()
        .then_calls("", vec![("sleep", json!({}))])
        .then_calls("", vec![("nap", json!({"minutes": 30}))])
        .then_calls("", vec![("sleep", json!({"minutes": 30}))]);
    let out = w
        .wake(
            &keeps_failing,
            ToolCallStyle::Parallel,
            WakeLimits::default(),
        )
        .await;
    assert_eq!(out.end, WakeEnd::Malformed);
    assert_eq!(out.turns, 2);
}

#[tokio::test]
async fn a_text_turn_breaks_a_malformed_streak() {
    let mut w = world();
    let provider = ScriptedProvider::new()
        .then_calls("", vec![("sleep", json!({}))])
        .then_text("Let me reconsider.")
        .then_calls("", vec![("sleep", json!({}))])
        .then_calls("", vec![("sleep", json!({}))]);
    let out = w
        .wake(&provider, ToolCallStyle::Parallel, WakeLimits::default())
        .await;

    assert_eq!(out.end, WakeEnd::Malformed);
    assert_eq!(
        out.turns, 4,
        "only consecutive malformed turns should end a wake"
    );
}

#[tokio::test]
async fn a_wake_that_never_sleeps_gets_one_reminder() {
    let mut w = world();
    let provider = ScriptedProvider::new()
        .then_text("Hmm.")
        .then_text("Done for now.");
    let out = w
        .wake(&provider, ToolCallStyle::Parallel, WakeLimits::default())
        .await;
    assert_eq!(out.end, WakeEnd::NoSleep);
    assert_eq!(out.turns, 2);
    assert!(matches!(
        &w.transcript[2].content[0],
        ContentBlock::Text { text } if text.contains("sleep")
    ));
}

#[tokio::test]
async fn sequential_models_get_one_call_per_turn() {
    let mut w = world();
    let provider = ScriptedProvider::new()
        .then_calls(
            "",
            vec![
                ("message_send", json!({"to": "dm", "text": "a"})),
                ("sleep", json!({"minutes": 30})),
            ],
        )
        .then_calls("", vec![("sleep", json!({"minutes": 30}))]);
    let out = w
        .wake(&provider, ToolCallStyle::Sequential, WakeLimits::default())
        .await;
    assert_eq!(
        out.turns, 2,
        "the second call in turn one must not have run"
    );
    let first = results(&w.transcript[2]);
    assert_eq!(first.len(), 2, "every call gets a result");
    assert!(first[1].content.contains("one tool call per turn"));
}

#[tokio::test]
async fn the_turn_limit_ends_a_runaway_wake() {
    let mut w = world();
    let mut provider = ScriptedProvider::new();
    for _ in 0..10 {
        provider =
            provider.then_calls("", vec![("message_send", json!({"to": "dm", "text": "x"}))]);
    }
    let out = w
        .wake(
            &provider,
            ToolCallStyle::Parallel,
            WakeLimits { max_turns: 3 },
        )
        .await;
    assert_eq!(out.end, WakeEnd::TurnLimit);
    assert_eq!(out.turns, 3);
}

#[tokio::test]
async fn a_tripped_breaker_pauses_the_agent() {
    let mut w = world();
    let mut provider = ScriptedProvider::new();
    for _ in 0..31 {
        provider =
            provider.then_calls("", vec![("message_send", json!({"to": "dm", "text": "x"}))]);
    }
    let out = w
        .wake(
            &provider,
            ToolCallStyle::Parallel,
            WakeLimits { max_turns: 50 },
        )
        .await;
    assert_eq!(
        out.end,
        WakeEnd::Paused {
            breaker: "messages_per_hour".into()
        }
    );
    assert_eq!(w.executor.paused(), Some("messages_per_hour"));
}

#[tokio::test]
async fn usage_and_cost_are_counted_per_wake() {
    let mut w = world();
    let provider = ScriptedProvider::new()
        .then_calls("", vec![("message_send", json!({"to": "dm", "text": "a"}))])
        .then_calls("", vec![("sleep", json!({"minutes": 30}))]);
    let out = w
        .wake(&provider, ToolCallStyle::Parallel, WakeLimits::default())
        .await;
    // The scripted model reports 100 in and 20 out per turn.
    assert_eq!(out.usage.input_tokens, 200);
    assert_eq!(out.usage.output_tokens, 40);
    assert!((out.cost_usd - (200.0 * 3.0 + 40.0 * 15.0) / 1e6).abs() < 1e-12);
}

#[tokio::test]
async fn every_request_keeps_the_cache_friendly_order() {
    let mut w = world();
    let provider = ScriptedProvider::new()
        .then_calls("", vec![("message_send", json!({"to": "dm", "text": "a"}))])
        .then_calls("", vec![("sleep", json!({"minutes": 30}))]);
    w.wake(&provider, ToolCallStyle::Parallel, WakeLimits::default())
        .await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].system, "You are Mara.");
    assert_eq!(
        &requests[1].messages[..requests[0].messages.len()],
        &requests[0].messages[..],
        "the second request extends the first"
    );
    assert_eq!(requests[0].tools.len(), 3);
}

#[tokio::test]
async fn provider_errors_surface() {
    let mut w = world();
    let provider = ScriptedProvider::new().then_error(ProviderError::RateLimited);
    let err = run_wake(
        &provider,
        &mut w.executor,
        &assembler(),
        &profile(ToolCallStyle::Parallel),
        &mut w.transcript,
        Message::user_text("wake"),
        WakeLimits::default(),
        &mut NoCheckpoint,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("rate limited"));
}

/// A model whose first call never answers, then sleeps.
struct HangsOnce {
    calls: std::sync::Mutex<Vec<Vec<Message>>>,
    then: ScriptedProvider,
}

#[async_trait]
impl harness_providers::Provider for HangsOnce {
    async fn complete(
        &self,
        request: &harness_providers::ModelRequest,
    ) -> Result<harness_providers::ModelResponse, ProviderError> {
        let first = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(request.messages.clone());
            calls.len() == 1
        };
        if first {
            std::future::pending::<()>().await;
        }
        self.then.complete(request).await
    }
}

/// One urgent message, ready once `ready` is set.
struct Urgent {
    ready: bool,
    message: Option<Message>,
}

impl harness_loop::Interrupts for Urgent {
    fn take(&mut self) -> Vec<Message> {
        if self.ready {
            self.message.take().into_iter().collect()
        } else {
            Vec::new()
        }
    }

    fn wait(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + std::marker::Send + '_>> {
        if self.message.is_some() {
            // It arrives while the first call is in flight.
            self.ready = true;
            Box::pin(std::future::ready(()))
        } else {
            Box::pin(std::future::pending())
        }
    }
}

#[tokio::test]
async fn an_urgent_message_cancels_the_call_in_flight() {
    let mut w = world();
    let provider = HangsOnce {
        calls: Default::default(),
        then: ScriptedProvider::new().then_calls("", vec![("sleep", json!({"minutes": 30}))]),
    };
    let mut urgent = Urgent {
        ready: false,
        message: Some(Message::user_text("Urgent: prod is down.")),
    };
    let outcome = harness_loop::run_wake_interruptible(
        &provider,
        &mut w.executor,
        &assembler(),
        &profile(ToolCallStyle::Parallel),
        &mut w.transcript,
        Message::user_text("Wake: the time you chose to look again."),
        WakeLimits::default(),
        &mut NoCheckpoint,
        &mut urgent,
    )
    .await
    .unwrap();

    assert!(matches!(outcome.end, WakeEnd::Slept { .. }));
    let calls = provider.calls.lock().unwrap();
    assert_eq!(calls.len(), 2, "the hung call was dropped and retried");
    let retried = calls[1].last().unwrap();
    assert_eq!(retried, &Message::user_text("Urgent: prod is down."));
    assert_eq!(w.transcript[1], Message::user_text("Urgent: prod is down."));
}

/// Urgent news that turns up on the `at`-th look.
struct NewsAt {
    looks: usize,
    at: usize,
}

impl harness_loop::Interrupts for NewsAt {
    fn take(&mut self) -> Vec<Message> {
        self.looks += 1;
        if self.looks == self.at {
            vec![Message::user_text("Urgent: stop the deploy.")]
        } else {
            Vec::new()
        }
    }

    fn wait(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + std::marker::Send + '_>> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn urgent_news_between_calls_stops_the_rest_of_the_batch() {
    let mut w = world();
    let provider = ScriptedProvider::new()
        .then_calls(
            "",
            vec![
                ("message_send", json!({"to": "dm", "text": "Deploying."})),
                ("sleep", json!({"minutes": 60})),
            ],
        )
        .then_calls("", vec![("sleep", json!({"minutes": 30}))]);
    // Looks: before the first model call, before each tool call.
    let mut news = NewsAt { looks: 0, at: 3 };

    let outcome = harness_loop::run_wake_interruptible(
        &provider,
        &mut w.executor,
        &assembler(),
        &profile(ToolCallStyle::Parallel),
        &mut w.transcript,
        Message::user_text("Wake."),
        WakeLimits::default(),
        &mut NoCheckpoint,
        &mut news,
    )
    .await
    .unwrap();

    let batch = results(&w.transcript[2]);
    assert_eq!(batch[0].content, "sent");
    assert!(batch[1].content.starts_with("not run: urgent news"));
    assert_eq!(
        w.transcript[3],
        Message::user_text("Urgent: stop the deploy.")
    );
    // It didn't sleep on the stale call; it decided again.
    assert_eq!(outcome.turns, 2);
    assert!(matches!(outcome.end, WakeEnd::Slept { .. }));
    assert_eq!(provider.remaining(), 0);
}
